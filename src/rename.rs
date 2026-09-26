//! Renaming many files at once by rules, in the order Bulk Rename Utility
//! applies its panels: each rule changes the name the one before it left.
//! A batch either renames every file or leaves every file as it was, and
//! no rename ever replaces a file already there.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime};
use regex::Regex;

use crate::explorer::{is_audio, natural};
use crate::{meta, wav};

/// A file the rules can rename, with what they may take from it.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// When the recording began, from a Broadcast WAV or iXML header.
    pub recorded: Option<NaiveDateTime>,
}

impl Candidate {
    pub fn name(&self) -> String {
        file_name(&self.path)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Everything the rules can do, panel by panel.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rules {
    pub regex: RegexRule,
    pub name: NameRule,
    pub replace: ReplaceRule,
    pub case: CaseRule,
    pub remove: RemoveRule,
    pub moves: MoveRule,
    pub add: AddRule,
    pub date: DateRule,
    pub folder: FolderRule,
    pub numbering: NumberRule,
    pub extension: ExtensionRule,
    pub filters: Filters,
}

/// Panel 1: A regular expression and what replaces its matches; `\1` and `$1`
/// both stand for the first group.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RegexRule {
    pub find: String,
    pub replace: String,
    /// Match against the extension too.
    pub extension: bool,
}

/// Panel 2: What happens to the name as it stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NameMode {
    #[default]
    Keep,
    /// Removed, for the rules after to build a new one.
    Remove,
    /// Replaced by `NameRule::fixed`.
    Fixed,
    Reverse,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NameRule {
    pub mode: NameMode,
    pub fixed: String,
}

/// Panel 3: Text replaced wherever it appears.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplaceRule {
    pub find: String,
    pub with: String,
    pub match_case: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Case {
    #[default]
    Same,
    Lower,
    Upper,
    /// Each word's first letter capital, the rest small.
    Title,
    /// Only the first letter capital.
    Sentence,
}

impl Case {
    pub const ALL: [Self; 5] = [
        Self::Same,
        Self::Lower,
        Self::Upper,
        Self::Title,
        Self::Sentence,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Same => "Same",
            Self::Lower => "lower",
            Self::Upper => "UPPER",
            Self::Title => "Title",
            Self::Sentence => "Sentence",
        }
    }
}

/// Panel 4: Letter case, with words that always stay as typed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaseRule {
    pub case: Case,
    /// Separated by spaces or commas.
    pub except: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Crop {
    #[default]
    Off,
    /// Everything before the text goes.
    Before,
    /// Everything after the text goes.
    After,
}

/// Panel 5: Characters and words taken out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoveRule {
    pub first: usize,
    pub last: usize,
    /// Characters `from` to `to`, counting from 1; 0 turns it off.
    pub from: usize,
    pub to: usize,
    /// Each of these characters, wherever it is.
    pub chars: String,
    /// Each of these words, separated by spaces.
    pub words: String,
    pub crop: Crop,
    pub crop_at: String,
    pub digits: bool,
    /// Letters with accents lose them.
    pub accents: bool,
    /// Anything that is neither a letter, a digit, a space, `-` nor `_`.
    pub symbols: bool,
    /// Anything outside plain ASCII.
    pub high: bool,
    /// Spaces at either end.
    pub trim: bool,
    /// Runs of spaces become one.
    pub double_spaces: bool,
    pub lead_dots: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MoveMode {
    #[default]
    Off,
    CopyFirst,
    CopyLast,
    MoveFirst,
    MoveLast,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Place {
    #[default]
    Start,
    End,
    /// Before the character at `MoveRule::at` or `NumberRule::at`, counting
    /// from 1.
    At,
}

/// Panel 6: Some characters from one end copied or moved elsewhere.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MoveRule {
    pub mode: MoveMode,
    pub count: usize,
    pub to: Place,
    pub at: usize,
    pub separator: String,
}

/// Panel 7: Text added.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AddRule {
    pub prefix: String,
    /// Put before the character at `at`, counting from 1; 0 turns it off.
    pub insert: String,
    pub at: usize,
    pub suffix: String,
    /// A space before each capital that follows a small letter or digit.
    pub word_space: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Side {
    #[default]
    Off,
    Prefix,
    Suffix,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DateSource {
    /// When the recording began, as its metadata says.
    #[default]
    Recorded,
    Modified,
    Created,
    Now,
}

impl DateSource {
    pub const ALL: [Self; 4] = [Self::Recorded, Self::Modified, Self::Created, Self::Now];

    pub fn label(self) -> &'static str {
        match self {
            Self::Recorded => "Recorded",
            Self::Modified => "Modified",
            Self::Created => "Created",
            Self::Now => "Now",
        }
    }
}

/// Date formats offered, in `chrono`'s notation.
pub const DATE_FORMATS: [&str; 6] = [
    "%Y-%m-%d",
    "%Y%m%d",
    "%Y-%m-%d_%H-%M-%S",
    "%Y%m%d_%H%M%S",
    "%y%m%d",
    "%H%M%S",
];

/// Panel 8: A date and time added to the name.
#[derive(Clone, Debug, PartialEq)]
pub struct DateRule {
    pub side: Side,
    pub source: DateSource,
    /// In `chrono`'s notation, as in [`DATE_FORMATS`].
    pub format: String,
    pub separator: String,
    /// When the recording's metadata has no time, the file's modification
    /// time instead.
    pub fallback: bool,
}

impl Default for DateRule {
    fn default() -> Self {
        Self {
            side: Side::Off,
            source: DateSource::Recorded,
            format: DATE_FORMATS[2].to_owned(),
            separator: "_".to_owned(),
            fallback: false,
        }
    }
}

