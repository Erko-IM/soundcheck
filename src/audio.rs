//! Opening a file and reading its samples: our own reader for the WAV
//! family, symphonia for the rest. Nothing holds a whole recording in
//! memory; analysis, meters, the spectrum and playback all read the file as
//! they go.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};

use rayon::prelude::*;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::well_known::FORMAT_ID_FLAC;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, RawValue, StandardTag};
use symphonia::core::packet::Packet;
use symphonia::core::units::Timestamp;

use crate::edit::Edits;
use crate::levels::Levels;
use crate::meta::{self, Details, Meta};
use crate::spectrogram::{Analysis, Analyzer, Spec};
use crate::tags;
use crate::wav::{self, SampleKind};

/// A timestamp further ahead than this is a damaged one, not a gap.
const MAX_GAP_SECONDS: usize = 10;
/// Samples, all channels counted, read at a time by a pass over a file or
/// a range of it: enough that the work on each read far outweighs sharing
/// it out between threads, in the same memory whatever the channel count.
const PIECE_SAMPLES: usize = 1 << 21;
/// The same for a compressed file, whose pieces a thread each decodes side
/// by side: a thread's worth in hand stays a few MB, and the lead-in each
/// piece needs costs a few percent.
const CODED_PIECE_SAMPLES: usize = 1 << 19;
/// Frames a compressed piece is decoded from before its start, and dropped:
/// a codec whose frames overlap, as AAC, MP3 and Vorbis do, decodes a frame
/// as reading from the top does only with the one before it in hand.
const LEAD_IN: usize = 1 << 14;
/// Frames over which a piece decoded on its own takes over from the one
/// before. AAC's noise substitution draws on a generator that runs through
/// the whole file, so where it is used two decodes of the same frames
/// differ, and a cut from one to the other would click.
const JOIN: usize = 2048;
/// How far back a failed seek is tried again from before the read fails.
const MAX_BACK_SECONDS: usize = 64;
/// PCM reads of more frames than this are split across threads.
const PARALLEL_FRAMES: usize = 1 << 15;
/// A compressed file is decoded forward over a jump this short, and seeks
/// past a longer one, which takes about as long as decoding a second.
const FORWARD_SECONDS: usize = 1;
/// Frames a compressed file keeps of what it last read, for a read that
/// starts that far back: playback's band filter reads in steps that overlap
/// by up to three quarters of 8192 frames.
const HISTORY: usize = 1 << 15;

#[derive(Clone)]
pub struct Info {
    pub container: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: Option<u16>,
    pub frames: usize,
    pub bytes: u64,
}

impl Info {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate)
    }

    /// The highest frequency the file can contain.
    pub fn nyquist(&self) -> f32 {
        self.sample_rate as f32 / 2.0
    }
}

/// Where samples are read from.
#[derive(Clone)]
pub enum Source {
    /// PCM, read straight from the WAV.
    Pcm {
        path: PathBuf,
        data: Range<u64>,
        kind: SampleKind,
    },
    /// Anything symphonia reads, decoded as it is read.
    Coded(PathBuf),
}

impl Source {
    /// The same source after its file was renamed.
    pub fn with_path(&self, path: &Path) -> Self {
        match self {
            Self::Pcm { data, kind, .. } => Self::Pcm {
                path: path.to_owned(),
                data: data.clone(),
                kind: *kind,
            },
            Self::Coded(_) => Self::Coded(path.to_owned()),
        }
    }
}

pub struct Loaded {
    pub info: Info,
    pub meta: Meta,
    pub details: Details,
    pub source: Source,
    pub levels: Levels,
    /// What can be edited: a WAV file's own chunks and markers, and the
    /// tags of any file lofty reads.
    pub edits: Option<Edits>,
}

/// A file's header and metadata, without its audio.
pub struct Opened {
    pub info: Info,
    pub meta: Meta,
    pub details: Details,
    pub source: Source,
    pub edits: Option<Edits>,
}

