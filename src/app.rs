//! Window layout, input, and the glue between the UI thread and the
//! workers.

use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Key, KeyboardShortcut, Modifiers, Painter,
    PointerButton, Rect, Response, RichText, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};
use serde::{Deserialize, Serialize};

use crate::audio::{self, Loaded};
use crate::explorer::Explorer;
use crate::finder::Inbox;
use crate::levels::{FLOOR_DB, Level};
use crate::playback::{self, Player};
use crate::probe::{self, Probe};
use crate::spectrogram::{self, Analysis, Channels, Spec, Target, View};
use crate::views::{self, Meters, Span};

const BRIGHTNESS: RangeInclusive<f32> = -20.0..=80.0;
const CONTRAST: RangeInclusive<f32> = 20.0..=160.0;
const GAIN: RangeInclusive<f32> = -60.0..=60.0;
/// Room around a plot for its labels, which also keeps the plot's own
/// dragging clear of the handles that resize the panels around it.
const PLOT_LEFT: f32 = 54.0;
const PLOT_RIGHT: f32 = 10.0;
const PLOT_EDGE: f32 = 6.0;
const TIME_AXIS: f32 = 44.0;
/// How long the view must rest before the part in view is analysed again.
const SETTLE: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// What the window remembers between runs.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    views: Views,
    explorer: bool,
    folder: Option<PathBuf>,
    fft: usize,
    colormap: Colormap,
    brightness: f32,
    contrast: f32,
    /// The band as last set by hand, applied to every file within its
    /// reach: a raised low end stays raised, and a top left at the Nyquist
    /// limit follows each file's own.
    band_low: f32,
    band_high: Option<f32>,
    log: bool,
    channels: Channels,
    speed: u32,
    /// Playback gain, in dB.
    gain: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            views: Views::default(),
            explorer: true,
            folder: None,
            fft: 2048,
            colormap: Colormap::Viridis,
            brightness: 0.0,
            contrast: 90.0,
            band_low: 0.0,
            band_high: None,
            log: false,
            channels: Channels::Mix,
            speed: 1,
            gain: 0.0,
        }
    }
}

