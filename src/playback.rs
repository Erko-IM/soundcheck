//! Playback. A feeder thread reads the file, resamples it to the output
//! device's rate at the chosen speed, and keeps a lock-free ring buffer
//! topped up; the audio callback only drains that buffer, so it never waits
//! on the disk or allocates. While paused the device is stopped and the
//! feeder sleeps until it is told something, so a paused player costs
//! nothing.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use crate::audio::{Info, Reader, Source};

/// Speed multipliers offered. Pitch moves with speed, as on tape.
pub const SPEEDS: [u32; 4] = [1, 2, 4, 8];

const CHUNK: usize = 1024;
/// How often the feeder tops the ring up while playing. The ring holds a
/// second, so a slow card reader has plenty of slack.
const TOP_UP: Duration = Duration::from_millis(100);
/// Positions are packed with the number of the seek they belong to.
const FRAME_BITS: u32 = 48;
const FINISHED: u32 = 1 << 16;

struct Shared {
    playing: AtomicBool,
    /// The seek number and source frame now leaving the speakers, packed.
    position: AtomicU64,
    /// `FINISHED` with the seek number, once that seek has played to the end.
    finished: AtomicU32,
    /// Linear gain as `f32` bits, applied on the way out so a change is
    /// heard at once rather than after the second of audio already queued.
    gain: AtomicU32,
    failure: Mutex<Option<String>>,
}

impl Shared {
    fn fail(&self, why: String) {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(why);
    }
}

fn pack(seek: u16, frame: f64) -> u64 {
    (u64::from(seek) << FRAME_BITS) | (frame as u64 & ((1 << FRAME_BITS) - 1))
}

fn unpack(packed: u64) -> (u16, usize) {
    let frame = packed & ((1 << FRAME_BITS) - 1);
    ((packed >> FRAME_BITS) as u16, frame as usize)
}

/// The audio callback's end of the ring. The feeder takes it to restart
/// playback somewhere else; the callback only ever `try_lock`s it, and
/// plays a moment of silence rather than wait.
struct Playhead {
    ring: rtrb::Consumer<f32>,
    seek: u16,
    /// Source frame of the oldest sample in the ring.
    position: f64,
    /// Source frames per output frame.
    step: f64,
    /// The frames being repeated: `position` wraps where the rendered audio
    /// did.
    looping: Option<Range<f64>>,
}

impl Playhead {
    fn advance(&mut self) {
        self.position += self.step;
        if let Some(r) = &self.looping
            && self.position >= r.end
        {
            self.position -= r.end - r.start;
        }
    }
}

struct Restart {
    frame: usize,
    speed: u32,
    seek: u16,
    looping: Option<Range<usize>>,
}

enum Command {
    Restart(Restart),
    /// Playback resumed, so the ring needs topping up again.
    Wake,
    Quit,
}

pub struct Player {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
    feeder: Option<JoinHandle<()>>,
    stream: cpal::Stream,
    frames: usize,
    speed: Cell<u32>,
    looping: RefCell<Option<Range<usize>>>,
    /// Seeks sent so far, and where the last one went: until the feeder has
    /// acted on it, playback is wherever it was last sent.
    seeks: Cell<u16>,
    target: Cell<usize>,
}

impl Player {
    pub fn new(source: &Source, info: &Info, start: usize, speed: u32) -> Result<Self, String> {
        let (sample_rate, frames) = (info.sample_rate, info.frames);
        let channels = usize::from(info.channels);
        let device = cpal::default_host()
            .default_output_device()
            .ok_or("no audio output device")?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("audio output: {e}"))?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let device_rate = config.sample_rate;

