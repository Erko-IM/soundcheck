//! Short-time Fourier analysis: the whole-file matrix behind the
//! spectrogram, and single spectra for the spectrum panel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{Color32, ColorImage};
use rayon::prelude::*;
use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

pub const FFT_SIZES: [usize; 5] = [512, 1024, 2048, 4096, 8192];

/// Columns in the whole-file view: wider than any screen, so the texture is
/// only ever scaled down.
const MAX_COLUMNS: usize = 2048;
const ROWS: usize = 1024;
const SILENCE_DB: f32 = -200.0;

pub struct Analysis {
    pub fft: usize,
    pub columns: usize,
    pub bins: usize,
    /// `columns` spectra of `bins` values each, in dBFS.
    pub db: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// Level drawn at full brightness, in dBFS.
    pub top_db: f32,
    /// How far below `top_db` still gets colour.
    pub range_db: f32,
    pub f_min: f32,
    pub f_max: f32,
    pub log: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            top_db: 0.0,
            range_db: 90.0,
            f_min: 0.0,
            f_max: 24_000.0,
            log: false,
        }
    }
}

struct Stft {
    plan: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    /// Scales power so a full-scale sine reads 0 dBFS.
    gain: f32,
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl Stft {
    fn new(size: usize) -> Self {
        let plan = RealFftPlanner::<f32>::new().plan_fft_forward(size);
        // Periodic Hann, the spectral-analysis form rather than the filter one.
        let window: Vec<f32> = (0..size)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / size as f32).cos())
            .collect();
        let amplitude = 2.0 / window.iter().sum::<f32>();
        Self {
            input: plan.make_input_vec(),
            output: plan.make_output_vec(),
            scratch: plan.make_scratch_vec(),
            plan,
            window,
            gain: amplitude * amplitude,
        }
    }

    /// Adds the power spectrum of the window centred on `centre` into `acc`,
    /// zero-padding past either end of the signal.
    fn add_power(&mut self, signal: &[f32], centre: usize, acc: &mut [f32]) {
        let half = self.window.len() / 2;
        for (i, (slot, w)) in self.input.iter_mut().zip(&self.window).enumerate() {
            *slot = (centre + i)
                .checked_sub(half)
                .and_then(|at| signal.get(at))
                .map_or(0.0, |s| s * w);
        }
        self.plan
            .process_with_scratch(&mut self.input, &mut self.output, &mut self.scratch)
            .expect("buffers come from the plan, so their lengths match");
        for (a, c) in acc.iter_mut().zip(&self.output) {
            *a += c.norm_sqr() * self.gain;
        }
    }
}

fn to_db(power: f32) -> f32 {
    (10.0 * power.log10()).max(SILENCE_DB)
}

/// The whole-file matrix, or `None` if `cancel` was set before it finished.
pub fn analyse(signal: &[f32], fft: usize, cancel: &AtomicBool) -> Option<Analysis> {
    let bins = fft / 2 + 1;
    let columns = signal.len().div_ceil(fft / 4).clamp(1, MAX_COLUMNS);
    let span = signal.len() as f64 / columns as f64;
    // Windows at most half a window apart, so every sample passes near the
    // middle of one rather than only through faded edges, and each column
    // keeps the loudest: on a long recording one column spans seconds, and
    // sampling or averaging it would hide a short call inside.
    let windows = ((2.0 * span / fft as f64).ceil() as usize).max(1);
    let mut db = vec![0.0; columns * bins];
    db.par_chunks_mut(bins).enumerate().for_each_init(
        || (Stft::new(fft), vec![0.0f32; bins]),
        |(stft, power), (column, out)| {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            for w in 0..windows {
                power.fill(0.0);
                let at = (column as f64 + (w as f64 + 0.5) / windows as f64) * span;
                stft.add_power(signal, at as usize, power);
                for (o, p) in out.iter_mut().zip(power.iter()) {
                    *o = f32::max(*o, *p);
                }
            }
            for v in out.iter_mut() {
                *v = to_db(*v);
            }
        },
    );
    (!cancel.load(Ordering::Relaxed)).then_some(Analysis {
        fft,
        columns,
        bins,
        db,
    })
}

