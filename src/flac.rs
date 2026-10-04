//! FLAC, read with a decoder of our own.
//!
//! symphonia's reader finds where each frame of a FLAC file ends before its
//! decoder gets the frame, going over every byte an extra time and copying
//! it twice, which takes as long as the decoding. Here decoding a frame
//! finds its end, so each byte is read once, straight from the file. It
//! also reads the 33-bit difference of a 32-bit stereo pair, which
//! symphonia 0.6.1 reads wrong.

use std::fs::File;
use std::io;
use std::ops::Range;
use std::path::Path;

/// What a file's STREAMINFO block says, and where its frames are.
#[derive(Clone, Debug)]
pub struct Stream {
    pub sample_rate: u32,
    pub channels: usize,
    pub bits: u32,
    /// Samples in each channel, or 0 where the encoder did not know.
    pub samples: u64,
    min_block: u32,
    max_block: u32,
    /// The largest frame in bytes, or 0 where the encoder did not know.
    max_frame: u32,
    /// From the first frame to the end of the file.
    frames: Range<u64>,
}

impl Stream {
    /// The stream of a FLAC file, or `None` for any other kind of file, or
    /// one this decoder does not take.
    pub fn read(file: &File) -> io::Result<Option<Self>> {
        let len = file.metadata()?.len();
        let mut at = 0;
        let mut head = [0; 10];
        if !read_exact_at(file, &mut head, 0)? {
            return Ok(None);
        }
        // Some taggers put an ID3v2 tag in front.
        if &head[..3] == b"ID3" {
            let size = head[6..10]
                .iter()
                .fold(0u64, |size, &b| (size << 7) | u64::from(b & 0x7F));
            let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
            at = 10 + size + footer;
        }
        let mut magic = [0; 4];
        if !read_exact_at(file, &mut magic, at)? || &magic != b"fLaC" {
            return Ok(None);
        }
        at += 4;
        let mut info = None;
        loop {
            let mut block = [0; 4];
            if !read_exact_at(file, &mut block, at)? {
                return Ok(None);
            }
            let size = u64::from(u32::from_be_bytes([0, block[1], block[2], block[3]]));
            if block[0] & 0x7F == 0 && size >= 34 {
                let mut body = [0; 34];
                if !read_exact_at(file, &mut body, at + 4)? {
                    return Ok(None);
                }
                info = Some(body);
            }
            at += 4 + size;
            if block[0] & 0x80 != 0 {
                break;
            }
        }
        let Some(body) = info else { return Ok(None) };
        let packed = u64::from_be_bytes(body[10..18].try_into().expect("eight bytes"));
        let stream = Self {
            min_block: u32::from(u16::from_be_bytes([body[0], body[1]])),
            max_block: u32::from(u16::from_be_bytes([body[2], body[3]])),
            max_frame: u32::from_be_bytes([0, body[7], body[8], body[9]]),
            sample_rate: (packed >> 44) as u32,
            channels: ((packed >> 41) & 7) as usize + 1,
            bits: ((packed >> 36) & 31) as u32 + 1,
            samples: packed & ((1 << 36) - 1),
            frames: at..len,
        };
        let usable = stream.sample_rate > 0
            && (4..=32).contains(&stream.bits)
            && stream.max_block >= 16
            && stream.min_block <= stream.max_block;
        Ok(usable.then_some(stream))
    }
}

