//! Converting recordings to another format, a file or a batch at a time.
//! Each new file goes next to its original under a name no file there has,
//! and the original stays as it was. The audio comes from the readers the
//! rest of the window uses, and every tag, picture and marker the original
//! has goes along, under the names the new format gives them: see
//! [`carry`].
//!
//! A conversion writes a hidden file first and reads it back before it
//! takes its name: a lossless format has to give back exactly the samples
//! that went in, a lossy one the whole length, and every format the tags
//! that went in.

mod alac;
mod carry;
mod flac;
mod matroska;
mod md5;
mod mp3;
mod mp4;
mod pcm;
mod vorbis;

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use serde::{Deserialize, Serialize};

use crate::audio::{self, Coding, Opened, Reader, Source};
use crate::wav::SampleKind;

use carry::Carried;

/// Frames read and written at a time.
const PIECE: usize = 1 << 16;
/// Thousandths of a conversion's progress spent writing; reading the new
/// file back takes the rest.
const WRITING: u32 = 800;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    Wav,
    Aiff,
    Caf,
    Flac,
    M4a,
    Mka,
    Mp3,
    Ogg,
    Webm,
}

impl Format {
    pub const ALL: [Self; 9] = [
        Self::Wav,
        Self::Aiff,
        Self::Caf,
        Self::Flac,
        Self::M4a,
        Self::Mka,
        Self::Mp3,
        Self::Ogg,
        Self::Webm,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Wav => "WAV",
            Self::Aiff => "AIFF",
            Self::Caf => "CAF",
            Self::Flac => "FLAC",
            Self::M4a => "M4A (ALAC)",
            Self::Mka => "MKA (FLAC)",
            Self::Mp3 => "MP3",
            Self::Ogg => "Ogg Vorbis",
            Self::Webm => "WebM (Vorbis)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Aiff => "aiff",
            Self::Caf => "caf",
            Self::Flac => "flac",
            Self::M4a => "m4a",
            Self::Mka => "mka",
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
            Self::Webm => "webm",
        }
    }

    pub fn lossy(self) -> bool {
        matches!(self, Self::Mp3 | Self::Ogg | Self::Webm)
    }

    /// Whether it keeps samples as 32-bit floats as well as whole numbers.
    pub fn takes_float(self) -> bool {
        matches!(self, Self::Wav | Self::Aiff | Self::Caf)
    }

    /// Whether it keeps pictures: CAF has no place for one, and WebM takes
    /// no attachments.
    pub fn takes_pictures(self) -> bool {
        !matches!(self, Self::Caf | Self::Webm)
    }

    /// The most channels it holds.
    fn most_channels(self) -> usize {
        match self {
            Self::Mp3 => 2,
            Self::Flac | Self::M4a | Self::Mka => 8,
            Self::Ogg | Self::Webm => 255,
            Self::Wav | Self::Aiff | Self::Caf => usize::from(u16::MAX),
        }
    }
}

/// How a lossless format keeps each sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Depth {
    /// As many bits as the original: whole numbers stay as wide, a float
    /// stays a float where the format takes one and becomes 24 bits where
    /// it does not, and a lossy file becomes 16 bits.
    AsFile,
    Int16,
    Int24,
    Float32,
}

impl Depth {
    pub fn name(self) -> &'static str {
        match self {
            Self::AsFile => "As the file",
            Self::Int16 => "16-bit",
            Self::Int24 => "24-bit",
            Self::Float32 => "32-bit float",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mp3 {
    V0,
    V2,
    Cbr320,
    Cbr256,
    Cbr192,
    Cbr128,
}

impl Mp3 {
    pub const ALL: [Self; 6] = [
        Self::V0,
        Self::V2,
        Self::Cbr320,
        Self::Cbr256,
        Self::Cbr192,
        Self::Cbr128,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::V0 => "VBR V0, about 245 kbps",
            Self::V2 => "VBR V2, about 190 kbps",
            Self::Cbr320 => "320 kbps",
            Self::Cbr256 => "256 kbps",
            Self::Cbr192 => "192 kbps",
            Self::Cbr128 => "128 kbps",
        }
    }
}

/// libvorbis's quality scale, as its usual steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Vorbis {
    Q3,
    Q5,
    Q6,
    Q8,
    Q10,
}

impl Vorbis {
    pub const ALL: [Self; 5] = [Self::Q3, Self::Q5, Self::Q6, Self::Q8, Self::Q10];

