//! The spectrum at the cursor, worked out on a thread of its own: it reads
//! the file around the cursor, which for a compressed file means decoding,
//! and the window must never wait for that.

use std::sync::{Arc, Condvar, Mutex, PoisonError};

use eframe::egui;

use crate::audio::{Reader, Source};
use crate::spectrogram::{self, Channels, Target};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Request {
    pub frame: usize,
    pub fft: usize,
    pub channels: Channels,
}

pub struct Spectrum {
    pub request: Request,
    /// dBFS per bin, one curve per target.
    pub curves: Vec<(Target, Vec<f32>)>,
}

#[derive(Default)]
struct State {
    wanted: Option<Request>,
    done: Option<Result<Spectrum, String>>,
    quit: bool,
}

type Shared = (Mutex<State>, Condvar);

pub struct Probe {
    shared: Arc<Shared>,
}

impl Probe {
    pub fn new(source: Source, channels: usize, frames: usize, ctx: egui::Context) -> Self {
        let shared = Arc::new(Shared::default());
        std::thread::spawn({
            let shared = Arc::clone(&shared);
            move || work(&source, channels, frames, &shared, &ctx)
        });
        Self { shared }
    }

    /// Replaces whatever was asked before and not yet started.
    pub fn ask(&self, request: Request) {
        let (state, wake) = &*self.shared;
        state.lock().unwrap_or_else(PoisonError::into_inner).wanted = Some(request);
        wake.notify_one();
    }

    /// The newest spectrum finished since the last call.
    pub fn take(&self) -> Option<Result<Spectrum, String>> {
        let (state, _) = &*self.shared;
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .done
            .take()
    }
}

impl Drop for Probe {
    /// The worker finishes the spectrum it is on, then stops; the window
    /// does not wait for it.
    fn drop(&mut self) {
        let (state, wake) = &*self.shared;
        state.lock().unwrap_or_else(PoisonError::into_inner).quit = true;
        wake.notify_one();
    }
}

fn work(source: &Source, channels: usize, frames: usize, shared: &Shared, ctx: &egui::Context) {
    let (state, wake) = shared;
    let mut reader = None;
    let mut samples = Vec::new();
    loop {
        let request = {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if state.quit {
                    return;
                }
                if let Some(request) = state.wanted.take() {
                    break request;
                }
                state = wake.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let result = match &mut reader {
            Some(reader) => spectrum(reader, channels, frames, request, &mut samples),
            None => Reader::open(source, channels).and_then(|opened| {
                spectrum(
                    reader.insert(opened),
                    channels,
                    frames,
                    request,
                    &mut samples,
                )
            }),
        };
        state.lock().unwrap_or_else(PoisonError::into_inner).done = Some(result);
        ctx.request_repaint();
    }
}

fn spectrum(
    reader: &mut Reader,
    channels: usize,
    frames: usize,
    request: Request,
    samples: &mut Vec<f32>,
) -> Result<Spectrum, String> {
    let extent = spectrogram::probe_extent(request.fft);
    let centre = request.frame.min(frames);
    let first = centre.saturating_sub(extent);
    let end = (centre + extent).min(frames);
    samples.resize((end - first) * channels, 0.0);
    reader.read(first, samples)?;
    let curves = request
        .channels
        .targets(channels)
        .into_iter()
        .map(|target| {
            let signal: Vec<f32> = samples
                .chunks_exact(channels)
                .map(|f| target.sample(f))
                .collect();
            let curve = spectrogram::spectrum_around(&signal, centre - first, request.fft);
            (target, curve)
        })
        .collect();
    Ok(Spectrum { request, curves })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::temp_file;
    use crate::wav;

    #[test]
    fn each_channel_gets_its_own_curve_and_the_latest_request_wins() {
        // Left: a loud 3 kHz tone. Right: silence.
        let tone = crate::spectrogram::tests::sine(3_000.0, 48_000.0, 1.0);
        let samples: Vec<i16> = tone
            .iter()
            .flat_map(|&s| [(s * 30_000.0) as i16, 0])
            .collect();
        let path = temp_file(
            "probe.wav",
            &wav::tests::build_channels(false, 2, &[], &samples),
        );
        let file = std::fs::read(&path).unwrap();
        let w = wav::parse(&mut std::io::Cursor::new(&file)).unwrap();
        let source = Source::Pcm {
            path: path.clone(),
            data: w.data.clone(),
            kind: w.kind,
        };
        let probe = Probe::new(source, 2, w.frames(), egui::Context::default());
        let ask = |frame| Request {
            frame,
            fft: 2048,
            channels: Channels::All,
        };
        probe.ask(ask(1_000));
        probe.ask(ask(24_000));
        // The first request may be answered or skipped; the last one never
        // goes unanswered.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let spectrum = loop {
            if let Some(result) = probe.take() {
                let spectrum = result.unwrap();
                if spectrum.request.frame == 24_000 {
                    break spectrum;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the last request was never answered"
            );
            std::thread::yield_now();
        };
        std::fs::remove_file(&path).unwrap();
        let loudest = |c: usize| {
            spectrum.curves[c]
                .1
                .iter()
                .copied()
                .fold(f32::MIN, f32::max)
        };
        assert_eq!(spectrum.curves.len(), 2);
        assert!(loudest(0) > -3.0, "left read {}", loudest(0));
        assert!(loudest(1) < -150.0, "right read {}", loudest(1));
    }
}
