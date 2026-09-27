//! Finding recordings anywhere under the explorer's folder by what they are
//! called and what they say about themselves: the name and folder, every
//! field of their metadata and tags, their format, the dates they carry and
//! their length. Each file is read once, on threads of their own, and kept
//! for as long as its size and time stay the same.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::SystemTime;

use chrono::{DateTime, Local, Months, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta};
use eframe::egui::{self, Color32, RichText, TextEdit};
use rayon::prelude::*;

use crate::audio::{self, Opened};
use crate::explorer::{is_audio, natural};
use crate::tags::Kind;

/// Files read at once: enough to keep a fast disk busy, few enough to
/// leave the rest of the machine to the spectrogram.
const READERS: usize = 4;
const INVALID: Color32 = Color32::from_rgb(235, 70, 60);

/// Which of a recording's dates a search goes by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DateKind {
    #[default]
    Recorded,
    Created,
    Modified,
    Added,
    Released,
    FirstReleased,
    Encoded,
    Tagged,
}

impl DateKind {
    const ALL: [Self; 8] = [
        Self::Recorded,
        Self::Created,
        Self::Modified,
        Self::Added,
        Self::Released,
        Self::FirstReleased,
        Self::Encoded,
        Self::Tagged,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Recorded => "Recorded",
            Self::Created => "Created",
            Self::Modified => "Modified (last saved)",
            Self::Added => "Added to its folder",
            Self::Released => "Released",
            Self::FirstReleased => "First released",
            Self::Encoded => "Encoded",
            Self::Tagged => "Tagged",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Recorded => {
                "When the recording was made, as the file says: its Broadcast WAV, GUANO or iXML time, the RIFF date, an ID3, Vorbis, MP4 or APE date, or an AudioMoth comment"
            }
            Self::Created => "When this copy of the file was made on the disk",
            Self::Modified => "When the file was last written to, as by a save",
            Self::Added => {
                "When the file was put in the folder it is in, by a copy, a move or a save"
            }
            Self::Released => "The ID3 release time",
            Self::FirstReleased => "The ID3 original release time",
            Self::Encoded => "The ID3 encoding time",
            Self::Tagged => "The ID3 tagging time",
        }
    }

    /// Offered whatever the files hold: the disk keeps these for every
    /// file, and the recording's own date is what a search is usually for.
    fn always(self) -> bool {
        matches!(self, Self::Recorded | Self::Created | Self::Modified)
            || (self == Self::Added && cfg!(target_os = "macos"))
    }
}

/// A moment as precisely as it is known: from `start` up to, not
/// including, `end`, so a year alone is the whole of that year.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
}

impl Stamp {
    fn second(at: NaiveDateTime) -> Self {
        Self {
            start: at,
            end: at + TimeDelta::seconds(1),
        }
    }

    fn day(date: NaiveDate) -> Option<Self> {
        let start = date.and_time(NaiveTime::MIN);
        Some(Self {
            start,
            end: start.checked_add_signed(TimeDelta::days(1))?,
        })
    }

    fn system(time: SystemTime) -> Self {
        Self::second(DateTime::<Local>::from(time).naive_local())
    }

    /// As precisely as it is known: `2024`, `2024-05`, `2024-05-01`,
    /// `2024-05-01 21:30`, or with the seconds.
    pub fn text(&self) -> String {
        let long = self.end - self.start;
        let format = if long >= TimeDelta::days(365) {
            "%Y"
        } else if long >= TimeDelta::days(28) {
            "%Y-%m"
        } else if long >= TimeDelta::days(1) {
            "%Y-%m-%d"
        } else if long >= TimeDelta::minutes(1) {
            "%Y-%m-%d %H:%M"
        } else {
            "%Y-%m-%d %H:%M:%S"
        };
        self.start.format(format).to_string()
    }
}

