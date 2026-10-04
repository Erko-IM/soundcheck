//! Ogg Vorbis, written by libvorbis (the aoTuV tuning, through vorbis_rs),
//! with the tags and pictures put in by lofty once the audio is written;
//! and the same encoder's packets for a WebM, timed from the stream's own
//! block sizes.

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::num::{NonZeroU8, NonZeroU32};
use std::path::{Path, PathBuf};

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::ogg::VorbisFile;
use lofty::tag::TagExt;
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoder, VorbisEncoderBuilder};

use super::carry::Carried;
use super::{Encode, Frames, Spec};

/// The Ogg stream serial: one stream a file, so any serial does, and the
/// same one each time makes a conversion done twice come out the same.
const SERIAL: i32 = 0x736f_756e;

/// libvorbis encoding to `sink`.
pub struct Stream<W: Write> {
    encoder: Option<VorbisEncoder<W>>,
    channels: usize,
    planar: Vec<Vec<f32>>,
}

impl<W: Write> Stream<W> {
    pub fn new(sink: W, spec: &Spec, quality: f32) -> Result<Self, String> {
        let bad = |e: vorbis_rs::VorbisError| format!("libvorbis cannot be set up: {e}");
        let rate = NonZeroU32::new(spec.rate).ok_or("no sample rate")?;
        let channels = u8::try_from(spec.channels)
            .ok()
            .and_then(NonZeroU8::new)
            .ok_or("Vorbis keeps 1 to 255 channels")?;
        let encoder = VorbisEncoderBuilder::new_with_serial(rate, channels, sink, SERIAL)
            .bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
                target_quality: quality,
            })
            .build()
            .map_err(bad)?;
        Ok(Self {
            encoder: Some(encoder),
            channels: spec.channels,
            planar: vec![Vec::new(); spec.channels],
        })
    }

    pub fn push(&mut self, samples: &[f32]) -> Result<(), String> {
        let ch = self.channels;
        for (c, plane) in self.planar.iter_mut().enumerate() {
            plane.clear();
            plane.extend(samples.iter().skip(c).step_by(ch));
        }
        if self.planar[0].is_empty() {
            return Ok(());
        }
        self.encoder
            .as_mut()
            .expect("encoding")
            .encode_audio_block(&self.planar)
            .map_err(|e| format!("libvorbis failed: {e}"))
    }

    pub fn finish(&mut self) -> Result<W, String> {
        self.encoder
            .take()
            .expect("encoding")
            .finish()
            .map_err(|e| format!("libvorbis failed: {e}"))
    }
}

/// An Ogg Vorbis file.
pub struct Writer {
    stream: Stream<BufWriter<File>>,
    path: PathBuf,
    tags: lofty::ogg::tag::VorbisComments,
}

impl Writer {
    pub fn ogg(
        file: File,
        path: &Path,
        spec: Spec,
        quality: f32,
        carried: &Carried,
    ) -> Result<Self, String> {
        Ok(Self {
            stream: Stream::new(BufWriter::with_capacity(1 << 20, file), &spec, quality)?,
            path: path.to_owned(),
            tags: carried.vorbis_with_pictures(),
        })
    }
}

impl Encode for Writer {
    fn push(&mut self, frames: Frames<'_>) -> Result<(), String> {
        let Frames::Float(samples) = frames else {
            return Err("Vorbis is encoded from floats".into());
        };
        self.stream.push(samples)
    }

    fn finish(mut self: Box<Self>) -> Result<(), String> {
        let mut out = self.stream.finish()?;
        out.flush()
            .map_err(|e| format!("cannot write the new file: {e}"))?;
        drop(out);
        // libvorbis's comment header names libvorbis; the tags go in with
        // that kept.
        let mut file = File::open(&self.path).map_err(|e| format!("cannot open: {e}"))?;
        let written = VorbisFile::read_from(&mut file, ParseOptions::new().read_properties(false))
            .map_err(|e| format!("the new file cannot be read: {e}"))?;
        self.tags
            .set_vendor(written.vorbis_comments().vendor().to_owned());
        self.tags
            .save_to_path(&self.path, WriteOptions::default())
            .map_err(|e| format!("its tags cannot be written: {e}"))
    }
}

