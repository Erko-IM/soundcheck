//! The bulk rename tools: a short form beside the other views for the
//! changes most renames need, and a window with every rule, laid out as
//! Bulk Rename Utility lays out its panels.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::SystemTime;

use chrono::{DateTime, Local, NaiveDateTime};
use eframe::egui::{self, Color32, RichText, Sense, TextEdit, Ui, Vec2};

use crate::rename::{
    self, Candidate, Case, Crop, DATE_FORMATS, DateSource, ExtensionMode, Filters, MoveMode,
    NameMode, NumberKind, NumberPlace, Place, Planned, Rules, Side,
};
use crate::views;

/// Renames to make: each file's path and its new one.
pub type Batch = Vec<(PathBuf, PathBuf)>;

/// What the tools want the window to do.
pub enum Request {
    Rename(Batch),
    /// Put the last batch back.
    Undo(Batch),
    /// Show the window with every rule.
    AllRules,
    /// Close that window.
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Name,
    Size,
    Modified,
    Recorded,
}

pub struct Renamer {
    pub rules: Rules,
    /// The folder listed, and the filters it was listed with.
    folder: Option<PathBuf>,
    listed_with: Option<Filters>,
    files: Vec<Candidate>,
    chosen: Vec<bool>,
    sort: Sort,
    descending: bool,
    reading: bool,
    /// Bumped for each listing, so an older one arriving late is dropped.
    listing: u64,
    tx: mpsc::Sender<(u64, Vec<Candidate>)>,
    rx: mpsc::Receiver<(u64, Vec<Candidate>)>,
    plan: Vec<Option<Planned>>,
    /// What the plan was made from, so it is only made again on a change.
    planned_from: Option<(Rules, Vec<bool>, u64)>,
    /// The last batch done, for Undo.
    last: Option<Batch>,
    message: Option<(String, bool)>,
}

impl Default for Renamer {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rules: Rules::default(),
            folder: None,
            listed_with: None,
            files: Vec::new(),
            chosen: Vec::new(),
            sort: Sort::Name,
            descending: false,
            reading: false,
            listing: 0,
            tx,
            rx,
            plan: Vec::new(),
            planned_from: None,
            last: None,
            message: None,
        }
    }
}

impl Renamer {
    /// Lists `folder` again whenever it or the filters change, and takes in
    /// a listing once it is read.
    pub fn follow(&mut self, folder: Option<&Path>, ctx: &egui::Context) {
        let arrived: Vec<(u64, Vec<Candidate>)> = self.rx.try_iter().collect();
        for (listing, files) in arrived {
            if listing == self.listing {
                self.chosen = vec![true; files.len()];
                self.files = files;
                self.reading = false;
                self.sort_files();
            }
        }
        if self.folder.as_deref() != folder
            || self.listed_with.as_ref() != Some(&self.rules.filters)
        {
            self.folder = folder.map(Path::to_owned);
            self.refresh(ctx);
        }
    }

    /// Reads the folder again, as after a rename.
    pub fn refresh(&mut self, ctx: &egui::Context) {
        self.listing += 1;
        self.listed_with = Some(self.rules.filters.clone());
        self.planned_from = None;
        let Some(folder) = self.folder.clone() else {
            self.files.clear();
            self.chosen.clear();
            return;
        };
        self.reading = true;
        let (tx, ctx, listing, filters) = (
            self.tx.clone(),
            ctx.clone(),
            self.listing,
            self.rules.filters.clone(),
        );
        std::thread::spawn(move || {
            let files = rename::list(&folder, &filters);
            // The receiver only goes away with the window: nobody to tell.
            let _ = tx.send((listing, files));
            ctx.request_repaint();
        });
    }

    /// Says how the batch the window was asked for went.
    pub fn finished(
        &mut self,
        batch: Batch,
        undone: bool,
        result: Result<(), String>,
        ctx: &egui::Context,
    ) {
        let count = batch.len();
        self.message = Some(match result {
            Ok(()) if undone => {
                self.last = None;
                (format!("{count} put back as they were"), false)
            }
            Ok(()) => {
                self.last = Some(batch);
                let files = if count == 1 { "file" } else { "files" };
                (format!("{count} {files} renamed"), false)
            }
            Err(why) => (why, true),
        });
        self.refresh(ctx);
    }

