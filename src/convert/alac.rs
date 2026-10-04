//! ALAC, Apple Lossless, written with an encoder of our own, worked out as
//! the inverse of symphonia's decoder: Apple's adaptive predictor, its
//! mixing of a channel pair, and its adaptive Golomb coding, with the
//! parameters Apple's encoder uses. Any of ALAC's widths, 16, 20, 24 and
//! 32 bits, at any rate, in up to eight channels.

use super::flac::Bits;

/// Frames a packet, as Apple's encoder writes them.
pub const FRAME: usize = 4096;
/// The adaptive Golomb coder's starting mean, its rate of adaptation and
/// its largest parameter, as Apple's encoder sets them.
const MB0: u32 = 10;
const PB0: u32 = 40;
const KB0: u32 = 14;
const MAX_RUN: u16 = 255;
/// The predictor's coefficients are fixed point with this many fraction
/// bits.
const DEN_SHIFT: u32 = 9;
const COEFS: usize = 8;

const SCE: u64 = 0;
const CPE: u64 = 1;
const END: u64 = 7;

/// The elements a packet of `channels` channels is made of, each with the
/// channels it carries, in the order Apple's channel layouts give them:
/// for three channels, centre then left and right. A decoder puts element
/// channel `i` in output channel `MAP[channels][i]`, so element channel `i`
/// carries the file's channel of that number.
fn elements(channels: usize) -> Vec<Vec<usize>> {
    let map: &[usize] = match channels {
        1 => &[0],
        2 => &[0, 1],
        3 => &[2, 0, 1],
        4 => &[2, 0, 1, 3],
        5 => &[2, 0, 1, 3, 4],
        6 => &[2, 0, 1, 4, 5, 3],
        7 => &[2, 0, 1, 5, 6, 4, 3],
        _ => &[2, 4, 5, 0, 1, 6, 7, 3],
    };
    let shape: &[usize] = match channels {
        1 => &[1],
        2 => &[2],
        3 => &[1, 2],
        4 => &[1, 2, 1],
        5 => &[1, 2, 2],
        6 => &[1, 2, 2, 1],
        7 => &[1, 2, 2, 1, 1],
        _ => &[1, 2, 2, 2, 1],
    };
    let mut at = 0;
    shape
        .iter()
        .map(|&n| {
            let element = map[at..at + n].to_vec();
            at += n;
            element
        })
        .collect()
}

pub struct Encoder {
    bits: u32,
    channels: usize,
    rate: u32,
    /// Each element channel's coefficients, carried from packet to packet:
    /// what one packet's predictor ends with starts the next, as in
    /// Apple's encoder.
    coefs: Vec<[i32; COEFS]>,
    max_packet: u32,
    bytes: u64,
    pub frames: u64,
}

impl Encoder {
    pub fn new(bits: u32, channels: usize, rate: u32) -> Result<Self, String> {
        if ![16, 20, 24, 32].contains(&bits) || !(1..=8).contains(&channels) {
            return Err(format!(
                "ALAC keeps 16, 20, 24 or 32 bits in 1 to 8 channels, not {bits} bits in {channels}"
            ));
        }
        let start = {
            let den = 1i32 << DEN_SHIFT;
            let mut c = [0; COEFS];
            c[0] = (38 * den) >> 4;
            c[1] = (-29 * den) >> 4;
            c[2] = (-2 * den) >> 4;
            c
        };
        Ok(Self {
            bits,
            channels,
            rate,
            coefs: vec![start; channels],
            max_packet: 0,
            bytes: 0,
            frames: 0,
        })
    }

    pub fn bits(&self) -> u32 {
        self.bits
    }

    /// The ALAC magic cookie a container keeps for the decoder.
    pub fn cookie(&self) -> [u8; 24] {
        let seconds = self.frames as f64 / f64::from(self.rate.max(1));
        let rate = if seconds > 0.0 {
            (self.bytes as f64 * 8.0 / seconds) as u32
        } else {
            0
        };
        let mut c = [0; 24];
        c[..4].copy_from_slice(&(FRAME as u32).to_be_bytes());
        c[4] = 0;
        c[5] = self.bits as u8;
        c[6] = PB0 as u8;
        c[7] = MB0 as u8;
        c[8] = KB0 as u8;
        c[9] = self.channels as u8;
        c[10..12].copy_from_slice(&MAX_RUN.to_be_bytes());
        c[12..16].copy_from_slice(&self.max_packet.to_be_bytes());
        c[16..20].copy_from_slice(&rate.to_be_bytes());
        c[20..24].copy_from_slice(&self.rate.to_be_bytes());
        c
    }

