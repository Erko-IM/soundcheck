//! Drawing for each view: axes and lanes, the waveform, the timeline, the
//! meters, the spectrum and the metadata.

use std::collections::HashMap;
use std::ops::Range;

use eframe::egui::{
    self, Align, Align2, Button, Color32, ColorImage, CursorIcon, FontId, Label, Layout, Painter,
    Pos2, Rect, Response, RichText, Sense, Shape, Stroke, StrokeKind, TextEdit, TextureHandle,
    TextureId, TextureOptions, Ui, Vec2,
};

use crate::edit::{BEXT_FIELDS, Edits};
use crate::levels::{FLOOR_DB, Level};
use crate::meta::{self, Details};
use crate::wav::Marker;

pub const CURSOR: Color32 = Color32::from_rgb(235, 70, 60);
pub const AXIS: Color32 = Color32::from_gray(150);
const GRID: Color32 = Color32::from_gray(48);
const ELAPSED: Color32 = Color32::from_gray(220);
const WALL_CLOCK: Color32 = Color32::from_gray(115);
/// Orange, like a control picked for the keys: what is chosen.
const SELECTION: Color32 = Color32::from_rgba_unmultiplied_const(255, 140, 50, 45);
const SELECTION_EDGE: Color32 = Color32::from_rgba_unmultiplied_const(255, 140, 50, 180);
const VIEWPORT: Color32 = Color32::from_rgba_unmultiplied_const(90, 159, 212, 64);
const VIEWPORT_EDGE: Color32 = Color32::from_rgba_unmultiplied_const(90, 159, 212, 153);
const STRIP_BACK: Color32 = Color32::from_gray(22);
const STRIP_EDGE: Color32 = Color32::from_rgba_unmultiplied_const(236, 224, 160, 170);
const TIMELINE_WAVE: Color32 = Color32::from_gray(85);
pub const MARK: Color32 = Color32::from_rgb(255, 196, 70);
const MARK_SPAN: Color32 = Color32::from_rgba_unmultiplied_const(255, 196, 70, 30);
const LISTENER: Color32 = Color32::from_gray(150);
const METER_BACK: Color32 = Color32::from_gray(28);
const METER_LOW: Color32 = Color32::from_rgb(70, 190, 110);
const METER_MID: Color32 = Color32::from_rgb(235, 160, 50);
const METER_HIGH: Color32 = Color32::from_rgb(230, 60, 50);

/// One colour per channel, repeating past eight.
pub const PALETTE: [Color32; 8] = [
    Color32::from_rgb(120, 175, 220),
    Color32::from_rgb(235, 160, 80),
    Color32::from_rgb(130, 200, 120),
    Color32::from_rgb(220, 110, 160),
    Color32::from_rgb(190, 160, 230),
    Color32::from_rgb(230, 210, 100),
    Color32::from_rgb(100, 200, 200),
    Color32::from_rgb(200, 130, 110),
];

/// Frames spread across a strip of the screen.
#[derive(Clone, Copy)]
pub struct Span {
    pub left: f32,
    pub width: f32,
    pub start: f64,
    pub len: f64,
}

impl Span {
    pub fn new(rect: Rect, view: &Range<f64>) -> Self {
        Self {
            left: rect.left(),
            width: rect.width().max(1.0),
            start: view.start,
            len: (view.end - view.start).max(1.0),
        }
    }

    pub fn x(self, frame: f64) -> f32 {
        self.left + ((frame - self.start) / self.len) as f32 * self.width
    }

    /// The frame at `x`, anywhere along the line through the strip.
    fn at(self, x: f32) -> f64 {
        self.start + f64::from((x - self.left) / self.width) * self.len
    }

    /// The frame under `x`, held to the strip.
    pub fn frame(self, x: f32) -> f64 {
        self.at(x.clamp(self.left, self.left + self.width))
    }
}

/// `rect` cut into `count` lanes, one above the other.
pub fn lanes(rect: Rect, count: usize) -> Vec<Rect> {
    let count = count.max(1);
    let gap = if count > 1 { 4.0 } else { 0.0 };
    let height = (rect.height() - gap * (count - 1) as f32) / count as f32;
    (0..count)
        .map(|i| {
            let top = rect.top() + i as f32 * (height + gap);
            Rect::from_x_y_ranges(rect.x_range(), top..=top + height)
        })
        .collect()
}

pub fn lane_label(painter: &Painter, lane: Rect, text: &str) {
    let galley =
        painter.layout_no_wrap(text.to_owned(), FontId::proportional(12.0), Color32::WHITE);
    let back = Rect::from_min_size(
        lane.min + Vec2::new(4.0, 4.0),
        galley.size() + Vec2::new(8.0, 4.0),
    );
    painter.rect_filled(back, 3.0, Color32::from_black_alpha(150));
    painter.galley(back.min + Vec2::new(4.0, 2.0), galley, Color32::WHITE);
}

/// Draws the part of `texture`, which shows the frames in `range`, that
/// falls inside `lane`.
pub fn place(painter: &Painter, lane: Rect, span: Span, texture: TextureId, range: &Range<usize>) {
    let (x0, x1) = (span.x(range.start as f64), span.x(range.end as f64));
    let (left, right) = (x0.max(lane.left()), x1.min(lane.right()));
    if right <= left {
        return;
    }
    let u = |x: f32| (x - x0) / (x1 - x0);
    let uv = Rect::from_min_max(Pos2::new(u(left), 0.0), Pos2::new(u(right), 1.0));
    let rect = Rect::from_x_y_ranges(left..=right, lane.y_range());
    painter.image(texture, rect, uv, Color32::WHITE);
}

