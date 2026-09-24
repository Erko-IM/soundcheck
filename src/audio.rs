//! Opening a file and reading its samples: our own reader for the WAV
//! family, symphonia for the rest. Nothing holds a whole recording in
//! memory; analysis, meters, the spectrum and playback all read the file as
//! they go.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rayon::prelude::*;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, RawValue, StandardTag};
use symphonia::core::units::Timestamp;

use crate::edit::Edits;
use crate::levels::Levels;
use crate::meta::{self, Details, Meta};
use crate::spectrogram::{Analysis, Analyzer, Spec};
use crate::wav::{self, SampleKind};

/// A timestamp further ahead than this is a damaged one, not a gap.
const MAX_GAP_SECONDS: usize = 10;
/// Frames read at a time by a pass over a file or a range of it.
const PIECE: usize = 1 << 18;
/// PCM reads of more frames than this are split across threads.
const PARALLEL_FRAMES: usize = 1 << 15;
/// A compressed file is decoded forward over a jump this short, and seeks
/// past a longer one.
const FORWARD_SECONDS: usize = 20;

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
    /// Per channel, the extremes of each column across the whole file.
    pub timeline: Vec<Vec<[f32; 2]>>,
    /// What can be edited; only WAV files can be saved into.
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
        Ok(w) => Ok(Opened {
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
            edits: Some(Edits::from_wav(&w)),
        }),
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
            let mut details = Details::default();
            details.add(
                "Tags",
                tags.iter()
                    .filter(|t| !matches!(t.raw.value, RawValue::Binary(_) | RawValue::Flag))
                    .map(|t| (t.raw.key.clone(), t.raw.value.to_string()))
                    .collect(),
            );
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
                edits: None,
            })
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The whole of `path` read once: the header, the metadata, the levels
/// for the meters, and the spectrogram of the whole file as `spec` asks.
pub fn load(
    path: &Path,
    spec: Spec,
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
    let mut reader = Reader::open(&source, channels)?;
    let mut levels = Levels::new(info.sample_rate, channels);
    // Compressed headers can be missing or wrong about the length: the
    // analysis starts on trust, and a second pass redoes it if the file
    // turns out different.
    let planned = info.frames;
    let mut analyzer = (planned > 0).then(|| Analyzer::new(spec, 0..planned, planned, channels));
    // Whole blocks of levels per read, so no block straddles two.
    let piece = (PIECE / levels.block).max(1) * levels.block;
    let mut buffer = vec![0.0; piece * channels];
    let mut at = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let got = reader.read(at, &mut buffer)?;
        let part = &buffer[..got * channels];
        levels.push(part);
        if let Some(analyzer) = &mut analyzer {
            let wanted = analyzer.wanted().end;
            if at < wanted {
                analyzer.push(at, &part[..(wanted - at).min(got) * channels]);
            }
        }
        at += got;
        if planned > 0 {
            report(progress, at, planned);
        }
        if got < piece {
            break;
        }
    }
    if at == 0 {
        return Err("the file holds no audio".into());
    }
    info.frames = at;
    let analysis = match analyzer {
        Some(analyzer) if planned == at => analyzer.finish(),
        _ => analyse(&source, &info, spec, 0..at, cancel, progress)?.ok_or("cancelled")?,
    };
    details
        .sections
        .insert(0, ("File".into(), file_rows(path, &info)));
    let timeline = analysis.envelope.clone();
    Ok((
        Loaded {
            info,
            meta,
            details,
            source,
            levels,
            timeline,
            edits,
        },
        analysis,
    ))
}

