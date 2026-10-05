//! The folder tree on the left, with the shortcuts kept above it and the
//! search that can show in its place. Folders are read on a background
//! thread, and only the rows in view are drawn, so neither a slow card
//! reader nor a folder of a hundred thousand recordings holds up the window.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Color32, Label, Pos2, Rect, RichText, Sense, Shape, Stroke, TextStyle, TextWrapMode,
    Vec2, WidgetInfo, WidgetText, WidgetType,
};
use serde::{Deserialize, Serialize};

use crate::search::{Found, Search};
use crate::views;

/// How often the shortcuts are looked for again, for a card put back in.
const LOOK_AGAIN: Duration = Duration::from_secs(5);

pub(crate) const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", "bwf", "rf64", "flac", "mp3", "m4a", "aac", "ogg", "oga", "aif", "aiff", "aifc",
    "caf", "mka", "webm",
];

pub(crate) fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

/// Why a folder could not be read: short enough for a row, and in full.
struct Unreadable {
    label: &'static str,
    why: String,
}

/// EPERM: what macOS refuses an app a folder with until its privacy
/// settings let the app in, as they ask for Downloads, Desktop, Documents
/// and cards.
const NOT_PERMITTED: i32 = 1;

/// Folders and audio files directly inside `dir`: folders first, then in
/// file-manager order, dotfiles hidden. Or why it could not be read.
fn list(dir: &Path) -> Result<Vec<Entry>, Unreadable> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        if cfg!(target_os = "macos") && e.raw_os_error() == Some(NOT_PERMITTED) {
            Unreadable {
                label: "Not allowed to read this folder",
                why: "macOS keeps soundcheck out of it. In System Settings, Privacy & Security, Files & Folders lets soundcheck into Downloads, Desktop, Documents and cards; anything else macOS guards, as the Photos library or Mail, only Full Disk Access opens. It is read again on coming back to soundcheck".into(),
            }
        } else {
            Unreadable {
                label: "Can't read this folder",
                why: e.to_string(),
            }
        }
    })?;
    let mut listed: Vec<(String, Entry)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let path = e.path();
            // The listing already knows each entry's type; only a link needs
            // looking up to see what it points at.
            let is_dir = match e.file_type() {
                Ok(t) if t.is_symlink() => path.is_dir(),
                Ok(t) => t.is_dir(),
                Err(_) => false,
            };
            (is_dir || is_audio(&path)).then(|| (name.to_lowercase(), Entry { path, name, is_dir }))
        })
        .collect();
    listed.sort_by(|(a, ea), (b, eb)| {
        eb.is_dir
            .cmp(&ea.is_dir)
            .then_with(|| natural(a, b))
            .then_with(|| ea.name.cmp(&eb.name))
    });
    Ok(listed.into_iter().map(|(_, entry)| entry).collect())
}

/// How far a two-finger swipe goes, in points, to count.
const SWIPE: f32 = 100.0;

/// A run of scrolling that starts this soon after the fingers lift is the
/// slide macOS goes on with, not a swipe of its own.
const SLIDE: f64 = 0.1;

/// Whether a trackpad moves what it scrolls the way the fingers go, as
/// macOS's natural scrolling does unless switched off. Elsewhere there is no
/// one setting to ask, and it is taken to.
pub fn natural_scrolling() -> bool {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSUserDefaults, ns_string};
        let defaults = NSUserDefaults::standardUserDefaults();
        let key = ns_string!("com.apple.swipescrolldirection");
        defaults.objectForKey(key).is_none() || defaults.boolForKey(key)
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Two-finger swipes on a trackpad, told apart from scrolling. One with the
/// fingers going right, as a browser goes back on, counts as they lift; the
/// slide on after them does not count again.
#[derive(Default)]
pub struct Swipe {
    /// How far the fingers have gone since they went down.
    travel: Vec2,
    touching: bool,
    /// When the fingers last lifted.
    lifted: Option<f64>,
}

impl Swipe {
    /// Takes in a frame's `events`, at `now`, and says whether a swipe
    /// right ended among them. `natural` says whether what scrolls moves
    /// the way the fingers go.
    pub fn right(&mut self, events: &[egui::Event], now: f64, natural: impl Fn() -> bool) -> bool {
        let mut swiped = false;
        for event in events {
            let egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta,
                phase,
                ..
            } = event
            else {
                continue;
            };
            match phase {
                egui::TouchPhase::Start => {
                    self.touching = self.lifted.is_none_or(|t| now - t > SLIDE);
                    self.travel = Vec2::ZERO;
                }
                egui::TouchPhase::Move => {
                    if self.touching {
                        self.travel += *delta;
                    }
                }
                egui::TouchPhase::End | egui::TouchPhase::Cancel => {
                    if std::mem::take(&mut self.touching) {
                        self.lifted = Some(now);
                        if *phase == egui::TouchPhase::End {
                            let right = if natural() {
                                self.travel.x
                            } else {
                                -self.travel.x
                            };
                            swiped |= right >= SWIPE && right > 2.0 * self.travel.y.abs();
                        }
                    }
                }
            }
        }
        swiped
    }
}

