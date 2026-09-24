//! Opening a file: our own reader for the WAV family, symphonia for the rest.

use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::units::Timestamp;

use crate::meta::{self, Meta};
use crate::wav::{self, SampleKind};

/// A timestamp further ahead than this is a damaged one, not a gap.
const MAX_GAP_SECONDS: usize = 10;

pub struct Info {
    pub container: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: Option<u16>,
    pub frames: usize,
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

/// Where playback reads its samples: always the file itself, so a long
/// recording is never held in memory a second time.
#[derive(Clone)]
pub enum Source {
    /// PCM, read straight from the WAV.
    Pcm {
        path: PathBuf,
        data: Range<u64>,
        kind: SampleKind,
        channels: usize,
    },
    /// Anything symphonia reads, decoded again from where playback starts.
    Coded(PathBuf),
}

pub struct Loaded {
    pub info: Info,
    pub meta: Meta,
    /// Channels averaged together, shared read-only with analysis threads.
    pub mono: Arc<Vec<f32>>,
    pub source: Source,
}

/// Reads and decodes `path`, stopping early once `cancel` is set.
pub fn load(path: &Path, cancel: &AtomicBool) -> Result<Loaded, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
    let loaded = match wav::parse(&mut file) {
        Ok(w) => Loaded {
            info: Info {
                container: w.container.to_owned(),
                sample_rate: w.sample_rate,
                channels: w.channels,
                bits: Some(w.kind.bits()),
                frames: w.frames(),
            },
            meta: meta::from_wav(&w),
            mono: Arc::new(
                w.mono(|| File::open(path), cancel)
                    .map_err(|e| format!("cannot read: {e}"))?,
            ),
            source: Source::Pcm {
                path: path.to_owned(),
                data: w.data.clone(),
                kind: w.kind,
                channels: usize::from(w.channels),
            },
        },
        Err(wav::Error::NotWav) => decode_other(path, cancel)?,
        Err(e) => return Err(e.to_string()),
    };
    if loaded.info.frames == 0 {
        return Err("the file holds no audio".into());
    }
    Ok(loaded)
}

fn decode_other(path: &Path, cancel: &AtomicBool) -> Result<Loaded, String> {
    let mut coded = Coded::open(path)?;
    let mut mono = Vec::with_capacity(coded.frames_hint);
    while let Some(block) = coded.next()? {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let scale = 1.0 / block.channels as f32;
        mono.extend(
            block
                .samples
                .chunks_exact(block.channels)
                .map(|frame| frame.iter().sum::<f32>() * scale),
        );
    }

    let tags = coded
        .format
        .metadata()
        .skip_to_latest()
        .map(|r| r.media.tags.clone())
        .unwrap_or_default();
    let standard = |want: fn(&StandardTag) -> Option<&str>| {
        tags.iter()
            .filter_map(move |t| t.std.as_ref().and_then(want))
    };
    let description = standard(|t| match t {
        StandardTag::Description(s) => Some(s.as_str()),
        _ => None,
    });
    let comment = standard(|t| match t {
        StandardTag::Comment(s) => Some(s.as_str()),
        _ => None,
    });

    Ok(Loaded {
        info: Info {
            container: path
                .extension()
                .map_or("audio".into(), |e| e.to_string_lossy().to_uppercase()),
            sample_rate: coded.sample_rate,
            channels: coded.channels,
            bits: coded.bits,
            frames: mono.len(),
        },
        meta: meta::from_tag_text(description.chain(comment)),
        mono: Arc::new(mono),
        source: Source::Coded(path.to_owned()),
    })
}

/// Decoded audio starting at frame `at` of the file's timeline.
pub struct Block<'a> {
    pub at: usize,
    pub channels: usize,
    /// Interleaved.
    pub samples: &'a [f32],
}

/// A file symphonia reads, decoded packet by packet onto the track's own
/// timeline, so analysis and playback agree on where every sample sits.
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
        // The header's length only sizes the first allocation, so a damaged
        // one cannot demand gigabytes up front.
        let frames_hint = usize::try_from(track.num_frames.unwrap_or(0).min(1 << 28)).unwrap_or(0);
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
                .map_or(1, |c| u16::try_from(c.count()).unwrap_or(u16::MAX)),
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

    #[test]
    fn coded_files_decode_onto_a_timeline_and_seek_within_it() {
        let samples: Vec<i16> = (0..20_000).map(|i| (i % 30_000) as i16).collect();
        let path = temp_file("timeline.aiff", &aiff(&samples, 48_000));
        let loaded = load(&path, &AtomicBool::new(false)).unwrap();
        assert!(matches!(loaded.source, Source::Coded(_)));
        assert_eq!(loaded.mono.len(), samples.len());
        assert_eq!(loaded.mono[12_345], 12_345.0 / 32_768.0);

        let mut coded = Coded::open(&path).unwrap();
        coded.seek(15_000).unwrap();
        let block = coded.next().unwrap().unwrap();
        let first = block.at;
        let offset = 15_000 - first;
        assert!(first <= 15_000);
        assert_eq!(block.samples[offset], 15_000.0 / 32_768.0);
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
        let path = temp_file("empty.wav", &wav::tests::build(false, &[], &[]));
        assert!(load(&path, &AtomicBool::new(false)).is_err());
        std::fs::remove_file(&path).unwrap();
        let path = temp_file("noise.bin", &[7u8; 4096]);
        assert!(load(&path, &AtomicBool::new(false)).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