    /// One packet of up to [`FRAME`] frames, interleaved.
    pub fn encode(&mut self, samples: &[i32]) -> Vec<u8> {
        let ch = self.channels;
        let n = samples.len() / ch;
        let mut out = Bits::new();
        let mut slot = 0;
        for (i, element) in elements(ch).into_iter().enumerate() {
            let chans: Vec<Vec<i32>> = element
                .iter()
                .map(|&c| samples.iter().skip(c).step_by(ch).copied().collect())
                .collect();
            let instance = elements(ch)[..i]
                .iter()
                .filter(|e| e.len() == element.len())
                .count() as u64;
            self.element(&mut out, instance, &chans, slot, n);
            slot += element.len();
        }
        out.put(END, 3);
        out.align();
        let packet = out.bytes;
        self.max_packet = self.max_packet.max(packet.len() as u32);
        self.bytes += packet.len() as u64;
        self.frames += n as u64;
        packet
    }

    /// One element: a channel, or a pair mixed as suits it best, each
    /// predicted and Golomb coded; or written plain where that is smaller.
    fn element(
        &mut self,
        out: &mut Bits,
        instance: u64,
        chans: &[Vec<i32>],
        slot: usize,
        n: usize,
    ) {
        let pair = chans.len() == 2;
        let partial = n != FRAME;
        // The low bytes of wide samples are noise to the predictor: they
        // go as they are, and only the rest is predicted.
        let bytes_shifted = match self.bits {
            24 => 1,
            32 => 2,
            _ => 0,
        };
        let shift = 8 * bytes_shifted;
        let mask = (1i64 << shift) - 1;
        let tops: Vec<Vec<i32>> = chans
            .iter()
            .map(|c| c.iter().map(|&s| s >> shift).collect())
            .collect();
        let width = self.bits - shift + u32::from(pair);
        let (mix_bits, mix_res, predicted) = if pair {
            let (l, r) = (&tops[0], &tops[1]);
            let rough = |s: &[i32]| -> u64 {
                s.windows(2)
                    .map(|w| (i64::from(w[1]) - i64::from(w[0])).unsigned_abs())
                    .sum()
            };
            let (res, u, v) = (0..=4)
                .map(|res| {
                    let (u, v) = mix(l, r, 2, res);
                    (res, u, v)
                })
                .min_by_key(|(_, u, v)| rough(u) + rough(v))
                .expect("five mixes");
            (2u64, res, vec![u, v])
        } else {
            (0, 0, tops.clone())
        };
        let mut header_coefs = Vec::new();
        let mut residuals = Vec::new();
        let mut fits = true;
        for (k, x) in predicted.iter().enumerate() {
            let mut coefs = self.coefs[slot + k];
            // A few passes over the start of the packet bring the carried
            // coefficients near what this packet wants before they are
            // written down; the decoder starts from what is written.
            let train = &x[..(n / 8).max(COEFS + 2).min(n)];
            let mut scratch = Vec::new();
            for _ in 0..2 {
                predict(train, &mut coefs, width, &mut scratch);
            }
            for c in &mut coefs {
                *c = (*c).clamp(i32::from(i16::MIN), i32::from(i16::MAX));
            }
            header_coefs.push(coefs);
            let mut residual = Vec::with_capacity(n);
            fits &= predict(x, &mut coefs, width, &mut residual);
            for c in &mut coefs {
                *c = (*c).clamp(i32::from(i16::MIN), i32::from(i16::MAX));
            }
            self.coefs[slot + k] = coefs;
            residuals.push(residual);
        }
        let mut coded = Bits::new();
        for residual in &residuals {
            golomb(&mut coded, residual, width);
        }
        let compressed = 16
            + if partial { 32 } else { 0 }
            + 16
            + chans.len() * (16 + 16 * COEFS)
            + n * chans.len() * shift as usize
            + 8 * coded.bytes.len()
            + 8;
        let plain = 16 + if partial { 32 } else { 0 } + n * chans.len() * self.bits as usize;
        out.put(if pair { CPE } else { SCE }, 3);
        out.put(instance, 4);
        out.put(0, 12);
        out.put(u64::from(partial), 1);
        if fits && compressed < plain {
            out.put(bytes_shifted as u64, 2);
            out.put(0, 1);
            if partial {
                out.put(n as u64, 32);
            }
            out.put(mix_bits, 8);
            out.put(mix_res as u8 as u64, 8);
            for coefs in &header_coefs {
                // Mode 0, the coefficients' fraction bits, Apple's factor
                // of 4 for the coder's adaptation, and the order.
                out.put(0, 4);
                out.put(u64::from(DEN_SHIFT), 4);
                out.put(4, 3);
                out.put(COEFS as u64, 5);
                for &c in coefs {
                    out.put(c as u16 as u64, 16);
                }
            }
            if shift > 0 {
                for i in 0..n {
                    for c in chans {
                        out.put((i64::from(c[i]) & mask) as u64, shift);
                    }
                }
            }
            for residual in &residuals {
                golomb(out, residual, width);
            }
        } else {
            out.put(0, 2);
            out.put(1, 1);
            if partial {
                out.put(n as u64, 32);
            }
            for i in 0..n {
                for c in chans {
                    out.put(c[i] as u32 as u64, self.bits);
                }
            }
        }
    }
}

