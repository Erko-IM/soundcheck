//! Per-channel levels over the whole file, for the meters: gathered while
//! the file is first read, so following the playhead costs nothing.

use rayon::prelude::*;

/// Levels below this read as silence.
pub const FLOOR_DB: f32 = -100.0;
/// About how many samples [`Levels::push`] takes at once.
const LANES: usize = 64;

pub struct Levels {
    /// Frames per block.
    pub block: usize,
    pub channels: usize,
    /// Peak and mean square of each block, block after block, channel
    /// after channel within a block.
    stats: Vec<[f32; 2]>,
}

/// Each channel's peak and mean square over `block`, interleaved frames of
/// `ch` channels, into `stats`. The block is taken a row of whole frames at
/// a time, a row wide enough that the compiler takes many samples at once,
/// each place in it adding up on its own in `peak` and `energy`.
fn measure(block: &[f32], ch: usize, peak: &mut [f32], energy: &mut [f32], stats: &mut [[f32; 2]]) {
    let width = peak.len();
    peak.fill(0.0);
    energy.fill(0.0);
    let rows = block.chunks_exact(width);
    let rest = rows.remainder();
    for row in rows.chain([rest]) {
        for ((p, e), &x) in peak.iter_mut().zip(energy.iter_mut()).zip(row) {
            *p = p.max(x.abs());
            *e += x * x;
        }
    }
    let frames = (block.len() / ch).max(1) as f32;
    for (c, s) in stats.iter_mut().enumerate() {
        let lanes = (c..width).step_by(ch);
        let top = lanes.clone().map(|i| peak[i]).fold(0.0, f32::max);
        *s = [top, lanes.map(|i| energy[i]).sum::<f32>() / frames];
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub rms_db: f32,
    pub peak_db: f32,
}

pub fn db(amplitude: f32) -> f32 {
    (20.0 * amplitude.log10()).max(FLOOR_DB)
}

impl Levels {
    /// Blocks of 20 ms, the rate the meters redraw at.
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            block: (sample_rate as usize / 50).max(1),
            channels,
            stats: Vec::new(),
        }
    }

    /// Frames read from the file, in order and starting on a block
    /// boundary.
    pub fn push(&mut self, samples: &[f32]) {
        let ch = self.channels;
        let start = self.stats.len();
        let blocks = samples.len().div_ceil(self.block * ch);
        self.stats.resize(start + blocks * ch, [0.0; 2]);
        let width = ch * LANES.div_ceil(ch);
        self.stats[start..]
            .par_chunks_mut(ch)
            .zip(samples.par_chunks(self.block * ch))
            .for_each_init(
                || (vec![0.0f32; width], vec![0.0f32; width]),
                |(peak, energy), (stats, block)| measure(block, ch, peak, energy, stats),
            );
    }

    /// Room for every block of a file `frames` long, block after block and
    /// channel after channel within a block, for [`Meter::measure`] to fill
    /// a stretch at a time, and the meter to do it with.
    pub fn blocks(&mut self, frames: usize) -> (Meter, &mut [[f32; 2]]) {
        self.stats = vec![[0.0; 2]; frames.div_ceil(self.block) * self.channels];
        let meter = Meter {
            block: self.block,
            channels: self.channels,
        };
        (meter, &mut self.stats)
    }

    /// Keeps only the blocks of the first `frames` frames: the meter, and
    /// room for the last block again when `frames` cuts it short.
    pub fn cut(&mut self, frames: usize) -> (Meter, &mut [[f32; 2]]) {
        let whole = frames / self.block * self.channels;
        self.stats
            .truncate(frames.div_ceil(self.block) * self.channels);
        let meter = Meter {
            block: self.block,
            channels: self.channels,
        };
        (meter, &mut self.stats[whole..])
    }
}

/// Measures stretches of a file into [`Levels::blocks`], on any thread.
#[derive(Clone, Copy)]
pub struct Meter {
    pub block: usize,
    channels: usize,
}

impl Meter {
    /// Measures `samples`, whole blocks from a block boundary on and maybe
    /// the file's last short one, into their stretch of the blocks.
    pub fn measure(&self, samples: &[f32], stats: &mut [[f32; 2]]) {
        let ch = self.channels;
        let width = ch * LANES.div_ceil(ch);
        let (mut peak, mut energy) = (vec![0.0f32; width], vec![0.0f32; width]);
        for (stats, block) in stats.chunks_mut(ch).zip(samples.chunks(self.block * ch)) {
            measure(block, ch, &mut peak, &mut energy, stats);
        }
    }
}

impl Levels {
    /// Each channel's level over `span` frames ending at `frame`.
    pub fn at(&self, frame: usize, span: usize) -> Vec<Level> {
        let blocks = self.stats.len() / self.channels.max(1);
        let last = (frame / self.block).min(blocks.saturating_sub(1));
        let first = last.saturating_sub((span / self.block).max(1) - 1);
        (0..self.channels)
            .map(|c| {
                let (peak, energy, n) = (first..=last)
                    .filter_map(|b| self.stats.get(b * self.channels + c))
                    .fold((0.0f32, 0.0f32, 0usize), |(p, e, n), s| {
                        (p.max(s[0]), e + s[1], n + 1)
                    });
                Level {
                    rms_db: db((energy / n.max(1) as f32).sqrt()),
                    peak_db: db(peak),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_per_channel_and_follow_time() {
        // One second of stereo: the left a full-scale square wave, the right
        // silent until a half-scale half second at the end.
        let mut samples = Vec::new();
        for i in 0..48_000 {
            samples.push(if i % 2 == 0 { 1.0 } else { -1.0 });
            samples.push(if i >= 24_000 { 0.5 } else { 0.0 });
        }
        let mut levels = Levels::new(48_000, 2);
        levels.push(&samples);
        let start = levels.at(4_800, 4_800);
        assert_eq!(start[0].peak_db, 0.0);
        assert!(start[0].rms_db.abs() < 0.01);
        assert_eq!(start[1].peak_db, FLOOR_DB);
        let end = levels.at(47_999, 4_800);
        assert!((end[1].peak_db - db(0.5)).abs() < 0.01);
        assert!((end[1].rms_db - db(0.5)).abs() < 0.01);
    }
}
