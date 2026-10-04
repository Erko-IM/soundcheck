//! M4A of ALAC: the packets in `mdat` as they are encoded, then the `moov`
//! that indexes them, with the tags as an iTunes `ilst` and the markers as
//! Nero chapters, which iTunes, VLC and ffmpeg read. Written after the
//! audio, as the index is only known then.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use lofty::config::WriteOptions;
use lofty::tag::TagExt;

use super::alac::{self, Encoder as Alac};
use super::carry::Carried;
use super::{Encode, Frames, Sample, Spec};
use crate::wav::Marker;

fn failed(e: io::Error) -> String {
    format!("cannot write the new file: {e}")
}

/// A box: its size and type, then `body`.
fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 8);
    b.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

/// A full box: a box whose body starts with a version and flags.
fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version];
    b.extend_from_slice(&flags.to_be_bytes()[1..]);
    b.extend_from_slice(body);
    mp4_box(kind, &b)
}

/// The unity matrix a track and the movie are shown through.
const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

pub struct Writer {
    out: BufWriter<File>,
    alac: Alac,
    channels: usize,
    pending: Vec<i32>,
    sizes: Vec<u32>,
    /// Frames in each packet, the last perhaps short.
    last: usize,
    /// Where the packets start, and how far they run so far.
    data_at: u64,
    data: u64,
    tags: Vec<u8>,
    markers: Vec<Marker>,
    rate: u32,
}

impl Writer {
    pub fn new(file: File, spec: Spec, carried: &Carried) -> Result<Self, String> {
        let Sample::Int(bits) = spec.sample else {
            return Err("ALAC keeps whole numbers only".into());
        };
        let alac = Alac::new(bits, spec.channels, spec.rate)?;
        let mut ilst = Vec::new();
        carried
            .ilst()
            .dump_to(&mut ilst, WriteOptions::default())
            .map_err(|e| format!("the iTunes tags cannot be written: {e}"))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let mut ftyp = b"M4A ".to_vec();
        ftyp.extend_from_slice(&0u32.to_be_bytes());
        for brand in [b"M4A ", b"mp42", b"isom", b"\0\0\0\0"] {
            ftyp.extend_from_slice(brand);
        }
        let head = mp4_box(b"ftyp", &ftyp);
        out.write_all(&head).map_err(failed)?;
        // Room for a 64-bit size, in case the audio runs past 4 GB: until
        // then, as an empty `free` box and a 32-bit size, which lofty
        // reads; it skips a box with a 64-bit size 8 bytes too far.
        out.write_all(&8u32.to_be_bytes()).map_err(failed)?;
        out.write_all(b"free").map_err(failed)?;
        out.write_all(&0u32.to_be_bytes()).map_err(failed)?;
        out.write_all(b"mdat").map_err(failed)?;
        Ok(Self {
            out,
            alac,
            channels: spec.channels,
            pending: Vec::new(),
            sizes: Vec::new(),
            last: alac::FRAME,
            data_at: head.len() as u64 + 16,
            data: 0,
            tags: ilst,
            markers: carried.markers.clone(),
            rate: spec.rate,
        })
    }

    fn packet(&mut self, samples: &[i32]) -> Result<(), String> {
        let packet = self.alac.encode(samples);
        self.sizes.push(packet.len() as u32);
        self.last = samples.len() / self.channels;
        self.data += packet.len() as u64;
        self.out.write_all(&packet).map_err(failed)
    }

    /// The `moov` that indexes the packets written.
    fn moov(&self) -> Vec<u8> {
        let frames = self.alac.frames;
        let rate = self.rate;
        // The movie counts in milliseconds, the track in frames.
        let movie = frames * 1000 / u64::from(rate.max(1));
        let long = frames > u64::from(u32::MAX) || movie > u64::from(u32::MAX);
        let times = |b: &mut Vec<u8>, scale: u32, duration: u64| {
            if long {
                b.extend_from_slice(&[0; 16]);
                b.extend_from_slice(&scale.to_be_bytes());
                b.extend_from_slice(&duration.to_be_bytes());
            } else {
                b.extend_from_slice(&[0; 8]);
                b.extend_from_slice(&scale.to_be_bytes());
                b.extend_from_slice(&(duration as u32).to_be_bytes());
            }
        };
        let version = u8::from(long);
        let mut mvhd = Vec::new();
        times(&mut mvhd, 1000, movie);
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes());
        mvhd.extend_from_slice(&[0; 10]);
        for m in MATRIX {
            mvhd.extend_from_slice(&m.to_be_bytes());
        }
        mvhd.extend_from_slice(&[0; 24]);
        mvhd.extend_from_slice(&2u32.to_be_bytes());