    fn sort_files(&mut self) {
        let mut rows: Vec<(Candidate, bool)> =
            self.files.drain(..).zip(self.chosen.drain(..)).collect();
        let name = |c: &Candidate| c.path.to_string_lossy().to_lowercase();
        rows.sort_by(|(a, _), (b, _)| {
            let order = match self.sort {
                Sort::Name => crate::explorer::natural(&name(a), &name(b)),
                Sort::Size => a.size.cmp(&b.size),
                Sort::Modified => a.modified.cmp(&b.modified),
                Sort::Recorded => a.recorded.cmp(&b.recorded),
            };
            order.then_with(|| crate::explorer::natural(&name(a), &name(b)))
        });
        if self.descending {
            rows.reverse();
        }
        (self.files, self.chosen) = rows.into_iter().unzip();
        self.planned_from = None;
    }

    /// Plans again if the rules, the choice or the files changed.
    fn replan(&mut self) {
        let key = (self.rules.clone(), self.chosen.clone(), self.listing);
        if self.planned_from.as_ref() != Some(&key) {
            self.plan = rename::plan(&self.files, &self.chosen, &self.rules);
            self.planned_from = Some(key);
        }
    }

    /// Counts of the chosen files: all, those that change, and those that
    /// cannot.
    fn counts(&mut self) -> (usize, usize, usize) {
        self.replan();
        let plan = &self.plan;
        let chosen = plan.iter().flatten().count();
        let changing = plan.iter().flatten().filter(|p| p.changes()).count();
        let problems = plan
            .iter()
            .flatten()
            .filter(|p| p.problem.is_some())
            .count();
        (chosen, changing, problems)
    }

    /// The batch to do now, planned afresh against the disk as it is; or
    /// why there is none.
    fn batch(&mut self) -> Result<Batch, String> {
        self.planned_from = None;
        self.replan();
        let plan = &self.plan;
        if let Some(p) = plan.iter().flatten().find(|p| p.problem.is_some()) {
            let name = p.from.file_name().map(|n| n.to_string_lossy().into_owned());
            let why = p
                .problem
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            return Err(format!("{}: {why}", name.unwrap_or_default()));
        }
        Ok(plan
            .iter()
            .flatten()
            .filter(|p| p.changes())
            .map(|p| (p.from.clone(), p.to.clone()))
            .collect())
    }

    fn folder_name(&self) -> String {
        self.folder
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(|| "no folder".into(), |n| n.to_string_lossy().into_owned())
    }

    /// Rules the short form does not show, still set from the window.
    fn hidden_rules(&self) -> usize {
        let (r, d) = (&self.rules, Rules::default());
        [
            r.regex != d.regex,
            r.name != d.name,
            r.replace.match_case,
            r.case.except != d.case.except,
            r.remove != d.remove,
            r.moves != d.moves,
            r.add.insert != d.add.insert || r.add.word_space,
            r.date != d.date,
            r.folder != d.folder,
            r.numbering
                != rename::NumberRule {
                    place: r.numbering.place,
                    start: r.numbering.start,
                    pad: r.numbering.pad,
                    ..d.numbering.clone()
                },
            r.extension != d.extension,
            r.filters != d.filters,
        ]
        .iter()
        .filter(|&&set| set)
        .count()
    }