/// A pair mixed as ALAC mixes it: with `res` of `2^bits` of the left in
/// the first, the rest the right, and the difference in the second. A
/// `res` of 0 leaves the two as they are.
fn mix(l: &[i32], r: &[i32], bits: u32, res: i32) -> (Vec<i32>, Vec<i32>) {
    if res == 0 {
        return (l.to_vec(), r.to_vec());
    }
    let m2 = (1 << bits) - res;
    l.iter()
        .zip(r)
        .map(|(&l, &r)| ((res * l + m2 * r) >> bits, l - r))
        .unzip()
}

fn clip(v: i32, bits: u32) -> i32 {
    let n = 32 - bits;
    (v << n) >> n
}

/// The residual of `x` under ALAC's adaptive predictor, starting from
/// `coefs`, which it adapts as the decoder will; samples `bits` wide.
/// False where a coefficient would leave the 16 bits Apple's decoder keeps
/// it in, where the two decoders would part.
fn predict(x: &[i32], coefs: &mut [i32; COEFS], bits: u32, out: &mut Vec<i32>) -> bool {
    out.clear();
    if x.is_empty() {
        return true;
    }
    out.push(x[0]);
    let order = COEFS;
    for i in 1..(order + 1).min(x.len()) {
        out.push(clip(x[i].wrapping_sub(x[i - 1]), bits));
    }
    let shift = DEN_SHIFT;
    let range = i32::from(i16::MIN)..=i32::from(i16::MAX);
    let mut fits = true;
    for i in order + 1..x.len() {
        let past0 = x[i - order - 1];
        let mut sum = 0i32;
        for (k, &c) in coefs.iter().enumerate() {
            sum = sum.wrapping_add(c.wrapping_mul(x[i - 1 - k].wrapping_sub(past0)));
        }
        let val = sum.wrapping_add((1 << shift) >> 1) >> shift;
        let r = clip(x[i].wrapping_sub(past0).wrapping_sub(val), bits);
        out.push(r);
        let mut res = r;
        if res > 0 {
            for j in 0..order {
                let k = order - 1 - j;
                let v = past0.wrapping_sub(x[i - order + j]);
                let sign = v.signum();
                coefs[k] -= sign;
                res -= (1 + j as i32) * (sign.wrapping_mul(v) >> shift);
                if res <= 0 {
                    break;
                }
            }
        } else if res < 0 {
            for j in 0..order {
                let k = order - 1 - j;
                let v = past0.wrapping_sub(x[i - order + j]);
                let sign = v.signum();
                coefs[k] += sign;
                res -= (1 + j as i32) * ((-sign).wrapping_mul(v) >> shift);
                if res >= 0 {
                    break;
                }
            }
        }
        fits &= coefs.iter().all(|c| range.contains(c));
    }
    fits
}

fn lg3a(mb: u32) -> u32 {
    31 - ((mb >> 9) + 3).leading_zeros()
}

/// `value` as ALAC's Golomb code with parameter `k` writes it: a unary
/// part then `k` bits, one fewer where the remainder is 0; or past a
/// unary part of 9, the value itself in `bits` bits.
fn code(out: &mut Bits, value: u32, k: u32, bits: u32) {
    let m = (1u32 << k) - 1;
    let unary = if k > 1 { value / m } else { value };
    if unary >= 9 {
        out.put(0x1FF, 9);
        out.put(u64::from(value), bits);
        return;
    }
    out.put(((1u64 << unary) - 1) << 1, unary + 1);
    if k > 1 {
        let rest = value - unary * m;
        if rest == 0 {
            out.put(0, k - 1);
        } else {
            out.put(u64::from(rest + 1), k);
        }
    }
}

/// A channel's residual, Golomb coded with the parameter adapting as it
/// goes, and runs of zeros counted rather than coded one by one.
fn golomb(out: &mut Bits, residual: &[i32], bits: u32) {
    let pb = (4 * PB0) >> 2;
    let n = residual.len();
    let mut mb = MB0;
    let mut toggle = 0;
    let mut i = 0;
    while i < n {
        let k = lg3a(mb).min(KB0);
        let r = residual[i];
        let value = ((r << 1) ^ (r >> 31)) as u32;
        code(out, value - toggle, k, bits);
        mb = if value > 0xffff {
            0xffff
        } else {
            mb.wrapping_add(pb * value)
                .wrapping_sub(pb.wrapping_mul(mb) >> 9)
        };
        toggle = 0;
        i += 1;
        if mb < 128 && i < n {
            let zeros = residual[i..]
                .iter()
                .take(0xffff)
                .take_while(|&&r| r == 0)
                .count() as u32;
            let k = (mb.leading_zeros() - 24 + ((mb + 16) >> 6)).min(KB0);
            code(out, zeros, k, 16);
            if zeros < 0xffff {
                toggle = 1;
            }
            mb = 0;
            i += zeros as usize;
        }
    }
}
