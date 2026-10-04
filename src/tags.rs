//! Tags besides the chunks WAV files keep their own metadata in: ID3v2,
//! ID3v1 and APE, Vorbis comments, MP4's iTunes atoms and AIFF's text
//! chunks, read and written through lofty, and GUANO, the text bat
//! detectors put in WAV files. Every text field shows, and can be changed,
//! taken out or added; the rest of a tag, pictures and the like, goes back
//! as it was, and so does every text field left alone.

use std::fs::File;
use std::path::Path;

use lofty::TextEncoding;
use lofty::ape::{ApeItem, ApeTag};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, FileType};
use lofty::id3::v1::Id3v1Tag;
use lofty::id3::v2::{
    CommentFrame, ExtendedTextFrame, ExtendedUrlFrame, Frame, FrameId, Id3v2Tag, Id3v2Version,
    TextInformationFrame, UnsynchronizedTextFrame, UrlLinkFrame,
};
use lofty::iff::aiff::{AiffFile, AiffTextChunks};
use lofty::iff::wav::WavFile;
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OggPictureStorage, OpusFile, SpeexFile, VorbisFile};
use lofty::probe::Probe;
use lofty::tag::{ItemValue, TagExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Id3v2,
    Id3v1,
    Ape,
    Vorbis,
    Mp4,
    AiffText,
    Guano,
}

/// One tag in a file: its text fields, and what else it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub kind: Kind,
    /// ID3v2's minor version, 3 or 4, and GUANO's.
    pub version: Option<String>,
    pub fields: Vec<Field>,
    /// What the tag holds besides text, as it will stay.
    pub other: Vec<String>,
    /// Read, but not written back: ID3v2 in a FLAC file, which lofty and
    /// most players only read.
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// The name the file stores it under: `TIT2`, `TXXX:location`,
    /// `TITLE`, `©nam`, `Species Auto ID`.
    pub key: String,
    pub value: String,
    /// Which of the tag's text fields it was when read; `None` once added.
    pub from: Option<usize>,
}

impl Block {
    fn new(kind: Kind, fields: Vec<(String, String)>, other: Vec<String>) -> Self {
        Self {
            kind,
            version: None,
            fields: fields
                .into_iter()
                .enumerate()
                .map(|(i, (key, value))| Field {
                    key,
                    value,
                    from: Some(i),
                })
                .collect(),
            other,
            read_only: false,
        }
    }

    pub fn title(&self) -> String {
        match (self.kind, &self.version) {
            (Kind::Id3v2, Some(v)) => format!("ID3v2.{v}"),
            (Kind::Id3v2, None) => "ID3v2".into(),
            (Kind::Id3v1, _) => "ID3v1".into(),
            (Kind::Ape, _) => "APE".into(),
            (Kind::Vorbis, _) => "Vorbis comments".into(),
            (Kind::Mp4, _) => "MP4 (iTunes)".into(),
            (Kind::AiffText, _) => "AIFF text".into(),
            (Kind::Guano, _) => "GUANO".into(),
        }
    }

    /// The text and the rest, compared without where each field came from:
    /// what reading the tag back has to give.
    fn content(&self) -> (Vec<(&str, &str)>, &[String]) {
        let fields = self
            .fields
            .iter()
            .map(|f| (f.key.as_str(), f.value.as_str()))
            .collect();
        (fields, &self.other)
    }
}

