//! FLAC, written with an encoder of our own: fixed and LPC prediction,
//! stereo decorrelation picked block by block, and residuals Rice-coded in
//! the partitions that cost least, about as small as `flac -5` makes, with
//! the blocks of each stretch encoded side by side. Any width from 4 to 32
//! bits, at rates up to the 655.35 kHz symphonia reads, which bat
//! recordings need.
//!
//! The metadata goes ahead of the audio: the original's WAV chunks as the
//! foreign metadata `flac --keep-foreign-metadata` keeps, then the Vorbis
//! comments, the pictures, and padding for tags edited later.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use lofty::config::WriteOptions;
use lofty::picture::PictureInformation;
use lofty::tag::TagExt;
use rayon::prelude::*;

use super::carry::{self, Carried, Riff};
use super::md5::Md5;
use super::{Encode, Frames, Sample, Spec, pcm};
use crate::flac::{crc8, crc16};

/// Frames a block, as libFLAC takes by default.
const BLOCK: usize = 4096;
/// Blocks encoded side by side at a time.
const BATCH: usize = 64;
/// Room left after the metadata, so tags edited later need not move the
/// audio.
const PADDING: usize = 8192;

/// A file's bits, first bit first.
pub(super) struct Bits {
    pub bytes: Vec<u8>,
    held: u64,
    count: u32,
}

impl Bits {
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            held: 0,
            count: 0,
        }
    }

    /// The low `n` bits of `value`, `n` at most 32.
    pub fn put(&mut self, value: u64, n: u32) {
        if n == 0 {
            return;
        }
        self.held = (self.held << n) | (value & ((1u64 << n) - 1));
        self.count += n;
        while self.count >= 8 {
            self.count -= 8;
            self.bytes.push((self.held >> self.count) as u8);
        }
        self.held &= (1u64 << self.count) - 1;
    }

    pub fn zeros(&mut self, mut n: u64) {
        while n > 32 {
            self.put(0, 32);
            n -= 32;
        }
        self.put(0, n as u32);
    }

    fn rice(&mut self, u: u32, k: u32) {
        self.zeros(u64::from(u >> k));
        self.put(1, 1);
        self.put(u64::from(u), k);
    }

    pub fn align(&mut self) {
        if self.count > 0 {
            self.put(0, 8 - self.count);
        }
    }
}

fn zigzag(r: i32) -> u32 {
    ((r << 1) ^ (r >> 31)) as u32
}

/// How a residual is Rice-coded: the partition order, and each partition's
/// parameter, or `None` for one written raw at `raw` bits.
struct Rice {
    order: u32,
    params: Vec<Option<u32>>,
    raw: u32,
    bits: u64,
    wide: bool,
}

/// The cheapest Rice coding of the residual `u` (zig-zagged) of a block
/// `n` long predicted from `warmup` samples.
fn rice(u: &[u32], n: usize, warmup: usize) -> Rice {
    let mut best: Option<Rice> = None;
    let most = (0..=8u32)
        .take_while(|&p| n.is_multiple_of(1 << p) && (n >> p) > warmup)
        .last()
        .unwrap_or(0);
    for order in 0..=most {
        let parts = 1usize << order;
        let size = n >> order;
        let mut params = Vec::with_capacity(parts);
        let mut bits = 0u64;
        let mut wide = false;
        let mut raw = 0;
        let mut from = 0;
        for p in 0..parts {
            let len = if p == 0 { size - warmup } else { size };
            let part = &u[from..from + len];
            from += len;
            let sum: u64 = part.iter().map(|&v| u64::from(v)).sum();
            let mean = if len == 0 { 0 } else { sum / len as u64 };
            let guess = if mean == 0 {
                0
            } else {
                63 - mean.leading_zeros()
            };
            let cost = |k: u32| -> u64 {
                len as u64 * u64::from(k + 1) + part.iter().map(|&v| u64::from(v >> k)).sum::<u64>()
            };
            let (k, k_bits) = [guess.saturating_sub(1), guess, guess + 1]
                .into_iter()
                .filter(|&k| k <= 30)
                .map(|k| (k, cost(k)))
                .min_by_key(|&(_, b)| b)
                .unwrap_or((30, cost(30)));
            // Written raw at as many bits as the widest value takes.
            let widest = part
                .iter()
                .map(|&v| {
                    let signed = ((v >> 1) as i32) ^ -((v & 1) as i32);
                    33 - signed.leading_zeros().min(signed.leading_ones()).min(32)
                })
                .max()
                .unwrap_or(0);
            if len > 0 && (len as u64 * u64::from(widest) + 5) < k_bits {
                params.push(None);
                raw = raw.max(widest);
                bits += 5 + len as u64 * u64::from(widest);
            } else {
                wide |= k > 14;
                params.push(Some(k));
                bits += k_bits;
            }
        }
        let header = parts as u64 * if wide { 5 } else { 4 };
        bits += header + 6;
        if best.as_ref().is_none_or(|b| bits < b.bits) {
            best = Some(Rice {
                order,
                params,
                raw,
                bits,
                wide,
            });
        }
    }
    best.expect("partition order 0 always fits")
}