        let shared = Arc::new(Shared {
            playing: AtomicBool::new(false),
            position: AtomicU64::new(pack(0, start as f64)),
            finished: AtomicU32::new(0),
            gain: AtomicU32::new(1.0f32.to_bits()),
            failure: Mutex::new(None),
        });
        // A second of stereo.
        let (producer, consumer) = rtrb::RingBuffer::new(2 * device_rate as usize);
        let playhead = Arc::new(Mutex::new(Playhead {
            ring: consumer,
            seek: 0,
            position: start as f64,
            step: step(sample_rate, speed, device_rate),
            looping: None,
        }));
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, &playhead, &shared),
            SampleFormat::I16 => build::<i16>(&device, config, &playhead, &shared),
            SampleFormat::I32 => build::<i32>(&device, config, &playhead, &shared),
            SampleFormat::U16 => build::<u16>(&device, config, &playhead, &shared),
            other => Err(format!("unsupported audio output format {other}")),
        }?;

        let (commands, inbox) = mpsc::channel();
        let feeder = std::thread::spawn({
            let (shared, source) = (Arc::clone(&shared), source.clone());
            move || {
                // Opened here rather than on the UI thread, which a slow
                // disk would stall.
                let renderer = Reader::open(&source, channels).and_then(|reader| {
                    let input = Box::new(Stereo {
                        reader,
                        channels,
                        scratch: Vec::new(),
                    });
                    Renderer::new(input, frames, sample_rate, device_rate, speed, start)
                });
                match renderer {
                    Ok(renderer) => feed(renderer, producer, &playhead, &inbox, &shared),
                    Err(e) => shared.fail(e),
                }
            }
        });
        Ok(Self {
            shared,
            commands,
            feeder: Some(feeder),
            stream,
            frames,
            speed: Cell::new(speed),
            looping: RefCell::new(None),
            seeks: Cell::new(0),
            target: Cell::new(start),
        })
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Acquire)
    }

    /// Plays from the current position, or from the start once at the end.
    pub fn play(&self) {
        if self.finished() {
            self.seek(0);
        }
        self.shared.playing.store(true, Ordering::Release);
        if let Err(e) = self.stream.play() {
            self.shared.fail(format!("cannot start audio output: {e}"));
        }
        let _ = self.commands.send(Command::Wake);
    }

    /// Stops the device as well. A backend that cannot pause plays silence
    /// instead.
    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Release);
        let _ = self.stream.pause();
    }

    /// The last seek has played to the end of the file.
    pub fn finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire) == FINISHED | u32::from(self.seeks.get())
    }

    pub fn position(&self) -> usize {
        if self.finished() {
            return self.frames;
        }
        let (seek, frame) = unpack(self.shared.position.load(Ordering::Acquire));
        if seek == self.seeks.get() {
            frame.min(self.frames)
        } else {
            self.target.get()
        }
    }

    pub fn failure(&self) -> Option<String> {
        self.shared
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn seek(&self, frame: usize) {
        self.restart(frame, self.speed.get());
    }

    pub fn set_speed(&self, speed: u32) {
        self.speed.set(speed);
        self.restart(self.position(), speed);
    }

    /// Output gain in dB, applied to what is already queued too.
    pub fn set_gain(&self, db: f32) {
        let gain = 10f32.powf(db / 20.0);
        self.shared.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Repeats `range` from its start, or stops repeating where playback
    /// now is.
    pub fn set_loop(&self, range: Option<Range<usize>>) {
        let range = range
            .map(|r| r.start.min(self.frames)..r.end.min(self.frames))
            .filter(|r| !r.is_empty());
        let from = range.as_ref().map_or_else(|| self.position(), |r| r.start);
        self.looping.replace(range);
        self.seek(from);
    }

    fn restart(&self, frame: usize, speed: u32) {
        let seek = self.seeks.get().wrapping_add(1);
        let looping = self.looping.borrow().clone();
        let frame = match &looping {
            Some(r) if frame >= r.end => r.start,
            _ => frame.min(self.frames),
        };
        self.seeks.set(seek);
        self.target.set(frame);
        // The feeder only stops on `Quit`, which only `Drop` sends, so this
        // always reaches it.
        let _ = self.commands.send(Command::Restart(Restart {
            frame,
            speed,
            seek,
            looping,
        }));
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Quit);
        if let Some(feeder) = self.feeder.take() {
            let _ = feeder.join();
        }
    }
}