/// Fills `buf` from `offset` on, or says the file ended first.
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<bool> {
    let mut done = 0;
    while done < buf.len() {
        match read_at(file, &mut buf[done..], offset + done as u64) {
            Ok(0) => return Ok(false),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

/// How a frame's channels are coded: each on its own, or a pair as one of
/// them and their difference.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    Apart(usize),
    LeftSide,
    SideRight,
    MidSide,
}

#[derive(Debug)]
struct Header {
    /// The frame's first sample.
    first: u64,
    block: usize,
    layout: Layout,
    /// Bytes up to and including the header's checksum.
    len: usize,
}

/// The header of the frame at the start of `bytes`, if it holds one of
/// `stream`.
fn header(stream: &Stream, bytes: &[u8]) -> Option<Header> {
    if bytes.len() < 6 || bytes[0] != 0xFF || bytes[1] & 0xFE != 0xF8 {
        return None;
    }
    let by_sample = bytes[1] & 1 == 1;
    let (block_code, rate_code) = (bytes[2] >> 4, bytes[2] & 0x0F);
    let (layout_code, bits_code) = (bytes[3] >> 4, (bytes[3] >> 1) & 7);
    if bytes[3] & 1 != 0 || block_code == 0 || rate_code == 15 || layout_code > 10 {
        return None;
    }
    // The frame or sample number, coded as UTF-8 codes characters.
    let lead = bytes[4];
    let length = match lead.leading_ones() {
        0 => 1,
        n @ 2..=7 => n as usize,
        _ => return None,
    };
    if length == 7 && !by_sample {
        return None;
    }
    let mut number = if length == 1 {
        u64::from(lead)
    } else {
        u64::from(lead & (0x7F >> length))
    };
    let mut at = 5;
    for _ in 1..length {
        let &byte = bytes.get(at)?;
        if byte & 0xC0 != 0x80 {
            return None;
        }
        number = (number << 6) | u64::from(byte & 0x3F);
        at += 1;
    }
    let mut take = |width: usize| -> Option<u32> {
        let field = bytes.get(at..at + width)?;
        at += width;
        Some(field.iter().fold(0, |v, &b| (v << 8) | u32::from(b)))
    };
    let block = match block_code {
        1 => 192,
        2..=5 => 576 << (block_code - 2),
        6 => take(1)? + 1,
        7 => take(2)? + 1,
        _ => 256 << (block_code - 8),
    };
    let rate = match rate_code {
        0 => stream.sample_rate,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => take(1)? * 1000,
        13 => take(2)?,
        _ => take(2)? * 10,
    };
    let bits = match bits_code {
        0 => stream.bits,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => return None,
    };
    let &check = bytes.get(at)?;
    if crc8(&bytes[..at]) != check {
        return None;
    }
    let layout = match layout_code {
        8 => Layout::LeftSide,
        9 => Layout::SideRight,
        10 => Layout::MidSide,
        n => Layout::Apart(usize::from(n) + 1),
    };
    let channels = match layout {
        Layout::Apart(n) => n,
        _ => 2,
    };
    // A frame unlike the stream it is in is no frame of it.
    if rate != stream.sample_rate
        || bits != stream.bits
        || channels != stream.channels
        || block > stream.max_block
    {
        return None;
    }
    let first = if by_sample {
        number
    } else if stream.min_block == stream.max_block {
        number * u64::from(stream.min_block)
    } else {
        number * u64::from(block)
    };
    Some(Header {
        first,
        block: block as usize,
        layout,
        len: at + 1,
    })
}

/// Why a frame could not be read.
#[derive(Debug, PartialEq)]
pub enum Bad {
    /// It runs past the bytes given.
    Short,
    /// No frame of the stream starts there, or it is damaged.
    Invalid,
}

/// A frame decoded.
#[derive(Debug)]
pub struct Frame {
    /// Its first sample.
    pub first: u64,
    pub samples: usize,
    /// Its length in the file.
    pub bytes: usize,
}

/// Decodes frames, with room for one frame's channels.
#[derive(Default)]
pub struct Decoder {
    planes: Vec<Vec<i32>>,
    /// The difference of a 32-bit pair, which takes 33 bits.
    wide: Vec<i64>,
}

impl Decoder {
    /// Decodes the frame at the start of `bytes` into `out`, interleaved,
    /// as the samples symphonia gives: full scale is 1.
    pub fn frame(
        &mut self,
        stream: &Stream,
        bytes: &[u8],
        out: &mut Vec<f32>,
    ) -> Result<Frame, Bad> {
        #[cfg(target_arch = "x86_64")]
        if crate::cpu::has_v3() {
            // SAFETY: the processor runs the v3 copy, as just asked.
            return unsafe { self.frame_v3(stream, bytes, out) };
        }
        self.frame_inner(stream, bytes, out)
    }

    crate::cpu::v3! {
        fn frame_v3(
            &mut self,
            stream: &Stream,
            bytes: &[u8],
            out: &mut Vec<f32>,
        ) -> Result<Frame, Bad> {
            self.frame_inner(stream, bytes, out)
        }
    }

    #[inline(always)]
    fn frame_inner(
        &mut self,
        stream: &Stream,
        bytes: &[u8],
        out: &mut Vec<f32>,
    ) -> Result<Frame, Bad> {
        let header = header(stream, bytes).ok_or(if bytes.len() < 16 {
            Bad::Short
        } else {
            Bad::Invalid
        })?;
        let n = header.block;
        self.planes.resize_with(stream.channels, Vec::new);
        let mut bits = Bits::new(&bytes[header.len..]);
        let mut restores = [Restore::NOTHING; 8];
        let mut wide = false;
        for (c, (plane, restore)) in self.planes.iter_mut().zip(&mut restores).enumerate() {
            plane.resize(n, 0);
            // The difference of a pair takes a bit more than either.
            let side = matches!(
                (header.layout, c),
                (Layout::LeftSide | Layout::MidSide, 1) | (Layout::SideRight, 0)
            );
            let sample_bits = stream.bits + u32::from(side);
            if sample_bits > 32 {
                self.wide.resize(n, 0);
                wide_subframe(&mut bits, plane, &mut self.wide)?;
                wide = true;
            } else {
                *restore = subframe(&mut bits, sample_bits, plane)?;
            }
        }
        let used = header.len + bits.consumed().div_ceil(8);
        let footer = bytes.get(used..used + 2).ok_or(Bad::Short)?;
        if bits.overrun() {
            return Err(Bad::Short);
        }
        if crc16(&bytes[..used]) != u16::from_be_bytes([footer[0], footer[1]]) {
            return Err(Bad::Invalid);
        }
        for (planes, restores) in self.planes.chunks_mut(2).zip(restores.chunks(2)) {
            match (planes, restores) {
                ([a, b], [ra, rb]) => restore_pair((ra, a), (rb, b)),
                (planes, restores) => {
                    for (plane, restore) in planes.iter_mut().zip(restores) {
                        restore.apply(plane);
                    }
                }
            }
        }
        let scale = 1.0 / (1u64 << (stream.bits - 1)) as f32;
        let ch = stream.channels;
        // Every sample is written below, so only a longer frame than any
        // before needs new room.
        out.resize(n * ch, 0.0);
        if wide {
            let (a, b, w) = (&self.planes[0][..n], &self.planes[1][..n], &self.wide[..n]);
            for (i, frame) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let (left, right) = match header.layout {
                    Layout::LeftSide => (i64::from(a[i]), i64::from(a[i]) - w[i]),
                    Layout::SideRight => (w[i] + i64::from(b[i]), i64::from(b[i])),
                    _ => {
                        let mid = (i64::from(a[i]) << 1) | (w[i] & 1);
                        ((mid + w[i]) >> 1, (mid - w[i]) >> 1)
                    }
                };
                frame[0] = left as i32 as f32 * scale;
                frame[1] = right as i32 as f32 * scale;
            }
            return Ok(Frame {
                first: header.first,
                samples: n,
                bytes: used + 2,
            });
        }
        match header.layout {
            Layout::Apart(1) => {
                for (o, &s) in out.iter_mut().zip(&self.planes[0]) {
                    *o = s as f32 * scale;
                }
            }
            Layout::Apart(_) => {
                for (c, plane) in self.planes.iter().enumerate() {
                    for (frame, &s) in out.chunks_exact_mut(ch).zip(plane) {
                        frame[c] = s as f32 * scale;
                    }
                }
            }
            layout => {
                let (a, b) = (&self.planes[0][..n], &self.planes[1][..n]);
                let out = &mut out[..2 * n];
                // A loop for each, which the compiler can take many samples
                // at a time through.
                match layout {
                    Layout::LeftSide => {
                        for i in 0..n {
                            out[2 * i] = a[i] as f32 * scale;
                            out[2 * i + 1] = a[i].wrapping_sub(b[i]) as f32 * scale;
                        }
                    }
                    Layout::SideRight => {
                        for i in 0..n {
                            out[2 * i] = a[i].wrapping_add(b[i]) as f32 * scale;
                            out[2 * i + 1] = b[i] as f32 * scale;
                        }
                    }
                    _ => {
                        for i in 0..n {
                            let mid = (a[i] << 1) | (b[i] & 1);
                            out[2 * i] = (mid.wrapping_add(b[i]) >> 1) as f32 * scale;
                            out[2 * i + 1] = (mid.wrapping_sub(b[i]) >> 1) as f32 * scale;
                        }
                    }
                }
            }
        }
        Ok(Frame {
            first: header.first,
            samples: n,
            bytes: used + 2,
        })
    }
}

/// What is left to do to a subframe once it is read: its prediction added
/// to the residual, and the low bits every sample lacked put back.
#[derive(Clone, Copy)]
struct Restore {
    predict: Predict,
    wasted: u32,
}

#[derive(Clone, Copy)]
enum Predict {
    Nothing,
    Fixed(usize),
    Lpc {
        coefs: [i32; 32],
        order: usize,
        shift: u32,
    },
}

impl Restore {
    const NOTHING: Self = Self {
        predict: Predict::Nothing,
        wasted: 0,
    };

    fn apply(&self, out: &mut [i32]) {
        match self.predict {
            Predict::Nothing => {}
            Predict::Fixed(order) => fixed(order, out),
            Predict::Lpc {
                coefs,
                order,
                shift,
            } => lpc(&coefs[..order], shift, out),
        }
        self.unwaste(out);
    }

    fn unwaste(&self, out: &mut [i32]) {
        if self.wasted > 0 {
            for s in out.iter_mut() {
                *s = s.wrapping_shl(self.wasted);
            }
        }
    }
}

/// Restores two channels of a frame, side by side where both are linearly
/// predicted: each sample waits on the one before, so the processor works
/// on one channel's while it waits on the other's.
fn restore_pair((a, x): (&Restore, &mut [i32]), (b, y): (&Restore, &mut [i32])) {
    let (
        Predict::Lpc {
            coefs: ca,
            order: oa,
            shift: sa,
        },
        Predict::Lpc {
            coefs: cb,
            order: ob,
            shift: sb,
        },
    ) = (a.predict, b.predict)
    else {
        a.apply(x);
        b.apply(y);
        return;
    };
    // The lower order is predicted on its own up to where the higher one's
    // samples start, and past that as that order with its coefficients
    // reaching further back as zeros.
    macro_rules! orders {
        ($($n:literal)*) => {
            match oa.max(ob) {
                $($n => {
                    lpc(&ca[..oa], sa, &mut x[..$n]);
                    lpc(&cb[..ob], sb, &mut y[..$n]);
                    let (ca, cb) = (ca[..$n].try_into().expect("order"), cb[..$n].try_into().expect("order"));
                    lpc_pair::<$n>((ca, sa, x), (cb, sb, y));
                })*
                _ => {
                    lpc(&ca[..oa], sa, x);
                    lpc(&cb[..ob], sb, y);
                }
            }
        };
    }
    orders!(1 2 3 4 5 6 7 8 9 10 11 12 16 20 24 32);
    a.unwaste(x);
    b.unwaste(y);
}

/// [`lpc_of`] for two channels at once, both from sample `N` on.
fn lpc_pair<const N: usize>(
    (ca, sa, x): (&[i32; N], u32, &mut [i32]),
    (cb, sb, y): (&[i32; N], u32, &mut [i32]),
) {
    if x.len() <= N || y.len() <= N {
        return;
    }
    // Copied, or the compiler reads them again after every sample written,
    // not knowing they are not among the samples.
    let (ca, cb) = (*ca, *cb);
    let mut ra: [i32; N] = std::array::from_fn(|j| x[N - 1 - j]);
    let mut rb: [i32; N] = std::array::from_fn(|j| y[N - 1 - j]);
    for (p, q) in x[N..].iter_mut().zip(&mut y[N..]) {
        let (mut sum_a, mut sum_b) = (0i64, 0i64);
        // The newest sample last, so the older ones are summed while the
        // sample before is still being worked out.
        for j in (0..N).rev() {
            sum_a += i64::from(ca[j]) * i64::from(ra[j]);
            sum_b += i64::from(cb[j]) * i64::from(rb[j]);
        }
        let (a, b) = (
            p.wrapping_add((sum_a >> sa) as i32),
            q.wrapping_add((sum_b >> sb) as i32),
        );
        (*p, *q) = (a, b);
        for j in (1..N).rev() {
            ra[j] = ra[j - 1];
            rb[j] = rb[j - 1];
        }
        (ra[0], rb[0]) = (a, b);
    }
}

/// Reads one channel's subframe of `sample_bits` bits a sample into `out`:
/// the samples, or the warm-up samples and the residual, with what is left
/// to do to them.
fn subframe(bits: &mut Bits, sample_bits: u32, out: &mut [i32]) -> Result<Restore, Bad> {
    if sample_bits > 32 || bits.read(1) != 0 {
        return Err(Bad::Invalid);
    }
    let kind = bits.read(6);
    let wasted = if bits.read(1) == 1 {
        bits.unary() + 1
    } else {
        0
    };
    if wasted >= sample_bits {
        return Err(Bad::Invalid);
    }
    let width = sample_bits - wasted;
    let predict = match kind {
        0 => {
            let value = bits.signed(width);
            out.fill(value);
            Predict::Nothing
        }
        1 => {
            for s in out.iter_mut() {
                *s = bits.signed(width);
            }
            Predict::Nothing
        }
        8..=12 => {
            let order = (kind - 8) as usize;
            if order > out.len() {
                return Err(Bad::Invalid);
            }
            for s in &mut out[..order] {
                *s = bits.signed(width);
            }
            residual(bits, order, out)?;
            Predict::Fixed(order)
        }
        32..=63 => {
            let order = (kind - 31) as usize;
            if order > out.len() {
                return Err(Bad::Invalid);
            }
            for s in &mut out[..order] {
                *s = bits.signed(width);
            }
            let precision = bits.read(4) + 1;
            let shift = bits.signed(5);
            if precision > 15 || shift < 0 {
                return Err(Bad::Invalid);
            }
            let mut coefs = [0i32; 32];
            for c in &mut coefs[..order] {
                *c = bits.signed(precision);
            }
            residual(bits, order, out)?;
            Predict::Lpc {
                coefs,
                order,
                shift: shift as u32,
            }
        }
        _ => return Err(Bad::Invalid),
    };
    if bits.overrun() {
        return Err(Bad::Short);
    }
    Ok(Restore { predict, wasted })
}

/// [`subframe`] for the difference of a 32-bit pair, 33 bits a sample:
/// into `out`, in 64 bits, with `residual_room` for its residual. Rare
/// enough to be predicted as it is read.
fn wide_subframe(bits: &mut Bits, residual_room: &mut [i32], out: &mut [i64]) -> Result<(), Bad> {
    if bits.read(1) != 0 {
        return Err(Bad::Invalid);
    }
    let kind = bits.read(6);
    let wasted = if bits.read(1) == 1 {
        bits.unary() + 1
    } else {
        0
    };
    if wasted >= 33 {
        return Err(Bad::Invalid);
    }
    let width = 33 - wasted;
    match kind {
        0 => {
            let value = bits.wide(width);
            out.fill(value);
        }
        1 => {
            for s in out.iter_mut() {
                *s = bits.wide(width);
            }
        }
        8..=12 | 32..=63 => {
            let order = if kind <= 12 { kind - 8 } else { kind - 31 } as usize;
            if order > out.len() {
                return Err(Bad::Invalid);
            }
            for s in &mut out[..order] {
                *s = bits.wide(width);
            }
            let mut coefs = [0i64; 32];
            let shift = if kind <= 12 {
                let fixed: &[i64] = [&[][..], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]][order];
                coefs[..order].copy_from_slice(fixed);
                0
            } else {
                let precision = bits.read(4) + 1;
                let shift = bits.signed(5);
                if precision > 15 || shift < 0 {
                    return Err(Bad::Invalid);
                }
                for c in &mut coefs[..order] {
                    *c = i64::from(bits.signed(precision));
                }
                shift as u32
            };
            residual(bits, order, residual_room)?;
            for i in order..out.len() {
                let predicted: i64 = coefs[..order]
                    .iter()
                    .zip(out[i - order..i].iter().rev())
                    .map(|(c, s)| c * s)
                    .sum();
                out[i] = i64::from(residual_room[i]) + (predicted >> shift);
            }
        }
        _ => return Err(Bad::Invalid),
    }
    if wasted > 0 {
        for s in out.iter_mut() {
            *s <<= wasted;
        }
    }
    if bits.overrun() {
        return Err(Bad::Short);
    }
    Ok(())
}