/// A date and time as recordings and tags write them, and as typed:
/// `2024-05-01 21:30:05`, `2024-05-01T21:30`, `2024:05:01`, `2024-05`,
/// `2024`, or with the year last, `1.5.2024`. A time zone after the time
/// is left as it is: a recording's times are those where it was made.
pub fn parse_stamp(text: &str) -> Option<Stamp> {
    let text = text.trim();
    let (date, time) = match text.find(['T', 't', ' ']) {
        Some(at) => (&text[..at], Some(text[at + 1..].trim())),
        None => (text, None),
    };
    let numbers: Vec<&str> = date.split(['-', ':', '/', '.']).collect();
    if numbers
        .iter()
        .any(|n| n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let number = |i: usize| numbers.get(i).and_then(|n| n.parse::<u32>().ok());
    let (year, month, day) = match numbers.as_slice() {
        [y] if y.len() == 4 => (number(0)?, None, None),
        [y, _] if y.len() == 4 => (number(0)?, Some(number(1)?), None),
        [y, _, _] if y.len() == 4 => (number(0)?, Some(number(1)?), Some(number(2)?)),
        [_, _, y] if y.len() == 4 => (number(2)?, Some(number(1)?), Some(number(0)?)),
        _ => return None,
    };
    let year = i32::try_from(year).ok()?;
    let (Some(month), Some(day)) = (month, day) else {
        if time.is_some() {
            return None;
        }
        let first = NaiveDate::from_ymd_opt(year, month.unwrap_or(1), 1)?;
        let months = if month.is_some() { 1 } else { 12 };
        let start = first.and_time(NaiveTime::MIN);
        return Some(Stamp {
            start,
            end: start.checked_add_months(Months::new(months))?,
        });
    };
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let Some(time) = time.filter(|t| !t.is_empty()) else {
        return Stamp::day(date);
    };
    // The clock, up to where a zone or anything else begins.
    let clock = time
        .split(|c: char| !(c.is_ascii_digit() || c == ':' || c == '.' || c == ','))
        .next()
        .unwrap_or_default();
    let parts: Vec<&str> = clock.split(':').collect();
    let whole = |i: usize| parts.get(i).and_then(|p| p.parse::<u32>().ok());
    let (hour, minute) = (whole(0)?, parts.get(1).map_or(Some(0), |_| whole(1))?);
    let (second, precision) = match parts.len() {
        1 => (0, TimeDelta::hours(1)),
        2 => (0, TimeDelta::minutes(1)),
        _ => {
            let s: f64 = parts[2].replace(',', ".").parse().ok()?;
            (s.floor() as u32, TimeDelta::seconds(1))
        }
    };
    let start = date.and_hms_opt(hour, minute, second)?;
    Some(Stamp {
        start,
        end: start.checked_add_signed(precision)?,
    })
}

/// A length as typed: `90`, `90 s`, `1:30`, `1:02:03`, `2 m`, `1.5 h`.
pub fn parse_length(text: &str) -> Option<f64> {
    let text = text.trim().to_lowercase().replace(',', ".");
    if text.contains(':') {
        let mut seconds = 0.0;
        for part in text.split(':') {
            let value: f64 = part.trim().parse().ok()?;
            if !value.is_finite() || value < 0.0 {
                return None;
            }
            seconds = seconds * 60.0 + value;
        }
        return Some(seconds);
    }
    let (number, scale) = if let Some(n) = text.strip_suffix("min") {
        (n, 60.0)
    } else if let Some(n) = text.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = text.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = text.strip_suffix('h') {
        (n, 3600.0)
    } else {
        (text.as_str(), 1.0)
    };
    let value: f64 = number.trim().parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value * scale)
}

/// The words of a search, lowercased: a phrase in quotes is one word, and
/// a leading `*`, as in `*.wav`, is dropped.
fn words(query: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut rest = query;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return words;
        }
        let (word, after) = match rest.strip_prefix('"') {
            Some(quoted) => match quoted.find('"') {
                Some(end) => (&quoted[..end], &quoted[end + 1..]),
                None => (quoted, ""),
            },
            None => {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
        };
        let word = word.trim_start_matches('*').to_lowercase();
        if !word.trim().is_empty() {
            words.push(word);
        }
        rest = after;
    }
}

/// Kinds of file, for narrowing a search to one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Any,
    Wav,
    Flac,
    Mp3,
    Mp4,
    Ogg,
    Aiff,
    Caf,
    Matroska,
}

impl Format {
    const ALL: [Self; 9] = [
        Self::Any,
        Self::Wav,
        Self::Flac,
        Self::Mp3,
        Self::Mp4,
        Self::Ogg,
        Self::Aiff,
        Self::Caf,
        Self::Matroska,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Any => "Any format",
            Self::Wav => "WAV",
            Self::Flac => "FLAC",
            Self::Mp3 => "MP3",
            Self::Mp4 => "M4A, AAC",
            Self::Ogg => "Ogg",
            Self::Aiff => "AIFF",
            Self::Caf => "CAF",
            Self::Matroska => "MKA, WebM",
        }
    }

    fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Any => &[],
            Self::Wav => &["wav", "wave", "bwf", "rf64"],
            Self::Flac => &["flac"],
            Self::Mp3 => &["mp3"],
            Self::Mp4 => &["m4a", "aac"],
            Self::Ogg => &["ogg", "oga"],
            Self::Aiff => &["aif", "aiff", "aifc"],
            Self::Caf => &["caf"],
            Self::Matroska => &["mka", "webm"],
        }
    }

    fn takes(self, extension: &str) -> bool {
        self == Self::Any || self.extensions().contains(&extension)
    }
}

/// One recording as a search sees it.
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    /// Lowercase, as the formats list it.
    extension: String,
    /// Size and time on the disk when read, to tell whether it has changed.
    stamp: (u64, Option<SystemTime>),
    /// Label and value of everything the file says about itself.
    fields: Vec<(String, String)>,
    /// The name and every value, lowercased, for matching.
    text: String,
    pub seconds: Option<f64>,
    dates: Vec<(DateKind, Stamp)>,
    /// Why the file could not be read, when it could not.
    pub error: Option<String>,
}

impl Entry {
    /// The file's first date of `kind`, if it has one.
    pub fn date(&self, kind: DateKind) -> Option<Stamp> {
        self.dates.iter().find(|(k, _)| *k == kind).map(|(_, s)| *s)
    }

