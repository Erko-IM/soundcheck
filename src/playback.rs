//! Playback. A feeder thread reads the file, shifts its pitch if asked,
//! resamples it to the output device's rate at the chosen speed, and keeps a
//! lock-free ring buffer topped up; the audio callback only drains that
//! buffer, so it never waits on the disk or allocates. While paused the
//! device is stopped and the feeder sleeps until it is told something, so a
//! paused player costs nothing.

use std::cell::{Cell, RefCell};
use std::f32::consts::TAU;
use std::ops::{Range, RangeInclusive};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample};
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction};

use crate::audio::{Info, Reader, Source};

/// A playback speed: how many times faster than it was recorded. Pitch
/// moves with speed, as on tape.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Speed(f64);

impl Speed {
    pub const NORMAL: Self = Self(1.0);
    pub const FAST: [Self; 4] = [Self(1.0), Self(2.0), Self(4.0), Self(8.0)];
    /// At a tenth of the speed, a bat's call is ten times lower and long
    /// enough to follow.
    pub const SLOW: [Self; 4] = [Self(0.1), Self(0.125), Self(0.25), Self(0.5)];
    /// The ends of the speed slider; a speed typed in goes further.
    pub const SLIDER: RangeInclusive<f64> = 0.001..=1000.0;
    /// Any slower and a second of sound would last more than a day.
    pub const SLOWEST: f64 = 1e-5;
    /// No file could be read faster; most stop sooner, see [`fastest`].
    pub const FASTEST: f64 = 1e5;

    /// `value` as a speed playback can go at.
    pub fn new(value: f64) -> Self {
        if value.is_finite() {
            Self(value.clamp(Self::SLOWEST, Self::FASTEST))
        } else {
            Self::NORMAL
        }
    }

    pub fn value(self) -> f64 {
        self.0
    }

    /// The preset at `value`, if it is one.
    pub fn preset(value: f64) -> Option<Self> {
        Self::SLOW
            .into_iter()
            .chain(Self::FAST)
            .find(|s| (s.0 - value).abs() < 1e-9)
    }

    pub fn label(self) -> String {
        speed_text(self.0)
    }
}

/// A speed as a label shows it: a whole number of times slower as a
/// fraction, like 1/8×, and anything else to four significant digits.
pub fn speed_text(value: f64) -> String {
    let over = 1.0 / value;
    if value < 1.0 && (over - over.round()).abs() < 1e-6 {
        return format!("1/{}×", over.round());
    }
    let decimals = (3 - value.log10().floor() as i32).max(0) as usize;
    let shown = format!("{value:.decimals$}");
    let shown = if shown.contains('.') {
        shown.trim_end_matches('0').trim_end_matches('.')
    } else {
        &shown
    };
    format!("{shown}×")
}

/// A speed typed as 2, 2.5, 1/8 or 0.125, with × or x or without.
pub fn parse_speed(text: &str) -> Option<f64> {
    let marks: &[char] = &['×', 'x', 'X'];
    let text = text.trim().trim_matches(marks).trim();
    let value = match text.split_once('/') {
        Some((a, b)) => a.trim().parse::<f64>().ok()? / b.trim().parse::<f64>().ok()?,
        None => text.parse().ok()?,
    };
    (value.is_finite() && value > 0.0).then_some(value)
}

/// The slowest a file at `sample_rate` plays: its highest frequency comes
/// out at 2.4 Hz, over three octaves below hearing. Any slower only keeps
/// each start and seek waiting, while the resampler works through the
/// thousands of frames it makes of each one before the first is heard.
pub fn slowest(sample_rate: u32) -> f64 {
    // Frames a second, a ten-thousandth of a 48 kHz file's.
    const FLOOR: f64 = 4.8;
    (FLOOR / f64::from(sample_rate)).max(Speed::SLOWEST)
}