fn write_residual(out: &mut Bits, rice: &Rice, u: &[u32], n: usize, warmup: usize) {
    out.put(u64::from(rice.wide), 2);
    out.put(u64::from(rice.order), 4);
    let size = n >> rice.order;
    let mut from = 0;
    for (p, param) in rice.params.iter().enumerate() {
        let len = if p == 0 { size - warmup } else { size };
        let part = &u[from..from + len];
        from += len;
        let escape = if rice.wide { 31 } else { 15 };
        let width = if rice.wide { 5 } else { 4 };
        match param {
            Some(k) => {
                out.put(u64::from(*k), width);
                for &v in part {
                    out.rice(v, *k);
                }
            }
            None => {
                out.put(escape, width);
                out.put(u64::from(rice.raw), 5);
                for &v in part {
                    let signed = ((v >> 1) as i32) ^ -((v & 1) as i32);
                    out.put(signed as u32 as u64, rice.raw);
                }
            }
        }
    }
}

/// A subframe's prediction, as chosen for it.
enum Predict {
    Constant,
    Verbatim,
    Fixed(usize),
    Lpc {
        coefs: Vec<i32>,
        precision: u32,
        shift: u32,
    },
}

/// The residual of a fixed predictor of `order`, or `None` where one does
/// not fit 32 bits, as FLAC asks of a residual.
fn fixed_residual(s: &[i32], order: usize, out: &mut Vec<u32>) -> bool {
    out.clear();
    for i in order..s.len() {
        let x = |k: usize| i64::from(s[i - k]);
        let predicted = match order {
            0 => 0,
            1 => x(1),
            2 => 2 * x(1) - x(2),
            3 => 3 * x(1) - 3 * x(2) + x(3),
            _ => 4 * x(1) - 6 * x(2) + 4 * x(3) - x(4),
        };
        let r = i64::from(s[i]) - predicted;
        let Ok(r) = i32::try_from(r) else {
            return false;
        };
        if r == i32::MIN {
            return false;
        }
        out.push(zigzag(r));
    }
    true
}

fn lpc_residual(s: &[i32], coefs: &[i32], shift: u32, out: &mut Vec<u32>) -> bool {
    out.clear();
    let order = coefs.len();
    for i in order..s.len() {
        let sum: i64 = coefs
            .iter()
            .zip(s[i - order..i].iter().rev())
            .map(|(&c, &x)| i64::from(c) * i64::from(x))
            .sum();
        let r = i64::from(s[i]) - (sum >> shift);
        let Ok(r) = i32::try_from(r) else {
            return false;
        };
        if r == i32::MIN {
            return false;
        }
        out.push(zigzag(r));
    }
    true
}

