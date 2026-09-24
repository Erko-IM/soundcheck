//! WAV, RF64 and BW64 reading.
//!
//! Hand-written rather than left to symphonia: its reader accepts only the
//! `RIFF` magic, so RF64/BW64 (what recorders switch to past 4 GiB) will not
//! open, and it discards the `bext` and `iXML` chunks where recorders keep
//! their metadata.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::Range;

/// Metadata chunks are read whole; past this they are cut short, so a
/// damaged size field cannot ask for gigabytes of memory.
const METADATA_MAX: u64 = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    U8,
    I16,
    I24,
    I32,
    F32,
    F64,
}

impl SampleKind {
    pub fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::I16 => 2,
            Self::I24 => 3,
            Self::I32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    pub fn bits(self) -> u16 {
        match self {
            Self::U8 => 8,
            Self::I16 => 16,
            Self::I24 => 24,
            Self::I32 | Self::F32 => 32,
            Self::F64 => 64,
        }
    }

    /// Every sample in `bytes`, with the format chosen once rather than per
    /// sample.
    pub fn decode_all(self, bytes: &[u8], out: &mut [f32]) {
        fn each<const N: usize>(bytes: &[u8], out: &mut [f32], f: impl Fn([u8; N]) -> f32) {
            for (o, s) in out.iter_mut().zip(bytes.as_chunks::<N>().0) {
                *o = f(*s);
            }
        }
        match self {
            Self::U8 => each::<1>(bytes, out, |[a]| (f32::from(a) - 128.0) / 128.0),
            Self::I16 => each::<2>(bytes, out, |s| f32::from(i16::from_le_bytes(s)) / 32_768.0),
            // Top-align the 24 bits so the arithmetic shift sign-extends them.
            Self::I24 => each::<3>(bytes, out, |[a, b, c]| {
                (i32::from_le_bytes([0, a, b, c]) >> 8) as f32 / 8_388_608.0
            }),
            Self::I32 => each::<4>(bytes, out, |s| {
                i32::from_le_bytes(s) as f32 / 2_147_483_648.0
            }),
            Self::F32 => each::<4>(bytes, out, f32::from_le_bytes),
            Self::F64 => each::<8>(bytes, out, |s| f64::from_le_bytes(s) as f32),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Bext {
    pub description: String,
    pub originator: String,
    pub originator_reference: String,
    pub origination_date: String,
    pub origination_time: String,
    /// Samples since midnight at the first sample of the file.
    pub time_reference: u64,
    pub coding_history: String,
}

#[derive(Debug)]
pub struct Wav {
    pub container: &'static str,
    pub sample_rate: u32,
    pub channels: u16,
    pub kind: SampleKind,
    /// Byte range of the sample data within the file.
    pub data: Range<u64>,
    pub bext: Option<Bext>,
    pub ixml: Option<String>,
    /// `LIST/INFO` entries, e.g. `(*b"ICMT", "a comment")`.
    pub info: Vec<([u8; 4], String)>,
}

#[derive(Debug)]
pub enum Error {
    /// Not a WAVE file at all, so another reader should try it.
    NotWav,
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotWav => f.write_str("not a WAV file"),
            Self::Invalid(why) => f.write_str(why),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Invalid(format!("cannot read: {e}"))
    }
}

fn le<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    b.get(at..at.checked_add(N)?)?.try_into().ok()
}

/// Fixed-width, NUL-padded text as recorders write it.
fn text(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_owned()
}

/// Up to `n` bytes from `at`: fewer where the file ends first.
fn bytes<R: Read + Seek>(file: &mut R, at: u64, n: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(at))?;
    let mut out = Vec::new();
    file.by_ref().take(n).read_to_end(&mut out)?;
    Ok(out)
}