impl Kind {
    /// Fields a tag of this kind can take that it may not have yet, for
    /// adding.
    pub fn suggestions(self) -> &'static [&'static str] {
        match self {
            Kind::Id3v2 => &[
                "TIT2", "TPE1", "TALB", "TPE2", "COMM", "TCON", "TDRC", "TRCK", "TCOM", "TCOP",
                "TPUB", "TIT3", "TXXX:",
            ],
            Kind::Id3v1 => &["Title", "Artist", "Album", "Year", "Comment", "Track"],
            Kind::Ape => &["Title", "Artist", "Album", "Year", "Comment", "Genre", ""],
            Kind::Vorbis => &[
                "TITLE",
                "ARTIST",
                "ALBUM",
                "DATE",
                "COMMENT",
                "DESCRIPTION",
                "GENRE",
                "LOCATION",
                "CONTACT",
                "COPYRIGHT",
                "LICENSE",
                "ORGANIZATION",
                "",
            ],
            Kind::Mp4 => &[
                "©nam",
                "©ART",
                "©alb",
                "aART",
                "©cmt",
                "desc",
                "©day",
                "©gen",
                "©wrt",
                "cprt",
                "----:com.apple.iTunes:",
            ],
            Kind::AiffText => &["NAME", "AUTH", "(c) ", "ANNO", "COMT"],
            Kind::Guano => &[
                "Timestamp",
                "Loc Position",
                "Loc Elevation",
                "Species Manual ID",
                "Species Auto ID",
                "Make",
                "Model",
                "Serial",
                "Firmware Version",
                "Note",
                "Samplerate",
                "Length",
                "TE",
                "Temperature Ext",
                "Humidity",
                "",
            ],
        }
    }

    /// What `key` is called, for its row.
    pub fn label(self, key: &str) -> String {
        let named = match self {
            Kind::Id3v2 => id3v2_name(key),
            Kind::Mp4 => mp4_name(key),
            Kind::AiffText => match key {
                "NAME" => Some("Name"),
                "AUTH" => Some("Author"),
                "(c) " => Some("Copyright"),
                "ANNO" => Some("Annotation"),
                "COMT" => Some("Comment"),
                _ => None,
            },
            Kind::Vorbis => return title_case(key),
            Kind::Id3v1 | Kind::Ape | Kind::Guano => None,
        };
        match (named, self, key.split_once(':')) {
            (Some(name), ..) => name.into(),
            (None, Kind::Id3v2, Some((_, description))) => description.into(),
            (None, Kind::Mp4, Some(_)) => key.rsplit(':').next().unwrap_or(key).into(),
            _ => key.into(),
        }
    }

    /// Whether `key` can go in a tag of this kind, and if not, why.
    pub fn check_key(self, key: &str) -> Result<(), String> {
        let bad = |why: &str| Err(format!("{key:?} cannot be a {} field: {why}", self.name()));
        match self {
            Kind::Id3v2 => match key.split_once(':') {
                Some(("TXXX" | "WXXX" | "COMM" | "USLT", _)) => Ok(()),
                Some(_) => bad("only TXXX, WXXX, COMM and USLT take a name after a colon"),
                None if key.len() == 4
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                    && (key.starts_with(['T', 'W']) || key == "COMM" || key == "USLT")
                    && key != "TXXX"
                    && key != "WXXX" =>
                {
                    Ok(())
                }
                None => bad("a text frame is four capitals starting with T, a link with W"),
            },
            Kind::Vorbis => {
                if key.is_empty()
                    || key.contains('=')
                    || !key.bytes().all(|b| (0x20..=0x7d).contains(&b))
                {
                    bad("the name needs plain letters, and no =")
                } else {
                    Ok(())
                }
            }
            Kind::Ape => {
                if key.len() < 2 || key.len() > 255 || !key.is_ascii() {
                    bad("the name needs 2 to 255 plain characters")
                } else {
                    Ok(())
                }
            }
            Kind::Mp4 => match key.strip_prefix("----:") {
                Some(rest)
                    if rest
                        .split_once(':')
                        .is_some_and(|(m, n)| !m.is_empty() && !n.is_empty()) =>
                {
                    Ok(())
                }
                Some(_) => bad("a custom field is ----:mean:name"),
                None if fourcc(key).is_some() => Ok(()),
                None => bad("an atom is four characters"),
            },
            Kind::Id3v1 => {
                if Self::Id3v1.suggestions().contains(&key) {
                    Ok(())
                } else {
                    bad("ID3v1 has only Title, Artist, Album, Year, Comment and Track")
                }
            }
            Kind::AiffText => {
                if Self::AiffText.suggestions().contains(&key) {
                    Ok(())
                } else {
                    bad("AIFF has only NAME, AUTH, (c), ANNO and COMT")
                }
            }
            Kind::Guano => {
                if key.is_empty()
                    || key.contains(':')
                    || key.contains('\n')
                    || key == "GUANO|Version"
                {
                    bad("the name cannot have a colon in it")
                } else {
                    Ok(())
                }
            }
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Id3v2 => "ID3v2",
            Kind::Id3v1 => "ID3v1",
            Kind::Ape => "APE",
            Kind::Vorbis => "Vorbis comment",
            Kind::Mp4 => "MP4",
            Kind::AiffText => "AIFF text",
            Kind::Guano => "GUANO",
        }
    }

    /// Whether more than one field can have the same name.
    pub fn repeats(self, key: &str) -> bool {
        match self {
            Kind::Vorbis => true,
            Kind::AiffText => matches!(key, "ANNO" | "COMT"),
            _ => false,
        }
    }
}