    /// The labels of the fields holding any of `words`.
    fn matched_in(&self, words: &[String]) -> Vec<&str> {
        let mut labels: Vec<&str> = Vec::new();
        for (label, value) in &self.fields {
            let value = value.to_lowercase();
            if words.iter().any(|w| value.contains(w.as_str())) && !labels.contains(&label.as_str())
            {
                labels.push(label);
            }
        }
        labels
    }
}

/// The file at `path` read for searching: its metadata and tags, its
/// length, and its dates, those it holds and those the disk keeps.
fn read(path: &Path, disk: &fs::Metadata) -> Entry {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut fields = Vec::new();
    let mut dates = Vec::new();
    let mut seconds = None;
    // A decoder panic on a damaged file leaves that file unread, and the
    // search going.
    let opened = std::panic::catch_unwind(|| audio::open(path))
        .unwrap_or_else(|_| Err("reading it crashed".into()));
    let error = match opened {
        Ok(opened) => {
            if opened.info.frames > 0 {
                seconds = Some(opened.info.seconds());
            }
            // Past the name and the folder, which the search has anyway.
            fields.extend(audio::file_rows(path, &opened.info).into_iter().skip(2));
            describe(&opened, &mut fields, &mut dates);
            None
        }
        Err(e) => Some(e),
    };
    let modified = disk.modified().ok();
    dates.extend(modified.map(|t| (DateKind::Modified, Stamp::system(t))));
    dates.extend(
        disk.created()
            .ok()
            .map(|t| (DateKind::Created, Stamp::system(t))),
    );
    dates.extend(added(path).map(|t| (DateKind::Added, Stamp::system(t))));
    let text = std::iter::once(name.as_str())
        .chain(fields.iter().map(|(_, v)| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    Entry {
        path: path.to_owned(),
        name,
        extension,
        stamp: (disk.len(), modified),
        fields,
        text,
        seconds,
        dates,
        error,
    }
}

/// Every field `opened` has, labelled with where it is from, and the dates
/// among them.
fn describe(
    opened: &Opened,
    fields: &mut Vec<(String, String)>,
    dates: &mut Vec<(DateKind, Stamp)>,
) {
    for (title, rows) in &opened.details.sections {
        for (label, value) in rows {
            fields.push((format!("{label} ({title})"), value.clone()));
        }
    }
    let mut recorded = Vec::new();
    if let Some(start) = &opened.meta.start
        && let Some(date) = start.date.as_deref().and_then(parse_stamp)
    {
        let seconds = start.seconds.rem_euclid(86_400.0) as u32;
        let time = NaiveTime::from_num_seconds_from_midnight_opt(seconds, 0);
        recorded.extend(time.map(|t| Stamp::second(date.start.date().and_time(t))));
    }
    let Some(edits) = &opened.edits else {
        dates.extend(recorded.into_iter().map(|s| (DateKind::Recorded, s)));
        return;
    };
    for m in &edits.markers {
        for text in [&m.label, &m.note].into_iter().filter(|t| !t.is_empty()) {
            fields.push(("Marker".into(), text.clone()));
        }
    }
    // iXML's own copy of the Broadcast WAV time, where the chunk is missing.
    if recorded.is_empty() {
        let ixml = |name: &str| {
            edits
                .ixml
                .iter()
                .find(|(label, value)| label.ends_with(name) && !value.trim().is_empty())
                .map(|(_, value)| value.trim())
        };
        if let Some(date) = ixml("BWF_ORIGINATION_DATE") {
            let time = ixml("BWF_ORIGINATION_TIME").unwrap_or_default();
            recorded.extend(parse_stamp(&format!("{date} {time}")).or_else(|| parse_stamp(date)));
        }
    }
    for (id, value) in &edits.info {
        match id {
            b"ICRD" => recorded.extend(parse_stamp(value)),
            b"ICMT" => recorded.extend(audiomoth(value)),
            _ => {}
        }
    }
    for block in &edits.tags {
        for f in &block.fields {
            fields.push((
                format!("{} ({})", block.kind.label(&f.key), block.title()),
                f.value.clone(),
            ));
            let kind = match (block.kind, f.key.as_str()) {
                (Kind::Id3v2, "TDRC") | (Kind::Mp4, "©day") | (Kind::Guano, "Timestamp") => {
                    DateKind::Recorded
                }
                (Kind::Id3v1, "Year") => DateKind::Recorded,
                (Kind::Vorbis, key) if key.eq_ignore_ascii_case("DATE") => DateKind::Recorded,
                (Kind::Ape, key)
                    if key.eq_ignore_ascii_case("Year")
                        || key.eq_ignore_ascii_case("Record Date") =>
                {
                    DateKind::Recorded
                }
                (Kind::Id3v2, "TDRL") => DateKind::Released,
                (Kind::Id3v2, "TDOR") => DateKind::FirstReleased,
                (Kind::Id3v2, "TDEN") => DateKind::Encoded,
                (Kind::Id3v2, "TDTG") => DateKind::Tagged,
                _ => continue,
            };
            if let Some(stamp) = parse_stamp(&f.value) {
                if kind == DateKind::Recorded {
                    recorded.push(stamp);
                } else {
                    dates.push((kind, stamp));
                }
            }
        }
    }
    dates.extend(recorded.into_iter().map(|s| (DateKind::Recorded, s)));
}

/// The time an AudioMoth writes into its comment: `Recorded at 21:30:00
/// 01/05/2024 (UTC+1) by AudioMoth ...`, the date day first.
fn audiomoth(comment: &str) -> Option<Stamp> {
    let rest = &comment[comment.find("Recorded at ")? + "Recorded at ".len()..];
    let mut parts = rest.split_whitespace();
    let (time, date) = (parts.next()?, parts.next()?);
    let mut day = date.split('/');
    let (d, m, y) = (day.next()?, day.next()?, day.next()?);
    parse_stamp(&format!("{y}-{m}-{d} {time}"))
}

/// When the file was put in its folder, as macOS keeps it.
#[cfg(target_os = "macos")]
fn added(path: &Path) -> Option<SystemTime> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_ADDEDTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // The length of what came back, then the time.
    #[repr(C, packed(4))]
    struct Answer {
        length: u32,
        time: libc::timespec,
    }
    let mut answer = std::mem::MaybeUninit::<Answer>::zeroed();
    // SAFETY: `path` is a C string, `list` asks for one timespec, and
    // `answer` has room for it and the length before it.
    let done = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&raw mut list).cast(),
            answer.as_mut_ptr().cast(),
            std::mem::size_of::<Answer>(),
            0,
        )
    };
    if done != 0 {
        return None;
    }
    // SAFETY: filled in by the call, which succeeded, or zeroed.
    let answer = unsafe { answer.assume_init() };
    let (length, time) = (answer.length, answer.time);
    let seconds = u64::try_from(time.tv_sec).ok().filter(|s| *s > 0)?;
    (length as usize >= std::mem::size_of::<Answer>()).then(|| {
        SystemTime::UNIX_EPOCH
            + std::time::Duration::new(seconds, u32::try_from(time.tv_nsec).unwrap_or(0))
    })
}