/// Reads the residual after `order` warm-up samples into the rest of `out`.
fn residual(bits: &mut Bits, order: usize, out: &mut [i32]) -> Result<(), Bad> {
    #[cfg(target_arch = "x86_64")]
    if crate::cpu::has_v3() {
        // SAFETY: the processor runs the v3 copy, as just asked.
        return unsafe { residual_v3(bits, order, out) };
    }
    residual_inner(bits, order, out)
}

crate::cpu::v3! {
    fn residual_v3(bits: &mut Bits, order: usize, out: &mut [i32]) -> Result<(), Bad> {
        residual_inner(bits, order, out)
    }
}

#[inline(always)]
fn residual_inner(bits: &mut Bits, order: usize, out: &mut [i32]) -> Result<(), Bad> {
    let width = match bits.read(2) {
        0 => 4,
        1 => 5,
        _ => return Err(Bad::Invalid),
    };
    let partitions = bits.read(4);
    let per = out.len() >> partitions;
    if per << partitions != out.len() || order > per {
        return Err(Bad::Invalid);
    }
    let escape = (1 << width) - 1;
    let mut start = order;
    for end in (1..=1usize << partitions).map(|p| p * per) {
        let k = bits.read(width);
        let part = &mut out[start..end];
        if k == escape {
            let plain = bits.read(5);
            for s in part.iter_mut() {
                *s = bits.signed(plain);
            }
        } else {
            bits.rice(k, part);
        }
        if bits.overrun() {
            return Err(Bad::Short);
        }
        start = end;
    }
    Ok(())
}

/// The fixed predictors, adding each prediction to the residual in `out`.
/// Samples wrap as the format has them do, so 32 bits are enough.
fn fixed(order: usize, out: &mut [i32]) {
    match order {
        0 => {}
        1 => fixed_of(out, |[a]| a),
        2 => fixed_of(out, |[a, b]| a.wrapping_mul(2).wrapping_sub(b)),
        3 => fixed_of(out, |[a, b, c]| {
            a.wrapping_sub(b).wrapping_mul(3).wrapping_add(c)
        }),
        _ => fixed_of(out, |[a, b, c, d]| {
            a.wrapping_add(c)
                .wrapping_mul(4)
                .wrapping_sub(b.wrapping_mul(6))
                .wrapping_sub(d)
        }),
    }
}

/// A fixed predictor of the last `N` samples, newest first, kept in
/// registers as [`lpc_of`] keeps them.
fn fixed_of<const N: usize>(out: &mut [i32], predict: impl Fn([i32; N]) -> i32) {
    if out.len() <= N {
        return;
    }
    let mut recent: [i32; N] = std::array::from_fn(|j| out[N - 1 - j]);
    for x in &mut out[N..] {
        let sample = x.wrapping_add(predict(recent));
        *x = sample;
        for j in (1..N).rev() {
            recent[j] = recent[j - 1];
        }
        recent[0] = sample;
    }
}

/// Linear prediction with `coefs`, adding each prediction to the residual
/// in `out`. Common orders get a loop of their own, which the compiler
/// unrolls.
fn lpc(coefs: &[i32], shift: u32, out: &mut [i32]) {
    #[cfg(target_arch = "x86_64")]
    if crate::cpu::has_v3() {
        // SAFETY: the processor runs the v3 copy, as just asked.
        return unsafe { lpc_v3(coefs, shift, out) };
    }
    lpc_inner(coefs, shift, out)
}

crate::cpu::v3! {
    fn lpc_v3(coefs: &[i32], shift: u32, out: &mut [i32]) {
        lpc_inner(coefs, shift, out)
    }
}

#[inline(always)]
fn lpc_inner(coefs: &[i32], shift: u32, out: &mut [i32]) {
    macro_rules! orders {
        ($($n:literal)*) => {
            match coefs.len() {
                $($n => lpc_of::<$n>(coefs.try_into().expect("order"), shift, out),)*
                _ => lpc_any(coefs, shift, out),
            }
        };
    }
    orders!(1 2 3 4 5 6 7 8 9 10 11 12 16 20 24 32)
}

/// The last `N` samples stay in registers, newest first, so a sample does
/// not wait on the one before it going out to memory and back.
fn lpc_of<const N: usize>(coefs: &[i32; N], shift: u32, out: &mut [i32]) {
    if out.len() <= N {
        return;
    }
    let mut recent: [i32; N] = std::array::from_fn(|j| out[N - 1 - j]);
    for x in &mut out[N..] {
        let mut sum = 0i64;
        for j in (0..N).rev() {
            sum += i64::from(coefs[j]) * i64::from(recent[j]);
        }
        let sample = x.wrapping_add((sum >> shift) as i32);
        *x = sample;
        for j in (1..N).rev() {
            recent[j] = recent[j - 1];
        }
        recent[0] = sample;
    }
}

fn lpc_any(coefs: &[i32], shift: u32, out: &mut [i32]) {
    let order = coefs.len();
    for i in order..out.len() {
        let sum: i64 = coefs
            .iter()
            .zip(out[i - order..i].iter().rev())
            .map(|(&c, &s)| i64::from(c) * i64::from(s))
            .sum();
        out[i] = out[i].wrapping_add((sum >> shift) as i32);
    }
}

