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

/// Columns in one analysis: wider than any screen, so the texture is only
/// ever scaled down.
pub const MAX_COLUMNS: usize = 2048;
pub const SILENCE_DB: f32 = -200.0;
/// Frames a thread takes at a time when a target's signal is drawn out of
/// the interleaved samples.
const MIX_FRAMES: usize = 1 << 14;

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

/// Builds an [`Analysis`] of `range` from the file's samples, fed in order
/// through [`Analyzer::push`]. Only the samples still needed are kept.
pub struct Analyzer {
    spec: Spec,
    range: Range<usize>,
    /// Frames in the whole file; windows reaching past either end read
    /// silence there.
    frames: usize,
    channels: usize,
    targets: Vec<Target>,
    columns: usize,
    bins: usize,
    /// Frames per column.
    span: f64,
    /// Windows per column, at most half a window apart.
    windows: usize,
    kernel: Kernel,
    /// Per target, the samples from `buffer_start` on.
    buffers: Vec<Vec<f32>>,
    buffer_start: usize,
    next_column: usize,
    planes: Vec<Vec<f32>>,
    envelope: Vec<Vec<[f32; 2]>>,
}

/// Where one column's windows sit, copied out so worker threads need no
/// borrow of the analyzer.
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

    /// The first frame any window of `column` reads.
    fn needs_from(self, column: usize) -> usize {
        self.centre(column, 0).saturating_sub(self.half)
    }

    /// One past the last frame any window of `column` reads.
    fn needs_to(self, column: usize) -> usize {
        (self.centre(column, self.windows - 1) + self.half).min(self.frames)
    }
}

impl Analyzer {
    pub fn new(spec: Spec, range: Range<usize>, frames: usize, channels: usize) -> Self {
        let len = range.len().max(1);
        let columns = len.min(MAX_COLUMNS);
        let span = len as f64 / columns as f64;
        // Enough windows that every sample passes near the middle of one
        // rather than only through faded edges, and each column keeps the
        // loudest: on a long recording one column spans seconds, and
        // sampling or averaging it would hide a short call inside.
        let windows = ((2.0 * span / spec.fft as f64).ceil() as usize).max(1);
        let bins = spec.fft / 2 + 1;
        let targets = spec.channels.targets(channels);
        let mut analyzer = Self {
            spec,
            frames,
            channels,
            columns,
            bins,
            span,
            windows,
            kernel: Kernel::new(spec.fft),
            buffers: vec![Vec::new(); targets.len()],
            buffer_start: 0,
            next_column: 0,
            planes: vec![vec![SILENCE_DB; columns * bins]; targets.len()],
            envelope: vec![vec![[f32::INFINITY, f32::NEG_INFINITY]; columns]; channels],
            targets,
            range,
        };
        analyzer.buffer_start = analyzer.wanted().start;
        analyzer
    }

    fn layout(&self) -> Layout {
        Layout {
            start: self.range.start,
            span: self.span,
            windows: self.windows,
            half: self.spec.fft / 2,
            frames: self.frames,
        }
    }

    /// The frames to push: the range, plus half a window either side.
    pub fn wanted(&self) -> Range<usize> {
        let half = self.spec.fft / 2;
        self.range.start.saturating_sub(half)..(self.range.end + half).min(self.frames)
    }