pub fn parse<R: Read + Seek>(file: &mut R) -> Result<Wav, Error> {
    let len = file.seek(SeekFrom::End(0))?;
    let head = bytes(file, 0, 12)?;
    let container = match &le::<4>(&head, 0).ok_or(Error::NotWav)? {
        b"RIFF" => "WAV",
        b"RF64" => "RF64",
        b"BW64" => "BW64",
        _ => return Err(Error::NotWav),
    };
    if head.get(8..12) != Some(b"WAVE".as_slice()) {
        return Err(Error::NotWav);
    }

    let mut format = None;
    let mut data = None;
    let mut data_size_64 = None;
    let mut bext = None;
    let mut ixml = None;
    let mut info = Vec::new();

    let mut pos = 12;
    loop {
        let header = bytes(file, pos, 8)?;
        let (Some(id), Some(size)) = (le::<4>(&header, 0), le::<4>(&header, 4)) else {
            break;
        };
        let size = u32::from_le_bytes(size);
        let size = match data_size_64 {
            Some(real) if &id == b"data" && size == u32::MAX => real,
            // A recorder that lost power mid-take can leave the size it
            // writes at the end as zero: the audio is still all there.
            _ if &id == b"data" && size == 0 => len - pos - 8,
            _ => u64::from(size),
        };
        let start = pos + 8;
        // A recorder that stopped mid-take leaves a header claiming more
        // than the file holds, so keep whatever is actually there.
        let end = start.saturating_add(size).min(len);
        let body = |file: &mut R| bytes(file, start, (end - start).min(METADATA_MAX));
        match &id {
            b"data" => data = Some(start..end),
            b"ds64" => data_size_64 = le(&body(file)?, 8).map(u64::from_le_bytes),
            b"fmt " => format = Some(parse_fmt(&body(file)?)?),
            b"bext" => bext = parse_bext(&body(file)?),
            b"iXML" => ixml = Some(text(&body(file)?)),
            b"LIST" => {
                let list = body(file)?;
                if let Some(entries) = list.strip_prefix(b"INFO") {
                    info = parse_info(entries);
                }
            }
            _ => {}
        }
        pos = end.saturating_add(size % 2);
    }

    let (sample_rate, channels, kind) =
        format.ok_or_else(|| Error::Invalid("WAV file has no fmt chunk".into()))?;
    let data = data.ok_or_else(|| Error::Invalid("WAV file has no data chunk".into()))?;
    Ok(Wav {
        container,
        sample_rate,
        channels,
        kind,
        data,
        bext,
        ixml,
        info,
    })
}

fn parse_fmt(b: &[u8]) -> Result<(u32, u16, SampleKind), Error> {
    let short = || Error::Invalid("WAV fmt chunk is truncated".into());
    let mut tag = u16::from_le_bytes(le(b, 0).ok_or_else(short)?);
    let channels = u16::from_le_bytes(le(b, 2).ok_or_else(short)?);
    let sample_rate = u32::from_le_bytes(le(b, 4).ok_or_else(short)?);
    let bits = u16::from_le_bytes(le(b, 14).ok_or_else(short)?);
    if tag == 0xFFFE {
        // WAVE_FORMAT_EXTENSIBLE: the real format tag leads the sub-format GUID.
        tag = u16::from_le_bytes(le(b, 24).ok_or_else(short)?);
    }
    let kind = match (tag, bits) {
        (1, 8) => SampleKind::U8,
        (1, 16) => SampleKind::I16,
        (1, 24) => SampleKind::I24,
        (1, 32) => SampleKind::I32,
        (3, 32) => SampleKind::F32,
        (3, 64) => SampleKind::F64,
        _ => {
            return Err(Error::Invalid(format!(
                "unsupported WAV encoding (format {tag:#06x}, {bits}-bit)"
            )));
        }
    };
    if channels == 0 || sample_rate == 0 {
        return Err(Error::Invalid(
            "WAV file declares no channels or no sample rate".into(),
        ));
    }
    Ok((sample_rate, channels, kind))
}

/// Field offsets from EBU Tech 3285.
fn parse_bext(b: &[u8]) -> Option<Bext> {
    Some(Bext {
        description: text(b.get(0..256)?),
        originator: text(b.get(256..288)?),
        originator_reference: text(b.get(288..320)?),
        origination_date: text(b.get(320..330)?),
        origination_time: text(b.get(330..338)?),
        time_reference: u64::from_le_bytes(le(b, 338)?),
        coding_history: b.get(602..).map(text).unwrap_or_default(),
    })
}

fn parse_info(mut b: &[u8]) -> Vec<([u8; 4], String)> {
    let mut out = Vec::new();
    while let (Some(id), Some(size)) = (le::<4>(b, 0), le::<4>(b, 4)) {
        let size = u32::from_le_bytes(size) as usize;
        let end = 8usize.saturating_add(size).min(b.len());
        out.push((id, text(&b[8..end])));
        b = b.get(end + size % 2..).unwrap_or_default();
    }
    out
}

