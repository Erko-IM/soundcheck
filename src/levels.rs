//! Per-channel levels over the whole file, for the meters: gathered while
//! the file is first read, so following the playhead costs nothing.

use rayon::prelude::*;

/// Levels below this read as silence.
pub const FLOOR_DB: f32 = -100.0;

pub struct Levels {
    /// Frames per block.
    pub block: usize,
    pub channels: usize,
    /// Peak and mean square of each block, block after block, channel
    /// after channel within a block.
    stats: Vec<[f32; 2]>,
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
        let blocks: Vec<Vec<[f32; 2]>> = samples
            .par_chunks(self.block * ch)
            .map(|block| {
                let frames = (block.len() / ch).max(1) as f32;
                (0..ch)
                    .map(|c| {
                        let (peak, energy) = block
                            .iter()
                            .skip(c)
                            .step_by(ch)
                            .fold((0.0f32, 0.0f32), |(p, e), &s| (p.max(s.abs()), e + s * s));
                        [peak, energy / frames]
                    })
                    .collect()
            })
            .collect();
        self.stats.extend(blocks.into_iter().flatten());
    }

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