pub fn selection(painter: &Painter, plot: Rect, span: Span, range: &Range<f64>) {
    let (x0, x1) = (span.x(range.start), span.x(range.end));
    let (left, right) = (x0.max(plot.left()), x1.min(plot.right()));
    if right <= left {
        return;
    }
    painter.rect_filled(
        Rect::from_x_y_ranges(left..=right, plot.y_range()),
        0.0,
        SELECTION,
    );
    for x in [x0, x1] {
        if (plot.left()..=plot.right()).contains(&x) {
            painter.vline(x, plot.y_range(), Stroke::new(1.0, SELECTION_EDGE));
        }
    }
}

pub fn cursor(painter: &Painter, plot: Rect, span: Span, frame: f64) {
    let x = span.x(frame);
    if (plot.left()..=plot.right()).contains(&x) {
        painter.vline(x, plot.y_range(), Stroke::new(1.5, CURSOR));
    }
}

/// Elapsed time under `plot`, and under that the time of day when the
/// recording says when it started.
pub fn time_axis(painter: &Painter, plot: Rect, span: Span, rate: f64, wall_start: Option<f64>) {
    let step = time_step(span.len / rate, plot.width());
    let decimals = decimals(step);
    let first = (span.start / rate / step).ceil() as i64;
    let last = ((span.start + span.len) / rate / step).floor() as i64;
    for i in first..=last {
        let t = i as f64 * step;
        let x = span.x(t * rate);
        painter.vline(
            x,
            plot.bottom()..=plot.bottom() + 5.0,
            Stroke::new(1.0, AXIS),
        );
        painter.text(
            Pos2::new(x, plot.bottom() + 7.0),
            Align2::CENTER_TOP,
            clock_with(t, decimals),
            FontId::monospace(13.0),
            ELAPSED,
        );
        if let Some(start) = wall_start {
            painter.text(
                Pos2::new(x, plot.bottom() + 25.0),
                Align2::CENTER_TOP,
                wall_with(start + t, decimals),
                FontId::monospace(10.0),
                WALL_CLOCK,
            );
        }
    }
}

pub fn freq_axis(painter: &Painter, lane: Rect, lo: f32, hi: f32, log: bool) {
    for f in freq_ticks(lo, hi, log, lane.height(), 16.0) {
        let y = lane.bottom() - freq_t(f, lo, hi, log) * lane.height();
        painter.hline(lane.left() - 5.0..=lane.left(), y, Stroke::new(1.0, AXIS));
        painter.text(
            Pos2::new(lane.left() - 8.0, y),
            Align2::RIGHT_CENTER,
            hz(f),
            FontId::monospace(11.0),
            AXIS,
        );
    }
}

/// Where `f` sits between `lo` (0) and `hi` (1).
pub fn freq_t(f: f32, lo: f32, hi: f32, log: bool) -> f32 {
    if log {
        (f / lo).ln() / (hi / lo).ln()
    } else {
        (f - lo) / (hi - lo)
    }
}

pub fn freq_at(t: f32, lo: f32, hi: f32, log: bool) -> f32 {
    if log {
        lo * (hi / lo).powf(t)
    } else {
        lo + (hi - lo) * t
    }
}

/// One channel's lowest and highest sample per column of `range`, drawn as
/// a band per point across `lane`, with full scale at its edges.
pub fn waveform(
    painter: &Painter,
    lane: Rect,
    span: Span,
    envelope: &[[f32; 2]],
    range: &Range<usize>,
    color: Color32,
) {
    let columns = envelope.len();
    if columns == 0 || range.is_empty() {
        return;
    }
    let per_column = range.len() as f64 / columns as f64;
    let (mid, half) = (lane.center().y, lane.height() / 2.0 - 1.0);
    let right = span.x(range.end as f64).min(lane.right());
    let mut x = span.x(range.start as f64).max(lane.left()).floor();
    let mut shapes = Vec::new();
    while x < right {
        let from = (span.at(x) - range.start as f64) / per_column;
        let to = (span.at(x + 1.0) - range.start as f64) / per_column;
        let first = from.floor().max(0.0) as usize;
        if first >= columns {
            break;
        }
        let last = (to.ceil() as usize).clamp(first + 1, columns);
        let (lo, hi) = envelope[first..last]
            .iter()
            .fold((f32::MAX, f32::MIN), |(l, h), e| (l.min(e[0]), h.max(e[1])));
        let top = mid - hi.clamp(-1.0, 1.0) * half;
        let bottom = (mid - lo.clamp(-1.0, 1.0) * half).max(top + 1.0);
        shapes.push(Shape::line_segment(
            [Pos2::new(x + 0.5, top), Pos2::new(x + 0.5, bottom)],
            Stroke::new(1.0, color),
        ));
        x += 1.0;
    }
    painter.extend(shapes);
}

pub fn centre_line(painter: &Painter, lane: Rect) {
    painter.hline(lane.x_range(), lane.center().y, Stroke::new(1.0, GRID));
}

/// The dark grey ground of a waveform lane or the timeline, edged in pale
/// yellow to stand out from the panel around it.
pub fn strip(painter: &Painter, rect: Rect) {
    painter.rect_filled(rect, 2.0, STRIP_BACK);
    painter.rect_stroke(rect, 2.0, Stroke::new(1.0, STRIP_EDGE), StrokeKind::Outside);
}

