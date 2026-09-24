//! The folder tree on the left, read lazily one directory at a time.

use std::path::{Path, PathBuf};

use eframe::egui;

const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", "bwf", "rf64", "flac", "mp3", "m4a", "aac", "ogg", "oga", "aif", "aiff", "aifc",
    "caf", "mka", "webm",
];

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

struct Node {
    path: PathBuf,
    name: String,
    is_dir: bool,
    children: Option<Vec<Node>>,
}

impl Node {
    fn new(path: PathBuf, is_dir: bool) -> Self {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        Self {
            path,
            name,
            is_dir,
            children: None,
        }
    }
}

/// Folders and audio files directly inside `dir`: folders first, then by
/// name ignoring case, dotfiles hidden. An unreadable folder lists as empty.
fn list(dir: &Path) -> Vec<Node> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut nodes: Vec<Node> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let path = e.path();
            let is_dir = path.is_dir();
            (is_dir || is_audio(&path)).then(|| Node::new(path, is_dir))
        })
        .collect();
    nodes.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    nodes
}

fn draw(
    node: &mut Node,
    ui: &mut egui::Ui,
    selected: Option<&Path>,
    clicked: &mut Option<PathBuf>,
) {
    if node.is_dir {
        let Node {
            path,
            name,
            children,
            ..
        } = node;
        egui::CollapsingHeader::new(name.as_str())
            .id_salt(&*path)
            .show(ui, |ui| {
                for child in children.get_or_insert_with(|| list(path)) {
                    draw(child, ui, selected, clicked);
                }
            });
    } else if ui
        .selectable_label(selected == Some(node.path.as_path()), node.name.as_str())
        .clicked()
    {
        *clicked = Some(node.path.clone());
    }
}

#[derive(Default)]
pub struct Explorer {
    root: Option<Node>,
    pub selected: Option<PathBuf>,
}

impl Explorer {
    pub fn set_root(&mut self, dir: &Path) {
        if self.root.as_ref().map(|r| r.path.as_path()) != Some(dir) {
            self.root = Some(Node::new(dir.to_owned(), true));
        }
    }

    /// Draws the tree and returns the audio file clicked this frame, if any.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        let Some(root) = &mut self.root else {
            ui.weak("Drop a folder or a file here");
            return None;
        };
        let mut up = None;
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.strong(root.name.as_str());
            if let Some(parent) = root.path.parent()
                && ui
                    .small_button("⬆")
                    .on_hover_text(parent.display().to_string())
                    .clicked()
            {
                up = Some(parent.to_owned());
            }
        });
        ui.separator();

        let mut clicked = None;
        let selected = self.selected.as_deref();
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                let Node { path, children, .. } = root;
                for node in children.get_or_insert_with(|| list(path)) {
                    draw(node, ui, selected, &mut clicked);
                }
            });
        if let Some(parent) = up {
            self.set_root(&parent);
        }
        clicked
    }
}
