//! The uncompressed formats: WAV, AIFF and CAF, each written straight from
//! the samples with its metadata chunks, and the sizes its header waits for
//! filled in at the end.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::carry::Carried;
use super::{Encode, Frames, Sample, Spec};
use crate::wav::Marker;

const BUFFER: usize = 1 << 20;

fn failed(e: io::Error) -> String {
    format!("cannot write the new file: {e}")
}

/// Samples as PCM bytes `bits` wide, whole numbers or 32-bit floats, in
/// the byte order asked for.
fn put_samples(out: &mut Vec<u8>, frames: Frames<'_>, bits: u32, big: bool) {
    match frames {
        Frames::Int(samples) => {
            for &s in samples {
                match bits {
                    // An 8-bit WAV keeps samples unsigned; an 8-bit AIFF or
                    // CAF signed.
                    8 if !big => out.push((s + 128) as u8),
                    8 => out.push(s as i8 as u8),
                    16 if big => out.extend_from_slice(&(s as i16).to_be_bytes()),
                    16 => out.extend_from_slice(&(s as i16).to_le_bytes()),
                    24 if big => out.extend_from_slice(&s.to_be_bytes()[1..]),
                    24 => out.extend_from_slice(&s.to_le_bytes()[..3]),
                    _ if big => out.extend_from_slice(&s.to_be_bytes()),
                    _ => out.extend_from_slice(&s.to_le_bytes()),
                }
            }
        }
        Frames::Float(samples) => {
            for &s in samples {
                if big {
                    out.extend_from_slice(&s.to_be_bytes());
                } else {
                    out.extend_from_slice(&s.to_le_bytes());
                }
            }
        }
    }
}

fn bits_of(spec: &Spec) -> u32 {
    match spec.sample {
        Sample::Int(bits) => bits,
        Sample::Float => 32,
    }
}

/// A WAV, which becomes an RF64 once its audio passes what a RIFF's sizes
/// hold: the `JUNK` chunk first in the file is the room its `ds64` takes.
pub struct Wav {
    out: BufWriter<File>,
    spec: Spec,
    /// Where the audio's size goes, and how many bytes of it so far.
    data_size_at: u64,
    data: u64,
    after: Vec<([u8; 4], Vec<u8>)>,
    bytes: Vec<u8>,
}

impl Wav {
    pub fn new(file: File, spec: Spec, carried: &Carried) -> Result<Self, String> {
        let chunks = carried.wav_chunks()?;
        let mut head = b"RIFF\0\0\0\0WAVE".to_vec();
        riff_chunk(&mut head, b"JUNK", &[0; 28]);
        riff_chunk(&mut head, b"fmt ", &fmt_chunk(&spec));
        if spec.sample == Sample::Float {
            // Floats are not plain PCM, which keeps the frame count in
            // `fact`; filled in at the end.
            riff_chunk(&mut head, b"fact", &[0; 4]);
        }
        for (id, body) in &chunks.before {
            riff_chunk(&mut head, id, body);
        }
        head.extend_from_slice(b"data");
        let data_size_at = head.len() as u64;
        head.extend_from_slice(&[0; 4]);
        let mut out = BufWriter::with_capacity(BUFFER, file);
        out.write_all(&head).map_err(failed)?;
        Ok(Self {
            out,
            spec,
            data_size_at,
            data: 0,
            after: chunks.after,
            bytes: Vec::new(),
        })
    }
}

fn riff_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