/// The whole file, as in the original's minimap: the loudest channel
/// mirrored about the middle, the markers, the part in view with a grip at
/// each end, and the playhead.
pub fn timeline(
    painter: &Painter,
    rect: Rect,
    overview: &[Vec<[f32; 2]>],
    frames: usize,
    view: &Range<f64>,
    playhead: Option<usize>,
    markers: &[Marker],
) {
    strip(painter, rect);
    let columns = overview.first().map_or(0, Vec::len);
    if columns == 0 || frames == 0 {
        return;
    }
    let (mid, half) = (rect.center().y, rect.height() / 2.0);
    let pixels = rect.width().ceil().max(1.0) as usize;
    let mut shapes = Vec::with_capacity(pixels);
    for p in 0..pixels {
        let first = (p * columns / pixels).min(columns - 1);
        let last = ((p + 1) * columns)
            .div_ceil(pixels)
            .clamp(first + 1, columns);
        let amplitude = overview
            .iter()
            .flat_map(|channel| &channel[first..last])
            .fold(0.0f32, |a, e| a.max(e[0].abs()).max(e[1].abs()))
            .min(1.0);
        let x = rect.left() + p as f32;
        shapes.push(Shape::rect_filled(
            Rect::from_x_y_ranges(x..=x + 1.0, mid - amplitude * half..=mid + amplitude * half),
            0.0,
            TIMELINE_WAVE,
        ));
    }
    painter.extend(shapes);
    let x_of = |frame: f64| rect.left() + (frame / frames as f64) as f32 * rect.width();
    for m in markers {
        painter.vline(x_of(m.frame as f64), rect.y_range(), Stroke::new(1.0, MARK));
    }
    let shown = Rect::from_x_y_ranges(x_of(view.start)..=x_of(view.end), rect.y_range());
    painter.rect_filled(shown, 0.0, VIEWPORT);
    painter.rect_stroke(
        shown,
        0.0,
        Stroke::new(1.0, VIEWPORT_EDGE),
        StrokeKind::Inside,
    );
    let grip = rect.center().y - 7.0..=rect.center().y + 7.0;
    for x in [shown.left() + 1.5, shown.right() - 1.5] {
        painter.vline(x, grip.clone(), Stroke::new(3.0, VIEWPORT_EDGE));
    }
    if let Some(frame) = playhead {
        painter.vline(x_of(frame as f64), rect.y_range(), Stroke::new(1.0, CURSOR));
    }
}

/// Markers across `plot`: a line at each and each region's span shaded,
/// and with `tabs` a tab along the top edge naming each. Returns the tabs,
/// for dragging.
pub fn markers_on(
    painter: &Painter,
    plot: Rect,
    span: Span,
    markers: &[Marker],
    tabs: bool,
) -> Vec<(u32, Rect)> {
    let mut found = Vec::new();
    for (i, m) in markers.iter().enumerate() {
        let x = span.x(m.frame as f64);
        if m.length > 0 {
            let end = span.x((m.frame + m.length) as f64);
            let (left, right) = (x.max(plot.left()), end.min(plot.right()));
            if right > left {
                painter.rect_filled(
                    Rect::from_x_y_ranges(left..=right, plot.y_range()),
                    0.0,
                    MARK_SPAN,
                );
            }
        }
        if !(plot.left()..=plot.right()).contains(&x) {
            continue;
        }
        painter.vline(x, plot.y_range(), Stroke::new(1.0, MARK));
        if tabs {
            let name = match m.label.chars().count() {
                0 => (i + 1).to_string(),
                1..=24 => m.label.clone(),
                _ => format!("{}…", m.label.chars().take(23).collect::<String>()),
            };
            let galley = painter.layout_no_wrap(name, FontId::proportional(11.0), Color32::BLACK);
            let tab = Rect::from_min_size(
                Pos2::new(x, plot.top()),
                galley.size() + Vec2::new(8.0, 2.0),
            );
            painter.rect_filled(tab, 2.0, MARK);
            painter.galley(tab.min + Vec2::new(4.0, 1.0), galley, Color32::BLACK);
            found.push((m.id, tab));
        }
    }
    found
}

/// Who the figure beside a frequency slider turns into, the further past
/// human hearing the slider goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Animal {
    /// Calls and listens far above people.
    Bat,
    /// Blue and fin whales call below people's hearing, and hear it.
    Whale,
}

impl Animal {
    /// How far a person has turned into this animal at `hz`: for a bat,
    /// from 20 kHz, where human hearing ends, wholly by 100 kHz; for a
    /// whale, from 20 Hz, where it starts, wholly by 10 Hz.
    pub fn change(self, hz: f32) -> f32 {
        let past = match self {
            Self::Bat => (hz / 20_000.0).ln() / 5f32.ln(),
            Self::Whale => (20.0 / hz).log2(),
        };
        past.clamp(0.0, 1.0)
    }

    fn figure(self) -> Figure {
        match self {
            Self::Bat => Figure::bat(),
            Self::Whale => Figure::whale(),
        }
    }
}

/// From one point to another, this thick, with round ends.
#[derive(Clone, Copy)]
struct Limb {
    from: Vec2,
    to: Vec2,
    width: f32,
}

impl Limb {
    fn new((ax, ay): (f32, f32), (bx, by): (f32, f32), width: f32) -> Self {
        Self {
            from: Vec2::new(ax, ay),
            to: Vec2::new(bx, by),
            width,
        }
    }

    fn mirrored(self) -> Self {
        Self {
            from: Vec2::new(-self.from.x, self.from.y),
            to: Vec2::new(-self.to.x, self.to.y),
            ..self
        }
    }

    fn covers(self, p: Vec2) -> bool {
        let along = self.to - self.from;
        let t = ((p - self.from).dot(along) / along.length_sq().max(1e-9)).clamp(0.0, 1.0);
        (p - (self.from + along * t)).length() <= self.width / 2.0
    }
}

fn points<const N: usize>(corners: [(f32, f32); N]) -> [Vec2; N] {
    corners.map(|(x, y)| Vec2::new(x, y))
}

fn mirrored<const N: usize>(corners: [Vec2; N]) -> [Vec2; N] {
    corners.map(|p| Vec2::new(-p.x, p.y))
}

/// Counting the edges a ray from `p` crosses, so an outline that folds in
/// on itself still fills as drawn.
fn inside(corners: &[Vec2], p: Vec2) -> bool {
    let mut inside = false;
    for (i, a) in corners.iter().enumerate() {
        let b = corners[(i + 1) % corners.len()];
        if (a.y > p.y) != (b.y > p.y) && p.x < a.x + (b.x - a.x) * (p.y - a.y) / (b.y - a.y) {
            inside = !inside;
        }
    }
    inside
}