/// The spectrogram of `range` as `spec` asks, or `None` once `cancel` is
/// set.
pub fn analyse(
    source: &Source,
    info: &Info,
    spec: Spec,
    range: Range<usize>,
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Result<Option<Analysis>, String> {
    let channels = usize::from(info.channels);
    let mut reader = Reader::open(source, channels)?;
    let mut analyzer = Analyzer::new(spec, range, info.frames, channels);
    let wanted = analyzer.wanted();
    let mut buffer = vec![0.0; PIECE * channels];
    let mut at = wanted.start;
    while at < wanted.end {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = PIECE.min(wanted.end - at);
        let part = &mut buffer[..n * channels];
        reader.read(at, part)?;
        analyzer.push(at, part);
        at += n;
        report(progress, at - wanted.start, wanted.len());
    }
    Ok(Some(analyzer.finish()))
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
}

enum Origin {
    Pcm(Pcm),
    Coded(Box<Decoded>),
}

impl Reader {
    pub fn open(source: &Source, channels: usize) -> Result<Self, String> {
        let origin = match source {
            Source::Pcm { path, data, kind } => Origin::Pcm(Pcm {
                path: path.clone(),
                file: File::open(path).map_err(|e| format!("cannot open: {e}"))?,
                data: data.clone(),
                kind: *kind,
                channels,
                bytes: Vec::new(),
            }),
            Source::Coded(path) => {
                let coded = Coded::open(path)?;
                Origin::Coded(Box::new(Decoded {
                    forward: FORWARD_SECONDS * coded.sample_rate as usize,
                    coded,
                    channels,
                    queue: VecDeque::new(),
                    from: 0,
                    ended: false,
                }))
            }
        };
        Ok(Self { channels, origin })
    }

    /// Fills `out` with the frames from `first` on and says how many the
    /// file had there; past its end, `out` is silence.
    pub fn read(&mut self, first: usize, out: &mut [f32]) -> Result<usize, String> {
        debug_assert_eq!(out.len() % self.channels, 0);
        match &mut self.origin {
            Origin::Pcm(pcm) => pcm.read(first, out),
            Origin::Coded(decoded) => decoded.read(first, out),
        }
    }
}

struct Pcm {
    path: PathBuf,
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
        // Each worker reads through a handle of its own, so the reads
        // overlap as a memory map's would, but a card pulled out mid-read
        // ends in an error instead of a crash.
        let (path, kind) = (&self.path, self.kind);
        wanted
            .par_chunks_mut(PARALLEL_FRAMES * ch)
            .enumerate()
            .try_for_each_init(
                || (None, Vec::new()),
                |(file, bytes): &mut (Option<File>, Vec<u8>), (i, part)| {
                    let file = match file {
                        Some(file) => file,
                        None => file.insert(File::open(path)?),
                    };
                    bytes.resize(part.len() * width, 0);
                    file.seek(SeekFrom::Start(
                        start + (i * PARALLEL_FRAMES * frame) as u64,
                    ))?;
                    file.read_exact(bytes)?;
                    kind.decode_all(bytes, part);
                    Ok(())
                },
            )
            .map_err(failed)?;
        Ok(available)
    }
}

/// A compressed file read by position: decoded forward while reads move
/// forward, and seeking only for jumps back or far ahead.
struct Decoded {
    coded: Coded,
    channels: usize,
    /// Decoded frames not yet read, interleaved.
    queue: VecDeque<f32>,
    /// The file's frame at the front of `queue`.
    from: usize,
    ended: bool,
    forward: usize,
}

impl Decoded {
    fn read(&mut self, first: usize, out: &mut [f32]) -> Result<usize, String> {
        let ch = self.channels;
        let queued = self.queue.len() / ch;
        if first < self.from || first > self.from + queued + self.forward {
            self.coded.seek(first)?;
            self.queue.clear();
            self.from = first;
            self.ended = false;
        }
        while self.from < first {
            let queued = self.queue.len() / ch;
            if queued == 0 {
                if !self.decode()? {
                    break;
                }
                continue;
            }
            let skip = (first - self.from).min(queued);
            self.queue.drain(..skip * ch);
            self.from += skip;
        }
        let wanted = out.len() / ch;
        while self.queue.len() / ch < wanted && self.decode()? {}
        let ready = (self.queue.len() / ch).min(wanted);
        for (o, s) in out.iter_mut().zip(self.queue.drain(..ready * ch)) {
            *o = s;
        }
        out[ready * ch..].fill(0.0);
        self.from += ready;
        Ok(ready)
    }