impl Settings {
    /// Values from an older or hand-edited settings file, brought within
    /// what the controls offer.
    fn sanitized(mut self) -> Self {
        let within = |value: f32, range: RangeInclusive<f32>, default: f32| {
            if value.is_finite() {
                value.clamp(*range.start(), *range.end())
            } else {
                default
            }
        };
        if !spectrogram::FFT_SIZES.contains(&self.fft) {
            self.fft = 2048;
        }
        if !playback::SPEEDS.contains(&self.speed) {
            self.speed = 1;
        }
        self.brightness = within(self.brightness, BRIGHTNESS, 0.0);
        self.contrast = within(self.contrast, CONTRAST, 90.0);
        self.gain = within(self.gain, GAIN, 0.0);
        self.band_low = within(self.band_low, 0.0..=f32::MAX, 0.0);
        self.band_high = self.band_high.filter(|f| f.is_finite() && *f > 0.0);
        self
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
struct Views {
    spectrogram: bool,
    waveform: bool,
    spectrum: bool,
    metadata: bool,
    timeline: bool,
    meters: bool,
}

impl Default for Views {
    fn default() -> Self {
        Self {
            spectrogram: true,
            waveform: true,
            spectrum: true,
            metadata: false,
            timeline: true,
            meters: true,
        }
    }
}

enum Job {
    Loaded {
        generation: u64,
        result: Result<Box<(Loaded, Analysis)>, String>,
    },
    Analysed {
        generation: u64,
        id: u64,
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

/// A job under way: how far it has got, in thousandths, and the handle
/// that stops it.
struct Running {
    id: u64,
    progress: Arc<AtomicU32>,
    _cancel: Cancel,
}

impl Running {
    fn percent(&self) -> u32 {
        self.progress.load(Ordering::Relaxed) / 10
    }
}

/// An analysis and its images, one per lane.
struct Shown {
    analysis: Analysis,
    textures: Vec<TextureHandle>,
    /// What the images were coloured with.
    look: Option<Look>,
}

#[derive(Clone, Copy, PartialEq)]
struct Look {
    view: View,
    colormap: Colormap,
}

impl Shown {
    fn new(analysis: Analysis) -> Self {
        Self {
            analysis,
            textures: Vec::new(),
            look: None,
        }
    }

    fn refresh(&mut self, ctx: &egui::Context, look: Look, sample_rate: u32, name: &str) {
        if self.look == Some(look) {
            return;
        }
        self.look = Some(look);
        let planes = self.analysis.planes.len();
        // Lanes share the height, so they can share the rows.
        let rows = (1024 / planes.max(1)).max(256);
        for plane in 0..planes {
            let image = spectrogram::colorize(
                &self.analysis,
                plane,
                sample_rate,
                &look.view,
                look.colormap.gradient(),
                rows,
            );
            match self.textures.get_mut(plane) {
                Some(texture) => texture.set(image, TextureOptions::LINEAR),
                None => self.textures.push(ctx.load_texture(
                    format!("{name}-{plane}"),
                    image,
                    TextureOptions::LINEAR,
                )),
            }
        }
    }
}

pub struct App {
    settings: Settings,
    explorer: Explorer,
    inbox: Inbox,
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Job>,
    /// Bumped for every opened file, so results for an earlier one are dropped.
    generation: u64,
    jobs: u64,
    /// The file the window is about: loading, loaded or failed to load.
    file: Option<PathBuf>,
    loading: Option<Running>,
    current: Option<Loaded>,
    error: Option<String>,
    /// The whole file analysed: drawn wherever nothing finer is ready.
    whole: Option<Shown>,
    whole_job: Option<Running>,
    /// The part in view analysed, once zoomed in.
    detail: Option<Shown>,
    detail_job: Option<Running>,
    detail_due: Option<Instant>,
    /// Frames in view.
    view: Range<f64>,
    selection: Option<Range<usize>>,
    /// A selection being dragged out: where the drag started, and where the
    /// pointer is now.
    selecting: Option<Range<f64>>,
    /// The frame the spectrum describes, and where playback starts.
    cursor: Option<usize>,
    probe: Option<Probe>,
    asked: Option<probe::Request>,
    spectrum: Option<probe::Spectrum>,
    /// Created on first play, dropped when another file opens.
    player: Option<Player>,
    meters: Meters,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>, inbox: Inbox) -> Self {
        cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
        inbox.wake(&cc.egui_ctx);
        let settings = cc
            .storage
            .and_then(|s| eframe::get_value::<Settings>(s, eframe::APP_KEY))
            .unwrap_or_default()
            .sanitized();
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            settings,
            explorer: Explorer::default(),
            inbox,
            tx,
            rx,
            generation: 0,
            jobs: 0,
            file: None,
            loading: None,
            current: None,
            error: None,
            whole: None,
            whole_job: None,
            detail: None,
            detail_job: None,
            detail_due: None,
            view: 0.0..0.0,
            selection: None,
            selecting: None,
            cursor: None,
            probe: None,
            asked: None,
            spectrum: None,
            player: None,
            meters: Meters::default(),
        };
        if let Some(path) = initial.or_else(|| app.inbox.latest()) {
            app.open_external(&cc.egui_ctx, path);
        }
        if app.explorer.root().is_none() {
            let folder = app
                .settings
                .folder
                .clone()
                .filter(|f| f.is_dir())
                .or_else(std::env::home_dir);
            if let Some(folder) = folder {
                app.explorer.set_root(&folder);
            }
        }
        app
    }

    /// Whatever arrives from outside the tree (Finder, the command line,
    /// drag and drop): a folder becomes the explorer's root, and a file
    /// opens with its folder shown around it.
    fn open_external(&mut self, ctx: &egui::Context, path: PathBuf) {
        if path.is_dir() {
            self.explorer.set_root(&path);
            self.settings.explorer = true;
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
        self.current = None;
        self.error = None;
        self.whole = None;
        self.whole_job = None;
        self.detail = None;
        self.detail_job = None;
        self.detail_due = None;
        self.view = 0.0..0.0;
        self.selection = None;
        self.selecting = None;
        self.cursor = None;
        self.probe = None;
        self.asked = None;
        self.spectrum = None;
        self.player = None;
        let (job, cancel) = Cancel::new();
        let progress = Arc::new(AtomicU32::new(0));
        self.loading = Some(Running {
            id: 0,
            progress: Arc::clone(&progress),
            _cancel: job,
        });
        let (tx, ctx, generation, spec) =
            (self.tx.clone(), ctx.clone(), self.generation, self.spec());
        std::thread::spawn(move || {
            // A decoder panic on a hostile or damaged file must end as an
            // error, not as a spinner that never stops.
            let result = std::panic::catch_unwind(|| audio::load(&path, spec, &cancel, &progress))
                .unwrap_or_else(|_| Err("the decoder crashed on this file".into()))
                .map(Box::new);
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            // The receiver only goes away when the window closes: nobody to tell.
            let _ = tx.send(Job::Loaded { generation, result });
            ctx.request_repaint();
        });
    }

    fn spec(&self) -> Spec {
        Spec {
            fft: self.settings.fft,
            channels: self.settings.channels,
        }
    }

    fn frames(&self) -> usize {
        self.current.as_ref().map_or(0, |c| c.info.frames)
    }

    fn rate(&self) -> f64 {
        self.current
            .as_ref()
            .map_or(48_000.0, |c| f64::from(c.info.sample_rate))
    }

    fn channel_count(&self) -> usize {
        self.current
            .as_ref()
            .map_or(1, |c| usize::from(c.info.channels))
    }

    fn targets(&self) -> Vec<Target> {
        self.settings.channels.targets(self.channel_count())
    }

    fn channel_name(&self, channel: usize) -> String {
        let name = self
            .current
            .as_ref()
            .and_then(|c| c.meta.channel_names.get(channel))
            .and_then(Option::as_deref);
        match name {
            Some(name) => format!("{} · {name}", channel + 1),
            None => (channel + 1).to_string(),
        }
    }

    fn target_name(&self, target: Target) -> String {
        match target {
            Target::Mix => "Mix".to_owned(),
            Target::Channel(c) => self.channel_name(c),
        }
    }

    fn look(&self) -> Look {
        let nyquist = self.current.as_ref().map_or(f32::MAX, |c| c.info.nyquist());
        let f_max = self.settings.band_high.map_or(nyquist, |f| f.min(nyquist));
        Look {
            view: View {
                brightness: self.settings.brightness,
                contrast: self.settings.contrast,
                f_min: self.settings.band_low.min(f_max),
                f_max,
                log: self.settings.log,
            },
            colormap: self.settings.colormap,
        }
    }

    /// Analyses the whole file, or the part in view, with the current
    /// settings, replacing any analysis of the same kind still under way.
    fn analyse(&mut self, ctx: &egui::Context, whole: bool) {
        let Some(current) = &self.current else { return };
        let frames = current.info.frames;
        let range = if whole {
            0..frames
        } else {
            self.view.start.floor() as usize..(self.view.end.ceil() as usize).min(frames)
        };
        let (source, info) = (current.source.clone(), current.info.clone());
        self.jobs += 1;
        let (id, generation, spec) = (self.jobs, self.generation, self.spec());
        let (job, cancel) = Cancel::new();
        let progress = Arc::new(AtomicU32::new(0));
        let running = Running {
            id,
            progress: Arc::clone(&progress),
            _cancel: job,
        };
        if whole {
            self.whole_job = Some(running);
        } else {
            self.detail_job = Some(running);
        }
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let run = || audio::analyse(&source, &info, spec, range, &cancel, &progress);
            let result = match std::panic::catch_unwind(run) {
                Ok(Ok(Some(analysis))) => Ok(analysis),
                Ok(Ok(None)) => return,
                Ok(Err(e)) => Err(e),
                Err(_) => Err("the analysis crashed on this file".to_owned()),
            };
            let _ = tx.send(Job::Analysed {
                generation,
                id,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(job) = self.rx.try_recv() {
            match job {
                Job::Loaded { generation, result } if generation == self.generation => {
                    self.loading = None;
                    match result {
                        Ok(done) => {
                            let (loaded, analysis) = *done;
                            let stale = analysis.spec != self.spec();
                            self.view = 0.0..loaded.info.frames as f64;
                            self.probe = Some(Probe::new(
                                loaded.source.clone(),
                                usize::from(loaded.info.channels),
                                loaded.info.frames,
                                ctx.clone(),
                            ));
                            self.meters = Meters::default();
                            self.current = Some(loaded);
                            self.whole = Some(Shown::new(analysis));
                            // The settings changed while the file loaded.
                            if stale {
                                self.analyse(ctx, true);
                            }
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Job::Analysed {
                    generation,
                    id,
                    result,
                } if generation == self.generation => {
                    let slot = if self.whole_job.as_ref().is_some_and(|j| j.id == id) {
                        self.whole_job = None;
                        &mut self.whole
                    } else if self.detail_job.as_ref().is_some_and(|j| j.id == id) {
                        self.detail_job = None;
                        &mut self.detail
                    } else {
                        continue;
                    };
                    match result {
                        Ok(analysis) => *slot = Some(Shown::new(analysis)),
                        Err(e) => self.error = Some(e),
                    }
                }
                // Superseded by a newer file or a newer analysis.
                _ => {}
            }
        }
    }

    fn settings_changed(&mut self, ctx: &egui::Context) {
        if self.current.is_none() {
            return;
        }
        self.analyse(ctx, true);
        if self.zoomed() {
            self.analyse(ctx, false);
        } else {
            self.detail = None;
            self.detail_job = None;
        }
    }

    fn update_probe(&mut self) {
        let Some(probe) = &self.probe else { return };
        match probe.take() {
            Some(Ok(spectrum)) => self.spectrum = Some(spectrum),
            Some(Err(e)) => self.error = Some(e),
            None => {}
        }
        let wanted = self.cursor.map(|frame| probe::Request {
            frame,
            fft: self.settings.fft,
            channels: self.settings.channels,
        });
        if let Some(request) = wanted
            && wanted != self.asked
        {
            probe.ask(request);
            self.asked = wanted;
        }
    }

    fn refresh_textures(&mut self, ctx: &egui::Context) {
        if !self.settings.views.spectrogram {
            return;
        }
        let Some(rate) = self.current.as_ref().map(|c| c.info.sample_rate) else {
            return;
        };
        let look = self.look();
        for (shown, name) in [(&mut self.whole, "whole"), (&mut self.detail, "detail")] {
            if let Some(shown) = shown {
                shown.refresh(ctx, look, rate, name);
            }
        }
    }

    fn zoomed(&self) -> bool {
        self.view.start > 0.5 || self.view.end < self.frames() as f64 - 0.5
    }

    /// Shows `len` frames from `start`, kept within the file.
    fn set_view(&mut self, start: f64, len: f64) {
        let frames = self.frames() as f64;
        if frames <= 0.0 {
            return;
        }
        let shortest = (self.rate() / 1000.0).max(64.0).min(frames);
        let len = len.clamp(shortest, frames);
        let start = start.clamp(0.0, frames - len);
        let view = start..start + len;
        if view == self.view {
            return;
        }
        self.view = view;
        if self.zoomed() {
            self.detail_due = Some(Instant::now() + SETTLE);
        } else {
            self.detail = None;
            self.detail_job = None;
            self.detail_due = None;
        }
    }

    fn view_len(&self) -> f64 {
        self.view.end - self.view.start
    }

    fn pan(&mut self, frames: f64) {
        self.set_view(self.view.start + frames, self.view_len());
    }

    /// Zooms by `factor` with `anchor` staying where it is on screen.
    fn zoom_at(&mut self, anchor: f64, factor: f64) {
        let len = self.view_len();
        let t = (anchor - self.view.start) / len;
        let new = len * factor;
        self.set_view(anchor - t * new, new);
    }

    /// Zooms by `factor` around the cursor, or the middle of the view.
    fn zoom(&mut self, factor: f64) {
        let centre = self
            .cursor
            .map_or((self.view.start + self.view.end) / 2.0, |c| c as f64);
        let new = self.view_len() * factor;
        self.set_view(centre - new / 2.0, new);
    }

    fn fit(&mut self) {
        self.set_view(0.0, self.frames() as f64);
    }

    fn zoom_to_selection(&mut self) {
        if let Some(s) = &self.selection {
            let pad = s.len() as f64 * 0.05;
            self.set_view(s.start as f64 - pad, s.len() as f64 + 2.0 * pad);
        }
    }

    /// Pages the view along under the playhead as the original does: once
    /// it passes nine tenths of the view, it moves back to one tenth.
    fn keep_in_view(&mut self, frame: f64) {
        let len = self.view_len();
        let past = frame > self.view.start + 0.9 * len && self.view.end < self.frames() as f64;
        if past || frame < self.view.start {
            self.set_view(frame - 0.1 * len, len);
        }
    }

    fn start_due_analysis(&mut self, ctx: &egui::Context) {
        let Some(due) = self.detail_due else { return };
        let now = Instant::now();
        if now >= due {
            self.detail_due = None;
            self.analyse(ctx, false);
        } else {
            ctx.request_repaint_after(due - now);
        }
    }

    fn seek(&mut self, frame: usize) {
        let frame = frame.min(self.frames());
        self.cursor = Some(frame);
        if let Some(player) = &self.player {
            player.seek(frame);
        }
    }

    fn seek_by(&mut self, seconds: f64) {
        let at = self.cursor.unwrap_or(0) as f64 + seconds * self.rate();
        self.seek(at.max(0.0) as usize);
    }

    /// Makes `selection` the selection. As in the original, a new one
    /// repeats and starts playing at once, and clearing it stops the
    /// repeat.
    fn select(&mut self, selection: Option<Range<usize>>) {
        let had = self.selection.is_some();
        self.selection = selection.clone();
        match selection {
            Some(range) => {
                self.cursor = Some(range.start);
                if self.ensure_player(range.start)
                    && let Some(player) = &self.player
                {
                    player.set_loop(Some(range));
                    player.play();
                }
            }
            None if had => {
                if let Some(player) = &self.player {
                    player.set_loop(None);
                }
            }
            None => {}
        }
    }

    fn ensure_player(&mut self, start: usize) -> bool {
        if self.player.is_some() {
            return true;
        }
        let Some(current) = &self.current else {
            return false;
        };
        match Player::new(&current.source, &current.info, start, self.settings.speed) {
            Ok(player) => {
                player.set_gain(self.settings.gain);
                if let Some(range) = &self.selection {
                    player.set_loop(Some(range.clone()));
                }
                self.player = Some(player);
                true
            }
            Err(e) => {
                self.error = Some(e);
                false
            }
        }
    }

    fn toggle_play(&mut self) {
        let frames = self.frames();
        if frames == 0 {
            return;
        }
        // From the cursor, or from the start when it sits at the end.
        let start = self.cursor.filter(|&c| c < frames).unwrap_or(0);
        if !self.ensure_player(start) {
            return;
        }
        let Some(player) = &self.player else { return };
        if player.is_playing() {
            player.pause();
            self.cursor = Some(player.position());
        } else {
            player.play();
        }
    }

    /// Back to the start: of the selection when there is one, else of the
    /// file.
    fn stop(&mut self) {
        let home = self.selection.as_ref().map_or(0, |s| s.start);
        if let Some(player) = &self.player {
            player.pause();
            player.seek(home);
        }
        if self.current.is_some() {
            self.cursor = Some(home);
        }
    }

    fn set_speed(&mut self, speed: u32) {
        self.settings.speed = speed;
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
        self.cursor = Some(at);
        if self.selection.is_none() {
            self.keep_in_view(at as f64);
        }
    }

    fn input(&mut self, ctx: &egui::Context) {
        let toggle_explorer = KeyboardShortcut::new(Modifiers::COMMAND, Key::B);
        if ctx.input_mut(|i| i.consume_shortcut(&toggle_explorer)) {
            self.settings.explorer = !self.settings.explorer;
        }
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_owned()))
        {
            self.open_external(ctx, path);
        }
        // Taken before any widget draws, so a focused button cannot also
        // act on these keys, and never while text is being typed.
        if ctx.egui_wants_keyboard_input() || self.current.is_none() {
            return;
        }
        let pressed = |modifiers, key| ctx.input_mut(|i| i.consume_key(modifiers, key));
        let plain = |key| pressed(Modifiers::NONE, key);
        if plain(Key::Space) {
            self.toggle_play();
        }
        // Shift first: a plain arrow would take the shifted one too.
        if pressed(Modifiers::SHIFT, Key::ArrowLeft) {
            self.seek_by(-10.0);
        }
        if pressed(Modifiers::SHIFT, Key::ArrowRight) {
            self.seek_by(10.0);
        }
        if plain(Key::ArrowLeft) {
            self.seek_by(-1.0);
        }
        if plain(Key::ArrowRight) {
            self.seek_by(1.0);
        }
        if plain(Key::Home) {
            self.seek(0);
        }
        if plain(Key::End) {
            self.seek(self.frames());
        }
        for (key, step) in [(Key::ArrowUp, 5.0), (Key::ArrowDown, -5.0)] {
            if plain(key) {
                self.settings.brightness =
                    (self.settings.brightness + step).clamp(*BRIGHTNESS.start(), *BRIGHTNESS.end());
            }
        }
        if plain(Key::Plus) || plain(Key::Equals) {
            self.zoom(0.5);
        }
        if plain(Key::Minus) {
            self.zoom(2.0);
        }
        if plain(Key::F) {
            self.fit();
        }
        if plain(Key::S) {
            self.zoom_to_selection();
        }
        if plain(Key::Escape) {
            self.select(None);
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            let views = &mut self.settings.views;
            ui.label(RichText::new("Show").color(views::AXIS));
            ui.checkbox(&mut self.settings.explorer, "Files")
                .on_hover_text("⌘B");
            ui.checkbox(&mut views.spectrogram, "Spectrogram");
            ui.checkbox(&mut views.waveform, "Waveform");
            ui.checkbox(&mut views.spectrum, "Spectrum");
            ui.checkbox(&mut views.metadata, "Metadata");
            ui.checkbox(&mut views.timeline, "Timeline");
            ui.checkbox(&mut views.meters, "Meters");
        });
        ui.add_space(4.0);
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
            if let Some(loading) = &self.loading {
                ui.spinner();
                ui.label(format!("{}%", loading.percent()));
            }
        });
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(views::CURSOR));
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
                    .color(views::AXIS),
            );
        }
        ui.add_space(6.0);
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let before = (self.settings.fft, self.settings.channels);
        ui.add_space(4.0);
        self.transport(ui);
        ui.separator();
        self.display(ui);
        ui.add_space(4.0);
        if (self.settings.fft, self.settings.channels) != before {
            self.settings_changed(ui.ctx());
        }
    }