fn title_case(key: &str) -> String {
    let lower = key.to_lowercase().replace('_', " ");
    let mut chars = lower.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

fn id3v2_name(key: &str) -> Option<&'static str> {
    let (id, rest) = key.split_once(':').map_or((key, ""), |(i, r)| (i, r));
    Some(match id {
        "TIT1" => "Grouping",
        "TIT2" => "Title",
        "TIT3" => "Subtitle",
        "TPE1" => "Artist",
        "TPE2" => "Album artist",
        "TPE3" => "Conductor",
        "TPE4" => "Remixed by",
        "TALB" => "Album",
        "TRCK" => "Track",
        "TPOS" => "Disc",
        "TYER" => "Year",
        "TDAT" => "Date",
        "TIME" => "Time",
        "TDRC" => "Recorded",
        "TDRL" => "Released",
        "TDOR" | "TORY" => "Originally released",
        "TCON" => "Genre",
        "TCOM" => "Composer",
        "TEXT" => "Lyricist",
        "TCOP" => "Copyright",
        "TPUB" => "Publisher",
        "TENC" => "Encoded by",
        "TSSE" => "Encoder",
        "TBPM" => "BPM",
        "TKEY" => "Key",
        "TLAN" => "Language",
        "TLEN" => "Length",
        "TMED" => "Media",
        "TMOO" => "Mood",
        "TOAL" => "Original album",
        "TOPE" => "Original artist",
        "TOFN" => "Original file name",
        "TOWN" => "Owner",
        "TSRC" => "ISRC",
        "TSOA" => "Album for sorting",
        "TSOP" => "Artist for sorting",
        "TSOT" => "Title for sorting",
        "COMM" if rest.is_empty() => "Comment",
        "USLT" if rest.is_empty() => "Lyrics",
        "WOAR" => "Artist's page",
        "WOAS" => "Source's page",
        "WOAF" => "File's page",
        "WCOP" => "Copyright page",
        "WPUB" => "Publisher's page",
        "WCOM" => "Where to buy",
        _ => return None,
    })
}

fn mp4_name(key: &str) -> Option<&'static str> {
    Some(match key {
        "©nam" => "Title",
        "©ART" => "Artist",
        "aART" => "Album artist",
        "©alb" => "Album",
        "©cmt" => "Comment",
        "desc" => "Description",
        "ldes" => "Long description",
        "©day" => "Date",
        "©gen" => "Genre",
        "©wrt" => "Composer",
        "©too" => "Encoder",
        "cprt" => "Copyright",
        "©lyr" => "Lyrics",
        "©grp" => "Grouping",
        "©pub" => "Publisher",
        _ => return None,
    })
}

/// A four-character atom name, where © is its one byte in Latin-1.
fn fourcc(key: &str) -> Option<[u8; 4]> {
    let bytes: Vec<u8> = key
        .chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect::<Option<_>>()?;
    bytes.try_into().ok()
}

pub(crate) fn fourcc_key(code: &[u8; 4]) -> String {
    code.iter().map(|&b| char::from(b)).collect()
}

/// The tags of a file other than WAV, or `None` for a kind of file lofty
/// does not read, whose tags then show only as read.
pub fn read(path: &Path) -> Option<Vec<Block>> {
    let file_type = Probe::open(path)
        .ok()?
        .guess_file_type()
        .ok()?
        .file_type()?;
    let mut file = File::open(path).ok()?;
    let options = ParseOptions::new().read_properties(false);
    let blocks = match file_type {
        FileType::Mpeg => {
            let f = MpegFile::read_from(&mut file, options).ok()?;
            let mut blocks = vec![id3v2_block(f.id3v2())];
            blocks.extend(f.id3v1().map(id3v1_block));
            blocks.extend(f.ape().map(ape_block));
            blocks
        }
        FileType::Aac => {
            let f = lofty::aac::AacFile::read_from(&mut file, options).ok()?;
            let mut blocks = vec![id3v2_block(f.id3v2())];
            blocks.extend(f.id3v1().map(id3v1_block));
            blocks
        }
        FileType::Flac => {
            let f = lofty::flac::FlacFile::read_from(&mut file, options).ok()?;
            let mut blocks = vec![vorbis_block(f.vorbis_comments())];
            blocks.extend(f.id3v2().map(|t| Block {
                read_only: true,
                ..id3v2_block(Some(t))
            }));
            blocks
        }
        FileType::Vorbis => vec![vorbis_block(Some(
            VorbisFile::read_from(&mut file, options)
                .ok()?
                .vorbis_comments(),
        ))],
        FileType::Opus => vec![vorbis_block(Some(
            OpusFile::read_from(&mut file, options)
                .ok()?
                .vorbis_comments(),
        ))],
        FileType::Speex => vec![vorbis_block(Some(
            SpeexFile::read_from(&mut file, options)
                .ok()?
                .vorbis_comments(),
        ))],
        FileType::Mp4 => vec![mp4_block(
            Mp4File::read_from(&mut file, options).ok()?.ilst(),
        )],
        FileType::Aiff => {
            let f = AiffFile::read_from(&mut file, options).ok()?;
            let mut blocks = vec![id3v2_block(f.id3v2())];
            blocks.extend(f.text_chunks().map(aiff_block));
            blocks
        }
        _ => return None,
    };
    Some(blocks)
}

/// The ID3v2 tag in the `id3 ` chunk of the WAV file at `path`.
pub fn wav_id3(path: &Path) -> Result<Option<Block>, String> {
    let tag = wav_id3v2(path)?;
    Ok(tag.as_ref().map(|t| id3v2_block(Some(t))))
}

pub(crate) fn wav_id3v2(path: &Path) -> Result<Option<Id3v2Tag>, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
    let wav = WavFile::read_from(&mut file, ParseOptions::new().read_properties(false))
        .map_err(|e| format!("its ID3 tag cannot be read: {e}"))?;
    Ok(wav.id3v2().cloned())
}