/// A figure, in a square from -1 to 1 with y down. A person, a bat and a
/// whale all have these parts, so one turns into another part by part.
#[derive(Clone, Copy)]
struct Figure {
    head: Vec2,
    head_radius: f32,
    body: Limb,
    /// Legs, then arms.
    limbs: [Limb; 4],
    /// Ears, or drops of spray.
    small: [[Vec2; 3]; 2],
    /// Wings, or a tail.
    large: [[Vec2; 9]; 2],
}

impl Figure {
    /// Arms raised, with the ears still inside the head, to grow out of it,
    /// and the wings folded along the arms, to unfold from them.
    fn person() -> Self {
        let head = Vec2::new(0.0, -0.66);
        let arm = Limb::new((0.12, -0.34), (0.62, -0.84), 0.14);
        let leg = Limb::new((0.08, 0.12), (0.28, 0.92), 0.16);
        // How far along the arm each corner of the wing starts.
        let along = [0.0, 1.0, 1.0, 0.9, 0.8, 0.65, 0.5, 0.3, 0.0];
        let wing = along.map(|a| arm.from + (arm.to - arm.from) * a);
        Self {
            head,
            head_radius: 0.19,
            body: Limb::new((0.0, -0.28), (0.0, 0.12), 0.32),
            limbs: [leg.mirrored(), leg, arm.mirrored(), arm],
            small: [[head; 3]; 2],
            large: [mirrored(wing), wing],
        }
    }

    fn bat() -> Self {
        let ear = points([(0.05, -0.4), (0.17, -0.34), (0.16, -0.62)]);
        let wing = points([
            (0.1, -0.14),
            (0.5, -0.44),
            (1.0, -0.2),
            (0.78, -0.02),
            (0.84, 0.2),
            (0.6, 0.12),
            (0.52, 0.34),
            (0.32, 0.18),
            (0.12, 0.26),
        ]);
        let arm = Limb::new((0.1, -0.14), (0.5, -0.44), 0.1);
        let leg = Limb::new((0.06, 0.22), (0.1, 0.42), 0.1);
        Self {
            head: Vec2::new(0.0, -0.3),
            head_radius: 0.17,
            body: Limb::new((0.0, -0.18), (0.0, 0.24), 0.36),
            limbs: [leg.mirrored(), leg, arm.mirrored(), arm],
            small: [mirrored(ear), ear],
            large: [mirrored(wing), wing],
        }
    }

    /// Side on, facing left, blowing: the raised arms become the spout and
    /// the ears its spray, one leg the flipper and one wing the tail, and
    /// the other leg and wing go into the body.
    fn whale() -> Self {
        let hidden = std::array::from_fn(|i| {
            let angle = i as f32 * std::f32::consts::TAU / 9.0;
            Vec2::new(-0.1 + 0.05 * angle.cos(), 0.06 + 0.05 * angle.sin())
        });
        Self {
            head: Vec2::new(-0.42, 0.04),
            head_radius: 0.36,
            body: Limb::new((-0.3, 0.08), (0.26, 0.06), 0.6),
            limbs: [
                Limb::new((-0.2, 0.28), (0.04, 0.46), 0.13),
                Limb::new((0.2, 0.05), (0.3, 0.05), 0.1),
                Limb::new((-0.42, -0.3), (-0.5, -0.66), 0.07),
                Limb::new((-0.4, -0.3), (-0.32, -0.66), 0.07),
            ],
            small: [
                points([(-0.5, -0.7), (-0.7, -0.66), (-0.68, -0.5)]),
                points([(-0.32, -0.7), (-0.12, -0.66), (-0.14, -0.5)]),
            ],
            large: [
                hidden,
                points([
                    (0.2, -0.24),
                    (0.6, -0.12),
                    (0.8, -0.34),
                    (0.84, -0.22),
                    (0.76, 0.0),
                    (0.84, 0.22),
                    (0.8, 0.34),
                    (0.6, 0.14),
                    (0.2, 0.34),
                ]),
            ],
        }
    }

    /// `t` of the way from this figure to `to`, every point moving straight
    /// from the one to the other.
    fn toward(&self, to: &Self, t: f32) -> Self {
        let at = |a: Vec2, b: Vec2| a + (b - a) * t;
        let size = |a: f32, b: f32| a + (b - a) * t;
        let limb = |a: Limb, b: Limb| Limb {
            from: at(a.from, b.from),
            to: at(a.to, b.to),
            width: size(a.width, b.width),
        };
        Self {
            head: at(self.head, to.head),
            head_radius: size(self.head_radius, to.head_radius),
            body: limb(self.body, to.body),
            limbs: std::array::from_fn(|i| limb(self.limbs[i], to.limbs[i])),
            small: std::array::from_fn(|i| {
                std::array::from_fn(|k| at(self.small[i][k], to.small[i][k]))
            }),
            large: std::array::from_fn(|i| {
                std::array::from_fn(|k| at(self.large[i][k], to.large[i][k]))
            }),
        }
    }

    fn covers(&self, p: Vec2) -> bool {
        (p - self.head).length() <= self.head_radius
            || self.body.covers(p)
            || self.limbs.iter().any(|limb| limb.covers(p))
            || self.small.iter().any(|corners| inside(corners, p))
            || self.large.iter().any(|corners| inside(corners, p))
    }

    /// In white on clear, `size` pixels square, each pixel sampled four by
    /// four so the edges come out smooth.
    fn image(&self, size: usize) -> ColorImage {
        const SAMPLES: usize = 4;
        let pixels = (0..size * size)
            .map(|i| {
                let corner = Vec2::new((i % size) as f32, (i / size) as f32);
                let hits = (0..SAMPLES * SAMPLES)
                    .filter(|k| {
                        let within = Vec2::new((k % SAMPLES) as f32, (k / SAMPLES) as f32)
                            + Vec2::splat(0.5);
                        let p = (corner + within / SAMPLES as f32) / size as f32 * 2.0
                            - Vec2::splat(1.0);
                        self.covers(p)
                    })
                    .count();
                Color32::from_white_alpha((hits * 255 / (SAMPLES * SAMPLES)) as u8)
            })
            .collect();
        ColorImage::new([size, size], pixels)
    }
}