    /// The short form, for the side of the window.
    pub fn simple(&mut self, ui: &mut Ui, locked: bool) -> Option<Request> {
        let mut request = None;
        ui.horizontal(|ui| {
            ui.strong("Rename");
            ui.weak(format!("files in {}", self.folder_name()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("All rules…")
                    .on_hover_text("Every rule, in a window of its own")
                    .clicked()
                {
                    request = Some(Request::AllRules);
                }
            });
        });
        ui.add_space(4.0);
        let rules = &mut self.rules;
        egui::Grid::new("rename-simple")
            .num_columns(2)
            .spacing([6.0, 4.0])
            .show(ui, |ui| {
                ui.label("Replace");
                ui.horizontal(|ui| {
                    text(ui, &mut rules.replace.find, 90.0);
                    ui.label("with");
                    text(ui, &mut rules.replace.with, 90.0);
                });
                ui.end_row();
                ui.label("Add");
                ui.horizontal(|ui| {
                    text(ui, &mut rules.add.prefix, 70.0);
                    ui.label("before,");
                    text(ui, &mut rules.add.suffix, 70.0);
                    ui.label("after");
                });
                ui.end_row();
                ui.label("Number");
                ui.horizontal(|ui| {
                    let mut on = rules.numbering.place != NumberPlace::Off;
                    if ui.checkbox(&mut on, "").changed() {
                        rules.numbering.place = if on {
                            NumberPlace::Suffix
                        } else {
                            NumberPlace::Off
                        };
                    }
                    ui.add_enabled_ui(on, |ui| {
                        ui.label("from");
                        ui.add(egui::DragValue::new(&mut rules.numbering.start));
                        ui.label("digits");
                        ui.add(egui::DragValue::new(&mut rules.numbering.pad).range(1..=9));
                        egui::ComboBox::from_id_salt("rename-number-place")
                            .width(62.0)
                            .selected_text(if rules.numbering.place == NumberPlace::Prefix {
                                "before"
                            } else {
                                "after"
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut rules.numbering.place,
                                    NumberPlace::Prefix,
                                    "before",
                                );
                                ui.selectable_value(
                                    &mut rules.numbering.place,
                                    NumberPlace::Suffix,
                                    "after",
                                );
                            });
                    });
                });
                ui.end_row();
                ui.label("Case");
                case_box(ui, "rename-simple-case", &mut rules.case.case);
                ui.end_row();
            });
        let hidden = self.hidden_rules();
        if hidden > 0 {
            let rules = if hidden == 1 { "rule" } else { "rules" };
            ui.weak(format!(
                "{hidden} more {rules} set in the window with every rule"
            ));
        }
        ui.separator();
        let (chosen, changing, problems) = self.counts();
        ui.horizontal(|ui| {
            let can = changing > 0 && problems == 0 && !locked;
            let label = format!("Rename {changing}");
            if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                request = self.go();
            }
            if ui
                .add_enabled(self.last.is_some() && !locked, egui::Button::new("Undo"))
                .on_hover_text("Put the last renamed files back as they were")
                .clicked()
            {
                request = self
                    .last
                    .clone()
                    .map(|batch| Request::Undo(reversed(&batch)));
            }
            status(ui, chosen, changing, problems, self.reading);
        });
        self.message_line(ui);
        self.replan();
        preview(ui, &self.files, &self.plan, false, &mut Vec::new());
        request
    }

    fn go(&mut self) -> Option<Request> {
        match self.batch() {
            Ok(batch) if batch.is_empty() => None,
            Ok(batch) => Some(Request::Rename(batch)),
            Err(why) => {
                self.message = Some((why, true));
                None
            }
        }
    }

    fn message_line(&self, ui: &mut Ui) {
        if let Some((message, error)) = &self.message {
            let color = if *error { views::CURSOR } else { views::AXIS };
            ui.label(RichText::new(message).color(color));
        }
    }

    /// The window with every rule. `embedded` inside the main window, where
    /// it needs a button of its own to close.
    pub fn full(&mut self, ui: &mut Ui, locked: bool, embedded: bool) -> Option<Request> {
        let mut request = None;
        let folder = self
            .folder
            .as_ref()
            .map_or_else(|| "no folder".into(), |f| f.display().to_string());
        ui.horizontal(|ui| {
            if embedded && ui.small_button("Close").clicked() {
                request = Some(Request::Close);
            }
            ui.strong(folder);
            if ui.small_button("Read again").clicked() {
                self.refresh(ui.ctx());
            }
            ui.separator();
            if ui.small_button("All").clicked() {
                self.chosen.fill(true);
            }
            if ui.small_button("None").clicked() {
                self.chosen.fill(false);
            }
            if ui.small_button("Invert").clicked() {
                for c in &mut self.chosen {
                    *c = !*c;
                }
            }
        });
        ui.add_space(4.0);
        // The rules take what they need at the bottom; the files get the rest.
        egui::Panel::bottom("rename-rules")
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                ui.add_space(6.0);
                if let Some(asked) = self.panels(ui, locked) {
                    request = Some(asked);
                }
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                self.header(ui);
                self.replan();
                preview(ui, &self.files, &self.plan, true, &mut self.chosen);
            });
        request
    }

    /// Column titles that sort the files, a second click the other way.
    fn header(&mut self, ui: &mut Ui) {
        let widths = Columns::of(ui);
        let mut clicked = None;
        ui.horizontal(|ui| {
            ui.allocate_exact_size(Vec2::new(widths.check, 18.0), Sense::hover());
            for (sort, title, width) in [
                (Sort::Name, "Name", widths.name),
                (Sort::Name, "New name", widths.name),
                (Sort::Size, "Size", widths.size),
                (Sort::Modified, "Modified", widths.date),
                (Sort::Recorded, "Recorded", widths.date),
            ] {
                let arrow = match (self.sort == sort && title != "New name", self.descending) {
                    (true, false) => " ⏶",
                    (true, true) => " ⏷",
                    _ => "",
                };
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::click());
                ui.painter().text(
                    rect.left_center(),
                    egui::Align2::LEFT_CENTER,
                    format!("{title}{arrow}"),
                    egui::FontId::proportional(13.0),
                    views::AXIS,
                );
                if title != "New name" && response.clicked() {
                    clicked = Some(sort);
                }
            }
        });
        if let Some(sort) = clicked {
            if self.sort == sort {
                self.descending = !self.descending;
            } else {
                (self.sort, self.descending) = (sort, false);
            }
            self.sort_files();
        }
    }

    fn panels(&mut self, ui: &mut Ui, locked: bool) -> Option<Request> {
        let d = Rules::default();
        let mut request = None;
        let r = &mut self.rules;
        let frame = egui::Frame::group(ui.style());
        let edge = frame.inner_margin.sum().x + 2.0 * frame.stroke.width;
        let gap = ui.spacing().item_spacing.x;
        let width = ((ui.available_width() - 4.0 * gap) / 5.0 - edge).max(170.0);
        ui.horizontal_top(|ui| {
            panel(
                ui,
                width,
                "1  RegEx",
                r.regex != d.regex,
                &mut r.regex,
                |ui, rule| {
                    row(ui, "Match", |ui| text(ui, &mut rule.find, f32::INFINITY));
                    row(ui, "Replace", |ui| {
                        text(ui, &mut rule.replace, f32::INFINITY)
                    });
                    ui.checkbox(&mut rule.extension, "Include the extension");
                },
            );
            panel(
                ui,
                width,
                "2  Name",
                r.name != d.name,
                &mut r.name,
                |ui, rule| {
                    ui.horizontal(|ui| {
                        for (mode, label) in [
                            (NameMode::Keep, "Keep"),
                            (NameMode::Remove, "Remove"),
                            (NameMode::Fixed, "Fixed"),
                            (NameMode::Reverse, "Reverse"),
                        ] {
                            ui.radio_value(&mut rule.mode, mode, label);
                        }
                    });
                    ui.add_enabled_ui(rule.mode == NameMode::Fixed, |ui| {
                        text(ui, &mut rule.fixed, f32::INFINITY);
                    });
                },
            );
            panel(
                ui,
                width,
                "3  Replace",
                r.replace != d.replace,
                &mut r.replace,
                |ui, rule| {
                    row(ui, "Replace", |ui| text(ui, &mut rule.find, f32::INFINITY));
                    row(ui, "With", |ui| text(ui, &mut rule.with, f32::INFINITY));
                    ui.checkbox(&mut rule.match_case, "Match case");
                },
            );
            panel(
                ui,
                width,
                "4  Case",
                r.case != d.case,
                &mut r.case,
                |ui, rule| {
                    case_box(ui, "rename-full-case", &mut rule.case);
                    row(ui, "Except", |ui| text(ui, &mut rule.except, f32::INFINITY))
                        .on_hover_text(
                            "Words kept exactly as typed here, separated by spaces or commas",
                        );
                },
            );
            panel(
                ui,
                width,
                "5  Remove",
                r.remove != d.remove,
                &mut r.remove,
                |ui, rule| {
                    ui.horizontal(|ui| {
                        ui.label("First");
                        count(ui, &mut rule.first);
                        ui.label("Last");
                        count(ui, &mut rule.last);
                    });
                    ui.horizontal(|ui| {
                        ui.label("From");
                        count(ui, &mut rule.from);
                        ui.label("to");
                        count(ui, &mut rule.to);
                    });
                    row(ui, "Chars", |ui| text(ui, &mut rule.chars, f32::INFINITY));
                    row(ui, "Words", |ui| text(ui, &mut rule.words, f32::INFINITY));
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("rename-crop")
                            .width(64.0)
                            .selected_text(match rule.crop {
                                Crop::Off => "No crop",
                                Crop::Before => "Before",
                                Crop::After => "After",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut rule.crop, Crop::Off, "No crop");
                                ui.selectable_value(&mut rule.crop, Crop::Before, "Before");
                                ui.selectable_value(&mut rule.crop, Crop::After, "After");
                            });
                        text(ui, &mut rule.crop_at, f32::INFINITY);
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut rule.digits, "Digits");
                        ui.checkbox(&mut rule.accents, "Accents");
                        ui.checkbox(&mut rule.symbols, "Symbols");
                        ui.checkbox(&mut rule.high, "Non-ASCII");
                        ui.checkbox(&mut rule.trim, "Trim");
                        ui.checkbox(&mut rule.double_spaces, "Double spaces");
                        ui.checkbox(&mut rule.lead_dots, "Leading dots");
                    });
                },
            );
        });
        ui.add_space(6.0);
        ui.horizontal_top(|ui| {
            panel(
                ui,
                width,
                "6  Move / copy",
                r.moves != d.moves,
                &mut r.moves,
                |ui, rule| {
                    egui::ComboBox::from_id_salt("rename-move")
                        .width(ui.available_width())
                        .selected_text(move_label(rule.mode))
                        .show_ui(ui, |ui| {
                            for mode in [
                                MoveMode::Off,
                                MoveMode::CopyFirst,
                                MoveMode::CopyLast,
                                MoveMode::MoveFirst,
                                MoveMode::MoveLast,
                            ] {
                                ui.selectable_value(&mut rule.mode, mode, move_label(mode));
                            }
                        });
                    ui.horizontal(|ui| {
                        count(ui, &mut rule.count);
                        ui.label("characters to");
                    });
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut rule.to, Place::Start, "Start");
                        ui.radio_value(&mut rule.to, Place::End, "End");
                        ui.radio_value(&mut rule.to, Place::At, "At");
                        count(ui, &mut rule.at);
                    });
                    row(ui, "Separator", |ui| {
                        text(ui, &mut rule.separator, f32::INFINITY)
                    });
                },
            );
            panel(
                ui,
                width,
                "7  Add",
                r.add != d.add,
                &mut r.add,
                |ui, rule| {
                    row(ui, "Prefix", |ui| text(ui, &mut rule.prefix, f32::INFINITY));
                    ui.horizontal(|ui| {
                        ui.label("Insert");
                        text(ui, &mut rule.insert, 80.0);
                        ui.label("at");
                        count(ui, &mut rule.at);
                    });
                    row(ui, "Suffix", |ui| text(ui, &mut rule.suffix, f32::INFINITY));
                    ui.checkbox(&mut rule.word_space, "Space before capitals");
                },
            );
            panel(
                ui,
                width,
                "8  Auto date",
                r.date != d.date,
                &mut r.date,
                |ui, rule| {
                    side_radios(ui, &mut rule.side);
                    ui.horizontal(|ui| {
                        ui.label("From");
                        egui::ComboBox::from_id_salt("rename-date-source")
                            .width(96.0)
                            .selected_text(rule.source.label())
                            .show_ui(ui, |ui| {
                                for source in DateSource::ALL {
                                    ui.selectable_value(&mut rule.source, source, source.label());
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Format");
                        egui::ComboBox::from_id_salt("rename-date-format")
                            .width(150.0)
                            .selected_text(date_example(&rule.format))
                            .show_ui(ui, |ui| {
                                for format in DATE_FORMATS {
                                    ui.selectable_value(
                                        &mut rule.format,
                                        format.to_owned(),
                                        date_example(format),
                                    );
                                }
                            });
                    });
                    row(ui, "Custom", |ui| text(ui, &mut rule.format, f32::INFINITY))
                        .on_hover_text("%Y year, %m month, %d day, %H hour, %M minute, %S second");
                    row(ui, "Separator", |ui| {
                        text(ui, &mut rule.separator, f32::INFINITY)
                    });
                    ui.add_enabled_ui(rule.source == DateSource::Recorded, |ui| {
                        ui.checkbox(&mut rule.fallback, "Modified, where not recorded");
                    });
                },
            );
            panel(
                ui,
                width,
                "9  Folder name",
                r.folder != d.folder,
                &mut r.folder,
                |ui, rule| {
                    side_radios(ui, &mut rule.side);
                    row(ui, "Separator", |ui| {
                        text(ui, &mut rule.separator, f32::INFINITY)
                    });
                    ui.horizontal(|ui| {
                        ui.label("Folders up");
                        ui.add(egui::DragValue::new(&mut rule.levels).range(1..=9));
                    });
                },
            );
            panel(
                ui,
                width,
                "10  Numbering",
                r.numbering != d.numbering,
                &mut r.numbering,
                |ui, rule| {
                    ui.horizontal_wrapped(|ui| {
                        for (place, label) in [
                            (NumberPlace::Off, "Off"),
                            (NumberPlace::Prefix, "Prefix"),
                            (NumberPlace::Suffix, "Suffix"),
                            (NumberPlace::Both, "Both"),
                            (NumberPlace::At, "At"),
                        ] {
                            ui.radio_value(&mut rule.place, place, label);
                        }
                        count(ui, &mut rule.at);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Start");
                        ui.add(egui::DragValue::new(&mut rule.start));
                        ui.label("Step");
                        ui.add(egui::DragValue::new(&mut rule.step));
                        ui.label("Digits");
                        ui.add(egui::DragValue::new(&mut rule.pad).range(1..=9));
                    });
                    row(ui, "Separator", |ui| {
                        text(ui, &mut rule.separator, f32::INFINITY)
                    });
                    ui.horizontal(|ui| {
                        ui.label("Restart when the first");
                        count(ui, &mut rule.restart_after);
                        ui.label("change");
                    })
                    .response
                    .on_hover_text("Characters of the name; 0 never restarts");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut rule.per_folder, "Per folder");
                        egui::ComboBox::from_id_salt("rename-number-kind")
                            .width(70.0)
                            .selected_text(rule.kind.label())
                            .show_ui(ui, |ui| {
                                for kind in NumberKind::ALL {
                                    ui.selectable_value(&mut rule.kind, kind, kind.label());
                                }
                            });
                    });
                },
            );
        });
        ui.add_space(6.0);
        ui.horizontal_top(|ui| {
            let r = &mut self.rules;
            panel(
                ui,
                width,
                "11  Extension",
                r.extension != d.extension,
                &mut r.extension,
                |ui, rule| {
                    egui::ComboBox::from_id_salt("rename-extension")
                        .width(90.0)
                        .selected_text(rule.mode.label())
                        .show_ui(ui, |ui| {
                            for mode in ExtensionMode::ALL {
                                ui.selectable_value(&mut rule.mode, mode, mode.label());
                            }
                        });
                    ui.add_enabled_ui(
                        matches!(rule.mode, ExtensionMode::Fixed | ExtensionMode::Extra),
                        |ui| {
                            text(ui, &mut rule.text, f32::INFINITY);
                        },
                    );
                },
            );
            panel(
                ui,
                width,
                "12  Filters",
                r.filters != d.filters,
                &mut r.filters,
                |ui, rule| {
                    row(ui, "Mask", |ui| text(ui, &mut rule.mask, f32::INFINITY)).on_hover_text(
                        "* for any run of characters, ? for any one, ; between patterns",
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut rule.match_case, "Match case");
                        ui.checkbox(&mut rule.subfolders, "Subfolders");
                        ui.checkbox(&mut rule.audio_only, "Audio files only");
                    });
                },
            );
            ui.vertical(|ui| {
                ui.add_space(4.0);
                let (chosen, changing, problems) = self.counts();
                status(ui, chosen, changing, problems, self.reading);
                ui.horizontal(|ui| {
                    let can = changing > 0 && problems == 0 && !locked;
                    if ui
                        .add_enabled(
                            can,
                            egui::Button::new(RichText::new(format!("Rename {changing}")).strong()),
                        )
                        .clicked()
                    {
                        request = self.go();
                    }
                    if ui
                        .add_enabled(self.last.is_some() && !locked, egui::Button::new("Undo"))
                        .on_hover_text("Put the last renamed files back as they were")
                        .clicked()
                    {
                        request = self
                            .last
                            .clone()
                            .map(|batch| Request::Undo(reversed(&batch)));
                    }
                    if ui
                        .add_enabled(
                            self.rules != Rules::default(),
                            egui::Button::new("Reset all"),
                        )
                        .clicked()
                    {
                        self.rules = Rules::default();
                    }
                });
                self.message_line(ui);
            });
        });
        request
    }
}

