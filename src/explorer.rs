//! The folder tree on the left. Folders are read on a background thread,
//! and only the rows in view are drawn, so neither a slow card reader nor a
//! folder of a hundred thousand recordings holds up the window.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use eframe::egui::{
    self, Color32, Label, Pos2, Rect, RichText, Sense, Shape, Stroke, TextStyle, TextWrapMode,
    Vec2, WidgetInfo, WidgetText, WidgetType,
};

const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", "bwf", "rf64", "flac", "mp3", "m4a", "aac", "ogg", "oga", "aif", "aiff", "aifc",
    "caf", "mka", "webm",
];

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

/// Folders and audio files directly inside `dir`: folders first, then in
/// file-manager order, dotfiles hidden. An unreadable folder lists as empty.
fn list(dir: &Path) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
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
    listed.into_iter().map(|(_, entry)| entry).collect()
}

/// Compares names the way file managers do: a run of digits by its value,
/// so `take 9` comes before `take 10`.
fn natural(a: &str, b: &str) -> Ordering {
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
/// the list of drives.
#[cfg(windows)]
fn drives() -> Vec<Entry> {
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

enum Row<'a> {
    Entry {
        depth: usize,
        entry: &'a Entry,
        open: bool,
    },
    Loading {
        depth: usize,
    },
}

enum Action {
    Open(PathBuf),
    Toggle(PathBuf),
    Root(Option<PathBuf>),
}

pub struct Explorer {
    /// `None` is the list of drives on Windows, and nothing elsewhere.
    root: Option<PathBuf>,
    /// Folders read so far; `None` while a read is under way.
    listings: HashMap<PathBuf, Option<Vec<Entry>>>,
    open: HashSet<PathBuf>,
    pub selected: Option<PathBuf>,
    /// Scroll the selected file into view once its folder has been read.
    reveal: bool,
    tx: mpsc::Sender<(PathBuf, Vec<Entry>)>,
    rx: mpsc::Receiver<(PathBuf, Vec<Entry>)>,
}

impl Default for Explorer {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            root: None,
            listings: HashMap::new(),
            open: HashSet::new(),
            selected: None,
            reveal: false,
            tx,
            rx,
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

    fn rows<'a>(
        &'a self,
        dir: &Path,
        depth: usize,
        rows: &mut Vec<Row<'a>>,
        unread: &mut Vec<PathBuf>,
    ) {
        let Some(Some(entries)) = self.listings.get(dir) else {
            if !self.listings.contains_key(dir) {
                unread.push(dir.to_owned());
            }
            rows.push(Row::Loading { depth });
            return;
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

    /// Draws the tree and returns the audio file clicked this frame, if any.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        for (dir, entries) in self.rx.try_iter() {
            // A folder closed while it was being read no longer wants it.
            if let Some(slot @ None) = self.listings.get_mut(&dir) {
                *slot = Some(entries);
            }
        }

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
            if ui.small_button("🔄").on_hover_text("Reload").clicked() {
                self.listings.clear();
            }
            let title = self.root.as_ref().map_or("Drives".into(), |r| {
                r.file_name().map_or_else(
                    || r.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                )
            });
            ui.add(Label::new(RichText::new(title).strong()).truncate());
        });
        ui.separator();

        let mut rows = Vec::new();
        let mut unread = Vec::new();
        #[cfg(windows)]
        let drive_list = drives();
        match &self.root {
            Some(root) => self.rows(root, 0, &mut rows, &mut unread),
            #[cfg(windows)]
            None => rows.extend(drive_list.iter().map(|entry| Row::Entry {
                depth: 0,
                open: self.open.contains(&entry.path),
                entry,
            })),
            #[cfg(not(windows))]
            None => {
                ui.weak("Drop a folder or a file here");
            }
        }

        let height = ui.spacing().interact_size.y;
        let spacing = ui.spacing().item_spacing.y;
        let selected = self.selected.as_deref();
        let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
        let revealed = if self.reveal {
            rows.iter().position(|row| {
                matches!(row, Row::Entry { entry, .. } if Some(entry.path.as_path()) == selected)
            })
        } else {
            None
        };
        if let Some(index) = revealed {
            let above = ui.available_height() / 3.0;
            scroll =
                scroll.vertical_scroll_offset((index as f32 * (height + spacing) - above).max(0.0));
        }
        scroll.show_rows(ui, height, rows.len(), |ui, range| {
            for row in &rows[range] {
                let (depth, entry, open) = match row {
                    Row::Entry { depth, entry, open } => (*depth, *entry, *open),
                    Row::Loading { depth } => {
                        draw_row(ui, *depth, height, "Reading…", None, false);
                        continue;
                    }
                };
                let toggle = entry.is_dir.then_some(open);
                let chosen = selected == Some(entry.path.as_path());
                let response = draw_row(ui, depth, height, &entry.name, toggle, chosen);
                if response.clicked() {
                    action = Some(if entry.is_dir {
                        Action::Toggle(entry.path.clone())
                    } else {
                        Action::Open(entry.path.clone())
                    });
                }
                if entry.is_dir {
                    response.context_menu(|ui| {
                        if ui.button("Open as root").clicked() {
                            action = Some(Action::Root(Some(entry.path.clone())));
                            ui.close();
                        }
                    });
                }
            }
        });

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
        match action? {
            Action::Open(file) => return Some(file),
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
        }
        None
    }
}

/// One row: indent, a triangle for folders, and the name cut short with an
/// ellipsis rather than widening the panel; the full name shows on hover.
fn draw_row(
    ui: &mut egui::Ui,
    depth: usize,
    height: f32,
    name: &str,
    open: Option<bool>,
    selected: bool,
) -> egui::Response {
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
    ui.painter()
        .galley(Pos2::new(left, top), galley, visuals.text_color());
    if elided {
        response.on_hover_text(name)
    } else {
        response
    }
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
    fn numbers_sort_by_value() {
        let mut names = vec!["take 10", "take 9", "take 1", "b", "a2", "a10", "010", "9"];
        names.sort_by(|a, b| natural(a, b));
        assert_eq!(
            names,
            ["9", "010", "a2", "a10", "b", "take 1", "take 9", "take 10"]
        );
    }

    #[test]
    fn folders_come_first_then_audio_in_order() {
        let dir = std::env::temp_dir().join(format!("soundcheck-list-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Day 10")).unwrap();
        std::fs::create_dir_all(dir.join("Day 9")).unwrap();
        for name in ["ZOOM0010.WAV", "zoom0002.wav", "notes.txt", ".hidden.wav"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let names: Vec<String> = list(&dir).into_iter().map(|e| e.name).collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(names, ["Day 9", "Day 10", "zoom0002.wav", "ZOOM0010.WAV"]);
    }
}