/// Compares names the way file managers do: a run of digits by its value,
/// so `take 9` comes before `take 10`.
pub(crate) fn natural(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a, b);
    loop {
        match (a.chars().next(), b.chars().next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let ((da, ra), (db, rb)) = (digits(a), digits(b));
                let (da, db) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                match da.len().cmp(&db.len()).then_with(|| da.cmp(db)) {
                    Ordering::Equal => (a, b) = (ra, rb),
                    unequal => return unequal,
                }
            }
            (Some(x), Some(y)) if x != y => return x.cmp(&y),
            (Some(x), Some(_)) => (a, b) = (&a[x.len_utf8()..], &b[x.len_utf8()..]),
        }
    }
}

/// The leading run of digits, and the rest.
fn digits(s: &str) -> (&str, &str) {
    s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()))
}

/// On Windows each drive is a tree of its own, so above a drive's root is
/// the list of drives. Elsewhere nothing is above the root folder.
fn drives() -> Vec<Entry> {
    #[cfg(windows)]
    {
        // SAFETY: takes no arguments and only returns a bit mask.
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        (b'A'..=b'Z')
            .filter(|letter| mask & (1 << (letter - b'A')) != 0)
            .map(|letter| {
                let name = format!("{}:", char::from(letter));
                Entry {
                    path: PathBuf::from(format!("{name}\\")),
                    name,
                    is_dir: true,
                }
            })
            .collect()
    }
    #[cfg(not(windows))]
    Vec::new()
}

enum Row<'a> {
    Entry {
        depth: usize,
        entry: &'a Entry,
        open: bool,
    },
    Loading {
        depth: usize,
    },
    Unreadable {
        depth: usize,
        why: &'a Unreadable,
    },
}

enum Action {
    Open(PathBuf),
    Toggle(PathBuf),
    Root(Option<PathBuf>),
    /// Kept among the shortcuts, or taken out once it is.
    Shortcut(Shortcut),
    /// A recording among the shortcuts: shown in its folder, and opened.
    Reveal(PathBuf),
}

/// A folder or a recording kept at the top of the explorer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    pub path: PathBuf,
    pub folder: bool,
}

/// What the explorer shows besides the tree: the shortcuts, which it adds
/// to and takes from, and the search.
pub struct Parts<'a> {
    pub shortcuts: Option<&'a mut Vec<Shortcut>>,
    pub search: bool,
}

pub struct Explorer {
    /// `None` is the list of drives on Windows, and nothing elsewhere.
    root: Option<PathBuf>,
    /// Folders read so far; `None` while a read is under way.
    listings: HashMap<PathBuf, Option<Result<Vec<Entry>, Unreadable>>>,
    open: HashSet<PathBuf>,
    /// The file open, or the folder the arrow keys last moved to.
    pub selected: Option<PathBuf>,
    /// Scroll the selected file into view once its folder has been read.
    reveal: bool,
    /// Scroll only as far as it takes to show the selected row.
    follow: bool,
    /// How far down the rows were scrolled last frame, and how much of
    /// them showed.
    scrolled: (f32, f32),
    tx: mpsc::Sender<(PathBuf, Result<Vec<Entry>, Unreadable>)>,
    rx: mpsc::Receiver<(PathBuf, Result<Vec<Entry>, Unreadable>)>,
    search: Search,
    /// The search's matches show in place of the tree.
    searching: bool,
    /// Whether each shortcut was there when last looked for, when that was,
    /// and whether a look is under way.
    there: HashMap<PathBuf, bool>,
    looked: Option<Instant>,
    looking: bool,
    there_tx: mpsc::Sender<Vec<(PathBuf, bool)>>,
    there_rx: mpsc::Receiver<Vec<(PathBuf, bool)>>,
    /// The window had the keyboard last frame.
    focused: bool,
}

impl Default for Explorer {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        let (there_tx, there_rx) = mpsc::channel();
        Self {
            root: None,
            listings: HashMap::new(),
            open: HashSet::new(),
            selected: None,
            reveal: false,
            follow: false,
            scrolled: (0.0, 0.0),
            tx,
            rx,
            search: Search::default(),
            searching: false,
            there: HashMap::new(),
            looked: None,
            looking: false,
            there_tx,
            there_rx,
            focused: true,
        }
    }
}

impl Explorer {
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn set_root(&mut self, dir: &Path) {
        if self.root.as_deref() != Some(dir) {
            self.root = Some(dir.to_owned());
            self.listings.retain(|path, _| path.starts_with(dir));
        }
    }

    /// Reads `dir` again, to show a file renamed in it.
    pub fn forget(&mut self, dir: &Path) {
        self.listings.remove(dir);
        self.search.refresh();
    }

