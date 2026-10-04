//! Matroska audio: an MKA of FLAC, and a WebM of Vorbis. The tags go in as
//! SimpleTags, the markers as chapters, and an MKA's pictures as
//! attachments, which WebM does not take.
//!
//! Timestamps count in nanoseconds, a block to a cluster, each block's its
//! first frame's time rounded up: working the frame back out of it, as
//! soundcheck's reader does, then gives exactly that frame at any rate.
//! The usual millisecond timestamps would put most blocks a few frames off
//! where they belong.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::carry::Carried;
use super::flac::FrameEncoder;
use super::vorbis::{self, Stream};
use super::{Encode, Frames, Sample, Spec};
use crate::wav::Marker;

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_UID: u32 = 0x73C5;
const TRACK_TYPE: u32 = 0x83;
const FLAG_LACING: u32 = 0x9C;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const BIT_DEPTH: u32 = 0x6264;
const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const CHAPTERS: u32 = 0x1043_A770;
const EDITION_ENTRY: u32 = 0x45B9;
const EDITION_UID: u32 = 0x45BC;
const CHAPTER_ATOM: u32 = 0xB6;
const CHAPTER_UID: u32 = 0x73C4;
const CHAPTER_TIME_START: u32 = 0x91;
const CHAPTER_TIME_END: u32 = 0x92;
const CHAPTER_DISPLAY: u32 = 0x80;
const CHAP_STRING: u32 = 0x85;
const CHAP_LANGUAGE: u32 = 0x437C;
const TAGS: u32 = 0x1254_C367;
const TAG: u32 = 0x7373;
const TARGETS: u32 = 0x63C0;
const TARGET_TYPE_VALUE: u32 = 0x68CA;
const SIMPLE_TAG: u32 = 0x67C8;
const TAG_NAME: u32 = 0x45A3;
const TAG_STRING: u32 = 0x4487;
const ATTACHMENTS: u32 = 0x1941_A469;
const ATTACHED_FILE: u32 = 0x61A7;
const FILE_DESCRIPTION: u32 = 0x467E;
const FILE_NAME: u32 = 0x466E;
const FILE_MIME_TYPE: u32 = 0x4660;
const FILE_DATA: u32 = 0x465C;
const FILE_UID: u32 = 0x46AE;

fn failed(e: io::Error) -> String {
    format!("cannot write the new file: {e}")
}

/// An element ID as its bytes, which carry their own length.
fn id(id: u32) -> Vec<u8> {
    let skip = (id.leading_zeros() / 8).min(3) as usize;
    id.to_be_bytes()[skip..].to_vec()
}

/// A size as the shortest variable-length integer that holds it.
fn size(n: u64) -> Vec<u8> {
    let length = (1..=8usize)
        .find(|&l| n < (1u64 << (7 * l)) - 1)
        .unwrap_or(8);
    let marked = if length == 8 {
        n
    } else {
        n | 1 << (7 * length)
    };
    let mut b = marked.to_be_bytes()[8 - length..].to_vec();
    if length == 8 {
        b[0] = 0x01;
    }
    b
}

/// An element, and how long its ID and size are.
fn wrapped(kind: u32, body: &[u8]) -> (Vec<u8>, usize) {
    let mut b = id(kind);
    b.extend(size(body.len() as u64));
    let header = b.len();
    b.extend_from_slice(body);
    (b, header)
}

fn element(kind: u32, body: &[u8]) -> Vec<u8> {
    wrapped(kind, body).0
}

fn uint(kind: u32, v: u64) -> Vec<u8> {
    let skip = ((v.leading_zeros() / 8) as usize).min(7);
    element(kind, &v.to_be_bytes()[skip..])
}

fn float(kind: u32, v: f64) -> Vec<u8> {
    element(kind, &v.to_be_bytes())
}

fn text(kind: u32, v: &str) -> Vec<u8> {
    element(kind, v.as_bytes())
}