    /// Interleaved frames starting at `first`, which must be where the last
    /// push ended, or `wanted().start` for the first.
    pub fn push(&mut self, first: usize, samples: &[f32]) {
        let ch = self.channels;
        for (target, buffer) in self.targets.iter().zip(&mut self.buffers) {
            let start = buffer.len();
            buffer.resize(start + samples.len() / ch, 0.0);
            buffer[start..]
                .par_chunks_mut(MIX_FRAMES)
                .zip(samples.par_chunks(MIX_FRAMES * ch))
                .for_each(|(out, frames)| target.fill(out, frames, ch));
        }
        self.add_envelope(first, samples);

        let have = first + samples.len() / ch;
        let layout = self.layout();
        let ready = if have >= self.wanted().end {
            self.columns
        } else {
            (self.next_column..self.columns)
                .find(|&c| layout.needs_to(c) > have)
                .unwrap_or(self.columns)
        };
        if ready > self.next_column {
            self.run(self.next_column..ready);
            self.next_column = ready;
        }
        let keep = if self.next_column < self.columns {
            layout.needs_from(self.next_column)
        } else {
            have
        };
        let drop = keep
            .saturating_sub(self.buffer_start)
            .min(self.buffers[0].len());
        for buffer in &mut self.buffers {
            buffer.drain(..drop);
        }
        self.buffer_start += drop;
    }

    pub fn finish(mut self) -> Analysis {
        if self.next_column < self.columns {
            self.run(self.next_column..self.columns);
        }
        for column in self.envelope.iter_mut().flatten() {
            if column[0] > column[1] {
                *column = [0.0, 0.0];
            }
        }
        Analysis {
            spec: self.spec,
            range: self.range,
            columns: self.columns,
            bins: self.bins,
            targets: self.targets,
            planes: self.planes,
            envelope: self.envelope,
        }
    }

    fn run(&mut self, columns: Range<usize>) {
        let (bins, layout, kernel) = (self.bins, self.layout(), &self.kernel);
        let offset = self.buffer_start;
        // Few, long columns (a long file) share out their windows instead.
        let by_column = columns.len() >= rayon::current_num_threads();
        for (plane, buffer) in self.planes.iter_mut().zip(&self.buffers) {
            let column_spectrum = |scratch: &mut Scratch, column: usize, out: &mut [f32]| {
                out.fill(0.0);
                for w in 0..layout.windows {
                    let centre = layout.centre(column, w) - offset;
                    kernel.power(scratch, buffer, centre, out, keep_max);
                }
                out.iter_mut().for_each(|v| *v = to_db(*v));
            };
            let out = &mut plane[columns.start * bins..columns.end * bins];
            if by_column {
                out.par_chunks_mut(bins).enumerate().for_each_init(
                    || kernel.scratch(),
                    |scratch, (i, out)| column_spectrum(scratch, columns.start + i, out),
                );
                continue;
            }
            for (i, out) in out.chunks_mut(bins).enumerate() {
                let column = columns.start + i;
                let spectrum = (0..layout.windows)
                    .into_par_iter()
                    .fold(
                        || (kernel.scratch(), vec![0.0f32; bins]),
                        |(mut scratch, mut acc), w| {
                            let centre = layout.centre(column, w) - offset;
                            kernel.power(&mut scratch, buffer, centre, &mut acc, keep_max);
                            (scratch, acc)
                        },
                    )
                    .map(|(_, acc)| acc)
                    .reduce(
                        || vec![0.0; bins],
                        |mut a, b| {
                            a.iter_mut().zip(&b).for_each(|(x, y)| *x = x.max(*y));
                            a
                        },
                    );
                for (o, p) in out.iter_mut().zip(spectrum) {
                    *o = to_db(p);
                }
            }
        }
    }