/// The ID3v2 tag `saved` says the WAV file at `path` holds, as `edited`,
/// ready to go in its `id3 ` chunk; empty once nothing is left in it.
pub fn wav_id3_bytes(path: &Path, saved: &Block, edited: &Block) -> Result<Vec<u8>, String> {
    let mut tag = wav_id3v2(path)?.unwrap_or_default();
    let v3 = apply_id3v2(&mut tag, saved, edited)?;
    if tag.is_empty() {
        return Ok(Vec::new());
    }
    let mut bytes = Vec::new();
    tag.dump_to(&mut bytes, id3_options(v3))
        .map_err(|e| format!("the ID3 tag cannot be written: {e}"))?;
    Ok(bytes)
}

/// Writes each block of `changed`, as read and as edited, into the file at
/// `path`, which must still hold what was read.
pub fn write(path: &Path, changed: &[(Block, Block)]) -> Result<(), String> {
    let file_type = Probe::open(path)
        .and_then(|p| p.guess_file_type().map_err(Into::into))
        .ok()
        .and_then(|p| p.file_type())
        .ok_or("lofty cannot tell what kind of file it is")?;
    let failed = |e: lofty::error::FileParseError| format!("its tags cannot be read: {e}");
    let written = |e: lofty::error::FileEncodingError| format!("its tags cannot be written: {e}");
    let options = || ParseOptions::new().read_properties(false);
    for (saved, edited) in changed {
        if saved.read_only {
            return Err(format!("{} is only read, never written", saved.title()));
        }
        let mut file = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
        match saved.kind {
            Kind::Id3v2 => {
                let mut tag = match file_type {
                    FileType::Mpeg => MpegFile::read_from(&mut file, options())
                        .map_err(failed)?
                        .id3v2()
                        .cloned(),
                    FileType::Aac => lofty::aac::AacFile::read_from(&mut file, options())
                        .map_err(failed)?
                        .id3v2()
                        .cloned(),
                    FileType::Aiff => AiffFile::read_from(&mut file, options())
                        .map_err(failed)?
                        .id3v2()
                        .cloned(),
                    _ => return Err("this kind of file takes no ID3v2 tag".into()),
                }
                .unwrap_or_default();
                let v3 = apply_id3v2(&mut tag, saved, edited)?;
                tag.save_to_path(path, id3_options(v3)).map_err(written)?;
            }
            Kind::Id3v1 => {
                let mut tag = match file_type {
                    FileType::Mpeg => MpegFile::read_from(&mut file, options())
                        .map_err(failed)?
                        .id3v1()
                        .cloned(),
                    FileType::Aac => lofty::aac::AacFile::read_from(&mut file, options())
                        .map_err(failed)?
                        .id3v1()
                        .cloned(),
                    _ => None,
                }
                .ok_or("its ID3v1 tag is gone")?;
                apply_id3v1(&mut tag, saved, edited)?;
                tag.save_to_path(path, WriteOptions::default())
                    .map_err(written)?;
            }
            Kind::Ape => {
                let mut tag = MpegFile::read_from(&mut file, options())
                    .map_err(failed)?
                    .ape()
                    .cloned()
                    .ok_or("its APE tag is gone")?;
                apply_ape(&mut tag, saved, edited)?;
                tag.save_to_path(path, WriteOptions::default())
                    .map_err(written)?;
            }
            Kind::Vorbis => {
                let mut tag = match file_type {
                    FileType::Flac => lofty::flac::FlacFile::read_from(&mut file, options())
                        .map_err(failed)?
                        .vorbis_comments()
                        .cloned()
                        .unwrap_or_default(),
                    FileType::Vorbis => VorbisFile::read_from(&mut file, options())
                        .map_err(failed)?
                        .vorbis_comments()
                        .clone(),
                    FileType::Opus => OpusFile::read_from(&mut file, options())
                        .map_err(failed)?
                        .vorbis_comments()
                        .clone(),
                    FileType::Speex => SpeexFile::read_from(&mut file, options())
                        .map_err(failed)?
                        .vorbis_comments()
                        .clone(),
                    _ => return Err("this kind of file takes no Vorbis comments".into()),
                };
                apply_vorbis(&mut tag, saved, edited)?;
                tag.save_to_path(path, WriteOptions::default())
                    .map_err(written)?;
            }
            Kind::Mp4 => {
                let mut tag = Mp4File::read_from(&mut file, options())
                    .map_err(failed)?
                    .ilst()
                    .cloned()
                    .unwrap_or_default();
                apply_mp4(&mut tag, saved, edited)?;
                tag.save_to_path(path, WriteOptions::default())
                    .map_err(written)?;
            }
            Kind::AiffText => {
                let mut tag = AiffFile::read_from(&mut file, options())
                    .map_err(failed)?
                    .text_chunks()
                    .cloned()
                    .unwrap_or_default();
                apply_aiff(&mut tag, saved, edited)?;
                tag.save_to_path(path, WriteOptions::default())
                    .map_err(written)?;
            }
            Kind::Guano => return Err("GUANO is only kept in WAV files".into()),
        }
    }
    Ok(())
}