    /// A file was written to, so the search reads it again.
    pub fn changed(&mut self) {
        self.search.refresh();
    }

    /// Roots the tree at `file`'s folder with `file` selected and in view.
    pub fn reveal(&mut self, file: &Path) {
        if let Some(dir) = file.parent() {
            self.set_root(dir);
        }
        self.selected = Some(file.to_owned());
        self.reveal = true;
    }

    fn up(&self) -> Option<Option<PathBuf>> {
        let root = self.root.as_deref()?;
        match root.parent() {
            Some(parent) => Some(Some(parent.to_owned())),
            None if cfg!(windows) => Some(None),
            None => None,
        }
    }

    /// Roots the tree one folder further up, as the ⬆ button does.
    pub fn go_up(&mut self) {
        match self.up() {
            Some(Some(parent)) => self.set_root(&parent),
            Some(None) => self.root = None,
            None => {}
        }
    }

    /// The rows as the tree shows them, and the folders among them not read
    /// yet. With no root, `drives` are the rows.
    fn listed<'a>(&'a self, drives: &'a [Entry]) -> (Vec<Row<'a>>, Vec<PathBuf>) {
        let (mut rows, mut unread) = (Vec::new(), Vec::new());
        match &self.root {
            Some(root) => self.rows(root, 0, &mut rows, &mut unread),
            None => rows.extend(drives.iter().map(|entry| Row::Entry {
                depth: 0,
                open: self.open.contains(&entry.path),
                entry,
            })),
        }
        (rows, unread)
    }

    /// Moves the selection a row down or up, as the arrow keys do. A folder
    /// it lands on is selected; a file is returned instead, for the caller
    /// to open, which selects it. With the search's matches showing, it
    /// moves through them.
    pub fn step(&mut self, down: bool) -> Option<PathBuf> {
        if self.searching {
            self.follow = true;
            return self.search.step(self.selected.as_deref(), down);
        }
        let drives = drives();
        let (rows, _) = self.listed(&drives);
        let entries: Vec<&Entry> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Entry { entry, .. } => Some(*entry),
                Row::Loading { .. } | Row::Unreadable { .. } => None,
            })
            .collect();
        let at = self
            .selected
            .as_deref()
            .and_then(|s| entries.iter().position(|e| e.path == s));
        let next = match (at, down) {
            (Some(i), true) => i + 1,
            (Some(i), false) => i.checked_sub(1)?,
            (None, true) => 0,
            (None, false) => entries.len().checked_sub(1)?,
        };
        let entry = entries.get(next)?;
        let (path, is_dir) = (entry.path.clone(), entry.is_dir);
        self.follow = true;
        if is_dir {
            self.selected = Some(path);
            return None;
        }
        Some(path)
    }

    fn rows<'a>(
        &'a self,
        dir: &Path,
        depth: usize,
        rows: &mut Vec<Row<'a>>,
        unread: &mut Vec<PathBuf>,
    ) {
        let entries = match self.listings.get(dir) {
            Some(Some(Ok(entries))) => entries,
            Some(Some(Err(why))) => {
                rows.push(Row::Unreadable { depth, why });
                return;
            }
            Some(None) => {
                rows.push(Row::Loading { depth });
                return;
            }
            None => {
                unread.push(dir.to_owned());
                rows.push(Row::Loading { depth });
                return;
            }
        };
        for entry in entries {
            let open = entry.is_dir && self.open.contains(&entry.path);
            rows.push(Row::Entry { depth, entry, open });
            if open {
                self.rows(&entry.path, depth + 1, rows, unread);
            }
        }
    }

    fn read(&mut self, dir: PathBuf, ctx: &egui::Context) {
        self.listings.insert(dir.clone(), None);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let entries = list(&dir);
            // The receiver only goes away with the window: nobody to tell.
            let _ = tx.send((dir, entries));
            ctx.request_repaint();
        });
    }

    /// Draws the shortcuts, the search and the tree or what the search
    /// found, as `parts` asks, and returns the audio file clicked this
    /// frame, if any.
    pub fn ui(&mut self, ui: &mut egui::Ui, parts: Parts<'_>) -> Option<PathBuf> {
        for (dir, entries) in self.rx.try_iter() {
            // A folder closed while it was being read no longer wants it.
            if let Some(slot @ None) = self.listings.get_mut(&dir) {
                *slot = Some(entries);
            }
        }
        // Back from answering macOS, or from its settings, a folder it kept
        // soundcheck out of may be open to it now.
        let focused = ui.input(|i| i.focused);
        if focused && !self.focused {
            self.listings
                .retain(|_, listing| !matches!(listing, Some(Err(_))));
        }
        self.focused = focused;
        let Parts {
            mut shortcuts,
            search,
        } = parts;
        let kept: Option<&[Shortcut]> = shortcuts.as_deref().map(Vec::as_slice);

        let mut action = None;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if let Some(up) = self.up() {
                let hint = up
                    .as_ref()
                    .map_or("Drives".into(), |p| p.display().to_string());
                if ui.small_button("⬆").on_hover_text(hint).clicked() {
                    action = Some(Action::Root(up));
                }
            }
            if ui
                .small_button("🔄")
                .on_hover_text("Reload, and look for the shortcuts again")
                .clicked()
            {
                self.listings.clear();
                self.search.refresh();
                self.looked = None;
            }
            if let (Some(list), Some(root)) = (kept, &self.root) {
                let (star, hint) = if list.iter().any(|s| &s.path == root) {
                    ("★", "Take this folder out of the shortcuts")
                } else {
                    ("☆", "Keep this folder in the shortcuts")
                };
                if ui.small_button(star).on_hover_text(hint).clicked() {
                    action = Some(Action::Shortcut(Shortcut {
                        path: root.clone(),
                        folder: true,
                    }));
                }
            }
            let title = self.root.as_ref().map_or("Drives".into(), |r| {
                r.file_name().map_or_else(
                    || r.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                )
            });
            ui.add(Label::new(RichText::new(title).strong()).truncate());
        });
        if let Some(list) = kept {
            self.shortcuts_ui(ui, list, &mut action);
        }
        ui.separator();
        self.searching = false;
        if search {
            self.search.bar(ui);
            self.search.update(self.root.as_deref(), ui.ctx());
            self.searching = self.search.asks();
        }
        if self.searching {
            self.results_ui(ui, kept, &mut action);
        } else {
            self.tree_ui(ui, kept, &mut action);
        }
        match action? {
            Action::Open(file) => return Some(file),
            Action::Reveal(file) => {
                self.reveal(&file);
                return Some(file);
            }
            Action::Toggle(dir) => {
                if !self.open.remove(&dir) {
                    self.open.insert(dir.clone());
                }
                // Read again on every open, so recordings copied in since
                // the last look show up.
                self.listings.retain(|path, _| !path.starts_with(&dir));
            }
            Action::Root(Some(dir)) => self.set_root(&dir),
            Action::Root(None) => self.root = None,
            Action::Shortcut(shortcut) => {
                if let Some(list) = shortcuts.as_mut() {
                    match list.iter().position(|s| s.path == shortcut.path) {
                        Some(i) => {
                            list.remove(i);
                        }
                        None => list.push(shortcut),
                    }
                }
                self.looked = None;
            }
        }
        None
    }

    /// The shortcuts, each a click from its folder or its recording, and
    /// dimmed while it is not there, as on a card taken out.
    fn shortcuts_ui(&mut self, ui: &mut egui::Ui, list: &[Shortcut], action: &mut Option<Action>) {
        self.look_for(list, ui.ctx());
        if list.is_empty() {
            let hint = RichText::new("Right-click a folder or a recording to keep it up here")
                .weak()
                .size(11.0);
            ui.add(Label::new(hint).wrap());
            return;
        }
        let height = ui.spacing().interact_size.y;
        // However many there are, the tree keeps most of the room.
        egui::ScrollArea::vertical()
            .id_salt("shortcuts")
            .max_height(ui.available_height() * 0.3)
            .auto_shrink([false, true])
            .show(ui, |ui| self.shortcut_rows(ui, list, height, action));
    }

    fn shortcut_rows(
        &self,
        ui: &mut egui::Ui,
        list: &[Shortcut],
        height: f32,
        action: &mut Option<Action>,
    ) {
        for shortcut in list {
            let path = &shortcut.path;
            let there = self.there.get(path).copied().unwrap_or(true);
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            let icon = if shortcut.folder { "📁" } else { "🎵" };
            let chosen = if shortcut.folder {
                self.root.as_ref() == Some(path)
            } else {
                self.selected.as_ref() == Some(path)
            };
            let (response, _) = draw_row(
                ui,
                0,
                height,
                &format!("{icon} {name}"),
                None,
                chosen,
                !there,
            );
            let tip = if there {
                path.display().to_string()
            } else {
                format!("Not there now: {}", path.display())
            };
            let response = response.on_hover_text(tip);
            if response.clicked() && there {
                *action = Some(if shortcut.folder {
                    Action::Root(Some(path.clone()))
                } else {
                    Action::Reveal(path.clone())
                });
            }
            response.context_menu(|ui| {
                if ui.button("Take out of the shortcuts").clicked() {
                    *action = Some(Action::Shortcut(shortcut.clone()));
                    ui.close();
                }
            });
        }
    }

    /// Looks again now and then whether each shortcut is there, on a thread
    /// of its own, so a drive that has gone never holds up the window.
    fn look_for(&mut self, list: &[Shortcut], ctx: &egui::Context) {
        for looked in self.there_rx.try_iter() {
            self.there.extend(looked);
            self.looking = false;
        }
        if list.is_empty() || self.looking || self.looked.is_some_and(|t| t.elapsed() < LOOK_AGAIN)
        {
            return;
        }
        self.looked = Some(Instant::now());
        self.looking = true;
        let paths: Vec<PathBuf> = list.iter().map(|s| s.path.clone()).collect();
        let (tx, ctx) = (self.there_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let looked = paths
                .into_iter()
                .map(|p| {
                    let there = p.try_exists().unwrap_or(false);
                    (p, there)
                })
                .collect();
            // The receiver only goes away with the window.
            let _ = tx.send(looked);
            ctx.request_repaint();
        });
    }

    /// What the search found, a row each, with the folder each is in.
    fn results_ui(
        &mut self,
        ui: &mut egui::Ui,
        shortcuts: Option<&[Shortcut]>,
        action: &mut Option<Action>,
    ) {
        ui.add(Label::new(RichText::new(self.search.status()).weak().size(11.0)).truncate());
        let height = ui.spacing().interact_size.y;
        let spacing = ui.spacing().item_spacing.y;
        let selected = self.selected.clone();
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("search results")
            .auto_shrink(false);
        if self.follow
            && let Some(index) = selected.as_deref().and_then(|s| self.search.position(s))
        {
            let (offset, shown) = self.scrolled;
            let top = index as f32 * (height + spacing);
            if top < offset {
                scroll = scroll.vertical_scroll_offset(top);
            } else if top + height > offset + shown {
                scroll = scroll.vertical_scroll_offset(top + height - shown);
            }
        }
        let search = &self.search;
        let output = scroll.show_rows(ui, height, search.len(), |ui, range| {
            for i in range {
                let Some(found) = search.result(i) else {
                    continue;
                };
                let path = &found.entry.path;
                let chosen = selected.as_ref() == Some(path);
                let response = result_row(ui, height, found, chosen)
                    .on_hover_ui(|ui| found_details(ui, found, search));
                if response.clicked() {
                    *action = Some(Action::Open(path.clone()));
                }
                if let Some(list) = shortcuts {
                    shortcut_menu(&response, list, path, false, action);
                }
            }
        });
        self.scrolled = (output.state.offset.y, output.inner_rect.height());
        self.follow = false;
    }

    fn tree_ui(
        &mut self,
        ui: &mut egui::Ui,
        shortcuts: Option<&[Shortcut]>,
        action: &mut Option<Action>,
    ) {
        if self.root.is_none() && cfg!(not(windows)) {
            ui.weak("Drop a folder or a file here");
        }
        let drive_list = drives();
        let (rows, unread) = self.listed(&drive_list);

        let height = ui.spacing().interact_size.y;
        let spacing = ui.spacing().item_spacing.y;
        let selected = self.selected.as_deref();
        let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
        let position = || {
            rows.iter().position(|row| {
                matches!(row, Row::Entry { entry, .. } if Some(entry.path.as_path()) == selected)
            })
        };
        let revealed = if self.reveal { position() } else { None };
        if let Some(index) = revealed {
            let above = ui.available_height() / 3.0;
            scroll =
                scroll.vertical_scroll_offset((index as f32 * (height + spacing) - above).max(0.0));
        } else if self.follow
            && let Some(index) = position()
        {
            let (offset, shown) = self.scrolled;
            let top = index as f32 * (height + spacing);
            if top < offset {
                scroll = scroll.vertical_scroll_offset(top);
            } else if top + height > offset + shown {
                scroll = scroll.vertical_scroll_offset(top + height - shown);
            }
        }
        let output = scroll.show_rows(ui, height, rows.len(), |ui, range| {
            for row in &rows[range] {
                let (depth, entry, open) = match row {
                    Row::Entry { depth, entry, open } => (*depth, *entry, *open),
                    Row::Loading { depth } => {
                        draw_row(ui, *depth, height, "Reading…", None, false, false);
                        continue;
                    }
                    Row::Unreadable { depth, why } => {
                        let (response, _) =
                            draw_row(ui, *depth, height, why.label, None, false, true);
                        response.on_hover_text(&why.why);
                        continue;
                    }
                };
                let toggle = entry.is_dir.then_some(open);
                let chosen = selected == Some(entry.path.as_path());
                let (response, elided) =
                    draw_row(ui, depth, height, &entry.name, toggle, chosen, false);
                let response = if elided {
                    response.on_hover_text(&entry.name)
                } else {
                    response
                };
                // egui counts a click anywhere just before as the first of
                // the run, and then the second click here as a third.
                if entry.is_dir && (response.double_clicked() || response.triple_clicked()) {
                    *action = Some(Action::Root(Some(entry.path.clone())));
                } else if response.clicked() {
                    *action = Some(if entry.is_dir {
                        Action::Toggle(entry.path.clone())
                    } else {
                        Action::Open(entry.path.clone())
                    });
                }
                if entry.is_dir || shortcuts.is_some() {
                    response.context_menu(|ui| {
                        if entry.is_dir && ui.button("Open as root").clicked() {
                            *action = Some(Action::Root(Some(entry.path.clone())));
                            ui.close();
                        }
                        if let Some(list) = shortcuts {
                            shortcut_item(ui, list, &entry.path, entry.is_dir, action);
                        }
                    });
                }
            }
        });
        self.scrolled = (output.state.offset.y, output.inner_rect.height());
        self.follow = false;

        // The file sits in the root folder, so once that is read it has
        // either been scrolled to or is not listed at all.
        let root_read = self
            .root
            .as_ref()
            .is_some_and(|root| matches!(self.listings.get(root), Some(Some(_))));
        if revealed.is_some() || root_read {
            self.reveal = false;
        }
        for dir in unread {
            self.read(dir, ui.ctx());
        }
    }
}

