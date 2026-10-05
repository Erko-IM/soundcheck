//! Every tag, picture and marker a recording has, carried into a new file
//! of another format.
//!
//! A field goes by what it means where lofty knows it in both formats: a
//! title is TIT2 in an MP3, TITLE in a FLAC and ©nam in an M4A. One a
//! format has no place for goes by its Vorbis comment name, as most
//! programs read it. Any other field keeps the name the file gave it,
//! `Species` becoming TXXX:Species, Species, ----:com.apple.iTunes:Species,
//! and a Matroska SimpleTag, as each format keeps a field of its own
//! naming. A track's number and count are two fields, as Vorbis comments
//! keep them, which go together again as TRCK's "3/12" and an M4A's trkn;
//! a disc's likewise. A tag of the same kind as the new file's goes across
//! whole, with whatever it holds besides text.
//!
//! A WAV's own chunks go into a WAV as they are, and into a FLAC as the
//! foreign metadata `flac --keep-foreign-metadata` keeps, which a
//! conversion back to WAV puts back. Into any other format they go as
//! fields: Broadcast WAV as `BWF_DESCRIPTION` and the like, iXML as `IXML`,
//! GUANO as `GUANO|Make` and so on, which a conversion back to WAV makes
//! chunks of again. Markers go where each format keeps them.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use lofty::TextEncoding;
use lofty::ape::ApeTag;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, FileType};
use lofty::id3::v1::Id3v1Tag;
use lofty::id3::v2::{
    AttachedPictureFrame, ChapterFrame, ChapterTableOfContentsFrame, CtocFlags, Frame, FrameId,
    FrameList, Id3v2Tag, TextInformationFrame,
};
use lofty::iff::aiff::{AiffFile, AiffTextChunks};
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OggPictureStorage, OpusFile, SpeexFile, VorbisFile};
use lofty::picture::{MimeType, Picture, PictureInformation, PictureType};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, ItemValue, TagExt, TagType};

use super::Format;
use crate::audio::{Opened, Source};
use crate::edit::Edits;
use crate::tags::{self, Kind};
use crate::wav::{self, Marker, Wav};

/// Non-audio chunks are carried up to this size; past it, a damaged size
/// field is not taken at its word.
const CHUNK_MAX: u64 = 64 << 20;

/// Fields that describe the original's encoding rather than the recording,
/// and would be wrong in the new file: the gapless padding of an MP3 or AAC.
const STALE: [&str; 1] = ["iTunSMPB"];

/// The Broadcast WAV fields, as carried in other formats, in the order of
/// the chunk.
const BWF: [&str; 5] = [
    "BWF_DESCRIPTION",
    "BWF_ORIGINATOR",
    "BWF_ORIGINATOR_REFERENCE",
    "BWF_ORIGINATION_DATE",
    "BWF_ORIGINATION_TIME",
];
const BWF_TIME_REFERENCE: &str = "BWF_TIME_REFERENCE";
const BWF_CODING_HISTORY: &str = "BWF_CODING_HISTORY";
const BWF_UMID: &str = "BWF_UMID";
const IXML: &str = "IXML";
const GUANO: &str = "GUANO|";
/// The rate GUANO says a recording was made at: the file's, or for a time
/// expanded one the rate before.
const GUANO_RATE: &str = "GUANO|Samplerate";
/// The gain a conversion lowered a file by to fit whole numbers.
const GAIN: &str = "SOUNDCHECK_GAIN";

/// What a field is: one lofty knows across formats, or one of the file's
/// own naming.
#[derive(Clone, Debug, PartialEq)]
pub enum Name {
    Known(ItemKey),
    Custom(String),
}

/// The kind of tag a field came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Id3v2,
    Id3v1,
    Ape,
    Vorbis,
    Mp4,
    AiffText,
    Info,
    Bext,
    Ixml,
    Guano,
    /// A format only symphonia reads, or soundcheck's own note.
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: Name,
    /// As the file stored it, for a format with no key of its own for a
    /// field lofty knows.
    pub key: String,
    pub value: String,
    pub origin: Origin,
}

impl Field {
    fn custom(name: &str, value: impl Into<String>, origin: Origin) -> Self {
        Self {
            name: Name::Custom(name.to_owned()),
            key: name.to_owned(),
            value: value.into(),
            origin,
        }
    }

    /// The name it goes under where nothing else names it.
    fn own_name(&self) -> &str {
        match &self.name {
            Name::Custom(name) => name,
            Name::Known(_) => &self.key,
        }
    }

    /// The name it goes under in a tag with no place of its own for it:
    /// its Vorbis comment name where lofty knows the field.
    fn plain_name(&self) -> &str {
        match &self.name {
            Name::Known(key) => key.map_key(TagType::VorbisComments).unwrap_or(&self.key),
            Name::Custom(name) => name,
        }
    }

    fn same(&self, other: &Self) -> bool {
        let names = match (&self.name, &other.name) {
            (Name::Known(a), Name::Known(b)) => a == b,
            (Name::Custom(a), Name::Custom(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        };
        names && self.value == other.value
    }
}

/// A WAV's chunks besides its format and audio, as stored, with each
/// chunk's id and body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Riff {
    pub before: Vec<([u8; 4], Vec<u8>)>,
    pub after: Vec<([u8; 4], Vec<u8>)>,
}

impl Riff {
    fn has(&self, id: &[u8; 4]) -> bool {
        self.before.iter().chain(&self.after).any(|(i, _)| i == id)
    }