/// A Vorbis stream's packets and when each starts, for a container that
/// times every packet, as Matroska does.
pub struct Packets {
    /// The identification, comment and setup headers.
    pub headers: Vec<Vec<u8>>,
    /// Each audio packet, with the frame its audio starts at.
    pub audio: Vec<(u64, Vec<u8>)>,
}

/// Reads the Ogg Vorbis stream at `path` back as packets.
pub fn packets(path: &Path) -> Result<Packets, String> {
    let file = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
    let mut reader = ogg::PacketReader::new(BufReader::new(file));
    let mut headers = Vec::new();
    let mut audio = Vec::new();
    let mut modes = None;
    let mut sizes = (0usize, 0usize);
    let mut previous = None;
    let mut at = 0u64;
    while let Some(packet) = reader
        .read_packet()
        .map_err(|e| format!("the Vorbis stream cannot be read back: {e}"))?
    {
        let data = packet.data;
        if headers.len() < 3 {
            if headers.is_empty() {
                let b = *data.get(28).ok_or("the Vorbis header is short")?;
                sizes = (1usize << (b & 0x0F), 1usize << (b >> 4));
            }
            if headers.len() == 2 {
                modes = Some(block_flags(&data).ok_or("the Vorbis setup cannot be read")?);
            }
            headers.push(data);
            continue;
        }
        let flags: &Vec<bool> = modes.as_ref().expect("read with the headers");
        let first = data.first().copied().unwrap_or(0);
        let mode = if flags.len() > 1 {
            let bits = usize::BITS - (flags.len() - 1).leading_zeros();
            (usize::from(first >> 1)) & ((1 << bits) - 1)
        } else {
            0
        };
        let size = if flags.get(mode).copied().unwrap_or(false) {
            sizes.1
        } else {
            sizes.0
        };
        // A packet's audio is the overlap of its block with the one before:
        // the first packet gives none.
        if let Some(before) = previous {
            audio.push((at, data));
            at += ((before + size) / 4) as u64;
        } else {
            audio.push((0, data));
        }
        previous = Some(size);
    }
    Ok(Packets { headers, audio })
}

/// Whether each of a Vorbis setup header's modes uses the long block,
/// read from the end of the header back, as ffmpeg does, since what comes
/// before the modes cannot be skipped without decoding it all. Each mode
/// is its block flag, a window and a transform type that are always 0,
/// and a mapping number; the count of modes comes before them.
fn block_flags(setup: &[u8]) -> Option<Vec<bool>> {
    // The setup header's bits, last first.
    let total = setup.len() * 8;
    let bit = |i: usize| -> bool {
        let n = total - 1 - i;
        setup[n / 8] >> (n % 8) & 1 == 1
    };
    let read = |from: usize, n: usize| -> u32 {
        (0..n).fold(0, |v, k| (v << 1) | u32::from(bit(from + k)))
    };
    let mut at = 0;
    // The framing bit closes the header, after any padding.
    while at < total.saturating_sub(97) && !bit(at) {
        at += 1;
    }
    at += 1;
    let framed = at;
    let mut count = 0;
    let mut found = None;
    while total - at >= 97 {
        if read(at, 8) > 63 || read(at + 8, 16) != 0 || read(at + 24, 16) != 0 {
            break;
        }
        at += 41;
        count += 1;
        if count > 64 {
            break;
        }
        // The six bits before the modes hold their count less one. Read
        // last bit first, a field comes back as it was written.
        let stored = read(at, 6) + 1;
        if stored == count {
            found = Some(count);
        }
    }
    let count = found? as usize;
    let mut flags = vec![false; count];
    let mut at = framed;
    for flag in flags.iter_mut().rev() {
        at += 40;
        *flag = bit(at);
        at += 1;
    }
    Some(flags)
}