/// A `fmt ` chunk for `spec`: WAVE_FORMAT_EXTENSIBLE for more than two
/// channels or more than 16 bits, as Microsoft asks, plain PCM or float
/// otherwise.
pub fn fmt_chunk(spec: &Spec) -> Vec<u8> {
    let bits = bits_of(spec);
    let container = bits.div_ceil(8) * 8;
    let channels = spec.channels as u16;
    let block = channels * (container / 8) as u16;
    let float = spec.sample == Sample::Float;
    let extensible = spec.channels > 2 || bits > 16 && !float;
    let tag: u16 = match (extensible, float) {
        (true, _) => 0xFFFE,
        (false, true) => 3,
        (false, false) => 1,
    };
    let mut b = Vec::new();
    b.extend_from_slice(&tag.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&spec.rate.to_le_bytes());
    b.extend_from_slice(&(spec.rate * u32::from(block)).to_le_bytes());
    b.extend_from_slice(&block.to_le_bytes());
    b.extend_from_slice(&(container as u16).to_le_bytes());
    if extensible {
        b.extend_from_slice(&22u16.to_le_bytes());
        b.extend_from_slice(&(bits as u16).to_le_bytes());
        let mask: u32 = match spec.channels {
            1 => 0x4,
            2 => 0x3,
            _ => 0,
        };
        b.extend_from_slice(&mask.to_le_bytes());
        b.extend_from_slice(&(if float { 3u16 } else { 1u16 }).to_le_bytes());
        b.extend_from_slice(&[
            0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
        ]);
    } else if float {
        b.extend_from_slice(&0u16.to_le_bytes());
    }
    b
}

impl Encode for Wav {
    fn push(&mut self, frames: Frames<'_>) -> Result<(), String> {
        self.bytes.clear();
        put_samples(&mut self.bytes, frames, bits_of(&self.spec), false);
        self.data += self.bytes.len() as u64;
        self.out.write_all(&self.bytes).map_err(failed)
    }

    fn finish(self: Box<Self>) -> Result<(), String> {
        let Self {
            mut out,
            spec,
            data_size_at,
            data,
            after,
            ..
        } = *self;
        (|| -> io::Result<()> {
            if data % 2 == 1 {
                out.write_all(&[0])?;
            }
            for (id, body) in &after {
                out.write_all(id)?;
                out.write_all(&(body.len() as u32).to_le_bytes())?;
                out.write_all(body)?;
                if body.len() % 2 == 1 {
                    out.write_all(&[0])?;
                }
            }
            out.flush()?;
            let mut file = out.into_inner().map_err(|e| e.into_error())?;
            let total = file.stream_position()?;
            let frame_bytes = (bits_of(&spec).div_ceil(8) as usize * spec.channels) as u64;
            let frames = data / frame_bytes.max(1);
            match (u32::try_from(total - 8), u32::try_from(data)) {
                (Ok(size), Ok(data)) => {
                    file.seek(SeekFrom::Start(4))?;
                    file.write_all(&size.to_le_bytes())?;
                    file.seek(SeekFrom::Start(data_size_at))?;
                    file.write_all(&data.to_le_bytes())?;
                }
                _ => {
                    file.seek(SeekFrom::Start(0))?;
                    file.write_all(b"RF64")?;
                    file.write_all(&u32::MAX.to_le_bytes())?;
                    file.seek(SeekFrom::Start(12))?;
                    file.write_all(b"ds64")?;
                    file.seek(SeekFrom::Start(20))?;
                    file.write_all(&(total - 8).to_le_bytes())?;
                    file.write_all(&data.to_le_bytes())?;
                    file.write_all(&frames.to_le_bytes())?;
                    file.seek(SeekFrom::Start(data_size_at))?;
                    file.write_all(&u32::MAX.to_le_bytes())?;
                }
            }
            if spec.sample == Sample::Float {
                // Right after the fmt chunk's header and body.
                let fact = 12 + 36 + 8 + fmt_chunk(&spec).len() as u64 + 8;
                file.seek(SeekFrom::Start(fact))?;
                file.write_all(&u32::try_from(frames).unwrap_or(u32::MAX).to_le_bytes())?;
            }
            file.sync_all()
        })()
        .map_err(failed)
    }
}

/// An AIFF, or for floats an AIFF-C, its metadata ahead of the audio so
/// that only sizes are left to fill in.
pub struct Aiff {
    out: BufWriter<File>,
    spec: Spec,
    frames_at: u64,
    ssnd_at: u64,
    data: u64,
    bytes: Vec<u8>,
}