/// The small grey figure beside each frequency slider, for who could hear
/// that frequency: a person, turning bit by bit into an animal that hears
/// what people cannot, the further past human hearing the slider goes.
/// Each step of the way is drawn once and kept.
#[derive(Default)]
pub struct Listeners(HashMap<(Animal, u16, usize), TextureHandle>);

impl Listeners {
    const STEPS: f32 = 64.0;

    pub fn show(&mut self, ui: &mut Ui, hz: f32, animal: Animal) -> Response {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
        let step = (animal.change(hz) * Self::STEPS).round() as u16;
        let size = (rect.width() * ui.ctx().pixels_per_point()).ceil() as usize;
        let texture = self.0.entry((animal, step, size)).or_insert_with(|| {
            let t = f32::from(step) / Self::STEPS;
            let image = Figure::person().toward(&animal.figure(), t).image(size);
            ui.ctx().load_texture(
                format!("listener-{animal:?}-{step}-{size}"),
                image,
                TextureOptions::LINEAR,
            )
        });
        let whole = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        ui.painter().image(texture.id(), rect, whole, LISTENER);
        response
    }
}

const METER_FLOOR: f32 = -60.0;
const METER_SCALE: [f32; 8] = [-60.0, -48.0, -36.0, -24.0, -12.0, -6.0, -3.0, 0.0];
/// How long the highest peak stays put before it falls, and how fast.
const HOLD_SECONDS: f64 = 1.5;
const DECAY_DB_PER_SECOND: f32 = 15.0;
/// Numbers change this often at most, so they can be read.
const NUMBERS_EVERY: f64 = 0.15;
const METER_ROW: f32 = 12.0;
const METER_GAP: f32 = 4.0;
const METER_LABEL: f32 = 22.0;
const METER_NUMBERS: f32 = 116.0;

/// Per-channel level bars on the original's -60 to 0 dBFS scale: the peak
/// as a bar, the highest recent peak held as a line, and RMS and held peak
/// as numbers.
#[derive(Default)]
pub struct Meters {
    held: Vec<(f32, f64)>,
    numbers: Vec<String>,
    numbers_at: f64,
    last: f64,
}

fn meter_t(db: f32) -> f32 {
    ((db - METER_FLOOR) / -METER_FLOOR).clamp(0.0, 1.0)
}

fn level_text(db: f32) -> String {
    if db <= FLOOR_DB {
        "-∞".to_owned()
    } else {
        format!("{db:.1}")
    }
}

impl Meters {
    /// `levels` is `None` while nothing plays, which empties the meters.
    pub fn ui(&mut self, ui: &mut Ui, levels: Option<&[Level]>, labels: &[String]) {
        let channels = labels.len();
        let now = ui.input(|i| i.time);
        let dt = (now - self.last).max(0.0) as f32;
        self.last = now;
        if self.held.len() != channels || levels.is_none() {
            self.held = vec![(FLOOR_DB, now); channels];
        }
        if let Some(levels) = levels {
            for (held, level) in self.held.iter_mut().zip(levels) {
                if level.peak_db >= held.0 {
                    *held = (level.peak_db, now);
                } else if now - held.1 > HOLD_SECONDS {
                    held.0 = (held.0 - DECAY_DB_PER_SECOND * dt).max(FLOOR_DB);
                }
            }
        }
        if self.numbers.len() != channels || now - self.numbers_at >= NUMBERS_EVERY {
            self.numbers_at = now;
            self.numbers = (0..channels)
                .map(|c| {
                    let rms = levels.and_then(|l| l.get(c)).map_or(FLOOR_DB, |l| l.rms_db);
                    format!("{} / {}", level_text(rms), level_text(self.held[c].0))
                })
                .collect();
        }

        let height = channels as f32 * (METER_ROW + METER_GAP) + 18.0;
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
        let painter = ui.painter_at(rect);
        let track_x = rect.left() + METER_LABEL + 6.0..=rect.right() - METER_NUMBERS - 8.0;
        let track_width = track_x.end() - track_x.start();
        let x_of = |db: f32| track_x.start() + meter_t(db) * track_width;
        for (c, label) in labels.iter().enumerate() {
            let y = rect.top() + 2.0 + c as f32 * (METER_ROW + METER_GAP);
            let track = Rect::from_x_y_ranges(track_x.clone(), y..=y + METER_ROW);
            painter.text(
                Pos2::new(rect.left() + METER_LABEL, track.center().y),
                Align2::RIGHT_CENTER,
                label,
                FontId::monospace(11.0),
                AXIS,
            );
            painter.rect_filled(track, 2.0, METER_BACK);
            if let Some(level) = levels.and_then(|l| l.get(c)) {
                let bands: [(f32, f32, Color32); 3] = [
                    (METER_FLOOR, -10.0, METER_LOW),
                    (-10.0, -3.0, METER_MID),
                    (-3.0, 0.0, METER_HIGH),
                ];
                for (from, to, color) in bands {
                    let (a, b) = (x_of(from), x_of(to.min(level.peak_db)));
                    if b > a {
                        painter.rect_filled(
                            Rect::from_x_y_ranges(a..=b, track.y_range()),
                            0.0,
                            color,
                        );
                    }
                }
                if level.peak_db >= -0.1 {
                    painter.rect_stroke(
                        track,
                        2.0,
                        Stroke::new(1.0, METER_HIGH),
                        StrokeKind::Outside,
                    );
                }
            }
            let held = self.held[c].0;
            if held > METER_FLOOR {
                painter.vline(
                    x_of(held),
                    track.y_range(),
                    Stroke::new(2.0, Color32::WHITE),
                );
            }
            painter.text(
                Pos2::new(rect.right(), track.center().y),
                Align2::RIGHT_CENTER,
                &self.numbers[c],
                FontId::monospace(11.0),
                ELAPSED,
            );
        }
        let y = rect.bottom() - 14.0;
        for db in METER_SCALE {
            painter.text(
                Pos2::new(x_of(db), y),
                Align2::CENTER_TOP,
                format!("{db:.0}"),
                FontId::monospace(9.0),
                AXIS,
            );
        }
        painter.text(
            Pos2::new(rect.right(), y),
            Align2::RIGHT_TOP,
            "RMS / Peak",
            FontId::proportional(10.0),
            AXIS,
        );
    }
}