fn step(sample_rate: u32, speed: u32, device_rate: u32) -> f64 {
    f64::from(sample_rate) * f64::from(speed) / f64::from(device_rate)
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    playhead: &Arc<Mutex<Playhead>>,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let (playhead, state, errors) = (Arc::clone(playhead), Arc::clone(shared), Arc::clone(shared));
    device
        .build_output_stream::<T, _, _>(
            config,
            move |out: &mut [T], _| {
                let head = if state.playing.load(Ordering::Acquire) {
                    playhead.try_lock().ok()
                } else {
                    None
                };
                let Some(mut head) = head else {
                    out.fill(T::EQUILIBRIUM);
                    return;
                };
                let gain = f32::from_bits(state.gain.load(Ordering::Relaxed));
                let level = |s: f32| (s * gain).clamp(-1.0, 1.0);
                for frame in out.chunks_mut(channels) {
                    // Blocks go in whole, so two queued samples are always a
                    // left and its right.
                    if head.ring.slots() < 2 {
                        frame.fill(T::EQUILIBRIUM);
                        continue;
                    }
                    let left = level(head.ring.pop().unwrap_or(0.0));
                    let right = level(head.ring.pop().unwrap_or(0.0));
                    head.advance();
                    match frame {
                        [mono] => *mono = T::from_sample(0.5 * (left + right)),
                        [l, r, rest @ ..] => {
                            *l = T::from_sample(left);
                            *r = T::from_sample(right);
                            rest.fill(T::EQUILIBRIUM);
                        }
                        [] => {}
                    }
                }
                state
                    .position
                    .store(pack(head.seek, head.position), Ordering::Release);
            },
            move |e: cpal::Error| {
                // Glitches, and the system moving sound to other speakers or
                // headphones, arrive here too; the stream carries on.
                if !matches!(
                    e.kind(),
                    ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
                ) {
                    errors.fail(e.to_string());
                    errors.playing.store(false, Ordering::Release);
                }
            },
            None,
        )
        .map_err(|e| format!("cannot open audio output: {e}"))
}