/// The nanosecond timestamp of `frame` at `rate`, rounded up, so that a
/// frame worked back out of it, rounding down, is `frame`.
fn ns(frame: u64, rate: u32) -> u64 {
    (u128::from(frame) * 1_000_000_000).div_ceil(u128::from(rate.max(1))) as u64
}

/// Where in the file the parts filled in at the end are.
pub struct Places {
    segment_size: u64,
    segment_start: u64,
    duration: u64,
    private: u64,
}

/// Everything before the clusters: the EBML header, and in the segment the
/// info, the track, the chapters, the tags and the attachments.
fn head(
    spec: &Spec,
    carried: &Carried,
    codec: &str,
    private: &[u8],
    webm: bool,
) -> (Vec<u8>, Places) {
    let mut head = element(
        EBML,
        &[
            uint(0x4286, 1),
            uint(0x42F7, 1),
            uint(0x42F2, 4),
            uint(0x42F3, 8),
            text(0x4282, if webm { "webm" } else { "matroska" }),
            uint(0x4287, 4),
            uint(0x4285, 2),
        ]
        .concat(),
    );
    head.extend(id(SEGMENT));
    let segment_size = head.len() as u64;
    // Eight bytes of size, the longest form, filled in at the end.
    head.extend_from_slice(&[0x01, 0, 0, 0, 0, 0, 0, 0]);
    let segment_start = head.len() as u64;

    let app = format!("soundcheck {}", env!("CARGO_PKG_VERSION"));
    let mut info = [
        uint(TIMESTAMP_SCALE, 1),
        text(MUXING_APP, &app),
        text(WRITING_APP, &app),
    ]
    .concat();
    // Past the Duration element's two-byte ID and one-byte size.
    let duration_in = info.len() + 3;
    info.extend(float(DURATION, 0.0));
    let (info, header) = wrapped(INFO, &info);
    let duration = head.len() as u64 + header as u64 + duration_in as u64;
    head.extend(info);

    let mut entry = [
        uint(TRACK_NUMBER, 1),
        uint(TRACK_UID, 1),
        uint(TRACK_TYPE, 2),
        uint(FLAG_LACING, 0),
        text(CODEC_ID, codec),
    ]
    .concat();
    let (private_element, private_header) = wrapped(CODEC_PRIVATE, private);
    let private_in_entry = entry.len() + private_header;
    entry.extend(private_element);
    let mut audio = [
        float(SAMPLING_FREQUENCY, f64::from(spec.rate)),
        uint(CHANNELS, spec.channels as u64),
    ]
    .concat();
    if let Sample::Int(bits) = spec.sample {
        audio.extend(uint(BIT_DEPTH, u64::from(bits)));
    }
    entry.extend(element(AUDIO, &audio));
    let (entry, entry_header) = wrapped(TRACK_ENTRY, &entry);
    let (tracks, tracks_header) = wrapped(TRACKS, &entry);
    let private_at = head.len() as u64 + (tracks_header + entry_header + private_in_entry) as u64;
    head.extend(tracks);

    if !carried.markers.is_empty() {
        head.extend(chapters(&carried.markers, spec.rate));
    }
    let tags = carried.simple();
    if !tags.is_empty() {
        let mut simple = Vec::new();
        for (name, value) in &tags {
            simple.extend(element(
                SIMPLE_TAG,
                &[text(TAG_NAME, name), text(TAG_STRING, value)].concat(),
            ));
        }
        // The whole recording, as an album holds its tracks.
        let targets = element(TARGETS, &uint(TARGET_TYPE_VALUE, 50));
        head.extend(element(TAGS, &element(TAG, &[targets, simple].concat())));
    }
    if !webm && !carried.pictures.is_empty() {
        let mut files = Vec::new();
        for (i, picture) in carried.pictures.iter().enumerate() {
            let mime = picture
                .mime_type()
                .map_or("application/octet-stream", |m| m.as_str());
            let ext = picture.mime_type().and_then(|m| m.ext()).unwrap_or("bin");
            let name = if i == 0 {
                format!("cover.{ext}")
            } else {
                format!("cover {}.{ext}", i + 1)
            };
            let description = picture.description().unwrap_or("cover").to_owned();
            files.extend(element(
                ATTACHED_FILE,
                &[
                    text(FILE_DESCRIPTION, &description),
                    text(FILE_NAME, &name),
                    text(FILE_MIME_TYPE, mime),
                    element(FILE_DATA, picture.data()),
                    uint(FILE_UID, i as u64 + 1),
                ]
                .concat(),
            ));
        }
        head.extend(element(ATTACHMENTS, &files));
    }
    (
        head,
        Places {
            segment_size,
            segment_start,
            duration,
            private: private_at,
        },
    )
}