    fn transport(&mut self, ui: &mut egui::Ui) {
        let playing = self.player.as_ref().is_some_and(Player::is_playing);
        let has_file = self.current.is_some();
        ui.horizontal_wrapped(|ui| {
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
                    .selectable_label(self.settings.speed == speed, format!("{speed}×"))
                    .clicked()
                {
                    self.set_speed(speed);
                }
            }
            ui.separator();
            ui.label("Gain").on_hover_text(
                "Playback volume: raise it for quiet recordings, lower it for 32-bit float files that go past full scale",
            );
            let mut gain = self.settings.gain;
            ui.add(egui::Slider::new(&mut gain, GAIN).step_by(1.0).suffix(" dB"));
            if gain != self.settings.gain {
                self.settings.gain = gain;
                if let Some(player) = &self.player {
                    player.set_gain(gain);
                }
            }
            ui.separator();
            if let Some(current) = &self.current {
                let rate = f64::from(current.info.sample_rate);
                let at = self.cursor.unwrap_or(0) as f64 / rate;
                ui.label(
                    RichText::new(format!(
                        "{} / {}",
                        views::clock_fine(at),
                        views::clock(current.info.seconds())
                    ))
                    .monospace(),
                );
                ui.separator();
            }
            ui.label("Zoom");
            if ui
                .add_enabled(has_file, egui::Button::new("−"))
                .on_hover_text("-")
                .clicked()
            {
                self.zoom(2.0);
            }
            if ui
                .add_enabled(has_file, egui::Button::new("+"))
                .on_hover_text("+")
                .clicked()
            {
                self.zoom(0.5);
            }
            if ui
                .add_enabled(has_file, egui::Button::new("Fit"))
                .on_hover_text("F")
                .clicked()
            {
                self.fit();
            }
            if ui
                .add_enabled(self.selection.is_some(), egui::Button::new("Selection"))
                .on_hover_text("S")
                .clicked()
            {
                self.zoom_to_selection();
            }
            if let Some(s) = &self.selection {
                let rate = self.rate();
                let (from, to) = (s.start as f64 / rate, s.end as f64 / rate);
                ui.label(
                    RichText::new(format!(
                        "{} to {} ({:.2} s)",
                        views::clock_fine(from),
                        views::clock_fine(to),
                        to - from
                    ))
                    .monospace()
                    .color(views::AXIS),
                )
                .on_hover_text("Esc clears it");
            }
        });
    }

    fn display(&mut self, ui: &mut egui::Ui) {
        let nyquist = self.current.as_ref().map_or(96_000.0, |c| c.info.nyquist());
        let channels = self.channel_count();
        let names: Vec<String> = (0..channels).map(|c| self.channel_name(c)).collect();
        let look = self.look().view;
        ui.horizontal_wrapped(|ui| {
            ui.label("FFT");
            egui::ComboBox::from_id_salt("fft")
                .selected_text(self.settings.fft.to_string())
                .show_ui(ui, |ui| {
                    for n in spectrogram::FFT_SIZES {
                        ui.selectable_value(&mut self.settings.fft, n, n.to_string());
                    }
                });
            ui.label("Colours");
            egui::ComboBox::from_id_salt("colormap")
                .selected_text(self.settings.colormap.name())
                .show_ui(ui, |ui| {
                    for c in Colormap::ALL {
                        ui.selectable_value(&mut self.settings.colormap, c, c.name());
                    }
                });
            if channels > 1 {
                ui.label("Channels");
                let shown = match self.settings.channels.targets(channels).as_slice() {
                    [Target::Mix] => "Mix".to_owned(),
                    [Target::Channel(c)] => names[*c].clone(),
                    _ => "All".to_owned(),
                };
                egui::ComboBox::from_id_salt("channels")
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        let choice = &mut self.settings.channels;
                        ui.selectable_value(choice, Channels::Mix, "Mix");
                        ui.selectable_value(choice, Channels::All, "All, one above another");
                        for (c, name) in names.iter().enumerate() {
                            ui.selectable_value(choice, Channels::One(c), name);
                        }
                    });
            }
            ui.separator();
            ui.label("Brightness").on_hover_text(
                "Added to every level before colouring: right is brighter. ↑ and ↓ step it by 5 dB",
            );
            ui.add(
                egui::Slider::new(&mut self.settings.brightness, BRIGHTNESS)
                    .step_by(1.0)
                    .suffix(" dB"),
            );
            ui.label("Contrast").on_hover_text(
                "How far below full brightness a level still gets colour: lower is more contrast",
            );
            ui.add(
                egui::Slider::new(&mut self.settings.contrast, CONTRAST)
                    .step_by(1.0)
                    .suffix(" dB"),
            );
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Frequency");
            let (mut lo, mut hi) = (f64::from(look.f_min), f64::from(look.f_max));
            ui.label("Min")
                .on_hover_text("Drag, or click the number and type: 200, 1.5k, 12 kHz");
            let min_changed = ui.add(frequency_slider(&mut lo, 0.0..=hi)).changed();
            ui.label("Max")
                .on_hover_text("Drag, or click the number and type: 200, 1.5k, 12 kHz");
            let max_changed = ui
                .add(frequency_slider(&mut hi, lo.max(10.0)..=f64::from(nyquist)))
                .changed();
            if min_changed || max_changed {
                self.settings.band_low = lo as f32;
                self.settings.band_high = ((hi as f32) < nyquist - 0.5).then_some(hi as f32);
            }
            if ui.button("Full").clicked() {
                self.settings.band_low = 0.0;
                self.settings.band_high = None;
            }
            ui.checkbox(&mut self.settings.log, "Log");
        });
    }

    fn placeholder(&self, ui: &mut egui::Ui) {
        ui.centered_and_justified(|ui| {
            if self.loading.is_some() {
                ui.spinner();
            } else if self.error.is_none() {
                ui.label(RichText::new("Pick a file in the explorer, or drop one here").weak());
            }
        });
    }

    /// The views that follow time: the spectrogram over the waveform, with
    /// the time axis under whichever is lower.
    fn central(&mut self, ui: &mut egui::Ui) {
        if self.current.is_none() {
            self.placeholder(ui);
            return;
        }
        let views = self.settings.views;
        if views.spectrogram && views.waveform {
            egui::Panel::bottom("waveform")
                .frame(egui::Frame::NONE)
                .resizable(true)
                .default_size(180.0)
                .min_size(70.0)
                .show(ui, |ui| self.waveform_view(ui, true));
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.spectrogram_view(ui, false));
        } else if views.spectrogram {
            self.spectrogram_view(ui, true);
        } else {
            self.waveform_view(ui, true);
        }
    }

    fn side(&mut self, ui: &mut egui::Ui) {
        Self::claim(ui);
        let views = self.settings.views;
        if views.spectrum && views.metadata {
            egui::Panel::bottom("metadata")
                .frame(egui::Frame::NONE)
                .resizable(true)
                .default_size(ui.available_height() / 2.0)
                .min_size(80.0)
                .show(ui, |ui| self.metadata_view(ui));
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.spectrum_view(ui));
        } else if views.spectrum {
            self.spectrum_view(ui);
        } else {
            self.metadata_view(ui);
        }
    }

    /// Claims the whole of `ui`, whatever the view then shows: a panel keeps
    /// only the size its contents take, so a view that just paints, or that
    /// shows a line of text before a file opens, would shrink its panel back
    /// to the minimum and keep it there.
    fn claim(ui: &mut egui::Ui) -> Rect {
        let area = ui.available_rect_before_wrap();
        ui.take_available_space();
        area
    }

    fn plot_rect(ui: &mut egui::Ui, axis: bool) -> (Rect, Rect) {
        let area = Self::claim(ui);
        let bottom = if axis { TIME_AXIS } else { PLOT_EDGE };
        let plot = Rect::from_min_max(
            area.min + Vec2::new(PLOT_LEFT, PLOT_EDGE),
            area.max - Vec2::new(PLOT_RIGHT, bottom),
        );
        (area, plot)
    }

    fn spectrogram_view(&mut self, ui: &mut egui::Ui, axis: bool) {
        let (area, plot) = Self::plot_rect(ui, axis);
        if plot.width() < 20.0 || plot.height() < 20.0 {
            return;
        }
        let Some(current) = &self.current else { return };
        let response = ui.interact(plot, ui.id().with("spectrogram"), Sense::click_and_drag());
        let painter = ui.painter_at(area);
        let span = Span::new(plot, &self.view);
        let rate = f64::from(current.info.sample_rate);
        let targets = self.targets();
        let lanes = views::lanes(plot, targets.len());
        let view = self.look().view;
        let (lo, hi) = spectrogram::band(&view, current.info.sample_rate, self.settings.fft);
        for (i, (lane, target)) in lanes.iter().zip(&targets).enumerate() {
            painter.rect_filled(*lane, 0.0, Color32::BLACK);
            for shown in [&self.whole, &self.detail].into_iter().flatten() {
                if shown.analysis.targets == targets
                    && let Some(texture) = shown.textures.get(i)
                {
                    views::place(&painter, *lane, span, texture.id(), &shown.analysis.range);
                }
            }
            views::freq_axis(&painter, *lane, lo, hi, view.log);
            if current.info.channels > 1 {
                views::lane_label(&painter, *lane, &self.target_name(*target));
            }
        }
        if axis {
            views::time_axis(
                &painter,
                plot,
                span,
                rate,
                current.meta.start.as_ref().map(|s| s.seconds),
            );
        }
        self.overlays(&painter, plot, span);
        if let Some(pos) = response.hover_pos()
            && let Some(lane) = lanes.iter().find(|l| l.contains(pos))
        {
            let f = views::freq_at((lane.bottom() - pos.y) / lane.height(), lo, hi, view.log);
            let t = span.frame(pos.x) / rate;
            painter.hline(
                lane.x_range(),
                pos.y,
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
                format!("{}Hz  {}", views::hz(f), views::clock_fine(t)),
                FontId::monospace(12.0),
                Color32::WHITE,
            );
        }
        self.busy(&painter, plot);
        self.plot_input(ui, &response, span);
    }

    fn waveform_view(&mut self, ui: &mut egui::Ui, axis: bool) {
        let (area, plot) = Self::plot_rect(ui, axis);
        if plot.width() < 20.0 || plot.height() < 20.0 {
            return;
        }
        let Some(current) = &self.current else { return };
        let response = ui.interact(plot, ui.id().with("waveform"), Sense::click_and_drag());
        let painter = ui.painter_at(area);
        let span = Span::new(plot, &self.view);
        let channels = usize::from(current.info.channels);
        let detail = self.detail.as_ref().map(|d| &d.analysis);
        for (c, lane) in views::lanes(plot, channels).into_iter().enumerate() {
            painter.rect_filled(lane, 0.0, Color32::from_gray(12));
            views::centre_line(&painter, lane);
            let color = views::PALETTE[c % views::PALETTE.len()];
            if let Some(whole) = &self.whole {
                let a = &whole.analysis;
                // Only where the finer analysis does not reach.
                let gaps = match detail {
                    Some(d) => vec![
                        lane.left()..=span.x(d.range.start as f64),
                        span.x(d.range.end as f64)..=lane.right(),
                    ],
                    None => vec![lane.x_range().into()],
                };
                for gap in gaps.into_iter().filter(|g| g.end() > g.start()) {
                    let clipped =
                        painter.with_clip_rect(Rect::from_x_y_ranges(gap, lane.y_range()));
                    views::waveform(&clipped, lane, span, &a.envelope[c], &a.range, color);
                }
            }
            if let Some(d) = detail {
                views::waveform(&painter, lane, span, &d.envelope[c], &d.range, color);
            }
            if channels > 1 {
                views::lane_label(&painter, lane, &self.channel_name(c));
            }
        }
        if axis {
            let rate = f64::from(current.info.sample_rate);
            views::time_axis(
                &painter,
                plot,
                span,
                rate,
                current.meta.start.as_ref().map(|s| s.seconds),
            );
        }
        self.overlays(&painter, plot, span);
        if !self.settings.views.spectrogram {
            self.busy(&painter, plot);
        }
        self.plot_input(ui, &response, span);
    }

    fn overlays(&self, painter: &Painter, plot: Rect, span: Span) {
        let selection = match &self.selecting {
            Some(s) => Some(s.start.min(s.end)..s.start.max(s.end)),
            None => self
                .selection
                .as_ref()
                .map(|s| s.start as f64..s.end as f64),
        };
        if let Some(s) = &selection {
            views::selection(painter, plot, span, s);
        }
        if let Some(c) = self.cursor {
            views::cursor(painter, plot, span, c as f64);
        }
    }

    fn busy(&self, painter: &Painter, plot: Rect) {
        if let Some(job) = self.detail_job.as_ref().or(self.whole_job.as_ref()) {
            painter.text(
                plot.right_top() + Vec2::new(-8.0, 8.0),
                Align2::RIGHT_TOP,
                format!("Analysing {}%", job.percent()),
                FontId::proportional(12.0),
                Color32::WHITE,
            );
        }
    }

    /// Mouse on a plot, as in the original: click to seek, drag to select,
    /// right-drag to pan, pinch or ⌘/Ctrl-scroll to zoom, sideways scroll to
    /// pan.
    fn plot_input(&mut self, ui: &egui::Ui, response: &Response, span: Span) {
        let (frames, rate) = (self.frames(), self.rate());
        if response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        }
        if response.drag_started_by(PointerButton::Primary) {
            let from = ui
                .input(|i| i.pointer.press_origin())
                .map_or(span.start, |p| span.frame(p.x));
            self.selecting = Some(from..from);
        }
        if response.dragged_by(PointerButton::Primary)
            && let (Some(selecting), Some(pos)) =
                (&mut self.selecting, response.interact_pointer_pos())
        {
            selecting.end = span.frame(pos.x);
        }
        if response.drag_stopped_by(PointerButton::Primary)
            && let Some(s) = self.selecting.take()
        {
            let (from, to) = (s.start.min(s.end), s.start.max(s.end));
            // Under a tenth of a second is a slip of the hand, as in the
            // original.
            if to - from >= 0.1 * rate {
                self.select(Some(from as usize..(to as usize).min(frames)));
            } else {
                self.select(None);
            }
        }
        if response.clicked_by(PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
        {
            self.select(None);
            self.seek(span.frame(pos.x) as usize);
        }
        if response.dragged_by(PointerButton::Secondary) {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            self.pan(-f64::from(response.drag_delta().x / span.width) * span.len);
        }
        if let Some(pos) = response.hover_pos() {
            let (zoom, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta()));
            if zoom != 1.0 {
                self.zoom_at(span.frame(pos.x), 1.0 / f64::from(zoom));
            }
            if scroll.x != 0.0 {
                self.pan(-f64::from(scroll.x / span.width) * span.len);
            }
        }
    }

    fn timeline_view(&mut self, ui: &mut egui::Ui) {
        let Some(current) = &self.current else { return };
        let area = Self::claim(ui);
        let rect = Rect::from_min_max(area.min + Vec2::new(0.0, 4.0), area.max);
        if rect.height() < 4.0 {
            return;
        }
        let response = ui.interact(rect, ui.id().with("timeline"), Sense::click_and_drag());
        let painter = ui.painter_at(area);
        let frames = current.info.frames;
        views::timeline(
            &painter,
            rect,
            &current.timeline,
            frames,
            &self.view,
            self.cursor,
        );
        if response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        // Click or drag moves the view to be centred there.
        if (response.clicked() || response.dragged())
            && let Some(pos) = response.interact_pointer_pos()
        {
            let centre =
                f64::from(((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0)) * frames as f64;
            let len = self.view_len();
            self.set_view(centre - len / 2.0, len);
        }
    }

    fn meters_view(&mut self, ui: &mut egui::Ui) {
        let Some(current) = &self.current else { return };
        let channels = usize::from(current.info.channels);
        let labels: Vec<String> = if channels == 1 {
            vec!["M".to_owned()]
        } else {
            (1..=channels).map(|c| c.to_string()).collect()
        };
        // What is heard: the file's level plus the playback gain, over the
        // stretch of file one screen refresh covers at this speed.
        let levels = self
            .player
            .as_ref()
            .filter(|p| p.is_playing())
            .map(|player| {
                let span =
                    f64::from(current.info.sample_rate) * f64::from(self.settings.speed) / 20.0;
                let gain = self.settings.gain;
                current
                    .levels
                    .at(player.position(), span as usize)
                    .into_iter()
                    .map(|level| lift(level, gain))
                    .collect::<Vec<_>>()
            });
        ui.add_space(4.0);
        self.meters.ui(ui, levels.as_deref(), &labels);
    }

    fn spectrum_view(&mut self, ui: &mut egui::Ui) {
        Self::claim(ui);
        ui.add_space(4.0);
        let (Some(current), Some(spectrum)) = (&self.current, &self.spectrum) else {
            ui.strong("Spectrum");
            ui.add_space(6.0);
            ui.weak("Click the spectrogram to inspect a moment");
            return;
        };
        let at = spectrum.request.frame as f64 / f64::from(current.info.sample_rate);
        ui.strong(format!("Spectrum at {}", views::clock_fine(at)));
        let area = ui.available_rect_before_wrap();
        let plot = Rect::from_min_max(
            area.min + Vec2::new(42.0, 10.0),
            area.max - Vec2::new(10.0, 26.0),
        );
        if plot.width() < 20.0 || plot.height() < 20.0 {
            return;
        }
        let painter = ui.painter_at(area);
        let nyquist = current.info.nyquist();
        let view = self.look().view;
        let lo = view.f_min.max(10.0).min(nyquist / 4.0);
        let hi = view.f_max.clamp(lo * 4.0, nyquist);
        // The floor follows the spectrogram's, but the top stays at full
        // scale or above, so a brighter spectrogram never flattens the
        // peaks here.
        let top = (-view.brightness).max(0.0);
        let bottom = -view.brightness - view.contrast;
        let curves: Vec<views::Curve<'_>> = spectrum
            .curves
            .iter()
            .map(|(target, levels)| views::Curve {
                name: self.target_name(*target),
                color: match target {
                    Target::Mix => Color32::WHITE,
                    Target::Channel(c) => views::PALETTE[c % views::PALETTE.len()],
                },
                levels,
            })
            .collect();
        views::spectrum(&painter, plot, &curves, nyquist, (lo, hi), (top, bottom));
    }

    fn metadata_view(&self, ui: &mut egui::Ui) {
        Self::claim(ui);
        ui.add_space(4.0);
        ui.strong("Metadata");
        match &self.current {
            Some(current) => views::metadata(ui, &current.details),
            None => {
                ui.weak("Open a file to see what it says about itself");
            }
        }
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
        self.start_due_analysis(&ctx);
        self.update_probe();
        if self.loading.is_some() || self.whole_job.is_some() || self.detail_job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let widest = (ui.available_width() / 6.0).max(140.0);
        let mut clicked = None;
        egui::Panel::left("explorer")
            .resizable(true)
            .default_size(widest.min(240.0))
            .min_size(140.0)
            .max_size(widest)
            .show_collapsible(ui, &mut self.settings.explorer, |ui| {
                clicked = self.explorer.ui(ui)
            });
        if let Some(path) = clicked {
            self.open(&ctx, path);
        }

        egui::Panel::top("header").show(ui, |ui| self.header(ui));
        egui::Panel::bottom("controls").show(ui, |ui| self.controls(ui));
        let views = self.settings.views;
        if self.current.is_some() && views.meters {
            egui::Panel::bottom("meters").show(ui, |ui| self.meters_view(ui));
        }
        if self.current.is_some() && views.timeline {
            egui::Panel::bottom("timeline")
                .resizable(true)
                .default_size(44.0)
                .size_range(28.0..=240.0)
                .show(ui, |ui| self.timeline_view(ui));
        }
        self.refresh_textures(&ctx);
        let central = views.spectrogram || views.waveform;
        let side = views.spectrum || views.metadata;
        if central && side {
            egui::Panel::right("side")
                .resizable(true)
                .default_size(360.0)
                .min_size(220.0)
                .show(ui, |ui| self.side(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            if central {
                self.central(ui);
            } else if side {
                self.side(ui);
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new("Every view is switched off: tick one at the top").weak(),
                    );
                });
            }
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.settings.folder = self.explorer.root().map(Path::to_owned);
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }
}