/// A frame's bits, first bit first.
struct Bits<'a> {
    bytes: &'a [u8],
    /// The next byte to take into `word`.
    next: usize,
    /// Bits not yet read, from the top.
    word: u64,
    left: u32,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            next: 0,
            word: 0,
            left: 0,
        }
    }

    /// Takes in as many whole bytes as `word` has room for. Past the end of
    /// the bytes it takes zeros, and `overrun` says so.
    #[inline(always)]
    fn refill(&mut self) {
        if let Some(chunk) = self.bytes.get(self.next..self.next + 8) {
            let fresh = u64::from_be_bytes(chunk.try_into().expect("eight bytes"));
            self.word |= fresh >> self.left;
            let taken = (63 - self.left) / 8;
            self.next += taken as usize;
            self.left += taken * 8;
        } else {
            while self.left <= 56 {
                let byte = self.bytes.get(self.next).copied().unwrap_or(0);
                self.word |= u64::from(byte) << (56 - self.left);
                self.next += 1;
                self.left += 8;
            }
        }
    }

    /// Reads `n` bits, up to 32.
    #[inline(always)]
    fn read(&mut self, n: u32) -> u32 {
        if self.left < n {
            self.refill();
        }
        let value = ((self.word >> 32) >> (32 - n)) as u32;
        self.word = self.word.checked_shl(n).unwrap_or(0);
        self.left -= n;
        value
    }

    /// Reads `n` bits, up to 32, as a signed number.
    #[inline(always)]
    fn signed(&mut self, n: u32) -> i32 {
        let value = self.read(n);
        if n == 0 {
            0
        } else {
            ((value << (32 - n)) as i32) >> (32 - n)
        }
    }

    /// Reads `n` bits, up to 33, as a signed number.
    fn wide(&mut self, n: u32) -> i64 {
        if n <= 32 {
            return i64::from(self.signed(n));
        }
        let high = u64::from(self.read(n - 32));
        let value = (high << 32) | u64::from(self.read(32));
        ((value << (64 - n)) as i64) >> (64 - n)
    }

    /// Counts zeros up to the next one, and reads past it.
    #[inline(always)]
    fn unary(&mut self) -> u32 {
        let mut count = 0;
        loop {
            if self.left == 0 {
                self.refill();
                if self.overrun() {
                    return count;
                }
            }
            let zeros = self.word.leading_zeros();
            if zeros < self.left {
                self.word <<= zeros;
                self.word <<= 1;
                self.left -= zeros + 1;
                return count + zeros;
            }
            count += self.left;
            self.word = 0;
            self.left = 0;
        }
    }

    /// Reads Rice codes of parameter `k` into `out`.
    #[inline(always)]
    fn rice(&mut self, k: u32, out: &mut [i32]) {
        for s in out {
            if self.left < 32 {
                self.refill();
            }
            let zeros = self.word.leading_zeros();
            let value = if zeros + 1 + k <= self.left {
                // The whole code is in hand.
                let rest = self.word << zeros << 1;
                let low = ((rest >> 32) >> (32 - k)) as u32;
                self.word = rest << k;
                self.left -= zeros + 1 + k;
                (zeros << k) | low
            } else {
                let high = self.unary();
                (high << k) | self.read(k)
            };
            *s = ((value >> 1) as i32) ^ -((value & 1) as i32);
        }
    }

    /// Bits read so far.
    fn consumed(&self) -> usize {
        self.next * 8 - self.left as usize
    }

    /// Whether reading ran past the end of the bytes.
    fn overrun(&self) -> bool {
        self.consumed() > self.bytes.len() * 8
    }
}

const fn crc8_table() -> [u8; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

const CRC8: [u8; 256] = crc8_table();

pub(crate) fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |crc, &b| CRC8[usize::from(crc ^ b)])
}

/// For each of eight places, the checksum of a byte followed by that many
/// zero bytes, so eight bytes are checked in one step.
const fn crc16_tables() -> [[u16; 256]; 8] {
    let mut tables = [[0; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            bit += 1;
        }
        tables[0][i] = c;
        i += 1;
    }
    let mut t = 1;
    while t < 8 {
        let mut i = 0;
        while i < 256 {
            let before = tables[t - 1][i];
            tables[t][i] = (before << 8) ^ tables[0][(before >> 8) as usize];
            i += 1;
        }
        t += 1;
    }
    tables
}

const CRC16: [[u16; 256]; 8] = crc16_tables();

/// The checksum a frame ends with.
pub(crate) fn crc16(bytes: &[u8]) -> u16 {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("aes") {
        // SAFETY: the processor multiplies without carrying, as just asked.
        return unsafe { fold::crc16(bytes) };
    }
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("pclmulqdq") && is_x86_feature_detected!("ssse3") {
        // SAFETY: the processor multiplies without carrying, as just asked.
        return unsafe { fold::crc16(bytes) };
    }
    crc16_from(0, bytes)
}

/// The checksum of `bytes` after bytes whose checksum is `crc`, from the
/// tables.
fn crc16_from(mut crc: u16, bytes: &[u8]) -> u16 {
    let (eights, rest) = bytes.as_chunks::<8>();
    for b in eights {
        let x = crc ^ u16::from_be_bytes([b[0], b[1]]);
        crc = CRC16[7][usize::from(x >> 8)]
            ^ CRC16[6][usize::from(x & 0xFF)]
            ^ CRC16[5][usize::from(b[2])]
            ^ CRC16[4][usize::from(b[3])]
            ^ CRC16[3][usize::from(b[4])]
            ^ CRC16[2][usize::from(b[5])]
            ^ CRC16[1][usize::from(b[6])]
            ^ CRC16[0][usize::from(b[7])];
    }
    for &b in rest {
        crc = (crc << 8) ^ CRC16[0][usize::from((crc >> 8) as u8 ^ b)];
    }
    crc
}

/// [`crc16`] for processors that multiply without carrying, over ten times
/// as fast as the tables on Apple's. Each 16 bytes are read as a polynomial, the first
/// byte's top bit its highest power, and multiplied by x to the power of how
/// far they are from the end, modulo the checksum's polynomial: the sum of
/// them all is 16 bytes with the same checksum as all the bytes.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod fold {
    #[cfg(target_arch = "aarch64")]
    use std::arch::aarch64::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    use super::crc16_from;

    /// x^n modulo the checksum's polynomial.
    const fn power(n: u32) -> u64 {
        let mut r = 1u64;
        let mut i = 0;
        while i < n {
            r <<= 1;
            if r & 0x1_0000 != 0 {
                r ^= 0x1_8005;
            }
            i += 1;
        }
        r
    }

    /// What multiplies 16 bytes by x to the power of `blocks` times their
    /// bits: the factor for their low half, then for their high half.
    const fn along(blocks: u32) -> [u64; 2] {
        [power(128 * blocks), power(128 * blocks + 64)]
    }

    /// Blocks of 16 bytes folded side by side, so each multiplication runs
    /// while others finish.
    const LANES: usize = 4;
    const ALONG: [[u64; 2]; LANES + 1] = [along(0), along(1), along(2), along(3), along(4)];

    /// # Safety
    ///
    /// The processor has to multiply without carrying: AES on ARM,
    /// PCLMULQDQ and SSSE3 on x86.
    #[cfg_attr(target_arch = "aarch64", target_feature(enable = "aes"))]
    #[cfg_attr(target_arch = "x86_64", target_feature(enable = "pclmulqdq,ssse3"))]
    pub unsafe fn crc16(bytes: &[u8]) -> u16 {
        let (blocks, tail) = bytes.as_chunks::<16>();
        if blocks.len() < 2 * LANES {
            return crc16_from(0, bytes);
        }
        let mut lanes = [
            load(&blocks[0]),
            load(&blocks[1]),
            load(&blocks[2]),
            load(&blocks[3]),
        ];
        let whole = blocks.len() / LANES * LANES;
        let stride = factors(ALONG[LANES]);
        for group in blocks[LANES..whole].as_chunks::<LANES>().0 {
            for (lane, block) in lanes.iter_mut().zip(group) {
                *lane = xor(fold(*lane, stride), load(block));
            }
        }
        let mut sum = lanes[LANES - 1];
        for (i, &lane) in lanes[..LANES - 1].iter().enumerate() {
            sum = xor(sum, fold(lane, factors(ALONG[LANES - 1 - i])));
        }
        let next = factors(ALONG[1]);
        for block in &blocks[whole..] {
            sum = xor(fold(sum, next), load(block));
        }
        crc16_from(crc16_from(0, &bytes_of(sum)), tail)
    }

    #[cfg(target_arch = "aarch64")]
    type Block = uint8x16_t;

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "aes")]
    fn load(bytes: &[u8; 16]) -> Block {
        // SAFETY: the 16 bytes are there to read.
        let block = unsafe { vld1q_u8(bytes.as_ptr()) };
        let block = vrev64q_u8(block);
        vextq_u8::<8>(block, block)
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "aes")]
    fn bytes_of(block: Block) -> [u8; 16] {
        let block = vrev64q_u8(block);
        let mut bytes = [0; 16];
        // SAFETY: the 16 bytes are there to write.
        unsafe { vst1q_u8(bytes.as_mut_ptr(), vextq_u8::<8>(block, block)) };
        bytes
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "aes")]
    fn factors([low, high]: [u64; 2]) -> poly64x2_t {
        vcombine_p64(vcreate_p64(low), vcreate_p64(high))
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "aes")]
    fn fold(block: Block, factors: poly64x2_t) -> Block {
        let block = vreinterpretq_p64_u8(block);
        let low = vmull_p64(vgetq_lane_p64::<0>(block), vgetq_lane_p64::<0>(factors));
        let high = vmull_high_p64(block, factors);
        veorq_u8(vreinterpretq_u8_p128(low), vreinterpretq_u8_p128(high))
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "aes")]
    fn xor(a: Block, b: Block) -> Block {
        veorq_u8(a, b)
    }

    #[cfg(target_arch = "x86_64")]
    type Block = __m128i;

    /// Turns the bytes of a block around.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "pclmulqdq,ssse3")]
    fn reversed(block: Block) -> Block {
        _mm_shuffle_epi8(
            block,
            _mm_set_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15),
        )
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "pclmulqdq,ssse3")]
    fn load(bytes: &[u8; 16]) -> Block {
        // SAFETY: the 16 bytes are there to read.
        reversed(unsafe { _mm_loadu_si128(bytes.as_ptr().cast()) })
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "pclmulqdq,ssse3")]
    fn bytes_of(block: Block) -> [u8; 16] {
        let mut bytes = [0; 16];
        // SAFETY: the 16 bytes are there to write.
        unsafe { _mm_storeu_si128(bytes.as_mut_ptr().cast(), reversed(block)) };
        bytes
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "pclmulqdq,ssse3")]
    fn factors([low, high]: [u64; 2]) -> Block {
        _mm_set_epi64x(high as i64, low as i64)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "pclmulqdq,ssse3")]
    fn fold(block: Block, factors: Block) -> Block {
        _mm_xor_si128(
            _mm_clmulepi64_si128::<0x00>(block, factors),
            _mm_clmulepi64_si128::<0x11>(block, factors),
        )
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "pclmulqdq,ssse3")]
    fn xor(a: Block, b: Block) -> Block {
        _mm_xor_si128(a, b)
    }
}