/// Whether the tags read back from `path` are those `changed` meant, each
/// block compared by its text and the rest, not by where fields came from.
pub fn came_out(path: &Path, changed: &[(Block, Block)]) -> Result<(), String> {
    let now = read(path).ok_or("its tags cannot be read back")?;
    for (_, edited) in changed {
        if !matches(now.iter().find(|b| b.kind == edited.kind), edited) {
            return Err(format!("{} came out different", edited.title()));
        }
    }
    Ok(())
}

/// Whether `back`, a tag read back after a save, holds what `edited` did:
/// the same text and the rest, or nothing at all where nothing was left.
pub fn matches(back: Option<&Block>, edited: &Block) -> bool {
    match back {
        Some(back) => back.content() == edited.content(),
        None => edited.fields.is_empty() && edited.other.is_empty(),
    }
}

fn id3_options(v3: bool) -> WriteOptions {
    WriteOptions::default().use_id3v23(v3)
}

/// The text of an ID3v2 frame that has text, under the name the block
/// gives it.
fn id3v2_text(frame: &Frame<'_>) -> Option<(String, String)> {
    let named = |id: &str, description: &str| {
        if description.is_empty() {
            id.to_owned()
        } else {
            format!("{id}:{description}")
        }
    };
    // ID3v2.4 keeps several values in one frame, apart by nulls.
    let values = |text: &str| text.replace('\0', "; ");
    Some(match frame {
        Frame::Text(t) => (frame.id_str().to_owned(), values(&t.value)),
        Frame::UserText(t) => (format!("TXXX:{}", t.description), values(&t.content)),
        Frame::Comment(c) => (named("COMM", &c.description), c.content.to_string()),
        Frame::UnsynchronizedText(u) => (named("USLT", &u.description), u.content.to_string()),
        Frame::Url(u) => (frame.id_str().to_owned(), u.url().to_owned()),
        Frame::UserUrl(u) => (format!("WXXX:{}", u.description), u.content.to_string()),
        _ => return None,
    })
}

fn id3v2_block(tag: Option<&Id3v2Tag>) -> Block {
    let Some(tag) = tag else {
        return Block {
            version: Some("4".into()),
            ..Block::new(Kind::Id3v2, Vec::new(), Vec::new())
        };
    };
    let mut fields = Vec::new();
    let mut other = Vec::new();
    for frame in tag.iter() {
        match id3v2_text(frame) {
            Some(field) => fields.push(field),
            None => other.push(match frame {
                Frame::Picture(p) => picture(&p.picture),
                _ => format!("{} frame", frame.id_str()),
            }),
        }
    }
    let version = match tag.original_version() {
        Id3v2Version::V2 => "2",
        Id3v2Version::V3 => "3",
        Id3v2Version::V4 => "4",
    };
    Block {
        version: Some(version.into()),
        ..Block::new(Kind::Id3v2, fields, other)
    }
}

/// Makes `tag`, which holds what `saved` shows, hold what `edited` does.
/// Returns whether to write it as ID3v2.3, as it was.
fn apply_id3v2(tag: &mut Id3v2Tag, saved: &Block, edited: &Block) -> Result<bool, String> {
    if id3v2_block(Some(&*tag)).content() != saved.content() && !tag.is_empty() {
        return Err(changed_since());
    }
    let v3 = saved.version.as_deref() == Some("3");
    let encoding = if v3 {
        TextEncoding::UTF16
    } else {
        TextEncoding::UTF8
    };
    let mut at = 0;
    tag.retain_mut(|frame| {
        if id3v2_text(frame).is_none() {
            return true;
        }
        let index = at;
        at += 1;
        let Some(field) = edited.fields.iter().find(|f| f.from == Some(index)) else {
            return false;
        };
        if saved
            .fields
            .get(index)
            .is_some_and(|s| s.value != field.value)
        {
            set_id3v2_text(frame, &field.value, encoding);
        }
        true
    });
    for field in edited.fields.iter().filter(|f| f.from.is_none()) {
        Kind::Id3v2.check_key(&field.key)?;
        tag.insert(new_id3v2_frame(&field.key, &field.value, encoding)?);
    }
    Ok(v3)
}

fn set_id3v2_text(frame: &mut Frame<'_>, value: &str, encoding: TextEncoding) {
    let value = value.to_owned();
    match frame {
        Frame::Text(t) => {
            t.encoding = encoding;
            t.value = value.into();
        }
        Frame::UserText(t) => {
            t.encoding = encoding;
            t.content = value.into();
        }
        Frame::Comment(c) => {
            c.encoding = encoding;
            c.content = value.into();
        }
        Frame::UnsynchronizedText(u) => {
            u.encoding = encoding;
            u.content = value.into();
        }
        Frame::Url(u) => {
            u.set_url(value);
        }
        Frame::UserUrl(u) => {
            u.encoding = encoding;
            u.content = value.into();
        }
        _ => {}
    }
}

