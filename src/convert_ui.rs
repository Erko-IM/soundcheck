//! The Convert view: the file open, or files chosen in the explorer's
//! folder, to another format. Each file lists what its new one will be
//! like before anything is made: its name, and any rate, depth or level
//! that changes, and what the format has no place for. Files convert a few
//! at a time on threads of their own, and Stop leaves those done as they
//! are and the rest as they were.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::SystemTime;

use eframe::egui::{self, Color32, RichText, Ui, Vec2};
use serde::{Deserialize, Serialize};

use crate::convert::{self, Depth, Format, Header, Mp3, Plan, Target, Vorbis};
use crate::explorer::{is_audio, natural};
use crate::views;

/// Files converting at once. Each FLAC already shares its blocks out over
/// every thread, and the other encoders are one thread each.
const AT_ONCE: usize = 3;

/// What the view converts, kept between runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Setup {
    pub target: Target,
    /// The files chosen in the explorer's folder, rather than the file open.
    pub folder: bool,
}

/// A file listed, with what converting it needs to know, once read.
struct Entry {
    path: PathBuf,
    header: Result<Header, String>,
}

fn entry(path: PathBuf) -> Entry {
    Entry {
        header: convert::header(&path),
        path,
    }
}

/// How loud a file that needs its level checked peaks.
enum Level {
    Reading,
    Peak(f32),
    Failed(String),
}

/// A file as the list shows it: its plan, or why there is none.
type Row = (PathBuf, Result<Plan, String>);

/// The files converting, and how far each is, in thousandths.
type UnderWay = Arc<Mutex<Vec<(PathBuf, Arc<AtomicU32>)>>>;

/// What a plan was made from: the setup, the file open, the folder's
/// listing, the choice, and how many results were in.
type Planned = (Setup, Option<PathBuf>, u64, Vec<bool>, usize);

/// A conversion under way.
struct Job {
    cancel: Arc<AtomicBool>,
    done: mpsc::Receiver<(PathBuf, Result<PathBuf, String>)>,
    under_way: UnderWay,
    total: usize,
    finished: usize,
    workers: Vec<JoinHandle<()>>,
}

impl Drop for Job {
    /// Stops the files still converting, which then take away their
    /// unfinished copies, and waits for them.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

enum Read {
    Folder(u64, Vec<Entry>),
    Open(Entry),
}

pub struct Converter {
    pub setup: Setup,
    folder: Option<PathBuf>,
    files: Vec<Entry>,
    chosen: Vec<bool>,
    open: Option<Entry>,
    /// The open file whose header is being read.
    opening: Option<PathBuf>,
    listing: u64,
    reading: bool,
    tx: mpsc::Sender<Read>,
    rx: mpsc::Receiver<Read>,
    levels: HashMap<PathBuf, (Option<SystemTime>, Level)>,
    level_tx: mpsc::Sender<(PathBuf, Result<f32, String>)>,
    level_rx: mpsc::Receiver<(PathBuf, Result<f32, String>)>,
    checking: bool,
    rows: Vec<Row>,
    planned_from: Option<Planned>,
    job: Option<Job>,
    results: HashMap<PathBuf, Result<PathBuf, String>>,
    message: Option<(String, bool)>,
    /// Folders new files went into since the window last asked.
    grown: HashSet<PathBuf>,
}

impl Default for Converter {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        let (level_tx, level_rx) = mpsc::channel();
        Self {
            setup: Setup::default(),
            folder: None,
            files: Vec::new(),
            chosen: Vec::new(),
            open: None,
            opening: None,
            listing: 0,
            reading: false,
            tx,
            rx,
            levels: HashMap::new(),
            level_tx,
            level_rx,
            checking: false,
            rows: Vec::new(),
            planned_from: None,
            job: None,
            results: HashMap::new(),
            message: None,
            grown: HashSet::new(),
        }
    }
}