fn feed(
    mut renderer: Renderer,
    mut ring: rtrb::Producer<f32>,
    playhead: &Mutex<Playhead>,
    inbox: &mpsc::Receiver<Command>,
    shared: &Shared,
) {
    let mut seek = 0;
    let mut drained = false;
    let mut waiting = None;
    loop {
        // Everything queued is taken before any work: dragging across the
        // spectrogram sends a burst of seeks, and only the last one matters.
        let mut restart = None;
        for command in waiting.take().into_iter().chain(inbox.try_iter()) {
            match command {
                Command::Restart(r) => restart = Some(r),
                Command::Wake => {}
                Command::Quit => return,
            }
        }
        if let Some(Restart {
            frame,
            speed,
            seek: number,
            looping,
        }) = restart
        {
            let wrap = looping.as_ref().map(|r| r.start as f64..r.end as f64);
            if let Err(e) = renderer.restart(frame, speed, looping) {
                shared.fail(e);
                return;
            }
            let mut head = playhead.lock().unwrap_or_else(PoisonError::into_inner);
            let queued = head.ring.slots();
            if let Ok(stale) = head.ring.read_chunk(queued) {
                stale.commit_all();
            }
            head.seek = number;
            head.position = frame as f64;
            head.step = renderer.step();
            head.looping = wrap;
            shared
                .position
                .store(pack(number, frame as f64), Ordering::Release);
            (seek, drained) = (number, false);
        }

        if !drained && ring.slots() >= renderer.block_capacity() {
            match renderer.render() {
                Ok(Some(block)) => {
                    if let Ok(chunk) = ring.write_chunk_uninit(block.len()) {
                        chunk.fill_from_iter(block.iter().copied());
                    }
                }
                Ok(None) => drained = true,
                Err(e) => {
                    shared.fail(e);
                    return;
                }
            }
            continue;
        }
        if drained && ring.slots() == ring.buffer().capacity() {
            shared
                .finished
                .store(FINISHED | u32::from(seek), Ordering::Release);
        }
        // Nothing to do until the callback frees room or a command arrives.
        let next = if shared.playing.load(Ordering::Acquire) {
            inbox.recv_timeout(TOP_UP)
        } else {
            inbox
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        match next {
            Ok(command) => waiting = Some(command),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Source frames in; stereo at the device rate and chosen speed out.
struct Renderer {
    input: Box<dyn Frames>,
    frames: usize,
    resampler: Fft<f32>,
    sample_rate: u32,
    device_rate: u32,
    speed: u32,
    /// Next source frame to read.
    next: usize,
    /// Source frame the next output frame plays. Rendering carries on past
    /// the last frame read until this reaches the end, so the resampler's
    /// tail is heard instead of cut off.
    heard: f64,
    /// Output frames still to drop after a (re)start: the resampler's delay,
    /// which would otherwise play as a gap and put the playhead late.
    skip: usize,
    /// Frames played over and over: reading wraps from the end to the start
    /// and never finishes.
    looping: Option<Range<usize>>,
    block: Vec<[f32; 2]>,
    output: Vec<f32>,
}

fn resampler(sample_rate: u32, device_rate: u32, speed: u32) -> Result<Fft<f32>, String> {
    let input_rate = sample_rate as usize * speed as usize;
    Fft::new(input_rate, device_rate as usize, CHUNK, 2, FixedSync::Input)
        .map_err(|e| format!("cannot play {sample_rate} Hz audio at {speed}x on this output: {e}"))
}

impl Renderer {
    fn new(
        input: Box<dyn Frames>,
        frames: usize,
        sample_rate: u32,
        device_rate: u32,
        speed: u32,
        start: usize,
    ) -> Result<Self, String> {
        let resampler = resampler(sample_rate, device_rate, speed)?;
        Ok(Self {
            input,
            frames,
            skip: resampler.output_delay(),
            output: vec![0.0; 2 * resampler.output_frames_max()],
            resampler,
            sample_rate,
            device_rate,
            speed,
            next: start,
            heard: start as f64,
            looping: None,
            block: Vec::new(),
        })
    }

    fn step(&self) -> f64 {
        step(self.sample_rate, self.speed, self.device_rate)
    }

    /// Samples the next block can hold, so ring space is checked first.
    fn block_capacity(&self) -> usize {
        self.output.len()
    }

    fn restart(
        &mut self,
        frame: usize,
        speed: u32,
        looping: Option<Range<usize>>,
    ) -> Result<(), String> {
        if speed != self.speed {
            self.resampler = resampler(self.sample_rate, self.device_rate, speed)?;
            self.output
                .resize(2 * self.resampler.output_frames_max(), 0.0);
            self.speed = speed;
        }
        self.resampler.reset();
        self.skip = self.resampler.output_delay();
        self.next = frame;
        self.heard = frame as f64;
        self.looping = looping;
        Ok(())
    }

    /// The next block of interleaved stereo, or `None` once all of the
    /// source has been played.
    fn render(&mut self) -> Result<Option<&[f32]>, String> {
        if self.looping.is_none() && self.heard >= self.frames as f64 {
            return Ok(None);
        }
        let count = self.resampler.input_frames_next();
        self.block.clear();
        self.block.resize(count, [0.0; 2]);
        match self.looping.clone() {
            None => {
                let available = self.frames.saturating_sub(self.next).min(count);
                if available > 0 {
                    self.input.read(self.next, &mut self.block[..available])?;
                }
                self.next += count;
            }
            Some(range) => {
                let mut filled = 0;
                while filled < count {
                    if self.next >= range.end {
                        self.next = range.start;
                    }
                    let n = (range.end - self.next).min(count - filled);
                    self.input
                        .read(self.next, &mut self.block[filled..filled + n])?;
                    filled += n;
                    self.next += n;
                }
            }
        }
        let input = InterleavedSlice::new(self.block.as_flattened(), 2, count)
            .map_err(|e| e.to_string())?;
        let capacity = self.output.len() / 2;
        let mut output =
            InterleavedSlice::new_mut(&mut self.output, 2, capacity).map_err(|e| e.to_string())?;
        let (_, produced) = self
            .resampler
            .process_into_buffer(&input, &mut output, None)
            .map_err(|e| e.to_string())?;
        let dropped = produced.min(self.skip);
        self.skip -= dropped;
        let kept = if self.looping.is_some() {
            produced - dropped
        } else {
            // Up to the last source frame and no further, so playback ends
            // when the recording does.
            let remaining = ((self.frames as f64 - self.heard) / self.step()).ceil() as usize;
            (produced - dropped).min(remaining)
        };
        self.heard += kept as f64 * self.step();
        Ok(Some(&self.output[2 * dropped..2 * (dropped + kept)]))
    }
}

/// Stereo frames of a file by position.
trait Frames {
    /// Fills `out` with the frames from `first` on. Never asked for frames
    /// past the end.
    fn read(&mut self, first: usize, out: &mut [[f32; 2]]) -> Result<(), String>;
}

/// The first two channels of a file: mono is doubled, and channels past
/// the second are left out.
struct Stereo {
    reader: Reader,
    channels: usize,
    scratch: Vec<f32>,
}

impl Frames for Stereo {
    fn read(&mut self, first: usize, out: &mut [[f32; 2]]) -> Result<(), String> {
        let ch = self.channels;
        self.scratch.resize(out.len() * ch, 0.0);
        self.reader.read(first, &mut self.scratch)?;
        let right = usize::from(ch > 1);
        for (pair, frame) in out.iter_mut().zip(self.scratch.chunks_exact(ch)) {
            *pair = [frame[0], frame[right]];
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spectrogram::spectrum_around;
    use crate::wav;

    fn tone(frequency: f64, rate: u32, seconds: f64) -> Vec<f32> {
        let n = (f64::from(rate) * seconds) as usize;
        (0..n)
            .map(|i| (std::f64::consts::TAU * frequency * i as f64 / f64::from(rate)).sin() as f32)
            .collect()
    }

    struct Memory(Vec<f32>);

    impl Frames for Memory {
        fn read(&mut self, first: usize, out: &mut [[f32; 2]]) -> Result<(), String> {
            for (pair, &s) in out.iter_mut().zip(&self.0[first..]) {
                *pair = [s, s];
            }
            Ok(())
        }
    }

    fn renderer(samples: Vec<f32>, rate: u32, device: u32, speed: u32) -> Renderer {
        let frames = samples.len();
        Renderer::new(Box::new(Memory(samples)), frames, rate, device, speed, 0).unwrap()
    }

    /// Up to `limit` frames of what the renderer produces, left channel
    /// only.
    fn render_left(mut renderer: Renderer, limit: usize) -> Vec<f32> {
        let mut left = Vec::new();
        while left.len() < limit
            && let Some(block) = renderer.render().unwrap()
        {
            left.extend(block.as_chunks::<2>().0.iter().map(|pair| pair[0]));
        }
        left
    }

    fn peak(signal: &[f32], rate: u32) -> (f32, f32) {
        let fft = 8192;
        let s = spectrum_around(signal, signal.len() / 2, fft);
        let (bin, level) = s
            .iter()
            .copied()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        (bin as f32 * rate as f32 / fft as f32, level)
    }

    fn middle_rms(signal: &[f32]) -> f32 {
        let middle = &signal[signal.len() / 10..signal.len() * 9 / 10];
        (middle.iter().map(|s| s * s).sum::<f32>() / middle.len() as f32).sqrt()
    }

    #[test]
    fn speed_multiplies_pitch_and_shortens_the_file() {
        let samples = tone(1_000.0, 48_000, 2.0);
        let frames = samples.len();
        let out = render_left(renderer(samples, 48_000, 48_000, 2), usize::MAX);
        let (hz, _) = peak(&out, 48_000);
        assert!((hz - 2_000.0).abs() < 12.0, "peak at {hz} Hz");
        assert!(
            out.len().abs_diff(frames / 2) < CHUNK,
            "{} frames",
            out.len()
        );
    }

    #[test]
    fn playback_ends_exactly_where_the_recording_does() {
        let mut samples = vec![0.0; 48_000];
        samples[47_900..].fill(0.5);
        let out = render_left(renderer(samples, 48_000, 44_100, 1), usize::MAX);
        assert!(out.len().abs_diff(44_100) <= 1, "{} frames", out.len());
        let last = &out[out.len() - 50..];
        assert!(
            last.iter().any(|s| s.abs() > 0.4),
            "the last source frames were not played"
        );
    }

    #[test]
    fn a_loop_repeats_its_range_and_never_ends() {
        // Silence, then a steady level: a loop over the second half never
        // plays the first, however long it runs.
        let mut samples = vec![0.0; 48_000];
        samples[24_000..].fill(0.5);
        let mut looped = renderer(samples, 48_000, 44_100, 1);
        looped.restart(30_000, 1, Some(24_000..48_000)).unwrap();
        let out = render_left(looped, 200_000);
        assert!(out.len() >= 200_000, "stopped after {} frames", out.len());
        let settled = &out[1_000..];
        assert!(
            settled.iter().all(|s| (s - 0.5).abs() < 0.05),
            "the loop left its range"
        );
    }

    #[test]
    fn the_playhead_wraps_where_the_loop_does() {
        let (_, ring) = rtrb::RingBuffer::new(2);
        let mut head = Playhead {
            ring,
            seek: 0,
            position: 2_999.5,
            step: 1.0,
            looping: Some(1_000.0..3_000.0),
        };
        head.advance();
        assert_eq!(head.position, 1_000.5);
    }

    #[test]
    fn ultrasound_is_filtered_rather_than_folded_into_the_audible_band() {
        let out = render_left(
            renderer(tone(100_000.0, 384_000, 1.0), 384_000, 48_000, 1),
            usize::MAX,
        );
        assert!(middle_rms(&out) < 1e-3, "rms {}", middle_rms(&out));
    }

    #[test]
    fn audible_content_of_a_high_rate_file_survives() {
        // Exactly on a bin of the measuring FFT, so window scalloping cannot
        // stand in for passband loss.
        let frequency = 1707.0 * 48_000.0 / 8192.0;
        let out = render_left(
            renderer(tone(frequency, 384_000, 1.0), 384_000, 48_000, 1),
            usize::MAX,
        );
        let (hz, level) = peak(&out, 48_000);
        assert!((f64::from(hz) - frequency).abs() < 1.0, "peak at {hz} Hz");
        assert!(level > -0.5, "level {level} dBFS");
    }

    #[test]
    fn eight_times_a_384k_file_still_resamples_cleanly() {
        let out = render_left(
            renderer(tone(2_000.0, 384_000, 1.0), 384_000, 48_000, 8),
            usize::MAX,
        );
        let (hz, _) = peak(&out, 48_000);
        assert!((hz - 16_000.0).abs() < 12.0, "peak at {hz} Hz");
    }

    fn stereo_of(file: &[u8], channels: u16) -> (Stereo, std::path::PathBuf) {
        let path = crate::audio::tests::temp_file(&format!("stereo-{channels}.wav"), file);
        let w = wav::parse(&mut std::io::Cursor::new(file)).unwrap();
        let source = Source::Pcm {
            path: path.clone(),
            data: w.data.clone(),
            kind: w.kind,
        };
        let reader = Reader::open(&source, usize::from(channels)).unwrap();
        let stereo = Stereo {
            reader,
            channels: usize::from(channels),
            scratch: Vec::new(),
        };
        (stereo, path)
    }

    #[test]
    fn mono_is_doubled_and_channels_past_two_are_left_out() {
        let (mut mono, path) = stereo_of(&wav::tests::build(false, &[], &[0, 16_384, -16_384]), 1);
        let mut out = [[0.0; 2]; 2];
        mono.read(1, &mut out).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(out, [[0.5, 0.5], [-0.5, -0.5]]);

        let samples = [8_192, -8_192, 16_384, 0, 16_384, -16_384, 1, 2, 3];
        let file = wav::tests::build_channels(false, 3, &[], &samples);
        let (mut three, path) = stereo_of(&file, 3);
        three.read(1, &mut out).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(out[0], [0.0, 0.5]);
    }

    #[test]
    fn positions_pack_with_their_seek() {
        assert_eq!(unpack(pack(7, 123_456.9)), (7, 123_456));
        assert_eq!(unpack(pack(u16::MAX, -3.0)), (u16::MAX, 0));
    }
}
