//! Window layout, input, and the glue between the UI thread and the
//! workers.

use std::collections::HashSet;
use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Key, KeyboardShortcut, Label, Modifiers, Painter,
    PointerButton, Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind, TextEdit,
    TextureHandle, TextureOptions, Vec2, ViewportCommand,
};
use serde::{Deserialize, Serialize};

use crate::audio::{self, Loaded};
use crate::edit::Edits;
use crate::explorer::{Explorer, Parts, Shortcut};
use crate::export;
use crate::finder::Inbox;
use crate::levels::{FLOOR_DB, Level};
use crate::playback::{self, Player, Region, Speed};
use crate::probe::{self, Probe};
use crate::rename;
use crate::rename_ui::{self, Renamer, Request};
use crate::save;
use crate::spectrogram::{self, Analysis, Channels, HIGHEST_HZ, Spec, Target, View};
use crate::views::{self, AreaStyle, MarkerAction, Meters, Span};
use crate::wav::Marker;

const BRIGHTNESS: RangeInclusive<f32> = -20.0..=80.0;
const CONTRAST: RangeInclusive<f32> = 20.0..=160.0;
const GAIN: RangeInclusive<f32> = -60.0..=60.0;
/// Semitones: four octaves either way.
const PITCH: RangeInclusive<f32> = -48.0..=48.0;
/// An area's edge, in points, and how much of its colour fills it.
const AREA_WIDTH: RangeInclusive<f32> = 1.0..=8.0;
const AREA_FILL: RangeInclusive<f32> = 0.0..=0.6;
/// The lowest a frequency slider or arrow key goes above zero.
const LOWEST_HZ: f32 = 10.0;
/// Room around a plot for its labels, which also keeps the plot's own
/// dragging clear of the handles that resize the panels around it.
const PLOT_LEFT: f32 = 54.0;
const PLOT_RIGHT: f32 = 10.0;
const PLOT_EDGE: f32 = 6.0;
const TIME_AXIS: f32 = 44.0;
/// Closes the bulk rename window, as Esc does.
const CLOSE_RENAME: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);

fn rename_viewport() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("bulk rename")
}

/// How far past an end of the part in view a drag on the whole file's
/// waveform takes that end.
const GRIP: f32 = 6.0;
/// How long the view must rest before the part in view is analysed again.
const SETTLE: Duration = Duration::from_millis(150);
/// How long the spectrogram says where an export went.
const EXPORTED_NOTE: Duration = Duration::from_secs(6);
const PICKED: Color32 = Color32::from_rgba_unmultiplied_const(255, 140, 50, 200);
const HEARING: &str = "People hear from about 20 Hz to 20 kHz. Whales call and listen below that, and bats far above it";
const PICKING: &str = "Click here to pick it for the keys: the arrows step it, with Shift further, R resets it and Shift+R resets every slider";

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
    /// The band as last set by hand, applied to every file: a raised low
    /// end stays raised, a top typed in past what a file holds stays there,
    /// and a top left at the Nyquist limit follows each file's own.
    band_low: f32,
    band_high: Option<f32>,
    log: bool,
    channels: Channels,
    /// Playback speed: 2 is twice as fast, 0.125 an eighth.
    speed: f64,
    /// Any speed on a slider, rather than the presets.
    speed_slider: bool,
    /// Playback gain, in dB.
    gain: f32,
    /// With a channel muted, the rest play in both speakers rather than on
    /// their own side.
    both_sides: bool,
    /// What the spectrogram only just shows fades into the sound rather than
    /// cutting in, with only what it shows heard.
    soft_edge: bool,
    /// A new area picked on the spectrogram joins those picked before,
    /// rather than taking their place.
    multiple_areas: bool,
    areas: AreaLook,
    /// The menu under the gear button at the top is open.
    menu: bool,
    tools: Tools,
    /// Semitones the sound, and the frequencies shown, move with the pitch
    /// shift on.
    pitch: f32,
    /// Folders and files kept at the top of the explorer.
    shortcuts: Vec<Shortcut>,
}

/// How the areas picked on the spectrogram are drawn, as the window the
/// gear's menu opens sets it.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct AreaLook {
    /// The edge's colour with one area at a time, and with Multiple areas.
    one: [u8; 3],
    several: [u8; 3],
    /// The edge's width, in points.
    width: f32,
    /// A dark line under the edge, so it shows on the light colours of the
    /// colour maps too.
    rim: bool,
    /// How much of the edge's colour fills the area.
    fill: f32,
}

impl Default for AreaLook {
    fn default() -> Self {
        Self {
            one: [255; 3],
            several: [255; 3],
            width: 3.0,
            rim: true,
            fill: 0.18,
        }
    }
}