/// Linear prediction coefficients of each order up to `most`, from the
/// block's autocorrelation under a Tukey window, as libFLAC works them
/// out; with the error each leaves.
fn lpc_orders(s: &[i32], most: usize) -> Vec<(Vec<f64>, f64)> {
    let n = s.len();
    let taper = (n / 4).max(1);
    let window = |i: usize| -> f64 {
        if i < taper {
            0.5 - 0.5 * (std::f64::consts::PI * i as f64 / taper as f64).cos()
        } else if i >= n - taper {
            0.5 - 0.5 * (std::f64::consts::PI * (n - 1 - i) as f64 / taper as f64).cos()
        } else {
            1.0
        }
    };
    let x: Vec<f64> = s
        .iter()
        .enumerate()
        .map(|(i, &v)| f64::from(v) * window(i))
        .collect();
    let auto: Vec<f64> = (0..=most)
        .map(|lag| x[lag..].iter().zip(&x).map(|(a, b)| a * b).sum())
        .collect();
    let mut orders = Vec::new();
    if auto[0] == 0.0 {
        return orders;
    }
    let mut error = auto[0];
    let mut lpc = vec![0.0; most];
    for i in 0..most {
        let mut r = -auto[i + 1];
        for j in 0..i {
            r -= lpc[j] * auto[i - j];
        }
        r /= error;
        lpc[i] = r;
        for j in 0..i / 2 {
            let tmp = lpc[j];
            lpc[j] += r * lpc[i - 1 - j];
            lpc[i - 1 - j] += r * tmp;
        }
        if i % 2 == 1 {
            lpc[i / 2] += lpc[i / 2] * r;
        }
        error *= 1.0 - r * r;
        orders.push((lpc[..=i].iter().map(|c| -c).collect(), error));
        if error <= 0.0 {
            break;
        }
    }
    orders
}

/// The precision libFLAC quantizes coefficients to for `bits` wide samples
/// in blocks of `n`.
fn precision(bits: u32, n: usize) -> u32 {
    if bits < 16 {
        (2 + bits / 2).max(5)
    } else if bits == 16 {
        match n {
            0..=192 => 7,
            193..=384 => 8,
            385..=576 => 9,
            577..=1152 => 10,
            1153..=2304 => 11,
            2305..=4608 => 12,
            _ => 13,
        }
    } else if n <= 384 {
        13
    } else if n <= 1152 {
        14
    } else {
        15
    }
}

/// `coefs` as whole numbers of `precision` bits, and the shift that scales
/// them back, or `None` where no shift FLAC allows fits them.
fn quantize(coefs: &[f64], precision: u32) -> Option<(Vec<i32>, u32)> {
    let most = coefs.iter().fold(0.0f64, |m, c| m.max(c.abs()));
    if most <= 0.0 || !most.is_finite() {
        return None;
    }
    let exponent = most.log2().floor() as i32 + 1;
    let shift = (precision as i32 - 1 - exponent).min(15);
    if shift < 0 {
        return None;
    }
    let limit = (1i64 << (precision - 1)) - 1;
    let mut carry = 0.0;
    let quantized = coefs
        .iter()
        .map(|c| {
            carry += c * f64::from(1u32 << shift);
            let q = (carry.round() as i64).clamp(-limit - 1, limit);
            carry -= q as f64;
            q as i32
        })
        .collect();
    Some((quantized, shift as u32))
}