#[cfg(not(target_os = "macos"))]
fn added(_: &Path) -> Option<SystemTime> {
    None
}

/// Every recording under `root`, with what the disk says of each, dotfiles
/// and folders left out as the tree leaves them out. A folder reached
/// twice, through a link, is gone through once.
fn walk(root: &Path, cancel: &AtomicBool, walked: &AtomicUsize) -> Vec<(PathBuf, fs::Metadata)> {
    let mut found = Vec::new();
    let mut folders = vec![root.to_owned()];
    let mut seen = HashSet::new();
    while let Some(dir) = folders.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if !seen.insert(dir.canonicalize().unwrap_or_else(|_| dir.clone())) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let is_dir = match entry.file_type() {
                Ok(t) if t.is_symlink() => path.is_dir(),
                Ok(t) => t.is_dir(),
                Err(_) => false,
            };
            if is_dir {
                folders.push(path);
            } else if is_audio(&path)
                && let Ok(disk) = fs::metadata(&path)
            {
                found.push((path, disk));
                walked.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    found
}

enum Update {
    /// The walk is done, and found this many.
    Listed(usize),
    Read(Arc<Entry>),
    Done,
}

/// A walk through a folder and the reading of what it found, under way.
struct Run {
    rx: mpsc::Receiver<Update>,
    cancel: Arc<AtomicBool>,
    walked: Arc<AtomicUsize>,
    listed: Option<usize>,
    read: usize,
    /// Read into here while an earlier run's results still show, so a
    /// fresh look does not empty the list while it runs.
    fresh: Option<Vec<Found>>,
}

impl Drop for Run {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// A recording under the root searched.
pub struct Found {
    pub entry: Arc<Entry>,
    /// The folder it is in, from the root: empty in the root itself.
    pub folder: String,
    folder_lower: String,
}

impl Found {
    /// Where any of `words` is: the name, the folder, and each field
    /// holding one.
    pub fn matched_in(&self, words: &[String]) -> Vec<&str> {
        let has = |text: &str| words.iter().any(|w| text.contains(w.as_str()));
        let mut labels = Vec::new();
        if has(&self.entry.name.to_lowercase()) {
            labels.push("Name");
        }
        if has(&self.folder_lower) {
            labels.push("Folder");
        }
        labels.extend(self.entry.matched_in(words));
        labels
    }

    fn new(entry: Arc<Entry>, root: &Path) -> Self {
        let folder = entry
            .path
            .parent()
            .and_then(|p| p.strip_prefix(root).ok())
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Self {
            folder_lower: folder.to_lowercase(),
            folder,
            entry,
        }
    }
}

/// What a search asks, as parsed from its fields.
#[derive(Clone, Debug, Default, PartialEq)]
struct Filter {
    words: Vec<String>,
    format: Format,
    date: DateKind,
    from: Option<NaiveDateTime>,
    to: Option<NaiveDateTime>,
    shortest: Option<f64>,
    longest: Option<f64>,
}

impl Filter {
    fn asks(&self) -> bool {
        *self
            != Self {
                date: self.date,
                ..Self::default()
            }
    }

    fn admits(&self, found: &Found) -> bool {
        let entry = &found.entry;
        let dated = self.from.is_some() || self.to.is_some();
        let long = self.shortest.is_some() || self.longest.is_some();
        self.format.takes(&entry.extension)
            && self
                .words
                .iter()
                .all(|w| entry.text.contains(w.as_str()) || found.folder_lower.contains(w.as_str()))
            && (!dated
                || entry.dates.iter().any(|(kind, s)| {
                    *kind == self.date
                        && self.from.is_none_or(|from| s.start >= from)
                        && self.to.is_none_or(|to| s.end <= to)
                }))
            && (!long
                || entry.seconds.is_some_and(|len| {
                    self.shortest.is_none_or(|s| len >= s) && self.longest.is_none_or(|l| len <= l)
                }))
    }
}

/// The explorer's search: what is typed and picked, every recording under
/// the root as read so far, and those that match.
#[derive(Default)]
pub struct Search {
    query: String,
    format: Format,
    date: DateKind,
    from: String,
    to: String,
    shortest: String,
    longest: String,
    /// The filters under the search box are open.
    open: bool,
    filter: Filter,
    root: Option<PathBuf>,
    found: Vec<Found>,
    /// Every file read so far, under any root.
    known: HashMap<PathBuf, Arc<Entry>>,
    run: Option<Run>,
    /// Look through the root again the next time the search is used.
    stale: bool,
    /// Indices into `found` of those that match, and how many of `found`
    /// have been tried.
    matched: Vec<usize>,
    tried: usize,
}

impl Search {
    /// Whether anything is asked, so that the matches show in place of the
    /// tree.
    pub fn asks(&self) -> bool {
        self.filter.asks()
    }

    /// Looks through the root again the next time the search is used, for
    /// files saved, renamed or copied in since.
    pub fn refresh(&mut self) {
        self.stale = true;
    }

    pub fn len(&self) -> usize {
        self.matched.len()
    }

    pub fn result(&self, i: usize) -> Option<&Found> {
        self.found.get(*self.matched.get(i)?)
    }

    pub fn words(&self) -> &[String] {
        &self.filter.words
    }

    pub fn date(&self) -> DateKind {
        self.filter.date
    }

    /// The match after or before `from`, the file open.
    pub fn step(&self, from: Option<&Path>, down: bool) -> Option<PathBuf> {
        let at = from.and_then(|p| {
            (0..self.len()).find(|&i| self.result(i).is_some_and(|f| f.entry.path == p))
        });
        let next = match (at, down) {
            (Some(i), true) => i + 1,
            (Some(i), false) => i.checked_sub(1)?,
            (None, true) => 0,
            (None, false) => self.len().checked_sub(1)?,
        };
        self.result(next).map(|f| f.entry.path.clone())
    }

    pub fn position(&self, path: &Path) -> Option<usize> {
        (0..self.len()).find(|&i| self.result(i).is_some_and(|f| f.entry.path == path))
    }

    /// Takes in what the fields ask and what the reading has found since
    /// the last frame, starting a look through `root` when one is needed.
    pub fn update(&mut self, root: Option<&Path>, ctx: &egui::Context) {
        let filter = self.parse();
        if filter != self.filter {
            self.filter = filter;
            self.matched.clear();
            self.tried = 0;
        }
        // Another folder: nothing found under the last one is any use.
        if self.root.as_deref() != root {
            self.root = root.map(Path::to_owned);
            self.run = None;
            self.found.clear();
            self.matched.clear();
            self.tried = 0;
            self.stale = true;
        }
        if !self.asks() {
            return;
        }
        if std::mem::take(&mut self.stale)
            && let Some(root) = root
        {
            self.run = Some(self.start(root, !self.found.is_empty(), ctx));
        }
        self.take();
        let filter = &self.filter;
        for (i, found) in self.found.iter().enumerate().skip(self.tried) {
            if filter.admits(found) {
                self.matched.push(i);
            }
        }
        self.tried = self.found.len();
    }

    /// Starts reading everything under `root`, into what shows now or, for
    /// `again`, into a fresh list that takes its place once done.
    fn start(&self, root: &Path, again: bool, ctx: &egui::Context) -> Run {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let walked = Arc::new(AtomicUsize::new(0));
        let known: HashMap<PathBuf, Arc<Entry>> = self
            .known
            .iter()
            .filter(|(path, _)| path.starts_with(root))
            .map(|(path, entry)| (path.clone(), Arc::clone(entry)))
            .collect();
        let (root, ctx) = (root.to_owned(), ctx.clone());
        let (stop, count) = (Arc::clone(&cancel), Arc::clone(&walked));
        std::thread::spawn(move || {
            let files = walk(&root, &stop, &count);
            // The receiver only goes when the run is dropped, which stops it.
            if tx.send(Update::Listed(files.len())).is_err() {
                return;
            }
            ctx.request_repaint();
            let one = |(path, disk): (PathBuf, fs::Metadata)| {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let stamp = (disk.len(), disk.modified().ok());
                let entry = match known.get(&path) {
                    Some(entry) if entry.stamp == stamp => Arc::clone(entry),
                    _ => Arc::new(read(&path, &disk)),
                };
                if tx.send(Update::Read(entry)).is_err() {
                    stop.store(true, Ordering::Relaxed);
                }
                ctx.request_repaint();
            };
            match rayon::ThreadPoolBuilder::new().num_threads(READERS).build() {
                Ok(pool) => pool.install(|| files.into_par_iter().for_each(one)),
                Err(_) => files.into_iter().for_each(one),
            }
            let _ = tx.send(Update::Done);
            ctx.request_repaint();
        });
        Run {
            rx,
            cancel,
            walked,
            listed: None,
            read: 0,
            fresh: again.then(Vec::new),
        }
    }

    /// What the run under way has sent since the last frame.
    fn take(&mut self) {
        let (Some(run), Some(root)) = (&mut self.run, &self.root) else {
            return;
        };
        let mut done = false;
        for update in run.rx.try_iter() {
            match update {
                Update::Listed(n) => run.listed = Some(n),
                Update::Read(entry) => {
                    run.read += 1;
                    self.known.insert(entry.path.clone(), Arc::clone(&entry));
                    let found = Found::new(entry, root);
                    match &mut run.fresh {
                        Some(fresh) => fresh.push(found),
                        None => self.found.push(found),
                    }
                }
                Update::Done => done = true,
            }
        }
        if !done {
            return;
        }
        if let Some(fresh) = run.fresh.take() {
            self.found = fresh;
        }
        self.run = None;
        self.found.sort_by(|a, b| {
            natural(&a.folder_lower, &b.folder_lower)
                .then_with(|| natural(&a.entry.name.to_lowercase(), &b.entry.name.to_lowercase()))
        });
        self.matched.clear();
        self.tried = 0;
    }

    /// The fields as typed, parsed; a field that does not parse asks
    /// nothing, and shows red.
    fn parse(&self) -> Filter {
        let from = parse_stamp(&self.from).map(|s| s.start);
        let to = parse_stamp(&self.to).map(|s| s.end);
        Filter {
            words: words(&self.query),
            format: self.format,
            date: self.date,
            from,
            to,
            shortest: parse_length(&self.shortest),
            longest: parse_length(&self.longest),
        }
    }

    /// How far the reading has got, and how many match.
    pub fn status(&self) -> String {
        let matched = self.matched.len();
        match &self.run {
            Some(run) if run.fresh.is_none() => match run.listed {
                None => format!(
                    "Looking through the folders: {} recordings so far",
                    run.walked.load(Ordering::Relaxed)
                ),
                Some(total) => format!("{matched} found, reading {} of {total}", run.read),
            },
            _ if matched == 0 => "No recordings match".into(),
            _ => {
                let total = self.found.len();
                let unread = self
                    .found
                    .iter()
                    .filter(|f| f.entry.error.is_some())
                    .count();
                let of = if matched == 1 {
                    format!("1 of {total} recordings")
                } else {
                    format!("{matched} of {total} recordings")
                };
                if unread > 0 {
                    format!("{of}, {unread} not readable")
                } else {
                    of
                }
            }
        }
    }

    /// The search box, and under it, once opened, the format, the date and
    /// the length to narrow it by.
    pub fn bar(&mut self, ui: &mut egui::Ui) {
        let narrowed = self.format != Format::Any
            || [&self.from, &self.to, &self.shortest, &self.longest]
                .iter()
                .any(|f| !f.trim().is_empty());
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .selectable_label(self.open || narrowed, "Filters")
                    .on_hover_text("The format, a date and the length")
                    .clicked()
                {
                    self.open = !self.open;
                }
                if !self.query.is_empty() && ui.small_button("×").on_hover_text("Clear").clicked() {
                    self.query.clear();
                }
                ui.add(
                    TextEdit::singleline(&mut self.query)
                        .hint_text("Search")
                        .desired_width(ui.available_width()),
                )
                .on_hover_text(
                    "Every recording under this folder, by its name, its folder, and anything its metadata and tags say. All the words have to be there; words in quotes go together, and .wav finds a kind of file.",
                );
            });
        });
        if self.open {
            self.filters(ui, narrowed);
        }
    }

    fn filters(&mut self, ui: &mut egui::Ui, narrowed: bool) {
        let offered: Vec<DateKind> = DateKind::ALL
            .into_iter()
            .filter(|k| k.always() || self.found.iter().any(|f| f.entry.date(*k).is_some()))
            .collect();
        let dates = "A date, or a date and a time: 2024-05-01, 2024-05-01 21:30, 2024-05 for all of May, 2024, or 1.5.2024";
        let lengths = "Seconds, minutes and seconds, or with a unit: 90, 1:30, 1:02:03, 2 m, 1.5 h";
        egui::Grid::new("search filters")
            .num_columns(2)
            .spacing([6.0, 4.0])
            .show(ui, |ui| {
                ui.label("Format");
                egui::ComboBox::from_id_salt("search format")
                    .selected_text(self.format.name())
                    .show_ui(ui, |ui| {
                        for f in Format::ALL {
                            ui.selectable_value(&mut self.format, f, f.name());
                        }
                    });
                ui.end_row();
                ui.label("Date");
                egui::ComboBox::from_id_salt("search date")
                    .selected_text(self.date.name())
                    .show_ui(ui, |ui| {
                        for kind in offered {
                            ui.selectable_value(&mut self.date, kind, kind.name())
                                .on_hover_text(kind.hint());
                        }
                    })
                    .response
                    .on_hover_text(self.date.hint());
                ui.end_row();
                ui.label("");
                range(
                    ui,
                    [&mut self.from, &mut self.to],
                    ["earliest", "latest"],
                    dates,
                    |t| parse_stamp(t).is_some(),
                );
                ui.end_row();
                ui.label("Length");
                range(
                    ui,
                    [&mut self.shortest, &mut self.longest],
                    ["shortest", "longest"],
                    lengths,
                    |t| parse_length(t).is_some(),
                );
                ui.end_row();
            });
        if narrowed && ui.small_button("Clear the filters").clicked() {
            self.format = Format::Any;
            for field in [
                &mut self.from,
                &mut self.to,
                &mut self.shortest,
                &mut self.longest,
            ] {
                field.clear();
            }
        }
    }
}