pub struct Curve<'a> {
    pub name: String,
    pub color: Color32,
    /// dBFS per bin, from 0 Hz to Nyquist.
    pub levels: &'a [f32],
}

/// Level against log frequency, `top` to `bottom` dB, with a legend once
/// there is more than one curve.
pub fn spectrum(
    painter: &Painter,
    plot: Rect,
    curves: &[Curve<'_>],
    nyquist: f32,
    (lo, hi): (f32, f32),
    (top, bottom): (f32, f32),
) {
    let x_of = |f: f32| plot.left() + freq_t(f, lo, hi, true) * plot.width();
    let y_of = |db: f32| plot.top() + ((top - db) / (top - bottom)).clamp(0.0, 1.0) * plot.height();
    for f in freq_ticks(lo, hi, true, plot.width(), 34.0) {
        let x = x_of(f);
        painter.vline(x, plot.y_range(), Stroke::new(1.0, GRID));
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
        painter.hline(plot.x_range(), y, Stroke::new(1.0, GRID));
        painter.text(
            Pos2::new(plot.left() - 6.0, y),
            Align2::RIGHT_CENTER,
            format!("{db:.0}"),
            FontId::monospace(10.0),
            AXIS,
        );
        db -= 20.0;
    }
    for curve in curves.iter().filter(|c| c.levels.len() > 1) {
        let bin_hz = nyquist / (curve.levels.len() - 1) as f32;
        let points: Vec<Pos2> = curve
            .levels
            .iter()
            .enumerate()
            .map(|(k, &level)| (k as f32 * bin_hz, level))
            .filter(|(f, _)| (lo..=hi).contains(f))
            .map(|(f, level)| Pos2::new(x_of(f), y_of(level)))
            .collect();
        painter.add(Shape::line(points, Stroke::new(1.2, curve.color)));
    }
    if curves.len() > 1 {
        let mut y = plot.top() + 6.0;
        for curve in curves {
            let galley =
                painter.layout_no_wrap(curve.name.clone(), FontId::proportional(11.0), curve.color);
            let (size, x) = (galley.size(), plot.right() - 6.0 - galley.size().x);
            painter.hline(
                x - 18.0..=x - 4.0,
                y + size.y / 2.0,
                Stroke::new(2.0, curve.color),
            );
            painter.galley(Pos2::new(x, y), galley, curve.color);
            y += size.y + 2.0;
        }
    }
}

/// A section of the Metadata view that folds away, starting `open` or not,
/// and then as it was last left.
fn section(ui: &mut Ui, title: &str, open: bool, rows: impl FnOnce(&mut Ui)) {
    egui::CollapsingHeader::new(RichText::new(title).strong().size(14.0))
        .default_open(open)
        .show(ui, rows);
}

/// A label, and beside it whatever `value` adds, wrapping at the edge.
fn row(ui: &mut Ui, label: &str, value: impl FnOnce(&mut Ui)) {
    let width = (ui.available_width() * 0.38).clamp(80.0, 180.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(width);
            ui.add(Label::new(RichText::new(label).color(AXIS)).wrap());
        });
        value(ui);
    });
}

fn filled<'a>(mut values: impl Iterator<Item = &'a String>) -> bool {
    values.any(|v| !v.trim().is_empty())
}

fn field(ui: &mut Ui, value: &mut String, limit: Option<usize>, multiline: bool, locked: bool) {
    let mut edit = if multiline {
        TextEdit::multiline(value).desired_rows(2)
    } else {
        TextEdit::singleline(value)
    }
    .desired_width(f32::INFINITY);
    if let Some(limit) = limit {
        edit = edit.char_limit(limit);
    }
    ui.add_enabled(!locked, edit);
}

/// The Metadata view. The File rows are as read; for a WAV every field it
/// can hold is editable, and locked while a save runs.
pub fn metadata(ui: &mut Ui, details: &Details, edits: Option<&mut Edits>, locked: bool) {
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            // Without edits, everything is shown as read; with them, only
            // the file's own facts, the rest being the fields below. The
            // facts start folded, as the header already has most of them,
            // and so does a section with nothing in it yet.
            let shown = if edits.is_some() {
                &details.sections[..details.sections.len().min(1)]
            } else {
                &details.sections[..]
            };
            for (title, rows) in shown {
                section(ui, title, title != "File", |ui| {
                    for (label, value) in rows {
                        row(ui, label, |ui| {
                            ui.add(Label::new(value.as_str()).selectable(true).wrap());
                        });
                    }
                });
            }
            let Some(edits) = edits else { return };
            let bext = filled(edits.bext.iter().chain([&edits.start, &edits.coding_history]));
            section(ui, "Broadcast WAV", bext, |ui| {
                for (i, (label, bytes)) in BEXT_FIELDS.iter().enumerate() {
                    row(ui, label, |ui| {
                        field(ui, &mut edits.bext[i], Some(bytes.len()), i == 0, locked)
                    });
                }
                row(ui, "Start", |ui| {
                    let edit = TextEdit::singleline(&mut edits.start)
                        .hint_text("hh:mm:ss.sss")
                        .desired_width(f32::INFINITY);
                    ui.add_enabled(!locked, edit).on_hover_text(
                        "Time of day at the first sample, which the timeline shows under elapsed time",
                    );
                });
                row(ui, "Coding history", |ui| {
                    field(ui, &mut edits.coding_history, None, true, locked)
                });
            });
            let info = filled(edits.info.iter().map(|(_, v)| v));
            section(ui, "RIFF INFO", info, |ui| {
                for (id, value) in &mut edits.info {
                    row(ui, &meta::info_name(id), |ui| {
                        field(ui, value, None, false, locked)
                    });
                }
            });
            if !edits.ixml.is_empty() {
                section(ui, "iXML", true, |ui| {
                    for (label, value) in &mut edits.ixml {
                        row(ui, label, |ui| field(ui, value, None, false, locked));
                    }
                });
            }
        });
}