impl Aiff {
    pub fn new(file: File, spec: Spec, carried: &Carried) -> Result<Self, String> {
        let float = spec.sample == Sample::Float;
        let mut b = Vec::new();
        let chunk = |b: &mut Vec<u8>, id: &[u8; 4], body: &[u8]| {
            b.extend_from_slice(id);
            b.extend_from_slice(&(body.len() as u32).to_be_bytes());
            b.extend_from_slice(body);
            if body.len() % 2 == 1 {
                b.push(0);
            }
        };
        b.extend_from_slice(b"FORM\0\0\0\0");
        b.extend_from_slice(if float { b"AIFC" } else { b"AIFF" });
        if float {
            chunk(&mut b, b"FVER", &0xA280_5140u32.to_be_bytes());
        }
        let mut comm = Vec::new();
        comm.extend_from_slice(&(spec.channels as u16).to_be_bytes());
        comm.extend_from_slice(&[0; 4]);
        comm.extend_from_slice(&(bits_of(&spec) as u16).to_be_bytes());
        comm.extend_from_slice(&extended(spec.rate));
        if float {
            comm.extend_from_slice(b"fl32");
            comm.extend_from_slice(&pstring("32-bit floating point"));
        }
        let frames_at = b.len() as u64 + 8 + 2;
        chunk(&mut b, b"COMM", &comm);
        if !carried.markers.is_empty() {
            chunk(&mut b, b"MARK", &mark_chunk(&carried.markers));
        }
        let mut text = Vec::new();
        lofty::tag::TagExt::dump_to(
            &carried.aiff_text(),
            &mut text,
            lofty::config::WriteOptions::default(),
        )
        .map_err(|e| format!("the AIFF text cannot be written: {e}"))?;
        b.extend_from_slice(&text);
        let tag = carried.id3v2(true, |_| false);
        if !tag.is_empty() {
            chunk(&mut b, b"ID3 ", &carried.id3v2_bytes(&tag)?);
        }
        b.extend_from_slice(b"SSND");
        let ssnd_at = b.len() as u64;
        b.extend_from_slice(&[0; 12]);
        let mut out = BufWriter::with_capacity(BUFFER, file);
        out.write_all(&b).map_err(failed)?;
        Ok(Self {
            out,
            spec,
            frames_at,
            ssnd_at,
            data: 0,
            bytes: Vec::new(),
        })
    }
}

/// `rate` as the 80-bit extended float AIFF keeps it in.
fn extended(rate: u32) -> [u8; 10] {
    let mut b = [0; 10];
    if rate == 0 {
        return b;
    }
    let shift = rate.leading_zeros();
    let exponent = 16_383 + 31 - shift as u16;
    let mantissa = u64::from(rate) << (32 + shift);
    b[..2].copy_from_slice(&exponent.to_be_bytes());
    b[2..].copy_from_slice(&mantissa.to_be_bytes());
    b
}

/// A Pascal string as AIFF keeps one: its length, its bytes, and a pad to
/// an even size.
fn pstring(text: &str) -> Vec<u8> {
    let bytes = &text.as_bytes()[..text.len().min(255)];
    let mut b = vec![bytes.len() as u8];
    b.extend_from_slice(bytes);
    if b.len() % 2 == 1 {
        b.push(0);
    }
    b
}

fn mark_chunk(markers: &[Marker]) -> Vec<u8> {
    let markers = &markers[..markers.len().min(usize::from(u16::MAX))];
    let mut b = (markers.len() as u16).to_be_bytes().to_vec();
    for (i, m) in markers.iter().enumerate() {
        b.extend_from_slice(&(i as u16 + 1).to_be_bytes());
        b.extend_from_slice(&u32::try_from(m.frame).unwrap_or(u32::MAX).to_be_bytes());
        b.extend_from_slice(&pstring(&m.label));
    }
    b
}

impl Encode for Aiff {
    fn push(&mut self, frames: Frames<'_>) -> Result<(), String> {
        self.bytes.clear();
        put_samples(&mut self.bytes, frames, bits_of(&self.spec), true);
        self.data += self.bytes.len() as u64;
        self.out.write_all(&self.bytes).map_err(failed)
    }