/// Writes the cheapest subframe for `s`, samples `bits` wide.
fn subframe(s: &[i32], bits: u32, most_lpc: usize, out: &mut Bits) {
    let n = s.len();
    let all = s.iter().fold(0, |a, &x| a | x);
    let wasted = if all == 0 {
        0
    } else {
        all.trailing_zeros().min(bits - 1)
    };
    let shifted: Vec<i32>;
    let s = if wasted > 0 {
        shifted = s.iter().map(|&x| x >> wasted).collect();
        &shifted[..]
    } else {
        s
    };
    let width = bits - wasted;
    let header = 8 + if wasted > 0 { u64::from(wasted) } else { 0 };
    let mut best = (
        Predict::Verbatim,
        header + n as u64 * u64::from(width),
        None,
    );
    if s.iter().all(|&x| x == s[0]) {
        best = (Predict::Constant, 0, None);
    } else {
        let mut u = Vec::with_capacity(n);
        for order in 0..=4.min(n - 1) {
            if !fixed_residual(s, order, &mut u) {
                continue;
            }
            let r = rice(&u, n, order);
            let cost = header + order as u64 * u64::from(width) + r.bits;
            if cost < best.1 {
                best = (Predict::Fixed(order), cost, Some((r, u.clone())));
            }
        }
        if n > most_lpc + 1 {
            let p = precision(width, n);
            let orders = lpc_orders(s, most_lpc);
            // The order whose error and coefficients together cost least,
            // by libFLAC's estimate, and its neighbours.
            let estimate = |(i, (_, error)): (usize, &(Vec<f64>, f64))| {
                let order = i + 1;
                let per = (0.5 * (0.5 / n as f64 * error).log2()).max(0.0);
                let total = order as f64 * f64::from(p + width) + (n - order) as f64 * per;
                (total * 1000.0) as u64
            };
            if let Some(pick) = orders
                .iter()
                .enumerate()
                .min_by_key(|&o| estimate(o))
                .map(|(i, _)| i)
            {
                for i in [
                    pick.saturating_sub(1),
                    pick,
                    (pick + 1).min(orders.len() - 1),
                ] {
                    let Some((coefs, shift)) = quantize(&orders[i].0, p) else {
                        continue;
                    };
                    if !lpc_residual(s, &coefs, shift, &mut u) {
                        continue;
                    }
                    let order = coefs.len();
                    let r = rice(&u, n, order);
                    let cost = header
                        + order as u64 * u64::from(width)
                        + 4
                        + 5
                        + order as u64 * u64::from(p)
                        + r.bits;
                    if cost < best.1 {
                        best = (
                            Predict::Lpc {
                                coefs,
                                precision: p,
                                shift,
                            },
                            cost,
                            Some((r, u.clone())),
                        );
                    }
                }
            }
        }
    }
    let kind: u64 = match &best.0 {
        Predict::Constant => 0,
        Predict::Verbatim => 1,
        Predict::Fixed(order) => 8 + *order as u64,
        Predict::Lpc { coefs, .. } => 31 + coefs.len() as u64,
    };
    out.put(0, 1);
    out.put(kind, 6);
    if wasted > 0 {
        out.put(1, 1);
        out.zeros(u64::from(wasted - 1));
        out.put(1, 1);
    } else {
        out.put(0, 1);
    }
    match best {
        (Predict::Constant, ..) => out.put(s[0] as u32 as u64, width),
        (Predict::Verbatim, ..) => {
            for &x in s {
                out.put(x as u32 as u64, width);
            }
        }
        (Predict::Fixed(order), _, Some((r, u))) => {
            for &x in &s[..order] {
                out.put(x as u32 as u64, width);
            }
            write_residual(out, &r, &u, n, order);
        }
        (
            Predict::Lpc {
                coefs,
                precision,
                shift,
            },
            _,
            Some((r, u)),
        ) => {
            let order = coefs.len();
            for &x in &s[..order] {
                out.put(x as u32 as u64, width);
            }
            out.put(u64::from(precision - 1), 4);
            out.put(u64::from(shift), 5);
            for &c in &coefs {
                out.put(c as u32 as u64, precision);
            }
            write_residual(out, &r, &u, n, order);
        }
        _ => unreachable!("a predicted subframe has its residual"),
    }
}

/// About the bits a channel's best fixed predictor leaves, to pick how a
/// stereo pair is coded.
fn rough_bits(s: &[i32]) -> u64 {
    let mut sums = [0u64; 5];
    for i in 4..s.len() {
        let x = |k: usize| i64::from(s[i - k]);
        let e = [
            x(0),
            x(0) - x(1),
            x(0) - 2 * x(1) + x(2),
            x(0) - 3 * x(1) + 3 * x(2) - x(3),
            x(0) - 4 * x(1) + 6 * x(2) - 4 * x(3) + x(4),
        ];
        for (sum, e) in sums.iter_mut().zip(e) {
            *sum += e.unsigned_abs();
        }
    }
    let n = s.len().max(1) as u64;
    sums.iter()
        .map(|&sum| {
            let mean = sum / n;
            n * u64::from(64 - mean.leading_zeros() + 1)
        })
        .min()
        .unwrap_or(0)
}