impl Wav {
    pub fn frame_bytes(&self) -> usize {
        self.kind.bytes() * usize::from(self.channels)
    }

    pub fn frames(&self) -> usize {
        usize::try_from(self.data.end - self.data.start).unwrap_or(usize::MAX) / self.frame_bytes()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    /// A minimal 16-bit WAV at 48 kHz, optionally RF64 with a placeholder
    /// data size, plus whatever extra chunks the test wants, placed before
    /// `data`. `samples` are interleaved over `channels`.
    pub fn build_channels(
        rf64: bool,
        channels: u16,
        extra: &[(&[u8; 4], Vec<u8>)],
        samples: &[i16],
    ) -> Vec<u8> {
        let mut body = b"WAVE".to_vec();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let chunk = |out: &mut Vec<u8>, id: &[u8; 4], size: u32, bytes: &[u8]| {
            out.extend_from_slice(id);
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(bytes);
            if bytes.len() % 2 == 1 {
                out.push(0);
            }
        };
        if rf64 {
            let mut ds64 = vec![0u8; 28];
            ds64[8..16].copy_from_slice(&(data.len() as u64).to_le_bytes());
            chunk(&mut body, b"ds64", 28, &ds64);
        }
        let block = 2 * channels;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&48_000u32.to_le_bytes());
        fmt.extend_from_slice(&(48_000 * u32::from(block)).to_le_bytes());
        fmt.extend_from_slice(&block.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());
        chunk(&mut body, b"fmt ", 16, &fmt);
        for (id, bytes) in extra {
            chunk(&mut body, id, bytes.len() as u32, bytes);
        }
        let data_size = if rf64 { u32::MAX } else { data.len() as u32 };
        chunk(&mut body, b"data", data_size, &data);
        let mut file = if rf64 {
            b"RF64".to_vec()
        } else {
            b"RIFF".to_vec()
        };
        file.extend_from_slice(&(if rf64 { u32::MAX } else { body.len() as u32 }).to_le_bytes());
        file.extend(body);
        file
    }

    /// Mono, as most tests want.
    pub fn build(rf64: bool, extra: &[(&[u8; 4], Vec<u8>)], samples: &[i16]) -> Vec<u8> {
        build_channels(rf64, 1, extra, samples)
    }

    #[test]
    fn reads_plain_riff() {
        let file = build(false, &[], &[0, 16_384, -16_384]);
        let w = parse(&mut Cursor::new(&file)).unwrap();
        assert_eq!(
            (w.container, w.sample_rate, w.channels, w.kind),
            ("WAV", 48_000, 1, SampleKind::I16)
        );
        assert_eq!(w.frames(), 3);
    }

    #[test]
    fn reads_rf64_through_ds64() {
        let file = build(true, &[], &[1, 2, 3, 4]);
        let w = parse(&mut Cursor::new(&file)).unwrap();
        assert_eq!(w.container, "RF64");
        assert_eq!(w.frames(), 4);
    }

    #[test]
    fn keeps_what_a_truncated_recording_holds() {
        let mut file = build(false, &[], &[7; 100]);
        file.truncate(file.len() - 50);
        assert_eq!(parse(&mut Cursor::new(&file)).unwrap().frames(), 75);
    }

    #[test]
    fn a_data_size_left_at_zero_still_reads_the_audio() {
        let mut file = build(false, &[], &[7; 100]);
        let at = file.len() - 200 - 4;
        file[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse(&mut Cursor::new(&file)).unwrap().frames(), 100);
    }

    #[test]
    fn decodes_24_bit_sign_correctly() {
        let mut out = [0.0; 2];
        SampleKind::I24.decode_all(&[0x00, 0x00, 0x80, 0x00, 0x00, 0x40], &mut out);
        assert_eq!(out, [-1.0, 0.5]);
    }

    #[test]
    fn rejects_non_wav_without_error_so_symphonia_can_try() {
        let flac = b"fLaC\0\0\0\"....";
        assert!(matches!(parse(&mut Cursor::new(flac)), Err(Error::NotWav)));
    }
}