/// Panel 9: The names of the folders the file is in.
#[derive(Clone, Debug, PartialEq)]
pub struct FolderRule {
    pub side: Side,
    pub separator: String,
    /// How many folders up, the nearest last.
    pub levels: usize,
}

impl Default for FolderRule {
    fn default() -> Self {
        Self {
            side: Side::Off,
            separator: "_".to_owned(),
            levels: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NumberPlace {
    #[default]
    Off,
    Prefix,
    Suffix,
    Both,
    /// Before the character at `NumberRule::at`.
    At,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NumberKind {
    #[default]
    Decimal,
    /// a to z, then aa.
    Lower,
    Upper,
    Roman,
}

impl NumberKind {
    pub const ALL: [Self; 4] = [Self::Decimal, Self::Lower, Self::Upper, Self::Roman];

    pub fn label(self) -> &'static str {
        match self {
            Self::Decimal => "1 2 3",
            Self::Lower => "a b c",
            Self::Upper => "A B C",
            Self::Roman => "I II III",
        }
    }
}

/// Panel 10: A running number.
#[derive(Clone, Debug, PartialEq)]
pub struct NumberRule {
    pub place: NumberPlace,
    pub at: usize,
    pub start: i64,
    pub step: i64,
    /// At least this many digits, with zeros in front.
    pub pad: usize,
    pub separator: String,
    /// Start again whenever the first this many characters of the name
    /// change; 0 never does.
    pub restart_after: usize,
    /// Start again in every folder.
    pub per_folder: bool,
    pub kind: NumberKind,
}

impl Default for NumberRule {
    fn default() -> Self {
        Self {
            place: NumberPlace::Off,
            at: 1,
            start: 1,
            step: 1,
            pad: 2,
            separator: "_".to_owned(),
            restart_after: 0,
            per_folder: false,
            kind: NumberKind::Decimal,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExtensionMode {
    #[default]
    Same,
    Lower,
    Upper,
    Title,
    /// Replaced by `ExtensionRule::text`.
    Fixed,
    /// `ExtensionRule::text` added after it.
    Extra,
    Remove,
}

impl ExtensionMode {
    pub const ALL: [Self; 7] = [
        Self::Same,
        Self::Lower,
        Self::Upper,
        Self::Title,
        Self::Fixed,
        Self::Extra,
        Self::Remove,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Same => "Same",
            Self::Lower => "lower",
            Self::Upper => "UPPER",
            Self::Title => "Title",
            Self::Fixed => "Fixed",
            Self::Extra => "Extra",
            Self::Remove => "Remove",
        }
    }
}

/// Panel 11: The extension.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtensionRule {
    pub mode: ExtensionMode,
    pub text: String,
}

/// Panel 12: Which files are listed at all.
#[derive(Clone, Debug, PartialEq)]
pub struct Filters {
    /// Patterns with `*` and `?`, separated by `;`.
    pub mask: String,
    pub match_case: bool,
    pub subfolders: bool,
    /// Only the kinds of file the explorer lists.
    pub audio_only: bool,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            mask: "*".to_owned(),
            match_case: false,
            subfolders: false,
            audio_only: true,
        }
    }
}

impl Filters {
    pub fn admits(&self, path: &Path) -> bool {
        if self.audio_only && !is_audio(path) {
            return false;
        }
        let name = file_name(path);
        let fold = |s: &str| {
            if self.match_case {
                s.to_owned()
            } else {
                s.to_lowercase()
            }
        };
        let name = fold(&name);
        self.mask
            .split(';')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .any(|mask| wildcard(&fold(mask), &name))
    }
}

/// Whether `text` matches `pattern`, where `*` stands for any run of
/// characters and `?` for any one.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// The files in `folder` the filters admit, in the natural order of their
/// paths, with what the rules may use of each.
pub fn list(folder: &Path, filters: &Filters) -> Vec<Candidate> {
    let mut found = Vec::new();
    let mut folders = vec![folder.to_owned()];
    while let Some(dir) = folders.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if file_name(&path).starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if filters.subfolders {
                    folders.push(path);
                }
                continue;
            }
            if !kind.is_file() || !filters.admits(&path) {
                continue;
            }
            let metadata = entry.metadata().ok();
            found.push(Candidate {
                recorded: recorded(&path),
                size: metadata.as_ref().map_or(0, fs::Metadata::len),
                modified: metadata.as_ref().and_then(|m| m.modified().ok()),
                created: metadata.as_ref().and_then(|m| m.created().ok()),
                path,
            });
        }
    }
    found.sort_by(|a, b| {
        natural(
            &a.path.to_string_lossy().to_lowercase(),
            &b.path.to_string_lossy().to_lowercase(),
        )
    });
    found
}

/// When a WAV's recording began, from its header alone.
fn recorded(path: &Path) -> Option<NaiveDateTime> {
    let mut file = fs::File::open(path).ok()?;
    let w = wav::parse(&mut file).ok()?;
    let start = meta::from_wav(&w).start?;
    let digits: String = start.date?.chars().filter(char::is_ascii_digit).collect();
    let (year, month, day) = (
        digits.get(..4)?.parse().ok()?,
        digits.get(4..6)?.parse().ok()?,
        digits.get(6..8)?.parse().ok()?,
    );
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let seconds = start.seconds.max(0.0) as u32;
    let time = NaiveTime::from_num_seconds_from_midnight_opt(seconds % 86_400, 0)?;
    Some(date.and_time(time))
}