/// The fastest `source` plays: any faster and reading and filtering it
/// would fall behind the sound going out.
pub fn fastest(source: &Source, sample_rate: u32) -> f64 {
    // Frames a second the feeder keeps up with: plain samples read and
    // filter far faster than a codec decodes.
    let budget = match source {
        Source::Pcm { .. } => 5e7,
        Source::Coded(_) => 1e7,
    };
    (budget / f64::from(sample_rate)).min(Speed::FASTEST)
}

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
    speed: Speed,
    /// Semitones.
    pitch: f32,
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
    speed: Cell<Speed>,
    pitch: Cell<f32>,
    looping: RefCell<Option<Range<usize>>>,
    /// Seeks sent so far, and where the last one went: until the feeder has
    /// acted on it, playback is wherever it was last sent.
    seeks: Cell<u16>,
    target: Cell<usize>,
}

impl Player {
    /// Plays from `start` at `speed`, `pitch` semitones up or down.
    pub fn new(
        source: &Source,
        info: &Info,
        start: usize,
        speed: Speed,
        pitch: f32,
    ) -> Result<Self, String> {
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
                    Renderer::new(input, frames, sample_rate, device_rate, speed, pitch, start)
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
            pitch: Cell::new(pitch),
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
        self.restart(frame);
    }

    pub fn speed(&self) -> Speed {
        self.speed.get()
    }

    pub fn pitch(&self) -> f32 {
        self.pitch.get()
    }

    pub fn set_speed(&self, speed: Speed) {
        self.speed.set(speed);
        self.restart(self.position());
    }