pub enum MarkerAction {
    Seek(usize),
    Remove(u32),
}

/// The Markers view: each marker's time, which goes there, its name, and a
/// button that removes it. The name box of `name`, a marker just made,
/// takes the keyboard.
pub fn marker_list(
    ui: &mut Ui,
    markers: &mut [Marker],
    rate: f64,
    locked: bool,
    name: Option<u32>,
) -> Option<MarkerAction> {
    let mut action = None;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            for (i, m) in markers.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new((i + 1).to_string()).monospace().color(MARK));
                    let at = m.frame as f64 / rate;
                    let when = if m.length > 0 {
                        let end = (m.frame + m.length) as f64 / rate;
                        format!("{} to {}", clock_fine(at), clock_fine(end))
                    } else {
                        clock_fine(at)
                    };
                    // Only the name boxes take the keyboard, so Tab goes from
                    // name to name and nothing but a click removes a marker.
                    let link = RichText::new(when)
                        .monospace()
                        .color(ui.visuals().hyperlink_color);
                    let go = ui.add(Label::new(link).selectable(false).sense(Sense::CLICK));
                    if go.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                    if go.on_hover_text("Go there").clicked() {
                        action = Some(MarkerAction::Seek(m.frame));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let remove =
                            ui.add_enabled(!locked, Button::new("✖").small().sense(Sense::CLICK));
                        if remove.on_hover_text("Remove").clicked() {
                            action = Some(MarkerAction::Remove(m.id));
                        }
                        let edit = TextEdit::singleline(&mut m.label)
                            .hint_text("name")
                            .desired_width(ui.available_width());
                        let edit = ui.add_enabled(!locked, edit);
                        if name == Some(m.id) {
                            edit.request_focus();
                            edit.scroll_to_me(Some(Align::Center));
                        }
                    });
                });
            }
        });
    action
}

pub fn hz(f: f32) -> String {
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

/// A frequency as a slider shows it.
pub fn hz_field(f: f64) -> String {
    if f >= 10_000.0 {
        format!("{:.1} kHz", f / 1000.0)
    } else if f >= 1000.0 {
        format!("{:.2} kHz", f / 1000.0)
    } else {
        format!("{f:.0} Hz")
    }
}

/// A frequency as typed: `12000`, `12k`, `12.5 kHz`, `440 Hz`, `1,5k`.
pub fn parse_hz(text: &str) -> Option<f64> {
    let text = text.trim().to_lowercase().replace(',', ".");
    let text = text.strip_suffix("hz").unwrap_or(&text).trim_end();
    let (number, scale) = match text.strip_suffix('k') {
        Some(number) => (number, 1000.0),
        None => (text, 1.0),
    };
    let value: f64 = number.trim().parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value * scale)
}

/// Elapsed time as `m:ss`, or `h:mm:ss` from an hour on.
pub fn clock(seconds: f64) -> String {
    clock_with(seconds, 0)
}

pub fn clock_fine(seconds: f64) -> String {
    clock_with(seconds, 2)
}

fn clock_with(seconds: f64, decimals: usize) -> String {
    let scale = 10u64.pow(decimals as u32);
    let ticks = (seconds.max(0.0) * scale as f64).round() as u64;
    let whole = ticks / scale;
    let (h, m, s) = (whole / 3600, whole / 60 % 60, whole % 60);
    let mut text = if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    };
    if decimals > 0 {
        text.push_str(&format!(".{:0decimals$}", ticks % scale));
    }
    text
}

/// Time of day, wrapping past midnight.
pub fn wall(seconds: f64) -> String {
    wall_with(seconds, 0)
}

fn wall_with(seconds: f64, decimals: usize) -> String {
    let scale = 10u64.pow(decimals as u32);
    let ticks = (seconds.rem_euclid(86_400.0) * scale as f64).round() as u64 % (86_400 * scale);
    let whole = ticks / scale;
    let mut text = format!(
        "{:02}:{:02}:{:02}",
        whole / 3600,
        whole / 60 % 60,
        whole % 60
    );
    if decimals > 0 {
        text.push_str(&format!(".{:0decimals$}", ticks % scale));
    }
    text
}

/// The smallest tick spacing, in seconds, that leaves room for each label.
fn time_step(seconds: f64, width: f32) -> f64 {
    const STEPS: [f64; 24] = [
        0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0,
        60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 14_400.0,
    ];
    let fits = f64::from(width / 90.0).max(1.0);
    STEPS
        .into_iter()
        .find(|step| seconds / step <= fits)
        .unwrap_or(28_800.0)
}