/// Why a file cannot be renamed as planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The rules themselves are wrong, such as a regular expression that
    /// does not parse.
    Rules(String),
    /// Not a name a file can have.
    Invalid(String),
    /// Another file in the batch would get the same name.
    Duplicate,
    /// A file that keeps its name already has it.
    Exists,
    /// The date asked for is not known for this file.
    NoDate,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rules(why) | Self::Invalid(why) => f.write_str(why),
            Self::Duplicate => f.write_str("another file would get this name too"),
            Self::Exists => f.write_str("a file already has this name"),
            Self::NoDate => f.write_str("its metadata has no recording time"),
        }
    }
}

/// What the rules make of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub from: PathBuf,
    pub to: PathBuf,
    pub problem: Option<Problem>,
}

impl Planned {
    pub fn changes(&self) -> bool {
        self.from != self.to
    }
}

/// The new name of each chosen file, in order, and what stands in the way
/// of any. Unchosen files get `None` and take no number.
pub fn plan(files: &[Candidate], chosen: &[bool], rules: &Rules) -> Vec<Option<Planned>> {
    let regex = match compile(&rules.regex) {
        Ok(regex) => regex,
        Err(why) => {
            return files
                .iter()
                .zip(chosen)
                .map(|(file, &chosen)| {
                    chosen.then(|| Planned {
                        from: file.path.clone(),
                        to: file.path.clone(),
                        problem: Some(Problem::Rules(why.clone())),
                    })
                })
                .collect();
        }
    };
    let now = Local::now().naive_local();
    let mut counter = Counter::new(&rules.numbering);
    let mut planned: Vec<Option<Planned>> = files
        .iter()
        .zip(chosen)
        .map(|(file, &chosen)| {
            chosen.then(|| {
                let (name, problem) = match new_name(file, rules, regex.as_ref(), now, &mut counter)
                {
                    Ok(name) => {
                        let problem = invalid(&name).map(Problem::Invalid);
                        (name, problem)
                    }
                    Err(problem) => (file.name(), Some(problem)),
                };
                Planned {
                    from: file.path.clone(),
                    to: file.path.with_file_name(name),
                    problem,
                }
            })
        })
        .collect();

    // Names compared as a disk that ignores case would.
    let key = |path: &Path| path.to_string_lossy().to_lowercase();
    // Only names that change count, so a file keeping its name is no
    // duplicate of one renamed onto it; that one is refused below instead.
    let mut targets: HashMap<String, usize> = HashMap::new();
    for p in planned.iter().flatten().filter(|p| p.changes()) {
        *targets.entry(key(&p.to)).or_default() += 1;
    }
    let leaving: HashSet<String> = planned
        .iter()
        .flatten()
        .filter(|p| p.changes() && p.problem.is_none())
        .map(|p| key(&p.from))
        .collect();
    for p in planned.iter_mut().flatten().filter(|p| p.problem.is_none()) {
        if targets.get(&key(&p.to)).is_some_and(|&n| n > 1) {
            p.problem = Some(Problem::Duplicate);
        } else if p.changes()
            && key(&p.to) != key(&p.from)
            && !leaving.contains(&key(&p.to))
            && fs::symlink_metadata(&p.to).is_ok()
        {
            p.problem = Some(Problem::Exists);
        }
    }
    planned
}

/// Why `name` cannot be a file name, if it cannot.
pub fn invalid(name: &str) -> Option<String> {
    let reserved: &[char] = if cfg!(windows) {
        &['/', '\\', ':', '<', '>', '"', '|', '?', '*']
    } else {
        &['/', '\\', ':']
    };
    if name.trim().is_empty() {
        Some("the name would be empty".into())
    } else if name.trim() != name {
        Some("the name would start or end with a space".into())
    } else if name.starts_with('.') {
        Some("the name would start with a dot".into())
    } else if name.contains(reserved) || name.chars().any(char::is_control) {
        Some(format!(
            "the name would hold one of {}",
            reserved.iter().collect::<String>()
        ))
    } else if name.len() > 255 {
        Some("the name would be longer than a disk allows".into())
    } else {
        None
    }
}

fn compile(rule: &RegexRule) -> Result<Option<Regex>, String> {
    if rule.find.is_empty() {
        return Ok(None);
    }
    Regex::new(&rule.find)
        .map(Some)
        .map_err(|e| format!("the regular expression does not work: {e}"))
}

/// Runs the numbering rule's counter along the chosen files.
struct Counter {
    next: i64,
    start: i64,
    step: i64,
    restart_after: usize,
    per_folder: bool,
    last_head: Option<String>,
    last_folder: Option<PathBuf>,
}

impl Counter {
    fn new(rule: &NumberRule) -> Self {
        Self {
            next: rule.start,
            start: rule.start,
            step: rule.step,
            restart_after: rule.restart_after,
            per_folder: rule.per_folder,
            last_head: None,
            last_folder: None,
        }
    }

    /// The number for a file whose name, so far, is `name`.
    fn take(&mut self, name: &str, folder: Option<&Path>) -> i64 {
        if self.restart_after > 0 {
            let head: String = name.chars().take(self.restart_after).collect();
            if self.last_head.as_ref().is_some_and(|last| *last != head) {
                self.next = self.start;
            }
            self.last_head = Some(head);
        }
        if self.per_folder {
            let folder = folder.map(Path::to_owned);
            if self.last_folder.is_some() && self.last_folder != folder {
                self.next = self.start;
            }
            self.last_folder = folder;
        }
        let number = self.next;
        self.next += self.step;
        number
    }
}