    fn add_envelope(&mut self, first: usize, samples: &[f32]) {
        let ch = self.channels;
        let from = first.max(self.range.start);
        let to = (first + samples.len() / ch).min(self.range.end);
        let (start, span, columns) = (self.range.start, self.span, self.columns);
        // Stretches that stay inside one column, so they fill in parallel.
        let mut stretches = Vec::new();
        let mut at = from;
        while at < to {
            let column = (((at - start) as f64 / span) as usize).min(columns - 1);
            let next = (start as f64 + (column + 1) as f64 * span).ceil() as usize;
            let end = next.max(at + 1).min(to).min(at + (1 << 16));
            stretches.push((column, at..end));
            at = end;
        }
        let found: Vec<(usize, Vec<[f32; 2]>)> = stretches
            .into_par_iter()
            .map(|(column, frames)| {
                let part = &samples[(frames.start - first) * ch..(frames.end - first) * ch];
                (column, extremes(part, ch))
            })
            .collect();
        for (column, extremes) in found {
            for (channel, e) in self.envelope.iter_mut().zip(extremes) {
                let slot = &mut channel[column];
                *slot = [slot[0].min(e[0]), slot[1].max(e[1])];
            }
        }
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

/// The frequency band actually drawn: never past what the file can hold,
/// and never down to 0 Hz on a log axis.
pub fn band(view: &View, sample_rate: u32, fft: usize) -> (f32, f32) {
    let nyquist = sample_rate as f32 / 2.0;
    let bin_hz = sample_rate as f32 / fft as f32;
    let hi = view.f_max.clamp(bin_hz * 2.0, nyquist);
    let floor = if view.log { bin_hz } else { 0.0 };
    (view.f_min.max(floor).min(hi - bin_hz), hi)
}

/// One plane of `a` as an image `rows` tall, top row the highest frequency.
pub fn colorize(
    a: &Analysis,
    plane: usize,
    sample_rate: u32,
    view: &View,
    gradient: colorous::Gradient,
    rows: usize,
) -> ColorImage {
    let (lo, hi) = band(view, sample_rate, a.spec.fft);
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
            // Each row shows the loudest bin it covers, so narrow tones
            // survive being scaled down.
            let top = freq(1.0 - row as f32 / rows as f32);
            let bottom = freq(1.0 - (row + 1) as f32 / rows as f32);
            let first = ((bottom / bin_hz).floor() as usize).min(a.bins - 1);
            let last = ((top / bin_hz).ceil() as usize).clamp(first + 1, a.bins);
            for (column, px) in line.iter_mut().enumerate() {
                let spectrum = &db[column * a.bins..][first..last];
                let level = spectrum.iter().copied().fold(SILENCE_DB, f32::max);
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

    /// `samples` (interleaved over `channels`) run through an analyzer in
    /// pieces of `piece` frames, as a file read would.
    fn analyse(
        samples: &[f32],
        channels: usize,
        spec: Spec,
        range: Range<usize>,
        piece: usize,
    ) -> Analysis {
        let frames = samples.len() / channels;
        let mut a = Analyzer::new(spec, range, frames, channels);
        let wanted = a.wanted();
        let mut at = wanted.start;
        while at < wanted.end {
            let end = at.saturating_add(piece).min(wanted.end);
            a.push(at, &samples[at * channels..end * channels]);
            at = end;
        }
        a.finish()
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
    fn columns_follow_the_signal_in_time() {
        let mut signal = vec![0.0; 48_000];
        signal.extend(sine(1_000.0, 48_000.0, 1.0));
        let a = analyse(&signal, 1, mix(1024), 0..signal.len(), 10_000);
        assert!(loudest(&a, 0, 0) < -150.0);
        assert!(loudest(&a, 0, a.columns - 1) > -10.0);
    }

    #[test]
    fn the_result_does_not_depend_on_how_the_file_is_read() {
        let signal: Vec<f32> = (0u64..300_000)
            .map(|i| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5)
            .collect();
        let whole = analyse(&signal, 1, mix(2048), 0..signal.len(), usize::MAX);
        let pieces = analyse(&signal, 1, mix(2048), 0..signal.len(), 777);
        assert_eq!(whole.planes, pieces.planes);
        assert_eq!(whole.envelope, pieces.envelope);
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
    fn band_is_capped_at_nyquist_and_positive_on_log() {
        let view = View {
            brightness: 0.0,
            contrast: 90.0,
            f_min: 0.0,
            f_max: 1e9,
            log: true,
        };
        let (lo, hi) = band(&view, 384_000, 2048);
        assert_eq!(hi, 192_000.0);
        assert!(lo > 0.0);
    }
}