fn reversed(batch: &Batch) -> Batch {
    batch
        .iter()
        .map(|(from, to)| (to.clone(), from.clone()))
        .collect()
}

fn text(ui: &mut Ui, value: &mut String, width: f32) {
    ui.add(TextEdit::singleline(value).desired_width(width));
}

fn count(ui: &mut Ui, value: &mut usize) {
    ui.add(egui::DragValue::new(value).range(0..=999));
}

/// A label and a field beside it.
fn row(ui: &mut Ui, label: &str, field: impl FnOnce(&mut Ui)) -> egui::Response {
    ui.horizontal(|ui| {
        ui.label(label);
        field(ui);
    })
    .response
}

fn case_box(ui: &mut Ui, id: &str, case: &mut Case) {
    egui::ComboBox::from_id_salt(id)
        .width(90.0)
        .selected_text(case.label())
        .show_ui(ui, |ui| {
            for c in Case::ALL {
                ui.selectable_value(case, c, c.label());
            }
        });
}

fn side_radios(ui: &mut Ui, side: &mut Side) {
    ui.horizontal(|ui| {
        ui.radio_value(side, Side::Off, "Off");
        ui.radio_value(side, Side::Prefix, "Prefix");
        ui.radio_value(side, Side::Suffix, "Suffix");
    });
}