/// Power-averaged over a few overlapping windows around `centre`: smooth
/// enough to read, still local to the cursor.
pub fn spectrum_at(signal: &[f32], centre: usize, fft: usize) -> Vec<f32> {
    const WINDOWS: usize = 8;
    let hop = fft / 2;
    let mut stft = Stft::new(fft);
    let mut acc = vec![0.0; fft / 2 + 1];
    let first = centre.saturating_sub(hop * (WINDOWS - 1) / 2);
    for w in 0..WINDOWS {
        stft.add_power(signal, first + w * hop, &mut acc);
    }
    acc.into_iter().map(|p| to_db(p / WINDOWS as f32)).collect()
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

pub fn colorize(
    a: &Analysis,
    sample_rate: u32,
    view: &View,
    gradient: colorous::Gradient,
) -> ColorImage {
    let (lo, hi) = band(view, sample_rate, a.fft);
    let bin_hz = sample_rate as f32 / a.fft as f32;
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

    let mut pixels = vec![Color32::BLACK; a.columns * ROWS];
    pixels
        .par_chunks_mut(a.columns)
        .enumerate()
        .for_each(|(row, line)| {
            // Row 0 is the top edge, the highest frequency. Each row shows the
            // loudest bin it covers, so narrow tones survive being scaled down.
            let top = freq(1.0 - row as f32 / ROWS as f32);
            let bottom = freq(1.0 - (row + 1) as f32 / ROWS as f32);
            let first = ((bottom / bin_hz).floor() as usize).min(a.bins - 1);
            let last = ((top / bin_hz).ceil() as usize).clamp(first + 1, a.bins);
            for (column, px) in line.iter_mut().enumerate() {
                let spectrum = &a.db[column * a.bins..][first..last];
                let level = spectrum.iter().copied().fold(SILENCE_DB, f32::max);
                let t = ((level - view.top_db) / view.range_db + 1.0).clamp(0.0, 1.0);
                *px = lut[(t * 255.0) as usize];
            }
        });
    ColorImage::new([a.columns, ROWS], pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f64, rate: f64, seconds: f64) -> Vec<f32> {
        let n = (rate * seconds) as usize;
        (0..n)
            .map(|i| (std::f64::consts::TAU * frequency * i as f64 / rate).sin() as f32)
            .collect()
    }

    #[test]
    fn full_scale_sine_on_a_bin_reads_zero_dbfs() {
        let (fft, rate, bin) = (2048, 48_000.0, 100);
        let signal = sine(bin as f64 * rate / fft as f64, rate, 1.0);
        let s = spectrum_at(&signal, signal.len() / 2, fft);
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

    fn loudest(a: &Analysis, column: usize) -> f32 {
        a.db[column * a.bins..(column + 1) * a.bins]
            .iter()
            .copied()
            .fold(f32::MIN, f32::max)
    }

    #[test]
    fn columns_follow_the_signal_in_time() {
        let mut signal = vec![0.0; 48_000];
        signal.extend(sine(1_000.0, 48_000.0, 1.0));
        let a = analyse(&signal, 1024, &AtomicBool::new(false)).unwrap();
        assert!(loudest(&a, 0) < -150.0);
        assert!(loudest(&a, a.columns - 1) > -10.0);
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
        let a = analyse(&signal, fft, &AtomicBool::new(false)).unwrap();
        let (centred, between) = (loudest(&a, 500), loudest(&a, 1000));
        assert!(
            between > centred - 7.0,
            "{between} dB between windows against {centred} dB centred"
        );
    }

    #[test]
    fn a_cancelled_analysis_returns_nothing() {
        let signal = sine(1_000.0, 48_000.0, 1.0);
        assert!(analyse(&signal, 1024, &AtomicBool::new(true)).is_none());
    }

    #[test]
    fn band_is_capped_at_nyquist_and_positive_on_log() {
        let view = View {
            f_min: 0.0,
            f_max: 1e9,
            log: true,
            ..View::default()
        };
        let (lo, hi) = band(&view, 384_000, 2048);
        assert_eq!(hi, 192_000.0);
        assert!(lo > 0.0);
    }
}