impl Converter {
    pub fn busy(&self) -> bool {
        self.job.is_some() || self.reading || self.checking || self.opening.is_some()
    }

    /// Lists `folder` again whenever it changes, reads the file `open`
    /// when it changes, and takes in whatever the threads have finished.
    /// Returns the folders new files went into since the last call.
    pub fn follow(
        &mut self,
        folder: Option<&Path>,
        open: Option<&Path>,
        ctx: &egui::Context,
    ) -> Vec<PathBuf> {
        for read in self.rx.try_iter().collect::<Vec<_>>() {
            match read {
                Read::Folder(listing, files) if listing == self.listing => {
                    // A file chosen before stays chosen; a file new to the
                    // folder, as one just converted, does not start chosen,
                    // except on a first listing.
                    let first = self.files.is_empty();
                    let was: HashMap<PathBuf, bool> = self
                        .files
                        .iter()
                        .map(|e| e.path.clone())
                        .zip(self.chosen.iter().copied())
                        .collect();
                    self.chosen = files
                        .iter()
                        .map(|e| was.get(&e.path).copied().unwrap_or(first))
                        .collect();
                    self.files = files;
                    self.reading = false;
                    self.planned_from = None;
                }
                Read::Folder(..) => {}
                Read::Open(entry) => {
                    if self.opening.as_deref() == Some(&entry.path) {
                        self.opening = None;
                        self.open = Some(entry);
                        self.planned_from = None;
                    }
                }
            }
        }
        for (path, level) in self.level_rx.try_iter() {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            let level = match level {
                Ok(peak) => Level::Peak(peak),
                Err(e) => Level::Failed(e),
            };
            self.levels.insert(path, (modified, level));
        }
        if self.checking
            && !self
                .levels
                .values()
                .any(|(_, l)| matches!(l, Level::Reading))
        {
            self.checking = false;
        }
        if self.folder.as_deref() != folder {
            self.folder = folder.map(Path::to_owned);
            self.files.clear();
            self.chosen.clear();
            self.refresh(ctx);
        }
        let open_now = self.open.as_ref().map(|e| e.path.as_path());
        if open != open_now && open != self.opening.as_deref() {
            self.open = None;
            self.opening = open.map(Path::to_owned);
            if let Some(path) = open {
                let (tx, ctx, path) = (self.tx.clone(), ctx.clone(), path.to_owned());
                std::thread::spawn(move || {
                    let _ = tx.send(Read::Open(entry(path)));
                    ctx.request_repaint();
                });
            }
        }
        self.take_finished(ctx);
        self.grown.drain().collect()
    }