    fn finish(self: Box<Self>) -> Result<(), String> {
        let Self {
            mut out,
            spec,
            frames_at,
            ssnd_at,
            data,
            ..
        } = *self;
        let frame_bytes = u64::from(bits_of(&spec).div_ceil(8)) * spec.channels as u64;
        let too_big = || "AIFF holds at most 4 GB of audio; WAV, CAF or FLAC take more".to_owned();
        let ssnd = u32::try_from(data + 8).map_err(|_| too_big())?;
        (|| -> io::Result<()> {
            if data % 2 == 1 {
                out.write_all(&[0])?;
            }
            out.flush()?;
            let mut file = out.into_inner().map_err(|e| e.into_error())?;
            let total = file.stream_position()?;
            let form = u32::try_from(total - 8).map_err(io::Error::other)?;
            file.seek(SeekFrom::Start(4))?;
            file.write_all(&form.to_be_bytes())?;
            file.seek(SeekFrom::Start(frames_at))?;
            file.write_all(&((data / frame_bytes.max(1)) as u32).to_be_bytes())?;
            file.seek(SeekFrom::Start(ssnd_at))?;
            file.write_all(&ssnd.to_be_bytes())?;
            file.sync_all()
        })()
        .map_err(failed)
    }
}

/// A CAF of PCM, its metadata ahead of the audio: the fields in `info`, the
/// markers in `mark` with their names in `strg`.
pub struct Caf {
    out: BufWriter<File>,
    spec: Spec,
    data_size_at: u64,
    data: u64,
    bytes: Vec<u8>,
}

impl Caf {
    pub fn new(file: File, spec: Spec, carried: &Carried) -> Result<Self, String> {
        let float = spec.sample == Sample::Float;
        let bits = bits_of(&spec);
        let mut b = b"caff".to_vec();
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        let chunk = |b: &mut Vec<u8>, id: &[u8; 4], body: &[u8]| {
            b.extend_from_slice(id);
            b.extend_from_slice(&(body.len() as i64).to_be_bytes());
            b.extend_from_slice(body);
        };
        let mut desc = Vec::new();
        desc.extend_from_slice(&f64::from(spec.rate).to_be_bytes());
        desc.extend_from_slice(b"lpcm");
        desc.extend_from_slice(&u32::from(float).to_be_bytes());
        let frame = bits.div_ceil(8) * spec.channels as u32;
        desc.extend_from_slice(&frame.to_be_bytes());
        desc.extend_from_slice(&1u32.to_be_bytes());
        desc.extend_from_slice(&(spec.channels as u32).to_be_bytes());
        desc.extend_from_slice(&bits.to_be_bytes());
        chunk(&mut b, b"desc", &desc);
        let info = carried.caf_info();
        if !info.is_empty() {
            let mut body = (info.len() as u32).to_be_bytes().to_vec();
            for (key, value) in &info {
                body.extend_from_slice(key.replace('\0', " ").as_bytes());
                body.push(0);
                body.extend_from_slice(value.replace('\0', " ").as_bytes());
                body.push(0);
            }
            chunk(&mut b, b"info", &body);
        }
        if !carried.markers.is_empty() {
            let (mark, strg) = caf_marks(&carried.markers);
            chunk(&mut b, b"strg", &strg);
            chunk(&mut b, b"mark", &mark);
        }
        b.extend_from_slice(b"data");
        let data_size_at = b.len() as u64;
        b.extend_from_slice(&[0; 8]);
        // The edit count, 0 for a file written once.
        b.extend_from_slice(&[0; 4]);
        let mut out = BufWriter::with_capacity(BUFFER, file);
        out.write_all(&b).map_err(failed)?;
        Ok(Self {
            out,
            spec,
            data_size_at,
            data: 0,
            bytes: Vec::new(),
        })
    }
}