/// A right-click menu on `response` with [`shortcut_item`] in it.
fn shortcut_menu(
    response: &egui::Response,
    list: &[Shortcut],
    path: &Path,
    folder: bool,
    action: &mut Option<Action>,
) {
    response.context_menu(|ui| shortcut_item(ui, list, path, folder, action));
}

/// The menu item that keeps `path` among the shortcuts, or takes it out.
fn shortcut_item(
    ui: &mut egui::Ui,
    list: &[Shortcut],
    path: &Path,
    folder: bool,
    action: &mut Option<Action>,
) {
    let kept = list.iter().any(|s| s.path == path);
    let label = if kept {
        "Take out of the shortcuts"
    } else {
        "Add to the shortcuts"
    };
    if ui.button(label).clicked() {
        *action = Some(Action::Shortcut(Shortcut {
            path: path.to_owned(),
            folder,
        }));
        ui.close();
    }
}

/// A recording the search found: its name, and after it the folder it is
/// in, both cut short rather than widening the panel.
fn result_row(ui: &mut egui::Ui, height: f32, found: &Found, selected: bool) -> egui::Response {
    let name = &found.entry.name;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    response
        .widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, selected, name));
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.style().interact_selectable(&response, selected);
    if selected || response.hovered() {
        ui.painter()
            .rect_filled(rect, visuals.corner_radius, visuals.weak_bg_fill);
    }
    let pad = ui.spacing().item_spacing.x / 2.0;
    let width = rect.width() - 2.0 * pad;
    let share = if found.folder.is_empty() { 1.0 } else { 0.65 };
    let galley = WidgetText::from(name.as_str()).into_galley(
        ui,
        Some(TextWrapMode::Truncate),
        (width * share).max(0.0),
        TextStyle::Button,
    );
    let wide = galley.size().x;
    let top = rect.center().y - galley.size().y / 2.0;
    ui.painter().galley(
        Pos2::new(rect.left() + pad, top),
        galley,
        visuals.text_color(),
    );
    let room = width - wide - 8.0;
    if !found.folder.is_empty() && room > 16.0 {
        let folder = WidgetText::from(RichText::new(&found.folder).weak()).into_galley(
            ui,
            Some(TextWrapMode::Truncate),
            room,
            TextStyle::Small,
        );
        let top = rect.center().y - folder.size().y / 2.0;
        let left = rect.left() + pad + wide + 8.0;
        ui.painter()
            .galley(Pos2::new(left, top), folder, visuals.text_color());
    }
    response
}