    pub fn name(self) -> &'static str {
        match self {
            Self::Q3 => "Quality 3, about 112 kbps",
            Self::Q5 => "Quality 5, about 160 kbps",
            Self::Q6 => "Quality 6, about 192 kbps",
            Self::Q8 => "Quality 8, about 256 kbps",
            Self::Q10 => "Quality 10, about 500 kbps",
        }
    }

    fn quality(self) -> f32 {
        match self {
            Self::Q3 => 0.3,
            Self::Q5 => 0.5,
            Self::Q6 => 0.6,
            Self::Q8 => 0.8,
            Self::Q10 => 1.0,
        }
    }
}

/// What to convert to: the format, and the one setting that matters for
/// it, the others kept for when the format changes back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Target {
    pub format: Format,
    pub depth: Depth,
    pub mp3: Mp3,
    pub vorbis: Vorbis,
}

impl Default for Target {
    fn default() -> Self {
        Self {
            format: Format::Flac,
            depth: Depth::AsFile,
            mp3: Mp3::V0,
            vorbis: Vorbis::Q6,
        }
    }
}

/// What a new file keeps in each sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sample {
    Int(u32),
    Float,
}

/// A recording to convert, as far as planning it goes.
#[derive(Clone, Debug, Default)]
pub struct Header {
    pub rate: u32,
    pub channels: usize,
    pub coding: Coding,
    /// What it holds that some formats have no place for.
    pub pictures: usize,
    pub markers: usize,
    pub marker_lengths: bool,
    pub marker_notes: bool,
}

/// Reads what converting `path` needs to know before it starts.
pub fn header(path: &Path) -> Result<Header, String> {
    let opened = audio::open(path)?;
    let carried = carry::read(path, &opened)?;
    Ok(Header {
        rate: opened.info.sample_rate,
        channels: usize::from(opened.info.channels),
        coding: opened.info.coding,
        pictures: carried.pictures.len(),
        markers: carried.markers.len(),
        marker_lengths: carried.markers.iter().any(|m| m.length > 0),
        marker_notes: carried.markers.iter().any(|m| !m.note.is_empty()),
    })
}

/// The shape of the new file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    pub rate: u32,
    pub channels: usize,
    pub sample: Sample,
    /// How the original keeps its samples.
    pub from: Coding,
}

impl Shape {
    /// Whether making whole numbers of the original's samples can need its
    /// level lowered: a float or a lossy decode can run past full scale.
    pub fn needs_peak(&self) -> bool {
        matches!(self.sample, Sample::Int(_)) && !matches!(self.from, Coding::Int(_))
    }
}

/// One file to convert, as worked out before converting.
#[derive(Clone, Debug)]
pub struct Plan {
    pub from: PathBuf,
    /// Next to the original, under a name no file there has, nor any other
    /// file of the same batch.
    pub to: PathBuf,
    pub shape: Shape,
    /// What will change, said before anything is made: another rate or
    /// depth, and whatever the format has no place for.
    pub changes: Vec<String>,
}