/// Bytes read from the file at a time.
const CHUNK: usize = 1 << 20;

/// A FLAC file read frame by frame from any position.
pub struct Reader {
    file: File,
    stream: Stream,
    decoder: Decoder,
    /// Bytes of the file from `at` on, up to `end`; room past that is
    /// used again rather than cleared.
    bytes: Vec<u8>,
    end: usize,
    at: u64,
    /// Where in `bytes` the next frame starts.
    pos: usize,
    samples: Vec<f32>,
}

impl Reader {
    /// The file at `path`, or `None` if it is no FLAC file this decoder takes.
    pub fn open(path: &Path) -> io::Result<Option<Self>> {
        let file = File::open(path)?;
        let Some(stream) = Stream::read(&file)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            at: stream.frames.start,
            file,
            stream,
            decoder: Decoder::default(),
            bytes: Vec::new(),
            end: 0,
            pos: 0,
            samples: Vec::new(),
        }))
    }

    pub fn stream(&self) -> &Stream {
        &self.stream
    }

    /// Bytes a frame takes at most, as far as can be told.
    fn frame_room(&self) -> usize {
        let raw =
            self.stream.max_block as usize * self.stream.channels * (self.stream.bits as usize + 1)
                / 8;
        (self.stream.max_frame as usize).max(raw / 2).max(1 << 16) + 64
    }

    /// Starts reading afresh from byte `at` of the file.
    fn restart(&mut self, at: u64) {
        (self.at, self.pos, self.end) = (at, 0, 0);
    }

    /// Makes sure `bytes` holds at least `room` bytes from `pos` on, as far
    /// as the file goes, reading `chunk` at a time.
    fn fill(&mut self, room: usize, chunk: usize) -> io::Result<()> {
        if self.end - self.pos >= room {
            return Ok(());
        }
        self.bytes.copy_within(self.pos..self.end, 0);
        self.at += self.pos as u64;
        (self.end, self.pos) = (self.end - self.pos, 0);
        let size = room.max(chunk);
        if self.bytes.len() < size {
            self.bytes.resize(size, 0);
        }
        let from = self.at + self.end as u64;
        let want = (size - self.end).min(self.stream.frames.end.saturating_sub(from) as usize);
        if !read_exact_at(&self.file, &mut self.bytes[self.end..self.end + want], from)? {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.end += want;
        Ok(())
    }

    /// Whether the bytes in hand run to the end of the file.
    fn at_end(&self) -> bool {
        self.at + self.end as u64 >= self.stream.frames.end
    }

    /// The next frame: where it starts, and its samples interleaved. `None`
    /// at the end of the file.
    pub fn next(&mut self) -> io::Result<Option<(u64, &[f32])>> {
        let mut room = self.frame_room();
        loop {
            self.fill(room, CHUNK)?;
            if self.pos >= self.end {
                return Ok(None);
            }
            let bytes = &self.bytes[self.pos..self.end];
            match self.decoder.frame(&self.stream, bytes, &mut self.samples) {
                Ok(frame) => {
                    self.pos += frame.bytes;
                    return Ok(Some((frame.first, &self.samples)));
                }
                Err(Bad::Short) if !self.at_end() && room < 1 << 26 => room *= 2,
                // A damaged frame is left out, and reading goes on from the
                // next one.
                Err(_) => self.pos += 1 + sync_after(&bytes[1..]).unwrap_or(bytes.len() - 1),
            }
        }
    }

    /// Makes the next frame the one holding `sample`, or one before it.
    pub fn seek(&mut self, sample: u64) -> io::Result<()> {
        let room = self.frame_room() as u64;
        let (mut lo, mut lo_first) = (self.stream.frames.start, 0u64);
        let mut hi = self.stream.frames.end;
        let mut hi_first = self.stream.samples;
        let mut guesses = 0;
        while hi - lo > 2 * room {
            // Where the frames around it say the sample should be, for the
            // first few tries; halving the stretch after that.
            let guess = if guesses < 4 && hi_first > lo_first && sample >= lo_first {
                let share = (sample - lo_first) as f64 / (hi_first - lo_first) as f64;
                lo + ((hi - lo) as f64 * share.min(1.0)) as u64
            } else {
                lo + (hi - lo) / 2
            };
            let guess = guess.clamp(lo + 1, hi - room);
            guesses += 1;
            match self.frame_at(guess, hi)? {
                Some((at, first, samples)) if first <= sample => {
                    if sample < first + samples as u64 {
                        lo = at;
                        break;
                    }
                    (lo, lo_first) = (at, first);
                }
                Some((at, first, _)) => (hi, hi_first) = (at, first),
                None => hi = guess,
            }
        }
        self.restart(lo);
        Ok(())
    }

    /// The first whole frame starting from byte `from` on and before `to`:
    /// where it starts, its first sample and its length in samples.
    fn frame_at(&mut self, from: u64, to: u64) -> io::Result<Option<(u64, u64, usize)>> {
        // A frame and the start of the next is all a look needs.
        let room = match self.stream.max_frame {
            0 => self.frame_room(),
            largest => 2 * largest as usize + 64,
        };
        self.restart(from);
        loop {
            self.fill(room, room)?;
            if self.end == 0 || self.at + self.pos as u64 >= to {
                return Ok(None);
            }
            match sync_after(&self.bytes[self.pos..self.end]) {
                Some(skip) => {
                    self.pos += skip;
                    if self.at + self.pos as u64 >= to {
                        return Ok(None);
                    }
                    let start = self.at + self.pos as u64;
                    let bytes = &self.bytes[self.pos..self.end];
                    match self.decoder.frame(&self.stream, bytes, &mut self.samples) {
                        Ok(frame) => return Ok(Some((start, frame.first, frame.samples))),
                        Err(_) => self.pos += 1,
                    }
                }
                None => {
                    // Keep the last byte, a sync code might start there.
                    self.pos = self.end.saturating_sub(1).max(self.pos);
                    if self.at_end() {
                        return Ok(None);
                    }
                    self.fill(self.end - self.pos + room, room)?;
                }
            }
        }
    }
}