/// What a recording the search found is, on hover: where it is, how long,
/// when by the date searched, and which of its fields matched.
fn found_details(ui: &mut egui::Ui, found: &Found, search: &Search) {
    let entry = &found.entry;
    ui.label(entry.path.display().to_string());
    let mut facts = Vec::new();
    if let Some(seconds) = entry.seconds {
        facts.push(views::clock(seconds));
    }
    if let Some(stamp) = entry.date(search.date()) {
        facts.push(format!("{} {}", search.date().name(), stamp.text()));
    }
    if !facts.is_empty() {
        ui.label(RichText::new(facts.join("  ·  ")).weak());
    }
    let matched = found.matched_in(search.words());
    if !matched.is_empty() {
        ui.label(RichText::new(format!("Matched in {}", matched.join(", "))).weak());
    }
    if let Some(why) = &entry.error {
        ui.label(RichText::new(format!("Not readable: {why}")).color(views::CURSOR));
    }
}

/// One row: indent, a triangle for folders, and the name cut short with an
/// ellipsis rather than widening the panel, `dim` while what it names is
/// not there. Says whether the name was cut short.
fn draw_row(
    ui: &mut egui::Ui,
    depth: usize,
    height: f32,
    name: &str,
    open: Option<bool>,
    selected: bool,
    dim: bool,
) -> (egui::Response, bool) {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    response
        .widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, selected, name));
    if !ui.is_rect_visible(rect) {
        return (response, false);
    }
    let visuals = ui.style().interact_selectable(&response, selected);
    if selected || response.hovered() {
        ui.painter()
            .rect_filled(rect, visuals.corner_radius, visuals.weak_bg_fill);
    }
    let indent = ui.spacing().icon_width;
    let icon = Rect::from_min_size(
        Pos2::new(rect.left() + depth as f32 * indent, rect.top()),
        Vec2::new(indent, height),
    );
    if let Some(open) = open {
        paint_triangle(ui.painter(), icon, open, visuals.fg_stroke.color);
    }
    let left = icon.right() + ui.spacing().item_spacing.x / 2.0;
    let galley = WidgetText::from(name).into_galley(
        ui,
        Some(TextWrapMode::Truncate),
        (rect.right() - left).max(0.0),
        TextStyle::Button,
    );
    let top = rect.center().y - galley.size().y / 2.0;
    let elided = galley.elided;
    let color = if dim {
        ui.visuals().weak_text_color()
    } else {
        visuals.text_color()
    };
    ui.painter().galley(Pos2::new(left, top), galley, color);
    (response, elided)
}