    /// Queues the next packet's frames, or says the file has ended.
    fn decode(&mut self) -> Result<bool, String> {
        if self.ended {
            return Ok(false);
        }
        let max_gap = MAX_GAP_SECONDS * self.coded.sample_rate as usize;
        let Some(block) = self.coded.next()? else {
            self.ended = true;
            return Ok(false);
        };
        let ch = self.channels;
        let end = self.from + self.queue.len() / ch;
        // After a seek the first packet usually starts before `from`.
        let skip = end.saturating_sub(block.at);
        let silence = block.at.saturating_sub(end).min(max_gap);
        self.queue.extend(std::iter::repeat_n(0.0, silence * ch));
        let last = block.channels - 1;
        for frame in block.samples.chunks_exact(block.channels).skip(skip) {
            self.queue.extend((0..ch).map(|c| frame[c.min(last)]));
        }
        Ok(true)
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
    decoded: Vec<f32>,
    block: Vec<f32>,
}

fn unsupported(e: DecodeError) -> String {
    format!("unsupported or damaged file: {e}")
}

impl Coded {
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
        let (track, start) = (track.id, track.start_ts.get());
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
            decoded: Vec::new(),
            block: Vec::new(),
        })
    }

    fn frame_of(&self, ts: Timestamp) -> usize {
        let (per, ticks) = self.frames_per_tick;
        let elapsed = i128::from(ts.get()) - i128::from(self.start);
        usize::try_from(elapsed * per / ticks).unwrap_or(0)
    }

    fn timestamp_of(&self, frame: usize) -> Timestamp {
        let (per, ticks) = self.frames_per_tick;
        let elapsed = i128::try_from(frame).unwrap_or(i128::MAX) * ticks / per;
        Timestamp::new(i64::try_from(elapsed + i128::from(self.start)).unwrap_or(i64::MAX))
    }

    /// Continues from the packet that holds `frame`, or the nearest one
    /// before it.
    ///
    /// Each seek starts from a freshly opened file. symphonia 0.6.1's FLAC
    /// reader keeps stale parser state when a seek lands on a frame it has
    /// seen before, and the next packet then fails with an unexpected end
    /// of file (pdeljanov/Symphonia#564).
    pub fn seek(&mut self, frame: usize) -> Result<(), String> {
        *self = Self::open(&self.path)?;
        if frame == 0 {
            return Ok(());
        }
        let to = SeekTo::Timestamp {
            ts: self.timestamp_of(frame),
            track_id: self.track,
        };
        self.format
            .seek(SeekMode::Accurate, to)
            .map_err(unsupported)?;
        self.next = None;
        Ok(())
    }

    /// The next stretch of decoded audio, or `None` at the end.
    ///
    /// Each packet lands at its own timestamp, so a damaged packet that
    /// fails to decode leaves a silent gap instead of pulling everything
    /// after it earlier, and overlapping audio is dropped.
    pub fn next(&mut self) -> Result<Option<Block<'_>>, String> {
        loop {
            let Some(packet) = self.format.next_packet().map_err(unsupported)? else {
                return Ok(None);
            };
            if packet.track_id != self.track {
                continue;
            }
            let at = self.frame_of(packet.pts);
            let buffer = match self.decoder.decode(&packet) {
                Ok(buffer) => buffer,
                Err(DecodeError::DecodeError(_)) => continue,
                Err(e) => return Err(unsupported(e)),
            };
            let channels = buffer.spec().channels().count().max(1);
            self.decoded.resize(buffer.samples_interleaved(), 0.0);
            buffer.copy_to_slice_interleaved(&mut self.decoded);
            let frames = self.decoded.len() / channels;

            let start = self.next.unwrap_or(at);
            let max_gap = MAX_GAP_SECONDS * self.sample_rate as usize;
            let (gap, skip) = match at.checked_sub(start) {
                Some(ahead) if ahead <= max_gap => (ahead, 0),
                Some(_) => (0, 0),
                None => (0, (start - at).min(frames)),
            };
            self.block.clear();
            self.block.resize(gap * channels, 0.0);
            self.block
                .extend_from_slice(&self.decoded[skip * channels..]);
            self.next = Some(start + gap + frames - skip);
            return Ok(Some(Block {
                at: start,
                channels,
                samples: &self.block,
            }));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::spectrogram::Channels;

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

    fn idle() -> (AtomicBool, AtomicU32) {
        (AtomicBool::new(false), AtomicU32::new(0))
    }

    #[test]
    fn a_wav_loads_with_levels_timeline_and_spectrogram() {
        let samples: Vec<i16> = (0..96_000)
            .flat_map(|i| [(i % 200) as i16 * 100, 0])
            .collect();
        let path = temp_file(
            "load.wav",
            &wav::tests::build_channels(false, 2, &[], &samples),
        );
        let (cancel, progress) = idle();
        let (loaded, analysis) = load(&path, mix(), &cancel, &progress).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(loaded.info.frames, 96_000);
        assert_eq!(loaded.timeline.len(), 2);
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
        let (loaded, _) = load(&path, mix(), &cancel, &progress).unwrap();
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
        let (loaded, analysis) = load(&path, mix(), &cancel, &progress).unwrap();
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
        assert!(load(&path, mix(), &cancel, &progress).is_err());
        std::fs::remove_file(&path).unwrap();
        let path = temp_file("noise.bin", &[7u8; 4096]);
        assert!(load(&path, mix(), &cancel, &progress).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