/// A CAF `mark` chunk for `markers`, and the `strg` chunk with their
/// names, which the marks refer to by number.
fn caf_marks(markers: &[Marker]) -> (Vec<u8>, Vec<u8>) {
    let mut mark = Vec::new();
    // No SMPTE times.
    mark.extend_from_slice(&0u32.to_be_bytes());
    mark.extend_from_slice(&(markers.len() as u32).to_be_bytes());
    let mut strings = Vec::new();
    let mut offsets = Vec::new();
    for (i, m) in markers.iter().enumerate() {
        let id = i as u32 + 1;
        // A generic marker.
        mark.extend_from_slice(&0u32.to_be_bytes());
        mark.extend_from_slice(&(m.frame as f64).to_be_bytes());
        mark.extend_from_slice(&id.to_be_bytes());
        mark.extend_from_slice(&[0xFF; 4]);
        mark.extend_from_slice(&0u32.to_be_bytes());
        mark.extend_from_slice(&0u32.to_be_bytes());
        offsets.push((id, strings.len() as i64));
        strings.extend_from_slice(m.label.replace('\0', " ").as_bytes());
        strings.push(0);
    }
    let mut strg = (offsets.len() as u32).to_be_bytes().to_vec();
    for (id, at) in offsets {
        strg.extend_from_slice(&id.to_be_bytes());
        strg.extend_from_slice(&at.to_be_bytes());
    }
    strg.extend(strings);
    (mark, strg)
}

impl Encode for Caf {
    fn push(&mut self, frames: Frames<'_>) -> Result<(), String> {
        self.bytes.clear();
        put_samples(&mut self.bytes, frames, bits_of(&self.spec), true);
        self.data += self.bytes.len() as u64;
        self.out.write_all(&self.bytes).map_err(failed)
    }

    fn finish(self: Box<Self>) -> Result<(), String> {
        let Self {
            mut out,
            data_size_at,
            data,
            ..
        } = *self;
        (|| -> io::Result<()> {
            out.flush()?;
            let mut file = out.into_inner().map_err(|e| e.into_error())?;
            file.seek(SeekFrom::Start(data_size_at))?;
            // The size counts the edit count too.
            file.write_all(&((data + 4) as i64).to_be_bytes())?;
            file.sync_all()
        })()
        .map_err(failed)
    }
}

/// The chunks of an IFF file as its reader walks them: id, and where the
/// body starts and how long it is.
fn chunks(
    file: &mut File,
    from: u64,
    header: usize,
    wide: bool,
) -> io::Result<Vec<([u8; 4], u64, u64)>> {
    let len = file.metadata()?.len();
    let mut found = Vec::new();
    let mut at = from;
    let mut head = vec![0; header];
    while at + header as u64 <= len {
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut head)?;
        let id: [u8; 4] = head[..4].try_into().expect("four bytes");
        let size = if wide {
            u64::from_be_bytes(head[4..12].try_into().expect("eight bytes"))
        } else {
            u64::from(u32::from_be_bytes(
                head[4..8].try_into().expect("four bytes"),
            ))
        };
        let start = at + header as u64;
        let size = size.min(len - start);
        found.push((id, start, size));
        at = start + size + if wide { 0 } else { size % 2 };
    }
    Ok(found)
}

fn body(file: &mut File, start: u64, size: u64) -> io::Result<Vec<u8>> {
    let mut b = vec![0; size.min(16 << 20) as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut b)?;
    Ok(b)
}