pub fn open(path: &Path) -> Result<Opened, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
    let bytes = file.metadata().map_or(0, |m| m.len());
    match wav::parse(&mut file) {
        Ok(w) => {
            let mut edits = Edits::from_wav(&w);
            // An ID3 tag lofty cannot read, in a kind of WAV it does not
            // know, stays in the file as it is, but cannot be edited.
            let id3 = w
                .chunks
                .iter()
                .any(|c| &c.id == b"id3 " || &c.id == b"ID3 ");
            if let Some(block) = id3.then(|| tags::wav_id3(path).ok().flatten()).flatten() {
                edits.tags.insert(0, block);
            }
            Ok(Opened {
                info: Info {
                    container: w.container.to_owned(),
                    sample_rate: w.sample_rate,
                    channels: w.channels,
                    bits: Some(w.kind.bits()),
                    frames: w.frames(),
                    bytes,
                },
                meta: meta::from_wav(&w),
                details: meta::wav_details(&w),
                source: Source::Pcm {
                    path: path.to_owned(),
                    data: w.data.clone(),
                    kind: w.kind,
                },
                edits: Some(edits),
            })
        }
        Err(wav::Error::NotWav) => {
            let mut coded = Coded::open(path)?;
            let tags = coded
                .format
                .metadata()
                .skip_to_latest()
                .map(|r| r.media.tags.clone())
                .unwrap_or_default();
            let text = |want: fn(&StandardTag) -> Option<&str>| {
                tags.iter()
                    .filter_map(move |t| t.std.as_ref().and_then(want))
            };
            let description = text(|t| match t {
                StandardTag::Description(s) => Some(s.as_str()),
                _ => None,
            });
            let comment = text(|t| match t {
                StandardTag::Comment(s) => Some(s.as_str()),
                _ => None,
            });
            let meta = meta::from_tag_text(description.chain(comment));
            // Tags lofty reads can be edited; the rest only show as read.
            let editable = tags::read(path);
            let mut details = Details::default();
            if editable.is_none() {
                details.add(
                    "Tags",
                    tags.iter()
                        .filter(|t| !matches!(t.raw.value, RawValue::Binary(_) | RawValue::Flag))
                        .map(|t| (t.raw.key.clone(), t.raw.value.to_string()))
                        .collect(),
                );
            }
            Ok(Opened {
                info: Info {
                    container: path
                        .extension()
                        .map_or("audio".into(), |e| e.to_string_lossy().to_uppercase()),
                    sample_rate: coded.sample_rate,
                    channels: coded.channels,
                    bits: coded.bits,
                    frames: coded.frames_hint,
                    bytes,
                },
                meta,
                details,
                source: Source::Coded(path.to_owned()),
                edits: editable.map(Edits::from_tags),
            })
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The whole of `path` read once: the header, the metadata, the levels
/// for the meters, and the spectrogram of the whole file as `spec` asks,
/// about `columns` wide.
pub fn load(
    path: &Path,
    spec: Spec,
    columns: usize,
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Result<(Loaded, Analysis), String> {
    let Opened {
        mut info,
        meta,
        mut details,
        source,
        edits,
    } = open(path)?;
    let channels = usize::from(info.channels);
    let mut levels = Levels::new(info.sample_rate, channels);
    // Compressed headers can be missing or wrong about the length: the
    // analysis starts on trust, and a second pass redoes it if the file
    // turns out different.
    let planned = info.frames;
    let mut analyzer =
        (planned > 0).then(|| Analyzer::new(spec, 0..planned, planned, channels, columns));
    // Whole blocks of levels per read, so no block straddles two.
    let piece = (piece_frames(&source, channels) / levels.block).max(1) * levels.block;
    let mut at = 0;
    // Past where the header says the file ends, pieces wait for reading to
    // get there, so a right header costs no decoding past the end; without
    // one, reading runs ahead as far as anywhere.
    let likely = if planned > 0 { planned } else { usize::MAX };
    let read = pieces(
        &source,
        channels,
        0..usize::MAX,
        likely,
        piece,
        cancel,
        |first, part, got| {
            let part = &part[..got * channels];
            levels.push(part);
            if let Some(analyzer) = &mut analyzer {
                let wanted = analyzer.wanted().end;
                if first < wanted {
                    analyzer.push(first, &part[..(wanted - first).min(got) * channels]);
                }
            }
            at = first + got;
            if planned > 0 {
                report(progress, at, planned);
            }
            got == piece
        },
    )?;
    if !read {
        return Err("cancelled".into());
    }
    if at == 0 {
        return Err("the file holds no audio".into());
    }
    info.frames = at;
    let analysis = match analyzer {
        Some(analyzer) if planned == at => analyzer.finish(),
        _ => analyse(&source, &info, spec, 0..at, columns, cancel, progress)?.ok_or("cancelled")?,
    };
    details
        .sections
        .insert(0, ("File".into(), file_rows(path, &info)));
    Ok((
        Loaded {
            info,
            meta,
            details,
            source,
            levels,
            edits,
        },
        analysis,
    ))
}

/// The spectrogram of `range` as `spec` asks, about `columns` wide, or
/// `None` once `cancel` is set.
pub fn analyse(
    source: &Source,
    info: &Info,
    spec: Spec,
    range: Range<usize>,
    columns: usize,
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Result<Option<Analysis>, String> {
    let channels = usize::from(info.channels);
    let analyzer = Analyzer::new(spec, range, info.frames, channels, columns);
    run(source, info, analyzer, cancel, progress)
}

fn run(
    source: &Source,
    info: &Info,
    mut analyzer: Analyzer,
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Result<Option<Analysis>, String> {
    let channels = usize::from(info.channels);
    let wanted = analyzer.wanted();
    let piece = piece_frames(source, channels).min(wanted.len().max(1));
    let read = pieces(
        source,
        channels,
        wanted.clone(),
        wanted.end,
        piece,
        cancel,
        |first, part, _| {
            analyzer.push(first, part);
            report(
                progress,
                first + part.len() / channels - wanted.start,
                wanted.len(),
            );
            true
        },
    )?;
    Ok(read.then(|| analyzer.finish()))
}

/// Reads `wanted` of `source` in pieces of `piece` frames and hands each to
/// `take` in order: where it starts, its frames, silent past the end of the
/// file, and how many of them the file had. Stops when `take` says to, and
/// is false if `cancel` stopped it. A compressed file's pieces are decoded
/// side by side, those from `likely` on, where the file likely ends, only
/// once reading gets there.
fn pieces(
    source: &Source,
    channels: usize,
    wanted: Range<usize>,
    likely: usize,
    piece: usize,
    cancel: &AtomicBool,
    mut take: impl FnMut(usize, &[f32], usize) -> bool,
) -> Result<bool, String> {
    let count = wanted.len().div_ceil(piece);
    let bounds = |k: usize| {
        let start = wanted.start + k * piece;
        start..(start + piece).min(wanted.end)
    };
    let mut from = 0;
    if let Source::Coded(_) = source {
        match side_by_side(source, channels, count, &bounds, likely, cancel, &mut take) {
            Ok(read) => return Ok(read),
            Err(failed) => from = failed,
        }
    }
    let mut reader = Reader::open(source, channels)?;
    // A compressed file that failed to read from some position is decoded
    // from the top on instead, as one that cannot seek has to be.
    reader.forward = usize::MAX;
    let mut buffer = Vec::new();
    for k in from..count {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let range = bounds(k);
        buffer.resize(range.len() * channels, 0.0);
        let got = reader.read(range.start, &mut buffer)?;
        if !take(range.start, &buffer, got) {
            break;
        }
    }
    Ok(true)
}

/// Decodes the `count` pieces `bounds` gives on rayon's threads, and hands
/// them to `take` in order, as [`pieces`] does. `Err` names the first piece
/// that failed.
fn side_by_side(
    source: &Source,
    channels: usize,
    count: usize,
    bounds: &(impl Fn(usize) -> Range<usize> + Sync),
    likely: usize,
    cancel: &AtomicBool,
    take: &mut impl FnMut(usize, &[f32], usize) -> bool,
) -> Result<bool, usize> {
    let (done, decoded) = mpsc::channel();
    // Readers left by finished pieces, so each is opened once.
    let readers = Mutex::new(Vec::new());
    rayon::in_place_scope(|scope| {
        let start = |k: usize, mut buffer: Vec<f32>| {
            let (done, readers) = (done.clone(), &readers);
            scope.spawn(move |_| {
                let range = bounds(k);
                let join = if k + 1 < count { JOIN } else { 0 };
                buffer.resize((range.len() + join) * channels, 0.0);
                let reader = readers.lock().unwrap_or_else(PoisonError::into_inner).pop();
                // A panic becomes a failed piece, which is read again on the
                // way that raises it where it always did.
                let got =
                    std::panic::catch_unwind(AssertUnwindSafe(|| -> Result<usize, String> {
                        let mut reader = match reader {
                            Some(reader) => reader,
                            None => Reader::open(source, channels)?,
                        };
                        // Pieces are far apart, and seeking to each is quicker
                        // than decoding the way there.
                        reader.forward = 0;
                        let got = reader.read(range.start, &mut buffer)?;
                        readers
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(reader);
                        Ok(got)
                    }))
                    .unwrap_or_else(|_| Err("panicked".into()));
                let _ = done.send((k, got, buffer));
            });
        };
        // Pieces decoded ahead of the one `take` is on, enough to keep every
        // thread busy while it runs; past where the file likely ends, only
        // the next one.
        let early = |k: usize| k == 0 || bounds(k).start < likely;
        let mut started = 0;
        while started < count.min(rayon::current_num_threads() + 2) && early(started) {
            start(started, Vec::new());
            started += 1;
        }
        let mut ready = BTreeMap::new();
        // The frames the piece before decoded past its end.
        let mut before = Vec::new();
        for k in 0..count {
            let (got, mut buffer) = loop {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(false);
                }
                if let Some(piece) = ready.remove(&k) {
                    break piece;
                }
                let Ok((j, got, buffer)) = decoded.recv() else {
                    return Err(k);
                };
                ready.insert(j, (got, buffer));
            };
            let Ok(got) = got else { return Err(k) };
            let range = bounds(k);
            let (piece, after) = buffer.split_at_mut(range.len() * channels);
            // Where the two decodes agree, which is everywhere but noise
            // substituted bands, this leaves the frames as they are.
            let steps = before.len() / channels + 1;
            let joined = piece
                .chunks_exact_mut(channels)
                .zip(before.chunks_exact(channels));
            for (i, (now, then)) in joined.enumerate() {
                let weight = (i + 1) as f32 / steps as f32;
                for (now, then) in now.iter_mut().zip(then) {
                    *now = then + weight * (*now - then);
                }
            }
            if !take(range.start, piece, got.min(range.len())) {
                return Ok(true);
            }
            before.clear();
            before.extend_from_slice(after);
            if started < count && (early(started) || started == k + 1) {
                start(started, buffer);
                started += 1;
            }
        }
        Ok(true)
    })
}

/// Frames of `channels` channels in one piece of `source`.
fn piece_frames(source: &Source, channels: usize) -> usize {
    let samples = match source {
        Source::Pcm { .. } => PIECE_SAMPLES,
        Source::Coded(_) => CODED_PIECE_SAMPLES,
    };
    (samples / channels.max(1)).max(1)
}

fn report(progress: &AtomicU32, done: usize, total: usize) {
    let permille = (done as f64 / total.max(1) as f64 * 1000.0).min(1000.0);
    progress.store(permille as u32, Ordering::Relaxed);
}

pub fn file_rows(path: &Path, info: &Info) -> Vec<(String, String)> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let folder = path.parent().map(|p| p.display().to_string());
    let bits = info.bits.map(|b| format!(", {b}-bit")).unwrap_or_default();
    vec![
        ("Name".into(), name.unwrap_or_default()),
        ("Folder".into(), folder.unwrap_or_default()),
        (
            "Size".into(),
            format!("{:.1} MB", info.bytes as f64 / 1_000_000.0),
        ),
        ("Format".into(), format!("{}{bits}", info.container)),
        ("Sample rate".into(), format!("{} Hz", info.sample_rate)),
        ("Channels".into(), info.channels.to_string()),
        (
            "Length".into(),
            format!("{:.3} s, {} frames", info.seconds(), info.frames),
        ),
    ]
}

/// A file opened for reading frames at any position, every channel
/// interleaved.
pub struct Reader {
    channels: usize,
    origin: Origin,
    /// How far ahead a compressed file is decoded to, rather than seeking.
    forward: usize,
}

enum Origin {
    Pcm(Pcm),
    Coded(Box<Decoded>),
}

impl Reader {
    pub fn open(source: &Source, channels: usize) -> Result<Self, String> {
        let origin = match source {
            Source::Pcm { path, data, kind } => Origin::Pcm(Pcm {
                file: File::open(path).map_err(|e| format!("cannot open: {e}"))?,
                data: data.clone(),
                kind: *kind,
                channels,
                bytes: Vec::new(),
            }),
            Source::Coded(path) => Origin::Coded(Box::new(Decoded {
                coded: Coded::open(path)?,
                channels,
                history: VecDeque::new(),
                queue: VecDeque::new(),
                from: 0,
                ended: false,
            })),
        };
        let forward = match &origin {
            Origin::Coded(decoded) => FORWARD_SECONDS * decoded.coded.sample_rate as usize,
            Origin::Pcm(_) => 0,
        };
        Ok(Self {
            channels,
            origin,
            forward,
        })
    }

    /// Fills `out` with the frames from `first` on and says how many the
    /// file had there; past its end, `out` is silence.
    pub fn read(&mut self, first: usize, out: &mut [f32]) -> Result<usize, String> {
        debug_assert_eq!(out.len() % self.channels, 0);
        match &mut self.origin {
            Origin::Pcm(pcm) => pcm.read(first, out),
            Origin::Coded(decoded) => decoded.read(first, out, self.forward),
        }
    }
}

struct Pcm {
    file: File,
    data: Range<u64>,
    kind: SampleKind,
    channels: usize,
    bytes: Vec<u8>,
}

impl Pcm {
    fn read(&mut self, first: usize, out: &mut [f32]) -> Result<usize, String> {
        let (ch, width) = (self.channels, self.kind.bytes());
        let frame = ch * width;
        let frames = usize::try_from(self.data.end - self.data.start).unwrap_or(usize::MAX) / frame;
        let available = frames.saturating_sub(first).min(out.len() / ch);
        let (wanted, rest) = out.split_at_mut(available * ch);
        rest.fill(0.0);
        let start = self.data.start + (first * frame) as u64;
        let failed = |e: std::io::Error| format!("cannot read: {e}");
        if available < PARALLEL_FRAMES {
            self.bytes.resize(available * frame, 0);
            self.file
                .seek(SeekFrom::Start(start))
                .and_then(|_| self.file.read_exact(&mut self.bytes))
                .map_err(failed)?;
            self.kind.decode_all(&self.bytes, wanted);
            return Ok(available);
        }
        // The workers share the handle, each reading its own part at its
        // own offset, so the reads overlap as a memory map's would, but a
        // card pulled out mid-read ends in an error instead of a crash.
        let (file, kind) = (&self.file, self.kind);
        wanted
            .par_chunks_mut(PARALLEL_FRAMES * ch)
            .enumerate()
            .try_for_each_init(Vec::new, |bytes: &mut Vec<u8>, (i, part)| {
                bytes.resize(part.len() * width, 0);
                read_at(file, bytes, start + (i * PARALLEL_FRAMES * frame) as u64)?;
                kind.decode_all(bytes, part);
                Ok(())
            })
            .map_err(failed)?;
        Ok(available)
    }
}

/// Fills `buf` from `offset` on without using the handle's position, so
/// threads can read through one handle at once.
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let (mut buf, mut offset) = (buf, offset);
        while !buf.is_empty() {
            match file.seek_read(buf, offset) {
                Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// A compressed file read by position: decoded forward while reads move
/// forward, and seeking only for jumps back or far ahead.
struct Decoded {
    coded: Coded,
    channels: usize,
    /// The last frames read, interleaved, up to `from`.
    history: VecDeque<f32>,
    /// Decoded frames not yet read, interleaved: the rest of the packet the
    /// last read ended in.
    queue: VecDeque<f32>,
    /// The file's frame at the front of `queue`.
    from: usize,
    ended: bool,
}

impl Decoded {
    fn read(&mut self, first: usize, out: &mut [f32], forward: usize) -> Result<usize, String> {
        let ch = self.channels;
        let (kept, queued) = (self.history.len() / ch, self.queue.len() / ch);
        if first.saturating_add(kept) < self.from
            || first > (self.from + queued).saturating_add(forward)
        {
            // From a little before, so a codec whose frames overlap decodes
            // `first` as reading from the top does.
            let from = first.saturating_sub(LEAD_IN);
            self.coded.seek(from)?;
            self.history.clear();
            self.queue.clear();
            self.from = from;
            self.ended = false;
        }
        // A read that starts a little before the last one ended, as one
        // whose steps overlap does, takes what it can from that one.
        let back = self.from.saturating_sub(first);
        let (behind, ahead) = out.split_at_mut((back * ch).min(out.len()));
        let old = self.history.len() - back * ch;
        for (o, s) in behind.iter_mut().zip(self.history.range(old..)) {
            *o = *s;
        }
        if ahead.is_empty() {
            return Ok(behind.len() / ch);
        }
        if first > self.from {
            self.history.clear();
        }
        let got = self.read_on(first + back, ahead)?;
        let fresh = &ahead[..got * ch];
        self.history
            .extend(&fresh[fresh.len().saturating_sub(HISTORY * ch)..]);
        let over = self.history.len().saturating_sub(HISTORY * ch);
        self.history.drain(..over);
        Ok(back + got)
    }

    /// [`Self::read`] from where the last read ended or later.
    fn read_on(&mut self, first: usize, out: &mut [f32]) -> Result<usize, String> {
        let ch = self.channels;
        let skip = (first - self.from).min(self.queue.len() / ch);
        self.queue.drain(..skip * ch);
        self.from += skip;
        let mut filled = 0;
        if self.from == first {
            filled = (self.queue.len() / ch).min(out.len() / ch);
            for (o, s) in out.iter_mut().zip(self.queue.drain(..filled * ch)) {
                *o = s;
            }
            self.from += filled;
        }
        let max_gap = MAX_GAP_SECONDS * self.coded.sample_rate as usize;
        while (self.from < first || filled < out.len() / ch) && !self.ended {
            let Some(block) = self.coded.next()? else {
                self.ended = true;
                break;
            };
            // After a seek the first packet usually starts before `from`.
            let skip = self.from.saturating_sub(block.at);
            let silence = block.at.saturating_sub(self.from).min(max_gap);
            let mut sink = Sink {
                first,
                out: &mut *out,
                filled,
                channels: ch,
                at: self.from,
                queue: &mut self.queue,
            };
            sink.put(silence, None);
            let samples = block
                .samples
                .get(skip * block.channels..)
                .unwrap_or_default();
            sink.put(
                samples.len() / block.channels,
                Some((samples, block.channels)),
            );
            (filled, self.from) = (sink.filled, sink.at);
        }
        out[filled * ch..].fill(0.0);
        Ok(filled)
    }
}

/// Where decoded frames go, in order from frame `at` on: those before
/// `first` are dropped, then `out` fills, and the rest wait in `queue`.
struct Sink<'a> {
    first: usize,
    out: &'a mut [f32],
    filled: usize,
    channels: usize,
    /// The frame the next one handed out lands on, or the first in `queue`
    /// once `out` is full.
    at: usize,
    queue: &'a mut VecDeque<f32>,
}

impl Sink<'_> {
    /// Hands on `frames` frames: silence, or interleaved samples with their
    /// own channel count.
    fn put(&mut self, frames: usize, samples: Option<(&[f32], usize)>) {
        let ch = self.channels;
        let dropped = frames.min(self.first.saturating_sub(self.at));
        let given = (frames - dropped).min(self.out.len() / ch - self.filled);
        let out = &mut self.out[self.filled * ch..(self.filled + given) * ch];
        match samples {
            None => {
                out.fill(0.0);
                let kept = frames - dropped - given;
                self.queue.extend(std::iter::repeat_n(0.0, kept * ch));
            }
            Some((samples, from)) => {
                let (given_part, kept_part) = samples[dropped * from..].split_at(given * from);
                if from == ch {
                    out.copy_from_slice(given_part);
                    self.queue.extend(kept_part);
                } else {
                    // Missing channels repeat the last one the file has.
                    for (o, frame) in out.chunks_exact_mut(ch).zip(given_part.chunks_exact(from)) {
                        for (c, o) in o.iter_mut().enumerate() {
                            *o = frame[c.min(from - 1)];
                        }
                    }
                    for frame in kept_part.chunks_exact(from) {
                        self.queue.extend((0..ch).map(|c| frame[c.min(from - 1)]));
                    }
                }
            }
        }
        self.filled += given;
        self.at += dropped + given;
    }
}

/// Decoded audio starting at frame `at` of the file's timeline.
pub struct Block<'a> {
    pub at: usize,
    pub channels: usize,
    /// Interleaved.
    pub samples: &'a [f32],
}

/// A file symphonia reads, decoded packet by packet onto the track's own
/// timeline, so every reader agrees on where each sample sits.
pub struct Coded {
    path: PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track: u32,
    /// Track ticks at frame 0.
    start: i64,
    /// Seconds per tick, as a fraction, times the sample rate.
    frames_per_tick: (i128, i128),
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: Option<u16>,
    pub frames_hint: usize,
    /// Where the next block starts, or `None` after a seek: then the first
    /// packet decoded says where it is.
    next: Option<usize>,
    /// The packet a seek read to see where it landed.
    pending: Option<Packet>,
    decoded: Vec<f32>,
}

fn unsupported(e: DecodeError) -> String {
    format!("unsupported or damaged file: {e}")
}

impl Coded {
    /// The next packet of the track as the file stores it, undecoded: when
    /// it starts, and its bytes.
    pub fn raw_packet(&mut self) -> Result<Option<(i64, Vec<u8>)>, String> {
        loop {
            match self.format.next_packet() {
                Ok(Some(p)) if p.track_id == self.track => {
                    return Ok(Some((p.pts.get(), p.data.to_vec())));
                }
                Ok(Some(_)) => {}
                Ok(None) => return Ok(None),
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                stream,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(unsupported)?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or("no audio track")?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or("no audio parameters")?
            .clone();
        let sample_rate = params.sample_rate.ok_or("unknown sample rate")?;
        let (numer, denom) = track
            .time_base
            .map_or((1, sample_rate), |tb| (tb.numer.get(), tb.denom.get()));
        let frames_hint = usize::try_from(track.num_frames.unwrap_or(0)).unwrap_or(0);
        // A track said to start before 0, as a Vorbis one does by its
        // encoder's delay, still sounds from 0: the decoder drops the delay.
        let (track, start) = (track.id, track.start_ts.get().max(0));
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(unsupported)?;
        Ok(Self {
            path: path.to_owned(),
            format,
            decoder,
            track,
            start,
            frames_per_tick: (
                i128::from(numer) * i128::from(sample_rate),
                i128::from(denom),
            ),
            sample_rate,
            channels: params
                .channels
                .as_ref()
                .map_or(1, |c| u16::try_from(c.count()).unwrap_or(u16::MAX))
                .max(1),
            bits: params.bits_per_sample.and_then(|b| u16::try_from(b).ok()),
            frames_hint,
            next: Some(0),
            pending: None,
            decoded: Vec::new(),
        })
    }

    /// The frame of the timeline `ts` falls on, negative before the start.
    fn frame_of(&self, ts: Timestamp) -> i64 {
        let (per, ticks) = self.frames_per_tick;
        let elapsed = i128::from(ts.get()) - i128::from(self.start);
        (elapsed * per / ticks).clamp(i64::MIN.into(), i64::MAX.into()) as i64
    }

    /// Where the first frame decoded from `packet` falls: after what the
    /// decoder trims off its start as the encoder's delay.
    fn start_of(&self, packet: &Packet) -> i64 {
        self.frame_of(
            packet
                .pts
                .checked_add(packet.trim_start)
                .unwrap_or(packet.pts),
        )
    }

    /// The next packet of the track.
    fn packet(&mut self) -> Result<Option<Packet>, String> {
        if let Some(packet) = self.pending.take() {
            return Ok(Some(packet));
        }
        loop {
            match self.format.next_packet().map_err(unsupported)? {
                Some(packet) if packet.track_id == self.track => return Ok(Some(packet)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    fn timestamp_of(&self, frame: usize) -> Timestamp {
        let (per, ticks) = self.frames_per_tick;
        let elapsed = i128::try_from(frame).unwrap_or(i128::MAX) * ticks / per;
        Timestamp::new(i64::try_from(elapsed + i128::from(self.start)).unwrap_or(i64::MAX))
    }

    /// Continues from the packet that holds `frame`, or one before it.
    ///
    /// symphonia 0.6.1's FLAC reader keeps stale parser state when a seek
    /// lands on a frame it has seen before, and the next packet then fails
    /// with an unexpected end of file (pdeljanov/Symphonia#564), so a FLAC
    /// seek starts from a freshly opened file, as one after a failed seek
    /// does. Its Matroska reader lands up to a cluster past where it is
    /// asked to, and fails in the last cluster: a seek that lands past
    /// `frame` or fails is tried again from further back, and the frames on
    /// to `frame` decoded.
    pub fn seek(&mut self, frame: usize) -> Result<(), String> {
        let rate = self.sample_rate as usize;
        let (mut back, mut failed) = (0, false);
        loop {
            let target = frame.saturating_sub(back);
            if target == 0 || failed || self.format.format_info().format == FORMAT_ID_FLAC {
                *self = Self::open(&self.path)?;
            } else {
                self.decoder.reset();
                self.pending = None;
            }
            if target == 0 {
                return Ok(());
            }
            let to = SeekTo::Timestamp {
                ts: self.timestamp_of(target),
                track_id: self.track,
            };
            let landed = match self.format.seek(SeekMode::Accurate, to) {
                Ok(_) => {
                    self.next = None;
                    self.pending = self.packet()?;
                    self.pending.as_ref().map(|p| self.start_of(p))
                }
                Err(e) if back >= MAX_BACK_SECONDS * rate => return Err(unsupported(e)),
                Err(_) => {
                    failed = true;
                    Some(i64::MAX)
                }
            };
            if landed.is_none_or(|at| at <= i64::try_from(frame).unwrap_or(i64::MAX)) {
                return Ok(());
            }
            back = if back == 0 { rate } else { back * 2 };
        }
    }

    /// The next stretch of decoded audio, or `None` at the end.
    ///
    /// Each packet lands at its own timestamp, so a damaged packet that
    /// fails to decode leaves a gap, which reads as silence, instead of
    /// pulling everything after it earlier, and overlapping audio is
    /// dropped.
    pub fn next(&mut self) -> Result<Option<Block<'_>>, String> {
        loop {
            let Some(packet) = self.packet()? else {
                return Ok(None);
            };
            let at = self.start_of(&packet);
            let buffer = match self.decoder.decode(&packet) {
                Ok(buffer) => buffer,
                Err(DecodeError::DecodeError(_)) => continue,
                Err(e) => return Err(unsupported(e)),
            };
            let channels = buffer.spec().channels().count().max(1);
            self.decoded.resize(buffer.samples_interleaved(), 0.0);
            buffer.copy_to_slice_interleaved(&mut self.decoded);
            // Frames before the start of the timeline are dropped.
            let early = usize::try_from(at.saturating_neg())
                .unwrap_or(0)
                .min(self.decoded.len() / channels);
            let (at, frames) = (
                usize::try_from(at).unwrap_or(0),
                self.decoded.len() / channels - early,
            );

            let start = self.next.unwrap_or(at);
            let max_gap = MAX_GAP_SECONDS * self.sample_rate as usize;
            let (gap, skip) = match at.checked_sub(start) {
                Some(ahead) if ahead <= max_gap => (ahead, 0),
                Some(_) => (0, 0),
                None => (0, (start - at).min(frames)),
            };
            self.next = Some(start + gap + frames - skip);
            return Ok(Some(Block {
                at: start + gap,
                channels,
                samples: &self.decoded[(early + skip) * channels..],
            }));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::spectrogram::{Channels, DEFAULT_COLUMNS};

    /// A 16-bit mono AIFF: a format symphonia reads, so it goes through
    /// [`Coded`] rather than the WAV reader.
    pub fn aiff(samples: &[i16], rate: u32) -> Vec<u8> {
        // AIFF stores the rate as an 80-bit extended float.
        let exponent = 16_383 + 31 - rate.leading_zeros() as u16;
        let mantissa = u64::from(rate) << (32 + rate.leading_zeros());
        let mut comm = Vec::new();
        comm.extend_from_slice(&1u16.to_be_bytes());
        comm.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        comm.extend_from_slice(&16u16.to_be_bytes());
        comm.extend_from_slice(&exponent.to_be_bytes());
        comm.extend_from_slice(&mantissa.to_be_bytes());
        let mut ssnd = vec![0u8; 8];
        ssnd.extend(samples.iter().flat_map(|s| s.to_be_bytes()));

        let mut body = b"AIFF".to_vec();
        for (id, chunk) in [(b"COMM", comm), (b"SSND", ssnd)] {
            body.extend_from_slice(id);
            body.extend_from_slice(&(chunk.len() as u32).to_be_bytes());
            body.extend(chunk);
        }
        let mut file = b"FORM".to_vec();
        file.extend_from_slice(&(body.len() as u32).to_be_bytes());
        file.extend(body);
        file
    }

    pub fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("soundcheck-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn mix() -> Spec {
        Spec {
            fft: 1024,
            channels: Channels::Mix,
        }
    }

    pub fn idle() -> (AtomicBool, AtomicU32) {
        (AtomicBool::new(false), AtomicU32::new(0))
    }

    #[test]
    fn a_wav_loads_with_levels_and_its_spectrogram() {
        let samples: Vec<i16> = (0..96_000)
            .flat_map(|i| [(i % 200) as i16 * 100, 0])
            .collect();
        let path = temp_file(
            "load.wav",
            &wav::tests::build_channels(false, 2, &[], &samples),
        );
        let (cancel, progress) = idle();
        let (loaded, analysis) = load(&path, mix(), DEFAULT_COLUMNS, &cancel, &progress).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(loaded.info.frames, 96_000);
        assert_eq!(analysis.envelope.len(), 2);
        assert_eq!(analysis.range, 0..96_000);
        assert_eq!(progress.load(Ordering::Relaxed), 1000);
        let level = loaded.levels.at(50_000, 4_800);
        let loudest = crate::levels::db(19_900.0 / 32_768.0);
        assert!((level[0].peak_db - loudest).abs() < 0.01);
        assert_eq!(level[1].peak_db, crate::levels::FLOOR_DB);
        assert_eq!(loaded.details.sections[0].0, "File");
    }

    #[test]
    fn pcm_reads_the_same_in_one_go_or_in_parallel_pieces() {
        let samples: Vec<i16> = (0..200_000).map(|i| (i % 30_000) as i16).collect();
        let path = temp_file("pieces.wav", &wav::tests::build(false, &[], &samples));
        let (cancel, progress) = idle();
        let (loaded, _) = load(&path, mix(), DEFAULT_COLUMNS, &cancel, &progress).unwrap();
        let mut reader = Reader::open(&loaded.source, 1).unwrap();
        let mut big = vec![0.0; 150_000];
        assert_eq!(reader.read(40_000, &mut big).unwrap(), 150_000);
        let mut small = vec![0.0; 1_000];
        assert_eq!(reader.read(40_000 + 120_000, &mut small).unwrap(), 1_000);
        assert_eq!(&big[120_000..121_000], &small[..]);
        let mut tail = vec![1.0; 10];
        assert_eq!(reader.read(199_995, &mut tail).unwrap(), 5);
        assert_eq!(&tail[5..], [0.0; 5]);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(big[0], 10_000.0 / 32_768.0);
    }

    #[test]
    fn coded_files_decode_onto_a_timeline_and_read_from_anywhere() {
        let samples: Vec<i16> = (0..30_000).map(|i| (i % 20_000) as i16).collect();
        let path = temp_file("timeline.aiff", &aiff(&samples, 48_000));
        let (cancel, progress) = idle();
        let (loaded, analysis) = load(&path, mix(), DEFAULT_COLUMNS, &cancel, &progress).unwrap();
        assert!(matches!(loaded.source, Source::Coded(_)));
        assert_eq!((loaded.info.frames, analysis.range.end), (30_000, 30_000));

        let mut reader = Reader::open(&loaded.source, 1).unwrap();
        let expect = |frame: usize| f32::from(samples[frame]) / 32_768.0;
        let mut out = [0.0; 700];
        for first in [0, 700, 21_234, 5, 29_500] {
            let got = reader.read(first, &mut out).unwrap();
            assert_eq!(got, 700.min(30_000 - first));
            assert_eq!(out[0], expect(first), "reading from {first}");
            assert_eq!(out[got - 1], expect(first + got - 1));
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_compressed_file_decoded_in_pieces_side_by_side_reads_as_its_wav_does() {
        // Several pieces long, in a format symphonia reads.
        let samples: Vec<i16> = (0..1_700_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 16) as i16 / 3)
            .collect();
        let aiff = temp_file("side.aiff", &aiff(&samples, 48_000));
        let wav = temp_file("side.wav", &wav::tests::build(false, &[], &samples));
        let (cancel, progress) = idle();
        let (coded, a) = load(&aiff, mix(), DEFAULT_COLUMNS, &cancel, &progress).unwrap();
        let (pcm, b) = load(&wav, mix(), DEFAULT_COLUMNS, &cancel, &progress).unwrap();
        assert!(matches!(coded.source, Source::Coded(_)));
        assert_eq!(coded.info.frames, pcm.info.frames);
        assert!(a.planes == b.planes && a.envelope == b.envelope);
        for at in [0, 700_000, 1_699_999] {
            assert_eq!(coded.levels.at(at, 4_800), pcm.levels.at(at, 4_800));
        }
        let range = 400_000..1_300_000;
        let zoom = |l: &Loaded| {
            analyse(
                &l.source,
                &l.info,
                mix(),
                range.clone(),
                DEFAULT_COLUMNS,
                &cancel,
                &progress,
            )
            .unwrap()
            .unwrap()
        };
        assert!(zoom(&coded).planes == zoom(&pcm).planes);
        std::fs::remove_file(&aiff).unwrap();
        std::fs::remove_file(&wav).unwrap();
    }

    #[test]
    fn reads_that_step_back_a_little_or_a_long_way_get_the_frames_they_ask_for() {
        let samples: Vec<i16> = (0..200_000).map(|i| (i % 30_000) as i16).collect();
        let path = temp_file("steps.aiff", &aiff(&samples, 48_000));
        let mut reader = Reader::open(&Source::Coded(path.clone()), 1).unwrap();
        let expect = |frame: usize| f32::from(samples[frame]) / 32_768.0;
        let mut out = vec![0.0; 4_096];
        // Steps overlapping by three quarters, as playback's band filter
        // takes them, then back further than is kept, and to the end.
        for first in [0, 1_024, 2_048, 3_072, 100_000, 101_024, 60_000, 199_000] {
            let got = reader.read(first, &mut out).unwrap();
            assert_eq!(got, 4_096.min(200_000 - first));
            for i in [0, got / 2, got - 1] {
                assert_eq!(
                    out[i],
                    expect(first + i),
                    "frame {i} of the read from {first}"
                );
            }
            assert!(out[got..].iter().all(|&v| v == 0.0));
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_flac_read_to_the_end_can_seek_back_to_the_start() {
        // 9600 frames of 8 kHz mono silence in three FLAC frames.
        const FLAC: &[u8] = b"fLaC\x80\x00\x00\x22\x12\x00\x12\x00\x00\x00\x0b\x00\x00\x0d\x01\xf4\x00\xf0\
            \x00\x00\x25\x80\x28\x1d\x1d\xf6\xa4\xca\xe2\x9b\x12\x7d\xd6\x17\xfe\x46\x1c\xe4\xff\xf8\x54\x08\
            \x00\xad\x00\x00\x00\xd5\x67\xff\xf8\x54\x08\x01\xaa\x00\x00\x00\xb9\x1f\xff\xf8\x74\x08\x02\x01\
            \x7f\x2c\x00\x00\x00\xf5\xff";
        fn read_all(coded: &mut Coded) -> usize {
            let mut frames = 0;
            while let Some(block) = coded.next().unwrap() {
                frames += block.samples.len() / block.channels;
            }
            frames
        }
        let path = temp_file("rewind.flac", FLAC);
        let mut coded = Coded::open(&path).unwrap();
        assert_eq!(read_all(&mut coded), 9600);
        coded.seek(0).unwrap();
        assert_eq!(read_all(&mut coded), 9600);
        coded.seek(5000).unwrap();
        assert!(coded.next().unwrap().is_some_and(|b| b.at <= 5000));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_empty_or_unknown_file_is_an_error_not_a_panic() {
        let (cancel, progress) = idle();
        let path = temp_file("empty.wav", &wav::tests::build(false, &[], &[]));
        assert!(load(&path, mix(), DEFAULT_COLUMNS, &cancel, &progress).is_err());
        std::fs::remove_file(&path).unwrap();
        let path = temp_file("noise.bin", &[7u8; 4096]);
        assert!(load(&path, mix(), DEFAULT_COLUMNS, &cancel, &progress).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