/// Decimals a label needs to tell ticks `step` seconds apart.
fn decimals(step: f64) -> usize {
    match step {
        s if s >= 1.0 => 0,
        s if s >= 0.1 => 1,
        s if s >= 0.01 => 2,
        _ => 3,
    }
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

/// Tick frequencies along an axis `length` long. A log axis gets 1, 2 and
/// 5 per decade, or only the decades where `room` per label would not fit.
fn freq_ticks(lo: f32, hi: f32, log: bool, length: f32, room: f32) -> Vec<f32> {
    if !log {
        let step = nice_step((hi - lo) / (length / 55.0).max(2.0));
        let mut ticks = Vec::new();
        let mut f = (lo / step).ceil() * step;
        while f <= hi {
            ticks.push(f);
            f += step;
        }
        return ticks;
    }
    let decades = |multiples: &[f32]| {
        let mut ticks = Vec::new();
        let mut decade = 10f32.powf(lo.log10().floor());
        while decade <= hi {
            ticks.extend(
                multiples
                    .iter()
                    .map(|m| decade * m)
                    .filter(|f| (lo..=hi).contains(f)),
            );
            decade *= 10.0;
        }
        ticks
    };
    let ticks = decades(&[1.0, 2.0, 5.0]);
    if ticks.len() as f32 * room > length {
        decades(&[1.0])
    } else {
        ticks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_meter_holds_its_peak_then_lets_it_fall() {
        let ctx = egui::Context::default();
        let mut meters = Meters::default();
        let labels = ["1".to_owned()];
        let at = |time: f64, peak_db: f32, meters: &mut Meters| {
            let level = [Level {
                rms_db: peak_db - 3.0,
                peak_db,
            }];
            let input = egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| meters.ui(ui, Some(&level), &labels));
            out.textures_delta.clear();
            meters.held[0].0
        };
        assert_eq!(at(0.0, -6.0, &mut meters), -6.0);
        assert_eq!(meters.numbers[0], "-9.0 / -6.0");
        // Quieter now, but inside the hold time the peak stays put.
        assert_eq!(at(1.0, -20.0, &mut meters), -6.0);
        // Past it, the held peak falls at 15 dB a second.
        assert_eq!(at(1.6, -30.0, &mut meters), -6.0 - 15.0 * 0.6);
        let input = egui::RawInput {
            time: Some(2.0),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| meters.ui(ui, None, &labels));
        out.textures_delta.clear();
        assert_eq!(meters.held[0].0, FLOOR_DB, "stopping empties the meters");
    }

    #[test]
    fn the_figure_is_a_person_in_human_hearing_and_an_animal_past_it() {
        use Animal::{Bat, Whale};
        assert_eq!((Bat.change(440.0), Bat.change(20_000.0)), (0.0, 0.0));
        assert!((Bat.change(44_721.0) - 0.5).abs() < 0.01);
        assert_eq!(Bat.change(192_000.0), 1.0);
        assert_eq!((Whale.change(20.0), Whale.change(24_000.0)), (0.0, 0.0));
        assert!((Whale.change(14.142) - 0.5).abs() < 0.01);
        assert_eq!((Whale.change(10.0), Whale.change(0.0)), (1.0, 1.0));
        // Standing, the figure is taller than it is wide; turned into
        // either animal, wider.
        let extent = |figure: Figure| {
            let image = figure.image(36);
            let solid: Vec<usize> = (0..image.pixels.len())
                .filter(|&i| image.pixels[i].a() > 127)
                .collect();
            let span = |of: fn(usize) -> usize| {
                let values = solid.iter().map(|&i| of(i));
                values.clone().max().unwrap() - values.min().unwrap()
            };
            (span(|i| i % 36), span(|i| i / 36))
        };
        let person = extent(Figure::person());
        assert!(person.1 > person.0, "{person:?}");
        for animal in [Bat, Whale] {
            let (wide, tall) = extent(Figure::person().toward(&animal.figure(), 1.0));
            assert!(wide > tall, "{animal:?} {wide} by {tall}");
        }
    }

    #[test]
    fn frequency_labels_read_naturally() {
        assert_eq!(hz(440.0), "440");
        assert_eq!(hz(2_500.0), "2.5k");
        assert_eq!(hz(192_000.0), "192k");
        assert_eq!(hz_field(12_345.0), "12.3 kHz");
        assert_eq!(hz_field(440.0), "440 Hz");
    }

    #[test]
    fn typed_frequencies_take_k_and_hz() {
        assert_eq!(parse_hz("12000"), Some(12_000.0));
        assert_eq!(parse_hz("12k"), Some(12_000.0));
        assert_eq!(parse_hz(" 12.5 kHz "), Some(12_500.0));
        assert_eq!(parse_hz("440 Hz"), Some(440.0));
        assert_eq!(parse_hz("1,5k"), Some(1_500.0));
        assert_eq!(parse_hz("loud"), None);
        assert_eq!(parse_hz("-3"), None);
    }

    #[test]
    fn clocks_format_elapsed_and_wall_time() {
        assert_eq!(clock(364.0), "6:04");
        assert_eq!(clock(3_725.0), "1:02:05");
        assert_eq!(clock_fine(59.994), "0:59.99");
        assert_eq!(wall(61_454.0 + 86_400.0), "17:04:14");
        assert_eq!(wall_with(86_399.999_6, 3), "00:00:00.000");
    }

    #[test]
    fn zoomed_in_ticks_step_below_a_second_with_decimals_to_match() {
        let step = time_step(0.5, 900.0);
        assert_eq!(step, 0.05);
        assert_eq!(clock_with(12.35, decimals(step)), "0:12.35");
        assert_eq!(decimals(time_step(3_600.0, 900.0)), 0);
    }

    #[test]
    fn linear_ticks_step_nicely() {
        let expected: Vec<f32> = (0..=4).map(|i| i as f32 * 5_000.0).collect();
        assert_eq!(freq_ticks(0.0, 24_000.0, false, 275.0, 16.0), expected);
    }

    #[test]
    fn a_short_log_axis_keeps_only_the_decades() {
        assert_eq!(
            freq_ticks(20.0, 20_000.0, true, 60.0, 16.0),
            [100.0, 1_000.0, 10_000.0]
        );
        assert_eq!(freq_ticks(20.0, 20_000.0, true, 600.0, 16.0).len(), 10);
    }
}