/// Turns samples into FLAC frames, a block each.
pub struct FrameEncoder {
    channels: usize,
    bits: u32,
    rate: u32,
    most_lpc: usize,
    pending: Vec<i32>,
    next: u64,
    md5: Md5,
    md5_bytes: Vec<u8>,
    pub samples: u64,
    min_frame: u32,
    max_frame: u32,
}

impl FrameEncoder {
    pub fn new(spec: &Spec) -> Result<Self, String> {
        let Sample::Int(bits) = spec.sample else {
            return Err("FLAC keeps whole numbers only".into());
        };
        if !(4..=32).contains(&bits) || !(1..=8).contains(&spec.channels) {
            return Err(format!(
                "FLAC keeps 4 to 32 bits and 1 to 8 channels, not {bits} bits and {} channels",
                spec.channels
            ));
        }
        if spec.rate == 0 || spec.rate > 655_350 {
            return Err(format!(
                "FLAC keeps rates up to 655350 Hz, not {} Hz",
                spec.rate
            ));
        }
        Ok(Self {
            channels: spec.channels,
            bits,
            rate: spec.rate,
            most_lpc: if spec.rate > 48_000 { 12 } else { 8 },
            pending: Vec::new(),
            next: 0,
            md5: Md5::new(),
            md5_bytes: Vec::new(),
            samples: 0,
            min_frame: u32::MAX,
            max_frame: 0,
        })
    }

    /// Takes samples, and hands each whole stretch's frames to `frames`,
    /// with how many samples a channel each holds.
    pub fn push(
        &mut self,
        samples: &[i32],
        frames: &mut dyn FnMut(Vec<u8>, usize) -> Result<(), String>,
    ) -> Result<(), String> {
        // The samples as MD5 takes them: little-endian, in whole bytes.
        let width = self.bits.div_ceil(8) as usize;
        self.md5_bytes.clear();
        for &s in samples {
            self.md5_bytes.extend_from_slice(&s.to_le_bytes()[..width]);
        }
        self.md5.update(&self.md5_bytes);
        self.pending.extend_from_slice(samples);
        let stretch = BATCH * BLOCK * self.channels;
        while self.pending.len() >= stretch {
            let rest = self.pending.split_off(stretch);
            let batch = std::mem::replace(&mut self.pending, rest);
            self.encode(&batch, frames)?;
        }
        Ok(())
    }

    /// Encodes what is left, the last block as short as it is.
    pub fn finish(
        &mut self,
        frames: &mut dyn FnMut(Vec<u8>, usize) -> Result<(), String>,
    ) -> Result<(), String> {
        let batch = std::mem::take(&mut self.pending);
        self.encode(&batch, frames)
    }

    fn encode(
        &mut self,
        samples: &[i32],
        frames: &mut dyn FnMut(Vec<u8>, usize) -> Result<(), String>,
    ) -> Result<(), String> {
        let ch = self.channels;
        let first = self.next;
        let encoded: Vec<(Vec<u8>, usize)> = samples
            .par_chunks(BLOCK * ch)
            .enumerate()
            .map(|(i, block)| (self.frame(block, first + i as u64), block.len() / ch))
            .collect();
        for (frame, n) in encoded {
            self.next += 1;
            let size = frame.len() as u32;
            self.min_frame = self.min_frame.min(size);
            self.max_frame = self.max_frame.max(size);
            frames(frame, n)?;
        }
        self.samples += (samples.len() / ch) as u64;
        Ok(())
    }

