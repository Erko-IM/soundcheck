//! Window layout, and the glue between the UI thread and the workers.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, FontId, Key, KeyboardShortcut, Modifiers, Pos2, Rect, RichText, Sense,
    Stroke, TextureOptions, Vec2,
};

use crate::audio::{self, Loaded};
use crate::explorer::Explorer;
use crate::finder::Inbox;
use crate::playback::{self, Player};
use crate::spectrogram::{self, Analysis, View};

const CURSOR: Color32 = Color32::from_rgb(235, 70, 60);
const AXIS: Color32 = Color32::from_gray(150);
const GRID: Color32 = Color32::from_gray(48);
const ELAPSED: Color32 = Color32::from_gray(220);
const WALL_CLOCK: Color32 = Color32::from_gray(115);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Colormap {
    Viridis,
    Inferno,
    Magma,
    Plasma,
    Turbo,
}

impl Colormap {
    const ALL: [Self; 5] = [
        Self::Viridis,
        Self::Inferno,
        Self::Magma,
        Self::Plasma,
        Self::Turbo,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Viridis => "Viridis",
            Self::Inferno => "Inferno",
            Self::Magma => "Magma",
            Self::Plasma => "Plasma",
            Self::Turbo => "Turbo",
        }
    }

    fn gradient(self) -> colorous::Gradient {
        match self {
            Self::Viridis => colorous::VIRIDIS,
            Self::Inferno => colorous::INFERNO,
            Self::Magma => colorous::MAGMA,
            Self::Plasma => colorous::PLASMA,
            Self::Turbo => colorous::TURBO,
        }
    }
}

enum Job {
    Loaded {
        generation: u64,
        result: Result<Loaded, String>,
    },
    Analysed {
        generation: u64,
        fft: usize,
        result: Result<Analysis, String>,
    },
}

/// Stops a background job when dropped, so replacing a job's handle stops
/// the job it replaces.
struct Cancel(Arc<AtomicBool>);

impl Cancel {
    fn new() -> (Self, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        (Self(Arc::clone(&flag)), flag)
    }
}

impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub struct App {
    explorer: Explorer,
    explorer_open: bool,
    inbox: Inbox,
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Job>,
    /// Bumped for every opened file, so results for an earlier one are dropped.
    generation: u64,
    /// The file the window is about: loading, loaded or failed to load.
    file: Option<PathBuf>,
    loading: bool,
    load_job: Option<Cancel>,
    analysis_job: Option<Cancel>,
    current: Option<Loaded>,
    error: Option<String>,
    analysis: Option<Analysis>,
    texture: Option<egui::TextureHandle>,
    texture_stale: bool,
    fft: usize,
    view: View,
    /// The band as last set by hand, applied to every file within its
    /// reach: a raised low end stays raised, and a top left at the Nyquist
    /// limit follows each file's own.
    band_low: f32,
    band_high: Option<f32>,
    colormap: Colormap,
    /// The sample the spectrum panel describes, and where playback starts.
    cursor: Option<usize>,
    spectrum: Vec<f32>,
    /// Created on first play, dropped when another file opens.
    player: Option<Player>,
    speed: u32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>, inbox: Inbox) -> Self {
        cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
        inbox.wake(&cc.egui_ctx);
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            explorer: Explorer::default(),
            explorer_open: true,
            inbox,
            tx,
            rx,
            generation: 0,
            file: None,
            loading: false,
            load_job: None,
            analysis_job: None,
            current: None,
            error: None,
            analysis: None,
            texture: None,
            texture_stale: false,
            fft: 2048,
            view: View::default(),
            band_low: 0.0,
            band_high: None,
            colormap: Colormap::Viridis,
            cursor: None,
            spectrum: Vec::new(),
            player: None,
            speed: 1,
        };
        if let Some(path) = initial.or_else(|| app.inbox.latest()) {
            app.open_external(&cc.egui_ctx, path);
        }
        if !app.explorer.has_root()
            && let Some(home) = std::env::home_dir()
        {
            app.explorer.set_root(&home);
        }
        app
    }

    /// Whatever arrives from outside the tree (Finder, the command line,
    /// drag and drop): a folder becomes the explorer's root, and a file
    /// opens with its folder shown around it.
    fn open_external(&mut self, ctx: &egui::Context, path: PathBuf) {
        if path.is_dir() {
            self.explorer.set_root(&path);
            self.explorer_open = true;
        } else if path.exists() {
            self.explorer.reveal(&path);
            self.open(ctx, path);
        } else {
            self.error = Some(format!("{} does not exist", path.display()));
        }
    }

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.generation += 1;
        self.explorer.selected = Some(path.clone());
        self.file = Some(path.clone());
        self.loading = true;
        self.current = None;
        self.error = None;
        self.analysis = None;
        self.texture = None;
        self.cursor = None;
        self.spectrum.clear();
        self.player = None;
        self.analysis_job = None;
        let (job, cancel) = Cancel::new();
        self.load_job = Some(job);
        let (tx, ctx, generation) = (self.tx.clone(), ctx.clone(), self.generation);
        std::thread::spawn(move || {
            // A decoder panic on a hostile or damaged file must end as an
            // error, not as a spinner that never stops.
            let result = std::panic::catch_unwind(|| audio::load(&path, &cancel))
                .unwrap_or_else(|_| Err("the decoder crashed on this file".into()));
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            // The receiver only goes away when the window closes: nobody to tell.
            let _ = tx.send(Job::Loaded { generation, result });
            ctx.request_repaint();
        });
    }

    fn analyse(&mut self, ctx: &egui::Context) {
        let Some(current) = &self.current else { return };
        let (tx, ctx, generation) = (self.tx.clone(), ctx.clone(), self.generation);
        let (mono, fft) = (Arc::clone(&current.mono), self.fft);
        let (job, cancel) = Cancel::new();
        self.analysis_job = Some(job);
        std::thread::spawn(move || {
            let result =
                match std::panic::catch_unwind(|| spectrogram::analyse(&mono, fft, &cancel)) {
                    Ok(Some(analysis)) => Ok(analysis),
                    Ok(None) => return,
                    Err(_) => Err("the analysis crashed on this file".to_owned()),
                };
            let _ = tx.send(Job::Analysed {
                generation,
                fft,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(job) = self.rx.try_recv() {
            match job {
                Job::Loaded { generation, result } if generation == self.generation => {
                    self.loading = false;
                    match result {
                        Ok(loaded) => {
                            let nyquist = loaded.info.nyquist();
                            self.view.f_max = self.band_high.map_or(nyquist, |f| f.min(nyquist));
                            self.view.f_min = self.band_low.min(self.view.f_max);
                            self.current = Some(loaded);
                            self.analyse(ctx);
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Job::Analysed {
                    generation,
                    fft,
                    result,
                } if generation == self.generation && fft == self.fft => match result {
                    Ok(analysis) => {
                        self.analysis = Some(analysis);
                        self.texture_stale = true;
                    }
                    Err(e) => self.error = Some(e),
                },
                // Superseded by a newer file or a different FFT size.
                _ => {}
            }
        }
    }

    fn refresh_texture(&mut self, ctx: &egui::Context) {
        if !std::mem::take(&mut self.texture_stale) {
            return;
        }
        let (Some(analysis), Some(current)) = (&self.analysis, &self.current) else {
            return;
        };
        let image = spectrogram::colorize(
            analysis,
            current.info.sample_rate,
            &self.view,
            self.colormap.gradient(),
        );
        match &mut self.texture {
            Some(texture) => texture.set(image, TextureOptions::LINEAR),
            None => {
                self.texture = Some(ctx.load_texture("spectrogram", image, TextureOptions::LINEAR));
            }
        }
    }

    /// Points the cursor, and the spectrum panel, at `sample`.
    fn show_moment(&mut self, sample: usize) {
        self.cursor = Some(sample);
        if let Some(current) = &self.current {
            self.spectrum = spectrogram::spectrum_at(&current.mono, sample, self.fft);
        }
    }

    fn seek(&mut self, sample: usize) {
        self.show_moment(sample);
        if let Some(player) = &self.player {
            player.seek(sample);
        }
    }

    fn toggle_play(&mut self) {
        let Some(current) = &self.current else { return };
        if self.player.is_none() {
            let frames = current.info.frames;
            // From the cursor, or from the start when it sits at the end.
            let start = self.cursor.filter(|&c| c < frames).unwrap_or(0);
            match Player::new(
                &current.source,
                current.info.sample_rate,
                frames,
                start,
                self.speed,
            ) {
                Ok(player) => self.player = Some(player),
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            }
        }
        let Some(player) = &self.player else { return };
        if player.is_playing() {
            player.pause();
            let at = player.position();
            self.show_moment(at);
        } else {
            player.play();
        }
    }

    fn stop(&mut self) {
        if let Some(player) = &self.player {
            player.pause();
            player.seek(0);
        }
        if self.current.is_some() {
            self.show_moment(0);
        }
    }

    fn set_speed(&mut self, speed: u32) {
        self.speed = speed;
        if let Some(player) = &self.player {
            player.set_speed(speed);
        }
    }

    fn follow_playback(&mut self, ctx: &egui::Context) {
        let Some(player) = &self.player else { return };
        if let Some(why) = player.failure() {
            self.player = None;
            self.error = Some(format!("playback stopped: {why}"));
            return;
        }
        if !player.is_playing() {
            return;
        }
        if player.finished() {
            player.pause();
        } else {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        let at = player.position();
        if self.cursor != Some(at) {
            self.show_moment(at);
        }
    }

    fn input(&mut self, ctx: &egui::Context) {
        let toggle_explorer = KeyboardShortcut::new(Modifiers::COMMAND, Key::B);
        if ctx.input_mut(|i| i.consume_shortcut(&toggle_explorer)) {
            self.explorer_open = !self.explorer_open;
        }
        // Taken before any widget draws, so a focused button cannot also
        // treat this Space as a click and toggle playback straight back.
        if !ctx.egui_wants_keyboard_input()
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Space))
        {
            self.toggle_play();
        }
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_owned()))
        {
            self.open_external(ctx, path);
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let name = self
                .file
                .as_deref()
                .and_then(Path::file_name)
                .map_or("No file open".into(), |n| n.to_string_lossy());
            ui.label(
                RichText::new(name)
                    .size(26.0)
                    .strong()
                    .color(Color32::WHITE),
            );
            if self.loading {
                ui.spinner();
            }
        });
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(CURSOR));
        }
        if let Some(current) = &self.current {
            match &current.meta.description {
                Some(d) => ui.label(RichText::new(d).size(17.0).color(Color32::from_gray(225))),
                None => ui.label(
                    RichText::new("No description in the metadata")
                        .italics()
                        .weak(),
                ),
            };
            ui.label(
                RichText::new(summary(current))
                    .monospace()
                    .size(12.0)
                    .color(AXIS),
            );
        }
        ui.add_space(8.0);
    }

    fn transport(&mut self, ui: &mut egui::Ui) {
        let playing = self.player.as_ref().is_some_and(Player::is_playing);
        let has_file = self.current.is_some();
        ui.horizontal(|ui| {
            let label = if playing { "Pause" } else { "Play" };
            let play = egui::Button::new(label).min_size(Vec2::new(64.0, 0.0));
            if ui
                .add_enabled(has_file, play)
                .on_hover_text("Space")
                .clicked()
            {
                self.toggle_play();
            }
            if ui
                .add_enabled(has_file, egui::Button::new("Stop"))
                .clicked()
            {
                self.stop();
            }
            ui.separator();
            ui.label("Speed");
            for speed in playback::SPEEDS {
                if ui
                    .selectable_label(self.speed == speed, format!("{speed}×"))
                    .clicked()
                {
                    self.set_speed(speed);
                }
            }
            if let Some(current) = &self.current {
                ui.separator();
                let at = self.cursor.unwrap_or(0) as f64 / f64::from(current.info.sample_rate);
                ui.label(
                    RichText::new(format!(
                        "{} / {}",
                        clock_fine(at),
                        clock(current.info.seconds())
                    ))
                    .monospace(),
                );
            }
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let nyquist = self
            .current
            .as_ref()
            .map_or(self.view.f_max, |c| c.info.nyquist());
        let (fft, view, colormap) = (self.fft, self.view, self.colormap);
        ui.add_space(4.0);
        self.transport(ui);
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label("FFT");
            egui::ComboBox::from_id_salt("fft")
                .selected_text(self.fft.to_string())
                .show_ui(ui, |ui| {
                    for n in spectrogram::FFT_SIZES {
                        ui.selectable_value(&mut self.fft, n, n.to_string());
                    }
                });
            ui.label("Colours");
            egui::ComboBox::from_id_salt("colormap")
                .selected_text(self.colormap.name())
                .show_ui(ui, |ui| {
                    for c in Colormap::ALL {
                        ui.selectable_value(&mut self.colormap, c, c.name());
                    }
                });
            ui.separator();
            // Gain lowers the level drawn at full brightness, so more of a
            // quiet recording lights up; range is how far below it colour
            // still reaches.
            let mut gain = -self.view.top_db;
            ui.add(
                egui::Slider::new(&mut gain, -20.0..=80.0)
                    .text("Gain")
                    .suffix(" dB"),
            );
            self.view.top_db = -gain;
            ui.add(
                egui::Slider::new(&mut self.view.range_db, 20.0..=160.0)
                    .text("Range")
                    .suffix(" dB"),
            );
            ui.separator();
            ui.label("Frequency");
            let speed = f64::from(nyquist) / 400.0;
            ui.add(
                egui::DragValue::new(&mut self.view.f_min)
                    .range(0.0..=self.view.f_max)
                    .speed(speed)
                    .suffix(" Hz"),
            );
            ui.label("to");
            ui.add(
                egui::DragValue::new(&mut self.view.f_max)
                    .range(self.view.f_min..=nyquist)
                    .speed(speed)
                    .suffix(" Hz"),
            );
            if ui.button("Full").clicked() {
                self.view.f_min = 0.0;
                self.view.f_max = nyquist;
            }
            ui.checkbox(&mut self.view.log, "Log");
        });
        ui.add_space(4.0);

        if (self.view.f_min, self.view.f_max) != (view.f_min, view.f_max) {
            self.band_low = self.view.f_min;
            self.band_high = (self.view.f_max < nyquist).then_some(self.view.f_max);
        }
        if self.fft != fft {
            self.analyse(ui.ctx());
            if let Some(sample) = self.cursor {
                self.show_moment(sample);
            }
        }
        if self.view != view || self.colormap != colormap {
            self.texture_stale = true;
        }
    }

    fn spectrogram_panel(&mut self, ui: &mut egui::Ui) {
        let (Some(current), Some(texture), Some(analysis)) =
            (&self.current, &self.texture, &self.analysis)
        else {
            ui.centered_and_justified(|ui| {
                if self.loading || self.current.is_some() {
                    ui.spinner();
                } else {
                    ui.label(RichText::new("Pick a file in the explorer, or drop one here").weak());
                }
            });
            return;
        };

        let area = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(area, Sense::click_and_drag());
        let plot = Rect::from_min_max(
            area.min + Vec2::new(54.0, 4.0),
            area.max - Vec2::new(8.0, 44.0),
        );
        let painter = ui.painter_at(area);
        let full_uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        painter.image(texture.id(), plot, full_uv, Color32::WHITE);

        let (lo, hi) = spectrogram::band(&self.view, current.info.sample_rate, analysis.fft);
        let log = self.view.log;
        let to_t = |f: f32| {
            if log {
                (f / lo).ln() / (hi / lo).ln()
            } else {
                (f - lo) / (hi - lo)
            }
        };
        let from_t = |t: f32| {
            if log {
                lo * (hi / lo).powf(t)
            } else {
                lo + (hi - lo) * t
            }
        };
        for f in freq_ticks(lo, hi, log, plot.height()) {
            let y = plot.bottom() - to_t(f) * plot.height();
            painter.line_segment(
                [Pos2::new(plot.left() - 5.0, y), Pos2::new(plot.left(), y)],
                Stroke::new(1.0, AXIS),
            );
            painter.text(
                Pos2::new(plot.left() - 8.0, y),
                Align2::RIGHT_CENTER,
                hz(f),
                FontId::monospace(11.0),
                AXIS,
            );
        }

        let seconds = current.info.seconds();
        let x_of = |t: f64| plot.left() + (t / seconds) as f32 * plot.width();
        let step = time_step(seconds, plot.width());
        let mut t = 0.0;
        while t <= seconds {
            let x = x_of(t);
            painter.line_segment(
                [
                    Pos2::new(x, plot.bottom()),
                    Pos2::new(x, plot.bottom() + 5.0),
                ],
                Stroke::new(1.0, AXIS),
            );
            painter.text(
                Pos2::new(x, plot.bottom() + 7.0),
                Align2::CENTER_TOP,
                clock(t),
                FontId::monospace(13.0),
                ELAPSED,
            );
            if let Some(start) = &current.meta.start {
                painter.text(
                    Pos2::new(x, plot.bottom() + 25.0),
                    Align2::CENTER_TOP,
                    wall(start.seconds + t),
                    FontId::monospace(10.0),
                    WALL_CLOCK,
                );
            }
            t += step;
        }

        let mut new_cursor = None;
        if (response.clicked() || response.dragged())
            && let Some(pos) = response.interact_pointer_pos()
        {
            let t = ((pos.x - plot.left()) / plot.width()).clamp(0.0, 1.0);
            new_cursor = Some((f64::from(t) * current.info.frames as f64) as usize);
        }
        if let Some(sample) = self.cursor {
            let x = x_of(sample as f64 / f64::from(current.info.sample_rate));
            painter.line_segment(
                [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
                Stroke::new(1.5, CURSOR),
            );
        }
        if let Some(pos) = response.hover_pos().filter(|p| plot.contains(*p)) {
            let f = from_t((plot.bottom() - pos.y) / plot.height());
            let t = f64::from((pos.x - plot.left()) / plot.width()) * seconds;
            painter.line_segment(
                [
                    Pos2::new(plot.left(), pos.y),
                    Pos2::new(plot.right(), pos.y),
                ],
                Stroke::new(0.5, Color32::from_white_alpha(60)),
            );
            // Readout on the side with room, so it is never cut off at the edge.
            let (offset, align) = if pos.x > plot.center().x {
                (Vec2::new(-12.0, -10.0), Align2::RIGHT_BOTTOM)
            } else {
                (Vec2::new(12.0, -10.0), Align2::LEFT_BOTTOM)
            };
            painter.text(
                pos + offset,
                align,
                format!("{}Hz  {}", hz(f), clock_fine(t)),
                FontId::monospace(12.0),
                Color32::WHITE,
            );
        }
        if let Some(sample) = new_cursor {
            self.seek(sample);
        }
    }

    fn spectrum_panel(&self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        let (Some(current), Some(sample)) = (&self.current, self.cursor) else {
            ui.strong("Spectrum");
            ui.add_space(6.0);
            ui.weak("Click the spectrogram to inspect a moment");
            return;
        };
        let at = sample as f64 / f64::from(current.info.sample_rate);
        ui.strong(format!("Spectrum at {}", clock_fine(at)));
        if self.spectrum.len() < 2 {
            return;
        }

        let area = ui.available_rect_before_wrap();
        let plot = Rect::from_min_max(
            area.min + Vec2::new(42.0, 10.0),
            area.max - Vec2::new(10.0, 26.0),
        );
        let painter = ui.painter_at(area);
        let nyquist = current.info.nyquist();
        let lo = self.view.f_min.max(10.0).min(nyquist / 4.0);
        let hi = self.view.f_max.clamp(lo * 4.0, nyquist);
        // The floor follows the spectrogram's, but the top stays at full
        // scale or above, so raising the spectrogram's gain never flattens
        // the peaks here.
        let (top, bottom) = (
            self.view.top_db.max(0.0),
            self.view.top_db - self.view.range_db,
        );
        let x_of = |f: f32| plot.left() + (f / lo).ln() / (hi / lo).ln() * plot.width();
        let y_of =
            |db: f32| plot.top() + ((top - db) / (top - bottom)).clamp(0.0, 1.0) * plot.height();

        for f in freq_ticks(lo, hi, true, plot.width()) {
            let x = x_of(f);
            painter.line_segment(
                [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
                Stroke::new(1.0, GRID),
            );
            painter.text(
                Pos2::new(x, plot.bottom() + 4.0),
                Align2::CENTER_TOP,
                hz(f),
                FontId::monospace(10.0),
                AXIS,
            );
        }
        let mut db = (top / 20.0).floor() * 20.0;
        while db >= bottom {
            let y = y_of(db);
            painter.line_segment(
                [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
                Stroke::new(1.0, GRID),
            );
            painter.text(
                Pos2::new(plot.left() - 6.0, y),
                Align2::RIGHT_CENTER,
                format!("{db:.0}"),
                FontId::monospace(10.0),
                AXIS,
            );
            db -= 20.0;
        }

        let bin_hz = nyquist / (self.spectrum.len() - 1) as f32;
        let points: Vec<Pos2> = self
            .spectrum
            .iter()
            .enumerate()
            .map(|(k, &level)| (k as f32 * bin_hz, level))
            .filter(|(f, _)| (lo..=hi).contains(f))
            .map(|(f, level)| Pos2::new(x_of(f), y_of(level)))
            .collect();
        painter.add(egui::Shape::line(points, Stroke::new(1.2, Color32::WHITE)));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Some(path) = self.inbox.latest() {
            self.open_external(&ctx, path);
        }
        self.poll(&ctx);
        self.input(&ctx);
        self.follow_playback(&ctx);

        let widest = (ui.available_width() / 6.0).max(140.0);
        let mut clicked = None;
        egui::Panel::left("explorer")
            .resizable(true)
            .default_size(widest.min(240.0))
            .min_size(140.0)
            .max_size(widest)
            .show_collapsible(ui, &mut self.explorer_open, |ui| {
                clicked = self.explorer.ui(ui)
            });
        if let Some(path) = clicked {
            self.open(&ctx, path);
        }

        egui::Panel::top("header").show(ui, |ui| self.header(ui));
        egui::Panel::bottom("controls").show(ui, |ui| self.controls(ui));
        self.refresh_texture(&ctx);
        egui::Panel::right("spectrum")
            .resizable(true)
            .default_size(360.0)
            .min_size(220.0)
            .show(ui, |ui| self.spectrum_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.spectrogram_panel(ui));
    }
}

fn summary(c: &Loaded) -> String {
    let info = &c.info;
    let mut parts = vec![
        info.container.clone(),
        format!("{}Hz", hz(info.sample_rate as f32)),
    ];
    parts.extend(info.bits.map(|b| format!("{b}-bit")));
    parts.push(format!("{} ch", info.channels));
    parts.push(clock(info.seconds()));
    parts.extend(c.meta.recorder.clone());
    if let Some(start) = &c.meta.start {
        let time = wall(start.seconds);
        parts.push(match &start.date {
            Some(date) => format!("{date} {time}"),
            None => time,
        });
    }
    parts.join("  ·  ")
}

fn hz(f: f32) -> String {
    if f < 1000.0 {
        return format!("{f:.0}");
    }
    let k = f / 1000.0;
    if (k - k.round()).abs() < 0.05 {
        format!("{k:.0}k")
    } else {
        format!("{k:.1}k")
    }
}

/// Elapsed time as `m:ss`, or `h:mm:ss` from an hour on.
fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn clock_fine(seconds: f64) -> String {
    let hundredths = (seconds.max(0.0) * 100.0).round() as u64;
    format!(
        "{}.{:02}",
        clock((hundredths / 100) as f64),
        hundredths % 100
    )
}

/// Time of day, wrapping past midnight.
fn wall(seconds: f64) -> String {
    let s = seconds.rem_euclid(86_400.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// The smallest tick spacing that leaves room for each label.
fn time_step(seconds: f64, width: f32) -> f64 {
    const STEPS: [f64; 14] = [
        1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0,
    ];
    let fits = f64::from(width / 90.0).max(1.0);
    STEPS
        .into_iter()
        .find(|step| seconds / step <= fits)
        .unwrap_or(14_400.0)
}

fn nice_step(raw: f32) -> f32 {
    let magnitude = 10f32.powf(raw.log10().floor());
    let mantissa = match raw / magnitude {
        m if m <= 1.0 => 1.0,
        m if m <= 2.0 => 2.0,
        m if m <= 2.5 => 2.5,
        m if m <= 5.0 => 5.0,
        _ => 10.0,
    };
    magnitude * mantissa
}

fn freq_ticks(lo: f32, hi: f32, log: bool, length: f32) -> Vec<f32> {
    let mut ticks = Vec::new();
    if log {
        let mut decade = 10f32.powf(lo.log10().floor());
        while decade <= hi {
            ticks.extend(
                [1.0, 2.0, 5.0]
                    .map(|m| decade * m)
                    .into_iter()
                    .filter(|f| (lo..=hi).contains(f)),
            );
            decade *= 10.0;
        }
    } else {
        let step = nice_step((hi - lo) / (length / 55.0).max(2.0));
        let mut f = (lo / step).ceil() * step;
        while f <= hi {
            ticks.push(f);
            f += step;
        }
    }
    ticks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_labels_read_naturally() {
        assert_eq!(hz(440.0), "440");
        assert_eq!(hz(2_500.0), "2.5k");
        assert_eq!(hz(192_000.0), "192k");
    }

    #[test]
    fn clocks_format_elapsed_and_wall_time() {
        assert_eq!(clock(364.0), "6:04");
        assert_eq!(clock(3_725.0), "1:02:05");
        assert_eq!(clock_fine(59.994), "0:59.99");
        assert_eq!(wall(61_454.0 + 86_400.0), "17:04:14");
    }

    #[test]
    fn linear_ticks_step_nicely() {
        let expected: Vec<f32> = (0..=4).map(|i| i as f32 * 5_000.0).collect();
        assert_eq!(freq_ticks(0.0, 24_000.0, false, 275.0), expected);
    }
}