/// The listening tools switched on, at the top of the window.
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
struct Tools {
    pitch: bool,
    slow: bool,
    /// Only what the spectrogram shows is heard.
    shown: bool,
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
            speed: 1.0,
            speed_slider: false,
            gain: 0.0,
            both_sides: true,
            soft_edge: true,
            multiple_areas: false,
            areas: AreaLook::default(),
            menu: false,
            tools: Tools::default(),
            pitch: 0.0,
            shortcuts: Vec::new(),
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
        let slow = self.tools.slow;
        self.speed = if self.speed_slider {
            Speed::new(self.speed).value()
        } else {
            Speed::preset(self.speed)
                .filter(|s| slow || s.value() >= 1.0)
                .map_or(1.0, Speed::value)
        };
        self.brightness = within(self.brightness, BRIGHTNESS, 0.0);
        self.contrast = within(self.contrast, CONTRAST, 90.0);
        self.gain = within(self.gain, GAIN, 0.0);
        self.pitch = within(self.pitch, PITCH, 0.0).round();
        self.band_low = within(self.band_low, 0.0..=HIGHEST_HZ, 0.0);
        self.band_high = self
            .band_high
            .filter(|f| f.is_finite() && *f > 0.0)
            .map(|f| f.min(HIGHEST_HZ));
        let areas = &mut self.areas;
        areas.width = within(areas.width, AREA_WIDTH, 3.0);
        areas.fill = within(areas.fill, AREA_FILL, 0.18);
        self
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
struct Views {
    spectrogram: bool,
    waveform: bool,
    spectrum: bool,
    markers: bool,
    metadata: bool,
    meters: bool,
    /// The short form for renaming the files in the explorer's folder.
    rename: bool,
    /// The explorer's search, and its shortcuts.
    search: bool,
    shortcuts: bool,
}

impl Default for Views {
    fn default() -> Self {
        Self {
            spectrogram: true,
            waveform: true,
            spectrum: true,
            markers: true,
            metadata: false,
            meters: true,
            rename: false,
            search: true,
            shortcuts: true,
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
    Saved {
        generation: u64,
        result: Result<(), String>,
    },
    Exported {
        generation: u64,
        result: Result<Option<PathBuf>, String>,
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
    fn new(id: u64) -> (Self, Arc<AtomicBool>, Arc<AtomicU32>) {
        let (cancel, flag) = Cancel::new();
        let progress = Arc::new(AtomicU32::new(0));
        let running = Self {
            id,
            progress: Arc::clone(&progress),
            _cancel: cancel,
        };
        (running, flag, progress)
    }

    fn percent(&self) -> u32 {
        self.progress.load(Ordering::Relaxed) / 10
    }
}

/// An analysis and its images, one per lane.
struct Shown {
    analysis: Analysis,
    textures: Vec<TextureHandle>,
    /// What the images were coloured with, and the rows each has.
    look: Option<(Look, usize)>,
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

    /// The images coloured as `look` has it, `rows` tall.
    fn refresh(
        &mut self,
        ctx: &egui::Context,
        look: Look,
        rows: usize,
        sample_rate: u32,
        name: &str,
    ) {
        if self.look == Some((look, rows)) {
            return;
        }
        self.look = Some((look, rows));
        let planes = self.analysis.planes.len();
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

/// What a click picks for the keys: a slider for the arrow keys, the file
/// explorer for them and Backspace, or the whole file's waveform for moving
/// and sizing the part in view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Control {
    Gain,
    Brightness,
    Contrast,
    Low,
    High,
    Pitch,
    Speed,
    Explorer,
    Overview,
}

impl Control {
    const SLIDERS: [Self; 7] = [
        Self::Gain,
        Self::Brightness,
        Self::Contrast,
        Self::Low,
        Self::High,
        Self::Pitch,
        Self::Speed,
    ];
}

/// What a drag along the whole file's waveform moves: one end of the part
/// in view, the
/// part itself, taken this many frames from its start, or a new span from
/// where the drag began.
#[derive(Clone, Copy)]
enum Grab {
    Start,
    End,
    Move(f64),
    Span(f64),
}

/// What waits until unsaved changes are saved or let go.
#[derive(Clone)]
enum Then {
    Open(PathBuf),
    Quit,
}

/// Playback and analyses as they were when the file was let go of.
struct Released {
    player: Option<(usize, bool)>,
    whole: bool,
    detail: bool,
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
    /// The whole file analysed again, once the spectrogram is drawn wider
    /// than its analysis has columns for.
    whole_due: Option<Instant>,
    /// Columns an analysis for the screen gets, and rows each lane's image
    /// gets: a pixel each, as the spectrogram was last drawn.
    columns: usize,
    rows: usize,
    /// Frames in view.
    view: Range<f64>,
    /// When the view was last moved by hand, if it has been since the
    /// playhead was last put anywhere: playback leaves it where it is put,
    /// and once it rests goes on from it rather than pulling it back.
    moved_at: Option<Instant>,
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
    /// Play the file loading once it has loaded, as the one before was
    /// playing when it was left.
    play_when_loaded: bool,
    meters: Meters,
    listeners: views::Listeners,
    /// Metadata and markers as edited, and as the file holds them.
    edits: Option<Edits>,
    saved: Option<Edits>,
    saving: Option<Running>,
    /// The spectrogram being saved as a PNG, where the last one went and
    /// when, and why the last one failed.
    exporting: Option<Running>,
    exported: Option<(PathBuf, Instant)>,
    export_error: Option<String>,
    released: Option<Released>,
    /// The new name being typed, without its extension.
    renaming: Option<String>,
    /// Why the name typed was not taken, shown under it.
    rename_error: Option<String>,
    /// Why the last save did not happen, shown until the next one.
    save_error: Option<String>,
    focus_rename: bool,
    /// Asking whether to save before this goes ahead.
    asking: Option<Then>,
    /// Going ahead once the save under way is done.
    then: Option<Then>,
    quitting: bool,
    picked: Option<Control>,
    /// Where each control was drawn last frame, for the press that picks it.
    controls: Vec<(Control, Rect)>,
    /// The press landed on the control already picked: a click lets it go.
    unpick_on_click: bool,
    grab: Option<Grab>,
    /// A marker being dragged by its tab, and where on it it was taken.
    marker_grab: Option<(u32, f64)>,
    /// A marker just made, whose name box takes the keyboard once drawn.
    name_next: Option<u32>,
    renamer: Renamer,
    /// The window with every rename rule is open.
    rename_window: bool,
    /// The window setting how areas look is open.
    area_window: bool,
    /// Channels muted in playback, to hear the others alone. Kept from file
    /// to file, not from one run to the next.
    mute: Vec<bool>,
    /// Areas picked on the spectrogram, played in place of the file.
    regions: Vec<Region>,
    /// An area being drawn with Shift held.
    drawing: Option<Drawing>,
}

/// An area being drawn with Shift held.
struct Drawing {
    /// The spectrogram lane it began in; none on the waveform, where an area
    /// takes every frequency.
    lane: Option<Rect>,
    /// Where the drag began and where it is now: frame, frequency and height
    /// on screen.
    from: (f64, f32, f32),
    to: (f64, f32, f32),
}

/// The spectrogram's frequency axis, for picking areas: its lanes, and the
/// band they show from bottom to top.
struct FreqAxis {
    lanes: Vec<Rect>,
    lo: f32,
    hi: f32,
    log: bool,
}

impl FreqAxis {
    fn lane_at(&self, pos: Pos2) -> Option<Rect> {
        self.lanes.iter().copied().find(|l| l.contains(pos))
    }

    /// The frequency at the height of `pos` in `lane`, held within it.
    fn hz(&self, lane: Rect, pos: Pos2) -> f32 {
        let t = ((lane.bottom() - pos.y) / lane.height()).clamp(0.0, 1.0);
        views::freq_at(t, self.lo, self.hi, self.log)
    }

    /// Where `band` lies up `lane`.
    fn rows(&self, lane: Rect, band: (f32, f32)) -> egui::Rangef {
        let y = |f: f32| {
            let t = views::freq_t(f.clamp(self.lo, self.hi), self.lo, self.hi, self.log);
            lane.bottom() - t * lane.height()
        };
        egui::Rangef::new(y(band.1), y(band.0))
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>, inbox: Inbox) -> Self {
        cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
        inbox.connect(&cc.egui_ctx);
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
            whole_due: None,
            columns: spectrogram::DEFAULT_COLUMNS,
            rows: 1024,
            view: 0.0..0.0,
            moved_at: None,
            selection: None,
            selecting: None,
            cursor: None,
            probe: None,
            asked: None,
            spectrum: None,
            player: None,
            play_when_loaded: false,
            meters: Meters::default(),
            listeners: views::Listeners::default(),
            edits: None,
            saved: None,
            saving: None,
            exporting: None,
            exported: None,
            export_error: None,
            released: None,
            renaming: None,
            rename_error: None,
            save_error: None,
            focus_rename: false,
            asking: None,
            then: None,
            quitting: false,
            picked: None,
            controls: Vec::new(),
            unpick_on_click: false,
            grab: None,
            marker_grab: None,
            name_next: None,
            renamer: Renamer::default(),
            rename_window: false,
            area_window: false,
            mute: Vec::new(),
            regions: Vec::new(),
            drawing: None,
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
            self.request_open(ctx, path);
        } else {
            self.error = Some(format!("{} does not exist", path.display()));
        }
    }

    fn dirty(&self) -> bool {
        self.edits != self.saved
    }

    /// Opens `path`, once unsaved changes to the file open now are saved or
    /// let go.
    fn request_open(&mut self, ctx: &egui::Context, path: PathBuf) {
        if self.saving.is_some() {
            self.then = Some(Then::Open(path));
        } else if self.dirty() {
            self.asking = Some(Then::Open(path));
        } else {
            self.open(ctx, path);
        }
    }

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.play_when_loaded |= self.player.as_ref().is_some_and(Player::is_playing);
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
        self.whole_due = None;
        self.view = 0.0..0.0;
        self.moved_at = None;
        self.selection = None;
        self.selecting = None;
        self.regions.clear();
        self.drawing = None;
        self.cursor = None;
        self.probe = None;
        self.asked = None;
        self.spectrum = None;
        self.player = None;
        self.edits = None;
        self.saved = None;
        self.renaming = None;
        self.rename_error = None;
        self.save_error = None;
        self.grab = None;
        self.marker_grab = None;
        self.exporting = None;
        self.exported = None;
        self.export_error = None;
        let (running, cancel, progress) = Running::new(0);
        self.loading = Some(running);
        let (tx, ctx, generation, spec, columns) = (
            self.tx.clone(),
            ctx.clone(),
            self.generation,
            self.spec(),
            self.columns,
        );
        std::thread::spawn(move || {
            // A decoder panic on a hostile or damaged file must end as an
            // error, not as a spinner that never stops.
            let result =
                std::panic::catch_unwind(|| audio::load(&path, spec, columns, &cancel, &progress))
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

    /// The speed set, within the slowest and fastest the open file plays at.
    fn speed(&self) -> Speed {
        let set = Speed::new(self.settings.speed);
        match &self.current {
            Some(c) => {
                let rate = c.info.sample_rate;
                Speed::new(
                    set.value()
                        .max(playback::slowest(rate))
                        .min(playback::fastest(&c.source, rate, self.shaped())),
                )
            }
            None => set,
        }
    }

    /// Playback goes through areas, or plays only what is shown.
    fn shaped(&self) -> bool {
        !self.regions.is_empty() || self.settings.tools.shown
    }

    /// Each channel's share of the left and the right, with the mutes.
    fn heard(&self) -> Vec<[f32; 2]> {
        let muted: Vec<bool> = (0..self.channel_count())
            .map(|c| self.mute.get(c) == Some(&true))
            .collect();
        playback::mix(&muted, self.settings.both_sides)
    }

    /// The preset nearest the speed set, among those shown.
    fn nearest_preset(&self) -> Speed {
        let slow: &[Speed] = if self.settings.tools.slow {
            &Speed::SLOW
        } else {
            &[]
        };
        let off = |s: &Speed| (s.value().ln() - self.settings.speed.ln()).abs();
        slow.iter()
            .chain(&Speed::FAST)
            .copied()
            .min_by(|a, b| off(a).total_cmp(&off(b)))
            .unwrap_or(Speed::NORMAL)
    }

    /// Semitones playback moves by: none with the pitch shift off.
    fn pitch(&self) -> f32 {
        if self.settings.tools.pitch {
            self.settings.pitch
        } else {
            0.0
        }
    }

    /// Hertz shown for each hertz in the file: a pitch shift moves them
    /// with the sound.
    fn hz_scale(&self) -> f32 {
        2f32.powf(self.pitch() / 12.0)
    }

    fn channel_count(&self) -> usize {
        self.current
            .as_ref()
            .map_or(1, |c| usize::from(c.info.channels))
    }

    fn targets(&self) -> Vec<Target> {
        self.settings.channels.targets(self.channel_count())
    }

    fn markers(&self) -> &[Marker] {
        self.edits.as_ref().map_or(&[], |e| &e.markers)
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

    /// What the spectrogram shows, while only that is heard.
    fn shown(&self) -> Option<playback::Shown> {
        let current = self
            .current
            .as_ref()
            .filter(|_| self.settings.tools.shown)?;
        let view = self.look().view;
        Some(playback::Shown {
            floor: -(view.brightness + view.contrast),
            band: spectrogram::band(&view, current.info.sample_rate, self.settings.fft),
            soft: self.settings.soft_edge,
            fft: self.settings.fft,
            apart: self.settings.channels != Channels::Mix,
        })
    }

    fn look(&self) -> Look {
        let nyquist = self.current.as_ref().map_or(f32::MAX, |c| c.info.nyquist());
        let f_max = self.settings.band_high.unwrap_or(nyquist);
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

    /// Said where the band runs past what the file holds.
    fn limit_note(&self) -> String {
        let Some(current) = &self.current else {
            return String::new();
        };
        let scale = self.hz_scale();
        let limit = views::hz_field(f64::from(current.info.nyquist() * scale));
        if scale == 1.0 {
            let rate = views::hz_field(f64::from(current.info.sample_rate));
            format!("Nothing past {limit}: the most a {rate} recording holds")
        } else {
            format!("Nothing past {limit}: the most this recording holds, moved by the pitch shift")
        }
    }

    fn area_style(&self) -> AreaStyle {
        let look = &self.settings.areas;
        let [r, g, b] = if self.settings.multiple_areas {
            look.several
        } else {
            look.one
        };
        AreaStyle {
            edge: Color32::from_rgb(r, g, b),
            width: look.width,
            rim: look.rim,
            fill: Color32::from_rgba_unmultiplied(r, g, b, (look.fill * 255.0).round() as u8),
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
        let (id, generation, spec, columns) =
            (self.jobs, self.generation, self.spec(), self.columns);
        let (running, cancel, progress) = Running::new(id);
        if whole {
            self.whole_job = Some(running);
            self.whole_due = None;
        } else {
            self.detail_job = Some(running);
            self.detail_due = None;
        }
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let run = || audio::analyse(&source, &info, spec, range, columns, &cancel, &progress);
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
                            let (mut loaded, analysis) = *done;
                            let stale = analysis.spec != self.spec();
                            self.view = 0.0..loaded.info.frames as f64;
                            self.probe = Some(Probe::new(
                                loaded.source.clone(),
                                usize::from(loaded.info.channels),
                                loaded.info.frames,
                                ctx.clone(),
                            ));
                            self.meters = Meters::default();
                            self.saved = loaded.edits.take();
                            self.edits = self.saved.clone();
                            self.current = Some(loaded);
                            self.whole = Some(Shown::new(analysis));
                            // The settings changed while the file loaded.
                            if stale {
                                self.analyse(ctx, true);
                            }
                            if std::mem::take(&mut self.play_when_loaded) {
                                self.toggle_play();
                            }
                        }
                        Err(e) => {
                            self.error = Some(e);
                            self.play_when_loaded = false;
                        }
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
                Job::Saved { generation, result } if generation == self.generation => {
                    self.saving = None;
                    match result {
                        Ok(()) => {
                            self.reread();
                            self.explorer.changed();
                        }
                        Err(e) => {
                            self.save_error = Some(format!("Not saved: {e}"));
                            self.then = None;
                        }
                    }
                    self.reacquire(ctx);
                    self.go_ahead(ctx);
                }
                Job::Exported { generation, result } if generation == self.generation => {
                    self.exporting = None;
                    match result {
                        Ok(Some(path)) => self.exported = Some((path, Instant::now())),
                        Ok(None) => {}
                        Err(e) => self.export_error = Some(format!("Not exported: {e}")),
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

    /// Writes the edited metadata and markers into the file.
    fn save(&mut self, ctx: &egui::Context) {
        if self.saving.is_some() {
            return;
        }
        self.save_error = None;
        let (Some(path), Some(edits), Some(saved)) = (self.file.clone(), &self.edits, &self.saved)
        else {
            return;
        };
        let changes = match edits.changes(saved) {
            Ok(changes) => changes,
            Err(e) => {
                self.save_error = Some(format!("Not saved: {e}"));
                self.then = None;
                return;
            }
        };
        self.release();
        let (running, _, progress) = Running::new(0);
        self.saving = Some(running);
        let (tx, ctx, generation) = (self.tx.clone(), ctx.clone(), self.generation);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| save::save(&path, &changes, &progress))
                .unwrap_or_else(|_| Err("saving crashed; the file is as it was".into()));
            let _ = tx.send(Job::Saved { generation, result });
            ctx.request_repaint();
        });
    }

    /// Saves the spectrogram as shown, with its axes, as a PNG beside the
    /// recording.
    fn export(&mut self, ctx: &egui::Context) {
        let (Some(path), Some(current)) = (&self.file, &self.current) else {
            return;
        };
        let frames = current.info.frames;
        let range = self.view.start.floor() as usize..(self.view.end.ceil() as usize).min(frames);
        let rate = self.rate();
        let lanes = self
            .targets()
            .into_iter()
            .map(|t| self.target_name(t))
            .collect();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let (from, to) = (range.start as f64 / rate, range.end as f64 / rate);
        let title = format!(
            "{name}    {} to {}    FFT {}",
            views::clock_fine(from),
            views::clock_fine(to),
            self.settings.fft
        );
        let request = export::Request {
            source: current.source.clone(),
            info: current.info.clone(),
            spec: self.spec(),
            to: export::path_for(path, &range, rate),
            range,
            view: self.look().view,
            gradient: self.settings.colormap.gradient(),
            scale: self.hz_scale(),
            lanes,
            wall_start: current.meta.start.as_ref().map(|s| s.seconds),
            title,
            beyond: self.limit_note(),
        };
        self.jobs += 1;
        let (running, cancel, progress) = Running::new(self.jobs);
        self.exporting = Some(running);
        self.exported = None;
        self.export_error = None;
        let (tx, ctx, generation) = (self.tx.clone(), ctx.clone(), self.generation);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| export::export(&request, &cancel, &progress))
                .unwrap_or_else(|_| Err("exporting crashed".into()));
            let _ = tx.send(Job::Exported { generation, result });
            ctx.request_repaint();
        });
    }

    /// The header and metadata again, from the file just saved: the audio
    /// is the same, only where it sits in the file may have moved.
    fn reread(&mut self) {
        let (Some(path), Some(current)) = (&self.file, &mut self.current) else {
            return;
        };
        match audio::open(path) {
            Ok(opened) => {
                current.info.bytes = opened.info.bytes;
                current.meta = opened.meta;
                current.details = opened.details;
                current
                    .details
                    .sections
                    .insert(0, ("File".into(), audio::file_rows(path, &current.info)));
                current.source = opened.source;
                self.saved = opened.edits;
                self.edits = self.saved.clone();
            }
            Err(e) => self.error = Some(format!("Saved, but it will not open again: {e}")),
        }
    }

    /// Lets go of the file, for a save or a rename to replace or move it:
    /// playback stops where it is and analyses under way are dropped, for
    /// `reacquire` to pick up again.
    fn release(&mut self) {
        self.released = Some(Released {
            player: self.player.take().map(|p| (p.position(), p.is_playing())),
            whole: self.whole_job.take().is_some() || self.whole_due.take().is_some(),
            detail: self.detail_job.take().is_some() || self.detail_due.take().is_some(),
        });
        self.probe = None;
        self.asked = None;
    }

    fn reacquire(&mut self, ctx: &egui::Context) {
        let Some(released) = self.released.take() else {
            return;
        };
        if let Some(current) = &self.current {
            self.probe = Some(Probe::new(
                current.source.clone(),
                usize::from(current.info.channels),
                current.info.frames,
                ctx.clone(),
            ));
        }
        if released.whole {
            self.analyse(ctx, true);
        }
        if released.detail && self.zoomed() {
            self.analyse(ctx, false);
        }
        if let Some((at, playing)) = released.player
            && self.ensure_player(at)
            && let Some(player) = &self.player
        {
            player.seek(at);
            if playing {
                player.play();
            }
        }
    }

    /// Closes the window, once unsaved changes are saved or let go.
    fn quit(&mut self, ctx: &egui::Context) {
        if self.saving.is_some() {
            self.then = Some(Then::Quit);
        } else if self.dirty() {
            self.asking = Some(Then::Quit);
        } else {
            self.quitting = true;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    /// Whatever was waiting on the save, if it left nothing unsaved.
    fn go_ahead(&mut self, ctx: &egui::Context) {
        match self.then.take() {
            Some(Then::Open(path)) if !self.dirty() => self.open(ctx, path),
            Some(Then::Quit) if !self.dirty() => self.quit(ctx),
            _ => {}
        }
    }

    fn rename_to(&mut self, ctx: &egui::Context, stem: &str) {
        let Some(path) = self.file.clone() else {
            return;
        };
        let name = match path.extension() {
            Some(ext) => format!("{}.{}", stem.trim(), ext.to_string_lossy()),
            None => stem.trim().to_owned(),
        };
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy() == name)
        {
            self.renaming = None;
            self.rename_error = None;
            return;
        }
        self.release();
        match save::rename(&path, &name) {
            Ok(new) => {
                self.renaming = None;
                self.rename_error = None;
                if let Some(dir) = new.parent() {
                    self.explorer.forget(dir);
                }
                self.moved(&new);
            }
            Err(e) => self.rename_error = Some(e),
        }
        self.reacquire(ctx);
    }

    /// The file open is now at `new`, after a rename.
    fn moved(&mut self, new: &Path) {
        if let Some(current) = &mut self.current {
            current.source = current.source.with_path(new);
            if let Some(file) = current.details.sections.first_mut() {
                file.1 = audio::file_rows(new, &current.info);
            }
        }
        self.explorer.selected = Some(new.to_owned());
        self.file = Some(new.to_owned());
    }

    fn rename_request(&mut self, ctx: &egui::Context, request: Request) {
        match request {
            Request::AllRules => {
                if self.rename_window {
                    ctx.send_viewport_cmd_to(rename_viewport(), ViewportCommand::Focus);
                }
                self.rename_window = true;
            }
            Request::Close => self.rename_window = false,
            Request::Rename(batch) => self.bulk_rename(ctx, batch, false),
            Request::Undo(batch) => self.bulk_rename(ctx, batch, true),
        }
    }

    /// Renames the files of `batch` all at once, letting go of the one open
    /// while it moves, as a single rename does.
    fn bulk_rename(&mut self, ctx: &egui::Context, batch: rename_ui::Batch, undo: bool) {
        let open = self
            .file
            .as_ref()
            .and_then(|f| batch.iter().find(|(from, _)| from == f))
            .map(|(_, to)| to.clone());
        let busy = if self.saving.is_some() {
            Some("Nothing renamed while a save is under way.")
        } else if open.is_some() && self.loading.is_some() {
            Some("Nothing renamed: the file open is still loading.")
        } else {
            None
        };
        if let Some(why) = busy {
            self.renamer.finished(batch, undo, Err(why.into()), ctx);
            return;
        }
        if open.is_some() {
            self.release();
        }
        let result = rename::execute(&batch);
        if result.is_ok()
            && let Some(new) = &open
        {
            self.moved(new);
        }
        let folders: HashSet<PathBuf> = batch
            .iter()
            .filter_map(|(from, _)| from.parent().map(Path::to_owned))
            .collect();
        for folder in &folders {
            self.explorer.forget(folder);
        }
        if open.is_some() {
            self.reacquire(ctx);
        }
        self.renamer.finished(batch, undo, result, ctx);
    }

    fn add_marker(&mut self) {
        let (at, selection) = (self.cursor.unwrap_or(0), self.selection.clone());
        let Some(edits) = self
            .edits
            .as_mut()
            .filter(|e| e.wav && self.saving.is_none())
        else {
            return;
        };
        let (frame, length) = selection.map_or((at, 0), |s| (s.start, s.len()));
        let id = edits.next_marker_id();
        self.name_next = Some(id);
        edits.markers.push(Marker {
            id,
            frame,
            length,
            label: String::new(),
            note: String::new(),
        });
        edits.markers.sort_by_key(|m| (m.frame, m.id));
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
        let (look, rows) = (self.look(), self.rows);
        for (shown, name) in [(&mut self.whole, "whole"), (&mut self.detail, "detail")] {
            if let Some(shown) = shown {
                shown.refresh(ctx, look, rows, rate, name);
            }
        }
    }

    fn zoomed(&self) -> bool {
        self.view.start > 0.5 || self.view.end < self.frames() as f64 - 0.5
    }

    fn shortest_view(&self) -> f64 {
        (self.rate() / 1000.0).max(64.0).min(self.frames() as f64)
    }

    /// Shows `len` frames from `start`, kept within the file, as moved by
    /// hand.
    fn set_view(&mut self, start: f64, len: f64) {
        if self.show(start, len) {
            self.moved_at = Some(Instant::now());
        }
    }

    /// Shows `len` frames from `start`, kept within the file, and says
    /// whether that moved the view.
    fn show(&mut self, start: f64, len: f64) -> bool {
        let frames = self.frames() as f64;
        if frames <= 0.0 {
            return false;
        }
        let len = len.clamp(self.shortest_view(), frames);
        let start = start.clamp(0.0, frames - len);
        let view = start..start + len;
        if view == self.view {
            return false;
        }
        self.view = view;
        if self.zoomed() {
            self.detail_due = Some(Instant::now() + SETTLE);
        } else {
            self.detail = None;
            self.detail_job = None;
            self.detail_due = None;
        }
        true
    }

    fn set_view_range(&mut self, start: f64, end: f64) {
        self.set_view(start, end - start);
    }

    fn view_len(&self) -> f64 {
        self.view.end - self.view.start
    }

    /// The first whole frame in view: a playhead put there is in it, where
    /// one a fraction before it would page the view back.
    fn view_start(&self) -> usize {
        self.view.start.ceil() as usize
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

    /// Shows the selection, or all the areas, with a margin.
    fn zoom_to_selection(&mut self) {
        let areas = || {
            let start = self.regions.iter().map(|r| r.frames.start).min()?;
            let end = self.regions.iter().map(|r| r.frames.end).max()?;
            Some(start..end)
        };
        if let Some(s) = self.selection.clone().or_else(areas) {
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
            self.show(frame - 0.1 * len, len);
        }
    }

    /// A stretch picked on the whole file's waveform: the playhead goes to
    /// its start, and playing, plays on from there. A selection outside it
    /// is let go; one still in view keeps repeating.
    fn play_from_view(&mut self) {
        let (start, end) = (self.view.start, self.view.end);
        let outside = |s: &Range<usize>| s.end as f64 <= start || s.start as f64 >= end;
        if self.selection.as_ref().is_some_and(outside) {
            self.select(None);
        }
        if self.selection.is_none() {
            self.seek(self.view_start());
        }
    }

    fn start_due_analysis(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        for whole in [true, false] {
            let due = if whole {
                self.whole_due
            } else {
                self.detail_due
            };
            match due {
                Some(at) if now >= at => self.analyse(ctx, whole),
                Some(at) => ctx.request_repaint_after(at - now),
                None => {}
            }
        }
    }

    /// Fits the analyses and their images to the spectrogram as drawn,
    /// `width` pixels across and each lane `lane` pixels up: a column and a
    /// row a pixel. Once it is drawn wider than an analysis has columns
    /// for, that one is done again when the size rests.
    fn fit_to(&mut self, width: f32, lane: f32) {
        self.rows = (lane.round() as usize).clamp(256, 4096);
        let columns =
            (width.round() as usize).clamp(spectrogram::DEFAULT_COLUMNS, spectrogram::MAX_COLUMNS);
        if columns == self.columns {
            return;
        }
        self.columns = columns;
        let Some(current) = &self.current else { return };
        let (spec, channels) = (self.spec(), usize::from(current.info.channels));
        let narrow = |shown: &Option<Shown>| {
            shown.as_ref().is_some_and(|s| {
                let fits = spectrogram::columns(spec, s.analysis.range.len(), channels, columns);
                s.analysis.columns * 10 < fits * 9
            })
        };
        let due = Instant::now() + SETTLE;
        if narrow(&self.whole) {
            self.whole_due = Some(due);
        }
        if narrow(&self.detail) && self.zoomed() {
            self.detail_due = Some(due);
        }
    }

    fn seek(&mut self, frame: usize) {
        let frame = frame.min(self.frames());
        self.cursor = Some(frame);
        self.moved_at = None;
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
                self.moved_at = None;
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
        let Some(current) = self.current.as_ref().filter(|_| self.saving.is_none()) else {
            return false;
        };
        match Player::new(
            &current.source,
            &current.info,
            start,
            self.speed(),
            self.pitch(),
            self.heard(),
        ) {
            Ok(player) => {
                player.set_gain(self.settings.gain);
                player.set_shown(self.shown());
                if let Some(range) = &self.selection {
                    player.set_loop(Some(range.clone()));
                } else if !self.regions.is_empty() {
                    player.set_regions(self.regions.clone(), start);
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
        // While a file loads, whether it plays once it has.
        if self.current.is_none() {
            if self.loading.is_some() {
                self.play_when_loaded = !self.play_when_loaded;
            }
            return;
        }
        let frames = self.frames();
        if frames == 0 {
            return;
        }
        // From the cursor, or from what is in view once the view has been
        // moved away from it by hand; without one, or with it at the end,
        // from the start of the view.
        let away = self.moved_at.is_some() && self.selection.is_none();
        let start = match self.cursor.filter(|&c| c < frames) {
            Some(c) if away && !self.view.contains(&(c as f64)) => self.view_start(),
            Some(c) => c,
            None => self.view_start(),
        };
        if !self.ensure_player(start) {
            return;
        }
        let Some(player) = &self.player else { return };
        if player.is_playing() {
            player.pause();
            self.cursor = Some(player.position());
        } else {
            if player.position() != start {
                player.seek(start);
            }
            self.cursor = Some(start);
            self.moved_at = None;
            player.play();
        }
    }

    /// Back to the start: of the selection or the first area when there is
    /// one, else of the file.
    fn stop(&mut self) {
        let home = self
            .selection
            .as_ref()
            .map(|s| s.start)
            .or_else(|| self.regions.iter().map(|r| r.frames.start).min())
            .unwrap_or(0);
        if let Some(player) = &self.player {
            player.pause();
            player.seek(home);
        }
        if self.current.is_some() {
            self.cursor = Some(home);
            self.moved_at = None;
        }
    }

    /// The player at the speed and pitch set, which a slider, a key or a
    /// switch at the top may have just changed.
    fn tune_player(&self) {
        let Some(player) = &self.player else { return };
        player.set_mix(self.heard());
        player.set_shown(self.shown());
        if player.speed() != self.speed() {
            player.set_speed(self.speed());
        }
        if player.pitch() != self.pitch() {
            player.set_pitch(self.pitch());
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
        // A selection repeats wherever the view is.
        if self.selection.is_some() {
            return;
        }
        // While the view is moved by hand, or something on it dragged, it
        // stays where it is. Once it rests, playback goes on from its start
        // if it left the playhead behind, rather than pulling it back.
        let in_hand = self.grab.is_some()
            || self.selecting.is_some()
            || self.drawing.is_some()
            || self.marker_grab.is_some();
        if in_hand || self.moved_at.is_some_and(|t| t.elapsed() < SETTLE) {
            return;
        }
        if self.moved_at.take().is_some() && !self.view.contains(&(at as f64)) {
            self.seek(self.view_start());
            return;
        }
        self.keep_in_view(at as f64);
    }

    /// Moves the picked slider one step: a dB for the levels, a semitone
    /// for the frequencies, or with Shift five dB and an octave.
    fn nudge(&mut self, control: Control, up: bool, coarse: bool) {
        let db = if coarse { 5.0 } else { 1.0 } * if up { 1.0 } else { -1.0 };
        let bump = |value: &mut f32, range: RangeInclusive<f32>| {
            *value = (*value + db).clamp(*range.start(), *range.end());
        };
        match control {
            Control::Gain => {
                bump(&mut self.settings.gain, GAIN);
                if let Some(player) = &self.player {
                    player.set_gain(self.settings.gain);
                }
            }
            Control::Brightness => bump(&mut self.settings.brightness, BRIGHTNESS),
            Control::Contrast => bump(&mut self.settings.contrast, CONTRAST),
            Control::Low | Control::High => {
                let (view, scale) = (self.look().view, self.hz_scale());
                let nyquist = self
                    .current
                    .as_ref()
                    .map_or(view.f_max, |c| c.info.nyquist())
                    * scale;
                let (f_min, f_max) = (view.f_min * scale, view.f_max * scale);
                let factor = if coarse { 2.0 } else { 2f32.powf(1.0 / 12.0) };
                let step = |f: f32| match (up, f < LOWEST_HZ) {
                    (true, true) => LOWEST_HZ,
                    (true, false) => f * factor,
                    (false, _) if f / factor < LOWEST_HZ => 0.0,
                    (false, _) => f / factor,
                };
                if control == Control::Low {
                    self.settings.band_low = step(f_min).min(f_max / 1.06) / scale;
                } else {
                    // Up to what the file holds, or once typed in past it,
                    // as far as the band goes.
                    let top = if f_max > nyquist + 0.5 * scale {
                        HIGHEST_HZ * scale
                    } else {
                        nyquist
                    };
                    let high = step(f_max).clamp((f_min * 1.06).max(LOWEST_HZ), top);
                    self.settings.band_high =
                        ((high - nyquist).abs() >= 0.5 * scale).then_some(high / scale);
                }
            }
            Control::Pitch => {
                let semitones = if coarse { 12.0 } else { 1.0 } * if up { 1.0 } else { -1.0 };
                self.settings.pitch =
                    (self.settings.pitch + semitones).clamp(*PITCH.start(), *PITCH.end());
            }
            // A semitone of pitch at a time, as on tape, or with Shift an
            // octave.
            Control::Speed => {
                let factor = if coarse { 2.0 } else { 2f64.powf(1.0 / 12.0) };
                let speed = self.settings.speed * if up { factor } else { 1.0 / factor };
                self.settings.speed = Speed::new(speed).value();
            }
            Control::Explorer | Control::Overview => {}
        }
    }

    /// Puts `control` back where it starts: the part in view back to the
    /// whole file.
    fn reset(&mut self, control: Control) {
        if control == Control::Overview {
            self.fit();
            return;
        }
        let start = Settings::default();
        let settings = &mut self.settings;
        match control {
            Control::Gain => {
                settings.gain = start.gain;
                if let Some(player) = &self.player {
                    player.set_gain(start.gain);
                }
            }
            Control::Brightness => settings.brightness = start.brightness,
            Control::Contrast => settings.contrast = start.contrast,
            Control::Low => settings.band_low = start.band_low,
            Control::High => settings.band_high = start.band_high,
            Control::Pitch => settings.pitch = start.pitch,
            Control::Speed => settings.speed = start.speed,
            Control::Explorer | Control::Overview => {}
        }
    }

    /// Where a press picks a control for the keys, or lets it go.
    fn pick(&mut self, ctx: &egui::Context) {
        let (pressed, clicked, dragging, at) = ctx.input(|i| {
            let p = &i.pointer;
            (
                p.primary_pressed(),
                p.primary_clicked(),
                p.is_decidedly_dragging(),
                p.interact_pos(),
            )
        });
        if pressed {
            let hit = at.and_then(|p| {
                self.controls
                    .iter()
                    .find(|(_, r)| r.contains(p))
                    .map(|(c, _)| *c)
            });
            // The explorer stays picked while it is clicked in; a slider
            // is let go by a second click on it.
            self.unpick_on_click =
                hit.is_some() && hit == self.picked && hit != Some(Control::Explorer);
            self.picked = hit;
        }
        if dragging {
            self.unpick_on_click = false;
        }
        if clicked && std::mem::take(&mut self.unpick_on_click) {
            self.picked = None;
        }
    }

    fn input(&mut self, ctx: &egui::Context) {
        // While unsaved changes are asked about, keys and clicks are the
        // question's.
        if self.asking.is_some() {
            return;
        }
        let toggle_explorer = KeyboardShortcut::new(Modifiers::COMMAND, Key::B);
        if ctx.input_mut(|i| i.consume_shortcut(&toggle_explorer)) {
            self.settings.explorer = !self.settings.explorer;
        }
        let toggle_menu = KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma);
        if ctx.input_mut(|i| i.consume_shortcut(&toggle_menu)) {
            self.settings.menu = !self.settings.menu;
        }
        let save_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
        if ctx.input_mut(|i| i.consume_shortcut(&save_shortcut)) && self.dirty() {
            self.save(ctx);
        }
        if let Some(path) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_owned()))
        {
            self.open_external(ctx, path);
        }
        self.pick(ctx);
        // egui takes the keyboard from the name box on Esc before the box
        // is drawn, so the box never sees the key itself.
        if self.renaming.is_some() && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.renaming = None;
            self.rename_error = None;
        }
        // Taken before any widget draws, so a focused button cannot also
        // act on these keys, and never while text is being typed.
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let pressed = |modifiers, key| ctx.input_mut(|i| i.consume_key(modifiers, key));
        let plain = |key| pressed(Modifiers::NONE, key);
        if self.picked == Some(Control::Explorer) {
            if plain(Key::Backspace) {
                self.explorer.go_up();
            }
            for (key, down) in [(Key::ArrowUp, false), (Key::ArrowDown, true)] {
                if plain(key)
                    && let Some(file) = self.explorer.step(down)
                {
                    // Back from a folder onto the file open, which stays as it is.
                    if self.file.as_ref() == Some(&file) {
                        self.explorer.selected = Some(file);
                    } else {
                        self.request_open(ctx, file);
                    }
                }
            }
        }
        // Shift first: a plain R would take the shifted one too.
        if pressed(Modifiers::SHIFT, Key::R) {
            for control in Control::SLIDERS {
                self.reset(control);
            }
        } else if let Some(control) = self.picked
            && plain(Key::R)
        {
            self.reset(control);
        }
        if (self.current.is_some() || self.loading.is_some()) && plain(Key::Space) {
            self.toggle_play();
        }
        if self.current.is_none() {
            return;
        }
        match self.picked.filter(|c| *c != Control::Explorer) {
            // Along by a tenth of the part in view, or with Shift by all of
            // it; wider and narrower by a quarter, or with Shift twice.
            Some(Control::Overview) => {
                let len = self.view_len();
                for (key, sign) in [(Key::ArrowLeft, -1.0), (Key::ArrowRight, 1.0)] {
                    if pressed(Modifiers::SHIFT, key) {
                        self.pan(sign * len);
                    } else if plain(key) {
                        self.pan(sign * len / 10.0);
                    }
                }
                for (key, step, coarse) in [(Key::ArrowUp, 1.25, 2.0), (Key::ArrowDown, 0.8, 0.5)] {
                    let centre = (self.view.start + self.view.end) / 2.0;
                    if pressed(Modifiers::SHIFT, key) {
                        self.zoom_at(centre, coarse);
                    } else if plain(key) {
                        self.zoom_at(centre, step);
                    }
                }
            }
            Some(control) => {
                for (key, up) in [
                    (Key::ArrowLeft, false),
                    (Key::ArrowDown, false),
                    (Key::ArrowRight, true),
                    (Key::ArrowUp, true),
                ] {
                    if pressed(Modifiers::SHIFT, key) {
                        self.nudge(control, up, true);
                    } else if plain(key) {
                        self.nudge(control, up, false);
                    }
                }
            }
            None => {
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
                // Up and down move through the explorer while it is picked.
                if self.picked.is_none() {
                    for (key, step) in [(Key::ArrowUp, 5.0), (Key::ArrowDown, -5.0)] {
                        if plain(key) {
                            self.settings.brightness = (self.settings.brightness + step)
                                .clamp(*BRIGHTNESS.start(), *BRIGHTNESS.end());
                        }
                    }
                }
            }
        }
        if plain(Key::Home) {
            self.seek(0);
        }
        if plain(Key::End) {
            self.seek(self.frames());
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
        if plain(Key::M) {
            self.add_marker();
        }
        if plain(Key::Escape) {
            if self.picked.is_some() {
                self.picked = None;
            } else {
                self.select(None);
            }
        }
    }

    /// Notes where `control` was drawn, for the press that picks it and
    /// the outline that shows it picked.
    fn mark_control(&mut self, control: Control, rect: Rect) {
        self.controls.push((control, rect));
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        if !self.settings.speed_slider && !self.settings.tools.slow && self.settings.speed < 1.0 {
            self.settings.speed = 1.0;
        }
        ui.add_space(4.0);
        let (dirty, can_rename) = (
            self.dirty(),
            self.current.is_some() && self.saving.is_none(),
        );
        let (mut start_rename, mut rename, mut keep_name, mut save, mut revert) =
            (false, None, false, false, false);
        ui.horizontal(|ui| {
            let path = self.file.as_deref();
            match &mut self.renaming {
                Some(stem) => {
                    let width = (ui.available_width() - 120.0).clamp(120.0, 560.0);
                    let edit = ui.add(
                        TextEdit::singleline(stem)
                            .font(FontId::proportional(22.0))
                            .desired_width(width),
                    );
                    if std::mem::take(&mut self.focus_rename) {
                        edit.request_focus();
                    }
                    if let Some(ext) = path.and_then(Path::extension) {
                        ui.label(RichText::new(format!(".{}", ext.to_string_lossy())).size(22.0));
                    }
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button("✔").on_hover_text("Rename (Enter)").clicked() || enter {
                        rename = Some(stem.clone());
                    }
                    if ui
                        .button("✖")
                        .on_hover_text("Keep the name (Esc)")
                        .clicked()
                    {
                        keep_name = true;
                    }
                }
                None => {
                    let name = path
                        .and_then(Path::file_name)
                        .map_or("No file open".into(), |n| n.to_string_lossy());
                    let title = RichText::new(name)
                        .size(26.0)
                        .strong()
                        .color(Color32::WHITE);
                    let label = ui.add(Label::new(title).sense(Sense::click()));
                    if can_rename && label.on_hover_text("Click to rename").clicked() {
                        start_rename = true;
                    }
                }
            }
            if let Some(loading) = &self.loading {
                ui.spinner();
                ui.label(format!("{}%", loading.percent()));
            }
            if let Some(saving) = &self.saving {
                ui.spinner();
                ui.label(format!("Saving {}%", saving.percent()));
            } else if dirty {
                ui.label(RichText::new("Unsaved changes").color(views::MARK));
                save = ui.button("Save").on_hover_text("⌘S").clicked();
                revert = ui
                    .button("Revert")
                    .on_hover_text("Back to what the file holds")
                    .clicked();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let open = self.settings.menu;
                let gear = RichText::new("⚙").size(20.0);
                if ui
                    .selectable_label(open, gear)
                    .on_hover_text("Which views show, and the tools (⌘,)")
                    .clicked()
                {
                    self.settings.menu = !open;
                }
            });
        });
        if self.settings.menu {
            self.menu(ui);
        }
        if start_rename {
            self.renaming = self
                .file
                .as_deref()
                .and_then(Path::file_stem)
                .map(|s| s.to_string_lossy().into_owned());
            self.focus_rename = true;
            self.rename_error = None;
        }
        if keep_name {
            self.renaming = None;
            self.rename_error = None;
        }
        if let Some(stem) = rename {
            self.rename_to(ui.ctx(), &stem);
        }
        if save {
            self.save(ui.ctx());
        }
        if revert {
            self.edits = self.saved.clone();
            self.save_error = None;
        }
        let problems = [
            &self.rename_error,
            &self.save_error,
            &self.export_error,
            &self.error,
        ];
        for problem in problems.into_iter().flatten() {
            ui.label(RichText::new(problem).color(views::CURSOR));
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
                RichText::new(summary(current, self.pitch()))
                    .monospace()
                    .size(12.0)
                    .color(views::AXIS),
            );
        }
        ui.add_space(6.0);
    }

    /// What the gear button opens: which views show, and the tools.
    fn menu(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let views = &mut self.settings.views;
                ui.label(RichText::new("Show").color(views::AXIS));
                ui.checkbox(&mut self.settings.explorer, "Files")
                    .on_hover_text("⌘B");
                ui.checkbox(&mut views.search, "Search").on_hover_text(
                    "In the files, a search through every recording under the folder shown: names, metadata and tags, with the format, a date and the length to narrow it",
                );
                ui.checkbox(&mut views.shortcuts, "Shortcuts").on_hover_text(
                    "In the files, folders and recordings kept at the top: right-click one to add it",
                );
                ui.checkbox(&mut views.spectrogram, "Spectrogram");
                ui.checkbox(&mut views.waveform, "Waveform")
                    .on_hover_text("Under the spectrogram, the whole file: drag across it to pick the part the spectrogram shows");
                ui.checkbox(&mut views.spectrum, "Spectrum");
                ui.checkbox(&mut views.markers, "Markers");
                ui.checkbox(&mut views.metadata, "Metadata");
                ui.checkbox(&mut views.meters, "Meters");
                ui.checkbox(&mut views.rename, "Bulk rename").on_hover_text(
                    "Renames every file in the explorer's folder at once: replace, add, number and change case. The name at the top renames just the file open.",
                );
                if ui
                    .selectable_label(self.rename_window, "All rules")
                    .on_hover_text("Bulk rename with every rule, in a window of its own")
                    .clicked()
                {
                    self.rename_window = !self.rename_window;
                }
            });
            ui.horizontal_wrapped(|ui| {
                let tools = &mut self.settings.tools;
                ui.label(RichText::new("Tools").color(views::AXIS));
                ui.checkbox(&mut tools.pitch, "Pitch shift").on_hover_text(
                    "A slider that moves the sound up or down without changing its speed, and the frequencies shown with it",
                );
                ui.checkbox(&mut tools.slow, "Slow speeds").on_hover_text(
                    "Speeds down to a tenth, which bring bat calls down into hearing",
                );
                ui.checkbox(&mut tools.shown, "Hear what's shown").on_hover_text(
                    "Only what the spectrogram shows is played: brightness and contrast set how faint a sound can be and still be heard, and Min and Max which frequencies",
                );
                ui.checkbox(&mut self.settings.multiple_areas, "Multiple areas").on_hover_text(
                    "A new area picked with Shift-drag on the spectrogram joins those picked before, rather than taking their place",
                );
                if ui
                    .selectable_label(self.area_window, "Area boxes")
                    .on_hover_text("How the areas picked with Shift-drag look: the colour and width of their edges, and their fill")
                    .clicked()
                {
                    self.area_window = !self.area_window;
                }
            });
        });
    }

    /// The small window setting how areas look.
    fn area_window(&mut self, ctx: &egui::Context) {
        let mut open = self.area_window;
        // Under the gear that opens it, clear of the files on the left.
        let corner = ctx.content_rect().right_top() + Vec2::new(-16.0, 110.0);
        egui::Window::new("Area boxes")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .pivot(Align2::RIGHT_TOP)
            .default_pos(corner)
            .show(ctx, |ui| {
                let look = &mut self.settings.areas;
                egui::Grid::new("area look")
                    .num_columns(2)
                    .spacing([14.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("One area").on_hover_text(
                            "The edge of the area picked, with Multiple areas off",
                        );
                        ui.color_edit_button_srgb(&mut look.one);
                        ui.end_row();
                        ui.label("Several areas")
                            .on_hover_text("The edges of the areas, with Multiple areas on");
                        ui.color_edit_button_srgb(&mut look.several);
                        ui.end_row();
                        ui.label("Edge width");
                        ui.add(
                            egui::Slider::new(&mut look.width, AREA_WIDTH)
                                .step_by(0.5)
                                .fixed_decimals(1)
                                .suffix(" px"),
                        );
                        ui.end_row();
                        ui.label("Dark rim");
                        ui.checkbox(&mut look.rim, "").on_hover_text(
                            "A dark line under the edge, so it shows on the light colours of a colour map too",
                        );
                        ui.end_row();
                        ui.label("Fill");
                        let mut percent = look.fill * 100.0;
                        let fill = egui::Slider::new(
                            &mut percent,
                            AREA_FILL.start() * 100.0..=AREA_FILL.end() * 100.0,
                        )
                        .step_by(1.0)
                        .fixed_decimals(0)
                        .suffix(" %");
                        if ui
                            .add(fill)
                            .on_hover_text("How much of the edge's colour fills the area")
                            .changed()
                        {
                            look.fill = percent / 100.0;
                        }
                        ui.end_row();
                    });
                ui.add_space(4.0);
                if ui
                    .add_enabled(*look != AreaLook::default(), egui::Button::new("Defaults"))
                    .clicked()
                {
                    *look = AreaLook::default();
                }
            });
        self.area_window = open;
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
        let loading = self.loading.is_some();
        let playing = self.player.as_ref().is_some_and(Player::is_playing)
            || (loading && self.play_when_loaded);
        let has_file = self.current.is_some();
        let can_mark = self.edits.as_ref().is_some_and(|e| e.wav) && self.saving.is_none();
        ui.horizontal_wrapped(|ui| {
            let label = if playing { "Pause" } else { "Play" };
            let play = egui::Button::new(label).min_size(Vec2::new(64.0, 0.0));
            if ui
                .add_enabled(has_file || loading, play)
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
            if ui
                .add_enabled(can_mark, egui::Button::new("Mark"))
                .on_hover_text("M: a marker at the playhead, or the selection as a region")
                .on_disabled_hover_text("Markers are kept inside WAV files, and this is not one")
                .clicked()
            {
                self.add_marker();
            }
            ui.separator();
            ui.label("Speed")
                .on_hover_text("Pitch moves with speed, as on tape");
            if self.settings.speed_slider {
                self.speed_slider(ui);
            } else {
                let slow = if self.settings.tools.slow {
                    &Speed::SLOW[..]
                } else {
                    &[]
                };
                for &speed in slow.iter().chain(&Speed::FAST) {
                    if ui
                        .selectable_label(self.speed() == speed, speed.label())
                        .clicked()
                    {
                        self.settings.speed = speed.value();
                    }
                }
            }
            let free = self.settings.speed_slider;
            let hint = if free {
                "Back to the preset speeds"
            } else {
                "Any speed, on a slider or typed in"
            };
            if ui.selectable_label(free, "Slider").on_hover_text(hint).clicked() {
                self.settings.speed_slider = !free;
                if free {
                    self.settings.speed = self.nearest_preset().value();
                }
            }
            ui.separator();
            let mut gain = self.settings.gain;
            let mut reset = false;
            let group = ui
                .scope(|ui| {
                    ui.label("Gain").on_hover_text(format!(
                        "Playback volume: raise it for quiet recordings, lower it for 32-bit float files that go past full scale.\n\n{PICKING}"
                    ));
                    ui.add(egui::Slider::new(&mut gain, GAIN).step_by(1.0).suffix(" dB"));
                    reset = views::reset_button(ui, gain != 0.0, "0 dB");
                })
                .response
                .rect;
            self.mark_control(Control::Gain, group);
            if reset {
                gain = 0.0;
            }
            if gain != self.settings.gain {
                self.settings.gain = gain;
                if let Some(player) = &self.player {
                    player.set_gain(gain);
                }
            }
            ui.separator();
            if has_file {
                let rate = self.rate();
                let at = self.cursor.unwrap_or(0) as f64 / rate;
                ui.label(
                    RichText::new(format!(
                        "{} / {}",
                        views::clock_fine(at),
                        views::clock(self.frames() as f64 / rate)
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
            let selected = self.selection.is_some() || !self.regions.is_empty();
            if ui
                .add_enabled(selected, egui::Button::new("Selection"))
                .on_hover_text("S")
                .clicked()
            {
                self.zoom_to_selection();
            }
            let can_export =
                has_file && self.settings.views.spectrogram && self.exporting.is_none();
            if ui
                .add_enabled(can_export, egui::Button::new("Export PNG"))
                .on_hover_text("The spectrogram as it shows now, with its axes, at full detail: saved beside the recording")
                .clicked()
            {
                self.export(ui.ctx());
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
            if !self.regions.is_empty() {
                let n = self.regions.len();
                let areas = if n == 1 { "area" } else { "areas" };
                let hint = if self.settings.multiple_areas {
                    "Shift-drag on the spectrogram adds an area, Shift-click on one takes it away, and a plain drag lets them all go"
                } else {
                    "Shift-drag on the spectrogram picks an area in place of this one, Shift-click on it lets it go, and so does a plain drag. Multiple areas, in the menu under the gear, keeps each one picked"
                };
                ui.label(RichText::new(format!("{n} {areas}")).color(views::AXIS))
                    .on_hover_text(hint);
            }
        });
    }

    /// Any speed from a thousandth to a thousand times, and any typed in up
    /// to what playback can do.
    fn speed_slider(&mut self, ui: &mut egui::Ui) {
        let mut reset = false;
        let group = ui
            .scope(|ui| {
                ui.add(
                    egui::Slider::new(&mut self.settings.speed, Speed::SLIDER)
                        .logarithmic(true)
                        .clamping(egui::SliderClamping::Never)
                        .custom_formatter(|v, _| playback::speed_text(v))
                        .custom_parser(playback::parse_speed),
                )
                .on_hover_text(format!(
                    "From a thousandth to a thousand times, or click the number and type any speed: 3, 1/250, 0.02.\n\n{PICKING}"
                ));
                reset = views::reset_button(ui, self.settings.speed != 1.0, "1×");
            })
            .response
            .rect;
        self.mark_control(Control::Speed, group);
        if reset {
            self.reset(Control::Speed);
        }
        self.settings.speed = Speed::new(self.settings.speed).value();
        let plays = self.speed();
        if plays.value() != self.settings.speed {
            let why = if plays.value() < self.settings.speed {
                "The fastest this file plays: any faster, and reading and filtering it would fall behind the sound going out"
            } else {
                "The slowest this file plays: its highest frequency already comes out at 2.4 Hz, far below hearing, and any slower would only keep each start and seek waiting"
            };
            ui.weak(format!("plays at {}", plays.label()))
                .on_hover_text(why);
        }
    }

    fn display(&mut self, ui: &mut egui::Ui) {
        let nyquist = self.current.as_ref().map_or(96_000.0, |c| c.info.nyquist());
        let rate = self.current.as_ref().map_or(0, |c| c.info.sample_rate);
        let scale = self.hz_scale();
        let channels = self.channel_count();
        let names: Vec<String> = (0..channels).map(|c| self.channel_name(c)).collect();
        let look = self.look().view;
        let start = Settings::default();
        let mut reset = None;
        ui.horizontal_wrapped(|ui| {
            ui.label("FFT").on_hover_text(
                "How many samples each column is worked out from. A larger FFT separates frequencies that lie closer together, but blurs whatever changes quickly; a smaller one keeps fast calls, trills and clicks sharp, but blurs frequencies together. The list gives each size's length, and how far apart two frequencies have to be for it to tell them apart, in this file.",
            );
            let size = |n: usize| match rate {
                0 => n.to_string(),
                rate => {
                    let rate = f64::from(rate);
                    format!(
                        "{n}    {:.1} ms, {} apart",
                        n as f64 * 1000.0 / rate,
                        views::hz_field(rate / n as f64)
                    )
                }
            };
            egui::ComboBox::from_id_salt("fft")
                .selected_text(self.settings.fft.to_string())
                .show_ui(ui, |ui| {
                    for n in spectrogram::FFT_SIZES {
                        ui.selectable_value(&mut self.settings.fft, n, size(n));
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
            let group = ui
                .scope(|ui| {
                    ui.label("Brightness").on_hover_text(format!(
                        "Added to every level before colouring: right is brighter. With nothing picked, ↑ and ↓ step it by 5 dB.\n\n{PICKING}"
                    ));
                    ui.add(
                        egui::Slider::new(&mut self.settings.brightness, BRIGHTNESS)
                            .step_by(1.0)
                            .suffix(" dB"),
                    );
                    let changed = self.settings.brightness != start.brightness;
                    if views::reset_button(ui, changed, "0 dB") {
                        reset = Some(Control::Brightness);
                    }
                })
                .response
                .rect;
            self.mark_control(Control::Brightness, group);
            let group = ui
                .scope(|ui| {
                    ui.label("Contrast").on_hover_text(format!(
                        "How far below full brightness a level still gets colour: lower is more contrast.\n\n{PICKING}"
                    ));
                    ui.add(
                        egui::Slider::new(&mut self.settings.contrast, CONTRAST)
                            .step_by(1.0)
                            .suffix(" dB"),
                    );
                    let changed = self.settings.contrast != start.contrast;
                    if views::reset_button(ui, changed, "90 dB") {
                        reset = Some(Control::Contrast);
                    }
                })
                .response
                .rect;
            self.mark_control(Control::Contrast, group);
            if self.settings.tools.shown {
                ui.checkbox(&mut self.settings.soft_edge, "Soft edge").on_hover_text(
                    "Sounds the spectrogram only just shows fade in rather than cut in, which leaves less warble behind",
                );
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Frequency");
            let (mut lo, mut hi) = (
                f64::from(look.f_min * scale),
                f64::from(look.f_max * scale),
            );
            let typed = format!("Drag, or click the number and type: 200, 1.5k, 12 kHz.\n\n{PICKING}");
            let (mut min_changed, mut max_changed) = (false, false);
            let group = ui
                .scope(|ui| {
                    ui.label("Min").on_hover_text(&typed);
                    min_changed = ui.add(frequency_slider(&mut lo, 0.0..=hi)).changed();
                    self.listeners
                        .show(ui, lo as f32, views::Animal::Whale)
                        .on_hover_text(HEARING);
                    if views::reset_button(ui, self.settings.band_low > 0.0, "0 Hz") {
                        reset = Some(Control::Low);
                    }
                })
                .response
                .rect;
            self.mark_control(Control::Low, group);
            let group = ui
                .scope(|ui| {
                    let limit = views::hz_field(f64::from(nyquist * scale));
                    let moved = if scale == 1.0 {
                        ""
                    } else {
                        ", moved by the pitch shift"
                    };
                    let top = format!(
                        "The file holds frequencies up to {limit}, half its {rate} Hz sample rate{moved}: a wave takes at least two samples, one up and one down, so none faster is recorded. Drag up to there, or type a higher number, up to {}, to see that nothing is past it.",
                        views::hz_field(f64::from(HIGHEST_HZ * scale))
                    );
                    ui.label("Max").on_hover_text(format!("{top}\n\n{typed}"));
                    let file = f64::from(nyquist * scale);
                    let floor = lo.max(f64::from(LOWEST_HZ));
                    max_changed = ui
                        .add(
                            frequency_slider(&mut hi, floor..=file.max(floor))
                                .clamping(egui::SliderClamping::Never),
                        )
                        .on_hover_text(top)
                        .changed();
                    self.listeners
                        .show(ui, hi as f32, views::Animal::Bat)
                        .on_hover_text(HEARING);
                    let changed = self.settings.band_high.is_some();
                    if views::reset_button(ui, changed, "the highest the file holds") {
                        reset = Some(Control::High);
                    }
                })
                .response
                .rect;
            self.mark_control(Control::High, group);
            if (min_changed || max_changed) && reset.is_none() {
                let floor = lo.max(f64::from(LOWEST_HZ));
                let hi = hi.clamp(floor, f64::from(HIGHEST_HZ * scale));
                let (lo, hi) = (lo as f32 / scale, hi as f32 / scale);
                self.settings.band_low = lo;
                self.settings.band_high = ((hi - nyquist).abs() >= 0.5).then_some(hi);
            }
            if ui.button("Full").clicked() {
                self.settings.band_low = 0.0;
                self.settings.band_high = None;
            }
            ui.checkbox(&mut self.settings.log, "Log");
            if self.settings.tools.pitch {
                ui.separator();
                let group = ui
                    .scope(|ui| {
                        ui.label("Pitch").on_hover_text(format!(
                            "Moves the sound up or down in semitones, twelve to the octave, at the same speed, and the frequencies shown with it. Three octaves down, bat calls at 40 to 90 kHz come out at 5 to 11 kHz.\n\n{PICKING}"
                        ));
                        ui.add(
                            egui::Slider::new(&mut self.settings.pitch, PITCH)
                                .step_by(1.0)
                                .suffix(" st"),
                        );
                        if views::reset_button(ui, self.settings.pitch != 0.0, "0 st") {
                            reset = Some(Control::Pitch);
                        }
                    })
                    .response
                    .rect;
                self.mark_control(Control::Pitch, group);
            }
        });
        if let Some(control) = reset {
            self.reset(control);
        }
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

    /// The views that follow time: the spectrogram over the waveform of the
    /// whole file, which picks the part the spectrogram shows. The waveform
    /// on its own follows the part in view itself, as the spectrogram does.
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
                .default_size(150.0)
                .min_size(90.0)
                .show(ui, |ui| self.overview_view(ui));
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.spectrogram_view(ui, true));
        } else if views.spectrogram {
            self.spectrogram_view(ui, true);
        } else {
            self.waveform_view(ui, true);
        }
    }

    /// The views beside: the spectrum over the markers over the metadata
    /// over renaming, each split resizable.
    fn side(&mut self, ui: &mut egui::Ui) {
        let height = Self::claim(ui).height();
        let views = self.settings.views;
        type View = fn(&mut App, &mut egui::Ui);
        let shown: Vec<(&str, View)> = [
            (views.spectrum, "spectrum", Self::spectrum_view as View),
            (views.markers, "markers", Self::markers_view),
            (views.metadata, "metadata", Self::metadata_view),
            (views.rename, "rename", Self::rename_view),
        ]
        .into_iter()
        .filter(|(on, _, _)| *on)
        .map(|(_, id, view)| (id, view))
        .collect();
        let Some(&(_, top)) = shown.first() else {
            return;
        };
        // Split only once a file is open: until then the column is taller
        // than it will be with the meters beneath it, and a
        // split keeps the size it is first given. Renaming needs no file,
        // so it shows on its own until then.
        if self.current.is_none() {
            let alone = if views.rename {
                Self::rename_view as View
            } else {
                top
            };
            alone(self, ui);
            return;
        }
        // Each set of views keeps splits of its own, so one switched on
        // starts with an even share rather than squeezing the others.
        let set: Vec<&str> = shown.iter().map(|(id, _)| *id).collect();
        let share = height / shown.len() as f32;
        for &(id, view) in shown[1..].iter().rev() {
            egui::Panel::bottom(egui::Id::new((id, &set)))
                .frame(egui::Frame::NONE)
                .resizable(true)
                .default_size(share)
                .min_size(80.0)
                .show(ui, |ui| view(self, ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| top(self, ui));
    }

    fn rename_view(&mut self, ui: &mut egui::Ui) {
        Self::claim(ui);
        ui.add_space(4.0);
        let locked = self.saving.is_some();
        if let Some(request) = self.renamer.simple(ui, locked) {
            self.rename_request(ui.ctx(), request);
        }
    }

    /// The window with every rename rule, a window of its own where the
    /// system gives one.
    fn rename_window(&mut self, ctx: &egui::Context) {
        let locked = self.saving.is_some();
        let (mut closed, mut asked) = (false, None);
        ctx.show_viewport_immediate(
            rename_viewport(),
            egui::ViewportBuilder::default()
                .with_title("Bulk rename")
                .with_inner_size([1180.0, 780.0])
                .with_min_inner_size([980.0, 620.0]),
            |ui, _| {
                // Esc only once no box is being typed in, which it leaves.
                let typing = ui.ctx().egui_wants_keyboard_input();
                closed = ui.input_mut(|i| {
                    i.viewport().close_requested()
                        || i.consume_shortcut(&CLOSE_RENAME)
                        || (!typing && i.consume_key(Modifiers::NONE, Key::Escape))
                });
                asked = self.renamer.full(ui, locked, &CLOSE_RENAME);
            },
        );
        if closed {
            self.rename_window = false;
        }
        if let Some(request) = asked {
            self.rename_request(ctx, request);
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
        let ppp = ui.ctx().pixels_per_point();
        let lane = views::lanes(plot, self.targets().len())[0].height();
        self.fit_to(plot.width() * ppp, lane * ppp);
        let Some(current) = &self.current else { return };
        let response = ui.interact(plot, ui.id().with("spectrogram"), Sense::click_and_drag());
        let painter = ui.painter_at(area);
        let span = Span::new(plot, &self.view);
        let (rate, scale) = (self.rate(), self.hz_scale());
        let targets = self.targets();
        let lanes = views::lanes(plot, targets.len());
        let view = self.look().view;
        let (lo, hi) = spectrogram::band(&view, current.info.sample_rate, self.settings.fft);
        let held = spectrogram::held(&view, current.info.sample_rate, self.settings.fft);
        let note = self.limit_note();
        for (i, (lane, target)) in lanes.iter().zip(&targets).enumerate() {
            painter.rect_filled(*lane, 0.0, Color32::BLACK);
            let height_of = |f: f32| {
                lane.bottom() - views::freq_t(f, lo, hi, view.log).clamp(0.0, 1.0) * lane.height()
            };
            // What the file holds from the bottom up, and past it, nothing.
            let limit = held.map_or(lane.bottom(), |(_, top)| height_of(top));
            let rows = Rect::from_x_y_ranges(lane.x_range(), limit..=lane.bottom());
            for shown in [&self.whole, &self.detail].into_iter().flatten() {
                if held.is_some()
                    && shown.analysis.targets == targets
                    && let Some(texture) = shown.textures.get(i)
                {
                    views::place(&painter, rows, span, texture.id(), &shown.analysis.range);
                }
            }
            if hi > current.info.nyquist() {
                let past = Rect::from_x_y_ranges(lane.x_range(), lane.top()..=limit);
                views::beyond(&painter, past, &note);
                painter.hline(lane.x_range(), limit, Stroke::new(1.0, views::LIMIT));
            }
            views::freq_axis(&painter, *lane, lo, hi, view.log, scale);
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
        let axis = FreqAxis {
            lanes: lanes.clone(),
            lo,
            hi,
            log: view.log,
        };
        let style = self.area_style();
        for region in self.regions.iter().chain(&self.drawn()) {
            let frames = region.frames.start as f64..region.frames.end as f64;
            for lane in &lanes {
                let rows = axis.rows(*lane, region.band);
                views::region(&painter, *lane, span, &frames, rows, style);
            }
        }
        let tabs = views::markers_on(&painter, plot, span, self.markers(), true);
        if let Some(pos) = response.hover_pos()
            && let Some(lane) = lanes.iter().find(|l| l.contains(pos))
        {
            let f =
                views::freq_at((lane.bottom() - pos.y) / lane.height(), lo, hi, view.log) * scale;
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
        self.marker_tabs(ui, tabs, span);
        self.plot_input(ui, &response, span, Some(&axis));
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
            views::strip(&painter, lane);
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
            let rate = self.rate();
            views::time_axis(
                &painter,
                plot,
                span,
                rate,
                current.meta.start.as_ref().map(|s| s.seconds),
            );
        }
        self.overlays(&painter, plot, span);
        let style = self.area_style();
        for region in self.regions.iter().chain(&self.drawn()) {
            let frames = region.frames.start as f64..region.frames.end as f64;
            for lane in views::lanes(plot, channels) {
                views::region(&painter, lane, span, &frames, lane.y_range(), style);
            }
        }
        // The tabs go on whichever of the two is on top.
        let on_top = !self.settings.views.spectrogram;
        let tabs = views::markers_on(&painter, plot, span, self.markers(), on_top);
        if on_top {
            self.busy(&painter, plot);
        }
        self.marker_tabs(ui, tabs, span);
        self.plot_input(ui, &response, span, None);
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

    /// What is under way in the corner of the plot on top: an analysis, an
    /// export, and for a moment where the export went. There rather than in
    /// a row of controls, which would wrap differently with each.
    fn busy(&self, painter: &Painter, plot: Rect) {
        let analysing = self.detail_job.as_ref().or(self.whole_job.as_ref());
        let exported = self
            .exported
            .as_ref()
            .filter(|(_, at)| at.elapsed() < EXPORTED_NOTE)
            .map(|(path, _)| path.file_name().unwrap_or_default().to_string_lossy());
        let lines = [
            analysing.map(|job| format!("Analysing {}%", job.percent())),
            self.exporting
                .as_ref()
                .map(|job| format!("Exporting {}%", job.percent())),
            exported.map(|name| format!("Saved {name}")),
        ];
        for (i, line) in lines.into_iter().flatten().enumerate() {
            painter.text(
                plot.right_top() + Vec2::new(-8.0, 8.0 + 18.0 * i as f32),
                Align2::RIGHT_TOP,
                line,
                FontId::proportional(12.0),
                Color32::WHITE,
            );
        }
    }

    /// A marker's tab: a click goes to the marker, a drag moves it.
    fn marker_tabs(&mut self, ui: &egui::Ui, tabs: Vec<(u32, Rect)>, span: Span) {
        let frames = self.frames();
        let mut go = None;
        for (id, tab) in tabs {
            let response = ui.interact(tab, ui.id().with(("marker", id)), Sense::click_and_drag());
            if response.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::Grab);
            }
            let locked = self.saving.is_some();
            let Some(m) = self
                .edits
                .as_mut()
                .and_then(|e| e.markers.iter_mut().find(|m| m.id == id))
            else {
                continue;
            };
            if response.drag_started() && !locked {
                let taken = ui
                    .input(|i| i.pointer.press_origin())
                    .map_or(m.frame as f64, |p| span.frame(p.x));
                self.marker_grab = Some((id, taken - m.frame as f64));
            }
            if response.dragged()
                && let (Some((grabbed, offset)), Some(pos)) =
                    (self.marker_grab, response.interact_pointer_pos())
                && grabbed == id
            {
                ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                let last = frames.saturating_sub(m.length) as f64;
                m.frame = (span.frame(pos.x) - offset).clamp(0.0, last) as usize;
            }
            if response.clicked() {
                go = Some(m.frame);
            }
            if response.drag_stopped() {
                self.marker_grab = None;
                if let Some(edits) = &mut self.edits {
                    edits.markers.sort_by_key(|m| (m.frame, m.id));
                }
            }
        }
        if let Some(frame) = go {
            self.seek(frame);
        }
    }

    /// Mouse on a plot, as in the original: click to seek, drag to select,
    /// right-drag to pan, pinch or ⌘/Ctrl-scroll to zoom, sideways scroll to
    /// pan. With Shift held, a drag adds an area to play and a click on one
    /// takes it away; on the spectrogram, `freq`, an area has a band as well
    /// as a stretch.
    fn plot_input(
        &mut self,
        ui: &egui::Ui,
        response: &Response,
        span: Span,
        freq: Option<&FreqAxis>,
    ) {
        let (frames, rate) = (self.frames(), self.rate());
        let shift = ui.input(|i| i.modifiers.shift);
        if response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        }
        if response.drag_started_by(PointerButton::Primary) {
            let origin = ui.input(|i| i.pointer.press_origin());
            let from = origin.map_or(span.start, |p| span.frame(p.x));
            if shift {
                let lane = origin.zip(freq).and_then(|(p, f)| f.lane_at(p));
                let hz = origin
                    .zip(freq)
                    .zip(lane)
                    .map_or(0.0, |((p, f), l)| f.hz(l, p));
                let y = origin.map_or(0.0, |p| p.y);
                self.drawing = Some(Drawing {
                    lane,
                    from: (from, hz, y),
                    to: (from, hz, y),
                });
            } else {
                self.selecting = Some(from..from);
            }
        }
        if response.dragged_by(PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
        {
            if let Some(selecting) = &mut self.selecting {
                selecting.end = span.frame(pos.x);
            }
            if let Some(drawing) = &mut self.drawing {
                let hz = freq.zip(drawing.lane).map_or(0.0, |(f, l)| f.hz(l, pos));
                drawing.to = (span.frame(pos.x), hz, pos.y);
            }
        }
        if response.drag_stopped_by(PointerButton::Primary) {
            if let Some(s) = self.selecting.take() {
                // A plain drag lets every area go, and selects as it did.
                self.clear_regions();
                let (from, to) = (s.start.min(s.end), s.start.max(s.end));
                // Under a tenth of a second is a slip of the hand, as in the
                // original.
                if to - from >= 0.1 * rate {
                    self.select(Some(from as usize..(to as usize).min(frames)));
                } else {
                    self.select(None);
                }
            }
            let drawn = self.drawn();
            if let (Some(d), Some(mut region)) = (self.drawing.take(), drawn) {
                region.frames.end = region.frames.end.min(frames);
                // Too short or too flat to be meant, or all of it past what
                // the file holds.
                let tall = d.lane.is_none() || (d.from.2 - d.to.2).abs() >= 3.0;
                let held = region.band.1 > region.band.0;
                if region.frames.len() as f64 >= 0.05 * rate && tall && held {
                    self.add_region(region);
                }
            }
        }
        if response.clicked_by(PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
        {
            match self.region_at(pos, span, freq).filter(|_| shift) {
                Some(i) => self.remove_region(i),
                None => {
                    self.select(None);
                    self.seek(span.frame(pos.x) as usize);
                }
            }
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

    /// The area being drawn, as it would be added: on the waveform, with
    /// every frequency, and on the spectrogram only up to what the file
    /// holds.
    fn drawn(&self) -> Option<Region> {
        let d = self.drawing.as_ref()?;
        let nyquist = self.current.as_ref().map_or(0.0, |c| c.info.nyquist());
        let (lo, hi) = (d.from.1.min(d.to.1), d.from.1.max(d.to.1));
        Some(Region {
            frames: d.from.0.min(d.to.0) as usize..d.from.0.max(d.to.0) as usize,
            band: match d.lane {
                Some(_) => (lo.min(nyquist), hi.min(nyquist)),
                None => (0.0, nyquist),
            },
        })
    }

    /// The last area drawn under `pos`: in its stretch, and on the
    /// spectrogram in its band too.
    fn region_at(&self, pos: Pos2, span: Span, freq: Option<&FreqAxis>) -> Option<usize> {
        let frame = span.frame(pos.x);
        let hz = freq.and_then(|f| f.lane_at(pos).map(|lane| f.hz(lane, pos)));
        self.regions.iter().rposition(|r| {
            (r.frames.start as f64..r.frames.end as f64).contains(&frame)
                && hz.is_none_or(|hz| (r.band.0..=r.band.1).contains(&hz))
        })
    }

    /// Adds an area, or with more than one not allowed puts it in place of
    /// the one before, and plays the areas from its start: in place of a
    /// selection, which it lets go.
    fn add_region(&mut self, region: Region) {
        let start = region.frames.start;
        if !self.settings.multiple_areas {
            self.regions.clear();
        }
        self.regions.push(region);
        self.selection = None;
        self.cursor = Some(start);
        if self.ensure_player(start)
            && let Some(player) = &self.player
        {
            player.set_regions(self.regions.clone(), start);
            player.play();
        }
    }

    fn remove_region(&mut self, index: usize) {
        self.regions.remove(index);
        if let Some(player) = &self.player {
            player.set_regions(self.regions.clone(), player.position());
        }
    }

    fn clear_regions(&mut self) {
        if self.regions.is_empty() {
            return;
        }
        self.regions.clear();
        if let Some(player) = &self.player {
            player.set_regions(Vec::new(), player.position());
        }
    }

    /// The whole file under the spectrogram: each channel's waveform, and
    /// the part the spectrogram shows. Dragging an end of that part moves
    /// the end, dragging the part itself moves it along, and dragging
    /// anywhere else shows the stretch dragged over, the last two taking
    /// the playhead, and playback, to its start; a click shows the moment
    /// clicked and moves the playhead there; a right-drag or a sideways
    /// scroll moves the part along, and a pinch or ⌘/Ctrl-scroll sizes it.
    /// Picked, the arrows move and size it.
    fn overview_view(&mut self, ui: &mut egui::Ui) {
        let (area, plot) = Self::plot_rect(ui, true);
        if plot.width() < 20.0 || plot.height() < 20.0 {
            return;
        }
        let Some(current) = &self.current else { return };
        let response = ui.interact(plot, ui.id().with("overview"), Sense::click_and_drag());
        self.controls.push((Control::Overview, plot));
        let painter = ui.painter_at(area);
        let frames = current.info.frames as f64;
        let span = Span::new(plot, &(0.0..frames));
        let channels = usize::from(current.info.channels);
        let lanes = views::lanes(plot, channels);
        for (c, lane) in lanes.iter().enumerate() {
            views::strip(&painter, *lane);
            views::centre_line(&painter, *lane);
            if let Some(whole) = &self.whole {
                let a = &whole.analysis;
                let color = views::PALETTE[c % views::PALETTE.len()];
                views::waveform(&painter, *lane, span, &a.envelope[c], &a.range, color);
            }
            if channels > 1 {
                views::lane_label(&painter, *lane, &self.channel_name(c));
            }
        }
        let start = current.meta.start.as_ref().map(|s| s.seconds);
        views::time_axis(&painter, plot, span, self.rate(), start);
        views::markers_on(&painter, plot, span, self.markers(), false);
        let style = self.area_style();
        for region in &self.regions {
            let frames = region.frames.start as f64..region.frames.end as f64;
            for lane in &lanes {
                views::region(&painter, *lane, span, &frames, lane.y_range(), style);
            }
        }
        if let Some(s) = &self.selection {
            views::selection(&painter, plot, span, &(s.start as f64..s.end as f64));
        }
        views::viewport(&painter, plot, frames, &self.view);
        if let Some(c) = self.cursor {
            views::cursor(&painter, plot, span, c as f64);
        }
        self.overview_input(ui, &response, plot, frames);
    }

    fn overview_input(&mut self, ui: &egui::Ui, response: &Response, plot: Rect, frames: f64) {
        let x_of = |frame: f64| plot.left() + (frame / frames) as f32 * plot.width();
        let frame_at =
            |x: f32| f64::from(((x - plot.left()) / plot.width()).clamp(0.0, 1.0)) * frames;
        let (x0, x1) = views::view_span(plot, frames, &self.view);
        // Each end is held from just outside the box to a quarter of the way
        // in, so even a narrow box keeps a middle to move it by.
        let reach = GRIP.min((x1 - x0) / 4.0);
        let end_at = |x: f32| {
            if (-GRIP..=reach).contains(&(x - x0)) {
                Some(Grab::Start)
            } else if (-GRIP..=reach).contains(&(x1 - x)) {
                Some(Grab::End)
            } else {
                None
            }
        };
        let inside = |x: f32| x > x0 + reach && x < x1 - reach;
        if let Some(pos) = response.hover_pos() {
            let resizing =
                end_at(pos.x).is_some() || matches!(self.grab, Some(Grab::Start | Grab::End));
            ui.ctx().set_cursor_icon(if resizing {
                CursorIcon::ResizeHorizontal
            } else if matches!(self.grab, Some(Grab::Move(_))) {
                CursorIcon::Grabbing
            } else if inside(pos.x) && self.zoomed() {
                CursorIcon::Grab
            } else {
                CursorIcon::Crosshair
            });
            let (zoom, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta()));
            if zoom != 1.0 {
                let centre = (self.view.start + self.view.end) / 2.0;
                self.zoom_at(centre, 1.0 / f64::from(zoom));
            }
            if scroll.x != 0.0 {
                self.pan(-f64::from(scroll.x / plot.width()) * frames);
            }
        }
        if response.drag_started_by(PointerButton::Primary) {
            let from = ui
                .input(|i| i.pointer.press_origin())
                .map_or(plot.left(), |p| p.x);
            let taken = frame_at(from) - self.view.start;
            self.grab = Some(end_at(from).unwrap_or(if inside(from) && self.zoomed() {
                Grab::Move(taken)
            } else {
                Grab::Span(frame_at(from))
            }));
        }
        if response.dragged_by(PointerButton::Primary)
            && let (Some(grab), Some(pos)) = (self.grab, response.interact_pointer_pos())
        {
            let at = frame_at(pos.x);
            let shortest = self.shortest_view();
            let (start, end) = (self.view.start, self.view.end);
            match grab {
                Grab::Start => self.set_view_range(at.min(end - shortest), end),
                Grab::End => self.set_view_range(start, at.max(start + shortest)),
                Grab::Move(taken) => self.set_view(at - taken, end - start),
                // Past a few points, so a click that wobbles stays a click.
                Grab::Span(from) if (x_of(from) - pos.x).abs() > 3.0 => {
                    self.set_view_range(from.min(at), from.max(at));
                }
                Grab::Span(_) => {}
            }
        }
        if response.drag_stopped() {
            // A new stretch, or the part in view moved to another, is where
            // playback goes; sizing it only moves the playhead once it is
            // left out.
            if matches!(self.grab, Some(Grab::Span(_) | Grab::Move(_))) {
                self.play_from_view();
            }
            self.grab = None;
        }
        if response.dragged_by(PointerButton::Secondary) {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            self.pan(f64::from(response.drag_delta().x / plot.width()) * frames);
        }
        if response.clicked_by(PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
        {
            let (at, len) = (frame_at(pos.x), self.view_len());
            self.set_view(at - len / 2.0, len);
            self.select(None);
            self.seek(at as usize);
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
        let names: Vec<String> = (0..channels).map(|c| self.channel_name(c)).collect();
        let mut muted: Vec<bool> = (0..channels)
            .map(|c| self.mute.get(c) == Some(&true))
            .collect();
        // What is heard: the file's level plus the playback gain, over the
        // stretch of file one screen refresh covers at this speed, and
        // nothing from a channel muted.
        let levels = self
            .player
            .as_ref()
            .filter(|p| p.is_playing())
            .map(|player| {
                let span = f64::from(current.info.sample_rate) * self.speed().value() / 20.0;
                let gain = self.settings.gain;
                current
                    .levels
                    .at(player.position(), span as usize)
                    .into_iter()
                    .zip(&muted)
                    .map(|(level, &off)| {
                        if off {
                            Level {
                                rms_db: FLOOR_DB,
                                peak_db: FLOOR_DB,
                            }
                        } else {
                            lift(level, gain)
                        }
                    })
                    .collect::<Vec<_>>()
            });
        ui.add_space(4.0);
        let both = &mut self.settings.both_sides;
        self.meters
            .ui(ui, levels.as_deref(), &labels, &names, &mut muted, both);
        if self.mute.len() < channels {
            self.mute.resize(channels, false);
        }
        self.mute[..channels].copy_from_slice(&muted);
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
        let at = spectrum.request.frame as f64 / self.rate();
        ui.strong(format!("Spectrum at {}", views::clock_fine(at)));
        let area = ui.available_rect_before_wrap();
        let plot = Rect::from_min_max(
            area.min + Vec2::new(42.0, 10.0),
            area.max - Vec2::new(10.0, 26.0),
        );
        if plot.width() < 20.0 || plot.height() < 20.0 {
            return;
        }
        let response = ui.interact(plot, ui.id().with("spectrum"), Sense::hover());
        if response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
        }
        let painter = ui.painter_at(area);
        let nyquist = current.info.nyquist();
        let view = self.look().view;
        // The same band as the spectrogram, on the same kind of axis.
        let (lo, hi) = spectrogram::band(&view, current.info.sample_rate, spectrum.request.fft);
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
        let axes = views::Axes {
            lo,
            hi,
            log: view.log,
            top,
            bottom,
            scale: self.hz_scale(),
        };
        views::spectrum(
            &painter,
            plot,
            &curves,
            nyquist,
            &axes,
            response.hover_pos(),
            &self.limit_note(),
        );
    }

    fn markers_view(&mut self, ui: &mut egui::Ui) {
        Self::claim(ui);
        ui.add_space(4.0);
        let count = self.markers().len();
        let can_add = self.edits.as_ref().is_some_and(|e| e.wav) && self.saving.is_none();
        let mut add = false;
        ui.horizontal(|ui| {
            ui.strong(format!("Markers ({count})"));
            add = ui
                .add_enabled(can_add, egui::Button::new("Add").small())
                .on_hover_text("M: a marker at the playhead, or the selection as a region")
                .clicked();
        });
        if add {
            self.add_marker();
        }
        let (rate, locked) = (self.rate(), self.saving.is_some());
        let action = match (&self.current, self.edits.as_mut().filter(|e| e.wav)) {
            (Some(_), Some(edits)) if edits.markers.is_empty() => {
                ui.weak("M marks the playhead, or a selected stretch as a region. Saving keeps them in the file as standard WAV cue points, as recorders and Reaper write them.");
                None
            }
            (Some(_), Some(edits)) => {
                views::marker_list(ui, &mut edits.markers, rate, locked, self.name_next.take())
            }
            (Some(_), None) => {
                ui.weak("Markers are kept inside WAV files, and this is not one.");
                None
            }
            (None, _) => {
                ui.weak("Open a file to see its markers");
                None
            }
        };
        match action {
            Some(MarkerAction::Seek(frame)) => self.seek(frame),
            Some(MarkerAction::Remove(id)) => {
                if let Some(edits) = &mut self.edits {
                    edits.markers.retain(|m| m.id != id);
                }
            }
            None => {}
        }
    }

    fn metadata_view(&mut self, ui: &mut egui::Ui) {
        Self::claim(ui);
        ui.add_space(4.0);
        ui.strong("Metadata");
        let locked = self.saving.is_some();
        match &self.current {
            Some(current) => views::metadata(ui, &current.details, self.edits.as_mut(), locked),
            None => {
                ui.weak("Open a file to see what it says about itself");
            }
        }
    }

    /// Asks what to do with unsaved changes before opening another file or
    /// quitting.
    fn ask(&mut self, ctx: &egui::Context) {
        let Some(then) = self.asking.clone() else {
            return;
        };
        let name = self
            .file
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let mut answer = None;
        let modal = egui::Modal::new(egui::Id::new("unsaved")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.heading("Unsaved changes");
            ui.add_space(4.0);
            ui.label(format!(
                "{name} has changes to its metadata or markers that are not saved."
            ));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    answer = Some(true);
                }
                if ui.button("Don't save").clicked() {
                    answer = Some(false);
                }
                if ui.button("Cancel").clicked() {
                    self.asking = None;
                }
            });
        });
        if modal.should_close() && answer.is_none() {
            self.asking = None;
        }
        match answer {
            Some(true) => {
                self.asking = None;
                self.then = Some(then);
                self.save(ctx);
            }
            Some(false) => {
                self.asking = None;
                self.edits = self.saved.clone();
                self.then = Some(then);
                self.go_ahead(ctx);
            }
            None => {}
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
        if self.settings.views.rename || self.rename_window {
            self.renamer.follow(self.explorer.root(), &ctx);
        }
        self.input(&ctx);
        self.controls.clear();
        self.follow_playback(&ctx);
        self.start_due_analysis(&ctx);
        self.update_probe();
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quitting
            && (self.dirty() || self.saving.is_some())
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.quit(&ctx);
        }
        if self.inbox.quit_asked() {
            self.quit(&ctx);
        }
        let busy = [
            &self.loading,
            &self.whole_job,
            &self.detail_job,
            &self.saving,
            &self.exporting,
        ];
        if busy.iter().any(|job| job.is_some()) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if let Some(left) = self
            .exported
            .as_ref()
            .and_then(|(_, at)| EXPORTED_NOTE.checked_sub(at.elapsed()))
        {
            ctx.request_repaint_after(left);
        }

        let widest = (ui.available_width() / 6.0).max(140.0);
        let mut clicked = None;
        let explorer = egui::Panel::left("explorer")
            .resizable(true)
            .default_size(widest.min(240.0))
            .min_size(140.0)
            .max_size(widest)
            .show_collapsible(ui, &mut self.settings.explorer, |ui| {
                let views = self.settings.views;
                let parts = Parts {
                    shortcuts: views.shortcuts.then_some(&mut self.settings.shortcuts),
                    search: views.search,
                };
                clicked = self.explorer.ui(ui, parts)
            });
        if let Some(explorer) = explorer {
            self.mark_control(Control::Explorer, explorer.response.rect.shrink(5.0));
        }
        if let Some(path) = clicked {
            self.request_open(&ctx, path);
        }

        egui::Panel::top("header").show(ui, |ui| self.header(ui));
        egui::Panel::bottom("controls").show(ui, |ui| self.controls(ui));
        let views = self.settings.views;
        if self.current.is_some() && views.meters {
            egui::Panel::bottom("meters").show(ui, |ui| self.meters_view(ui));
        }
        self.refresh_textures(&ctx);
        let central = views.spectrogram || views.waveform;
        let side = views.spectrum || views.markers || views.metadata || views.rename;
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
        // A new marker's name box that was not drawn this frame waits for
        // no later one.
        self.name_next = None;
        self.tune_player();
        // Last, so that no panel paints over it.
        if let Some((_, rect)) = self.controls.iter().find(|(c, _)| Some(*c) == self.picked) {
            ui.painter().rect_stroke(
                rect.expand(3.0),
                4.0,
                Stroke::new(1.5, PICKED),
                StrokeKind::Outside,
            );
        }
        if self.rename_window {
            self.rename_window(&ctx);
        }
        if self.area_window {
            self.area_window(&ctx);
        }
        self.ask(&ctx);
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
        .smallest_positive(f64::from(LOWEST_HZ))
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

fn summary(c: &Loaded, pitch: f32) -> String {
    let info = &c.info;
    let mut parts = vec![
        info.container.clone(),
        format!("{}Hz", views::hz(info.sample_rate as f32)),
    ];
    parts.extend(info.bits.map(|b| format!("{b}-bit")));
    parts.push(format!("{} ch", info.channels));
    parts.push(views::clock(info.seconds()));
    if pitch != 0.0 {
        parts.push(format!("pitch {pitch:+} semitones"));
    }
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
        assert!(!back.views.meters && back.views.spectrogram && back.views.markers);
    }

    #[test]
    fn a_speed_from_an_older_file_still_reads_and_slow_ones_need_switching_on() {
        let mut storage = Memory::default();
        storage.set_string(eframe::APP_KEY, "(speed: 4)".to_owned());
        let back: Settings = eframe::get_value(&storage, eframe::APP_KEY).unwrap();
        assert_eq!(back.sanitized().speed, 4.0);
        let slow = Settings {
            speed: 0.125,
            ..Settings::default()
        };
        assert_eq!(slow.sanitized().speed, 1.0);
        let switched_on = Settings {
            speed: 0.125,
            tools: Tools {
                slow: true,
                ..Tools::default()
            },
            ..Settings::default()
        };
        assert_eq!(switched_on.sanitized().speed, 0.125);
    }

    #[test]
    fn settings_out_of_range_are_brought_back() {
        let settings = Settings {
            fft: 1000,
            speed: 3.0,
            brightness: 500.0,
            contrast: f32::NAN,
            gain: -90.0,
            ..Settings::default()
        }
        .sanitized();
        assert_eq!((settings.fft, settings.speed), (2048, 1.0));
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