    /// One block's frame: header, a subframe a channel, and the checksums.
    fn frame(&self, block: &[i32], number: u64) -> Vec<u8> {
        let ch = self.channels;
        let n = block.len() / ch;
        let channel =
            |c: usize| -> Vec<i32> { block.iter().skip(c).step_by(ch).copied().collect() };
        let mut chans: Vec<Vec<i32>> = (0..ch).map(channel).collect();
        let mut widths = vec![self.bits; ch];
        // Stereo as left and right, either beside their difference, or
        // their mean with it: whichever leaves least to code. A 32-bit
        // pair's difference would need 33 bits, which stay out of it.
        let mut assignment = ch as u64 - 1;
        if ch == 2 && self.bits < 32 && n > 4 {
            let (l, r) = (&chans[0], &chans[1]);
            let side: Vec<i32> = l.iter().zip(r).map(|(&a, &b)| a - b).collect();
            let mid: Vec<i32> = l.iter().zip(r).map(|(&a, &b)| (a + b) >> 1).collect();
            let (bl, br, bs, bm) = (
                rough_bits(l),
                rough_bits(r),
                rough_bits(&side),
                rough_bits(&mid),
            );
            let choices = [(1u64, bl + br), (8, bl + bs), (9, bs + br), (10, bm + bs)];
            let (pick, _) = choices
                .into_iter()
                .min_by_key(|&(_, b)| b)
                .expect("four choices");
            match pick {
                8 => {
                    chans[1] = side;
                    widths[1] += 1;
                }
                9 => {
                    chans[0] = side;
                    widths[0] += 1;
                }
                10 => {
                    chans = vec![mid, side];
                    widths[1] += 1;
                }
                _ => {}
            }
            assignment = pick;
        }
        let mut out = Bits::new();
        out.put(0xFFF8, 16);
        let (size_code, size_extra) = match n {
            192 => (1, None),
            576 | 1152 | 2304 | 4608 => (2 + (n / 576).trailing_zeros(), None),
            256 | 512 | 1024 | 2048 | 4096 | 8192 | 16384 | 32768 => {
                (8 + (n / 256).trailing_zeros(), None)
            }
            n if n <= 256 => (6, Some((n as u64 - 1, 8))),
            n => (7, Some((n as u64 - 1, 16))),
        };
        let (rate_code, rate_extra) = rate_code(self.rate);
        out.put(u64::from(size_code), 4);
        out.put(rate_code, 4);
        out.put(assignment, 4);
        let bits_code = match self.bits {
            8 => 1,
            12 => 2,
            16 => 4,
            20 => 5,
            24 => 6,
            32 => 7,
            _ => 0,
        };
        out.put(bits_code, 3);
        out.put(0, 1);
        for byte in coded_number(number) {
            out.put(u64::from(byte), 8);
        }
        if let Some((value, width)) = size_extra {
            out.put(value, width);
        }
        if let Some((value, width)) = rate_extra {
            out.put(value, width);
        }
        let crc = crc8(&out.bytes);
        out.put(u64::from(crc), 8);
        for (samples, width) in chans.iter().zip(&widths) {
            subframe(samples, *width, self.most_lpc, &mut out);
        }
        out.align();
        let crc = crc16(&out.bytes);
        out.put(u64::from(crc), 16);
        out.bytes
    }

    /// The STREAMINFO block's body, with what the frames written so far
    /// say: their sizes, the length, and the MD5 of the samples.
    pub fn streaminfo(&self) -> [u8; 34] {
        let block = if self.samples < BLOCK as u64 {
            (self.samples as u16).max(16)
        } else {
            BLOCK as u16
        };
        let mut b = Bits::new();
        b.put(u64::from(block), 16);
        b.put(u64::from(block), 16);
        let (min, max) = if self.max_frame == 0 {
            (0, 0)
        } else {
            (self.min_frame, self.max_frame)
        };
        b.put(u64::from(min), 24);
        b.put(u64::from(max), 24);
        b.put(u64::from(self.rate), 20);
        b.put(self.channels as u64 - 1, 3);
        b.put(u64::from(self.bits - 1), 5);
        b.put(self.samples >> 32, 4);
        b.put(self.samples & 0xFFFF_FFFF, 32);
        let mut info = [0; 34];
        info[..18].copy_from_slice(&b.bytes);
        info[18..].copy_from_slice(&self.md5_digest());
        info
    }

