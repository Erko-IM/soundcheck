//! WAV, RF64 and BW64 reading, and the chunk bodies saving writes.
//!
//! Hand-written rather than left to symphonia: its reader accepts only the
//! `RIFF` magic, so RF64/BW64 (what recorders switch to past 4 GiB) will not
//! open, and it discards the `bext`, `iXML` and `cue ` chunks where
//! recorders keep their metadata and marks.

use std::collections::HashMap;
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
    /// The chunk as stored, so a save can change some fields and keep the
    /// rest (version, UMID, loudness) byte for byte.
    pub raw: Vec<u8>,
}

/// A mark in the recording, as recorders and editors keep them: a `cue `
/// point, named and given a length in `LIST/adtl`.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub id: u32,
    pub frame: usize,
    /// Frames it spans; 0 for a point.
    pub length: usize,
    pub label: String,
    pub note: String,
}

/// A chunk's place in the file: its id, and its body after the eight
/// header bytes, cut short where the file ends.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub id: [u8; 4],
    pub body: Range<u64>,
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
    /// The file's markers, in order, or `None` when it has no `cue ` chunk.
    pub cues: Option<Vec<Marker>>,
    /// Every chunk, in file order.
    pub chunks: Vec<Chunk>,
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

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    le(b, at).map(u32::from_le_bytes)
}

/// Fixed-width, NUL-padded text as recorders write it. Text that is not
/// UTF-8 is read as Windows-1252, as older Windows software wrote it, so
/// every byte still reads as a letter and none is lost.
fn text(b: &[u8]) -> String {
    let b = &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())];
    let decoded = match std::str::from_utf8(b) {
        Ok(text) => text.to_owned(),
        Err(_) => b.iter().map(|&c| windows_1252(c)).collect(),
    };
    decoded.trim().to_owned()
}