/// Two fields for the ends of a range, apart by "to", each red while what
/// is typed in it does not read as `valid` says.
fn range(
    ui: &mut egui::Ui,
    ends: [&mut String; 2],
    hints: [&str; 2],
    help: &str,
    valid: impl Fn(&str) -> bool,
) {
    ui.horizontal(|ui| {
        let width = ((ui.available_width() - 24.0) / 2.0).max(36.0);
        let [first, second] = ends;
        for (i, (end, hint)) in [first, second].into_iter().zip(hints).enumerate() {
            if i == 1 {
                ui.label(RichText::new("to").weak());
            }
            let wrong = !end.trim().is_empty() && !valid(end);
            let mut edit = TextEdit::singleline(end)
                .hint_text(hint)
                .desired_width(width);
            if wrong {
                edit = edit.text_color(INVALID);
            }
            ui.add(edit).on_hover_text(help);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer::AUDIO_EXTENSIONS;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn dates_read_as_the_stretch_they_name() {
        let stamp = |t| parse_stamp(t).map(|s| (s.start, s.end));
        assert_eq!(
            stamp("2024-05-01 21:30:05"),
            Some((at("2024-05-01 21:30:05"), at("2024-05-01 21:30:06")))
        );
        assert_eq!(
            stamp("2024-05-01T21:30:05.250+02:00"),
            Some((at("2024-05-01 21:30:05"), at("2024-05-01 21:30:06")))
        );
        assert_eq!(
            stamp("2024-05-01T21:30"),
            Some((at("2024-05-01 21:30:00"), at("2024-05-01 21:31:00")))
        );
        assert_eq!(
            stamp("2024:05:01"),
            Some((at("2024-05-01 00:00:00"), at("2024-05-02 00:00:00")))
        );
        assert_eq!(
            stamp("2024-02"),
            Some((at("2024-02-01 00:00:00"), at("2024-03-01 00:00:00")))
        );
        assert_eq!(
            stamp("2024"),
            Some((at("2024-01-01 00:00:00"), at("2025-01-01 00:00:00")))
        );
        assert_eq!(stamp("1.5.2024"), stamp("2024-05-01"));
        for wrong in [
            "",
            "May",
            "2024-13-01",
            "24-05-01",
            "2024-05-01 25:00",
            "2024 21:00",
        ] {
            assert_eq!(parse_stamp(wrong), None, "{wrong:?}");
        }
    }

    #[test]
    fn an_audiomoth_comment_says_when_it_recorded() {
        let comment =
            "Recorded at 21:30:00 01/05/2024 (UTC+1) by AudioMoth 24F319045FB6F4C2 at medium gain";
        assert_eq!(
            audiomoth(comment).map(|s| s.start),
            Some(at("2024-05-01 21:30:00"))
        );
        assert_eq!(audiomoth("Heron colony at dusk"), None);
    }

    #[test]
    fn lengths_read_in_seconds_minutes_and_hours() {
        assert_eq!(parse_length("90"), Some(90.0));
        assert_eq!(parse_length("1:30"), Some(90.0));
        assert_eq!(parse_length("1:02:03"), Some(3723.0));
        assert_eq!(parse_length("2 m"), Some(120.0));
        assert_eq!(parse_length("2min"), Some(120.0));
        assert_eq!(parse_length("1,5 h"), Some(5400.0));
        assert_eq!(parse_length("45s"), Some(45.0));
        assert_eq!(parse_length("long"), None);
        assert_eq!(parse_length("-3"), None);
    }

    #[test]
    fn words_go_by_spaces_or_quotes_and_a_star_is_dropped() {
        assert_eq!(
            words(r#"Heron "dawn chorus"  *.WAV"#),
            ["heron", "dawn chorus", ".wav"]
        );
        assert!(words("   ").is_empty());
    }

    #[test]
    fn every_kind_of_file_the_explorer_lists_has_one_format() {
        for ext in AUDIO_EXTENSIONS {
            let formats: Vec<Format> = Format::ALL[1..]
                .iter()
                .copied()
                .filter(|f| f.takes(ext))
                .collect();
            assert_eq!(formats.len(), 1, "{ext}: {formats:?}");
        }
    }

    fn entry(name: &str, text: &str, seconds: Option<f64>, dates: Vec<(DateKind, Stamp)>) -> Found {
        let path = PathBuf::from("/field/Day 1").join(name);
        let entry = Entry {
            name: name.to_owned(),
            extension: Path::new(name)
                .extension()
                .unwrap()
                .to_string_lossy()
                .to_lowercase(),
            stamp: (0, None),
            fields: vec![("Title (ID3v2.4)".into(), text.into())],
            text: format!("{name}\n{text}").to_lowercase(),
            seconds,
            dates,
            error: None,
            path,
        };
        Found::new(Arc::new(entry), Path::new("/field"))
    }

    #[test]
    fn a_filter_takes_words_from_names_fields_and_folders_and_narrows_by_format_date_and_length() {
        let may = parse_stamp("2024-05-03 05:10:00").unwrap();
        let heron = entry(
            "heron.wav",
            "Grey heron at dawn",
            Some(95.0),
            vec![(DateKind::Recorded, may)],
        );
        let year = entry(
            "song.mp3",
            "Heron song",
            None,
            vec![(DateKind::Recorded, parse_stamp("2024").unwrap())],
        );
        let filter = |f: Filter| {
            [&heron, &year]
                .iter()
                .map(|e| f.admits(e))
                .collect::<Vec<_>>()
        };
        let words = |q: &str| Filter {
            words: super::words(q),
            ..Filter::default()
        };
        assert_eq!(filter(words("HERON")), [true, true]);
        assert_eq!(filter(words("heron dawn")), [true, false]);
        assert_eq!(filter(words("day 1 .mp3")), [false, true]);
        assert_eq!(
            filter(Filter {
                format: Format::Wav,
                ..words("")
            }),
            [true, false]
        );
        // May 2024 holds the one recorded then; a year alone could be any
        // time in it, so it is not taken to be in May.
        let in_may = Filter {
            from: Some(at("2024-05-01 00:00:00")),
            to: parse_stamp("2024-05").map(|s| s.end),
            ..Filter::default()
        };
        assert_eq!(filter(in_may.clone()), [true, false]);
        assert_eq!(
            filter(Filter {
                date: DateKind::Modified,
                ..in_may
            }),
            [false, false]
        );
        let a_while = Filter {
            shortest: Some(60.0),
            longest: Some(120.0),
            ..Filter::default()
        };
        assert_eq!(filter(a_while), [true, false]);
        assert!(!Filter::default().asks());
        assert!(
            !Filter {
                date: DateKind::Tagged,
                ..Filter::default()
            }
            .asks()
        );
        assert_eq!(heron.matched_in(&super::words("dawn")), ["Title (ID3v2.4)"]);
        assert_eq!(
            heron.matched_in(&super::words("heron day")),
            ["Name", "Folder", "Title (ID3v2.4)"]
        );
    }

    #[test]
    fn a_folder_is_read_for_its_recordings_their_tags_and_their_dates() {
        let root = std::env::temp_dir().join(format!("soundcheck-search-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Day 2/.hidden")).unwrap();
        let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        fs::copy(testdata.join("tagged.flac"), root.join("Day 2/pier.flac")).unwrap();
        fs::copy(testdata.join("tagged.mp3"), root.join("pier.mp3")).unwrap();
        fs::copy(
            testdata.join("tagged.mp3"),
            root.join("Day 2/.hidden/skipped.mp3"),
        )
        .unwrap();
        fs::write(root.join("notes.txt"), b"not a recording").unwrap();
        let (cancel, walked) = (AtomicBool::new(false), AtomicUsize::new(0));
        let mut files = walk(&root, &cancel, &walked);
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let names: Vec<PathBuf> = files
            .iter()
            .map(|(p, _)| p.strip_prefix(&root).unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            [PathBuf::from("Day 2/pier.flac"), PathBuf::from("pier.mp3")]
        );
        let flac = read(&files[0].0, &files[0].1);
        assert!(flac.error.is_none(), "{:?}", flac.error);
        assert!(
            flac.text.contains("diane") && flac.text.contains("57qjg4cj+rj"),
            "{}",
            flac.text
        );
        assert!(flac.seconds.is_some_and(|s| s > 0.0));
        assert!(flac.date(DateKind::Modified).is_some() && flac.date(DateKind::Created).is_some());
        let found = Found::new(Arc::new(flac), &root);
        assert_eq!(found.folder, "Day 2");
        fs::remove_dir_all(&root).unwrap();
    }
}