fn chapters(markers: &[Marker], rate: u32) -> Vec<u8> {
    let mut atoms = Vec::new();
    for (i, m) in markers.iter().enumerate() {
        let mut atom = [
            uint(CHAPTER_UID, i as u64 + 1),
            uint(CHAPTER_TIME_START, ns(m.frame as u64, rate)),
        ]
        .concat();
        if m.length > 0 {
            atom.extend(uint(
                CHAPTER_TIME_END,
                ns((m.frame + m.length) as u64, rate),
            ));
        }
        atom.extend(element(
            CHAPTER_DISPLAY,
            &[text(CHAP_STRING, &m.label), text(CHAP_LANGUAGE, "und")].concat(),
        ));
        atoms.extend(element(CHAPTER_ATOM, &atom));
    }
    element(
        CHAPTERS,
        &element(EDITION_ENTRY, &[uint(EDITION_UID, 1), atoms].concat()),
    )
}

/// A block in a cluster of its own, starting at `frame`.
fn cluster(out: &mut impl Write, rate: u32, frame: u64, data: &[u8]) -> io::Result<()> {
    // Track 1, no offset from the cluster, a keyframe.
    let mut block = vec![0x81, 0, 0, 0x80];
    block.extend_from_slice(data);
    out.write_all(&element(
        CLUSTER,
        &[
            uint(TIMESTAMP, ns(frame, rate)),
            element(SIMPLE_BLOCK, &block),
        ]
        .concat(),
    ))
}

/// The segment's size and the duration, once the clusters are written.
fn close(out: &mut BufWriter<File>, places: &Places, frames: u64, rate: u32) -> io::Result<()> {
    out.flush()?;
    let file = out.get_mut();
    let end = file.stream_position()?;
    file.seek(SeekFrom::Start(places.segment_size))?;
    let mut size = (end - places.segment_start).to_be_bytes();
    size[0] = 0x01;
    file.write_all(&size)?;
    file.seek(SeekFrom::Start(places.duration))?;
    file.write_all(&(frames as f64 * 1e9 / f64::from(rate.max(1))).to_be_bytes())?;
    Ok(())
}

pub enum Writer {
    Flac {
        out: BufWriter<File>,
        encoder: Box<FrameEncoder>,
        places: Places,
        rate: u32,
        frame: u64,
    },
    /// The audio goes to an Ogg stream beside the new file first, as the
    /// track's header takes the stream's own headers, which come out of
    /// the encoder only as it ends.
    Vorbis {
        out: BufWriter<File>,
        stream: Box<Stream<BufWriter<File>>>,
        side: Side,
        spec: Spec,
        carried: Box<Carried>,
    },
}