        let mut tkhd = Vec::new();
        if long {
            tkhd.extend_from_slice(&[0; 16]);
            tkhd.extend_from_slice(&1u32.to_be_bytes());
            tkhd.extend_from_slice(&[0; 4]);
            tkhd.extend_from_slice(&movie.to_be_bytes());
        } else {
            tkhd.extend_from_slice(&[0; 8]);
            tkhd.extend_from_slice(&1u32.to_be_bytes());
            tkhd.extend_from_slice(&[0; 4]);
            tkhd.extend_from_slice(&(movie as u32).to_be_bytes());
        }
        tkhd.extend_from_slice(&[0; 8]);
        tkhd.extend_from_slice(&[0; 4]);
        tkhd.extend_from_slice(&0x0100u16.to_be_bytes());
        tkhd.extend_from_slice(&[0; 2]);
        for m in MATRIX {
            tkhd.extend_from_slice(&m.to_be_bytes());
        }
        tkhd.extend_from_slice(&[0; 8]);

        let mut mdhd = Vec::new();
        times(&mut mdhd, rate, frames);
        // Language "und", packed as ISO 639-2/T into 15 bits.
        mdhd.extend_from_slice(&0x55C4u16.to_be_bytes());
        mdhd.extend_from_slice(&[0; 2]);

        let mut hdlr = vec![0; 4];
        hdlr.extend_from_slice(b"soun");
        hdlr.extend_from_slice(&[0; 12]);
        hdlr.extend_from_slice(b"SoundHandler\0");

        let dref = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&full_box(b"url ", 0, 1, &[]));
            full_box(b"dref", 0, 0, &b)
        };

        // The sample entry keeps a rate only to 65535 Hz, so past that a
        // placeholder, as Apple writes; the magic cookie holds the real
        // one.
        let mut entry = vec![0; 6];
        entry.extend_from_slice(&1u16.to_be_bytes());
        entry.extend_from_slice(&[0; 8]);
        entry.extend_from_slice(&(self.channels as u16).to_be_bytes());
        entry.extend_from_slice(&(self.alac.bits() as u16).to_be_bytes());
        entry.extend_from_slice(&[0; 4]);
        let placeholder = if rate <= 0xFFFF { rate } else { 44_100 };
        entry.extend_from_slice(&(placeholder << 16).to_be_bytes());
        entry.extend_from_slice(&full_box(b"alac", 0, 0, &self.alac.cookie()));
        let stsd = {
            let mut b = 1u32.to_be_bytes().to_vec();
            b.extend_from_slice(&mp4_box(b"alac", &entry));
            full_box(b"stsd", 0, 0, &b)
        };
        let packets = self.sizes.len() as u32;
        let stts = {
            let full = if self.last == alac::FRAME {
                packets
            } else {
                packets.saturating_sub(1)
            };
            let mut entries = Vec::new();
            if full > 0 {
                entries.push((full, alac::FRAME as u32));
            }
            if full < packets {
                entries.push((1, self.last as u32));
            }
            let mut b = (entries.len() as u32).to_be_bytes().to_vec();
            for (count, delta) in entries {
                b.extend_from_slice(&count.to_be_bytes());
                b.extend_from_slice(&delta.to_be_bytes());
            }
            full_box(b"stts", 0, 0, &b)
        };
        // Every packet in one chunk, as they follow one another.
        let stsc = {
            let mut b = 1u32.to_be_bytes().to_vec();
            for v in [1, packets, 1] {
                b.extend_from_slice(&v.to_be_bytes());
            }
            full_box(b"stsc", 0, 0, &b)
        };
        let stsz = {
            let mut b = 0u32.to_be_bytes().to_vec();
            b.extend_from_slice(&packets.to_be_bytes());
            for size in &self.sizes {
                b.extend_from_slice(&size.to_be_bytes());
            }
            full_box(b"stsz", 0, 0, &b)
        };
        let offsets = match u32::try_from(self.data_at) {
            Ok(at) => {
                let mut b = 1u32.to_be_bytes().to_vec();
                b.extend_from_slice(&at.to_be_bytes());
                full_box(b"stco", 0, 0, &b)
            }
            Err(_) => {
                let mut b = 1u32.to_be_bytes().to_vec();
                b.extend_from_slice(&self.data_at.to_be_bytes());
                full_box(b"co64", 0, 0, &b)
            }
        };
        let stbl = mp4_box(b"stbl", &[stsd, stts, stsc, stsz, offsets].concat());
        let minf = mp4_box(
            b"minf",
            &[
                full_box(b"smhd", 0, 0, &[0; 4]),
                mp4_box(b"dinf", &dref),
                stbl,
            ]
            .concat(),
        );
        let mdia = mp4_box(
            b"mdia",
            &[
                full_box(b"mdhd", version, 0, &mdhd),
                full_box(b"hdlr", 0, 0, &hdlr),
                minf,
            ]
            .concat(),
        );
        let trak = mp4_box(
            b"trak",
            &[full_box(b"tkhd", version, 7, &tkhd), mdia].concat(),
        );

        let mut udta = Vec::new();
        if !self.markers.is_empty() {
            udta.extend_from_slice(&chpl(&self.markers, rate));
        }
        let mut meta_hdlr = vec![0; 4];
        meta_hdlr.extend_from_slice(b"mdir");
        meta_hdlr.extend_from_slice(b"appl");
        meta_hdlr.extend_from_slice(&[0; 8]);
        meta_hdlr.push(0);
        let meta = full_box(
            b"meta",
            0,
            0,
            &[full_box(b"hdlr", 0, 0, &meta_hdlr), self.tags.clone()].concat(),
        );
        udta.extend_from_slice(&meta);
        mp4_box(
            b"moov",
            &[
                full_box(b"mvhd", version, 0, &mvhd),
                trak,
                mp4_box(b"udta", &udta),
            ]
            .concat(),
        )
    }
}