    /// Moves the sound `pitch` semitones up or down, at the same speed.
    pub fn set_pitch(&self, pitch: f32) {
        self.pitch.set(pitch);
        self.restart(self.position());
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

    fn restart(&self, frame: usize) {
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
            speed: self.speed.get(),
            pitch: self.pitch.get(),
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

fn step(sample_rate: u32, speed: Speed, device_rate: u32) -> f64 {
    f64::from(sample_rate) * speed.value() / f64::from(device_rate)
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
            pitch,
            seek: number,
            looping,
        }) = restart
        {
            let wrap = looping.as_ref().map(|r| r.start as f64..r.end as f64);
            if let Err(e) = renderer.restart(frame, speed, pitch, looping) {
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

/// Source frames in; stereo at the device rate, chosen speed and pitch out.
struct Renderer {
    input: Box<dyn Frames>,
    frames: usize,
    chain: Chain,
    sample_rate: u32,
    device_rate: u32,
    speed: Speed,
    pitch: f32,
    /// Next source frame to read.
    next: usize,
    /// Source frame the next output frame plays. Rendering carries on past
    /// the last frame read until this reaches the end, so the resampler's
    /// tail is heard instead of cut off.
    heard: f64,
    /// Output frames still to drop after a (re)start: the chain's delay,
    /// which would otherwise play as a gap and put the playhead late.
    skip: usize,
    /// Frames played over and over: reading wraps from the end to the start
    /// and never finishes.
    looping: Option<Range<usize>>,
    block: Vec<[f32; 2]>,
    thinned: Vec<[f32; 2]>,
    output: Vec<f32>,
}

/// What the source frames go through on the way out, built for one speed
/// and pitch.
struct Chain {
    /// Source frames to each frame going on: more than one only at speeds
    /// where the resampler would otherwise have to thin out by far.
    factor: usize,
    decimator: Option<Decimator>,
    shifter: Option<Shifter>,
    resampler: Async<f32>,
}

impl Chain {
    fn new(sample_rate: u32, device_rate: u32, speed: Speed, pitch: f32) -> Result<Self, String> {
        let ratio = 2f64.powf(f64::from(pitch) / 12.0);
        let step = step(sample_rate, speed, device_rate);
        // Thinned by as much as still keeps all that will be heard, pitch
        // shifted down included, with room for the filter to fall.
        let factor = ((step * ratio.min(1.0) / 2.0).floor() as usize).max(1);
        let rate = f64::from(sample_rate) / factor as f64;
        let out_per_in = f64::from(device_rate) / (rate * speed.value());
        // Longer where the resampler thins out, so it still falls steeply.
        let sinc_len = ((128.0 / out_per_in.min(1.0)).ceil() as usize).clamp(256, 2048);
        let parameters =
            SincInterpolationParameters::new(sinc_len, WindowFunction::BlackmanHarris2)
                .oversampling_factor(256);
        let resampler =
            Async::<f32>::new_sinc(out_per_in, 1.0, &parameters, CHUNK, 2, FixedAsync::Output)
                .map_err(|e| {
                    let speed = speed.label();
                    format!("cannot play {sample_rate} Hz audio at {speed} on this output: {e}")
                })?;
        Ok(Self {
            factor,
            decimator: (factor > 1).then(|| Decimator::new(factor)),
            shifter: (pitch != 0.0).then(|| Shifter::new(ratio as f32, rate)),
            resampler,
        })
    }

    fn reset(&mut self) {
        if let Some(decimator) = &mut self.decimator {
            decimator.reset();
        }
        if let Some(shifter) = &mut self.shifter {
            shifter.reset();
        }
        self.resampler.reset();
    }

    /// Output frames that come out ahead of the source frame they start
    /// from, with `step` source frames to each output frame.
    fn delay(&self, step: f64) -> usize {
        let thinned = self.decimator.as_ref().map_or(0.0, |d| d.delay() / step);
        let shifted = self
            .shifter
            .as_ref()
            .map_or(0.0, |s| s.latency() as f64 * self.factor as f64 / step);
        self.resampler.output_delay() + (thinned + shifted).round() as usize
    }
}

impl Renderer {
    fn new(
        input: Box<dyn Frames>,
        frames: usize,
        sample_rate: u32,
        device_rate: u32,
        speed: Speed,
        pitch: f32,
        start: usize,
    ) -> Result<Self, String> {
        let chain = Chain::new(sample_rate, device_rate, speed, pitch)?;
        let mut renderer = Self {
            input,
            frames,
            skip: 0,
            output: vec![0.0; 2 * chain.resampler.output_frames_max()],
            chain,
            sample_rate,
            device_rate,
            speed,
            pitch,
            next: start,
            heard: start as f64,
            looping: None,
            block: Vec::new(),
            thinned: Vec::new(),
        };
        renderer.skip = renderer.delay();
        Ok(renderer)
    }

    fn step(&self) -> f64 {
        step(self.sample_rate, self.speed, self.device_rate)
    }

    /// Output frames that come out ahead of the frame they start from.
    fn delay(&self) -> usize {
        self.chain.delay(self.step())
    }

    /// Samples the next block can hold, so ring space is checked first.
    fn block_capacity(&self) -> usize {
        self.output.len()
    }

    fn restart(
        &mut self,
        frame: usize,
        speed: Speed,
        pitch: f32,
        looping: Option<Range<usize>>,
    ) -> Result<(), String> {
        if speed != self.speed || pitch != self.pitch {
            self.chain = Chain::new(self.sample_rate, self.device_rate, speed, pitch)?;
            self.output
                .resize(2 * self.chain.resampler.output_frames_max(), 0.0);
            (self.speed, self.pitch) = (speed, pitch);
        }
        self.chain.reset();
        self.skip = self.delay();
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
        let Chain {
            factor,
            decimator,
            shifter,
            resampler,
        } = &mut self.chain;
        let wanted = resampler.input_frames_next();
        let count = wanted * *factor;
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
        let going_on = match decimator {
            Some(decimator) => {
                decimator.process(&self.block, &mut self.thinned);
                &mut self.thinned
            }
            None => &mut self.block,
        };
        if let Some(shifter) = shifter {
            shifter.process(going_on);
        }
        let input =
            InterleavedSlice::new(going_on.as_flattened(), 2, wanted).map_err(|e| e.to_string())?;
        let capacity = self.output.len() / 2;
        let mut output =
            InterleavedSlice::new_mut(&mut self.output, 2, capacity).map_err(|e| e.to_string())?;
        let (_, produced) = resampler
            .process_into_buffer(&input, &mut output, None)
            .map_err(|e| e.to_string())?;
        let dropped = produced.min(self.skip);
        self.skip -= dropped;
        let step = step(self.sample_rate, self.speed, self.device_rate);
        let kept = if self.looping.is_some() {
            produced - dropped
        } else {
            // Up to the last source frame and no further, so playback ends
            // when the recording does.
            let remaining = ((self.frames as f64 - self.heard) / step).ceil() as usize;
            (produced - dropped).min(remaining)
        };
        self.heard += kept as f64 * step;
        Ok(Some(&self.output[2 * dropped..2 * (dropped + kept)]))
    }
}

/// Low-passes and keeps one frame in every `factor`, so a high speed reads
/// ahead fast without the resampler thinning out by hundreds.
struct Decimator {
    factor: usize,
    /// A windowed sinc, cutting off at the Nyquist frequency of the frames
    /// kept.
    taps: Vec<f32>,
    /// The frames before this block the filter still reaches, oldest first.
    history: Vec<[f32; 2]>,
    frames: Vec<[f32; 2]>,
}

impl Decimator {
    /// Taps for each frame kept: enough for the filter to fall about 90 dB
    /// between a frequency kept and one that would fold back onto it.
    const TAPS_PER_FRAME: usize = 16;

    fn new(factor: usize) -> Self {
        let len = Self::TAPS_PER_FRAME * factor + 1;
        let middle = (len - 1) as f64 / 2.0;
        let cutoff = 0.5 / factor as f64;
        // Kaiser's window at β 8.6, about 90 dB down.
        let beta = 8.6;
        let taps: Vec<f64> = (0..len)
            .map(|k| {
                let x = k as f64 - middle;
                let sinc = if x == 0.0 {
                    2.0 * cutoff
                } else {
                    (std::f64::consts::TAU * cutoff * x).sin() / (std::f64::consts::PI * x)
                };
                let edge = (1.0 - (x / middle).powi(2)).max(0.0).sqrt();
                sinc * bessel_i0(beta * edge) / bessel_i0(beta)
            })
            .collect();
        let sum: f64 = taps.iter().sum();
        Self {
            factor,
            taps: taps.iter().map(|t| (t / sum) as f32).collect(),
            history: vec![[0.0; 2]; len - 1],
            frames: Vec::new(),
        }
    }

    /// Source frames between one going in and its sound coming out.
    fn delay(&self) -> f64 {
        (self.taps.len() - 1) as f64 / 2.0 - (self.factor - 1) as f64
    }

    fn reset(&mut self) {
        self.history.fill([0.0; 2]);
    }

    /// One frame for every `factor` of `block`, whose length is a multiple
    /// of it.
    fn process(&mut self, block: &[[f32; 2]], out: &mut Vec<[f32; 2]>) {
        let (len, held) = (self.taps.len(), self.history.len());
        self.frames.clear();
        self.frames.extend_from_slice(&self.history);
        self.frames.extend_from_slice(block);
        out.clear();
        out.extend((1..=block.len() / self.factor).map(|j| {
            let end = held + j * self.factor;
            dot(&self.taps, &self.frames[end - len..end])
        }));
        self.history
            .copy_from_slice(&self.frames[self.frames.len() - held..]);
    }
}

/// Both channels of `frames` weighted by `taps`, eight at a time so the
/// compiler can work them side by side.
fn dot(taps: &[f32], frames: &[[f32; 2]]) -> [f32; 2] {
    let (mut left, mut right) = ([0.0f32; 8], [0.0f32; 8]);
    let ((t8, t_rest), (f8, f_rest)) = (taps.as_chunks::<8>(), frames.as_chunks::<8>());
    for (t, f) in t8.iter().zip(f8) {
        for i in 0..8 {
            left[i] += t[i] * f[i][0];
            right[i] += t[i] * f[i][1];
        }
    }
    let mut out = [left.iter().sum(), right.iter().sum()];
    for (t, f) in t_rest.iter().zip(f_rest) {
        out[0] += t * f[0];
        out[1] += t * f[1];
    }
    out
}

/// The modified Bessel function I0, for Kaiser's window.
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..50 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

/// Frames overlap this many times over in the shifter.
const OVERLAP: usize = 8;

/// Moves every frequency by `ratio` and leaves the timing as it is: a phase
/// vocoder that moves each spectral peak with the bins around it, keeping
/// their phases locked to the peak's (Laroche and Dolson, "New
/// phase-vocoder techniques for pitch-shifting", 1999), so a partial keeps
/// its strength at any ratio. It works before the resampler, at the file's
/// own rate unless a high speed thins the frames out first, so that calls
/// far above hearing are brought down into it before the resampler would
/// have filtered them out.
struct Shifter {
    ratio: f32,
    size: usize,
    hop: usize,
    window: Vec<f32>,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    voices: [Voice; 2],
    /// Where the next frame goes in each voice's input.
    rover: usize,
    frame: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    /// Each bin's magnitude, and the frequency in bins of what it hears.
    heard: Vec<(f32, f32)>,
    peaks: Vec<usize>,
    /// The moved peaks' phases for the next frame to go on from.
    next_phase: Vec<f32>,
}

/// One channel of the shifter.
#[derive(Clone)]
struct Voice {
    input: Vec<f32>,
    output: Vec<f32>,
    /// Frames added together as they overlap.
    sum: Vec<f32>,
    heard_phase: Vec<f32>,
    moved_phase: Vec<f32>,
}

/// `angle` brought within half a turn either way.
fn wrap(angle: f32) -> f32 {
    angle - TAU * (angle / TAU).round()
}

impl Shifter {
    /// For frames at `rate` a second.
    fn new(ratio: f32, rate: f64) -> Self {
        // About 20 ms: shorter smears a call less in time, longer its pitch.
        let size = ((rate / 50.0) as usize).next_power_of_two().max(256);
        let bins = size / 2 + 1;
        let mut planner = RealFftPlanner::<f32>::new();
        let voice = Voice {
            input: vec![0.0; size],
            output: vec![0.0; size],
            sum: vec![0.0; size],
            heard_phase: vec![0.0; bins],
            moved_phase: vec![0.0; bins],
        };
        let hop = size / OVERLAP;
        Self {
            ratio,
            size,
            hop,
            window: (0..size)
                .map(|k| 0.5 - 0.5 * (TAU * k as f32 / size as f32).cos())
                .collect(),
            forward: planner.plan_fft_forward(size),
            inverse: planner.plan_fft_inverse(size),
            voices: [voice.clone(), voice],
            rover: size - hop,
            frame: vec![0.0; size],
            spectrum: vec![Complex::default(); bins],
            heard: vec![(0.0, 0.0); bins],
            peaks: Vec::with_capacity(bins),
            next_phase: vec![0.0; bins],
        }
    }

    /// Frames between one going in and coming out.
    fn latency(&self) -> usize {
        self.size - self.hop
    }

    fn reset(&mut self) {
        for voice in &mut self.voices {
            for part in [
                &mut voice.input,
                &mut voice.output,
                &mut voice.sum,
                &mut voice.heard_phase,
                &mut voice.moved_phase,
            ] {
                part.fill(0.0);
            }
        }
        self.rover = self.latency();
    }

    /// Replaces each frame of `block` with the shifted sound of the frame
    /// `latency()` earlier.
    fn process(&mut self, block: &mut [[f32; 2]]) {
        let latency = self.latency();
        for frame in block {
            for (voice, sample) in self.voices.iter_mut().zip(frame.iter_mut()) {
                voice.input[self.rover] = *sample;
                *sample = voice.output[self.rover - latency];
            }
            self.rover += 1;
            if self.rover == self.size {
                self.rover = latency;
                for channel in 0..2 {
                    self.shift(channel);
                }
            }
        }
    }

    /// One frame of `channel`: each partial found, moved, and added back.
    fn shift(&mut self, channel: usize) {
        let (size, hop, bins) = (self.size, self.hop, self.size / 2 + 1);
        // How far a bin's phase turns in a hop, were it exactly on the bin.
        let expected = TAU * hop as f32 / size as f32;
        let voice = &mut self.voices[channel];
        for ((f, &x), &w) in self.frame.iter_mut().zip(&voice.input).zip(&self.window) {
            *f = x * w;
        }
        // The buffers are the sizes the plans were made for, so neither
        // transform can fail.
        let _ = self.forward.process(&mut self.frame, &mut self.spectrum);
        for k in 0..bins {
            let (magnitude, phase) = self.spectrum[k].to_polar();
            let turned = wrap(phase - voice.heard_phase[k] - k as f32 * expected);
            voice.heard_phase[k] = phase;
            self.heard[k] = (magnitude, k as f32 + turned * OVERLAP as f32 / TAU);
        }
        self.peaks.clear();
        self.peaks.extend((1..bins - 1).filter(|&k| {
            let m = self.heard[k].0;
            m > self.heard[k - 1].0 && m >= self.heard[k + 1].0
        }));
        self.spectrum.fill(Complex::default());
        self.next_phase.copy_from_slice(&voice.moved_phase);
        // Each peak's region runs to the quietest bin before the next peak.
        let mut start = 0;
        for (i, &peak) in self.peaks.iter().enumerate() {
            let end = match self.peaks.get(i + 1) {
                Some(&next) => (peak..next)
                    .min_by(|&a, &b| self.heard[a].0.total_cmp(&self.heard[b].0))
                    .unwrap_or(peak),
                None => bins - 1,
            };
            let region = start..=end;
            start = end + 1;
            let frequency = self.heard[peak].1 * self.ratio;
            let to = frequency.round() as isize;
            let Ok(to) = usize::try_from(to) else {
                continue;
            };
            if to >= bins {
                continue;
            }
            // The peak turns at its new frequency from where its bin was;
            // the bins around it keep the phase they had against it.
            let phase = wrap(voice.moved_phase[to] + frequency * expected);
            self.next_phase[to] = phase;
            let shift = to as isize - peak as isize;
            for k in region {
                let Some(m) = k.checked_add_signed(shift).filter(|&m| m < bins) else {
                    continue;
                };
                let turned = phase + voice.heard_phase[k] - voice.heard_phase[peak];
                self.spectrum[m] += Complex::from_polar(self.heard[k].0, turned);
            }
        }
        voice.moved_phase.copy_from_slice(&self.next_phase);
        // A real signal's first and last bins have no imaginary part.
        self.spectrum[0].im = 0.0;
        self.spectrum[bins - 1].im = 0.0;
        let _ = self.inverse.process(&mut self.spectrum, &mut self.frame);
        // The window twice over, OVERLAP times, averages 3/8 of the frame.
        let scale = 1.0 / (size as f32 * OVERLAP as f32 * 0.375);
        for ((s, &y), &w) in voice.sum.iter_mut().zip(&self.frame).zip(&self.window) {
            *s += y * w * scale;
        }
        voice.output[..hop].copy_from_slice(&voice.sum[..hop]);
        voice.sum.copy_within(hop.., 0);
        voice.sum[size - hop..].fill(0.0);
        voice.input.copy_within(hop.., 0);
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
        shifted(samples, rate, device, Speed::new(f64::from(speed)), 0.0)
    }

    fn shifted(samples: Vec<f32>, rate: u32, device: u32, speed: Speed, pitch: f32) -> Renderer {
        let frames = samples.len();
        Renderer::new(
            Box::new(Memory(samples)),
            frames,
            rate,
            device,
            speed,
            pitch,
            0,
        )
        .unwrap()
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

    /// The level of `hz` over the middle of `signal` in dBFS, and how far
    /// below it everything else there sits: a sine and cosine fitted at
    /// `hz`, which neither a short signal nor `hz` falling between FFT bins
    /// can throw off.
    fn fit(signal: &[f32], rate: u32, hz: f64) -> (f64, f64) {
        let middle = &signal[signal.len() / 10..signal.len() * 9 / 10];
        let w = std::f64::consts::TAU * hz / f64::from(rate);
        let wave = |n: usize| (w * n as f64).sin_cos();
        let (mut a, mut b) = (0.0, 0.0);
        for (n, &x) in middle.iter().enumerate() {
            let (sin, cos) = wave(n);
            a += f64::from(x) * sin;
            b += f64::from(x) * cos;
        }
        let scale = 2.0 / middle.len() as f64;
        let (a, b) = (a * scale, b * scale);
        let rest: f64 = middle
            .iter()
            .enumerate()
            .map(|(n, &x)| {
                let (sin, cos) = wave(n);
                (f64::from(x) - a * sin - b * cos).powi(2)
            })
            .sum();
        let amplitude = a.hypot(b);
        let rest = (rest * scale).sqrt();
        (20.0 * amplitude.log10(), 20.0 * (rest / amplitude).log10())
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
        looped
            .restart(30_000, Speed::NORMAL, 0.0, Some(24_000..48_000))
            .unwrap();
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

    #[test]
    fn a_pitch_shift_moves_the_tone_and_keeps_the_length() {
        for (pitch, expected) in [(12.0, 2_000.0), (-12.0, 500.0), (7.0, 1_498.3)] {
            let samples = tone(1_000.0, 48_000, 2.0);
            let frames = samples.len();
            let out = render_left(
                shifted(samples, 48_000, 48_000, Speed::NORMAL, pitch),
                usize::MAX,
            );
            let (hz, level) = peak(&out, 48_000);
            assert!((hz - expected).abs() < 12.0, "{pitch}: peak at {hz} Hz");
            assert!(level > -3.0, "{pitch}: {level} dBFS");
            assert!(
                out.len().abs_diff(frames) <= 1,
                "{pitch}: {} frames",
                out.len()
            );
        }
    }

    #[test]
    fn a_bat_call_brought_down_three_octaves_is_heard() {
        // 60 kHz, where nobody hears and the output cannot go, comes out
        // at 7.5 kHz.
        let out = render_left(
            shifted(
                tone(60_000.0, 384_000, 1.0),
                384_000,
                48_000,
                Speed::NORMAL,
                -36.0,
            ),
            usize::MAX,
        );
        let (hz, level) = peak(&out, 48_000);
        assert!((hz - 7_500.0).abs() < 12.0, "peak at {hz} Hz");
        assert!(level > -3.0, "{level} dBFS");
    }

    #[test]
    fn slow_speeds_lower_the_pitch_and_lengthen_the_file() {
        for (speed, expected) in [(Speed::new(0.5), 500.0), (Speed::new(0.1), 100.0)] {
            let samples = tone(1_000.0, 48_000, 0.5);
            let frames = samples.len();
            let out = render_left(shifted(samples, 48_000, 44_100, speed, 0.0), usize::MAX);
            let (hz, _) = peak(&out, 44_100);
            assert!((hz - expected).abs() < 12.0, "{speed:?}: peak at {hz} Hz");
            let wanted = frames as f64 * 44_100.0 / 48_000.0 / speed.value();
            assert!(
                (out.len() as f64 - wanted).abs() < 2.0,
                "{speed:?}: {} frames",
                out.len()
            );
        }
    }

    #[test]
    fn presets_are_found_and_every_speed_reads_back_as_written() {
        assert_eq!(Speed::preset(0.125), Some(Speed::new(0.125)));
        assert_eq!(Speed::preset(3.0), None);
        let labels: Vec<String> = [0.1, 0.5, 1.0, 8.0, 0.001, 2.5, 1000.0, 0.3, 1e-5]
            .into_iter()
            .map(speed_text)
            .collect();
        assert_eq!(
            labels,
            [
                "1/10×",
                "1/2×",
                "1×",
                "8×",
                "1/1000×",
                "2.5×",
                "1000×",
                "0.3×",
                "1/100000×"
            ]
        );
        for typed in ["2.5", "2.5×", "x2.5", " 5/2 "] {
            assert_eq!(parse_speed(typed), Some(2.5), "{typed}");
        }
        assert_eq!(parse_speed("0"), None);
        assert_eq!(parse_speed("fast"), None);
        assert_eq!(Speed::new(1e9).value(), Speed::FASTEST);
    }

    #[test]
    fn a_speed_between_the_presets_plays_at_its_pitch_and_length() {
        let samples = tone(1_000.0, 48_000, 2.0);
        let frames = samples.len();
        let out = render_left(
            shifted(samples, 48_000, 48_000, Speed::new(3.7), 0.0),
            usize::MAX,
        );
        let (hz, _) = peak(&out, 48_000);
        assert!((hz - 3_700.0).abs() < 12.0, "peak at {hz} Hz");
        let (level, rest) = fit(&out, 48_000, 3_700.0);
        assert!(level > -0.1, "{level} dBFS");
        assert!(rest < -80.0, "the rest {rest} dB below it");
        let wanted = frames as f64 / 3.7;
        assert!(
            (out.len() as f64 - wanted).abs() < 3.0,
            "{} frames",
            out.len()
        );
    }

    #[test]
    fn a_thousand_times_faster_brings_infrasound_up_and_filters_the_rest() {
        // 12 Hz, 200 seconds of it, comes out at 12 kHz in a fifth of a
        // second.
        let out = render_left(
            shifted(
                tone(12.0, 48_000, 200.0),
                48_000,
                48_000,
                Speed::new(1000.0),
                0.0,
            ),
            usize::MAX,
        );
        let (hz, _) = peak(&out, 48_000);
        assert!((hz - 12_000.0).abs() < 12.0, "peak at {hz} Hz");
        let (level, rest) = fit(&out, 48_000, 12_000.0);
        assert!(level > -0.1, "{level} dBFS");
        assert!(rest < -80.0, "the rest {rest} dB below it");
        // Everything above 24 Hz would come out past what the output holds,
        // and must be filtered rather than folded back into hearing.
        let out = render_left(
            shifted(
                tone(1_000.0, 48_000, 200.0),
                48_000,
                48_000,
                Speed::new(1000.0),
                0.0,
            ),
            usize::MAX,
        );
        assert!(middle_rms(&out) < 1e-3, "rms {}", middle_rms(&out));
    }

    #[test]
    fn a_thousandth_of_the_speed_stretches_the_sound_out() {
        let samples = tone(20_000.0, 48_000, 0.01);
        let frames = samples.len();
        let out = render_left(
            shifted(samples, 48_000, 48_000, Speed::new(0.001), 0.0),
            usize::MAX,
        );
        let (hz, _) = peak(&out, 48_000);
        assert!((hz - 20.0).abs() < 6.0, "peak at {hz} Hz");
        assert!(
            (out.len() as f64 - frames as f64 * 1000.0).abs() < 3.0,
            "{} frames",
            out.len()
        );
    }

    #[test]
    fn the_slowest_speed_loses_nothing_audible_and_starts_soon() {
        for rate in [8_000, 48_000, 384_000] {
            let speed = slowest(rate);
            let top = f64::from(rate) / 2.0 * speed;
            assert!((top - 2.4).abs() < 1e-9, "{rate} Hz: top at {top} Hz");
            let mut renderer =
                shifted(tone(100.0, rate, 1.0), rate, 48_000, Speed::new(speed), 0.0);
            // The resampler's delay is dropped before anything plays: it
            // must not take long to work through.
            let blocks = (1..)
                .find(|_| renderer.render().unwrap().is_none_or(|b| !b.is_empty()))
                .unwrap();
            assert!(
                blocks < 1_300,
                "{rate} Hz: first sound after {blocks} blocks"
            );
        }
    }

    #[test]
    fn a_pitch_shift_still_works_at_a_high_speed() {
        // 20 Hz at 100 times is 2 kHz, an octave down 1 kHz.
        let out = render_left(
            shifted(
                tone(20.0, 48_000, 20.0),
                48_000,
                48_000,
                Speed::new(100.0),
                -12.0,
            ),
            usize::MAX,
        );
        let (hz, _) = peak(&out, 48_000);
        assert!((hz - 1_000.0).abs() < 12.0, "peak at {hz} Hz");
        let (level, rest) = fit(&out, 48_000, 1_000.0);
        assert!(level > -3.0, "{level} dBFS");
        assert!(rest < -60.0, "the rest {rest} dB below it");
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