/// Where in `bytes` the next frame sync code starts.
fn sync_after(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(2)
        .position(|w| w[0] == 0xFF && w[1] & 0xFE == 0xF8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::temp_file;

    /// Bits put down first bit first, as the format reads them.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        bits: u32,
    }

    impl Writer {
        fn bit(&mut self, one: bool) {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if one {
                *self.bytes.last_mut().unwrap() |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }

        fn put(&mut self, value: u64, width: u32) {
            for i in (0..width).rev() {
                self.bit(value >> i & 1 == 1);
            }
        }

        fn signed(&mut self, value: i64, width: u32) {
            if width > 0 {
                self.put(value as u64 & (u64::MAX >> (64 - width)), width);
            }
        }

        fn unary(&mut self, zeros: u64) {
            for _ in 0..zeros {
                self.bit(false);
            }
            self.bit(true);
        }

        fn align(&mut self) {
            while !self.bits.is_multiple_of(8) {
                self.bit(false);
            }
        }
    }

    /// How a test codes a subframe.
    #[derive(Clone)]
    enum Kind {
        Constant,
        Verbatim,
        Fixed(usize),
        Lpc {
            coefs: Vec<i64>,
            precision: u32,
            shift: u32,
        },
    }

    #[derive(Clone)]
    struct Coding {
        kind: Kind,
        /// Low bits every sample lacks, which are left out.
        wasted: u32,
        /// Partitions of the residual, as a power of two.
        partitions: u32,
        /// Rice parameters of five bits rather than four.
        wide_parameters: bool,
        /// The first partition's residual written plainly.
        escape: bool,
    }

    fn coding(kind: Kind) -> Coding {
        Coding {
            kind,
            wasted: 0,
            partitions: 0,
            wide_parameters: false,
            escape: false,
        }
    }

    /// A predictor of `order` about as an encoder would find for a slow
    /// tone: mostly the sample before, a little of the others.
    fn lpc(order: usize, precision: u32, shift: u32) -> Kind {
        let unit = 1i64 << shift;
        let most = (1i64 << (precision - 1)) - 1;
        let coefs = (0..order)
            .map(|j| match j {
                0 => (unit * 9 / 10).min(most),
                j => (unit / 16 * if j % 2 == 0 { 1 } else { -1 } / j as i64).clamp(-most, most),
            })
            .collect();
        Kind::Lpc {
            coefs,
            precision,
            shift,
        }
    }

    const FIXED: [&[i64]; 5] = [&[], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]];

    /// `samples`, `bits` bits each, as a subframe.
    fn subframe(w: &mut Writer, samples: &[i64], bits: u32, coding: &Coding) {
        let wasted = coding.wasted;
        assert!(samples.iter().all(|v| v & ((1 << wasted) - 1) == 0));
        let s: Vec<i64> = samples.iter().map(|v| v >> wasted).collect();
        let width = bits - wasted;
        let (code, coefs, shift) = match &coding.kind {
            Kind::Constant => (0, vec![], 0),
            Kind::Verbatim => (1, vec![], 0),
            Kind::Fixed(order) => (8 + *order as u64, FIXED[*order].to_vec(), 0),
            Kind::Lpc { coefs, shift, .. } => (31 + coefs.len() as u64, coefs.clone(), *shift),
        };
        w.bit(false);
        w.put(code, 6);
        if wasted > 0 {
            w.bit(true);
            w.unary(u64::from(wasted - 1));
        } else {
            w.bit(false);
        }
        match &coding.kind {
            Kind::Constant => w.signed(s[0], width),
            Kind::Verbatim => s.iter().for_each(|&v| w.signed(v, width)),
            kind => {
                let order = coefs.len();
                s[..order].iter().for_each(|&v| w.signed(v, width));
                if let Kind::Lpc {
                    coefs,
                    precision,
                    shift,
                } = kind
                {
                    w.put(u64::from(precision - 1), 4);
                    w.signed(i64::from(*shift), 5);
                    coefs.iter().for_each(|&c| w.signed(c, *precision));
                }
                let residual: Vec<i64> = (order..s.len())
                    .map(|i| {
                        let predicted: i64 = coefs
                            .iter()
                            .enumerate()
                            .map(|(j, c)| c * s[i - 1 - j])
                            .sum();
                        s[i] - (predicted >> shift)
                    })
                    .collect();
                assert!(
                    residual.iter().all(|&r| i32::try_from(r).is_ok()),
                    "a residual too big to code"
                );
                rice(w, &residual, order, s.len(), coding);
            }
        }
    }

    /// The residual of a subframe of `len` samples, the first `order` of
    /// them its warm-up.
    fn rice(w: &mut Writer, residual: &[i64], order: usize, len: usize, coding: &Coding) {
        w.put(u64::from(coding.wide_parameters), 2);
        w.put(u64::from(coding.partitions), 4);
        let per = len >> coding.partitions;
        let (width, escape): (u32, u32) = if coding.wide_parameters {
            (5, 31)
        } else {
            (4, 15)
        };
        let mut at = 0;
        for p in 0..1usize << coding.partitions {
            let part = &residual[at..at + per - if p == 0 { order } else { 0 }];
            at += part.len();
            if coding.escape && p == 0 {
                let raw = part
                    .iter()
                    .map(|&e| 65 - (e ^ (e >> 63)).leading_zeros())
                    .max()
                    .unwrap_or(0);
                let raw = if part.iter().all(|&e| e == 0) { 0 } else { raw };
                w.put(u64::from(escape), width);
                w.put(u64::from(raw), 5);
                part.iter().for_each(|&e| w.signed(e, raw));
                continue;
            }
            let folded: Vec<u64> = part
                .iter()
                .map(|&e| ((e << 1) ^ (e >> 63)) as u64)
                .collect();
            let mean = folded.iter().sum::<u64>() / folded.len().max(1) as u64;
            let k = (64 - mean.leading_zeros()).min(escape - 1);
            w.put(u64::from(k), width);
            for &u in &folded {
                w.unary(u >> k);
                w.put(u & ((1 << k) - 1), k);
            }
        }
    }

    fn utf8(w: &mut Writer, n: u64) {
        if n < 0x80 {
            return w.put(n, 8);
        }
        let bits = 64 - n.leading_zeros();
        let len = (2..=7u32)
            .find(|&len| bits <= 7 - len + 6 * (len - 1))
            .expect("36 bits at most");
        // As many ones as bytes, then a zero.
        w.put(((1 << len) - 1) << 1, len + 1);
        w.put(n >> (6 * (len - 1)), 7 - len);
        for i in (0..len - 1).rev() {
            w.put(0b10, 2);
            w.put(n >> (6 * i) & 0x3F, 6);
        }
    }

    /// How a stream's frames are coded, beyond their samples.
    #[derive(Clone, Copy)]
    struct Stream {
        rate: u32,
        bits: u32,
        /// The header's code for the rate: 0 for the stream's, or one of its
        /// own.
        rate_code: u64,
        /// The header's code for the sample size: 0 for the stream's.
        bits_code: u64,
        /// Numbered by sample rather than by frame.
        by_sample: bool,
    }

    const CD: Stream = Stream {
        rate: 44_100,
        bits: 16,
        rate_code: 9,
        bits_code: 4,
        by_sample: false,
    };

    /// A frame numbered `number`, of `channels` (each the same length) coded
    /// in `layout` as `codings` say.
    fn frame(
        stream: Stream,
        number: u64,
        layout: Layout,
        channels: &[Vec<i64>],
        codings: &[Coding],
    ) -> Vec<u8> {
        let block = channels[0].len();
        let block_code = match block {
            192 => 1,
            576 | 1152 | 2304 | 4608 => 2 + (block / 576).trailing_zeros() as u64,
            b if b.is_power_of_two() && (256..=32_768).contains(&b) => {
                8 + (b / 256).trailing_zeros() as u64
            }
            b if b <= 256 => 6,
            _ => 7,
        };
        let layout_code = match layout {
            Layout::Apart(n) => n as u64 - 1,
            Layout::LeftSide => 8,
            Layout::SideRight => 9,
            Layout::MidSide => 10,
        };
        let mut w = Writer::default();
        w.put(0b11111111111110, 14);
        w.bit(false);
        w.bit(stream.by_sample);
        w.put(block_code, 4);
        w.put(stream.rate_code, 4);
        w.put(layout_code, 4);
        w.put(stream.bits_code, 3);
        w.bit(false);
        utf8(&mut w, number);
        match block_code {
            6 => w.put(block as u64 - 1, 8),
            7 => w.put(block as u64 - 1, 16),
            _ => {}
        }
        match stream.rate_code {
            12 => w.put(u64::from(stream.rate / 1000), 8),
            13 => w.put(u64::from(stream.rate), 16),
            14 => w.put(u64::from(stream.rate / 10), 16),
            _ => {}
        }
        w.put(u64::from(crc8(&w.bytes)), 8);
        let sent: Vec<(Vec<i64>, u32)> = match (layout, channels) {
            (Layout::Apart(_), channels) => {
                channels.iter().map(|c| (c.clone(), stream.bits)).collect()
            }
            (layout, [left, right]) => {
                let side: Vec<i64> = left.iter().zip(right).map(|(l, r)| l - r).collect();
                let mid: Vec<i64> = left.iter().zip(right).map(|(l, r)| (l + r) >> 1).collect();
                let wider = stream.bits + 1;
                match layout {
                    Layout::LeftSide => vec![(left.clone(), stream.bits), (side, wider)],
                    Layout::SideRight => vec![(side, wider), (right.clone(), stream.bits)],
                    _ => vec![(mid, stream.bits), (side, wider)],
                }
            }
            _ => panic!("a pair's layout for other than two channels"),
        };
        for ((samples, bits), coding) in sent.iter().zip(codings) {
            subframe(&mut w, samples, *bits, coding);
        }
        w.align();
        let crc = crc16_from(0, &w.bytes);
        w.put(u64::from(crc), 16);
        w.bytes
    }

    /// A file of `frames` with the stream information they need.
    fn file(stream: Stream, channels: usize, blocks: (u16, u16), frames: &[Vec<u8>]) -> Vec<u8> {
        let samples: u64 = 0;
        let largest = frames.iter().map(Vec::len).max().unwrap_or(0) as u64;
        let mut w = Writer::default();
        w.put(u64::from(blocks.0), 16);
        w.put(u64::from(blocks.1), 16);
        w.put(0, 24);
        w.put(largest, 24);
        w.put(u64::from(stream.rate), 20);
        w.put(channels as u64 - 1, 3);
        w.put(u64::from(stream.bits) - 1, 5);
        w.put(samples, 36);
        w.put(0, 64);
        w.put(0, 64);
        let mut out = b"fLaC\x80\x00\x00\x22".to_vec();
        out.extend(w.bytes);
        frames.iter().for_each(|f| out.extend(f));
        out
    }

    /// Every frame of `bytes` as the reader reads it.
    fn read(name: &str, bytes: &[u8]) -> Vec<(u64, Vec<f32>)> {
        let path = temp_file(name, bytes);
        let mut reader = Reader::open(&path).unwrap().expect("a FLAC stream");
        let mut frames = Vec::new();
        while let Some((first, samples)) = reader.next().unwrap() {
            frames.push((first, samples.to_vec()));
        }
        std::fs::remove_file(&path).unwrap();
        frames
    }

    /// What the reader gives for `channels`, interleaved.
    fn expected(channels: &[Vec<i64>], bits: u32) -> Vec<f32> {
        let scale = 1.0 / (1u64 << (bits - 1)) as f32;
        (0..channels[0].len())
            .flat_map(|i| channels.iter().map(move |c| c[i] as i32 as f32 * scale))
            .collect()
    }

    /// A slow tone with a little noise at full scale for `bits`, the low
    /// `wasted` bits clear.
    fn tone(len: usize, bits: u32, seed: u64, wasted: u32) -> Vec<i64> {
        let full = ((1i64 << (bits - 1)) - 1) as f64;
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let noise = (state % 2001) as f64 / 1000.0 - 1.0;
                let v =
                    (((i as f64 * 0.013 + seed as f64).sin() * 0.7 + noise * 0.01) * full) as i64;
                v >> wasted << wasted
            })
            .collect()
    }

    #[test]
    fn every_kind_of_subframe_reads_back_as_written() {
        let mut kinds = vec![
            (coding(Kind::Constant), vec![-1234; 4096]),
            (coding(Kind::Verbatim), tone(4096, 16, 1, 0)),
        ];
        for order in 0..=4 {
            kinds.push((
                coding(Kind::Fixed(order)),
                tone(4096, 16, 2 + order as u64, 0),
            ));
        }
        for (i, order) in [1, 2, 3, 5, 8, 12, 13, 16, 20, 24, 31, 32]
            .into_iter()
            .enumerate()
        {
            let (precision, shift) = [(15, 14), (12, 9), (8, 5)][i % 3];
            kinds.push((
                coding(lpc(order, precision, shift)),
                tone(4096, 16, 20 + order as u64, 0),
            ));
        }
        // Low bits left out, partitions, five-bit parameters and a plain
        // partition.
        let mut spare = coding(Kind::Fixed(2));
        spare.wasted = 3;
        kinds.push((spare, tone(4096, 16, 40, 3)));
        let mut spare = coding(lpc(8, 13, 10));
        (spare.wasted, spare.partitions, spare.wide_parameters) = (1, 4, true);
        kinds.push((spare, tone(4096, 16, 41, 1)));
        let mut plain = coding(lpc(4, 14, 12));
        (plain.partitions, plain.escape) = (2, true);
        kinds.push((plain, tone(4096, 16, 42, 0)));
        let mut silent = coding(Kind::Fixed(1));
        silent.escape = true;
        kinds.push((silent, vec![7; 4096]));
        let frames: Vec<Vec<u8>> = kinds
            .iter()
            .enumerate()
            .map(|(n, (coding, samples))| {
                frame(
                    CD,
                    n as u64,
                    Layout::Apart(1),
                    std::slice::from_ref(samples),
                    std::slice::from_ref(coding),
                )
            })
            .collect();
        let read = read("kinds.flac", &file(CD, 1, (4096, 4096), &frames));
        assert_eq!(read.len(), kinds.len());
        for (n, ((first, samples), (_, written))) in read.iter().zip(&kinds).enumerate() {
            assert_eq!(*first, n as u64 * 4096);
            assert!(
                *samples == expected(std::slice::from_ref(written), 16),
                "frame {n}"
            );
        }
    }

    /// A pair of channels `len` long for `layout`, with what the layout
    /// sends first or second held constant where `constant` says.
    fn pair(layout: Layout, constant: (bool, bool), bits: u32, seed: u64) -> [Vec<i64>; 2] {
        let left = tone(1152, bits, seed, 0);
        let right: Vec<i64> = left.iter().map(|l| l * 9 / 10).collect();
        let still = |value: i64| vec![value; left.len()];
        let apart = |value: i64| left.iter().map(|l| l - value).collect::<Vec<_>>();
        match (layout, constant) {
            (_, (false, false)) => [left.clone(), right],
            (Layout::Apart(_) | Layout::LeftSide, (true, _)) => [still(-9), right],
            (Layout::Apart(_) | Layout::SideRight, (_, true)) => [left.clone(), still(9)],
            (Layout::LeftSide | Layout::MidSide, (_, true)) => [left.clone(), apart(5)],
            (Layout::SideRight, (true, _)) => [left.clone(), apart(-5)],
            // The mean of the two held still, their difference moving.
            (Layout::MidSide, _) => {
                let swing: Vec<i64> = left.iter().map(|l| l / 2).collect();
                [
                    swing.iter().map(|d| 3 + d).collect(),
                    swing.iter().map(|d| 3 - d).collect(),
                ]
            }
        }
    }

    #[test]
    fn pairs_read_back_in_every_layout_at_every_depth() {
        let pairs = [
            (coding(lpc(3, 15, 14)), coding(lpc(7, 12, 9))),
            (coding(lpc(12, 14, 13)), coding(lpc(1, 11, 8))),
            (coding(lpc(13, 15, 14)), coding(lpc(4, 13, 12))),
            (coding(lpc(5, 10, 7)), coding(lpc(5, 15, 14))),
            (coding(Kind::Fixed(2)), coding(lpc(5, 15, 14))),
            (coding(Kind::Fixed(1)), coding(Kind::Fixed(3))),
            (coding(Kind::Verbatim), coding(Kind::Constant)),
            (coding(Kind::Constant), coding(Kind::Verbatim)),
            (coding(Kind::Fixed(4)), coding(Kind::Verbatim)),
        ];
        for bits in [8, 12, 16, 20, 24, 32] {
            let stream = Stream {
                rate: 48_000,
                bits,
                rate_code: 10,
                bits_code: match bits {
                    8 => 1,
                    12 => 2,
                    16 => 4,
                    20 => 5,
                    24 => 6,
                    _ => 7,
                },
                by_sample: false,
            };
            for layout in [
                Layout::Apart(2),
                Layout::LeftSide,
                Layout::SideRight,
                Layout::MidSide,
            ] {
                let written: Vec<[Vec<i64>; 2]> = pairs
                    .iter()
                    .enumerate()
                    .map(|(n, (a, b))| {
                        let constant = (
                            matches!(a.kind, Kind::Constant),
                            matches!(b.kind, Kind::Constant),
                        );
                        pair(layout, constant, bits, 60 + n as u64)
                    })
                    .collect();
                let frames: Vec<Vec<u8>> = written
                    .iter()
                    .zip(&pairs)
                    .enumerate()
                    .map(|(n, (channels, (a, b)))| {
                        frame(stream, n as u64, layout, channels, &[a.clone(), b.clone()])
                    })
                    .collect();
                let read = read(
                    &format!("pairs-{bits}-{layout:?}.flac"),
                    &file(stream, 2, (1152, 1152), &frames),
                );
                assert_eq!(read.len(), frames.len(), "{bits} bits, {layout:?}");
                for (n, ((_, samples), channels)) in read.iter().zip(&written).enumerate() {
                    assert!(
                        *samples == expected(channels, bits),
                        "{bits} bits, {layout:?}, pair {n}"
                    );
                }
            }
        }
    }

    #[test]
    fn more_channels_than_a_pair_are_predicted_two_at_a_time_and_one_alone() {
        let stream = Stream {
            rate: 96_000,
            bits: 24,
            rate_code: 11,
            bits_code: 6,
            by_sample: false,
        };
        let channels: Vec<Vec<i64>> = (0..5).map(|c| tone(4608, 24, 80 + c, 0)).collect();
        let codings = [
            coding(lpc(5, 15, 14)),
            coding(lpc(3, 15, 14)),
            coding(Kind::Fixed(2)),
            coding(lpc(8, 15, 14)),
            coding(lpc(4, 15, 14)),
        ];
        let frames = [frame(stream, 0, Layout::Apart(5), &channels, &codings)];
        let read = read("five.flac", &file(stream, 5, (4608, 4608), &frames));
        assert!(read.len() == 1 && read[0].1 == expected(&channels, 24));
    }

    #[test]
    fn frames_numbered_by_sample_with_sizes_and_rates_of_their_own_read_where_they_start() {
        for (rate, rate_code) in [(12_345, 13), (37_000, 12), (123_450, 14), (22_050, 0)] {
            let stream = Stream {
                rate,
                bits: 16,
                rate_code,
                bits_code: 0,
                by_sample: true,
            };
            let sizes = [100, 3000, 192, 576, 256, 16_384, 17, 4608];
            let mut first = 1u64 << 20;
            let (mut frames, mut starts, mut written) = (Vec::new(), Vec::new(), Vec::new());
            for (n, &size) in sizes.iter().enumerate() {
                let samples = tone(size, 16, 100 + n as u64, 0);
                frames.push(frame(
                    stream,
                    first,
                    Layout::Apart(1),
                    std::slice::from_ref(&samples),
                    &[coding(lpc(2, 15, 14))],
                ));
                starts.push(first);
                written.push(samples);
                first += size as u64;
            }
            let read = read(
                &format!("sizes-{rate}.flac"),
                &file(stream, 1, (17, 16_384), &frames),
            );
            assert_eq!(
                read.iter().map(|f| f.0).collect::<Vec<_>>(),
                starts,
                "{rate} Hz"
            );
            for ((_, samples), written) in read.iter().zip(&written) {
                assert!(
                    *samples == expected(std::slice::from_ref(written), 16),
                    "{rate} Hz"
                );
            }
        }
    }

    #[test]
    fn frame_numbers_of_many_bytes_give_where_frames_start() {
        let numbers = [
            0,
            127,
            128,
            2047,
            2048,
            65_535,
            65_536,
            1 << 21,
            (1 << 31) - 1,
        ];
        let frames: Vec<Vec<u8>> = numbers
            .iter()
            .map(|&n| {
                frame(
                    CD,
                    n,
                    Layout::Apart(1),
                    &[tone(1152, 16, n, 0)],
                    &[coding(Kind::Fixed(1))],
                )
            })
            .collect();
        let read = read("numbers.flac", &file(CD, 1, (1152, 1152), &frames));
        assert_eq!(
            read.iter().map(|f| f.0).collect::<Vec<_>>(),
            numbers.map(|n| n * 1152)
        );
    }

    /// Frames of a slow tone, 1152 samples each, and the tone.
    fn many(count: u64) -> (Vec<Vec<u8>>, Vec<Vec<i64>>) {
        (0..count)
            .map(|n| {
                let samples = tone(1152, 16, 200 + n, 0);
                (
                    frame(
                        CD,
                        n,
                        Layout::Apart(1),
                        std::slice::from_ref(&samples),
                        &[coding(lpc(4, 15, 14))],
                    ),
                    samples,
                )
            })
            .unzip()
    }

    #[test]
    fn a_damaged_frame_is_left_out_and_reading_goes_on_after_it() {
        let (mut frames, written) = many(5);
        let middle = frames[2].len() / 2;
        frames[2][middle] ^= 0x10;
        let mut bytes = file(CD, 1, (1152, 1152), &frames);
        // And the last frame cut short, as a copy that stopped would leave it.
        bytes.truncate(bytes.len() - 40);
        let read = read("damaged.flac", &bytes);
        assert_eq!(
            read.iter().map(|f| f.0).collect::<Vec<_>>(),
            [0, 1152, 3456]
        );
        for ((_, samples), n) in read.iter().zip([0, 1, 3]) {
            assert!(*samples == expected(&[written[n].clone()], 16));
        }
    }

    #[test]
    fn a_seek_lands_before_the_sample_close_enough_to_read_on_to_it() {
        let (frames, _) = many(900);
        let size = frames.iter().map(Vec::len).max().unwrap();
        let mut bytes = file(CD, 1, (1152, 1152), &frames);
        for known in [true, false] {
            if !known {
                // A stream that does not say how big its frames get.
                bytes[8 + 7..8 + 10].fill(0);
            }
            let path = temp_file(&format!("seek-{known}.flac"), &bytes);
            let mut reader = Reader::open(&path).unwrap().unwrap();
            // A seek narrows down to two frames' room, at least 64 kB each.
            let most = 2 * (reader.frame_room() / size + 1);
            for sample in [
                0,
                1,
                1151,
                1152,
                300_000,
                450_000,
                900 * 1152 - 1,
                12,
                690_000,
            ] {
                reader.seek(sample).unwrap();
                let (mut first, mut len, mut read) = (u64::MAX, 0, 0);
                while let Some((at, samples)) = reader.next().unwrap() {
                    if read == 0 {
                        assert!(at <= sample, "a seek to {sample} landed on {at}");
                    }
                    (first, len, read) = (at, samples.len() as u64, read + 1);
                    if sample < first + len {
                        break;
                    }
                }
                assert!(
                    first <= sample && sample < first + len,
                    "reading on from a seek to {sample}"
                );
                assert!(read <= most, "{read} frames read on to {sample}");
            }
            reader.seek(1 << 40).unwrap();
            assert!(
                reader
                    .next()
                    .unwrap()
                    .is_some_and(|(first, _)| first <= 899 * 1152)
            );
            std::fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn a_file_with_a_tag_in_front_is_read_and_other_files_are_not() {
        let (frames, written) = many(2);
        let flac = file(CD, 1, (1152, 1152), &frames);
        let mut tagged = b"ID3\x04\x00\x00\x00\x00\x01\x05".to_vec();
        tagged.extend([0u8; 133]);
        tagged.extend(&flac);
        let read = read("tagged.flac", &tagged);
        assert!(read.len() == 2 && read[1].1 == expected(&[written[1].clone()], 16));
        let mut silent_rate = flac.clone();
        let low = silent_rate[8 + 12] & 0x0F;
        silent_rate[8 + 10..8 + 13].copy_from_slice(&[0, 0, low]);
        for (name, bytes) in [
            ("wav.flac", &b"RIFF\x24\x00\x00\x00WAVEfmt "[..]),
            ("rate.flac", &silent_rate[..]),
            ("short.flac", &flac[..20]),
        ] {
            let path = temp_file(name, bytes);
            assert!(Reader::open(&path).unwrap().is_none(), "{name}");
            std::fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn checksums_are_the_formats_and_folding_gives_what_the_tables_do() {
        assert_eq!(crc8(b"123456789"), 0xF4);
        assert_eq!(crc16_from(0, b"123456789"), 0xFEE8);
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let bytes: Vec<u8> = (0..5000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        for len in (0..700).chain([1024, 4095, 4096, 4097]) {
            for from in [0, 1, 7, 15] {
                let part = &bytes[from..from + len];
                assert_eq!(crc16(part), crc16_from(0, part), "{len} bytes from {from}");
            }
        }
    }
}