    /// Reads the folder again.
    fn refresh(&mut self, ctx: &egui::Context) {
        self.listing += 1;
        self.planned_from = None;
        let Some(folder) = self.folder.clone() else {
            return;
        };
        self.reading = true;
        let (tx, ctx, listing) = (self.tx.clone(), ctx.clone(), self.listing);
        std::thread::spawn(move || {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(&folder)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            p.is_file()
                                && is_audio(p)
                                && !p
                                    .file_name()
                                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                        })
                        .collect()
                })
                .unwrap_or_default();
            paths.sort_by(|a, b| {
                natural(
                    &a.to_string_lossy().to_lowercase(),
                    &b.to_string_lossy().to_lowercase(),
                )
            });
            let files = paths.into_iter().map(entry).collect();
            let _ = tx.send(Read::Folder(listing, files));
            ctx.request_repaint();
        });
    }

    fn take_finished(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.job else {
            return;
        };
        for (from, result) in job.done.try_iter() {
            job.finished += 1;
            if let Ok(to) = &result
                && let Some(dir) = to.parent()
            {
                self.grown.insert(dir.to_owned());
            }
            self.results.insert(from, result);
        }
        if job.finished < job.total {
            return;
        }
        let cancelled = job.cancel.load(Ordering::Relaxed);
        self.job = None;
        let made = self.results.values().filter(|r| r.is_ok()).count();
        let failed = self.results.values().filter(|r| r.is_err()).count();
        let files = |n: usize| if n == 1 { "file" } else { "files" };
        let mut text = format!("{made} {} converted", files(made));
        if failed > 0 {
            text.push_str(&format!(", {failed} not: hover the red ones for why"));
        }
        if cancelled {
            text.push_str(", then stopped");
        }
        self.message = Some((text, failed > 0));
        self.refresh(ctx);
    }

    /// The files the view is about: the file open, or those chosen in the
    /// folder.
    fn sources(&self) -> Vec<&Entry> {
        if self.setup.folder {
            self.files
                .iter()
                .zip(&self.chosen)
                .filter(|(_, c)| **c)
                .map(|(e, _)| e)
                .collect()
        } else {
            self.open.iter().collect()
        }
    }

    /// Plans again whenever the setup, the files or the choice changed, and
    /// reads the levels the new plans need.
    fn replan(&mut self, ctx: &egui::Context) {
        let key = (
            self.setup,
            self.open.as_ref().map(|e| e.path.clone()),
            self.listing,
            self.chosen.clone(),
            self.results.len(),
        );
        if self.planned_from.as_ref() == Some(&key) {
            return;
        }
        let target = self.setup.target;
        let mut taken = HashSet::new();
        let rows: Vec<Row> = self
            .sources()
            .into_iter()
            .map(|e| {
                let plan = e.header.clone().and_then(|header| {
                    let (shape, changes) = convert::shape(&header, &target)?;
                    let to = convert::path_for(&e.path, target.format, &taken);
                    taken.insert(to.clone());
                    Ok(Plan {
                        from: e.path.clone(),
                        to,
                        shape,
                        changes,
                    })
                });
                (e.path.clone(), plan)
            })
            .collect();
        self.rows = rows;
        self.planned_from = Some(key);
        self.check_levels(ctx);
    }

    /// Reads the level of each file listed that needs it and has none yet,
    /// on a thread of its own.
    fn check_levels(&mut self, ctx: &egui::Context) {
        let wanted: Vec<(PathBuf, Option<SystemTime>)> = self
            .rows
            .iter()
            .filter_map(|(path, plan)| plan.as_ref().ok().map(|p| (path, p)))
            .filter(|(_, plan)| plan.shape.needs_peak())
            .map(|(path, _)| {
                let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                (path.clone(), modified)
            })
            .filter(|(path, modified)| {
                self.levels
                    .get(path)
                    .is_none_or(|(when, _)| when != modified)
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        for (path, modified) in &wanted {
            self.levels
                .insert(path.clone(), (*modified, Level::Reading));
        }
        self.checking = true;
        let (tx, ctx) = (self.level_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            for (path, _) in wanted {
                let peak = convert::peak(&path, &AtomicBool::new(false));
                let _ = tx.send((path, peak));
                ctx.request_repaint();
            }
        });
    }

    /// The gain a plan's file is lowered by, where its level is known.
    fn gain(&self, plan: &Plan) -> Option<Option<f32>> {
        if !plan.shape.needs_peak() {
            return Some(None);
        }
        match self.levels.get(&plan.from) {
            Some((_, Level::Peak(peak))) => Some(convert::fit(*peak)),
            _ => None,
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let target = self.setup.target;
        let queue: VecDeque<(Plan, Option<Option<f32>>)> = self
            .rows
            .iter()
            .filter_map(|(_, plan)| plan.as_ref().ok())
            .map(|plan| (plan.clone(), self.gain(plan)))
            .collect();
        if queue.is_empty() {
            return;
        }
        for (plan, _) in &queue {
            self.results.remove(&plan.from);
        }
        self.message = None;
        let total = queue.len();
        let queue = Arc::new(Mutex::new(queue));
        let cancel = Arc::new(AtomicBool::new(false));
        let under_way = Arc::new(Mutex::new(Vec::new()));
        let (tx, done) = mpsc::channel();
        let workers = (0..AT_ONCE.min(total))
            .map(|_| {
                let (queue, cancel, under_way, tx, ctx) = (
                    Arc::clone(&queue),
                    Arc::clone(&cancel),
                    Arc::clone(&under_way),
                    tx.clone(),
                    ctx.clone(),
                );
                std::thread::spawn(move || {
                    loop {
                        let next = queue
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .pop_front();
                        let Some((plan, gain)) = next else {
                            return;
                        };
                        if cancel.load(Ordering::Relaxed) {
                            let _ = tx.send((plan.from, Err("stopped before it started".into())));
                            continue;
                        }
                        let progress = Arc::new(AtomicU32::new(0));
                        under_way
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push((plan.from.clone(), Arc::clone(&progress)));
                        let result = std::panic::catch_unwind(|| {
                            // A level not read yet is read now.
                            let gain = match gain {
                                Some(gain) => gain,
                                None => convert::fit(convert::peak(&plan.from, &cancel)?),
                            };
                            convert::convert(&plan, &target, gain, &cancel, &progress)
                        })
                        .unwrap_or_else(|_| Err("converting crashed; nothing was made".into()));
                        under_way
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .retain(|(p, _)| *p != plan.from);
                        let _ = tx.send((plan.from, result));
                        ctx.request_repaint();
                    }
                })
            })
            .collect();
        self.job = Some(Job {
            cancel,
            done,
            under_way,
            total,
            finished: 0,
            workers,
        });
    }

    /// The view, beside the others.
    pub fn ui(&mut self, ui: &mut Ui, open: Option<&Path>, dirty: bool, locked: bool) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.strong("Convert");
            let about = if self.setup.folder {
                format!("files in {}", folder_name(self.folder.as_deref()))
            } else {
                "the file open".to_owned()
            };
            ui.weak(about);
        });
        ui.add_space(4.0);
        let busy = self.job.is_some();
        ui.add_enabled_ui(!busy, |ui| {
            egui::Grid::new("convert-setup")
                .num_columns(2)
                .spacing([6.0, 4.0])
                .show(ui, |ui| {
                    ui.label("From");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.setup.folder, false, "The file open");
                        ui.radio_value(&mut self.setup.folder, true, "Files in the folder")
                            .on_hover_text("The audio files in the folder the explorer shows; tick the ones to convert");
                    });
                    ui.end_row();
                    ui.label("To");
                    ui.horizontal(|ui| target_boxes(ui, &mut self.setup.target));
                    ui.end_row();
                });
        });
        self.replan(&ctx);
        ui.separator();
        let ready = self.rows.iter().filter(|(_, p)| p.is_ok()).count();
        ui.horizontal(|ui| {
            if busy {
                if ui.button("Stop").on_hover_text("Files done stay; the one converting is taken away, and the rest are left as they were").clicked()
                    && let Some(job) = &self.job
                {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            } else {
                let label = format!("Convert {ready}");
                let why = if locked {
                    Some("Not while a save is under way")
                } else if ready == 0 {
                    Some("Nothing to convert")
                } else {
                    None
                };
                let button = ui.add_enabled(why.is_none(), egui::Button::new(label));
                let button = match why {
                    Some(why) => button.on_disabled_hover_text(why),
                    None => button,
                };
                if button.clicked() {
                    self.start(&ctx);
                }
            }
            self.status(ui);
        });
        if let Some((message, error)) = &self.message {
            let color = if *error { views::CURSOR } else { views::AXIS };
            ui.label(RichText::new(message).color(color));
        }
        if !self.setup.folder && dirty && open.is_some() {
            ui.label(
                RichText::new(
                    "Its metadata has changes not saved yet: save first to take them along",
                )
                .color(views::MARK),
            );
        }
        if self.setup.folder {
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal(|ui| {
                    if ui.small_button("All").clicked() {
                        self.chosen.fill(true);
                    }
                    if ui.small_button("None").clicked() {
                        self.chosen.fill(false);
                    }
                    if self.reading {
                        ui.spinner();
                        ui.weak("Reading the folder…");
                    }
                });
            });
            self.list_folder(ui, busy);
        } else {
            match (&self.open, open) {
                (_, None) => {
                    ui.weak("Open a file to convert it, or pick Files in the folder");
                }
                (None, Some(_)) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("Reading the file…");
                    });
                }
                (Some(_), Some(_)) => {
                    if let Some((path, plan)) = self.rows.first() {
                        let row = self.row_text(path, plan);
                        draw_row(ui, None, &row);
                    }
                }
            }
        }
    }

    fn status(&self, ui: &mut Ui) {
        if let Some(job) = &self.job {
            let under_way = job.under_way.lock().unwrap_or_else(PoisonError::into_inner);
            let partial: f32 = under_way
                .iter()
                .map(|(_, p)| p.load(Ordering::Relaxed) as f32 / 1000.0)
                .sum();
            let done = (job.finished as f32 + partial) / job.total.max(1) as f32;
            let text = format!(
                "{} of {}",
                (job.finished + under_way.len()).min(job.total),
                job.total
            );
            ui.add(egui::ProgressBar::new(done).desired_width(160.0).text(text));
        } else if self.checking {
            ui.spinner();
            ui.weak("Reading levels…");
        }
    }

    /// The folder's files, a box to choose each, and what each becomes.
    fn list_folder(&mut self, ui: &mut Ui, busy: bool) {
        let plans: HashMap<PathBuf, &Result<Plan, String>> = self
            .rows
            .iter()
            .map(|(p, plan)| (p.clone(), plan))
            .collect();
        let texts: Vec<RowText> = self
            .files
            .iter()
            .map(|e| match plans.get(&e.path) {
                Some(plan) => self.row_text(&e.path, plan),
                None => RowText {
                    from: file_name(&e.path),
                    to: String::new(),
                    note: e.header.as_ref().err().cloned().unwrap_or_default(),
                    color: Color32::from_gray(110),
                    note_color: Color32::from_gray(90),
                },
            })
            .collect();
        let height = 2.0 * ui.spacing().interact_size.y;
        let chosen = &mut self.chosen;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(
            ui,
            height,
            texts.len(),
            |ui, range| {
                for i in range {
                    let choose = chosen.get_mut(i).filter(|_| !busy);
                    draw_row(ui, Some(choose), &texts[i]);
                }
            },
        );
    }

    /// What a row says: the file, what it becomes, and the notes or the
    /// outcome under them.
    fn row_text(&self, path: &Path, plan: &Result<Plan, String>) -> RowText {
        let from = file_name(path);
        if let Some(result) = self.results.get(path) {
            return match result {
                Ok(to) => RowText {
                    from,
                    to: file_name(to),
                    note: "made".into(),
                    color: Color32::WHITE,
                    note_color: views::AXIS,
                },
                Err(why) => RowText {
                    from,
                    to: String::new(),
                    note: why.clone(),
                    color: views::CURSOR,
                    note_color: views::CURSOR,
                },
            };
        }
        match plan {
            Ok(plan) => {
                let mut notes = plan.changes.clone();
                if plan.shape.needs_peak() {
                    match self.levels.get(path).map(|(_, l)| l) {
                        Some(Level::Peak(peak)) => {
                            if let Some(gain) = convert::fit(*peak) {
                                notes.push(format!(
                                    "{} to fit, as it peaks past full scale",
                                    convert::decibels(gain)
                                ));
                            }
                        }
                        Some(Level::Failed(why)) => {
                            notes.push(format!("its level cannot be read: {why}"))
                        }
                        _ => notes.push("reading its level…".into()),
                    }
                }
                RowText {
                    from,
                    to: file_name(&plan.to),
                    note: notes.join(" · "),
                    color: Color32::WHITE,
                    note_color: views::AXIS,
                }
            }
            Err(why) => RowText {
                from,
                to: String::new(),
                note: why.clone(),
                color: views::CURSOR,
                note_color: views::CURSOR,
            },
        }
    }
}