/// The new file's shape for an original with `header`, or why there can be
/// none.
pub fn shape(header: &Header, target: &Target) -> Result<(Shape, Vec<String>), String> {
    let format = target.format;
    let most = format.most_channels();
    if header.channels > most {
        return Err(format!(
            "{} holds at most {most} channels; this file has {}",
            format.name(),
            header.channels
        ));
    }
    // What symphonia reads of a FLAC, which soundcheck opens one through.
    if matches!(format, Format::Flac | Format::Mka) && header.rate > 655_350 {
        return Err(format!(
            "FLAC keeps rates up to 655.35 kHz; this file is {}: WAV, CAF or M4A keep it",
            khz(header.rate)
        ));
    }
    let mut changes = Vec::new();
    let (rate, why) = match format {
        Format::Mp3 => (mp3_rate(header.rate), "MP3 keeps 8 to 48 kHz, in steps"),
        Format::Ogg | Format::Webm => (vorbis_rate(header.rate), "Vorbis keeps up to 200 kHz"),
        _ => (header.rate, ""),
    };
    if rate != header.rate {
        changes.push(format!("{} from {} ({why})", khz(rate), khz(header.rate)));
    }
    let sample = if format.lossy() {
        Sample::Float
    } else {
        let sample = match (target.depth, header.coding) {
            (Depth::Int16, _) => Sample::Int(16),
            (Depth::Int24, _) => Sample::Int(24),
            (Depth::Float32, _) if format.takes_float() => Sample::Float,
            (Depth::Float32, _) => Sample::Int(24),
            (Depth::AsFile, Coding::Int(bits)) => Sample::Int(u32::from(bits)),
            (Depth::AsFile, Coding::Float) if format.takes_float() => Sample::Float,
            (Depth::AsFile, Coding::Float) => Sample::Int(24),
            (Depth::AsFile, Coding::Lossy) => Sample::Int(16),
        };
        let sample = match (format, sample) {
            // ALAC keeps 16, 20, 24 or 32 bits.
            (Format::M4a, Sample::Int(bits)) if bits <= 16 => Sample::Int(16),
            (Format::M4a, Sample::Int(bits)) if bits <= 20 => Sample::Int(20),
            (Format::M4a, Sample::Int(bits)) if bits <= 24 => Sample::Int(24),
            (Format::M4a, Sample::Int(_)) => Sample::Int(32),
            // FLAC and the PCM formats take any width, the PCM ones in
            // whole bytes; symphonia reads no 8-bit CAF.
            (Format::Flac | Format::Mka, Sample::Int(bits)) => Sample::Int(bits.clamp(4, 32)),
            (Format::Caf, Sample::Int(bits)) => Sample::Int(bits.div_ceil(8).clamp(2, 4) * 8),
            (_, Sample::Int(bits)) => Sample::Int(bits.div_ceil(8).clamp(1, 4) * 8),
            (_, Sample::Float) => Sample::Float,
        };
        let was = match header.coding {
            Coding::Int(bits) => Some(format!("{bits}-bit")),
            Coding::Float => Some("32-bit float".into()),
            Coding::Lossy => None,
        };
        let now = match sample {
            Sample::Int(bits) => format!("{bits}-bit"),
            Sample::Float => "32-bit float".into(),
        };
        match was {
            Some(was) if was != now => changes.push(format!("{now} from {was}")),
            None => changes.push(now),
            Some(_) => {}
        }
        sample
    };
    if header.coding == Coding::Lossy && format.lossy() {
        changes.push("encoded again from a lossy file".into());
    }
    if header.pictures > 0 && !format.takes_pictures() {
        changes.push(format!("{} has no place for its pictures", format.name()));
    }
    let lost = match format {
        Format::M4a | Format::Caf => [header.marker_lengths, header.marker_notes],
        Format::Mka | Format::Webm => [false, header.marker_notes],
        _ => [false, false],
    };
    let lost: Vec<&str> = ["lengths", "notes"]
        .into_iter()
        .zip(lost)
        .filter_map(|(what, lost)| lost.then_some(what))
        .collect();
    if !lost.is_empty() {
        changes.push(format!(
            "its markers' {} have no place in {}",
            lost.join(" and "),
            format.name()
        ));
    }
    if format == Format::M4a && header.markers > 255 {
        changes.push("M4A keeps its first 255 markers".into());
    }
    Ok((
        Shape {
            rate,
            channels: header.channels,
            sample,
            from: header.coding,
        },
        changes,
    ))
}

/// The rates an MP3 can have.
const MP3_RATES: [u32; 9] = [
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000,
];

/// The rate an MP3 of a `rate` original is written at: its own where MP3
/// has it, the next one up below 48 kHz, and past that whichever of 44.1
/// and 48 kHz it is a multiple of.
fn mp3_rate(rate: u32) -> u32 {
    match rate {
        r if r > 48_000 && r % 11_025 == 0 => 44_100,
        r => MP3_RATES.into_iter().find(|&m| m >= r).unwrap_or(48_000),
    }
}

/// The rate a Vorbis stream of a `rate` original is written at: libvorbis
/// takes up to 200 kHz, so a higher rate is halved until it fits, which
/// keeps what is heard and a little more, and the ratio simple.
fn vorbis_rate(rate: u32) -> u32 {
    let mut r = rate;
    while r > 200_000 {
        r /= 2;
    }
    r
}

fn khz(rate: u32) -> String {
    let khz = f64::from(rate) / 1000.0;
    if khz.fract() == 0.0 {
        format!("{khz:.0} kHz")
    } else {
        format!("{khz} kHz")
    }
}

/// The path a conversion of `from` to `format` would take: the original's
/// name with the new extension, numbered past any file already there, any
/// in `taken`, and the original itself.
pub fn path_for(from: &Path, format: Format, taken: &HashSet<PathBuf>) -> PathBuf {
    let stem = from.file_stem().unwrap_or_default().to_string_lossy();
    let ext = format.extension();
    let free = |p: &Path| p != from && !p.exists() && !taken.contains(p);
    let first = from.with_file_name(format!("{stem}.{ext}"));
    if free(&first) {
        return first;
    }
    (2..)
        .map(|n| from.with_file_name(format!("{stem} {n}.{ext}")))
        .find(|p| free(p))
        .expect("some number is free")
}