    fn md5_digest(&self) -> [u8; 16] {
        self.md5.clone().finish()
    }
}

/// A frame header's sample rate code, and the bits that follow it where
/// the code says to.
fn rate_code(rate: u32) -> (u64, Option<(u64, u32)>) {
    match rate {
        88_200 => (1, None),
        176_400 => (2, None),
        192_000 => (3, None),
        8_000 => (4, None),
        16_000 => (5, None),
        22_050 => (6, None),
        24_000 => (7, None),
        32_000 => (8, None),
        44_100 => (9, None),
        48_000 => (10, None),
        96_000 => (11, None),
        r if r % 1000 == 0 && r / 1000 <= 255 => (12, Some((u64::from(r / 1000), 8))),
        r if r <= 65_535 => (13, Some((u64::from(r), 16))),
        r if r % 10 == 0 && r / 10 <= 65_535 => (14, Some((u64::from(r / 10), 16))),
        // Where nothing else says it, STREAMINFO does.
        _ => (0, None),
    }
}

/// A frame's number as FLAC codes it, as UTF-8 codes a character.
fn coded_number(n: u64) -> Vec<u8> {
    if n < 0x80 {
        return vec![n as u8];
    }
    let bytes = match n {
        n if n < 0x800 => 2,
        n if n < 0x1_0000 => 3,
        n if n < 0x20_0000 => 4,
        n if n < 0x400_0000 => 5,
        n if n < 0x8000_0000 => 6,
        _ => 7,
    };
    let mut out = vec![0u8; bytes];
    let mut rest = n;
    for b in out[1..].iter_mut().rev() {
        *b = 0x80 | (rest & 0x3F) as u8;
        rest >>= 6;
    }
    let lead = !(0xFFu8 >> bytes);
    out[0] = lead | rest as u8;
    out
}

fn failed(e: io::Error) -> String {
    format!("cannot write the new file: {e}")
}

/// A metadata block's header: whether it is the last, its type, its size.
fn block_header(last: bool, kind: u8, size: usize) -> [u8; 4] {
    let size = size as u32;
    [
        u8::from(last) << 7 | kind,
        (size >> 16) as u8,
        (size >> 8) as u8,
        size as u8,
    ]
}

/// A FLAC file.
pub struct Writer {
    out: BufWriter<File>,
    frames: FrameEncoder,
    /// Where each foreign metadata block's body starts, and the blocks,
    /// whose sizes are filled in at the end.
    foreign: Vec<(u64, Vec<u8>)>,
    width: u64,
    channels: u64,
}

impl Writer {
    pub fn new(file: File, spec: Spec, carried: &Carried) -> Result<Self, String> {
        let frames = FrameEncoder::new(&spec)?;
        let mut head = b"fLaC".to_vec();
        head.extend_from_slice(&block_header(false, 0, 34));
        head.extend_from_slice(&[0; 34]);
        let mut foreign = Vec::new();
        for block in carried
            .flac_foreign(&pcm::fmt_chunk(&spec))
            .unwrap_or_default()
        {
            head.extend_from_slice(&block_header(false, 2, block.len()));
            foreign.push((head.len() as u64, block.clone()));
            head.extend_from_slice(&block);
        }
        let mut comments = carried.vorbis();
        comments.set_vendor(format!("soundcheck {}", env!("CARGO_PKG_VERSION")));
        let mut body = Vec::new();
        comments
            .dump_to(&mut body, WriteOptions::default())
            .map_err(|e| format!("the Vorbis comments cannot be written: {e}"))?;
        head.extend_from_slice(&block_header(false, 4, body.len()));
        head.extend_from_slice(&body);
        for picture in &carried.pictures {
            let info = PictureInformation::from_picture(picture).unwrap_or_default();
            let body = picture.as_flac_bytes(info, false);
            head.extend_from_slice(&block_header(false, 6, body.len()));
            head.extend_from_slice(&body);
        }
        head.extend_from_slice(&block_header(true, 1, PADDING));
        head.extend_from_slice(&[0; PADDING]);
        let mut out = BufWriter::with_capacity(1 << 20, file);
        out.write_all(&head).map_err(failed)?;
        let Sample::Int(bits) = spec.sample else {
            unreachable!("checked by the frame encoder")
        };
        Ok(Self {
            out,
            frames,
            foreign,
            width: u64::from(bits.div_ceil(8)),
            channels: spec.channels as u64,
        })
    }
}