struct RowText {
    from: String,
    to: String,
    note: String,
    color: Color32,
    note_color: Color32,
}

/// A row two lines tall: a box to choose the file where there is one, the
/// file and what it becomes, and the notes under them, each line cut short
/// with the whole of it on hover.
fn draw_row(ui: &mut Ui, choose: Option<Option<&mut bool>>, row: &RowText) {
    let line = ui.spacing().interact_size.y;
    ui.horizontal(|ui| {
        if let Some(choose) = choose {
            match choose {
                Some(chosen) => {
                    ui.add_sized([22.0, line], egui::Checkbox::without_text(chosen));
                }
                None => {
                    ui.add_space(22.0 + ui.spacing().item_spacing.x);
                }
            }
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let width = ui.available_width();
            ui.horizontal(|ui| {
                let half = ((width - 24.0) / 2.0).max(60.0);
                cell(ui, &row.from, half, views::AXIS);
                ui.label(RichText::new("→").monospace().color(Color32::from_gray(90)));
                cell(ui, &row.to, half, row.color);
            });
            cell(ui, &row.note, width, row.note_color);
        });
    });
}

/// A line of text from the left of a column `width` wide, cut short there,
/// with the whole of it on hover.
fn cell(ui: &mut Ui, text: &str, width: f32, color: Color32) -> egui::Response {
    let label = egui::Label::new(RichText::new(text).color(color)).truncate();
    let size = Vec2::new(width, ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(
        size,
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(size);
            ui.add(label)
        },
    )
    .inner
}