fn move_label(mode: MoveMode) -> &'static str {
    match mode {
        MoveMode::Off => "Off",
        MoveMode::CopyFirst => "Copy the first",
        MoveMode::CopyLast => "Copy the last",
        MoveMode::MoveFirst => "Move the first",
        MoveMode::MoveLast => "Move the last",
    }
}

/// A date format as a sample date looks in it.
fn date_example(format: &str) -> String {
    let sample = NaiveDateTime::parse_from_str("2026-04-03 07:14:05", "%Y-%m-%d %H:%M:%S")
        .unwrap_or_default();
    let mut shown = String::new();
    if std::fmt::Write::write_fmt(&mut shown, format_args!("{}", sample.format(format))).is_err() {
        return format.to_owned();
    }
    shown
}

/// One numbered group of rules, with a reset in its title.
fn panel<T: Default>(
    ui: &mut Ui,
    width: f32,
    title: &str,
    changed: bool,
    rule: &mut T,
    contents: impl FnOnce(&mut Ui, &mut T),
) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(width);
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                let title = RichText::new(title).strong();
                ui.label(if changed {
                    title.color(views::MARK)
                } else {
                    title
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if views::reset_button(ui, changed, "no change") {
                        *rule = T::default();
                    }
                });
            });
            contents(ui, rule);
        });
    });
}