/// Windows-1252 differs from Latin-1 only from 0x80 to 0x9F; the five bytes
/// there it leaves undefined read as Latin-1's.
fn windows_1252(c: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{81}', '\u{201A}', '\u{192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{2C6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8D}', '\u{17D}',
        '\u{8F}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}',
        '\u{2014}', '\u{2DC}', '\u{2122}', '\u{161}', '\u{203A}', '\u{153}', '\u{9D}', '\u{17E}',
        '\u{178}',
    ];
    match c {
        0x80..=0x9F => HIGH[usize::from(c - 0x80)],
        _ => char::from(c),
    }
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
    let mut cue_points = None;
    let mut adtl = Adtl::default();
    let mut chunks = Vec::new();

    let mut pos = 12;
    loop {
        let header = bytes(file, pos, 8)?;
        let (Some(id), Some(size)) = (le::<4>(&header, 0), u32_at(&header, 4)) else {
            break;
        };
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
        chunks.push(Chunk {
            id,
            body: start..end,
        });
        let body = |file: &mut R| bytes(file, start, (end - start).min(METADATA_MAX));
        match &id {
            b"data" => data = Some(start..end),
            b"ds64" => data_size_64 = le(&body(file)?, 8).map(u64::from_le_bytes),
            b"fmt " => format = Some(parse_fmt(&body(file)?)?),
            // A second bext or iXML is a writer's mistake, and readers
            // take the first.
            b"bext" if bext.is_none() => bext = parse_bext(body(file)?),
            b"iXML" if ixml.is_none() => ixml = Some(text(&body(file)?)),
            b"cue " => cue_points
                .get_or_insert_with(Vec::new)
                .extend(parse_cue(&body(file)?)),
            b"LIST" => {
                let list = body(file)?;
                if let Some(entries) = list.strip_prefix(b"INFO") {
                    info.extend(parse_info(entries));
                } else if let Some(entries) = list.strip_prefix(b"adtl") {
                    adtl.read(entries);
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
        cues: cue_points.map(|points| adtl.markers(points)),
        chunks,
    })
}

fn parse_fmt(b: &[u8]) -> Result<(u32, u16, SampleKind), Error> {
    let short = || Error::Invalid("WAV fmt chunk is truncated".into());
    let mut tag = u16::from_le_bytes(le(b, 0).ok_or_else(short)?);
    let channels = u16::from_le_bytes(le(b, 2).ok_or_else(short)?);
    let sample_rate = u32_at(b, 4).ok_or_else(short)?;
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
fn parse_bext(b: Vec<u8>) -> Option<Bext> {
    Some(Bext {
        description: text(b.get(0..256)?),
        originator: text(b.get(256..288)?),
        originator_reference: text(b.get(288..320)?),
        origination_date: text(b.get(320..330)?),
        origination_time: text(b.get(330..338)?),
        time_reference: u64::from_le_bytes(le(&b, 338)?),
        coding_history: b.get(602..).map(text).unwrap_or_default(),
        raw: b,
    })
}

/// The chunks packed inside a `LIST`, after its type, each with its body.
pub fn subchunks(mut b: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        let id = le::<4>(b, 0)?;
        let size = u32_at(b, 4)? as usize;
        let end = 8usize.saturating_add(size).min(b.len());
        let body = &b[8..end];
        b = b.get(end.saturating_add(size % 2)..).unwrap_or_default();
        Some((id, body))
    })
}

fn parse_info(b: &[u8]) -> Vec<([u8; 4], String)> {
    subchunks(b).map(|(id, body)| (id, text(body))).collect()
}

/// The cue points in a `cue ` body, as stored.
fn cue_points(b: &[u8]) -> impl Iterator<Item = &[u8; 24]> {
    let count = u32_at(b, 0).unwrap_or(0) as usize;
    b.get(4..)
        .unwrap_or_default()
        .as_chunks()
        .0
        .iter()
        .take(count)
}

/// A cue point's id and frame. The frame is `dwSampleOffset`; a few writers
/// leave that zero and put it in `dwPosition` instead.
fn point(p: &[u8; 24]) -> Option<(u32, u32)> {
    let (id, position, offset) = (u32_at(p, 0)?, u32_at(p, 4)?, u32_at(p, 20)?);
    Some((id, if offset == 0 { position } else { offset }))
}

fn parse_cue(b: &[u8]) -> Vec<(u32, u32)> {
    cue_points(b).filter_map(point).collect()
}

/// The text in a `labl`, `note` or `ltxt` subchunk, after its cue id and,
/// in an `ltxt`, the region's details.
fn mark_text(kind: &[u8; 4], body: &[u8]) -> String {
    let from = if kind == b"ltxt" { 20 } else { 4 };
    body.get(from..).map(text).unwrap_or_default()
}

/// What `LIST/adtl` says about each cue point, by id.
#[derive(Default)]
struct Adtl {
    labels: HashMap<u32, String>,
    notes: HashMap<u32, String>,
    /// From `ltxt`: frames spanned, and the text some writers put there.
    regions: HashMap<u32, (u32, String)>,
}

impl Adtl {
    fn read(&mut self, b: &[u8]) {
        for (kind, body) in subchunks(b) {
            let Some(cue) = u32_at(body, 0) else { continue };
            let text = mark_text(&kind, body);
            match &kind {
                b"labl" => {
                    self.labels.insert(cue, text);
                }
                b"note" => {
                    self.notes.insert(cue, text);
                }
                b"ltxt" => {
                    if let Some(length) = u32_at(body, 4) {
                        self.regions.insert(cue, (length, text));
                    }
                }
                _ => {}
            }
        }
    }

    fn markers(mut self, points: Vec<(u32, u32)>) -> Vec<Marker> {
        let mut markers: Vec<Marker> = points
            .into_iter()
            .map(|(id, frame)| {
                let (length, named) = self.regions.remove(&id).unwrap_or_default();
                let label = self.labels.remove(&id).filter(|l| !l.is_empty());
                Marker {
                    id,
                    frame: frame as usize,
                    length: length as usize,
                    label: label.unwrap_or(named),
                    note: self.notes.remove(&id).unwrap_or_default(),
                }
            })
            .collect();
        markers.sort_by_key(|m| (m.frame, m.id));
        markers
    }
}

fn subchunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

/// A file's marks as stored: its cue points and its `LIST/adtl`
/// subchunks, so that a save writes back whatever an edit left alone.
#[derive(Default)]
pub struct StoredMarks {
    points: Vec<[u8; 24]>,
    adtl: Vec<([u8; 4], Vec<u8>)>,
}

impl StoredMarks {
    /// Takes in a `cue ` chunk's body.
    pub fn add_cue(&mut self, body: &[u8]) {
        self.points.extend(cue_points(body).copied());
    }

    /// Takes in a `LIST/adtl` chunk's subchunks.
    pub fn add_adtl(&mut self, entries: &[u8]) {
        self.adtl
            .extend(subchunks(entries).map(|(kind, body)| (kind, body.to_vec())));
    }

    fn find(&self, kind: &[u8; 4], id: u32, keep: impl Fn(&[u8]) -> bool) -> Option<&[u8]> {
        self.adtl
            .iter()
            .find(|(k, body)| k == kind && u32_at(body, 0) == Some(id) && keep(body))
            .map(|(_, body)| body.as_slice())
    }
}

/// The `cue ` body for `markers`, and the `LIST/adtl` body naming them when
/// there is anything to put in one: an `ltxt` with the length of each
/// region, a `labl` with each name and a `note` with each note. Whatever of
/// a marker is still as `stored` has it goes back as it was stored, and
/// `stored` subchunks of other kinds are carried over.
pub fn mark_bodies(markers: &[Marker], stored: &StoredMarks) -> (Vec<u8>, Option<Vec<u8>>) {
    let mut cue = (markers.len() as u32).to_le_bytes().to_vec();
    let mut adtl = b"adtl".to_vec();
    for m in markers {
        let id = m.id.to_le_bytes();
        let frame = u32::try_from(m.frame).unwrap_or(u32::MAX);
        match stored
            .points
            .iter()
            .find(|p| point(p) == Some((m.id, frame)))
        {
            Some(p) => cue.extend_from_slice(p),
            None => {
                let frame = frame.to_le_bytes();
                for field in [&id, &frame, b"data", &[0; 4], &[0; 4], &frame] {
                    cue.extend_from_slice(field);
                }
            }
        }
        let mut named_in_region = false;
        if m.length > 0 {
            let length = u32::try_from(m.length).unwrap_or(u32::MAX);
            // Some writers name a region in its ltxt, which reads as the
            // marker's name where no labl names it: kept only while it
            // still names the marker, or while a kept labl names it.
            let label_kept = !m.label.is_empty()
                && stored
                    .find(b"labl", m.id, |b| mark_text(b"labl", b) == m.label)
                    .is_some();
            let kept = stored.find(b"ltxt", m.id, |b| {
                let name = mark_text(b"ltxt", b);
                u32_at(b, 4) == Some(length) && (name.is_empty() || name == m.label || label_kept)
            });
            match kept {
                Some(body) => {
                    named_in_region = !m.label.is_empty() && mark_text(b"ltxt", body) == m.label;
                    subchunk(&mut adtl, b"ltxt", body);
                }
                None => {
                    let mut body = id.to_vec();
                    body.extend_from_slice(&length.to_le_bytes());
                    body.extend_from_slice(b"rgn ");
                    body.extend_from_slice(&[0; 8]);
                    subchunk(&mut adtl, b"ltxt", &body);
                }
            }
        }
        for (kind, value) in [(b"labl", &m.label), (b"note", &m.note)] {
            if value.is_empty() {
                continue;
            }
            match stored.find(kind, m.id, |b| mark_text(kind, b) == *value) {
                Some(body) => subchunk(&mut adtl, kind, body),
                // The file kept this name in the region alone.
                None if kind == b"labl" && named_in_region => {}
                None => {
                    let mut body = id.to_vec();
                    body.extend_from_slice(value.as_bytes());
                    body.push(0);
                    subchunk(&mut adtl, kind, &body);
                }
            }
        }
    }
    for (kind, body) in &stored.adtl {
        if !matches!(kind, b"labl" | b"note" | b"ltxt") {
            subchunk(&mut adtl, kind, body);
        }
    }
    (cue, (adtl.len() > 4).then_some(adtl))
}

/// A `LIST/INFO` body for `entries`. An entry `stored`, the file's own,
/// holds with the same text goes back as it was stored.
pub fn info_body(entries: &[([u8; 4], String)], stored: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut unused: Vec<&([u8; 4], Vec<u8>)> = stored.iter().collect();
    let mut b = b"INFO".to_vec();
    for (id, value) in entries {
        match unused
            .iter()
            .position(|(k, body)| k == id && text(body) == *value)
        {
            Some(i) => subchunk(&mut b, id, &unused.remove(i).1),
            None => {
                let mut body = value.as_bytes().to_vec();
                body.push(0);
                subchunk(&mut b, id, &body);
            }
        }
    }
    b
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
        let ids: Vec<&[u8; 4]> = w.chunks.iter().map(|c| &c.id).collect();
        assert_eq!(ids, [b"fmt ", b"data"]);
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

    pub fn marker(id: u32, frame: usize, length: usize, label: &str) -> Marker {
        Marker {
            id,
            frame,
            length,
            label: label.into(),
            note: String::new(),
        }
    }

    /// A file holding `marks`, written as a save writes new ones.
    pub fn marked(marks: &[Marker]) -> Vec<u8> {
        let (cue, adtl) = mark_bodies(marks, &StoredMarks::default());
        let mut extra = vec![(b"cue ", cue)];
        extra.extend(adtl.map(|adtl| (b"LIST", adtl)));
        build(false, &extra, &[0; 100])
    }

    #[test]
    fn marks_come_from_cue_points_named_in_adtl() {
        let heron = Marker {
            note: "far bank".into(),
            ..marker(2, 30, 20, "Heron")
        };
        let marks = [heron, marker(7, 10, 0, "")];
        let w = parse(&mut Cursor::new(marked(&marks))).unwrap();
        assert_eq!(w.cues.unwrap(), [marks[1].clone(), marks[0].clone()]);
    }

    #[test]
    fn a_cue_point_with_only_its_position_set_is_still_placed() {
        let (mut cue, _) = mark_bodies(&[marker(1, 5, 0, "")], &StoredMarks::default());
        cue[24..28].copy_from_slice(&0u32.to_le_bytes());
        let file = build(false, &[(b"cue ", cue)], &[0; 10]);
        let w = parse(&mut Cursor::new(&file)).unwrap();
        assert_eq!(w.cues.unwrap()[0].frame, 5);
        let plain = parse(&mut Cursor::new(&build(false, &[], &[0; 10]))).unwrap();
        assert!(plain.cues.is_none());
    }

    #[test]
    fn text_that_is_not_utf_8_reads_as_windows_1252() {
        assert_eq!(text(b"Caf\xE9 \0junk"), "Caf\u{E9}");
        assert_eq!(text(b"\x93hi\x94 \x805"), "\u{201C}hi\u{201D} \u{20AC}5");
        assert_eq!(text("Rõõm".as_bytes()), "Rõõm");
    }

    #[test]
    fn marks_an_edit_leaves_alone_go_back_as_stored() {
        // As older Windows software writes them: a name in Windows-1252, a
        // region named only in its ltxt, and a subchunk of another kind.
        let mut cue = 2u32.to_le_bytes().to_vec();
        for (id, frame) in [(1u32, 10u32), (2, 40)] {
            cue.extend(id.to_le_bytes());
            cue.extend(frame.to_le_bytes());
            cue.extend(b"data");
            cue.extend([0; 8]);
            cue.extend(frame.to_le_bytes());
        }
        let mut adtl = Vec::new();
        subchunk(&mut adtl, b"labl", b"\x01\0\0\0R\xF5\xF5m\0");
        subchunk(
            &mut adtl,
            b"ltxt",
            b"\x02\0\0\0\x32\0\0\0rgn \x01\0\x09\0\0\0\xE4\x04Caf\xE9\0",
        );
        subchunk(&mut adtl, b"file", b"\x02\0\0\0xy");
        let list = [b"adtl".as_slice(), &adtl].concat();
        let file = build(
            false,
            &[(b"cue ", cue.clone()), (b"LIST", list.clone())],
            &[0; 100],
        );
        let marks = parse(&mut Cursor::new(&file)).unwrap().cues.unwrap();
        assert_eq!(marks, [marker(1, 10, 0, "Rõõm"), marker(2, 40, 50, "Café")]);

        let mut stored = StoredMarks::default();
        stored.add_cue(&cue);
        stored.add_adtl(&adtl);
        assert_eq!(mark_bodies(&marks, &stored), (cue, Some(list)));

        // Moved, the first keeps its name as stored; renamed, the second
        // loses the old name from its region.
        let mut edited = marks.clone();
        edited[0].frame = 20;
        edited[1].label = "Kohvik".into();
        let (cue, list) = mark_bodies(&edited, &stored);
        let list = list.unwrap();
        let holds = |part: &[u8]| list.windows(part.len()).any(|w| w == part);
        assert!(holds(b"R\xF5\xF5m\0") && holds(b"Kohvik\0") && holds(b"file"));
        assert!(!holds(b"Caf\xE9"));
        let file = build(false, &[(b"cue ", cue), (b"LIST", list)], &[0; 100]);
        assert_eq!(
            parse(&mut Cursor::new(&file)).unwrap().cues.unwrap(),
            edited
        );
    }

    #[test]
    fn info_an_edit_leaves_alone_goes_back_as_stored() {
        let mut entries = Vec::new();
        subchunk(&mut entries, b"ICMT", b"Caf\xE9 \0");
        subchunk(&mut entries, b"INAM", b"Take\0");
        let list = [b"INFO".as_slice(), &entries].concat();
        let file = build(false, &[(b"LIST", list.clone())], &[0; 4]);
        let info = parse(&mut Cursor::new(&file)).unwrap().info;
        assert_eq!(
            info,
            [(*b"ICMT", "Café".to_owned()), (*b"INAM", "Take".to_owned())]
        );
        let stored: Vec<([u8; 4], Vec<u8>)> = subchunks(&entries)
            .map(|(id, body)| (id, body.to_vec()))
            .collect();
        assert_eq!(info_body(&info, &stored), list);
        let renamed = [info[0].clone(), (*b"INAM", "Take 2".to_owned())];
        let body = info_body(&renamed, &stored);
        let holds = |part: &[u8]| body.windows(part.len()).any(|w| w == part);
        assert!(holds(b"Caf\xE9 \0") && holds(b"Take 2\0"));
    }
}