/// Nero's chapter list: each marker's place, in 100 ns steps, and its
/// name. It counts up to 255.
fn chpl(markers: &[Marker], rate: u32) -> Vec<u8> {
    let markers = &markers[..markers.len().min(255)];
    let mut b = 0u32.to_be_bytes().to_vec();
    b.push(markers.len() as u8);
    for m in markers {
        let at = m.frame as u128 * 10_000_000 / u128::from(rate.max(1));
        b.extend_from_slice(&(at as u64).to_be_bytes());
        let name = &m.label.as_bytes()[..m.label.len().min(255)];
        b.push(name.len() as u8);
        b.extend_from_slice(name);
    }
    full_box(b"chpl", 1, 0, &b)
}

impl Encode for Writer {
    fn push(&mut self, frames: Frames<'_>) -> Result<(), String> {
        let Frames::Int(samples) = frames else {
            return Err("ALAC keeps whole numbers only".into());
        };
        self.pending.extend_from_slice(samples);
        let packet = alac::FRAME * self.channels;
        let mut at = 0;
        while self.pending.len() - at >= packet {
            let block = self.pending[at..at + packet].to_vec();
            self.packet(&block)?;
            at += packet;
        }
        self.pending.drain(..at);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<(), String> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.packet(&rest)?;
        }
        let moov = self.moov();
        let (data_at, data) = (self.data_at, self.data);
        (|| -> io::Result<()> {
            self.out.write_all(&moov)?;
            self.out.flush()?;
            let file = self.out.get_mut();
            match u32::try_from(data + 8) {
                Ok(size) => {
                    file.seek(SeekFrom::Start(data_at - 8))?;
                    file.write_all(&size.to_be_bytes())?;
                }
                Err(_) => {
                    file.seek(SeekFrom::Start(data_at - 16))?;
                    file.write_all(&1u32.to_be_bytes())?;
                    file.write_all(b"mdat")?;
                    file.write_all(&(data + 16).to_be_bytes())?;
                }
            }
            file.sync_all()
        })()
        .map_err(failed)
    }
}

/// An M4A's Nero chapters, as markers at `rate`.
pub fn chapters(path: &Path, rate: u32) -> Result<Vec<Marker>, String> {
    let read = || -> io::Result<Vec<Marker>> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let Some((start, size)) = find(&mut file, 0, len, &[b"moov", b"udta", b"chpl"])? else {
            return Ok(Vec::new());
        };
        let mut b = vec![0; size.min(1 << 20) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut b)?;
        let version = b.first().copied().unwrap_or(0);
        let mut at = if version == 1 { 8 } else { 4 };
        let count = b.get(at).copied().unwrap_or(0);
        at += 1;
        let mut markers = Vec::new();
        for id in 1..=u32::from(count) {
            let (Some(time), Some(&n)) = (b.get(at..at + 8), b.get(at + 8)) else {
                break;
            };
            let time = u64::from_be_bytes(time.try_into().expect("eight bytes"));
            let name = b.get(at + 9..at + 9 + usize::from(n)).unwrap_or_default();
            markers.push(Marker {
                id,
                frame: (u128::from(time) * u128::from(rate) / 10_000_000) as usize,
                length: 0,
                label: String::from_utf8_lossy(name).into_owned(),
                note: String::new(),
            });
            at += 9 + usize::from(n);
        }
        Ok(markers)
    };
    read().map_err(|e| format!("its chapters cannot be read: {e}"))
}

/// Where the box down the path `kinds` from the boxes in `start..end` has
/// its body, and how long that is.
fn find(
    file: &mut File,
    start: u64,
    end: u64,
    kinds: &[&[u8; 4]],
) -> io::Result<Option<(u64, u64)>> {
    let mut at = start;
    while at + 8 <= end {
        let mut head = [0u8; 16];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut head[..8])?;
        let mut size = u64::from(u32::from_be_bytes(head[..4].try_into().expect("four")));
        let mut header = 8;
        if size == 1 {
            file.read_exact(&mut head[8..16])?;
            size = u64::from_be_bytes(head[8..16].try_into().expect("eight"));
            header = 16;
        } else if size == 0 {
            size = end - at;
        }
        if size < header {
            return Ok(None);
        }
        if &head[4..8] == kinds[0] {
            let body = (at + header, size - header);
            return match kinds.len() {
                1 => Ok(Some(body)),
                _ => find(file, body.0, body.0 + body.1, &kinds[1..]),
            };
        }
        at += size;
    }
    Ok(None)
}