impl Writer {
    pub fn flac(file: File, spec: Spec, carried: &Carried) -> Result<Self, String> {
        let encoder = FrameEncoder::new(&spec)?;
        // fLaC and STREAMINFO as the last metadata block, filled in at the
        // end.
        let mut private = b"fLaC".to_vec();
        private.extend_from_slice(&[0x80, 0, 0, 34]);
        private.extend_from_slice(&encoder.streaminfo());
        let (head, places) = head(&spec, carried, "A_FLAC", &private, false);
        let mut out = BufWriter::with_capacity(1 << 20, file);
        out.write_all(&head).map_err(failed)?;
        Ok(Self::Flac {
            out,
            encoder: Box::new(encoder),
            places,
            rate: spec.rate,
            frame: 0,
        })
    }

    pub fn vorbis(
        file: File,
        path: &Path,
        spec: Spec,
        quality: f32,
        carried: &Carried,
    ) -> Result<Self, String> {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let side = path.with_file_name(format!("{name}.ogg"));
        let sink = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&side)
            .map_err(failed)?;
        let stream = Stream::new(BufWriter::new(sink), &spec, quality)?;
        Ok(Self::Vorbis {
            out: BufWriter::with_capacity(1 << 20, file),
            stream: Box::new(stream),
            side: Side(side),
            spec,
            carried: Box::new(carried.clone()),
        })
    }
}

impl Encode for Writer {
    fn push(&mut self, frames: Frames<'_>) -> Result<(), String> {
        match (self, frames) {
            (
                Self::Flac {
                    out,
                    encoder,
                    rate,
                    frame,
                    ..
                },
                Frames::Int(samples),
            ) => {
                let rate = *rate;
                encoder.push(samples, &mut |block, n| {
                    cluster(out, rate, *frame, &block).map_err(failed)?;
                    *frame += n as u64;
                    Ok(())
                })
            }
            (Self::Vorbis { stream, .. }, Frames::Float(samples)) => stream.push(samples),
            _ => Err("the samples are not the codec's kind".into()),
        }
    }

    fn finish(self: Box<Self>) -> Result<(), String> {
        match *self {
            Self::Flac {
                mut out,
                mut encoder,
                places,
                rate,
                mut frame,
            } => {
                encoder.finish(&mut |block, n| {
                    cluster(&mut out, rate, frame, &block).map_err(failed)?;
                    frame += n as u64;
                    Ok(())
                })?;
                let info = encoder.streaminfo();
                (|| -> io::Result<()> {
                    close(&mut out, &places, frame, rate)?;
                    let file = out.get_mut();
                    // Past fLaC and the block header.
                    file.seek(SeekFrom::Start(places.private + 8))?;
                    file.write_all(&info)?;
                    file.sync_all()
                })()
                .map_err(failed)
            }
            Self::Vorbis {
                mut out,
                mut stream,
                side,
                spec,
                carried,
            } => {
                let written = (|| -> Result<(), String> {
                    let mut sink = stream.finish()?;
                    sink.flush().map_err(failed)?;
                    drop(sink);
                    let packets = vorbis::packets(&side.0)?;
                    // Xiph lacing: the count less one, then the sizes of
                    // all but the last header, then the headers.
                    let mut private = vec![2u8];
                    for header in &packets.headers[..2] {
                        let mut n = header.len();
                        while n >= 255 {
                            private.push(255);
                            n -= 255;
                        }
                        private.push(n as u8);
                    }
                    for header in &packets.headers {
                        private.extend_from_slice(header);
                    }
                    let (head, places) = head(&spec, &carried, "A_VORBIS", &private, true);
                    out.write_all(&head).map_err(failed)?;
                    let mut end = 0;
                    for (frame, data) in &packets.audio {
                        cluster(&mut out, spec.rate, *frame, data).map_err(failed)?;
                        end = *frame;
                    }
                    (|| -> io::Result<()> {
                        close(&mut out, &places, end, spec.rate)?;
                        out.get_mut().sync_all()
                    })()
                    .map_err(failed)
                })();
                drop(side);
                written
            }
        }
    }
}

/// The Ogg stream a WebM's audio goes to first, removed however the
/// conversion ends.
pub struct Side(PathBuf);

impl Drop for Side {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