/// `file`'s name after every rule.
fn new_name(
    file: &Candidate,
    rules: &Rules,
    regex: Option<&Regex>,
    now: NaiveDateTime,
    counter: &mut Counter,
) -> Result<String, Problem> {
    let (mut stem, mut ext) = split(&file.name());
    if let Some(regex) = regex {
        let replace = groups(&rules.regex.replace);
        if rules.regex.extension {
            let whole = join(&stem, &ext);
            (stem, ext) = split(&regex.replace_all(&whole, replace.as_str()));
        } else {
            stem = regex.replace_all(&stem, replace.as_str()).into_owned();
        }
    }
    stem = match rules.name.mode {
        NameMode::Keep => stem,
        NameMode::Remove => String::new(),
        NameMode::Fixed => rules.name.fixed.clone(),
        NameMode::Reverse => stem.chars().rev().collect(),
    };
    stem = replace(&stem, &rules.replace);
    stem = recase(&stem, &rules.case);
    stem = remove(&stem, &rules.remove);
    stem = shift(&stem, &rules.moves);
    stem = add(&stem, &rules.add);
    if rules.date.side != Side::Off {
        let when = match rules.date.source {
            DateSource::Recorded => file
                .recorded
                .or_else(|| rules.date.fallback.then(|| local(file.modified)).flatten()),
            DateSource::Modified => local(file.modified),
            DateSource::Created => local(file.created),
            DateSource::Now => Some(now),
        }
        .ok_or(Problem::NoDate)?;
        let date = format_time(when, &rules.date.format)?;
        stem = beside(&stem, &date, &rules.date.separator, rules.date.side);
    }
    if rules.folder.side != Side::Off {
        let folders: Vec<String> = file
            .path
            .ancestors()
            .skip(1)
            .take(rules.folder.levels.max(1))
            .filter_map(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        let folders: Vec<&str> = folders.iter().rev().map(String::as_str).collect();
        let joined = folders.join(&rules.folder.separator);
        stem = beside(&stem, &joined, &rules.folder.separator, rules.folder.side);
    }
    let numbering = &rules.numbering;
    if numbering.place != NumberPlace::Off {
        let number = counter.take(&stem, file.path.parent());
        let text = number_text(number, numbering);
        let sep = &numbering.separator;
        stem = match numbering.place {
            NumberPlace::Off => stem,
            NumberPlace::Prefix => beside(&stem, &text, sep, Side::Prefix),
            NumberPlace::Suffix => beside(&stem, &text, sep, Side::Suffix),
            NumberPlace::Both => beside(
                &beside(&stem, &text, sep, Side::Prefix),
                &text,
                sep,
                Side::Suffix,
            ),
            NumberPlace::At => insert(&stem, &text, numbering.at),
        };
    }
    ext = match rules.extension.mode {
        ExtensionMode::Same => ext,
        ExtensionMode::Lower => ext.to_lowercase(),
        ExtensionMode::Upper => ext.to_uppercase(),
        ExtensionMode::Title => title_word(&ext),
        ExtensionMode::Fixed => rules.extension.text.trim_start_matches('.').to_owned(),
        ExtensionMode::Extra if ext.is_empty() => {
            rules.extension.text.trim_start_matches('.').to_owned()
        }
        ExtensionMode::Extra => {
            format!("{ext}.{}", rules.extension.text.trim_start_matches('.'))
        }
        ExtensionMode::Remove => String::new(),
    };
    Ok(join(&stem, &ext))
}

/// A name as the part before its last dot and the part after it.
fn split(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(dot) if dot > 0 => (name[..dot].to_owned(), name[dot + 1..].to_owned()),
        _ => (name.to_owned(), String::new()),
    }
}

fn join(stem: &str, ext: &str) -> String {
    if ext.is_empty() {
        stem.to_owned()
    } else {
        format!("{stem}.{ext}")
    }
}

/// `\1`, as Bulk Rename Utility writes a group, as the `regex` crate does.
fn groups(replace: &str) -> String {
    let mut out = String::with_capacity(replace.len());
    let mut chars = replace.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(d)) if d.is_ascii_digit() => {
                out.push_str("${");
                while let Some(d) = chars.peek().copied().filter(char::is_ascii_digit) {
                    out.push(d);
                    chars.next();
                }
                out.push('}');
            }
            _ => out.push(c),
        }
    }
    out
}

fn replace(stem: &str, rule: &ReplaceRule) -> String {
    if rule.find.is_empty() {
        return stem.to_owned();
    }
    if rule.match_case {
        return stem.replace(&rule.find, &rule.with);
    }
    let pattern = regex::escape(&rule.find);
    match Regex::new(&format!("(?i){pattern}")) {
        Ok(regex) => regex
            .replace_all(stem, regex::NoExpand(&rule.with))
            .into_owned(),
        Err(_) => stem.to_owned(),
    }
}

fn recase(stem: &str, rule: &CaseRule) -> String {
    let changed = match rule.case {
        Case::Same => return stem.to_owned(),
        Case::Lower => stem.to_lowercase(),
        Case::Upper => stem.to_uppercase(),
        Case::Title => words(stem, title_word),
        Case::Sentence => {
            let lower = stem.to_lowercase();
            let mut chars = lower.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => lower,
            }
        }
    };
    let exceptions: Vec<&str> = rule
        .except
        .split([' ', ','])
        .filter(|w| !w.is_empty())
        .collect();
    if exceptions.is_empty() {
        return changed;
    }
    // Each word put back as the exception spells it.
    words(&changed, |word| {
        exceptions
            .iter()
            .find(|e| e.to_lowercase() == word.to_lowercase())
            .map_or_else(|| word.to_owned(), |e| (*e).to_owned())
    })
}