fn status(ui: &mut Ui, chosen: usize, changing: usize, problems: usize, reading: bool) {
    if reading {
        ui.spinner();
        ui.weak("Reading the folder…");
        return;
    }
    let mut parts = vec![format!("{changing} of {chosen} change")];
    if problems > 0 {
        parts.push(format!("{problems} cannot"));
    }
    let text = RichText::new(parts.join(", "));
    ui.label(if problems > 0 {
        text.color(views::CURSOR)
    } else {
        text.color(views::AXIS)
    });
}

/// How wide each column is at `width`.
struct Columns {
    check: f32,
    name: f32,
    size: f32,
    date: f32,
}

impl Columns {
    /// Six columns across `ui`, with the gaps between them and room for the
    /// scroll bar.
    fn of(ui: &Ui) -> Self {
        let (check, size, date) = (22.0, 70.0, 128.0);
        let spare = ui.available_width() - 5.0 * ui.spacing().item_spacing.x - 12.0;
        let name = ((spare - check - size - 2.0 * date) / 2.0).max(120.0);
        Self {
            check,
            name,
            size,
            date,
        }
    }
}

/// The files and what each becomes: changed names bright, problems red
/// with the reason on hover. With `full`, a box to choose each file and the
/// columns beside.
fn preview(
    ui: &mut Ui,
    files: &[Candidate],
    plan: &[Option<Planned>],
    full: bool,
    chosen: &mut [bool],
) {
    let height = ui.spacing().interact_size.y;
    let widths = Columns::of(ui);
    let gap = ui.spacing().item_spacing.x;
    egui::ScrollArea::vertical().auto_shrink(false).show_rows(
        ui,
        height,
        files.len(),
        |ui, range| {
            for i in range {
                let (file, planned) = (&files[i], plan.get(i).and_then(Option::as_ref));
                ui.horizontal(|ui| {
                    if full && let Some(c) = chosen.get_mut(i) {
                        ui.add_sized([widths.check, height], egui::Checkbox::without_text(c));
                    }
                    let old = file.name();
                    let (new, color, why) = match planned {
                        None => (String::new(), Color32::from_gray(90), None),
                        Some(p) => {
                            let new =
                                p.to.file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                            let color = if p.problem.is_some() {
                                views::CURSOR
                            } else if p.changes() {
                                Color32::WHITE
                            } else {
                                Color32::from_gray(120)
                            };
                            (new, color, p.problem.as_ref().map(ToString::to_string))
                        }
                    };
                    let name_width = if full {
                        widths.name
                    } else {
                        ((ui.available_width() - 2.0 * gap - 12.0) / 2.0).max(80.0)
                    };
                    let old_color = if planned.is_some() {
                        views::AXIS
                    } else {
                        Color32::from_gray(80)
                    };
                    cell(ui, &old, name_width, old_color);
                    if !full {
                        ui.label(RichText::new("→").monospace().color(Color32::from_gray(90)));
                    }
                    let response = cell(ui, &new, name_width, color);
                    if let Some(why) = why {
                        response.on_hover_text(why);
                    }
                    if full {
                        cell(ui, &size_text(file.size), widths.size, views::AXIS);
                        cell(ui, &time_text(file.modified), widths.date, views::AXIS);
                        let recorded = file.recorded.map_or_else(String::new, |t| {
                            t.format("%Y-%m-%d %H:%M:%S").to_string()
                        });
                        cell(ui, &recorded, widths.date, views::AXIS);
                    }
                });
            }
        },
    );
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

fn size_text(bytes: u64) -> String {
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 100.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
}

fn time_text(time: Option<SystemTime>) -> String {
    time.map_or_else(String::new, |t| {
        DateTime::<Local>::from(t)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    })
}