    fn has_list(&self, kind: &[u8; 4]) -> bool {
        self.before
            .iter()
            .chain(&self.after)
            .any(|(id, body)| id == b"LIST" && body.starts_with(kind))
    }
}

/// A track's number with its count, and a disc's, which Vorbis comments
/// keep apart and ID3 and APE keep together as "3/12".
const NUMBERS: [(ItemKey, ItemKey); 2] = [
    (ItemKey::TrackNumber, ItemKey::TrackTotal),
    (ItemKey::DiscNumber, ItemKey::DiscTotal),
];

/// What `key` means in a tag of kind `tag`. lofty reads TRCK, TPOS, trkn,
/// disk and APE's Track and Disc as the count, though each holds the
/// number first.
fn item_key(tag: TagType, key: &str) -> Option<ItemKey> {
    NUMBERS
        .iter()
        .map(|(number, _)| *number)
        .find(|number| {
            number
                .map_key(tag)
                .is_some_and(|k| k.eq_ignore_ascii_case(key))
        })
        .or_else(|| ItemKey::from_key(tag, key))
}

/// Whether `text` is a number in digits alone, as a track's is.
fn digits(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// A WAV chunk of the original with what it counts in samples counted by
/// `at` instead, and the rate it says the file is at as `rate`: the
/// Broadcast WAV time reference, the cue points and the lengths of the
/// stretches they start, a sampler's loops, iXML and GUANO. The rest of
/// each stays as it was, byte for byte.
fn chunk_at(id: &[u8; 4], body: &mut Vec<u8>, at: &impl Fn(u64) -> u64, rate: u32) {
    let u32_at = |body: &mut [u8], offset: usize, to: &dyn Fn(u32) -> u32| {
        if let Some(bytes) = body.get_mut(offset..offset + 4) {
            let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            bytes.copy_from_slice(&to(n).to_le_bytes());
        }
    };
    let scaled = |n: u32| u32::try_from(at(u64::from(n))).unwrap_or(u32::MAX);
    match id {
        b"bext" => {
            if let Some(bytes) = body.get_mut(338..346) {
                let mut reference = [0; 8];
                reference.copy_from_slice(bytes);
                bytes.copy_from_slice(&at(u64::from_le_bytes(reference)).to_le_bytes());
            }
        }
        b"cue " => {
            let points = body
                .get(..4)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            for point in 0..points as usize {
                let start = 4 + point * 24;
                // Its place in the play order, and in the samples.
                u32_at(body, start + 4, &scaled);
                u32_at(body, start + 20, &scaled);
            }
        }
        b"LIST" if body.starts_with(b"adtl") => {
            let mut at_sub = 4;
            while at_sub + 8 <= body.len() {
                let size = u32::from_le_bytes([
                    body[at_sub + 4],
                    body[at_sub + 5],
                    body[at_sub + 6],
                    body[at_sub + 7],
                ]) as usize;
                if &body[at_sub..at_sub + 4] == b"ltxt" {
                    u32_at(body, at_sub + 12, &scaled);
                }
                at_sub += 8 + size + (size & 1);
            }
        }
        b"smpl" => {
            u32_at(body, 8, &|_| (1e9 / f64::from(rate)).round() as u32);
            let loops = body
                .get(28..32)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            for n in 0..loops as usize {
                let start = 36 + n * 24;
                u32_at(body, start + 8, &scaled);
                u32_at(body, start + 12, &scaled);
            }
        }
        b"iXML" => {
            if let Ok(text) = std::str::from_utf8(body) {
                *body = ixml_at(text, at, rate).into_bytes();
            }
        }
        b"guan" => {
            if let Ok(text) = std::str::from_utf8(body) {
                *body = guano_at(text, at).into_bytes();
            }
        }
        _ => {}
    }
}

/// iXML with what it counts in samples counted by `at` instead: its sync
/// points, the Broadcast WAV time reference it repeats, and the file's rate
/// as `rate`. Its timestamp names the rate it counts at itself, so it
/// stays, as does everything else, byte for byte.
fn ixml_at(xml: &str, at: &impl Fn(u64) -> u64, rate: u32) -> String {
    use crate::meta::{self, Element, Span};
    let Some(root) = meta::parse_xml(xml) else {
        return xml.to_owned();
    };
    let number = |e: Option<&Element>| {
        let e = e?;
        match &e.span {
            Some(Span::Content(range)) => Some((e.text.trim().parse::<u64>().ok()?, range.clone())),
            _ => None,
        }
    };
    // A count kept as its low 32 bits and, beside them, its high ones.
    let split = |low: Option<&Element>, high: Option<&Element>| {
        let Some((low, low_at)) = number(low) else {
            return Vec::new();
        };
        match number(high) {
            Some((high, high_at)) => {
                let n = at((high << 32) | low);
                vec![
                    (low_at, (n & 0xffff_ffff).to_string()),
                    (high_at, (n >> 32).to_string()),
                ]
            }
            None => {
                let n = at(low);
                if n <= u64::from(u32::MAX) {
                    vec![(low_at, n.to_string())]
                } else {
                    Vec::new()
                }
            }
        }
    };
    let mut edits = Vec::new();
    if let Some(bext) = root.child("BEXT") {
        edits.extend(split(
            bext.child("BWF_TIME_REFERENCE_LOW"),
            bext.child("BWF_TIME_REFERENCE_HIGH"),
        ));
    }
    if let Some(list) = root.child("SYNC_POINT_LIST") {
        for point in list.children.iter().filter(|c| c.name == "SYNC_POINT") {
            edits.extend(split(
                point.child("SYNC_POINT_LOW"),
                point.child("SYNC_POINT_HIGH"),
            ));
            if let Some((length, range)) = number(point.child("SYNC_POINT_EVENT_DURATION")) {
                edits.push((range, at(length).to_string()));
            }
        }
    }
    if let Some((_, range)) = number(
        root.child("SPEED")
            .and_then(|s| s.child("FILE_SAMPLE_RATE")),
    ) {
        edits.push((range, rate.to_string()));
    }
    // From the end back, so the places still to change stay where they were.
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut out = xml.to_owned();
    for (range, text) in edits {
        out.replace_range(range, &text);
    }
    out
}

/// GUANO's text with its Samplerate counted by `at` instead.
fn guano_at(text: &str, at: &impl Fn(u64) -> u64) -> String {
    let rate = &GUANO_RATE[GUANO.len()..];
    text.split_inclusive('\n')
        .map(|line| {
            let (body, end) = line.split_at(line.trim_end_matches(['\r', '\n']).len());
            match body.split_once(':') {
                Some((key, value)) if key.trim() == rate => match value.trim().parse::<u64>() {
                    Ok(n) => format!("{key}: {}{end}", at(n)),
                    Err(_) => line.to_owned(),
                },
                _ => line.to_owned(),
            }
        })
        .collect()
}

/// Whether a tag kept whole holds a field already, as reading it with
/// `read` gives it.
fn holds(read: impl FnOnce(&mut Carried)) -> impl Fn(&Field) -> bool {
    let mut whole = Carried::default();
    read(&mut whole);
    move |field| whole.fields.iter().any(|w| w.same(field))
}

/// Everything a recording says about itself besides its audio.
#[derive(Clone, Debug, Default)]
pub struct Carried {
    pub fields: Vec<Field>,
    pub pictures: Vec<Picture>,
    pub markers: Vec<Marker>,
    /// The rate the markers' positions count in.
    pub rate: u32,
    /// The original's own WAV chunks: those of a WAV, or those a FLAC keeps
    /// as foreign metadata.
    pub riff: Option<Riff>,
    /// Tags kept whole, for a new file that keeps the same kind.
    pub id3v2: Option<Id3v2Tag>,
    pub vorbis: Option<VorbisComments>,
    pub ilst: Option<Ilst>,
    pub aiff_text: Option<AiffTextChunks>,
}

impl Carried {
    fn push(&mut self, origin: Origin, key: &str, name: Name, value: &str) {
        let value = value.trim_end_matches('\0');
        if value.trim().is_empty() || STALE.iter().any(|s| key.ends_with(s)) {
            return;
        }
        if let Name::Known(known) = &name
            && let Some((_, total)) = NUMBERS.iter().find(|(number, _)| number == known)
            && let Some((number, count)) = value.split_once('/')
            && digits(number)
            && digits(count)
        {
            self.push(origin, key, name.clone(), number.trim());
            let total_key = total.map_key(TagType::VorbisComments).unwrap_or(key);
            self.push(origin, total_key, Name::Known(*total), count.trim());
            return;
        }
        let field = Field {
            name,
            key: key.to_owned(),
            value: value.to_owned(),
            origin,
        };
        if !self.fields.iter().any(|f| f.same(&field)) {
            self.fields.push(field);
        }
    }

    fn push_known(&mut self, origin: Origin, tag: TagType, key: &str, value: &str) {
        let name = item_key(tag, key).map_or_else(|| Name::Custom(key.into()), Name::Known);
        self.push(origin, key, name, value);
    }

    fn picture(&mut self, picture: Picture) {
        if !self.pictures.iter().any(|p| p.data() == picture.data()) {
            self.pictures.push(picture);
        }
    }

    /// Notes the gain a conversion lowered the level by.
    pub fn note_gain(&mut self, gain: f32) {
        self.fields
            .push(Field::custom(GAIN, super::decibels(gain), Origin::Other));
    }

    /// Positions counted at `rate` instead, for a new file at that rate:
    /// the markers', and every count of samples the tags and the WAV chunks
    /// keep, with the rate they say the file is at.
    pub fn rescale(&mut self, rate: u32) {
        if rate == self.rate || self.rate == 0 {
            return;
        }
        let from = self.rate;
        let at = |samples: u64| (u128::from(samples) * u128::from(rate) / u128::from(from)) as u64;
        for m in &mut self.markers {
            m.frame = at(m.frame as u64) as usize;
            m.length = at(m.length as u64) as usize;
        }
        for f in &mut self.fields {
            match f.own_name() {
                BWF_TIME_REFERENCE | GUANO_RATE => {
                    if let Ok(samples) = f.value.trim().parse::<u64>() {
                        f.value = at(samples).to_string();
                    }
                }
                IXML => f.value = ixml_at(&f.value, &at, rate),
                _ => {}
            }
        }
        if let Some(riff) = &mut self.riff {
            for (id, body) in riff.before.iter_mut().chain(riff.after.iter_mut()) {
                chunk_at(id, body, &at, rate);
            }
        }
        self.rate = rate;
    }

    /// Fields the WAV chunks being carried already hold.
    fn in_riff(&self, field: &Field) -> bool {
        let Some(riff) = &self.riff else {
            return false;
        };
        match field.origin {
            Origin::Info => riff.has_list(b"INFO"),
            Origin::Bext => riff.has(b"bext"),
            Origin::Ixml => riff.has(b"iXML"),
            Origin::Guano => riff.has(b"guan"),
            Origin::Id3v2 => riff.has(b"id3 ") || riff.has(b"ID3 "),
            _ => false,
        }
    }
}

/// Reads all `path` says about itself. `opened` is it opened by the
/// window's reader.
pub fn read(path: &Path, opened: &Opened) -> Result<Carried, String> {
    let mut carried = Carried {
        rate: opened.info.sample_rate,
        ..Carried::default()
    };
    match &opened.source {
        Source::Pcm { .. } => {
            let mut file = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
            let w = wav::parse(&mut file).map_err(|e| e.to_string())?;
            let riff = riff_of(&mut file, &w)?;
            read_wav(&w, riff, &mut carried);
            if let Ok(Some(tag)) = tags::wav_id3v2(path) {
                read_id3v2(&tag, &mut carried);
                carried.id3v2 = Some(tag);
            }
        }
        Source::Coded(_) => {
            // lofty reads neither Matroska nor CAF, and takes some for
            // ADTS by what their first bytes look like. A file whose tags
            // lofty fails on still has what symphonia reads of them.
            let container = opened.info.container.as_str();
            let lofty_reads = !matches!(container, "MKA" | "MKV" | "WEBM" | "CAF");
            if !(lofty_reads && read_lofty(path, &mut carried).unwrap_or(false)) {
                read_symphonia(path, &mut carried)?;
            }
            if carried.markers.is_empty() {
                carried.markers = match opened.info.container.as_str() {
                    "AIFF" | "AIF" | "AIFC" => super::pcm::aiff_markers(path)?,
                    "CAF" => super::pcm::caf_markers(path)?,
                    "M4A" | "MP4" => super::mp4::chapters(path, carried.rate)?,
                    _ => Vec::new(),
                };
            }
            if carried.markers.is_empty() {
                carried.markers = symphonia_chapters(path, carried.rate);
            }
            if opened.info.container == "CAF" {
                for (key, value) in super::pcm::caf_info(path)? {
                    let name = caf_name(&key);
                    carried.push(Origin::Other, &key, name, &value);
                }
            }
        }
    }
    carried.markers.sort_by_key(|m| (m.frame, m.id));
    Ok(carried)
}

/// The WAV's chunks other than its format, audio and padding, as stored.
fn riff_of(file: &mut File, w: &Wav) -> Result<Riff, String> {
    let mut riff = Riff::default();
    let mut after = false;
    for chunk in &w.chunks {
        match &chunk.id {
            b"data" => after = true,
            b"fmt " | b"ds64" | b"fact" | b"JUNK" | b"junk" | b"PAD " | b"pad " | b"FLLR" => {}
            id => {
                let len = chunk.body.end - chunk.body.start;
                if len > CHUNK_MAX {
                    continue;
                }
                let mut body = vec![0; len as usize];
                file.seek(SeekFrom::Start(chunk.body.start))
                    .and_then(|_| file.read_exact(&mut body))
                    .map_err(|e| format!("cannot read: {e}"))?;
                if after {
                    riff.after.push((*id, body));
                } else {
                    riff.before.push((*id, body));
                }
            }
        }
    }
    Ok(riff)
}

/// A WAV's own metadata, its chunks with it.
fn read_wav(w: &Wav, riff: Riff, carried: &mut Carried) {
    for (id, value) in &w.info {
        let key = String::from_utf8_lossy(id);
        carried.push_known(Origin::Info, TagType::RiffInfo, &key, value);
    }
    if let Some(b) = &w.bext {
        let fields = [
            &b.description,
            &b.originator,
            &b.originator_reference,
            &b.origination_date,
            &b.origination_time,
        ];
        for (name, value) in BWF.iter().zip(fields) {
            carried.push(Origin::Bext, name, Name::Custom((*name).into()), value);
        }
        if b.time_reference > 0 {
            let value = b.time_reference.to_string();
            carried.push(
                Origin::Bext,
                BWF_TIME_REFERENCE,
                Name::Custom(BWF_TIME_REFERENCE.into()),
                &value,
            );
        }
        carried.push(
            Origin::Bext,
            BWF_CODING_HISTORY,
            Name::Custom(BWF_CODING_HISTORY.into()),
            &b.coding_history,
        );
        if let Some(umid) = b.raw.get(348..412).filter(|u| u.iter().any(|&x| x != 0)) {
            let hex: String = umid.iter().map(|x| format!("{x:02x}")).collect();
            carried.push(Origin::Bext, BWF_UMID, Name::Custom(BWF_UMID.into()), &hex);
        }
    }
    if let Some(ixml) = &w.ixml {
        carried.push(Origin::Ixml, IXML, Name::Custom(IXML.into()), ixml.trim());
    }
    if let Some(text) = &w.guano {
        let block = tags::guano(text);
        let version = block.version.unwrap_or_else(|| "1.0".into());
        let key = format!("{GUANO}Version");
        carried.push(Origin::Guano, &key, Name::Custom(key.clone()), &version);
        for f in block.fields {
            let key = format!("{GUANO}{}", f.key);
            carried.push(Origin::Guano, &key, Name::Custom(key.clone()), &f.value);
        }
    }
    // Marks from a `cue ` chunk, or where there is none the sync points of
    // the recorder that wrote the iXML.
    carried.markers = Edits::from_wav(w).markers;
    carried.riff = Some(riff);
}

/// Reads the tags of a file lofty reads, and says whether it read one.
fn read_lofty(path: &Path, carried: &mut Carried) -> Result<bool, String> {
    let Some(file_type) = Probe::open(path)
        .ok()
        .and_then(|p| p.guess_file_type().ok())
        .and_then(|p| p.file_type())
    else {
        return Ok(false);
    };
    let failed = |e: lofty::error::FileParseError| format!("its tags cannot be read: {e}");
    let mut file = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
    let options = ParseOptions::new().read_properties(false);
    match file_type {
        FileType::Mpeg => {
            let f = MpegFile::read_from(&mut file, options).map_err(failed)?;
            if let Some(tag) = f.id3v2() {
                read_id3v2(tag, carried);
                carried.id3v2 = Some(tag.clone());
            }
            if let Some(tag) = f.ape() {
                read_ape(tag, carried);
            }
            if let Some(tag) = f
                .id3v1()
                .filter(|_| f.id3v2().is_none() && f.ape().is_none())
            {
                read_id3v1(tag, carried);
            }
        }
        FileType::Aac => {
            let f = lofty::aac::AacFile::read_from(&mut file, options).map_err(failed)?;
            if let Some(tag) = f.id3v2() {
                read_id3v2(tag, carried);
                carried.id3v2 = Some(tag.clone());
            }
            if let Some(tag) = f.id3v1().filter(|_| f.id3v2().is_none()) {
                read_id3v1(tag, carried);
            }
        }
        FileType::Flac => {
            let f = lofty::flac::FlacFile::read_from(&mut file, options).map_err(failed)?;
            // The foreign metadata of a FLAC made from a WAV first, so the
            // WAV's own fields lead, as they would have in the WAV.
            if let Some(riff) = super::flac::foreign(path)? {
                let w = riff_wav(&riff, carried.rate);
                read_wav(&w, riff, carried);
                if let Some(tag) = id3_in(carried.riff.as_ref()) {
                    read_id3v2(&tag, carried);
                    carried.id3v2 = Some(tag);
                }
            }
            if let Some(tag) = f.vorbis_comments() {
                read_vorbis(tag, carried);
                carried.vorbis = Some(tag.clone());
            }
            for (picture, _) in f.pictures() {
                carried.picture(picture.clone());
            }
            if let Some(tag) = f.id3v2() {
                read_id3v2(tag, carried);
            }
        }
        FileType::Vorbis => {
            let f = VorbisFile::read_from(&mut file, options).map_err(failed)?;
            read_vorbis(f.vorbis_comments(), carried);
            carried.vorbis = Some(f.vorbis_comments().clone());
        }
        FileType::Opus => {
            let f = OpusFile::read_from(&mut file, options).map_err(failed)?;
            read_vorbis(f.vorbis_comments(), carried);
            carried.vorbis = Some(f.vorbis_comments().clone());
        }
        FileType::Speex => {
            let f = SpeexFile::read_from(&mut file, options).map_err(failed)?;
            read_vorbis(f.vorbis_comments(), carried);
            carried.vorbis = Some(f.vorbis_comments().clone());
        }
        FileType::Mp4 => {
            let f = Mp4File::read_from(&mut file, options).map_err(failed)?;
            if let Some(tag) = f.ilst() {
                read_ilst(tag, carried);
                carried.ilst = Some(tag.clone());
            }
        }
        FileType::Aiff => {
            let f = AiffFile::read_from(&mut file, options).map_err(failed)?;
            if let Some(tag) = f.id3v2() {
                read_id3v2(tag, carried);
                carried.id3v2 = Some(tag.clone());
            }
            if let Some(tag) = f.text_chunks() {
                read_aiff_text(tag, carried);
                carried.aiff_text = Some(tag.clone());
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// A WAV of no audio around `riff`'s chunks, for the WAV reader to read
/// them as it reads any WAV's.
fn riff_wav(riff: &Riff, rate: u32) -> Wav {
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    let mut chunk = |id: &[u8; 4], body: &[u8]| {
        bytes.extend_from_slice(id);
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(body);
        if body.len() % 2 == 1 {
            bytes.push(0);
        }
    };
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&1u16.to_le_bytes());
    fmt.extend_from_slice(&rate.max(1).to_le_bytes());
    fmt.extend_from_slice(&(rate.max(1) * 2).to_le_bytes());
    fmt.extend_from_slice(&2u16.to_le_bytes());
    fmt.extend_from_slice(&16u16.to_le_bytes());
    chunk(b"fmt ", &fmt);
    for (id, body) in &riff.before {
        chunk(id, body);
    }
    chunk(b"data", &[]);
    for (id, body) in &riff.after {
        chunk(id, body);
    }
    wav::parse(&mut Cursor::new(bytes)).expect("a WAV built here reads")
}

/// The ID3v2 tag in a WAV's `id3 ` chunk.
fn id3_in(riff: Option<&Riff>) -> Option<Id3v2Tag> {
    let riff = riff?;
    let (_, body) = riff
        .before
        .iter()
        .chain(&riff.after)
        .find(|(id, _)| id == b"id3 " || id == b"ID3 ")?;
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    let mut fmt = Vec::new();
    for v in [1u16, 1] {
        fmt.extend_from_slice(&v.to_le_bytes());
    }
    for v in [8000u32, 16000] {
        fmt.extend_from_slice(&v.to_le_bytes());
    }
    for v in [2u16, 16] {
        fmt.extend_from_slice(&v.to_le_bytes());
    }
    for (id, body) in [
        (b"fmt ", fmt.as_slice()),
        (b"data", &[][..]),
        (b"id3 ", body),
    ] {
        bytes.extend_from_slice(id);
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(body);
        if body.len() % 2 == 1 {
            bytes.push(0);
        }
    }
    let size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    lofty::iff::wav::WavFile::read_from(
        &mut Cursor::new(bytes),
        ParseOptions::new().read_properties(false),
    )
    .ok()?
    .id3v2()
    .cloned()
}

fn read_id3v2(tag: &Id3v2Tag, carried: &mut Carried) {
    let mut markers = Vec::new();
    for frame in tag.iter() {
        match frame {
            Frame::Text(t) => {
                for value in t.value.split('\0') {
                    carried.push_known(Origin::Id3v2, TagType::Id3v2, frame.id_str(), value);
                }
            }
            Frame::UserText(t) => {
                let key = format!("TXXX:{}", t.description);
                let name = item_key(TagType::Id3v2, &t.description)
                    .or_else(|| item_key(TagType::VorbisComments, &t.description))
                    .map_or_else(|| Name::Custom(t.description.to_string()), Name::Known);
                for value in t.content.split('\0') {
                    carried.push(Origin::Id3v2, &key, name.clone(), value);
                }
            }
            Frame::Comment(c) if c.description.is_empty() => {
                carried.push_known(Origin::Id3v2, TagType::Id3v2, "COMM", &c.content);
            }
            Frame::Comment(c) => {
                let key = format!("COMM:{}", c.description);
                carried.push(
                    Origin::Id3v2,
                    &key,
                    Name::Custom(c.description.to_string()),
                    &c.content,
                );
            }
            Frame::UnsynchronizedText(u) if u.description.is_empty() => {
                carried.push_known(Origin::Id3v2, TagType::Id3v2, "USLT", &u.content);
            }
            Frame::UnsynchronizedText(u) => {
                let key = format!("USLT:{}", u.description);
                carried.push(
                    Origin::Id3v2,
                    &key,
                    Name::Custom(u.description.to_string()),
                    &u.content,
                );
            }
            Frame::Url(u) => {
                carried.push_known(Origin::Id3v2, TagType::Id3v2, frame.id_str(), u.url());
            }
            Frame::UserUrl(u) => {
                let key = format!("WXXX:{}", u.description);
                carried.push(
                    Origin::Id3v2,
                    &key,
                    Name::Custom(u.description.to_string()),
                    &u.content,
                );
            }
            Frame::Picture(p) => carried.picture(p.picture.clone().into_owned()),
            Frame::Chapter(c) => {
                let title = c.children.get_text(&FrameId::new("TIT2").expect("an ID"));
                let note = c.children.iter().find_map(|f| match f {
                    Frame::Comment(c) => Some(c.content.to_string()),
                    _ => None,
                });
                markers.push((c.times.clone(), title.map(str::to_owned), note));
            }
            _ => {}
        }
    }
    if carried.markers.is_empty() && !markers.is_empty() {
        let rate = u64::from(carried.rate);
        carried.markers = markers
            .into_iter()
            .zip(1..)
            .map(|((times, title, note), id)| {
                let at = |ms: u32| (u64::from(ms) * rate).div_ceil(1000) as usize;
                Marker {
                    id,
                    frame: at(times.start),
                    length: at(times.end).saturating_sub(at(times.start)),
                    label: title.unwrap_or_default(),
                    note: note.unwrap_or_default(),
                }
            })
            .collect();
    }
}

/// The key a text frame goes by, as [`key_in`] gives keys, and its text.
fn frame_key(frame: &Frame<'_>) -> Option<(String, String)> {
    let named = |id: &str, description: &str| {
        if description.is_empty() {
            id.to_owned()
        } else {
            format!("{id}:{description}")
        }
    };
    Some(match frame {
        Frame::Text(t) => (frame.id_str().to_owned(), t.value.to_string()),
        Frame::UserText(t) => (format!("TXXX:{}", t.description), t.content.to_string()),
        Frame::Comment(c) => (named("COMM", &c.description), c.content.to_string()),
        Frame::UnsynchronizedText(u) => (named("USLT", &u.description), u.content.to_string()),
        Frame::Url(u) => (frame.id_str().to_owned(), u.url().to_owned()),
        Frame::UserUrl(u) => (format!("WXXX:{}", u.description), u.content.to_string()),
        _ => return None,
    })
}

fn read_id3v1(tag: &Id3v1Tag, carried: &mut Carried) {
    let text = [
        (ItemKey::TrackTitle, "Title", tag.title.clone()),
        (ItemKey::TrackArtist, "Artist", tag.artist.clone()),
        (ItemKey::AlbumTitle, "Album", tag.album.clone()),
        (ItemKey::Year, "Year", tag.year.map(|y| y.to_string())),
        (ItemKey::Comment, "Comment", tag.comment.clone()),
        (
            ItemKey::TrackNumber,
            "Track",
            tag.track_number.map(|t| t.to_string()),
        ),
    ];
    for (key, name, value) in text {
        if let Some(value) = value {
            carried.push(Origin::Id3v1, name, Name::Known(key), &value);
        }
    }
}

fn read_ape(tag: &ApeTag, carried: &mut Carried) {
    for item in tag {
        match item.value() {
            ItemValue::Text(t) | ItemValue::Locator(t) => {
                for value in t.split('\0') {
                    carried.push_known(Origin::Ape, TagType::Ape, item.key(), value);
                }
            }
            ItemValue::Binary(b) => {
                if let Ok(picture) = Picture::from_ape_bytes(item.key(), b) {
                    carried.picture(picture);
                }
            }
        }
    }
}

/// Chapters as Vorbis comments carry them: `CHAPTER001=00:01:02.500` with
/// its `CHAPTER001NAME`, and `CHAPTER001END` and `CHAPTER001NOTE`, which
/// soundcheck adds for a marker's length and note.
fn read_vorbis(tag: &VorbisComments, carried: &mut Carried) {
    let mut chapters: Vec<(u32, Marker)> = Vec::new();
    let rate = carried.rate;
    for (key, value) in tag.items() {
        if let Some((n, part)) = chapter_key(key) {
            let i = match chapters.iter().position(|(m, _)| *m == n) {
                Some(i) => i,
                None => {
                    chapters.push((
                        n,
                        Marker {
                            id: n,
                            frame: 0,
                            length: 0,
                            label: String::new(),
                            note: String::new(),
                        },
                    ));
                    chapters.len() - 1
                }
            };
            let marker = &mut chapters[i].1;
            match part {
                "" => marker.frame = frames_at(value, rate).unwrap_or(0),
                "NAME" => marker.label = value.to_owned(),
                "NOTE" => marker.note = value.to_owned(),
                "END" => {
                    let end = frames_at(value, rate).unwrap_or(0);
                    marker.length = end.saturating_sub(marker.frame);
                }
                _ => {}
            }
            continue;
        }
        carried.push_known(Origin::Vorbis, TagType::VorbisComments, key, value);
    }
    for (picture, _) in tag.pictures() {
        carried.picture(picture.clone());
    }
    if carried.markers.is_empty() {
        // An END read before its start leaves a length to put right.
        carried.markers = chapters.into_iter().map(|(_, m)| m).collect();
    }
}

/// `CHAPTER012NAME` as 12 and `NAME`.
fn chapter_key(key: &str) -> Option<(u32, &str)> {
    let upper = key.get(..7)?;
    if !upper.eq_ignore_ascii_case("CHAPTER") {
        return None;
    }
    let rest = &key[7..];
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let n = rest[..digits].parse().ok()?;
    let part = &rest[digits..];
    ["", "NAME", "NOTE", "END"]
        .iter()
        .find(|p| p.eq_ignore_ascii_case(part))
        .map(|p| (n, *p))
}

/// `hh:mm:ss.sss` as frames at `rate`.
fn frames_at(text: &str, rate: u32) -> Option<usize> {
    let mut parts = text.trim().split(':');
    let (h, m, s) = (parts.next()?, parts.next()?, parts.next()?);
    let seconds =
        h.parse::<f64>().ok()? * 3600.0 + m.parse::<f64>().ok()? * 60.0 + s.parse::<f64>().ok()?;
    Some((seconds * f64::from(rate)).round() as usize)
}

/// Frames at `rate` as `hh:mm:ss.sss`.
fn clock(frames: usize, rate: u32) -> String {
    let ms = (frames as u128 * 1000 / u128::from(rate.max(1))) as u64;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn read_ilst(tag: &Ilst, carried: &mut Carried) {
    let numbers = [
        (ItemKey::TrackNumber, tag.track()),
        (ItemKey::TrackTotal, tag.track_total()),
        (ItemKey::DiscNumber, tag.disk()),
        (ItemKey::DiscTotal, tag.disk_total()),
    ];
    for (key, value) in numbers {
        if let Some(value) = value.filter(|&v| v > 0) {
            let name = key
                .map_key(TagType::VorbisComments)
                .unwrap_or("TRACKNUMBER");
            carried.push(Origin::Mp4, name, Name::Known(key), &value.to_string());
        }
    }
    for atom in tag {
        let key = match atom.ident() {
            AtomIdent::Fourcc(code) => tags::fourcc_key(code),
            AtomIdent::Freeform { mean, name } => format!("----:{mean}:{name}"),
        };
        for data in atom.data() {
            match data {
                AtomData::UTF8(text) | AtomData::UTF16(text) => {
                    let name = item_key(TagType::Mp4Ilst, &key)
                        .or_else(|| match atom.ident() {
                            AtomIdent::Freeform { name, .. } => {
                                item_key(TagType::VorbisComments, name)
                            }
                            AtomIdent::Fourcc(_) => None,
                        })
                        .map_or_else(
                            || match atom.ident() {
                                AtomIdent::Freeform { name, .. } => Name::Custom(name.to_string()),
                                AtomIdent::Fourcc(_) => Name::Custom(key.clone()),
                            },
                            Name::Known,
                        );
                    carried.push(Origin::Mp4, &key, name, text);
                }
                AtomData::Picture(p) => carried.picture(p.clone()),
                _ => {}
            }
        }
    }
}

fn read_aiff_text(tag: &AiffTextChunks, carried: &mut Carried) {
    for (key, value) in [
        ("NAME", &tag.name),
        ("AUTH", &tag.author),
        ("(c) ", &tag.copyright),
    ] {
        if let Some(value) = value {
            carried.push_known(Origin::AiffText, TagType::AiffText, key, value);
        }
    }
    for a in tag.annotations.iter().flatten() {
        carried.push_known(Origin::AiffText, TagType::AiffText, "ANNO", a);
    }
    for c in tag.comments.iter().flatten() {
        carried.push_known(Origin::AiffText, TagType::AiffText, "COMT", &c.text);
    }
}

/// The tags, pictures and attachments of a file only symphonia reads, as
/// Matroska is. A tag's name is read as a Vorbis comment's would be, as
/// Matroska's SimpleTags are named the same way.
fn read_symphonia(path: &Path, carried: &mut Carried) -> Result<(), String> {
    use symphonia::core::meta::RawValue;
    let Some(mut format) = symphonia_format(path) else {
        return Ok(());
    };
    let revision = format.metadata().skip_to_latest().cloned();
    if let Some(revision) = revision {
        let containers =
            std::iter::once(&revision.media).chain(revision.per_track.iter().map(|t| &t.metadata));
        for container in containers {
            for tag in &container.tags {
                let value = match &tag.raw.value {
                    RawValue::String(s) => s.to_string(),
                    RawValue::StringList(list) => list.join("; "),
                    RawValue::SignedInt(n) => n.to_string(),
                    RawValue::UnsignedInt(n) => n.to_string(),
                    RawValue::Float(n) => n.to_string(),
                    RawValue::Boolean(b) => b.to_string(),
                    _ => continue,
                };
                let key = tag_name(&tag.raw.key);
                let name = item_key(TagType::VorbisComments, key)
                    .map_or_else(|| Name::Custom(key.to_owned()), Name::Known);
                carried.push(Origin::Other, key, name, &value);
            }
            for visual in &container.visuals {
                carried.picture(picture_of(
                    visual.data.to_vec(),
                    visual.media_type.as_deref(),
                    None,
                ));
            }
        }
    }
    for attachment in format.attachments() {
        if let symphonia::core::formats::Attachment::File(file) = attachment {
            let image = file
                .media_type
                .as_deref()
                .is_some_and(|m| m.starts_with("image/"));
            if image {
                carried.picture(picture_of(
                    file.data.to_vec(),
                    file.media_type.as_deref(),
                    file.description.as_deref(),
                ));
            }
        }
    }
    Ok(())
}

/// A Matroska SimpleTag's own name from the key symphonia gives it, which
/// leads with the level the tag is for, as `ALBUM@TITLE`, and the parents
/// of a nested tag, as `ALBUM@ORIGINAL/TITLE`.
fn tag_name(key: &str) -> &str {
    let key = key.rsplit('/').next().unwrap_or(key);
    match key.split_once('@') {
        Some((level, name))
            if !name.is_empty()
                && level
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'#') =>
        {
            name
        }
        _ => key,
    }
}

fn picture_of(data: Vec<u8>, media_type: Option<&str>, description: Option<&str>) -> Picture {
    let mut builder = Picture::unchecked(data).pic_type(PictureType::CoverFront);
    if let Some(media_type) = media_type {
        builder = builder.mime_type(MimeType::from_str(media_type));
    }
    if let Some(description) = description {
        builder = builder.description(description.to_owned());
    }
    builder.build()
}

fn symphonia_format(path: &Path) -> Option<Box<dyn symphonia::core::formats::FormatReader>> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    let file = File::open(path).ok()?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    symphonia::default::get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .ok()
}

/// Chapters symphonia reads, as markers at `rate`.
fn symphonia_chapters(path: &Path, rate: u32) -> Vec<Marker> {
    use symphonia::core::meta::ChapterGroupItem;
    use symphonia::core::meta::StandardTag;
    let Some(format) = symphonia_format(path) else {
        return Vec::new();
    };
    let Some(group) = format.chapters() else {
        return Vec::new();
    };
    let mut markers = Vec::new();
    let mut stack = vec![group];
    while let Some(group) = stack.pop() {
        for item in &group.items {
            match item {
                ChapterGroupItem::Group(g) => stack.push(g),
                ChapterGroupItem::Chapter(c) => {
                    let frames = |t: &symphonia::core::units::Time| {
                        (t.as_secs_f64() * f64::from(rate)).round() as usize
                    };
                    let start = frames(&c.start_time);
                    let end = c.end_time.as_ref().map_or(start, frames);
                    let label = c.tags.iter().find_map(|t| match &t.std {
                        Some(StandardTag::ChapterTitle(s)) => Some(s.to_string()),
                        _ => None,
                    });
                    markers.push(Marker {
                        id: markers.len() as u32 + 1,
                        frame: start,
                        length: end.saturating_sub(start),
                        label: label.unwrap_or_default(),
                        note: String::new(),
                    });
                }
            }
        }
    }
    markers
}

/// The fields, each under the key it goes by in a tag of `kind`, values for
/// one key together, in the order they first come. Those `skip` says are
/// already in place are left out.
fn keyed(
    carried: &Carried,
    kind: TagType,
    skip: impl Fn(&Field) -> bool,
) -> Vec<(String, Vec<String>)> {
    let mut keyed: Vec<(String, Vec<String>)> = Vec::new();
    for field in carried.fields.iter().filter(|f| !skip(f)) {
        let key = key_in(field, kind);
        match keyed.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(&key)) {
            Some((_, values)) if !values.contains(&field.value) => {
                values.push(field.value.clone());
            }
            Some(_) => {}
            None => keyed.push((key, vec![field.value.clone()])),
        }
    }
    keyed
}

/// The key `field` goes under in a tag of `kind`.
fn key_in(field: &Field, kind: TagType) -> String {
    let own = field.own_name();
    let mapped = match &field.name {
        Name::Known(key) => key.map_key(kind),
        Name::Custom(_) => None,
    };
    match kind {
        TagType::Id3v2 => match mapped {
            // TRCK and TPOS hold a count only after its number, as
            // Carried::id3v2 puts them together; a count alone goes apart.
            _ if matches!(
                field.name,
                Name::Known(ItemKey::TrackTotal | ItemKey::DiscTotal)
            ) =>
            {
                format!("TXXX:{}", field.plain_name())
            }
            // TIPL and TMCL pair roles with names, which plain text is not.
            Some(id) if Kind::Id3v2.check_key(id).is_ok() && !matches!(id, "TIPL" | "TMCL") => {
                id.to_owned()
            }
            Some(description) if !description.contains(':') && description.len() > 4 => {
                format!("TXXX:{description}")
            }
            _ if field.origin == Origin::Id3v2 && Kind::Id3v2.check_key(&field.key).is_ok() => {
                field.key.clone()
            }
            _ => format!("TXXX:{}", field.plain_name()),
        },
        TagType::Mp4Ilst => match mapped {
            Some(key) if !matches!(key, "trkn" | "disk" | "rate" | "tmpo") => key.to_owned(),
            _ if field.origin == Origin::Mp4 && Kind::Mp4.check_key(&field.key).is_ok() => {
                field.key.clone()
            }
            _ => format!("----:com.apple.iTunes:{}", field.plain_name()),
        },
        TagType::VorbisComments => match mapped {
            Some(key) => key.to_owned(),
            None => vorbis_key(own),
        },
        _ => mapped.map_or_else(|| own.to_owned(), str::to_owned),
    }
}

/// A Vorbis comment's name for `name`: printable ASCII but `=`, which is
/// what a name can hold; anything else is spelled with `_`.
fn vorbis_key(name: &str) -> String {
    let key: String = name
        .chars()
        .map(|c| {
            if (' '..='}').contains(&c) && c != '=' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if key.is_empty() { "_".into() } else { key }
}

impl Carried {
    /// An ID3v2 tag holding everything: the original's own whole, and the
    /// rest added. `chapters` adds the markers as CHAP frames, where the
    /// format keeps no markers of its own.
    pub fn id3v2(&self, chapters: bool, skip: impl Fn(&Field) -> bool) -> Id3v2Tag {
        let mut tag = self.id3v2.clone().unwrap_or_default();
        tag.retain(|f| !matches!(f, Frame::Chapter(_) | Frame::TableOfContents(_)));
        let held = holds(|c| {
            if let Some(whole) = &self.id3v2 {
                read_id3v2(whole, c);
            }
        });
        let encoding = TextEncoding::UTF8;
        // A number and the one count there is for it go in one frame,
        // "3/12", where the tag has none yet.
        let mut paired: Vec<&Field> = Vec::new();
        for (number, total) in NUMBERS {
            let Some(id) = number.map_key(TagType::Id3v2) else {
                continue;
            };
            let of = |key: ItemKey| -> Vec<&Field> {
                self.fields
                    .iter()
                    .filter(|f| f.name == Name::Known(key) && !held(f) && !skip(f))
                    .collect()
            };
            let (numbers, totals) = (of(number), of(total));
            if let (Some(&first), [count]) = (numbers.first(), totals.as_slice())
                && digits(&first.value)
                && digits(&count.value)
                && !tag.iter().any(|f| f.id_str() == id)
                && let Ok(frame) = tags::new_id3v2_frame(
                    id,
                    &format!("{}/{}", first.value.trim(), count.value.trim()),
                    encoding,
                )
            {
                tag.insert(frame);
                paired.extend([first, *count]);
            }
        }
        for (key, values) in keyed(self, TagType::Id3v2, |f| {
            held(f) || skip(f) || paired.iter().any(|p| std::ptr::eq(*p, f))
        }) {
            // A comment or lyrics frame holds one text, a text frame
            // several apart by nulls, as ID3v2.4 keeps them.
            let separator = if key.starts_with("COMM") || key.starts_with("USLT") {
                "\n"
            } else {
                "\0"
            };
            // A frame of the same name already there takes these as more of
            // its values, rather than giving way to them.
            let mut all: Vec<String> = Vec::new();
            tag.retain(|f| match frame_key(f) {
                Some((k, v)) if k.eq_ignore_ascii_case(&key) => {
                    all.extend(v.split(separator).map(str::to_owned));
                    false
                }
                _ => true,
            });
            for value in values {
                if !all.contains(&value) {
                    all.push(value);
                }
            }
            if let Ok(frame) = tags::new_id3v2_frame(&key, &all.join(separator), encoding) {
                tag.insert(frame);
            }
        }
        for picture in &self.pictures {
            let there = tag.iter().any(|f| match f {
                Frame::Picture(p) => p.picture.data() == picture.data(),
                _ => false,
            });
            if !there {
                tag.insert(Frame::Picture(AttachedPictureFrame::new(
                    encoding,
                    picture.clone(),
                )));
            }
        }
        if chapters && !self.markers.is_empty() {
            let ms = |frames: usize| {
                u32::try_from(frames as u128 * 1000 / u128::from(self.rate.max(1)))
                    .unwrap_or(u32::MAX)
            };
            let mut ids = Vec::new();
            for (i, m) in self.markers.iter().enumerate() {
                let id = format!("chp{i}");
                let mut children = FrameList::new();
                if !m.label.is_empty() {
                    children.insert(Frame::Text(TextInformationFrame::new(
                        FrameId::new("TIT2").expect("an ID"),
                        encoding,
                        m.label.clone(),
                    )));
                }
                if !m.note.is_empty()
                    && let Ok(frame) = tags::new_id3v2_frame("COMM", &m.note, encoding)
                {
                    children.insert(frame);
                }
                let start = ms(m.frame);
                let end = ms(m.frame + m.length).max(start);
                tag.insert(Frame::Chapter(ChapterFrame::new(
                    id.clone(),
                    start..end,
                    u32::MAX..u32::MAX,
                    children,
                )));
                ids.push(std::borrow::Cow::Owned(id));
            }
            tag.insert(Frame::TableOfContents(ChapterTableOfContentsFrame::new(
                "toc",
                CtocFlags {
                    top_level: true,
                    ordered: true,
                },
                ids,
                FrameList::new(),
            )));
        }
        tag
    }

    /// The ID3v2 tag's bytes as a WAV's or an AIFF's chunk keeps them.
    pub fn id3v2_bytes(&self, tag: &Id3v2Tag) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        tag.dump_to(&mut bytes, WriteOptions::default())
            .map_err(|e| format!("the ID3 tag cannot be written: {e}"))?;
        Ok(bytes)
    }

    /// Vorbis comments holding everything but the pictures, which a FLAC
    /// keeps in blocks of their own and an Ogg file takes from lofty; with
    /// the markers as chapters.
    pub fn vorbis(&self) -> VorbisComments {
        let mut tag = self.vorbis.clone().unwrap_or_default();
        let whole = self.vorbis.is_some();
        let held = holds(|c| {
            if let Some(whole) = &self.vorbis {
                read_vorbis(whole, c);
            }
        });
        let old: Vec<String> = tag
            .items()
            .filter(|(k, _)| chapter_key(k).is_some())
            .map(|(k, _)| k.to_owned())
            .collect();
        for key in old {
            tag.remove(&key).for_each(drop);
        }
        if whole {
            // Pictures go with the rest, whatever the original kept them in.
            let _ = tag.remove_pictures();
        }
        for (key, values) in keyed(self, TagType::VorbisComments, held) {
            for value in values {
                tag.push(key.clone(), value);
            }
        }
        for (i, m) in self.markers.iter().enumerate() {
            let n = format!("CHAPTER{:03}", i + 1);
            tag.push(n.clone(), clock(m.frame, self.rate));
            if !m.label.is_empty() {
                tag.push(format!("{n}NAME"), m.label.clone());
            }
            if m.length > 0 {
                tag.push(format!("{n}END"), clock(m.frame + m.length, self.rate));
            }
            if !m.note.is_empty() {
                tag.push(format!("{n}NOTE"), m.note.clone());
            }
        }
        tag
    }

    /// Vorbis comments with the pictures too, for an Ogg file.
    pub fn vorbis_with_pictures(&self) -> VorbisComments {
        let mut tag = self.vorbis();
        for picture in &self.pictures {
            if let Ok(info) = PictureInformation::from_picture(picture) {
                let _ = tag.insert_picture(picture.clone(), Some(info));
            }
        }
        tag
    }

    /// An M4A's ilst, holding everything.
    pub fn ilst(&self) -> Ilst {
        let mut tag = self.ilst.clone().unwrap_or_default();
        let held = holds(|c| {
            if let Some(whole) = &self.ilst {
                read_ilst(whole, c);
            }
        });
        // trkn and disk hold one number and one count each; a number that
        // finds its place taken, or a track named as a vinyl side's "A1",
        // goes in a field of its own.
        let mut placed: Vec<&Field> = Vec::new();
        for field in self.fields.iter().filter(|f| !held(f)) {
            let Name::Known(key) = &field.name else {
                continue;
            };
            let Some(n) = field
                .value
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|&n| n > 0 && digits(&field.value))
                .map(u32::from)
            else {
                continue;
            };
            let taken = match key {
                ItemKey::TrackNumber => tag.track(),
                ItemKey::TrackTotal => tag.track_total(),
                ItemKey::DiscNumber => tag.disk(),
                ItemKey::DiscTotal => tag.disk_total(),
                _ => continue,
            };
            // A count set before a number reads as number 0, and the other
            // way round.
            if taken.is_some_and(|t| t > 0) {
                continue;
            }
            match key {
                ItemKey::TrackNumber => tag.set_track(n),
                ItemKey::TrackTotal => tag.set_track_total(n),
                ItemKey::DiscNumber => tag.set_disk(n),
                _ => tag.set_disk_total(n),
            }
            placed.push(field);
        }
        for (key, values) in keyed(self, TagType::Mp4Ilst, |f| {
            held(f) || placed.iter().any(|p| std::ptr::eq(*p, f))
        }) {
            let Ok(ident) = tags::mp4_ident(&key) else {
                continue;
            };
            let data = values.into_iter().map(AtomData::UTF8).collect();
            if let Some(atom) = Atom::from_collection(ident, data) {
                tag.insert(atom);
            }
        }
        let had: Vec<Vec<u8>> = tag
            .pictures()
            .into_iter()
            .flatten()
            .map(|p| p.data().to_vec())
            .collect();
        for picture in &self.pictures {
            // An M4A takes JPEG, PNG and BMP pictures.
            let fits = matches!(
                picture.mime_type(),
                Some(MimeType::Jpeg | MimeType::Png | MimeType::Bmp)
            );
            if fits && !had.iter().any(|d| d == picture.data()) {
                tag.insert_picture(picture.clone());
            }
        }
        tag
    }

    /// RIFF INFO entries for a new WAV: every field INFO has a place for.
    pub fn info(&self) -> Vec<([u8; 4], String)> {
        let mut info = Vec::new();
        for field in &self.fields {
            let id = match &field.name {
                Name::Known(key) => key.map_key(TagType::RiffInfo).map(str::to_owned),
                Name::Custom(name) if info_id(name) => Some(name.clone()),
                Name::Custom(_) => None,
            };
            let Some(id) = id.and_then(|id| <[u8; 4]>::try_from(id.as_bytes()).ok()) else {
                continue;
            };
            if !info.iter().any(|(i, v)| *i == id && *v == field.value) {
                info.push((id, field.value.clone()));
            }
        }
        info
    }

    /// AIFF's own text chunks for what they have a place for: the name,
    /// author and copyright, and the comments.
    pub fn aiff_text(&self) -> AiffTextChunks {
        if let Some(tag) = &self.aiff_text {
            return tag.clone();
        }
        let mut tag = AiffTextChunks::default();
        let one = |key: ItemKey| {
            let values: Vec<&str> = self
                .fields
                .iter()
                .filter(|f| f.name == Name::Known(key))
                .map(|f| f.value.as_str())
                .collect();
            (!values.is_empty()).then(|| values.join("; "))
        };
        tag.name = one(ItemKey::TrackTitle);
        tag.author = one(ItemKey::TrackArtist);
        tag.copyright = one(ItemKey::CopyrightMessage);
        let annotations: Vec<String> = self
            .fields
            .iter()
            .filter(|f| f.name == Name::Known(ItemKey::Comment))
            .map(|f| f.value.clone())
            .collect();
        tag.annotations = (!annotations.is_empty()).then_some(annotations);
        tag
    }

    /// Every field as a name and a value, a Vorbis comment's names for
    /// those lofty knows, as Matroska's SimpleTags take them.
    pub fn simple(&self) -> Vec<(String, String)> {
        keyed(self, TagType::VorbisComments, |_| false)
            .into_iter()
            .flat_map(|(key, values)| values.into_iter().map(move |v| (key.clone(), v)))
            .collect()
    }

    /// Every field as CAF's `info` chunk names it: its own names for those
    /// it has one for, values for one name together.
    pub fn caf_info(&self) -> Vec<(String, String)> {
        let mut info: Vec<(String, String)> = Vec::new();
        for field in &self.fields {
            let key = match &field.name {
                Name::Known(key) => CAF_KEYS
                    .iter()
                    .find(|(_, k)| k == key)
                    .map(|(name, _)| (*name).to_owned()),
                Name::Custom(_) => None,
            }
            .unwrap_or_else(|| field.plain_name().to_owned());
            match info.iter_mut().find(|(k, _)| *k == key) {
                Some((_, value)) => {
                    value.push_str("; ");
                    value.push_str(&field.value);
                }
                None => info.push((key, field.value.clone())),
            }
        }
        info
    }

    /// The chunks a new WAV gets besides its format and audio: before the
    /// audio, then after it. The original's own go as they were; anything
    /// they do not hold goes in chunks made for it.
    pub fn wav_chunks(&self) -> Result<Riff, String> {
        let mut out = self.riff.clone().unwrap_or_default();
        let held = |f: &Field| self.in_riff(f);
        // Each WAV chunk made from the fields it holds, where the original
        // had none.
        let mut made = Vec::new();
        if !out.has(b"bext")
            && let Some(bext) = self.bext()
        {
            out.before.insert(0, (*b"bext", bext));
        }
        if !out.has_list(b"INFO") {
            let info: Vec<_> =
                self.info()
                    .into_iter()
                    .filter(|(id, value)| {
                        !self.fields.iter().any(|f| {
                            held(f) && f.value == *value && f.key.as_bytes() == id.as_slice()
                        })
                    })
                    .collect();
            if !info.is_empty() {
                made.push((*b"LIST", wav::info_body(&info, &[])));
            }
        }
        let markers_held = out.has(b"cue ")
            || (self.riff.is_some() && self.fields.iter().any(|f| f.origin == Origin::Ixml));
        if !markers_held && !self.markers.is_empty() {
            let (cue, adtl) = wav::mark_bodies(&self.markers, &wav::StoredMarks::default());
            made.push((*b"cue ", cue));
            if let Some(adtl) = adtl {
                made.push((*b"LIST", adtl));
            }
        }
        if !out.has(b"iXML")
            && let Some(f) = self.fields.iter().find(|f| f.own_name() == IXML)
        {
            made.push((*b"iXML", f.value.clone().into_bytes()));
        }
        if !out.has(b"guan") {
            let fields: Vec<&Field> = self
                .fields
                .iter()
                .filter(|f| f.own_name().starts_with(GUANO) && f.own_name() != "GUANO|Version")
                .collect();
            if !fields.is_empty() {
                let version = self
                    .fields
                    .iter()
                    .find(|f| f.own_name() == "GUANO|Version")
                    .map_or("1.0", |f| f.value.as_str());
                let mut text = format!("GUANO|Version: {version}\n");
                for f in fields {
                    let key = &f.own_name()[GUANO.len()..];
                    text.push_str(&format!("{key}: {}\n", f.value.replace('\n', "\\n")));
                }
                made.push((*b"guan", text.into_bytes()));
            }
        }
        // What none of the above holds goes in an ID3 tag: into the
        // original's own, or a new one.
        let chunked = |f: &Field| {
            let name = f.own_name();
            held(f)
                || BWF.contains(&name)
                || [BWF_TIME_REFERENCE, BWF_CODING_HISTORY, BWF_UMID, IXML].contains(&name)
                || name.starts_with(GUANO)
                || matches!(&f.name, Name::Custom(n) if info_id(n))
        };
        let pictures_held = |p: &Picture| {
            self.riff.is_some()
                && self.id3v2.as_ref().is_some_and(|t| {
                    t.iter()
                        .any(|f| matches!(f, Frame::Picture(fp) if fp.picture.data() == p.data()))
                })
        };
        let extra = self.fields.iter().any(|f| !chunked(f))
            || self.pictures.iter().any(|p| !pictures_held(p));
        if extra {
            let tag = self.id3v2(false, chunked);
            let bytes = self.id3v2_bytes(&tag)?;
            let id3 = |id: &[u8; 4]| id == b"id3 " || id == b"ID3 ";
            let place = out
                .before
                .iter_mut()
                .chain(out.after.iter_mut())
                .find(|(id, _)| id3(id));
            match place {
                Some((_, body)) => *body = bytes,
                None => made.push((*b"id3 ", bytes)),
            }
        }
        out.after.extend(made);
        Ok(out)
    }

    /// A Broadcast WAV chunk from the BWF fields a converted file carries.
    fn bext(&self) -> Option<Vec<u8>> {
        let get = |name: &str| {
            self.fields
                .iter()
                .find(|f| f.own_name() == name)
                .map(|f| f.value.as_str())
        };
        if !BWF
            .iter()
            .chain(&[BWF_TIME_REFERENCE, BWF_CODING_HISTORY, BWF_UMID])
            .any(|n| get(n).is_some())
        {
            return None;
        }
        let mut raw = vec![0u8; 602];
        let fields = [0..256, 256..288, 288..320, 320..330, 330..338];
        for (name, range) in BWF.iter().zip(fields) {
            if let Some(value) = get(name) {
                let text = value.replace("\r\n", "\n").replace('\n', "\r\n");
                let bytes = &text.as_bytes()[..text.len().min(range.len())];
                raw[range.start..range.start + bytes.len()].copy_from_slice(bytes);
            }
        }
        let reference = get(BWF_TIME_REFERENCE).and_then(|v| v.parse::<u64>().ok());
        raw[338..346].copy_from_slice(&reference.unwrap_or(0).to_le_bytes());
        raw[346..348].copy_from_slice(&1u16.to_le_bytes());
        if let Some(umid) = get(BWF_UMID) {
            let bytes: Vec<u8> = (0..umid.len() / 2)
                .filter_map(|i| u8::from_str_radix(&umid[2 * i..2 * i + 2], 16).ok())
                .collect();
            let n = bytes.len().min(64);
            raw[348..348 + n].copy_from_slice(&bytes[..n]);
        }
        if let Some(history) = get(BWF_CODING_HISTORY) {
            raw.extend_from_slice(
                history
                    .replace("\r\n", "\n")
                    .replace('\n', "\r\n")
                    .as_bytes(),
            );
        }
        Some(raw)
    }

    /// The original's WAV chunks as the APPLICATION blocks `flac
    /// --keep-foreign-metadata` keeps them in, one chunk a block, with a
    /// format chunk for the FLAC's own audio. The two sizes they hold, the
    /// whole file's and the audio's, are filled in by [`riff_sizes`] once
    /// the audio is written.
    pub fn flac_foreign(&self, fmt: &[u8]) -> Option<Vec<Vec<u8>>> {
        let riff = self.riff.as_ref()?;
        let chunk = |id: &[u8; 4], body: &[u8]| {
            let mut b = b"riff".to_vec();
            b.extend_from_slice(id);
            b.extend_from_slice(&(body.len() as u32).to_le_bytes());
            b.extend_from_slice(body);
            if body.len() % 2 == 1 {
                b.push(0);
            }
            b
        };
        let mut blocks = vec![b"riffRIFF\0\0\0\0WAVE".to_vec(), chunk(b"fmt ", fmt)];
        blocks.extend(riff.before.iter().map(|(id, body)| chunk(id, body)));
        blocks.push(b"riffdata\0\0\0\0".to_vec());
        blocks.extend(riff.after.iter().map(|(id, body)| chunk(id, body)));
        Some(blocks)
    }
}

/// Fills in the sizes of the foreign metadata blocks `blocks` once the
/// audio, `data` bytes, is known: the whole WAV's, and the audio's.
pub fn riff_sizes(blocks: &mut [Vec<u8>], data: u64) {
    let chunks: u64 = blocks
        .iter()
        .skip(1)
        .map(|b| (b.len() - 4) as u64)
        .sum::<u64>()
        + data
        + data % 2;
    let whole = u32::try_from(4 + chunks).unwrap_or(u32::MAX);
    let data = u32::try_from(data).unwrap_or(u32::MAX);
    if let Some(first) = blocks.first_mut() {
        first[8..12].copy_from_slice(&whole.to_le_bytes());
    }
    if let Some(header) = blocks.iter_mut().find(|b| b.starts_with(b"riffdata")) {
        header[8..12].copy_from_slice(&data.to_le_bytes());
    }
}

/// The original's WAV chunks back from a FLAC's foreign metadata blocks:
/// all but the RIFF header, the format and the audio's own header.
pub fn riff_from_blocks(blocks: &[Vec<u8>]) -> Option<Riff> {
    let mut riff = Riff::default();
    let mut after = false;
    let mut any = false;
    for block in blocks {
        let Some(chunk) = block.strip_prefix(b"riff") else {
            continue;
        };
        any = true;
        let Some(id) = chunk.get(..4).and_then(|i| <[u8; 4]>::try_from(i).ok()) else {
            continue;
        };
        match &id {
            b"RIFF" | b"fmt " => {}
            b"data" => after = true,
            _ => {
                let size = chunk
                    .get(4..8)
                    .map_or(0, |s| u32::from_le_bytes(s.try_into().expect("four bytes")));
                let body = chunk.get(8..8 + size as usize).unwrap_or_default().to_vec();
                if after {
                    riff.after.push((id, body));
                } else {
                    riff.before.push((id, body));
                }
            }
        }
    }
    any.then_some(riff)
}

/// Whether `name` is a RIFF INFO entry's id: I and three capitals or
/// digits, as `ITCH` is.
fn info_id(name: &str) -> bool {
    name.len() == 4
        && name.starts_with('I')
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// What CAF's `info` chunk calls the fields lofty knows, as Apple's Core
/// Audio Format specification names them.
const CAF_KEYS: [(&str, ItemKey); 14] = [
    ("title", ItemKey::TrackTitle),
    ("artist", ItemKey::TrackArtist),
    ("album", ItemKey::AlbumTitle),
    ("recorded date", ItemKey::RecordingDate),
    ("year", ItemKey::Year),
    ("comments", ItemKey::Comment),
    ("copyright", ItemKey::CopyrightMessage),
    ("composer", ItemKey::Composer),
    ("lyricist", ItemKey::Lyricist),
    ("genre", ItemKey::Genre),
    ("track number", ItemKey::TrackNumber),
    ("encoding application", ItemKey::EncoderSoftware),
    ("tempo", ItemKey::Bpm),
    ("key signature", ItemKey::InitialKey),
];

fn caf_name(key: &str) -> Name {
    CAF_KEYS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, k)| *k)
        .or_else(|| item_key(TagType::VorbisComments, key))
        .map_or_else(|| Name::Custom(key.to_owned()), Name::Known)
}

/// Reads the new file back and checks it holds every field's value, every
/// picture where its format keeps pictures, and every marker, each where
/// it should be to within a millisecond.
pub fn check(
    path: &Path,
    opened: &Opened,
    format: Format,
    carried: &Carried,
) -> Result<(), String> {
    let back = read(path, opened)?;
    // trkn keeps a track's "03" as 3.
    let value = |f: &Field| match &f.name {
        Name::Known(k) if NUMBERS.iter().any(|(n, t)| n == k || t == k) && digits(&f.value) => {
            f.value.trim().trim_start_matches('0').to_owned()
        }
        _ => f.value.clone(),
    };
    let mut values: HashSet<String> = back.fields.iter().map(value).collect();
    if format == Format::Caf {
        // CAF keeps one value a name, several as one apart by "; ".
        let parts: Vec<String> = values
            .iter()
            .flat_map(|v| v.split("; ").map(str::to_owned))
            .collect();
        values.extend(parts);
    }
    if let Some(missing) = carried.fields.iter().find(|f| !values.contains(&value(f))) {
        return Err(format!("tag {} is missing", missing.own_name()));
    }
    if format.takes_pictures()
        && let Some(missing) = carried
            .pictures
            .iter()
            .find(|p| !back.pictures.iter().any(|b| b.data() == p.data()))
    {
        let kind = format!("{:?}", missing.pic_type()).to_lowercase();
        return Err(format!("{kind} picture is missing"));
    }
    let close = |a: usize, b: usize| a.abs_diff(b) as u64 * 1000 <= u64::from(carried.rate.max(1));
    let wanted = if format == Format::M4a {
        &carried.markers[..carried.markers.len().min(255)]
    } else {
        &carried.markers[..]
    };
    if back.markers.len() != wanted.len() {
        return Err(format!(
            "{} markers came back of {}",
            back.markers.len(),
            wanted.len()
        ));
    }
    for (b, m) in back.markers.iter().zip(wanted) {
        if !close(b.frame, m.frame) || b.label != m.label {
            return Err(format!("marker {:?} moved or lost its name", m.label));
        }
    }
    Ok(())
}