/// `stem` with each word changed by `change`, and what separates the words
/// left as it was.
fn words(stem: &str, change: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(stem.len());
    let mut word = String::new();
    for c in stem.chars() {
        if c.is_alphanumeric() || c == '\'' {
            word.push(c);
        } else {
            if !word.is_empty() {
                out.push_str(&change(&word));
                word.clear();
            }
            out.push(c);
        }
    }
    if !word.is_empty() {
        out.push_str(&change(&word));
    }
    out
}

fn title_word(word: &str) -> String {
    let lower = word.to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => lower,
    }
}

fn remove(stem: &str, rule: &RemoveRule) -> String {
    let mut chars: Vec<char> = stem.chars().collect();
    let first = rule.first.min(chars.len());
    chars.drain(..first);
    let last = rule.last.min(chars.len());
    chars.truncate(chars.len() - last);
    if rule.from > 0 && rule.to >= rule.from && rule.from <= chars.len() {
        chars.drain(rule.from - 1..rule.to.min(chars.len()));
    }
    let mut s: String = chars.into_iter().collect();
    if !rule.chars.is_empty() {
        s.retain(|c| !rule.chars.contains(c));
    }
    if !rule.words.trim().is_empty() {
        let gone: Vec<String> = rule
            .words
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        s = words(&s, |word| {
            if gone.contains(&word.to_lowercase()) {
                String::new()
            } else {
                word.to_owned()
            }
        });
    }
    if rule.crop != Crop::Off && !rule.crop_at.is_empty() {
        let at = Regex::new(&format!("(?i){}", regex::escape(&rule.crop_at)))
            .ok()
            .and_then(|r| r.find(&s).map(|m| m.range()));
        if let Some(at) = at {
            s = match rule.crop {
                Crop::Off => s,
                Crop::Before => s[at.start..].to_owned(),
                Crop::After => s[..at.end].to_owned(),
            };
        }
    }
    if rule.digits {
        s.retain(|c| !c.is_ascii_digit());
    }
    if rule.accents {
        s = s.chars().map(unaccented).collect();
    }
    if rule.symbols {
        s.retain(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '_');
    }
    if rule.high {
        s.retain(|c| c.is_ascii());
    }
    if rule.double_spaces {
        while s.contains("  ") {
            s = s.replace("  ", " ");
        }
    }
    if rule.trim {
        s = s.trim().to_owned();
    }
    if rule.lead_dots {
        s = s.trim_start_matches('.').to_owned();
    }
    s
}

/// The letter without its accent, for the Latin letters with one.
fn unaccented(c: char) -> char {
    const FROM: &str = "ÀÁÂÃÄÅàáâãäåĀāĂăĄąÇçĆćĈĉĊċČčĎďĐđÈÉÊËèéêëĒēĔĕĖėĘęĚěĜĝĞğĠġĢģĤĥĦħÌÍÎÏìíîïĨĩĪīĬĭĮįİıĴĵĶķĹĺĻļĽľĿŀŁłÑñŃńŅņŇňÒÓÔÕÖØòóôõöøŌōŎŏŐőŔŕŖŗŘřŚśŜŝŞşŠšŢţŤťŦŧÙÚÛÜùúûüŨũŪūŬŭŮůŰűŲųŴŵÝýÿŶŷŸŹźŻżŽž";
    const TO: &str = "AAAAAAaaaaaaAaAaAaCcCcCcCcCcDdDdEEEEeeeeEeEeEeEeEeGgGgGgGgHhHhIIIIiiiiIiIiIiIiIiJjKkLlLlLlLlLlNnNnNnNnOOOOOOooooooOoOoOoRrRrRrSsSsSsSsTtTtTtUUUUuuuuUuUuUuUuUuUuWwYyyYyYZzZzZz";
    FROM.chars()
        .position(|f| f == c)
        .and_then(|i| TO.chars().nth(i))
        .unwrap_or(c)
}

fn shift(stem: &str, rule: &MoveRule) -> String {
    if rule.mode == MoveMode::Off || rule.count == 0 {
        return stem.to_owned();
    }
    let chars: Vec<char> = stem.chars().collect();
    let n = rule.count.min(chars.len());
    let first = matches!(rule.mode, MoveMode::CopyFirst | MoveMode::MoveFirst);
    let part: String = if first {
        chars[..n].iter().collect()
    } else {
        chars[chars.len() - n..].iter().collect()
    };
    let rest: String = match rule.mode {
        MoveMode::MoveFirst => chars[n..].iter().collect(),
        MoveMode::MoveLast => chars[..chars.len() - n].iter().collect(),
        _ => chars.iter().collect(),
    };
    let sep = &rule.separator;
    match rule.to {
        Place::Start => format!("{part}{sep}{rest}"),
        Place::End => format!("{rest}{sep}{part}"),
        Place::At => insert(&rest, &format!("{sep}{part}{sep}"), rule.at),
    }
}

fn add(stem: &str, rule: &AddRule) -> String {
    let mut s = stem.to_owned();
    if rule.word_space {
        let mut spaced = String::with_capacity(s.len() + 8);
        let mut before: Option<char> = None;
        for c in s.chars() {
            if c.is_uppercase() && before.is_some_and(|b| b.is_lowercase() || b.is_ascii_digit()) {
                spaced.push(' ');
            }
            spaced.push(c);
            before = Some(c);
        }
        s = spaced;
    }
    if rule.at > 0 && !rule.insert.is_empty() {
        s = insert(&s, &rule.insert, rule.at);
    }
    format!("{}{s}{}", rule.prefix, rule.suffix)
}

/// `text` put before the character at `at`, counting from 1, or at the end
/// of a name shorter than that.
fn insert(stem: &str, text: &str, at: usize) -> String {
    let chars: Vec<char> = stem.chars().collect();
    let at = at.saturating_sub(1).min(chars.len());
    let head: String = chars[..at].iter().collect();
    let tail: String = chars[at..].iter().collect();
    format!("{head}{text}{tail}")
}