/// The format box, and beside it the one setting that matters for the
/// format picked.
fn target_boxes(ui: &mut Ui, target: &mut Target) {
    egui::ComboBox::from_id_salt("convert-format")
        .width(120.0)
        .selected_text(target.format.name())
        .show_ui(ui, |ui| {
            for format in Format::ALL {
                ui.selectable_value(&mut target.format, format, format.name());
            }
        });
    match target.format {
        Format::Mp3 => {
            egui::ComboBox::from_id_salt("convert-mp3")
                .width(150.0)
                .selected_text(target.mp3.name())
                .show_ui(ui, |ui| {
                    for q in Mp3::ALL {
                        ui.selectable_value(&mut target.mp3, q, q.name());
                    }
                });
        }
        Format::Ogg | Format::Webm => {
            egui::ComboBox::from_id_salt("convert-vorbis")
                .width(150.0)
                .selected_text(target.vorbis.name())
                .show_ui(ui, |ui| {
                    for q in Vorbis::ALL {
                        ui.selectable_value(&mut target.vorbis, q, q.name());
                    }
                });
        }
        format => {
            let depths: &[Depth] = if format.takes_float() {
                &[Depth::AsFile, Depth::Int16, Depth::Int24, Depth::Float32]
            } else {
                &[Depth::AsFile, Depth::Int16, Depth::Int24]
            };
            if !depths.contains(&target.depth) {
                target.depth = Depth::AsFile;
            }
            egui::ComboBox::from_id_salt("convert-depth")
                .width(110.0)
                .selected_text(target.depth.name())
                .show_ui(ui, |ui| {
                    for &d in depths {
                        ui.selectable_value(&mut target.depth, d, d.name());
                    }
                })
                .response
                .on_hover_text("As the file: whole numbers stay as wide, a float stays a float where the format takes one and is lowered to fit 24 bits where it does not, and a lossy file becomes 16-bit");
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn folder_name(folder: Option<&Path>) -> String {
    folder
        .and_then(Path::file_name)
        .map_or_else(|| "no folder".into(), |n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A WAV of a second of a tone at `peak`, 16-bit or as floats.
    fn wav(path: &Path, peak: f32, float: bool) {
        let rate = 48_000u32;
        let (tag, width) = if float { (3u16, 4u16) } else { (1, 2) };
        let mut data = Vec::new();
        for i in 0..rate {
            let s = peak * (i as f32 * 0.05).sin();
            if float {
                data.extend_from_slice(&s.to_le_bytes());
            } else {
                data.extend_from_slice(&((s * 32_000.0) as i16).to_le_bytes());
            }
        }
        let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        b.extend_from_slice(&16u32.to_le_bytes());
        for v in [tag, 1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in [rate, rate * u32::from(width)] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in [width, width * 8] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend(data);
        let size = (b.len() - 8) as u32;
        b[4..8].copy_from_slice(&size.to_le_bytes());
        std::fs::write(path, b).unwrap();
    }

    /// Frames of the view, as the window draws them, until `done` says
    /// so: the folders new files went into meanwhile.
    fn until(
        ctx: &egui::Context,
        converter: &mut Converter,
        folder: &Path,
        done: impl Fn(&Converter) -> bool,
    ) -> Vec<PathBuf> {
        let start = Instant::now();
        let mut grown = Vec::new();
        loop {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                grown.extend(converter.follow(Some(folder), None, ui.ctx()));
                converter.ui(ui, None, false, false);
            });
            // No window to hand the fonts' texture to.
            output.textures_delta.clear();
            if done(converter) {
                return grown;
            }
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "the view never got there"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_folder_converts_and_its_new_files_show_unchosen() {
        let dir =
            std::env::temp_dir().join(format!("soundcheck-convert-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        wav(&dir.join("a.wav"), 0.5, false);
        wav(&dir.join("b.wav"), 0.5, false);
        wav(&dir.join("loud.wav"), 1.6, true);
        let ctx = egui::Context::default();
        let mut converter = Converter {
            setup: Setup {
                target: Target {
                    format: Format::Flac,
                    ..Target::default()
                },
                folder: true,
            },
            ..Converter::default()
        };
        until(&ctx, &mut converter, &dir, |c| {
            !c.reading && c.files.len() == 3
        });
        assert!(
            converter.chosen.iter().all(|&c| c),
            "a first listing starts all chosen"
        );
        until(&ctx, &mut converter, &dir, |c| {
            !c.checking && !c.levels.is_empty()
        });
        let loud = dir.join("loud.wav");
        let row = converter
            .rows
            .iter()
            .find(|(p, _)| *p == loud)
            .map(|(p, plan)| converter.row_text(p, plan))
            .unwrap();
        assert_eq!(row.to, "loud.flac");
        assert!(row.note.contains("-4.08 dB to fit"), "{}", row.note);
        assert!(
            row.note.contains("24-bit from 32-bit float"),
            "{}",
            row.note
        );
        converter.start(&ctx);
        let grown = until(&ctx, &mut converter, &dir, |c| {
            c.job.is_none() && !c.reading
        });
        // Each file that lands shows in the explorer then, so the folder can
        // come back once a frame while the job runs.
        assert!(
            !grown.is_empty() && grown.iter().all(|g| *g == dir),
            "{grown:?}"
        );
        assert!(
            converter.results.values().all(Result::is_ok),
            "{:?}",
            converter.results
        );
        assert_eq!(
            converter.message.as_ref().map(|(m, _)| m.as_str()),
            Some("3 files converted")
        );
        until(&ctx, &mut converter, &dir, |c| c.files.len() == 6);
        let chosen: Vec<String> = converter
            .files
            .iter()
            .zip(&converter.chosen)
            .filter(|(_, c)| **c)
            .map(|(e, _)| file_name(&e.path))
            .collect();
        assert_eq!(chosen, ["a.wav", "b.wav", "loud.wav"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