/// An AIFF's `MARK` markers: places and names.
pub fn aiff_markers(path: &Path) -> Result<Vec<Marker>, String> {
    let read = || -> io::Result<Vec<Marker>> {
        let mut file = File::open(path)?;
        let Some((_, start, size)) = chunks(&mut file, 12, 8, false)?
            .into_iter()
            .find(|(id, _, _)| id == b"MARK")
        else {
            return Ok(Vec::new());
        };
        let b = body(&mut file, start, size)?;
        let mut markers = Vec::new();
        let count = b.get(..2).map_or(0, |c| u16::from_be_bytes([c[0], c[1]]));
        let mut at = 2;
        for _ in 0..count {
            let (Some(id), Some(pos), Some(&len)) =
                (b.get(at..at + 2), b.get(at + 2..at + 6), b.get(at + 6))
            else {
                break;
            };
            let name = b.get(at + 7..at + 7 + usize::from(len)).unwrap_or_default();
            markers.push(Marker {
                id: u32::from(u16::from_be_bytes([id[0], id[1]])),
                frame: u32::from_be_bytes(pos.try_into().expect("four bytes")) as usize,
                length: 0,
                label: String::from_utf8_lossy(name).into_owned(),
                note: String::new(),
            });
            let text = 1 + usize::from(len);
            at += 6 + text + text % 2;
        }
        Ok(markers)
    };
    read().map_err(|e| format!("its markers cannot be read: {e}"))
}

/// A CAF's `info` chunk: names and values.
pub fn caf_info(path: &Path) -> Result<Vec<(String, String)>, String> {
    let read = || -> io::Result<Vec<(String, String)>> {
        let mut file = File::open(path)?;
        let Some((_, start, size)) = chunks(&mut file, 8, 12, true)?
            .into_iter()
            .find(|(id, _, _)| id == b"info")
        else {
            return Ok(Vec::new());
        };
        let b = body(&mut file, start, size)?;
        let mut strings = b.get(4..).unwrap_or_default().split(|&c| c == 0);
        let mut info = Vec::new();
        while let (Some(key), Some(value)) = (strings.next(), strings.next()) {
            if key.is_empty() {
                break;
            }
            info.push((
                String::from_utf8_lossy(key).into_owned(),
                String::from_utf8_lossy(value).into_owned(),
            ));
        }
        Ok(info)
    };
    read().map_err(|e| format!("its tags cannot be read: {e}"))
}

/// A CAF's `mark` markers, named from its `strg` chunk.
pub fn caf_markers(path: &Path) -> Result<Vec<Marker>, String> {
    let read = || -> io::Result<Vec<Marker>> {
        let mut file = File::open(path)?;
        let found = chunks(&mut file, 8, 12, true)?;
        let Some(&(_, start, size)) = found.iter().find(|(id, _, _)| id == b"mark") else {
            return Ok(Vec::new());
        };
        let mark = body(&mut file, start, size)?;
        let strg = match found.iter().find(|(id, _, _)| id == b"strg") {
            Some(&(_, start, size)) => body(&mut file, start, size)?,
            None => Vec::new(),
        };
        let u32_at = |b: &[u8], at: usize| {
            b.get(at..at + 4)
                .map(|x| u32::from_be_bytes(x.try_into().expect("four bytes")))
        };
        let count = u32_at(&strg, 0).unwrap_or(0) as usize;
        let text_from = 4 + 12 * count;
        let name_of = |id: u32| -> String {
            (0..count)
                .find(|&i| u32_at(&strg, 4 + 12 * i) == Some(id))
                .and_then(|i| strg.get(4 + 12 * i + 4..4 + 12 * i + 12))
                .map(|o| i64::from_be_bytes(o.try_into().expect("eight bytes")))
                .and_then(|o| strg.get(text_from + o as usize..))
                .map(|t| {
                    let end = t.iter().position(|&c| c == 0).unwrap_or(t.len());
                    String::from_utf8_lossy(&t[..end]).into_owned()
                })
                .unwrap_or_default()
        };
        let mut markers = Vec::new();
        for i in 0..u32_at(&mark, 4).unwrap_or(0) as usize {
            let at = 8 + 28 * i;
            let Some(entry) = mark.get(at..at + 28) else {
                break;
            };
            let frame = f64::from_be_bytes(entry[4..12].try_into().expect("eight bytes"));
            let id = u32::from_be_bytes(entry[12..16].try_into().expect("four bytes"));
            markers.push(Marker {
                id,
                frame: frame.max(0.0).round() as usize,
                length: 0,
                label: name_of(id),
                note: String::new(),
            });
        }
        Ok(markers)
    };
    read().map_err(|e| format!("its markers cannot be read: {e}"))
}