/// A frequency slider on a log scale, with the value typed in Hz or kHz.
fn frequency_slider(value: &mut f64, range: RangeInclusive<f64>) -> egui::Slider<'_> {
    egui::Slider::new(value, range)
        .logarithmic(true)
        .smallest_positive(10.0)
        .custom_formatter(|v, _| views::hz_field(v))
        .custom_parser(views::parse_hz)
}

/// `level` played with `gain` dB on: silence stays silence.
fn lift(level: Level, gain: f32) -> Level {
    let raise = |db: f32| {
        if db > FLOOR_DB {
            (db + gain).max(FLOOR_DB)
        } else {
            FLOOR_DB
        }
    };
    Level {
        rms_db: raise(level.rms_db),
        peak_db: raise(level.peak_db),
    }
}

fn summary(c: &Loaded) -> String {
    let info = &c.info;
    let mut parts = vec![
        info.container.clone(),
        format!("{}Hz", views::hz(info.sample_rate as f32)),
    ];
    parts.extend(info.bits.map(|b| format!("{b}-bit")));
    parts.push(format!("{} ch", info.channels));
    parts.push(views::clock(info.seconds()));
    parts.extend(c.meta.recorder.clone());
    if let Some(start) = &c.meta.start {
        let time = views::wall(start.seconds);
        parts.push(match &start.date {
            Some(date) => format!("{date} {time}"),
            None => time,
        });
    }
    parts.join("  ·  ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::Storage;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Memory(HashMap<String, String>);

    impl eframe::Storage for Memory {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }

        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }

        fn flush(&mut self) {}
    }

    #[test]
    fn settings_come_back_as_they_were_left() {
        let mut storage = Memory::default();
        let mut settings = Settings::default();
        settings.views.metadata = true;
        settings.views.waveform = false;
        settings.channels = Channels::One(3);
        settings.brightness = 25.0;
        eframe::set_value(&mut storage, eframe::APP_KEY, &settings);
        let back: Settings = eframe::get_value(&storage, eframe::APP_KEY).unwrap();
        assert!(back.views.metadata && !back.views.waveform);
        assert_eq!((back.channels, back.brightness), (Channels::One(3), 25.0));
    }

    #[test]
    fn settings_missing_from_an_older_file_take_their_defaults() {
        let mut storage = Memory::default();
        storage.set_string(
            eframe::APP_KEY,
            "(fft: 4096, views: (meters: false))".to_owned(),
        );
        let back: Settings = eframe::get_value(&storage, eframe::APP_KEY).unwrap();
        assert_eq!((back.fft, back.contrast), (4096, 90.0));
        assert!(!back.views.meters && back.views.spectrogram);
    }

    #[test]
    fn settings_out_of_range_are_brought_back() {
        let settings = Settings {
            fft: 1000,
            speed: 3,
            brightness: 500.0,
            contrast: f32::NAN,
            gain: -90.0,
            ..Settings::default()
        }
        .sanitized();
        assert_eq!((settings.fft, settings.speed), (2048, 1));
        assert_eq!(
            (settings.brightness, settings.contrast, settings.gain),
            (80.0, 90.0, -60.0)
        );
    }

    #[test]
    fn gain_lifts_levels_but_silence_stays_silent() {
        let quiet = Level {
            rms_db: -40.0,
            peak_db: FLOOR_DB,
        };
        assert_eq!(
            lift(quiet, 30.0),
            Level {
                rms_db: -10.0,
                peak_db: FLOOR_DB
            }
        );
        assert_eq!(lift(quiet, -70.0).rms_db, FLOOR_DB);
    }
}