/// `text` joined on at `side` of `stem`, with `sep` between unless either is
/// empty.
fn beside(stem: &str, text: &str, sep: &str, side: Side) -> String {
    if text.is_empty() {
        return stem.to_owned();
    }
    if stem.is_empty() {
        return text.to_owned();
    }
    match side {
        Side::Off => stem.to_owned(),
        Side::Prefix => format!("{text}{sep}{stem}"),
        Side::Suffix => format!("{stem}{sep}{text}"),
    }
}

fn local(time: Option<SystemTime>) -> Option<NaiveDateTime> {
    time.map(|t| DateTime::<Local>::from(t).naive_local())
}

fn format_time(when: NaiveDateTime, format: &str) -> Result<String, Problem> {
    use std::fmt::Write as _;
    let mut out = String::new();
    write!(out, "{}", when.format(format))
        .map_err(|_| Problem::Rules(format!("\"{format}\" is not a date format")))?;
    Ok(out)
}

fn number_text(number: i64, rule: &NumberRule) -> String {
    let (sign, n) = (if number < 0 { "-" } else { "" }, number.unsigned_abs());
    let digits = match rule.kind {
        NumberKind::Decimal => n.to_string(),
        NumberKind::Lower => letters(n, b'a'),
        NumberKind::Upper => letters(n, b'A'),
        NumberKind::Roman => roman(n),
    };
    let pad = if rule.kind == NumberKind::Decimal {
        "0".repeat(rule.pad.saturating_sub(digits.len()))
    } else {
        String::new()
    };
    format!("{sign}{pad}{digits}")
}

/// 1 is `a`, 26 `z`, 27 `aa`, as spreadsheet columns count; 0 is empty.
fn letters(mut n: u64, first: u8) -> String {
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(first + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn roman(mut n: u64) -> String {
    const NUMERALS: [(u64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for (value, numeral) in NUMERALS {
        while n >= value {
            out.push_str(numeral);
            n -= value;
        }
    }
    out
}

/// Renames each `from` to its `to`, all or none. Every file first takes a
/// name no other has, then its new one, so names can swap and pass along a
/// chain; if any step fails, every file done so far goes back.
pub fn execute(renames: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let renames: Vec<&(PathBuf, PathBuf)> = renames.iter().filter(|(a, b)| a != b).collect();
    let mut parked: Vec<PathBuf> = Vec::with_capacity(renames.len());
    for (i, (from, _)) in renames.iter().enumerate() {
        match park(from, i) {
            Ok(temp) => parked.push(temp),
            Err(e) => {
                let back = unpark(&renames, &parked, 0);
                return Err(failure(from, &e, back));
            }
        }
    }
    for (i, (_, to)) in renames.iter().enumerate() {
        if let Err(e) = rename_new(&parked[i], to) {
            // The ones already renamed go back to their parking names first.
            let mut stuck = Vec::new();
            for j in 0..i {
                if rename_new(&renames[j].1, &parked[j]).is_err() {
                    stuck.push(renames[j].1.clone());
                }
            }
            let mut back = unpark(&renames, &parked, 0);
            back.extend(stuck);
            return Err(failure(&renames[i].0, &e, back));
        }
    }
    Ok(())
}

/// Moves `from` to a hidden name beside it that no file has.
fn park(from: &Path, index: usize) -> io::Result<PathBuf> {
    for attempt in 0..100 {
        let temp = from.with_file_name(format!(
            ".soundcheck-renaming-{}-{index}-{attempt}",
            std::process::id()
        ));
        match rename_new(from, &temp) {
            Ok(()) => return Ok(temp),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name to rename through",
    ))
}

/// Puts every parked file from `start` back under its old name, and gives
/// the ones that would not go.
fn unpark(renames: &[&(PathBuf, PathBuf)], parked: &[PathBuf], start: usize) -> Vec<PathBuf> {
    parked
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(i, temp)| rename_new(temp, &renames[*i].0).is_err())
        .map(|(_, temp)| temp.to_path_buf())
        .collect()
}

fn failure(file: &Path, e: &io::Error, stuck: Vec<PathBuf>) -> String {
    let name = file_name(file);
    if stuck.is_empty() {
        format!("Nothing renamed: {name} could not be ({e}), so every file is as it was.")
    } else {
        let names: Vec<String> = stuck.iter().map(|p| p.display().to_string()).collect();
        format!(
            "Stopped at {name} ({e}), and these could not be put back, so they are here now: {}",
            names.join(", ")
        )
    }
}

/// Renames `from` to `to` unless something is already at `to`.
fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    match exclusive(from, to) {
        Some(result) => result,
        // A disk that cannot refuse by itself, such as a memory card's FAT.
        None => {
            if fs::symlink_metadata(to).is_ok() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(from, to)
        }
    }
}

/// A rename the system refuses outright where `to` exists, or `None` where
/// the disk cannot do that.
#[cfg(target_os = "macos")]
fn exclusive(from: &Path, to: &Path) -> Option<io::Result<()>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (Ok(from), Ok(to)) = (
        CString::new(from.as_os_str().as_bytes()),
        CString::new(to.as_os_str().as_bytes()),
    ) else {
        return Some(Err(io::ErrorKind::InvalidInput.into()));
    };
    // SAFETY: both are valid, nul-terminated paths.
    let done = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if done == 0 {
        return Some(Ok(()));
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOTSUP | libc::EINVAL) => None,
        _ => Some(Err(e)),
    }
}