pub(crate) fn new_id3v2_frame(
    key: &str,
    value: &str,
    encoding: TextEncoding,
) -> Result<Frame<'static>, String> {
    let value = value.to_owned();
    let (id, description) = key.split_once(':').map_or((key, ""), |(i, d)| (i, d));
    let description = description.to_owned();
    let frame_id = || FrameId::new(id.to_owned()).map_err(|e| format!("{key:?}: {e}"));
    Ok(match id {
        "TXXX" => Frame::UserText(ExtendedTextFrame::new(encoding, description, value)),
        "WXXX" => Frame::UserUrl(ExtendedUrlFrame::new(encoding, description, value)),
        "COMM" => Frame::Comment(CommentFrame::new(encoding, *b"XXX", description, value)),
        "USLT" => Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
            encoding,
            *b"XXX",
            description,
            value,
        )),
        _ if id.starts_with('W') => Frame::Url(UrlLinkFrame::new(frame_id()?, value)),
        _ => Frame::Text(TextInformationFrame::new(frame_id()?, encoding, value)),
    })
}

fn id3v1_block(tag: &Id3v1Tag) -> Block {
    let text = |v: &Option<String>| v.clone().unwrap_or_default();
    let fields = vec![
        ("Title".into(), text(&tag.title)),
        ("Artist".into(), text(&tag.artist)),
        ("Album".into(), text(&tag.album)),
        (
            "Year".into(),
            tag.year.map(|y| y.to_string()).unwrap_or_default(),
        ),
        ("Comment".into(), text(&tag.comment)),
        (
            "Track".into(),
            tag.track_number.map(|t| t.to_string()).unwrap_or_default(),
        ),
    ];
    let other = tag
        .genre
        .map(|g| format!("genre {g}"))
        .into_iter()
        .collect();
    Block::new(Kind::Id3v1, fields, other)
}