impl Encode for Writer {
    fn push(&mut self, frames: Frames<'_>) -> Result<(), String> {
        let Frames::Int(samples) = frames else {
            return Err("FLAC keeps whole numbers only".into());
        };
        let out = &mut self.out;
        self.frames.push(samples, &mut |frame, _| {
            out.write_all(&frame).map_err(failed)
        })
    }

    fn finish(mut self: Box<Self>) -> Result<(), String> {
        let out = &mut self.out;
        self.frames
            .finish(&mut |frame, _| out.write_all(&frame).map_err(failed))?;
        let info = self.frames.streaminfo();
        let data = self.frames.samples * self.channels * self.width;
        let mut blocks: Vec<Vec<u8>> = self.foreign.iter().map(|(_, b)| b.clone()).collect();
        carry::riff_sizes(&mut blocks, data);
        (|| -> io::Result<()> {
            self.out.flush()?;
            let file = self.out.get_mut();
            file.seek(SeekFrom::Start(8))?;
            file.write_all(&info)?;
            for ((at, _), block) in self.foreign.iter().zip(&blocks) {
                file.seek(SeekFrom::Start(*at))?;
                file.write_all(block)?;
            }
            file.sync_all()
        })()
        .map_err(failed)
    }
}

/// The WAV chunks a FLAC keeps as foreign metadata, or `None` where it
/// keeps none.
pub fn foreign(path: &Path) -> Result<Option<Riff>, String> {
    let read = || -> io::Result<Vec<Vec<u8>>> {
        let mut file = File::open(path)?;
        let mut head = [0u8; 10];
        file.read_exact(&mut head)?;
        let mut at = 0u64;
        if &head[..3] == b"ID3" {
            let size = head[6..10]
                .iter()
                .fold(0u64, |size, &b| (size << 7) | u64::from(b & 0x7F));
            at = 10 + size + if head[5] & 0x10 != 0 { 10 } else { 0 };
        }
        file.seek(SeekFrom::Start(at))?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != b"fLaC" {
            return Ok(Vec::new());
        }
        let mut blocks = Vec::new();
        loop {
            let mut header = [0u8; 4];
            file.read_exact(&mut header)?;
            let size = u64::from(u32::from_be_bytes([0, header[1], header[2], header[3]]));
            if header[0] & 0x7F == 2 && size >= 4 {
                let mut body = vec![0; size as usize];
                file.read_exact(&mut body)?;
                blocks.push(body);
            } else {
                file.seek(SeekFrom::Current(size as i64))?;
            }
            if header[0] & 0x80 != 0 {
                return Ok(blocks);
            }
        }
    };
    let blocks = read().map_err(|e| format!("its metadata cannot be read: {e}"))?;
    Ok(carry::riff_from_blocks(&blocks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_numbers_are_coded_as_utf_8_codes_characters() {
        assert_eq!(coded_number(0x41), [0x41]);
        assert_eq!(coded_number(0xE9), "é".as_bytes());
        assert_eq!(coded_number(0x20AC), "€".as_bytes());
        assert_eq!(coded_number(0x1F600), "😀".as_bytes());
    }

    #[test]
    fn coefficients_quantize_into_the_precision_and_shift_flac_allows() {
        let (q, shift) = quantize(&[1.8, -0.9], 12).unwrap();
        assert_eq!(shift, 10);
        assert_eq!(q, [1843, -921]);
        assert!(quantize(&[0.0, 0.0], 12).is_none());
    }
}