#[cfg(target_os = "linux")]
fn exclusive(from: &Path, to: &Path) -> Option<io::Result<()>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (Ok(from), Ok(to)) = (
        CString::new(from.as_os_str().as_bytes()),
        CString::new(to.as_os_str().as_bytes()),
    ) else {
        return Some(Err(io::ErrorKind::InvalidInput.into()));
    };
    // SAFETY: both are valid, nul-terminated paths, relative to nothing.
    let done = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if done == 0 {
        return Some(Ok(()));
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOSYS | libc::EINVAL | libc::ENOTSUP) => None,
        _ => Some(Err(e)),
    }
}

#[cfg(windows)]
fn exclusive(from: &Path, to: &Path) -> Option<io::Result<()>> {
    use std::os::windows::ffi::OsStrExt;
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() };
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both are valid, nul-terminated wide paths. Without
    // MOVEFILE_REPLACE_EXISTING, an existing `to` makes the move fail.
    let done = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(from.as_ptr(), to.as_ptr(), 0)
    };
    Some(if done != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn exclusive(_from: &Path, _to: &Path) -> Option<io::Result<()>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            size: 0,
            modified: None,
            created: None,
            recorded: None,
        }
    }

    fn names(files: &[Candidate], rules: &Rules) -> Vec<String> {
        let chosen = vec![true; files.len()];
        plan(files, &chosen, rules)
            .into_iter()
            .map(|p| file_name(&p.unwrap().to))
            .collect()
    }

    fn one(name: &str, rules: &Rules) -> String {
        names(&[file(&format!("/nowhere/{name}"))], rules).remove(0)
    }

    #[test]
    fn each_panel_changes_the_name_as_bulk_rename_utility_does() {
        let rules = Rules {
            regex: RegexRule {
                find: r"^(\d+)_(\w+)$".into(),
                replace: r"\2-\1".into(),
                extension: false,
            },
            ..Rules::default()
        };
        assert_eq!(one("0714_heron.wav", &rules), "heron-0714.wav");

        let name = |mode, fixed: &str| Rules {
            name: NameRule {
                mode,
                fixed: fixed.into(),
            },
            ..Rules::default()
        };
        assert_eq!(one("abc.wav", &name(NameMode::Reverse, "")), "cba.wav");
        assert_eq!(one("abc.wav", &name(NameMode::Fixed, "take")), "take.wav");

        let rules = Rules {
            replace: ReplaceRule {
                find: "ZOOM".into(),
                with: "Field".into(),
                match_case: false,
            },
            ..Rules::default()
        };
        assert_eq!(one("zoom0001.WAV", &rules), "Field0001.WAV");

        let case = |case| Rules {
            case: CaseRule {
                case,
                except: "BWF".into(),
            },
            ..Rules::default()
        };
        assert_eq!(
            one("dawn CHORUS bwf.wav", &case(Case::Title)),
            "Dawn Chorus BWF.wav"
        );
        assert_eq!(
            one("DAWN chorus.wav", &case(Case::Sentence)),
            "Dawn chorus.wav"
        );

        let remove = |remove| Rules {
            remove,
            ..Rules::default()
        };
        let cleaned = remove(RemoveRule {
            first: 2,
            last: 1,
            digits: true,
            accents: true,
            double_spaces: true,
            trim: true,
            ..RemoveRule::default()
        });
        assert_eq!(one("xxJõe  laht 12 z.wav", &cleaned), "Joe laht.wav");
        let cropped = remove(RemoveRule {
            crop: Crop::Before,
            crop_at: "DAWN".into(),
            ..RemoveRule::default()
        });
        assert_eq!(one("x İ dawn chorus.wav", &cropped), "dawn chorus.wav");

        let rules = Rules {
            moves: MoveRule {
                mode: MoveMode::MoveFirst,
                count: 4,
                to: Place::End,
                at: 0,
                separator: "_".into(),
            },
            ..Rules::default()
        };
        assert_eq!(one("0714heron.wav", &rules), "heron_0714.wav");

        let rules = Rules {
            add: AddRule {
                prefix: "EE_".into(),
                insert: "-".into(),
                at: 5,
                suffix: "_A".into(),
                word_space: true,
            },
            ..Rules::default()
        };
        assert_eq!(one("DawnChorus.wav", &rules), "EE_Dawn- Chorus_A.wav");

        let rules = Rules {
            extension: ExtensionRule {
                mode: ExtensionMode::Lower,
                text: String::new(),
            },
            ..Rules::default()
        };
        assert_eq!(one("A.WAV", &rules), "A.wav");
    }

    #[test]
    fn dates_come_from_the_recording_and_numbers_count_the_chosen_files() {
        let take = Candidate {
            recorded: NaiveDate::from_ymd_opt(2026, 4, 3).and_then(|d| d.and_hms_opt(7, 14, 5)),
            ..file("/field/day1/take.wav")
        };
        let rules = Rules {
            date: DateRule {
                side: Side::Prefix,
                ..DateRule::default()
            },
            ..Rules::default()
        };
        assert_eq!(
            names(std::slice::from_ref(&take), &rules),
            ["2026-04-03_07-14-05_take.wav"]
        );
        let planned = plan(&[file("/field/untimed.wav")], &[true], &rules);
        assert_eq!(planned[0].as_ref().unwrap().problem, Some(Problem::NoDate));

        let rules = Rules {
            folder: FolderRule {
                side: Side::Prefix,
                separator: "-".into(),
                levels: 2,
            },
            ..Rules::default()
        };
        assert_eq!(names(&[take], &rules), ["field-day1-take.wav"]);

        let files = [file("/f/a.wav"), file("/f/b.wav"), file("/f/c.wav")];
        let numbered = |kind, start| Rules {
            numbering: NumberRule {
                place: NumberPlace::Prefix,
                kind,
                start,
                pad: 3,
                ..NumberRule::default()
            },
            ..Rules::default()
        };
        let planned = plan(
            &files,
            &[true, false, true],
            &numbered(NumberKind::Decimal, 1),
        );
        let got: Vec<Option<String>> = planned
            .iter()
            .map(|p| p.as_ref().map(|p| file_name(&p.to)))
            .collect();
        assert_eq!(
            got,
            [Some("001_a.wav".into()), None, Some("002_c.wav".into())]
        );
        assert_eq!(names(&files, &numbered(NumberKind::Roman, 9))[1], "X_b.wav");
        assert_eq!(
            names(&files, &numbered(NumberKind::Lower, 26))[1],
            "aa_b.wav"
        );
    }

    #[test]
    fn a_name_already_taken_or_given_twice_is_refused() {
        let dir = std::env::temp_dir().join(format!("soundcheck-plan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.wav", "b.wav", "keep.wav"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let files: Vec<Candidate> = ["a.wav", "b.wav"]
            .iter()
            .map(|n| file(&dir.join(n).to_string_lossy()))
            .collect();
        let rules = Rules {
            name: NameRule {
                mode: NameMode::Fixed,
                fixed: "keep".into(),
            },
            ..Rules::default()
        };
        let planned = plan(&files[..1], &[true], &rules);
        assert_eq!(planned[0].as_ref().unwrap().problem, Some(Problem::Exists));
        let planned = plan(&files, &[true, true], &rules);
        assert!(
            planned
                .iter()
                .flatten()
                .all(|p| p.problem == Some(Problem::Duplicate))
        );
        // A name another file in the batch is leaving is free: a becomes b
        // as b becomes c.
        let rules = Rules {
            name: NameRule {
                mode: NameMode::Remove,
                fixed: String::new(),
            },
            numbering: NumberRule {
                place: NumberPlace::Prefix,
                kind: NumberKind::Lower,
                start: 2,
                ..NumberRule::default()
            },
            ..Rules::default()
        };
        let chain: Vec<(String, Option<Problem>)> = plan(&files, &[true, true], &rules)
            .into_iter()
            .flatten()
            .map(|p| (file_name(&p.to), p.problem))
            .collect();
        assert_eq!(chain, [("b.wav".into(), None), ("c.wav".into(), None)]);
        // Only the file moving onto a name another keeps is refused.
        let rules = Rules {
            replace: ReplaceRule {
                find: "a".into(),
                with: "b".into(),
                match_case: false,
            },
            ..Rules::default()
        };
        let problems: Vec<Option<Problem>> = plan(&files, &[true, true], &rules)
            .into_iter()
            .flatten()
            .map(|p| p.problem)
            .collect();
        assert_eq!(problems, [Some(Problem::Exists), None]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            invalid(".hidden"),
            Some("the name would start with a dot".into())
        );
        assert!(invalid("a/b").is_some());
    }

    #[test]
    fn a_batch_swaps_chains_and_changes_case_and_undoes_cleanly() {
        let dir = std::env::temp_dir().join(format!("soundcheck-execute-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in [("a.wav", "A"), ("b.wav", "B"), ("c.wav", "C")] {
            std::fs::write(dir.join(name), body).unwrap();
        }
        let p = |n: &str| dir.join(n);
        let read = |n: &str| std::fs::read_to_string(p(n)).unwrap();
        // a and b swap; c moves to d; b's case changes on the way.
        let batch = vec![
            (p("a.wav"), p("B.wav")),
            (p("b.wav"), p("a.wav")),
            (p("c.wav"), p("d.wav")),
        ];
        execute(&batch).unwrap();
        assert_eq!(
            (read("a.wav"), read("d.wav")),
            ("B".to_owned(), "C".to_owned())
        );
        let listed: HashSet<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            listed.contains("B.wav") && !listed.contains("c.wav"),
            "{listed:?}"
        );
        let undo: Vec<(PathBuf, PathBuf)> =
            batch.iter().map(|(a, b)| (b.clone(), a.clone())).collect();
        execute(&undo).unwrap();
        assert_eq!(
            (read("a.wav"), read("b.wav"), read("c.wav")),
            ("A".to_owned(), "B".to_owned(), "C".to_owned())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_batch_that_cannot_finish_leaves_every_file_as_it_was() {
        let dir = std::env::temp_dir().join(format!("soundcheck-rollback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.wav", "b.wav", "taken.wav"] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        let p = |n: &str| dir.join(n);
        // The second rename would land on a file outside the batch.
        let batch = vec![(p("a.wav"), p("x.wav")), (p("b.wav"), p("taken.wav"))];
        let error = execute(&batch).unwrap_err();
        assert!(error.starts_with("Nothing renamed"), "{error}");
        for name in ["a.wav", "b.wav", "taken.wav"] {
            assert_eq!(std::fs::read_to_string(p(name)).unwrap(), name);
        }
        assert!(!p("x.wav").exists());
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn masks_match_like_a_file_manager_s() {
        assert!(wildcard("*.wav", "take 1.wav"));
        assert!(wildcard("take ?.wav", "take 7.wav"));
        assert!(!wildcard("*.wav", "take.flac"));
        assert!(wildcard("*", ""));
        let filters = Filters {
            mask: "*.WAV; *.flac".into(),
            match_case: false,
            subfolders: false,
            audio_only: true,
        };
        assert!(filters.admits(Path::new("/x/Take.wav")));
        assert!(!filters.admits(Path::new("/x/notes.txt")));
    }
}