fn apply_id3v1(tag: &mut Id3v1Tag, saved: &Block, edited: &Block) -> Result<(), String> {
    if id3v1_block(tag).content() != saved.content() {
        return Err(changed_since());
    }
    let get = |key: &str| {
        edited
            .fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.value.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let fits = |key: &str, most: usize| match get(key) {
        Some(v) if v.chars().count() > most => {
            Err(format!("ID3v1 keeps at most {most} characters for {key}"))
        }
        v => Ok(v),
    };
    let number = |key: &str| -> Result<Option<String>, String> {
        get(key)
            .map(|v| {
                v.parse::<u16>()
                    .map(|_| v.clone())
                    .map_err(|_| format!("ID3v1's {key} has to be a number"))
            })
            .transpose()
    };
    tag.title = fits("Title", 30)?;
    tag.artist = fits("Artist", 30)?;
    tag.album = fits("Album", 30)?;
    tag.comment = fits("Comment", if get("Track").is_some() { 28 } else { 30 })?;
    tag.year = number("Year")?.and_then(|y| y.parse().ok());
    tag.track_number = match number("Track")? {
        Some(t) => Some(
            t.parse::<u8>()
                .map_err(|_| "ID3v1's Track goes up to 255".to_owned())?,
        ),
        None => None,
    };
    Ok(())
}

fn ape_block(tag: &ApeTag) -> Block {
    let mut fields = Vec::new();
    let mut other = Vec::new();
    for item in tag {
        match item.value() {
            ItemValue::Text(t) | ItemValue::Locator(t) => {
                fields.push((item.key().to_owned(), t.clone()))
            }
            ItemValue::Binary(b) => other.push(format!("{}, {} bytes", item.key(), b.len())),
        }
    }
    Block::new(Kind::Ape, fields, other)
}

fn apply_ape(tag: &mut ApeTag, saved: &Block, edited: &Block) -> Result<(), String> {
    if ape_block(tag).content() != saved.content() {
        return Err(changed_since());
    }
    for (i, field) in saved.fields.iter().enumerate() {
        match edited.fields.iter().find(|f| f.from == Some(i)) {
            None => tag.remove(&field.key),
            Some(f) if f.value != field.value => {
                let item = ApeItem::new(field.key.clone(), ItemValue::Text(f.value.clone()))
                    .map_err(|e| e.to_string())?;
                tag.insert(item);
            }
            Some(_) => {}
        }
    }
    for field in edited.fields.iter().filter(|f| f.from.is_none()) {
        Kind::Ape.check_key(&field.key)?;
        let item = ApeItem::new(field.key.clone(), ItemValue::Text(field.value.clone()))
            .map_err(|e| e.to_string())?;
        tag.insert(item);
    }
    Ok(())
}

fn vorbis_block(tag: Option<&VorbisComments>) -> Block {
    let fields = tag.map_or_else(Vec::new, |t| {
        t.items()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    });
    let other = tag
        .map(|t| t.pictures().iter().map(|(p, _)| picture(p)).collect())
        .unwrap_or_default();
    Block::new(Kind::Vorbis, fields, other)
}

fn picture(p: &lofty::picture::Picture) -> String {
    format!(
        "{} picture, {} KB",
        format!("{:?}", p.pic_type()).to_lowercase(),
        p.data().len().div_ceil(1024)
    )
}

fn apply_vorbis(tag: &mut VorbisComments, saved: &Block, edited: &Block) -> Result<(), String> {
    if vorbis_block(Some(tag)).content() != saved.content() {
        return Err(changed_since());
    }
    for field in edited.fields.iter().filter(|f| f.from.is_none()) {
        Kind::Vorbis.check_key(&field.key)?;
    }
    let items: Vec<(String, String)> = tag.take_items().collect();
    for (i, (key, value)) in items.into_iter().enumerate() {
        if let Some(f) = edited.fields.iter().find(|f| f.from == Some(i)) {
            tag.push(
                key,
                if f.value == value {
                    value
                } else {
                    f.value.clone()
                },
            );
        }
    }
    for field in edited.fields.iter().filter(|f| f.from.is_none()) {
        tag.push(field.key.clone(), field.value.clone());
    }
    Ok(())
}

fn mp4_text(atom: &Atom<'_>) -> Option<(String, String)> {
    let key = match atom.ident() {
        AtomIdent::Fourcc(code) => fourcc_key(code),
        AtomIdent::Freeform { mean, name } => format!("----:{mean}:{name}"),
    };
    let mut texts = Vec::new();
    for data in atom.data() {
        match data {
            AtomData::UTF8(t) | AtomData::UTF16(t) => texts.push(t.clone()),
            _ => return None,
        }
    }
    (!texts.is_empty()).then(|| (key, texts.join("; ")))
}

fn mp4_block(tag: Option<&Ilst>) -> Block {
    let mut fields = Vec::new();
    let mut other = Vec::new();
    for atom in tag.into_iter().flatten() {
        match mp4_text(atom) {
            Some(field) => fields.push(field),
            None => other.push(match atom.ident() {
                AtomIdent::Fourcc(code) => match &fourcc_key(code)[..] {
                    "covr" => "cover picture".to_owned(),
                    "trkn" => "track number".to_owned(),
                    "disk" => "disc number".to_owned(),
                    key => format!("{key} atom"),
                },
                AtomIdent::Freeform { name, .. } => format!("{name} atom"),
            }),
        }
    }
    Block::new(Kind::Mp4, fields, other)
}

pub(crate) fn mp4_ident(key: &str) -> Result<AtomIdent<'static>, String> {
    match key.strip_prefix("----:").and_then(|r| r.split_once(':')) {
        Some((mean, name)) => Ok(AtomIdent::Freeform {
            mean: mean.to_owned().into(),
            name: name.to_owned().into(),
        }),
        None => fourcc(key)
            .map(AtomIdent::Fourcc)
            .ok_or_else(|| format!("{key:?} is not an atom name")),
    }
}

fn apply_mp4(tag: &mut Ilst, saved: &Block, edited: &Block) -> Result<(), String> {
    if mp4_block(Some(tag)).content() != saved.content() {
        return Err(changed_since());
    }
    for (i, field) in saved.fields.iter().enumerate() {
        match edited.fields.iter().find(|f| f.from == Some(i)) {
            None => {
                let ident = mp4_ident(&field.key)?;
                tag.remove(&ident).for_each(drop);
            }
            Some(f) if f.value != field.value => {
                tag.replace_atom(Atom::new(
                    mp4_ident(&field.key)?,
                    AtomData::UTF8(f.value.clone()),
                ));
            }
            Some(_) => {}
        }
    }
    for field in edited.fields.iter().filter(|f| f.from.is_none()) {
        Kind::Mp4.check_key(&field.key)?;
        tag.insert(Atom::new(
            mp4_ident(&field.key)?,
            AtomData::UTF8(field.value.clone()),
        ));
    }
    Ok(())
}

fn aiff_block(tag: &AiffTextChunks) -> Block {
    let mut fields = Vec::new();
    for (key, value) in [
        ("NAME", &tag.name),
        ("AUTH", &tag.author),
        ("(c) ", &tag.copyright),
    ] {
        if let Some(v) = value {
            fields.push((key.to_owned(), v.clone()));
        }
    }
    for a in tag.annotations.iter().flatten() {
        fields.push(("ANNO".to_owned(), a.clone()));
    }
    for c in tag.comments.iter().flatten() {
        fields.push(("COMT".to_owned(), c.text.clone()));
    }
    Block::new(Kind::AiffText, fields, Vec::new())
}