/// The level a file peaks at: the largest sample, at full scale 1.
pub fn peak(path: &Path, cancel: &AtomicBool) -> Result<f32, String> {
    let opened = audio::open(path)?;
    let mut input = Input::open(&opened)?;
    let mut peak = 0.0f32;
    let mut buffer = Vec::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let Some(piece) = input.next(&mut buffer)? else {
            return Ok(peak);
        };
        peak = match piece {
            Piece::Float(samples) => samples.iter().fold(peak, |p, s| p.max(s.abs())),
            Piece::Int(samples, bits) => {
                let most = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
                peak.max(most as f32 / (1u64 << (bits - 1)) as f32)
            }
        };
    }
}

/// The gain that brings a file peaking at `peak` down to full scale, as
/// whole numbers need, or `None` when it fits as it is.
pub fn fit(peak: f32) -> Option<f32> {
    (peak > 1.0).then(|| 1.0 / peak)
}

pub fn decibels(gain: f32) -> String {
    format!("{:.2} dB", 20.0 * gain.log10())
}

/// Converts `plan`'s original to `target`, its level lowered by `gain` on
/// the way, and puts the new file where the plan says or, if a file has
/// taken that name since, under the next free one. Returns where it went.
pub fn convert(
    plan: &Plan,
    target: &Target,
    gain: Option<f32>,
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Result<PathBuf, String> {
    let opened = audio::open(&plan.from)?;
    let mut carried = carry::read(&plan.from, &opened)?;
    if let Some(gain) = gain {
        carried.note_gain(gain);
    }
    carried.rescale(plan.shape.rate);
    let temp = temp_path(&plan.to);
    // Only ever a conversion of ours that was cut off.
    let _ = fs::remove_file(&temp);
    let done = (|| {
        let written = write(
            &opened, &carried, plan, target, gain, &temp, cancel, progress,
        )?;
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        verify(&temp, plan, target, &written, &carried, cancel, progress)?;
        keep_dates_and_finder_tags(&plan.from, &temp)?;
        place(&temp, &plan.to)
    })();
    if done.is_err() {
        let _ = fs::remove_file(&temp);
    }
    progress.store(1000, Ordering::Relaxed);
    done
}

/// A hidden name in the same folder, keeping the new file's extension so
/// that reading it back finds its format as the finished file's would be.
fn temp_path(to: &Path) -> PathBuf {
    let stem = to.file_stem().unwrap_or_default().to_string_lossy();
    let ext = to.extension().unwrap_or_default().to_string_lossy();
    to.with_file_name(format!(".{stem}.converting.{ext}"))
}

/// Samples as a piece of the original gives them.
enum Piece<'a> {
    /// Whole numbers this many bits wide, read straight from a WAV.
    Int(&'a [i32], u32),
    Float(&'a [f32]),
}

/// The original's samples, in pieces from the top: a WAV's whole numbers as
/// they are stored, so none wider than a float holds exactly is rounded,
/// and anything else as the window's reader decodes it.
enum Input {
    Pcm {
        file: File,
        left: u64,
        kind: SampleKind,
        channels: usize,
        bytes: Vec<u8>,
        ints: Vec<i32>,
    },
    Decoded {
        reader: Box<Reader>,
        channels: usize,
        at: usize,
        ended: bool,
    },
}

impl Input {
    fn open(opened: &Opened) -> Result<Self, String> {
        let channels = usize::from(opened.info.channels);
        Ok(match &opened.source {
            Source::Pcm { path, data, kind } if !matches!(kind, SampleKind::F64) => {
                let mut file = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
                file.seek(SeekFrom::Start(data.start))
                    .map_err(|e| format!("cannot read: {e}"))?;
                Self::Pcm {
                    file,
                    left: data.end - data.start,
                    kind: *kind,
                    channels,
                    bytes: Vec::new(),
                    ints: Vec::new(),
                }
            }
            source => Self::Decoded {
                reader: Box::new(Reader::open(source, channels)?),
                channels,
                at: 0,
                ended: false,
            },
        })
    }

    /// The next piece, or `None` at the end. Floats go in `buffer`.
    fn next<'a>(&'a mut self, buffer: &'a mut Vec<f32>) -> Result<Option<Piece<'a>>, String> {
        match self {
            Self::Pcm {
                file,
                left,
                kind,
                channels,
                bytes,
                ints,
            } => {
                let frame = kind.bytes() * *channels;
                let want = (PIECE as u64 * frame as u64).min(*left / frame as u64 * frame as u64);
                if want == 0 {
                    return Ok(None);
                }
                bytes.resize(want as usize, 0);
                file.read_exact(bytes)
                    .map_err(|e| format!("cannot read: {e}"))?;
                *left -= want;
                if *kind == SampleKind::F32 {
                    buffer.resize(bytes.len() / 4, 0.0);
                    kind.decode_all(bytes, buffer);
                    return Ok(Some(Piece::Float(buffer)));
                }
                ints.clear();
                match kind {
                    SampleKind::U8 => ints.extend(bytes.iter().map(|&b| i32::from(b) - 128)),
                    SampleKind::I16 => ints.extend(
                        bytes
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|s| i32::from(i16::from_le_bytes(*s))),
                    ),
                    SampleKind::I24 => ints.extend(
                        bytes
                            .as_chunks::<3>()
                            .0
                            .iter()
                            .map(|&[a, b, c]| i32::from_le_bytes([0, a, b, c]) >> 8),
                    ),
                    SampleKind::I32 => ints.extend(
                        bytes
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|s| i32::from_le_bytes(*s)),
                    ),
                    SampleKind::F32 | SampleKind::F64 => unreachable!("read as floats"),
                }
                Ok(Some(Piece::Int(ints, u32::from(kind.bits()))))
            }
            Self::Decoded {
                reader,
                channels,
                at,
                ended,
            } => {
                if *ended {
                    return Ok(None);
                }
                buffer.resize(PIECE * *channels, 0.0);
                let got = reader.read(*at, buffer)?;
                *at += got;
                if got < PIECE {
                    *ended = true;
                    buffer.truncate(got * *channels);
                    if got == 0 {
                        return Ok(None);
                    }
                }
                Ok(Some(Piece::Float(buffer)))
            }
        }
    }
}

