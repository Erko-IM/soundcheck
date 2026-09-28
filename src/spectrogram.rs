//! Short-time Fourier analysis: spectrogram matrices for any range of a
//! file, computed while its samples stream past so memory stays flat
//! whatever the length, and single spectra for the spectrum panel.

use std::ops::Range;
use std::sync::Arc;

use eframe::egui::{Color32, ColorImage};
use rayon::prelude::*;
use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};

pub const FFT_SIZES: [usize; 5] = [512, 1024, 2048, 4096, 8192];

/// Columns an analysis for the screen gets: a column a pixel across the
/// spectrogram, this many before it has been drawn, and at most the most.
pub const DEFAULT_COLUMNS: usize = 2048;
pub const MAX_COLUMNS: usize = 4096;
/// The most levels an analysis keeps, of every plane together: 256 MB, which
/// a wide one of many channels at the largest FFT would pass.
const MOST_LEVELS: usize = 1 << 26;
pub const SILENCE_DB: f32 = -200.0;

/// Which signals are analysed: the channels averaged, each channel on its
/// own, or one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Channels {
    Mix,
    All,
    One(usize),
}

impl Channels {
    /// What this means for a file with `count` channels: All on a mono
    /// file, or a channel the file does not have, falls back to the mix.
    pub fn targets(self, count: usize) -> Vec<Target> {
        match self {
            Self::All if count > 1 => (0..count).map(Target::Channel).collect(),
            Self::One(c) if c < count => vec![Target::Channel(c)],
            _ => vec![Target::Mix],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Mix,
    Channel(usize),
}

impl Target {
    pub fn sample(self, frame: &[f32]) -> f32 {
        match self {
            Self::Mix => frame.iter().sum::<f32>() / frame.len() as f32,
            Self::Channel(c) => frame[c],
        }
    }

    /// One value per frame of the interleaved `frames` of `ch` channels.
    /// Mono and a stereo mix are written out, so the compiler takes many
    /// frames at a time.
    fn fill(self, out: &mut [f32], frames: &[f32], ch: usize) {
        match (self, ch) {
            (_, 1) => out.copy_from_slice(frames),
            (Self::Mix, 2) => {
                for (o, f) in out.iter_mut().zip(frames.as_chunks::<2>().0) {
                    *o = (f[0] + f[1]) / 2.0;
                }
            }
            _ => {
                for (o, f) in out.iter_mut().zip(frames.chunks_exact(ch)) {
                    *o = self.sample(f);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spec {
    pub fft: usize,
    pub channels: Channels,
}

pub struct Analysis {
    pub spec: Spec,
    /// The frames analysed.
    pub range: Range<usize>,
    pub columns: usize,
    pub bins: usize,
    pub targets: Vec<Target>,
    /// One matrix per target: `columns` spectra of `bins` values, in dBFS.
    pub planes: Vec<Vec<f32>>,
    /// Per channel, the lowest and highest sample in each column.
    pub envelope: Vec<Vec<[f32; 2]>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// Added to every level before colouring, in dB.
    pub brightness: f32,
    /// How many dB below full brightness still get colour.
    pub contrast: f32,
    pub f_min: f32,
    pub f_max: f32,
    pub log: bool,
}

/// The FFT plan and window for one size, shared by every thread.
#[derive(Clone)]
struct Kernel {
    plan: Arc<dyn RealToComplex<f32>>,
    window: Arc<[f32]>,
    /// Scales power so a full-scale sine reads 0 dBFS.
    gain: f32,
}

/// One thread's working buffers for a [`Kernel`].
struct Scratch {
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    fft: Vec<Complex<f32>>,
}

impl Kernel {
    fn new(size: usize) -> Self {
        let plan = RealFftPlanner::<f32>::new().plan_fft_forward(size);
        // Periodic Hann, the spectral-analysis form rather than the filter one.
        let window: Arc<[f32]> = (0..size)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / size as f32).cos())
            .collect();
        let amplitude = 2.0 / window.iter().sum::<f32>();
        Self {
            plan,
            window,
            gain: amplitude * amplitude,
        }
    }

    fn scratch(&self) -> Scratch {
        Scratch {
            input: self.plan.make_input_vec(),
            output: self.plan.make_output_vec(),
            fft: self.plan.make_scratch_vec(),
        }
    }

    /// Folds the power spectrum of the window centred on `signal[centre]`
    /// into `acc` with `fold`. Anything outside `signal` is silence.
    fn power(
        &self,
        s: &mut Scratch,
        signal: &[f32],
        centre: usize,
        acc: &mut [f32],
        fold: fn(&mut f32, f32),
    ) {
        let (size, half) = (self.window.len(), self.window.len() / 2);
        match centre
            .checked_sub(half)
            .filter(|from| from + size <= signal.len())
        {
            // Inside the signal all the way, as nearly every window is.
            Some(from) => {
                let inside = s.input.iter_mut().zip(&signal[from..from + size]);
                for ((slot, x), w) in inside.zip(self.window.iter()) {
                    *slot = x * w;
                }
            }
            None => {
                for (i, (slot, w)) in s.input.iter_mut().zip(self.window.iter()).enumerate() {
                    *slot = (centre + i)
                        .checked_sub(half)
                        .and_then(|at| signal.get(at))
                        .map_or(0.0, |x| x * w);
                }
            }
        }
        self.plan
            .process_with_scratch(&mut s.input, &mut s.output, &mut s.fft)
            .expect("buffers come from the plan, so their lengths match");
        for (a, c) in acc.iter_mut().zip(&s.output) {
            fold(a, c.norm_sqr() * self.gain);
        }
    }
}

fn keep_max(acc: &mut f32, p: f32) {
    *acc = acc.max(p);
}

fn add(acc: &mut f32, p: f32) {
    *acc += p;
}

fn to_db(power: f32) -> f32 {
    (10.0 * power.log10()).max(SILENCE_DB)
}

/// How an analysis of a range lays out, as [`Plan::new`] makes it: its
/// columns, the windows each takes the loudest of, and the FFT. Threads
/// work out a stretch of its columns each with [`Plan::work`].
pub struct Plan {
    spec: Spec,
    range: Range<usize>,
    channels: usize,
    targets: Vec<Target>,
    columns: usize,
    bins: usize,
    layout: Layout,
    kernel: Kernel,
}

/// Room for a thread to work out columns in, used again from part to part.
pub struct Room {
    signal: Vec<f32>,
    scratch: Scratch,
}

/// Some of a stretch of columns, read and worked out at once, as
/// [`Plan::parts`] makes it.
pub struct Part {
    /// Its windows, numbered through the whole analysis.
    windows: Range<usize>,
    /// The frames whose lowest and highest samples it finds.
    pub own: Range<usize>,
    /// The frames it reads: its own, and its windows'.
    pub reads: Range<usize>,
}

/// Where each column's windows sit.
#[derive(Clone, Copy)]
struct Layout {
    start: usize,
    span: f64,
    windows: usize,
    half: usize,
    frames: usize,
}

impl Layout {
    fn centre(self, column: usize, window: usize) -> usize {
        (self.start as f64
            + (column as f64 + (window as f64 + 0.5) / self.windows as f64) * self.span)
            as usize
    }

    /// The centre of window `window`, numbered through the whole analysis.
    fn centre_of(self, window: usize) -> usize {
        self.centre(window / self.windows, window % self.windows)
    }
}

/// The columns an analysis of `len` frames gets when `wanted` are asked
/// for: no more than there are frames, nor than [`MOST_LEVELS`] holds.
pub fn columns(spec: Spec, len: usize, channels: usize, wanted: usize) -> usize {
    let levels = (spec.fft / 2 + 1) * spec.channels.targets(channels).len();
    wanted.min(MOST_LEVELS / levels).clamp(1, len.max(1))
}

impl Plan {
    /// An analysis of `range` about `columns` wide, as [`columns`] allows,
    /// of a file `frames` long: windows reaching past either end of it read
    /// silence there.
    pub fn new(
        spec: Spec,
        range: Range<usize>,
        frames: usize,
        channels: usize,
        columns: usize,
    ) -> Self {
        let len = range.len().max(1);
        let columns = self::columns(spec, len, channels, columns);
        let span = len as f64 / columns as f64;
        // Enough windows that every sample passes near the middle of one
        // rather than only through faded edges, and each column keeps the
        // loudest: on a long recording one column spans seconds, and
        // sampling or averaging it would hide a short call inside.
        let windows = ((2.0 * span / spec.fft as f64).ceil() as usize).max(1);
        Self {
            spec,
            channels,
            columns,
            bins: spec.fft / 2 + 1,
            targets: spec.channels.targets(channels),
            layout: Layout {
                start: range.start,
                span,
                windows,
                half: spec.fft / 2,
                frames,
            },
            kernel: Kernel::new(spec.fft),
            range,
        }
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    /// Frames of the range each column spans.
    pub fn span(&self) -> f64 {
        self.layout.span
    }

    /// Where the frames of column `column` start; the column past the last
    /// starts where the range ends.
    pub fn begins(&self, column: usize) -> usize {
        let at = (self.range.start as f64 + column as f64 * self.layout.span).ceil() as usize;
        at.min(self.range.end)
    }

    /// `columns` in parts of about `most` frames or fewer each, in order, so
    /// what a thread holds at once stays the same however long a column is.
    pub fn parts(&self, columns: Range<usize>, most: usize) -> impl Iterator<Item = Part> {
        let layout = self.layout;
        let (first, windows) = (
            columns.start * layout.windows,
            columns.len() * layout.windows,
        );
        let (from, to) = (self.begins(columns.start), self.begins(columns.end));
        let count = (to - from).div_ceil(most.max(1)).clamp(1, windows);
        let at = move |part: usize| first + windows * part / count;
        // A part's own frames run from its first window's centre to the next
        // part's.
        let edge = move |part: usize| match part {
            0 => from,
            part if part == count => to,
            part => layout.centre_of(at(part)).clamp(from, to),
        };
        (0..count).map(move |part| {
            let own = edge(part)..edge(part + 1);
            let lowest = layout.centre_of(at(part)).saturating_sub(layout.half);
            let highest = layout.centre_of(at(part + 1) - 1) + layout.half;
            Part {
                windows: at(part)..at(part + 1),
                reads: own.start.min(lowest)..own.end.max(highest).min(layout.frames),
                own,
            }
        })
    }

    /// Room for parts that read up to `frames` frames.
    pub fn room(&self, frames: usize) -> Room {
        Room {
            signal: Vec::with_capacity(frames),
            scratch: self.kernel.scratch(),
        }
    }

    /// The analysis with every column silent, for [`Plan::work`] to fill in.
    pub fn blank(&self) -> Analysis {
        Analysis {
            spec: self.spec,
            range: self.range.clone(),
            columns: self.columns,
            bins: self.bins,
            targets: self.targets.clone(),
            planes: vec![vec![SILENCE_DB; self.columns * self.bins]; self.targets.len()],
            envelope: vec![vec![[f32::INFINITY, f32::NEG_INFINITY]; self.columns]; self.channels],
        }
    }

    /// Works out `part` of `columns`, in their order, from `samples`,
    /// interleaved frames from `first` on holding what the part reads: in
    /// each target's stretch of `planes`, each column the loudest of its
    /// windows, in dBFS once they are all in; in each channel's stretch of
    /// `envelope`, the lowest and highest sample of the part's own frames.
    #[allow(clippy::too_many_arguments)]
    pub fn work(
        &self,
        columns: Range<usize>,
        part: &Part,
        first: usize,
        samples: &[f32],
        planes: &mut [&mut [f32]],
        envelope: &mut [&mut [[f32; 2]]],
        room: &mut Room,
    ) {
        let (ch, bins, layout) = (self.channels, self.bins, self.layout);
        let each = layout.windows;
        room.signal.resize(samples.len() / ch, 0.0);
        for (target, plane) in self.targets.iter().zip(planes.iter_mut()) {
            target.fill(&mut room.signal, samples, ch);
            for window in part.windows.clone() {
                let (column, w) = (window / each, window % each);
                let out = &mut plane[(column - columns.start) * bins..][..bins];
                if w == 0 {
                    out.fill(0.0);
                }
                let centre = layout.centre(column, w) - first;
                self.kernel
                    .power(&mut room.scratch, &room.signal, centre, out, keep_max);
                if w + 1 == each {
                    out.iter_mut().for_each(|v| *v = to_db(*v));
                }
            }
        }
        let (start, span) = (self.range.start, layout.span);
        let to = part.own.end.min(first + samples.len() / ch);
        let mut at = part.own.start.max(first);
        while at < to {
            let column =
                (((at - start) as f64 / span) as usize).clamp(columns.start, columns.end - 1);
            let next = (start as f64 + (column + 1) as f64 * span).ceil() as usize;
            let end = next.max(at + 1).min(to);
            let found = extremes(&samples[(at - first) * ch..(end - first) * ch], ch);
            for (channel, e) in envelope.iter_mut().zip(found) {
                let slot = &mut channel[column - columns.start];
                *slot = [slot[0].min(e[0]), slot[1].max(e[1])];
            }
            at = end;
        }
    }
}

impl Analysis {
    /// Once every column is worked out: a column no frame fell in shows
    /// nothing.
    pub fn finish(mut self) -> Self {
        for column in self.envelope.iter_mut().flatten() {
            if column[0] > column[1] {
                *column = [0.0, 0.0];
            }
        }
        self
    }
}

/// Each channel's lowest and highest sample in the interleaved frames of
/// `part`. They are compared a row of whole frames at a time, a row wide
/// enough that the compiler compares many samples at once.
fn extremes(part: &[f32], ch: usize) -> Vec<[f32; 2]> {
    let width = ch * 64usize.div_ceil(ch);
    let mut lo = vec![f32::INFINITY; width];
    let mut hi = vec![f32::NEG_INFINITY; width];
    let rows = part.chunks_exact(width);
    let rest = rows.remainder();
    for row in rows {
        for ((l, h), &s) in lo.iter_mut().zip(&mut hi).zip(row) {
            *l = l.min(s);
            *h = h.max(s);
        }
    }
    for ((l, h), &s) in lo.iter_mut().zip(&mut hi).zip(rest) {
        *l = l.min(s);
        *h = h.max(s);
    }
    (0..ch)
        .map(|c| {
            let lanes = (c..width).step_by(ch);
            [
                lanes.clone().map(|i| lo[i]).fold(f32::INFINITY, f32::min),
                lanes.map(|i| hi[i]).fold(f32::NEG_INFINITY, f32::max),
            ]
        })
        .collect()
}

/// Frames either side of a centre that [`spectrum_around`] reads.
pub fn probe_extent(fft: usize) -> usize {
    fft / 2 * (PROBE_WINDOWS - 1) / 2 + fft / 2 + 1
}

const PROBE_WINDOWS: usize = 8;

/// Power-averaged over a few overlapping windows around `signal[centre]`:
/// smooth enough to read, still local to the cursor.
pub fn spectrum_around(signal: &[f32], centre: usize, fft: usize) -> Vec<f32> {
    let hop = fft / 2;
    let kernel = Kernel::new(fft);
    let mut scratch = kernel.scratch();
    let mut acc = vec![0.0; fft / 2 + 1];
    let first = centre.saturating_sub(hop * (PROBE_WINDOWS - 1) / 2);
    for w in 0..PROBE_WINDOWS {
        kernel.power(&mut scratch, signal, first + w * hop, &mut acc, add);
    }
    acc.into_iter()
        .map(|p| to_db(p / PROBE_WINDOWS as f32))
        .collect()
}

/// The highest the band can be set to, for seeing that a file holds
/// nothing past half its sample rate.
pub const HIGHEST_HZ: f32 = 1_000_000.0;

/// The frequency band drawn: as far up as asked, past what the file can
/// hold too, and never down to 0 Hz on a log axis.
pub fn band(view: &View, sample_rate: u32, fft: usize) -> (f32, f32) {
    let nyquist = sample_rate as f32 / 2.0;
    let bin_hz = sample_rate as f32 / fft as f32;
    let hi = view.f_max.clamp(bin_hz * 2.0, HIGHEST_HZ.max(nyquist));
    let floor = if view.log { bin_hz } else { 0.0 };
    (view.f_min.max(floor).min(hi - bin_hz), hi)
}

/// The part of [`band`] the file holds: up to half its sample rate, and
/// nothing when the band starts above that.
pub fn held(view: &View, sample_rate: u32, fft: usize) -> Option<(f32, f32)> {
    let (lo, hi) = band(view, sample_rate, fft);
    let nyquist = sample_rate as f32 / 2.0;
    (lo < nyquist).then(|| (lo, hi.min(nyquist)))
}

/// One plane of `a` as an image `rows` tall, top row the highest frequency
/// the file holds within the band.
pub fn colorize(
    a: &Analysis,
    plane: usize,
    sample_rate: u32,
    view: &View,
    gradient: colorous::Gradient,
    rows: usize,
) -> ColorImage {
    let Some((lo, hi)) = held(view, sample_rate, a.spec.fft) else {
        return ColorImage::new([1, 1], vec![Color32::BLACK]);
    };
    let bin_hz = sample_rate as f32 / a.spec.fft as f32;
    let freq = |t: f32| {
        if view.log {
            lo * (hi / lo).powf(t)
        } else {
            lo + (hi - lo) * t
        }
    };
    let lut: Vec<Color32> = (0..=255)
        .map(|i| {
            let c = gradient.eval_continuous(f64::from(i) / 255.0);
            Color32::from_rgb(c.r, c.g, c.b)
        })
        .collect();

    let db = &a.planes[plane];
    let mut pixels = vec![Color32::BLACK; a.columns * rows];
    pixels
        .par_chunks_mut(a.columns)
        .enumerate()
        .for_each(|(row, line)| {
            // A row a bin tall or more shows the loudest bin it covers, so
            // narrow tones survive being scaled down. A shorter one shows
            // the level between the two bins nearest its middle, so a band
            // drawn taller than its bins comes out smooth rather than in
            // steps.
            let top = freq(1.0 - row as f32 / rows as f32);
            let bottom = freq(1.0 - (row + 1) as f32 / rows as f32);
            let first = ((bottom / bin_hz).floor() as usize).min(a.bins - 1);
            let last = ((top / bin_hz).ceil() as usize).clamp(first + 1, a.bins);
            let middle = freq(1.0 - (row as f32 + 0.5) / rows as f32) / bin_hz;
            let below = (middle.floor().max(0.0) as usize).min(a.bins - 2);
            let between = (top - bottom < bin_hz).then(|| (middle - below as f32).clamp(0.0, 1.0));
            for (column, px) in line.iter_mut().enumerate() {
                let spectrum = &db[column * a.bins..][..a.bins];
                let level = match between {
                    Some(t) => spectrum[below] + (spectrum[below + 1] - spectrum[below]) * t,
                    None => spectrum[first..last]
                        .iter()
                        .copied()
                        .fold(SILENCE_DB, f32::max),
                };
                let t = ((level + view.brightness) / view.contrast + 1.0).clamp(0.0, 1.0);
                *px = lut[(t * 255.0) as usize];
            }
        });
    ColorImage::new([a.columns, rows], pixels)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sine(frequency: f64, rate: f64, seconds: f64) -> Vec<f32> {
        let n = (rate * seconds) as usize;
        (0..n)
            .map(|i| (std::f64::consts::TAU * frequency * i as f64 / rate).sin() as f32)
            .collect()
    }

    /// `samples` (interleaved over `channels`) analysed about `columns` wide
    /// in stretches of `per` columns, each in parts of about `most` frames
    /// from only the frames the part reads, as threads do.
    #[allow(clippy::too_many_arguments)]
    pub fn analyse_in(
        samples: &[f32],
        channels: usize,
        spec: Spec,
        range: Range<usize>,
        columns: usize,
        per: usize,
        most: usize,
    ) -> Analysis {
        let frames = samples.len() / channels;
        let plan = Plan::new(spec, range, frames, channels, columns);
        let mut analysis = plan.blank();
        let (bins, mut room) = (analysis.bins, plan.room(0));
        for first in (0..plan.columns()).step_by(per) {
            let stretch = first..(first + per).min(plan.columns());
            let mut planes: Vec<&mut [f32]> = analysis
                .planes
                .iter_mut()
                .map(|p| &mut p[stretch.start * bins..stretch.end * bins])
                .collect();
            let mut envelope: Vec<&mut [[f32; 2]]> = analysis
                .envelope
                .iter_mut()
                .map(|e| &mut e[stretch.clone()])
                .collect();
            for part in plan.parts(stretch.clone(), most) {
                let reads = part.reads.clone();
                let read = &samples[reads.start * channels..reads.end * channels];
                plan.work(
                    stretch.clone(),
                    &part,
                    reads.start,
                    read,
                    &mut planes,
                    &mut envelope,
                    &mut room,
                );
            }
        }
        analysis.finish()
    }

    fn analyse(
        samples: &[f32],
        channels: usize,
        spec: Spec,
        range: Range<usize>,
        per: usize,
    ) -> Analysis {
        analyse_in(
            samples,
            channels,
            spec,
            range,
            DEFAULT_COLUMNS,
            per,
            usize::MAX,
        )
    }

    fn mix(fft: usize) -> Spec {
        Spec {
            fft,
            channels: Channels::Mix,
        }
    }

    fn loudest(a: &Analysis, plane: usize, column: usize) -> f32 {
        a.planes[plane][column * a.bins..(column + 1) * a.bins]
            .iter()
            .copied()
            .fold(f32::MIN, f32::max)
    }

    #[test]
    fn full_scale_sine_on_a_bin_reads_zero_dbfs() {
        let (fft, rate, bin) = (2048, 48_000.0, 100);
        let signal = sine(bin as f64 * rate / fft as f64, rate, 1.0);
        let s = spectrum_around(&signal, signal.len() / 2, fft);
        let (peak, level) = s
            .iter()
            .copied()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        assert_eq!(peak, bin);
        assert!(level.abs() < 0.1, "peak read {level} dBFS");
        assert!(
            s[bin + 40] < -80.0,
            "leakage 40 bins away is {} dB",
            s[bin + 40]
        );
    }

    #[test]
    fn a_column_of_many_windows_reads_its_loudest_in_dbfs() {
        let (fft, rate, bin) = (1024, 48_000.0, 40);
        let half: Vec<f32> = sine(bin as f64 * rate / fft as f64, rate, 1.0)
            .iter()
            .map(|s| s / 2.0)
            .collect();
        let a = analyse_in(&half, 1, mix(fft), 0..half.len(), 8, usize::MAX, usize::MAX);
        for column in 0..8 {
            let level = a.planes[0][column * a.bins + bin];
            assert!(
                (level + 6.02).abs() < 0.1,
                "column {column} read {level} dBFS"
            );
        }
    }

    #[test]
    fn columns_follow_the_signal_in_time() {
        let mut signal = vec![0.0; 48_000];
        signal.extend(sine(1_000.0, 48_000.0, 1.0));
        let a = analyse(&signal, 1, mix(1024), 0..signal.len(), 10_000);
        assert!(loudest(&a, 0, 0) < -150.0);
        assert!(loudest(&a, 0, a.columns - 1) > -10.0);
    }

    #[test]
    fn the_result_does_not_depend_on_how_the_columns_are_shared_out() {
        let signal: Vec<f32> = (0u64..300_000)
            .map(|i| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5)
            .collect();
        // Columns a window wide, and columns many windows wide.
        for columns in [DEFAULT_COLUMNS, 16] {
            let at_once =
                |per, most| analyse_in(&signal, 1, mix(2048), 0..signal.len(), columns, per, most);
            let whole = at_once(usize::MAX, usize::MAX);
            for (per, most) in [
                (1, usize::MAX),
                (7, usize::MAX),
                (500, 1000),
                (3, 1),
                (1, 5000),
            ] {
                let shared = at_once(per, most);
                let how = format!("{columns} columns, {per} a stretch, {most} frames a part");
                assert_eq!(whole.planes, shared.planes, "{how}");
                assert_eq!(whole.envelope, shared.envelope, "{how}");
            }
        }
    }

    #[test]
    fn a_part_reads_about_as_much_as_it_is_given_however_long_a_column_is() {
        let frames = 100_000_000;
        let plan = Plan::new(mix(2048), 0..frames, frames, 2, 16);
        let parts: Vec<Part> = plan.parts(3..5, 50_000).collect();
        assert_eq!(parts[0].own.start, plan.begins(3));
        assert_eq!(parts[parts.len() - 1].own.end, plan.begins(5));
        for pair in parts.windows(2) {
            assert_eq!(pair[0].own.end, pair[1].own.start);
            assert_eq!(pair[0].windows.end, pair[1].windows.start);
        }
        assert!(parts.iter().all(|p| p.reads.len() < 50_000 + 2 * 2048));
    }

    #[test]
    fn a_click_between_two_windows_is_not_lost() {
        // 2048 columns of 1024 samples, analysed with 512-sample windows.
        let (fft, span) = (512, 1024);
        let mut signal = vec![0.0; 2048 * span];
        let burst = &sine(6_000.0, 48_000.0, 0.01)[..16];
        // Column 500 gets the click where a window is centred. Column 1000
        // gets it where two windows a whole window apart would both have
        // faded to nothing.
        for centre in [500 * span + span / 8, 1000 * span + span / 2] {
            signal[centre - 8..centre + 8].copy_from_slice(burst);
        }
        let a = analyse(&signal, 1, mix(fft), 0..signal.len(), 100_000);
        let (centred, between) = (loudest(&a, 0, 500), loudest(&a, 0, 1000));
        assert!(
            between > centred - 7.0,
            "{between} dB between windows against {centred} dB centred"
        );
    }

    #[test]
    fn channels_are_analysed_apart_or_together() {
        // Left: a tone. Right: silence. Long enough that every column
        // holds whole cycles.
        let tone = sine(3_000.0, 48_000.0, 4.0);
        let stereo: Vec<f32> = tone.iter().flat_map(|&s| [s, 0.0]).collect();
        let spec = |channels| Spec {
            fft: 1024,
            channels,
        };
        let all = analyse(&stereo, 2, spec(Channels::All), 0..tone.len(), 50_000);
        assert_eq!(all.targets, [Target::Channel(0), Target::Channel(1)]);
        assert!(loudest(&all, 0, 1000) > -1.0);
        assert!(loudest(&all, 1, 1000) < -150.0);
        let mixed = analyse(&stereo, 2, spec(Channels::Mix), 0..tone.len(), 50_000);
        // Averaged with a silent channel: half the amplitude, 6 dB down.
        assert!((loudest(&mixed, 0, 1000) + 6.0).abs() < 1.0);
        let right = analyse(&stereo, 2, spec(Channels::One(1)), 0..tone.len(), 50_000);
        assert_eq!(right.targets, [Target::Channel(1)]);
        assert_eq!(all.envelope[1], vec![[0.0, 0.0]; all.columns]);
        assert!(all.envelope[0].iter().all(|e| e[1] > 0.9 && e[0] < -0.9));
    }

    #[test]
    fn a_zoomed_range_reads_real_samples_past_its_edges() {
        let signal = sine(2_000.0, 48_000.0, 1.0);
        let range = 20_000..21_000;
        let a = analyse(&signal, 1, mix(4096), range.clone(), 1_234);
        assert_eq!(a.columns, 1_000);
        // Every column sees the tone at full strength, the first and last
        // included, because the windows read on past the range.
        for column in [0, 500, 999] {
            assert!(loudest(&a, 0, column) > -1.0, "column {column}");
        }
    }

    #[test]
    fn the_band_goes_past_what_the_file_holds_and_what_it_holds_stops_there() {
        let mut view = View {
            brightness: 0.0,
            contrast: 90.0,
            f_min: 0.0,
            f_max: 1e9,
            log: true,
        };
        let (lo, hi) = band(&view, 384_000, 2048);
        assert_eq!(hi, HIGHEST_HZ);
        assert!(lo > 0.0);
        assert_eq!(held(&view, 384_000, 2048), Some((lo, 192_000.0)));
        view.f_max = 60_000.0;
        assert_eq!(band(&view, 48_000, 2048).1, 60_000.0);
        assert_eq!(held(&view, 48_000, 2048).map(|b| b.1), Some(24_000.0));
        view.f_min = 30_000.0;
        assert_eq!(held(&view, 48_000, 2048), None);
    }

    #[test]
    fn a_band_drawn_taller_than_its_bins_is_smooth_rather_than_in_steps() {
        let signal = sine(3_000.0, 48_000.0, 0.5);
        let a = analyse(&signal, 1, mix(1024), 0..signal.len(), 100_000);
        // 2.5 to 3.5 kHz: about 21 bins of 47 Hz, over 400 rows.
        let view = View {
            brightness: 0.0,
            contrast: 90.0,
            f_min: 2_500.0,
            f_max: 3_500.0,
            log: false,
        };
        let image = colorize(&a, 0, 48_000, &view, colorous::VIRIDIS, 400);
        let column = a.columns / 2;
        let floor = image.pixels[column];
        let lit: Vec<Color32> = (0..400)
            .map(|row| image.pixels[row * a.columns + column])
            .filter(|&px| px != floor)
            .collect();
        // A bin is 19 rows tall here: in steps, each would be one colour.
        let longest = lit
            .chunk_by(|a, b| a == b)
            .map(<[Color32]>::len)
            .max()
            .unwrap_or(0);
        assert!(
            lit.len() > 40 && longest < 8,
            "{} lit rows, longest run {longest}",
            lit.len()
        );
        // Still brightest at the tone, halfway up.
        let brightest = (0..400)
            .max_by_key(|&row| image.pixels[row * a.columns + column].g())
            .unwrap();
        assert!(
            (190..=210).contains(&brightest),
            "brightest row {brightest}"
        );
    }

    #[test]
    fn past_what_the_file_holds_nothing_takes_up_rows() {
        let signal = sine(6_000.0, 48_000.0, 0.5);
        let a = analyse(&signal, 1, mix(1024), 0..signal.len(), 100_000);
        let view = |f_max| View {
            brightness: 0.0,
            contrast: 90.0,
            f_min: 0.0,
            f_max,
            log: false,
        };
        let at_limit = colorize(&a, 0, 48_000, &view(24_000.0), colorous::VIRIDIS, 400);
        let past = colorize(&a, 0, 48_000, &view(96_000.0), colorous::VIRIDIS, 400);
        assert_eq!(at_limit.size, [a.columns, 400]);
        assert!(at_limit.pixels == past.pixels);
    }
}