fn apply_aiff(tag: &mut AiffTextChunks, saved: &Block, edited: &Block) -> Result<(), String> {
    if aiff_block(tag).content() != saved.content() {
        return Err(changed_since());
    }
    for field in &edited.fields {
        Kind::AiffText.check_key(&field.key)?;
    }
    let one = |key: &str| {
        edited
            .fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.value.clone())
    };
    tag.name = one("NAME");
    tag.author = one("AUTH");
    tag.copyright = one("(c) ");
    let annotations: Vec<String> = edited
        .fields
        .iter()
        .filter(|f| f.key == "ANNO")
        .map(|f| f.value.clone())
        .collect();
    tag.annotations = (!annotations.is_empty()).then_some(annotations);
    // Each comment keeps the time and marker it came with.
    let old = tag.comments.take().unwrap_or_default();
    let comments: Vec<_> = edited
        .fields
        .iter()
        .filter(|f| f.key == "COMT")
        .map(|f| {
            let came = f
                .from
                .and_then(|i| saved.fields.get(i))
                .filter(|s| s.key == "COMT")
                .and_then(|s| old.iter().find(|c| c.text == s.value));
            lofty::iff::aiff::Comment {
                timestamp: came.map_or(0, |c| c.timestamp),
                marker_id: came.map_or(0, |c| c.marker_id),
                text: f.value.clone(),
            }
        })
        .collect();
    tag.comments = (!comments.is_empty()).then_some(comments);
    Ok(())
}

fn changed_since() -> String {
    "its tags changed since it was opened; open it again to see them".into()
}

/// GUANO's text, as a block: a field a line, `Key: Value`, after the line
/// with the version.
pub fn guano(text: &str) -> Block {
    let mut version = None;
    let mut fields = Vec::new();
    for line in text.trim_end_matches(['\0', ' ', '\n', '\r']).lines() {
        let line = line.trim_end_matches('\r');
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim_start().replace("\\n", "\n");
        if key.trim() == "GUANO|Version" {
            version = Some(value);
        } else {
            fields.push((key.trim().to_owned(), value));
        }
    }
    Block {
        version: Some(version.unwrap_or_else(|| "1.0".into())),
        ..Block::new(Kind::Guano, fields, Vec::new())
    }
}

/// A GUANO block as the text of its chunk; empty once it has no fields.
pub fn guano_text(block: &Block) -> Result<String, String> {
    if block.fields.is_empty() {
        return Ok(String::new());
    }
    let version = block.version.as_deref().unwrap_or("1.0");
    let mut text = format!("GUANO|Version: {version}\n");
    for field in &block.fields {
        Kind::Guano.check_key(&field.key)?;
        text.push_str(&format!(
            "{}: {}\n",
            field.key,
            field.value.replace('\n', "\\n")
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guano_reads_back_as_it_is_written_and_keeps_its_version() {
        let text = "GUANO|Version: 1.0\nMake: Wildlife Acoustics\nWA|Song Meter|Prefix: SMU\nNote: two\\nlines\n\0\0";
        let block = guano(text);
        assert_eq!(block.version.as_deref(), Some("1.0"));
        let fields: Vec<(&str, &str)> = block
            .fields
            .iter()
            .map(|f| (f.key.as_str(), f.value.as_str()))
            .collect();
        assert_eq!(
            fields,
            [
                ("Make", "Wildlife Acoustics"),
                ("WA|Song Meter|Prefix", "SMU"),
                ("Note", "two\nlines")
            ]
        );
        assert_eq!(guano_text(&block).unwrap(), text.trim_end_matches('\0'));
    }

    #[test]
    fn keys_are_checked_for_what_each_kind_of_tag_takes() {
        assert!(Kind::Id3v2.check_key("TIT2").is_ok());
        assert!(Kind::Id3v2.check_key("TXXX:location").is_ok());
        assert!(Kind::Id3v2.check_key("title").is_err());
        assert!(Kind::Vorbis.check_key("LOCATION").is_ok());
        assert!(Kind::Vorbis.check_key("A=B").is_err());
        assert!(Kind::Mp4.check_key("©nam").is_ok());
        assert!(
            Kind::Mp4
                .check_key("----:com.apple.iTunes:Location")
                .is_ok()
        );
        assert!(Kind::Mp4.check_key("title").is_err());
        assert!(Kind::Guano.check_key("Loc Position").is_ok());
        assert!(Kind::Guano.check_key("a: b").is_err());
    }

    #[test]
    fn fields_have_names_people_know() {
        assert_eq!(Kind::Id3v2.label("TIT2"), "Title");
        assert_eq!(Kind::Id3v2.label("TXXX:location"), "location");
        assert_eq!(Kind::Vorbis.label("ALBUMARTIST"), "Albumartist");
        assert_eq!(Kind::Vorbis.label("title"), "Title");
        assert_eq!(Kind::Mp4.label("©ART"), "Artist");
        assert_eq!(
            Kind::Mp4.label("----:com.apple.iTunes:Location"),
            "Location"
        );
    }
}