/// Samples as an encoder takes them, interleaved, whole frames.
pub(crate) enum Frames<'a> {
    /// Whole numbers as wide as the new file keeps them.
    Int(&'a [i32]),
    Float(&'a [f32]),
}

pub(crate) trait Encode {
    fn push(&mut self, frames: Frames<'_>) -> Result<(), String>;

    /// Writes what is left: the end of the audio, the sizes headers wait
    /// for, and anything else the format keeps after the audio.
    fn finish(self: Box<Self>) -> Result<(), String>;
}

/// What an encoder is told about the audio it gets.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Spec {
    pub rate: u32,
    pub channels: usize,
    pub sample: Sample,
}

/// What a conversion wrote, for reading it back against.
struct Written {
    frames: usize,
    /// The samples as reading the new file back gives them, hashed: for a
    /// lossless format only.
    hash: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
fn write(
    opened: &Opened,
    carried: &Carried,
    plan: &Plan,
    target: &Target,
    gain: Option<f32>,
    temp: &Path,
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Result<Written, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(temp)
        .map_err(|e| format!("cannot write the new file: {e}"))?;
    let shape = plan.shape;
    let spec = Spec {
        rate: shape.rate,
        channels: shape.channels,
        sample: shape.sample,
    };
    let mut encoder: Box<dyn Encode> = match target.format {
        Format::Wav => Box::new(pcm::Wav::new(file, spec, carried)?),
        Format::Aiff => Box::new(pcm::Aiff::new(file, spec, carried)?),
        Format::Caf => Box::new(pcm::Caf::new(file, spec, carried)?),
        Format::Flac => Box::new(flac::Writer::new(file, spec, carried)?),
        Format::M4a => Box::new(mp4::Writer::new(file, spec, carried)?),
        Format::Mka => Box::new(matroska::Writer::flac(file, spec, carried)?),
        Format::Mp3 => Box::new(mp3::Writer::new(file, spec, target.mp3, carried)?),
        Format::Ogg => Box::new(vorbis::Writer::ogg(
            file,
            temp,
            spec,
            target.vorbis.quality(),
            carried,
        )?),
        Format::Webm => Box::new(matroska::Writer::vorbis(
            file,
            temp,
            spec,
            target.vorbis.quality(),
            carried,
        )?),
    };
    let mut feed = Feed::new(
        &shape,
        usize::from(opened.info.channels),
        opened.info.sample_rate,
        gain,
        !target.format.lossy(),
    )?;
    let mut input = Input::open(opened)?;
    let total = opened.info.frames.max(1);
    let mut buffer = Vec::new();
    let mut read = 0;
    while let Some(piece) = input.next(&mut buffer)? {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        read += feed.push(piece, &mut *encoder)?;
        let done = (read as f64 / total as f64).min(1.0);
        progress.store((done * f64::from(WRITING)) as u32, Ordering::Relaxed);
    }
    feed.finish(&mut *encoder)?;
    encoder.finish()?;
    Ok(Written {
        frames: feed.frames,
        hash: feed.hash.map(|h| h.0),
    })
}

/// Hashes samples as reading them back as floats gives them.
#[derive(Clone, Copy)]
struct Hash(u64);

impl Hash {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn add(&mut self, samples: &[f32]) {
        for s in samples {
            self.0 = (self.0.rotate_left(5) ^ u64::from(s.to_bits()))
                .wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
}

/// Turns the original's samples into what the encoder takes: lowered to
/// fit, resampled for a format that keeps no such rate, and made whole
/// numbers, with dither wherever that drops bits the original had.
struct Feed {
    channels: usize,
    sample: Sample,
    gain: f32,
    /// Bits the original's samples carry: past what the new file keeps,
    /// they are dithered away rather than cut off.
    precision: u32,
    dither: Dither,
    resampler: Option<Resample>,
    hash: Option<Hash>,
    frames: usize,
    ints: Vec<i32>,
    floats: Vec<f32>,
}

impl Feed {
    fn new(
        shape: &Shape,
        channels: usize,
        rate: u32,
        gain: Option<f32>,
        lossless: bool,
    ) -> Result<Self, String> {
        Ok(Self {
            channels,
            sample: shape.sample,
            gain: gain.unwrap_or(1.0),
            precision: match shape.from {
                Coding::Int(bits) => u32::from(bits),
                // A float's 24 bits of mantissa, and a little more for
                // what a decoder works out.
                Coding::Float | Coding::Lossy => 25,
            },
            dither: Dither(0x9e37_79b9_7f4a_7c15),
            resampler: (shape.rate != rate)
                .then(|| Resample::new(rate, shape.rate, channels))
                .transpose()?,
            hash: lossless.then(Hash::new),
            frames: 0,
            ints: Vec::new(),
            floats: Vec::new(),
        })
    }

    /// Hands a piece on, and says how many of the original's frames it had.
    fn push(&mut self, piece: Piece<'_>, encoder: &mut dyn Encode) -> Result<usize, String> {
        let read = match &piece {
            Piece::Int(s, _) => s.len(),
            Piece::Float(s) => s.len(),
        } / self.channels;
        match (piece, self.sample) {
            (Piece::Int(samples, bits), Sample::Int(to)) => {
                self.ints.clear();
                if to >= bits {
                    self.ints.extend(samples.iter().map(|&s| s << (to - bits)));
                } else {
                    let (shift, top) = (bits - to, (1i64 << (to - 1)) - 1);
                    let step = (1i64 << shift) as f64;
                    for &s in samples {
                        let q = ((f64::from(s) + self.dither.next() * step) / step).round() as i64;
                        self.ints.push(q.clamp(-top - 1, top) as i32);
                    }
                }
                self.hand_ints(to, encoder)?;
            }
            (Piece::Int(samples, bits), Sample::Float) => {
                let scale = 1.0 / (1u64 << (bits - 1)) as f32;
                self.floats.clear();
                self.floats
                    .extend(samples.iter().map(|&s| s as f32 * scale));
                let floats = std::mem::take(&mut self.floats);
                self.hand_floats(&floats, encoder)?;
                self.floats = floats;
            }
            (Piece::Float(samples), Sample::Int(to)) => {
                let full = (1u64 << (to - 1)) as f64;
                let top = full as i64 - 1;
                let gain = f64::from(self.gain);
                let dither = to < self.precision || self.gain != 1.0;
                self.ints.clear();
                for &s in samples {
                    let noise = if dither { self.dither.next() } else { 0.0 };
                    let q = (f64::from(s) * gain * full + noise).round() as i64;
                    self.ints.push(q.clamp(-top - 1, top) as i32);
                }
                self.hand_ints(to, encoder)?;
            }
            (Piece::Float(samples), Sample::Float) => {
                if self.gain == 1.0 {
                    self.hand_floats(samples, encoder)?;
                } else {
                    let floats: Vec<f32> = samples.iter().map(|s| s * self.gain).collect();
                    self.hand_floats(&floats, encoder)?;
                }
            }
        }
        Ok(read)
    }

    fn hand_ints(&mut self, bits: u32, encoder: &mut dyn Encode) -> Result<(), String> {
        if let Some(hash) = &mut self.hash {
            let scale = 1.0 / (1u64 << (bits - 1)) as f32;
            let back: Vec<f32> = self.ints.iter().map(|&q| q as f32 * scale).collect();
            hash.add(&back);
        }
        self.frames += self.ints.len() / self.channels;
        encoder.push(Frames::Int(&self.ints))
    }

    fn hand_floats(&mut self, samples: &[f32], encoder: &mut dyn Encode) -> Result<(), String> {
        let mut out = Vec::new();
        let samples = match &mut self.resampler {
            Some(resampler) => {
                resampler.push(samples, &mut out)?;
                &out[..]
            }
            None => samples,
        };
        if let Some(hash) = &mut self.hash {
            hash.add(samples);
        }
        self.frames += samples.len() / self.channels;
        encoder.push(Frames::Float(samples))
    }

    fn finish(&mut self, encoder: &mut dyn Encode) -> Result<(), String> {
        if let Some(resampler) = &mut self.resampler {
            let mut out = Vec::new();
            resampler.finish(&mut out)?;
            self.frames += out.len() / self.channels;
            encoder.push(Frames::Float(&out))?;
        }
        Ok(())
    }
}

/// Triangular dither one step of the new file's resolution wide, from a
/// generator seeded the same for every file, so a conversion done twice
/// comes out the same.
struct Dither(u64);

impl Dither {
    fn uniform(&mut self) -> f64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn next(&mut self) -> f64 {
        self.uniform() - self.uniform()
    }
}

/// A sample rate change, for an MP3 or a Vorbis stream of a recording made
/// at a rate it does not keep: the frames come out in step with the
/// original's, with none lost at either end.
struct Resample {
    resampler: Fft<f32>,
    channels: usize,
    from: u32,
    to: u32,
    /// Frames waiting for a whole chunk.
    pending: Vec<f32>,
    /// Frames of the start the resampler's delay still has to drop.
    delay: usize,
    taken: usize,
    given: usize,
    out: Vec<f32>,
}

impl Resample {
    fn new(from: u32, to: u32, channels: usize) -> Result<Self, String> {
        let resampler =
            Fft::<f32>::new(from as usize, to as usize, 4096, channels, FixedSync::Input)
                .map_err(|e| format!("cannot resample {from} Hz to {to} Hz: {e}"))?;
        let delay = resampler.output_delay();
        let out = vec![0.0; resampler.output_frames_max() * channels];
        Ok(Self {
            resampler,
            channels,
            from,
            to,
            pending: Vec::new(),
            delay,
            taken: 0,
            given: 0,
            out,
        })
    }

    fn push(&mut self, samples: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        self.pending.extend_from_slice(samples);
        self.taken += samples.len() / self.channels;
        loop {
            let need = self.resampler.input_frames_next();
            if self.pending.len() < need * self.channels {
                return Ok(());
            }
            let chunk: Vec<f32> = self.pending.drain(..need * self.channels).collect();
            self.run(&chunk, out)?;
        }
    }

    fn run(&mut self, chunk: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        let ch = self.channels;
        let frames = chunk.len() / ch;
        let input = InterleavedSlice::new(chunk, ch, frames).map_err(|e| e.to_string())?;
        let room = self.out.len() / ch;
        let mut output =
            InterleavedSlice::new_mut(&mut self.out, ch, room).map_err(|e| e.to_string())?;
        let (_, made) = self
            .resampler
            .process_into_buffer(&input, &mut output, None)
            .map_err(|e| format!("resampling failed: {e}"))?;
        let skip = self.delay.min(made);
        self.delay -= skip;
        out.extend_from_slice(&self.out[skip * ch..made * ch]);
        self.given += made - skip;
        Ok(())
    }

    /// The frames still owed: as many as the original's length at the new
    /// rate, the resampler fed silence until it has given them.
    fn finish(&mut self, out: &mut Vec<f32>) -> Result<(), String> {
        let owed = (self.taken as u128 * u128::from(self.to) / u128::from(self.from)) as usize;
        while self.given < owed {
            let need = self.resampler.input_frames_next();
            let mut chunk = std::mem::take(&mut self.pending);
            chunk.resize(need * self.channels, 0.0);
            let before = out.len();
            self.run(&chunk, out)?;
            if out.len() == before && self.delay == 0 {
                break;
            }
        }
        out.truncate(out.len() - (self.given.saturating_sub(owed)) * self.channels);
        self.given = self.given.min(owed);
        Ok(())
    }
}

/// Reads the new file back: its samples have to hash as those written, or
/// for a lossy format come to the length written; and its tags have to say
/// what was meant to go in.
fn verify(
    temp: &Path,
    plan: &Plan,
    target: &Target,
    written: &Written,
    carried: &Carried,
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Result<(), String> {
    let wrong = |what: &str| format!("the new file came out wrong ({what}), so it was not kept");
    let opened = audio::open(temp).map_err(|e| wrong(&e))?;
    let shape = plan.shape;
    if (opened.info.sample_rate, usize::from(opened.info.channels)) != (shape.rate, shape.channels)
    {
        return Err(wrong("format"));
    }
    let channels = shape.channels;
    let mut reader = Reader::open(&opened.source, channels).map_err(|e| wrong(&e))?;
    let mut hash = Hash::new();
    let mut buffer = vec![0.0; PIECE * channels];
    let mut frames = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let got = reader.read(frames, &mut buffer).map_err(|e| wrong(&e))?;
        hash.add(&buffer[..got * channels]);
        frames += got;
        let done = (frames as f64 / written.frames.max(1) as f64).min(1.0);
        progress.store(
            WRITING + (done * f64::from(1000 - WRITING)) as u32,
            Ordering::Relaxed,
        );
        if got < PIECE {
            break;
        }
    }
    match written.hash {
        Some(expected) => {
            if frames != written.frames || hash.0 != expected {
                return Err(wrong("audio"));
            }
        }
        None => {
            // A decoder that does not trim an MP3 encoder's delay and
            // padding gives up to two frames' worth more. Matroska keeps no
            // end for Vorbis, so a WebM's last block plays whole: at most
            // half of libvorbis's longest.
            let slack = match target.format {
                Format::Mp3 => 2 * 1152 + 1105,
                Format::Webm => 4096,
                _ => 0,
            };
            if frames < written.frames || frames > written.frames + slack {
                return Err(wrong("length"));
            }
        }
    }
    carry::check(temp, &opened, target.format, carried).map_err(|e| wrong(&e))
}

/// The original's dates onto the new file, and on macOS its Finder tags and
/// comment, which are the tags a file has outside itself.
fn keep_dates_and_finder_tags(original: &Path, new: &Path) -> Result<(), String> {
    let failed = |e: io::Error| format!("cannot carry over the file's dates: {e}");
    let meta = fs::metadata(original).map_err(failed)?;
    let mut times = fs::FileTimes::new().set_modified(meta.modified().map_err(failed)?);
    if let Ok(accessed) = meta.accessed() {
        times = times.set_accessed(accessed);
    }
    #[cfg(target_os = "macos")]
    if let Ok(created) = meta.created() {
        use std::os::macos::fs::FileTimesExt;
        times = times.set_created(created);
    }
    #[cfg(windows)]
    if let Ok(created) = meta.created() {
        use std::os::windows::fs::FileTimesExt;
        times = times.set_created(created);
    }
    OpenOptions::new()
        .write(true)
        .open(new)
        .and_then(|f| f.set_times(times))
        .map_err(failed)?;
    #[cfg(target_os = "macos")]
    finder::copy(original, new).map_err(|e| format!("cannot carry over its Finder tags: {e}"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
mod finder {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// The attributes Finder keeps a file's tags and comment in.
    const NAMES: [&str; 2] = [
        "com.apple.metadata:_kMDItemUserTags",
        "com.apple.metadata:kMDItemFinderComment",
    ];

    pub fn copy(from: &Path, to: &Path) -> io::Result<()> {
        let path = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other);
        let (from, to) = (path(from)?, path(to)?);
        for name in NAMES {
            let name = CString::new(name).map_err(io::Error::other)?;
            // SAFETY: the strings are NUL-terminated and outlive each call;
            // a null buffer asks only for the size.
            let size = unsafe {
                libc::getxattr(from.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0)
            };
            if size < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENOATTR) {
                    continue;
                }
                return Err(error);
            }
            let mut value = vec![0u8; size as usize];
            // SAFETY: as above, with a buffer `value.len()` bytes long.
            let got = unsafe {
                libc::getxattr(
                    from.as_ptr(),
                    name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            };
            if got < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: as above.
            let set = unsafe {
                libc::setxattr(
                    to.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    got as usize,
                    0,
                    0,
                )
            };
            if set < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// Gives the finished file its name without ever taking another file's: a
/// hard link fails where the name is taken. A disk without links, as a
/// recorder's card is, gets a rename once the name is seen to be free.
fn place(temp: &Path, to: &Path) -> Result<PathBuf, String> {
    let mut to = to.to_owned();
    let stem = to
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let ext = to
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    for n in 2.. {
        match fs::hard_link(temp, &to) {
            Ok(()) => {
                fs::remove_file(temp).map_err(|e| format!("cannot tidy up after writing: {e}"))?;
                return Ok(to);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) if !to.exists() => {
                fs::rename(temp, &to).map_err(|e| format!("cannot name the new file: {e}"))?;
                return Ok(to);
            }
            Err(_) => {}
        }
        to = to.with_file_name(format!("{stem} {n}.{ext}"));
    }
    unreachable!("some number is free")
}

#[cfg(test)]
mod tests;