fn paint_triangle(painter: &egui::Painter, rect: Rect, open: bool, color: Color32) {
    let (c, r) = (rect.center(), rect.width().min(rect.height()) * 0.22);
    let points = if open {
        vec![
            c + Vec2::new(-r, -r * 0.6),
            c + Vec2::new(r, -r * 0.6),
            c + Vec2::new(0.0, r * 0.8),
        ]
    } else {
        vec![
            c + Vec2::new(-r * 0.6, -r),
            c + Vec2::new(-r * 0.6, r),
            c + Vec2::new(r * 0.8, 0.0),
        ]
    };
    painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_swipe_right_counts_once_and_scrolling_never() {
        let wheel = |phase, x: f32, y: f32| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(x, y),
            phase,
            modifiers: egui::Modifiers::NONE,
        };
        use egui::TouchPhase::{End, Move, Start};
        let natural = || true;
        let mut swipe = Swipe::default();
        assert!(!swipe.right(
            &[wheel(Start, 0.0, 0.0), wheel(Move, 60.0, 4.0)],
            1.0,
            natural
        ));
        assert!(swipe.right(
            &[wheel(Move, 70.0, -3.0), wheel(End, 0.0, 0.0)],
            1.05,
            natural
        ));
        // The slide macOS goes on with as the fingers lift is not a second.
        assert!(!swipe.right(
            &[wheel(Start, 0.0, 0.0), wheel(Move, 400.0, 0.0)],
            1.08,
            natural
        ));
        assert!(!swipe.right(&[wheel(End, 0.0, 0.0)], 1.6, natural));
        // A swipe of its own counts again.
        assert!(swipe.right(
            &[
                wheel(Start, 0.0, 0.0),
                wheel(Move, 150.0, 0.0),
                wheel(End, 0.0, 0.0)
            ],
            3.0,
            natural
        ));
        // Scrolling the list, swiping left, or the fingers lifting short.
        for moved in [(20.0, 300.0), (-150.0, 0.0), (60.0, 0.0)] {
            assert!(!swipe.right(
                &[
                    wheel(Start, 0.0, 0.0),
                    wheel(Move, moved.0, moved.1),
                    wheel(End, 0.0, 0.0)
                ],
                5.0,
                natural
            ));
        }
        // A mouse wheel's notches are no swipe, however far they go.
        let notches = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: Vec2::new(500.0, 0.0),
            phase: Move,
            modifiers: egui::Modifiers::NONE,
        };
        assert!(!swipe.right(&[notches], 9.0, natural));
        // With natural scrolling off, the fingers going right move what
        // scrolls to the left.
        let swiped = |x| {
            Swipe::default().right(
                &[
                    wheel(Start, 0.0, 0.0),
                    wheel(Move, x, 0.0),
                    wheel(End, 0.0, 0.0),
                ],
                1.0,
                || false,
            )
        };
        assert!(swiped(-150.0) && !swiped(150.0));
    }

    /// Two clicks on the folder `name`, the second coming `apart` seconds
    /// after the first, with egui told double-clicks come within `system`
    /// seconds: where the explorer is rooted after.
    fn double_click(name: &str, apart: f64, system: f64) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "soundcheck-double-{}-{}",
            std::process::id(),
            (system * 1000.0) as u32
        ));
        std::fs::create_dir_all(dir.join(name)).unwrap();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        ctx.options_mut(|o| o.input_options.max_double_click_delay = system);
        let mut explorer = Explorer::default();
        explorer.set_root(&dir);
        let frame = |explorer: &mut Explorer, time: f64, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                time: Some(time),
                events,
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                explorer.ui(
                    ui,
                    Parts {
                        shortcuts: None,
                        search: false,
                    },
                );
            });
            output.textures_delta.clear();
            output
        };
        // Its row, once the folder around it is read.
        let mut time = 0.0;
        let at = loop {
            let output = frame(&mut explorer, time, Vec::new());
            let row = output.platform_output.accesskit_update.and_then(|update| {
                update
                    .nodes
                    .into_iter()
                    .find(|(_, node)| node.label() == Some(name))
                    .and_then(|(_, node)| node.bounds())
            });
            if let Some(b) = row {
                break egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32);
            }
            assert!(time < 10.0, "the folder never showed");
            time += 0.01;
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(
            &mut explorer,
            time + 1.0,
            vec![egui::Event::PointerMoved(at), button(true)],
        );
        frame(&mut explorer, time + 1.05, vec![button(false)]);
        frame(&mut explorer, time + 1.0 + apart, vec![button(true)]);
        frame(&mut explorer, time + 1.05 + apart, vec![button(false)]);
        let root = explorer.root().map(Path::to_owned);
        std::fs::remove_dir_all(&dir).unwrap();
        root
    }

    #[test]
    fn a_folder_double_clicked_at_the_pace_the_system_takes_opens_as_the_root() {
        // 0.35 s apart, slower than egui's own 0.3 s: two single clicks to
        // it, which opened the folder and closed it again.
        let opened = double_click("Day 1", 0.35, 0.5).unwrap();
        assert!(opened.ends_with("Day 1"), "{opened:?}");
        let not = double_click("Day 2", 0.35, 0.3).unwrap();
        assert!(!not.ends_with("Day 2"), "{not:?}");
    }

    #[test]
    fn numbers_sort_by_value() {
        let mut names = vec!["take 10", "take 9", "take 1", "b", "a2", "a10", "010", "9"];
        names.sort_by(|a, b| natural(a, b));
        assert_eq!(
            names,
            ["9", "010", "a2", "a10", "b", "take 1", "take 9", "take 10"]
        );
    }

    #[test]
    fn the_arrows_walk_the_rows_and_hand_back_the_files() {
        let dir = std::env::temp_dir().join(format!("soundcheck-step-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Day 1")).unwrap();
        for name in ["a.wav", "b.wav"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let mut explorer = Explorer::default();
        explorer.set_root(&dir);
        explorer.listings.insert(dir.clone(), Some(list(&dir)));
        // From nothing selected, down is the top row: a folder, selected at once.
        assert_eq!(explorer.step(true), None);
        assert_eq!(explorer.selected, Some(dir.join("Day 1")));
        // A file comes back to be opened, and is selected once it is.
        assert_eq!(explorer.step(true), Some(dir.join("a.wav")));
        explorer.selected = Some(dir.join("a.wav"));
        assert_eq!(explorer.step(true), Some(dir.join("b.wav")));
        explorer.selected = Some(dir.join("b.wav"));
        // Neither end goes any further.
        assert_eq!(explorer.step(true), None);
        assert_eq!(explorer.selected, Some(dir.join("b.wav")));
        assert_eq!(explorer.step(false), Some(dir.join("a.wav")));
        explorer.selected = Some(dir.join("Day 1"));
        assert_eq!(explorer.step(false), None);
        assert_eq!(explorer.selected, Some(dir.join("Day 1")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folders_come_first_then_audio_in_order() {
        let dir = std::env::temp_dir().join(format!("soundcheck-list-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Day 10")).unwrap();
        std::fs::create_dir_all(dir.join("Day 9")).unwrap();
        for name in ["ZOOM0010.WAV", "zoom0002.wav", "notes.txt", ".hidden.wav"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let names: Vec<String> = list(&dir)
            .ok()
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(names, ["Day 9", "Day 10", "zoom0002.wav", "ZOOM0010.WAV"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_read_says_so_rather_than_showing_empty() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("soundcheck-shut-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("take 1.wav"), b"").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let listed = list(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let Err(why) = listed else {
            panic!("a folder shut to everyone listed as read");
        };
        assert_eq!(why.label, "Can't read this folder");
        let mut explorer = Explorer::default();
        explorer.set_root(&dir);
        explorer.listings.insert(dir.clone(), Some(Err(why)));
        let (rows, unread) = explorer.listed(&[]);
        assert!(matches!(
            rows.as_slice(),
            [Row::Unreadable { depth: 0, .. }]
        ));
        assert!(unread.is_empty(), "it is not read again until reloaded");
    }

    #[test]
    fn a_folder_kept_out_is_read_again_on_coming_back_to_the_window() {
        let dir = std::env::temp_dir().join(format!("soundcheck-back-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut explorer = Explorer::default();
        explorer.set_root(&dir);
        let kept_out = Unreadable {
            label: "Not allowed to read this folder",
            why: String::new(),
        };
        explorer.listings.insert(dir.clone(), Some(Err(kept_out)));
        let ctx = egui::Context::default();
        for focused in [false, true] {
            let input = egui::RawInput {
                focused,
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                explorer.ui(
                    ui,
                    Parts {
                        shortcuts: None,
                        search: false,
                    },
                );
            });
            // No window to hand the fonts' texture to.
            output.textures_delta.clear();
            if !focused {
                assert!(matches!(explorer.listings.get(&dir), Some(Some(Err(_)))));
            }
        }
        assert!(
            !matches!(explorer.listings.get(&dir), Some(Some(Err(_)))),
            "it is read again"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
