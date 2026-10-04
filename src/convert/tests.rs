use super::*;
use crate::audio::tests::temp_file;
use crate::tags;
use crate::wav::{self, Marker};
use lofty::config::WriteOptions;
use lofty::id3::v2::{AttachedPictureFrame, Frame, Id3v2Tag};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{ItemKey, TagExt};

/// How a test WAV keeps its samples.
#[derive(Clone, Copy)]
enum Kind {
    Int(u32),
    Float,
}

/// A recording's samples: a sweep and a little noise in each channel,
/// different in each, at `peak` of full scale.
fn samples(frames: usize, channels: usize, rate: u32, peak: f64) -> Vec<f64> {
    let mut noise = 0x1234_5678u32;
    let mut out = Vec::with_capacity(frames * channels);
    for i in 0..frames {
        let t = i as f64 / f64::from(rate);
        for c in 0..channels {
            noise ^= noise << 13;
            noise ^= noise >> 17;
            noise ^= noise << 5;
            let n = f64::from(noise) / f64::from(u32::MAX) - 0.5;
            let f = 300.0 * (c + 1) as f64 + 2000.0 * t;
            let s = 0.8 * (std::f64::consts::TAU * f * t).sin() + 0.1 * n;
            out.push(s * peak / 0.9);
        }
    }
    out
}

fn chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

/// A WAV of `samples`, with `before` chunks ahead of the audio and `after`
/// ones behind it.
fn wav_file(
    rate: u32,
    channels: usize,
    kind: Kind,
    samples: &[f64],
    before: &[(&[u8; 4], Vec<u8>)],
    after: &[(&[u8; 4], Vec<u8>)],
) -> Vec<u8> {
    let (tag, bits) = match kind {
        Kind::Int(bits) => (1u16, bits),
        Kind::Float => (3, 32),
    };
    let bytes = bits.div_ceil(8);
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&tag.to_le_bytes());
    fmt.extend_from_slice(&(channels as u16).to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * bytes * channels as u32).to_le_bytes());
    fmt.extend_from_slice(&((bytes * channels as u32) as u16).to_le_bytes());
    fmt.extend_from_slice(&(bits as u16).to_le_bytes());
    let mut data = Vec::new();
    for &s in samples {
        match kind {
            Kind::Float => data.extend_from_slice(&(s as f32).to_le_bytes()),
            Kind::Int(bits) => {
                let full = (1i64 << (bits - 1)) as f64;
                let q = (s * full).round().clamp(-full, full - 1.0) as i64;
                match bits {
                    8 => data.push((q + 128) as u8),
                    16 => data.extend_from_slice(&(q as i16).to_le_bytes()),
                    24 => data.extend_from_slice(&(q as i32).to_le_bytes()[..3]),
                    _ => data.extend_from_slice(&(q as i32).to_le_bytes()),
                }
            }
        }
    }
    let mut body = b"WAVE".to_vec();
    chunk(&mut body, b"fmt ", &fmt);
    for (id, b) in before {
        chunk(&mut body, id, b);
    }
    chunk(&mut body, b"data", &data);
    for (id, b) in after {
        chunk(&mut body, id, b);
    }
    let mut file = b"RIFF".to_vec();
    file.extend_from_slice(&(body.len() as u32).to_le_bytes());
    file.extend(body);
    file
}

/// A small PNG, as a cover picture.
fn png() -> Vec<u8> {
    let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let ihdr = [0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0];
    b.extend_from_slice(&13u32.to_be_bytes());
    b.extend_from_slice(b"IHDR");
    b.extend_from_slice(&ihdr);
    b.extend_from_slice(&[0x90, 0x77, 0x53, 0xDE]);
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(b"IEND");
    b.extend_from_slice(&[0xAE, 0x42, 0x60, 0x82]);
    b
}

fn picture() -> Picture {
    Picture::unchecked(png())
        .pic_type(PictureType::CoverFront)
        .mime_type(MimeType::Png)
        .build()
}

fn id3_chunk() -> Vec<u8> {
    let mut tag = Id3v2Tag::default();
    for (key, value) in [
        ("TIT2", "Pier at dusk"),
        ("TXXX:Species", "Myotis daubentonii"),
        ("TXXX:Location", "Loobu"),
    ] {
        tag.insert(tags::new_id3v2_frame(key, value, lofty::TextEncoding::UTF8).unwrap());
    }
    tag.insert(Frame::Picture(AttachedPictureFrame::new(
        lofty::TextEncoding::UTF8,
        picture(),
    )));
    let mut bytes = Vec::new();
    tag.dump_to(&mut bytes, WriteOptions::default()).unwrap();
    bytes
}

fn bext(description: &str, reference: u64) -> Vec<u8> {
    let mut b = vec![0u8; 602];
    b[..description.len()].copy_from_slice(description.as_bytes());
    b[256..264].copy_from_slice(b"Zoom F3 ");
    b[320..330].copy_from_slice(b"2026-05-14");
    b[330..338].copy_from_slice(b"21:30:05");
    b[338..346].copy_from_slice(&reference.to_le_bytes());
    b[346] = 1;
    b.extend_from_slice(b"A=PCM,F=48000,W=24,M=stereo,T=Zoom F3\r\n");
    b
}

fn markers() -> Vec<Marker> {
    vec![
        wav::tests::marker(1, 4_800, 0, "owl"),
        Marker {
            note: "two calls".into(),
            ..wav::tests::marker(2, 24_000, 9_600, "bat pass")
        },
    ]
}

/// A WAV with every kind of metadata a WAV keeps: Broadcast WAV, iXML,
/// INFO, GUANO, markers, an ID3 tag with a picture, and a vendor chunk.
fn tagged_wav(name: &str, rate: u32, channels: usize, kind: Kind, peak: f64) -> PathBuf {
    let frames = rate as usize; // a second
    let s = samples(frames, channels, rate, peak);
    let info = wav::info_body(
        &[
            (*b"INAM", "Pier at dusk".into()),
            (*b"ICMT", "rain later".into()),
            (*b"ITCH", "Erko".into()),
        ],
        &[],
    );
    let (cue, adtl) = wav::mark_bodies(&markers(), &wav::StoredMarks::default());
    let ixml = b"<BWFXML><NOTE>frogs and rain</NOTE><SPEED><TIMECODE_RATE>25/1</TIMECODE_RATE></SPEED></BWFXML>".to_vec();
    let guano = b"GUANO|Version: 1.0\nMake: Wildlife Acoustics\nSpecies Manual ID: Myotis daubentonii\nLoc Position: 59.43 24.75\n".to_vec();
    temp_file(
        name,
        &wav_file(
            rate,
            channels,
            kind,
            &s,
            &[
                (b"bext", bext("sSPEED=048.000-ND\r\nsTAKE=01", 77_000)),
                (b"LIST", info),
                (b"zoom", vec![1, 2, 3, 4]),
            ],
            &[
                (b"cue ", cue),
                (b"LIST", adtl.unwrap()),
                (b"iXML", ixml),
                (b"guan", guano),
                (b"id3 ", id3_chunk()),
            ],
        ),
    )
}

fn plan(from: &Path, target: &Target) -> Plan {
    let header = header(from).unwrap();
    let (shape, changes) = shape(&header, target).unwrap();
    Plan {
        from: from.to_owned(),
        to: path_for(from, target.format, &HashSet::new()),
        shape,
        changes,
    }
}

fn run(from: &Path, target: &Target) -> PathBuf {
    let plan = plan(from, target);
    let gain = if plan.shape.needs_peak() {
        fit(peak(from, &AtomicBool::new(false)).unwrap())
    } else {
        None
    };
    convert(
        &plan,
        target,
        gain,
        &AtomicBool::new(false),
        &AtomicU32::new(0),
    )
    .unwrap_or_else(|e| panic!("{:?}: {e}", target.format))
}

fn target(format: Format) -> Target {
    Target {
        format,
        ..Target::default()
    }
}

/// Every frame of `path`, as the window reads it.
fn frames(path: &Path) -> (Vec<f32>, usize, u32) {
    let opened = audio::open(path).unwrap();
    let channels = usize::from(opened.info.channels);
    let mut reader = Reader::open(&opened.source, channels).unwrap();
    let mut all = Vec::new();
    let mut buffer = vec![0.0; 4096 * channels];
    loop {
        let got = reader.read(all.len() / channels, &mut buffer).unwrap();
        all.extend_from_slice(&buffer[..got * channels]);
        if got < 4096 {
            break;
        }
    }
    (all, channels, opened.info.sample_rate)
}

fn values(path: &Path) -> Vec<String> {
    let opened = audio::open(path).unwrap();
    carry::read(path, &opened)
        .unwrap()
        .fields
        .into_iter()
        .map(|f| f.value)
        .collect()
}

#[test]
fn every_lossless_format_gives_the_samples_back_and_every_tag_and_marker() {
    let from = tagged_wav("all-lossless.wav", 48_000, 2, Kind::Int(24), 0.9);
    let (original, ..) = frames(&from);
    for format in [
        Format::Wav,
        Format::Aiff,
        Format::Caf,
        Format::Flac,
        Format::M4a,
        Format::Mka,
    ] {
        let new = run(&from, &target(format));
        let (got, channels, rate) = frames(&new);
        assert_eq!((channels, rate), (2, 48_000), "{format:?}");
        assert!(got == original, "{format:?}: the samples differ");
        let vals = values(&new);
        for wanted in [
            "Myotis daubentonii",
            "Loobu",
            "Pier at dusk",
            "Erko",
            "Wildlife Acoustics",
        ] {
            assert!(
                vals.iter().any(|v| v == wanted),
                "{format:?}: {wanted} missing from {vals:?}"
            );
        }
        // Back to WAV, the chunks come back.
        let back = run(&new, &target(Format::Wav));
        let w = wav::parse(&mut File::open(&back).unwrap()).unwrap();
        assert_eq!(
            w.bext.as_ref().map(|b| b.description.as_str()),
            Some("sSPEED=048.000-ND\r\nsTAKE=01"),
            "{format:?}"
        );
        assert_eq!(
            w.bext.as_ref().unwrap().time_reference,
            77_000,
            "{format:?}"
        );
        assert!(
            w.guano
                .as_deref()
                .unwrap()
                .contains("Species Manual ID: Myotis daubentonii"),
            "{format:?}"
        );
        assert!(
            w.ixml.as_deref().unwrap().contains("frogs and rain"),
            "{format:?}"
        );
        assert!(
            w.info.iter().any(|(id, v)| id == b"ITCH" && v == "Erko"),
            "{format:?}: {:?}",
            w.info
        );
        let marks = w.cues.clone().unwrap_or_default();
        assert_eq!(marks.len(), 2, "{format:?}: {marks:?}");
        assert_eq!(marks[1].label, "bat pass", "{format:?}");
        let (again, ..) = frames(&back);
        assert!(
            again == original,
            "{format:?}: the samples differ after coming back"
        );
        for p in [&new, &back] {
            fs::remove_file(p).unwrap();
        }
    }
    fs::remove_file(&from).unwrap();
}

fn rms(s: &[f32]) -> f64 {
    (s.iter().map(|&x| f64::from(x) * f64::from(x)).sum::<f64>() / s.len().max(1) as f64).sqrt()
}

#[test]
fn lossy_formats_keep_the_length_the_tags_and_the_markers() {
    for (name, rate, channels) in [
        ("lossy-stereo.wav", 48_000, 2),
        ("lossy-bat.wav", 384_000, 1),
    ] {
        let from = tagged_wav(name, rate, channels, Kind::Int(16), 0.5);
        let (original, ..) = frames(&from);
        for format in [Format::Mp3, Format::Ogg, Format::Webm] {
            let new = run(&from, &target(format));
            let (got, ch, new_rate) = frames(&new);
            assert_eq!(ch, channels, "{format:?}");
            let expected = match format {
                Format::Mp3 => mp3_rate(rate),
                _ => vorbis_rate(rate),
            };
            assert_eq!(new_rate, expected, "{format:?}");
            let level = rms(&got) / rms(&original);
            assert!(
                (0.8..1.2).contains(&level),
                "{format:?} from {name}: level {level}"
            );
            let vals = values(&new);
            for wanted in ["Myotis daubentonii", "Wildlife Acoustics", "Pier at dusk"] {
                assert!(
                    vals.iter().any(|v| v == wanted),
                    "{format:?}: {wanted} missing"
                );
            }
            let back = run(&new, &target(Format::Wav));
            let w = wav::parse(&mut File::open(&back).unwrap()).unwrap();
            assert!(
                w.guano
                    .as_deref()
                    .unwrap_or_default()
                    .contains("Make: Wildlife Acoustics"),
                "{format:?}"
            );
            assert_eq!(
                w.bext.as_ref().map(|b| b.originator.as_str()),
                Some("Zoom F3"),
                "{format:?}"
            );
            // The time reference counts at the new rate.
            let reference = w.bext.as_ref().unwrap().time_reference;
            assert_eq!(
                reference,
                77_000 * u64::from(expected) / u64::from(rate),
                "{format:?}"
            );
            let marks = w.cues.clone().unwrap_or_default();
            assert_eq!(marks.len(), 2, "{format:?}");
            let at = 24_000 * expected as usize / rate as usize;
            assert!(
                marks[1].frame.abs_diff(at) <= expected as usize / 1000,
                "{format:?}: {marks:?}"
            );
            assert_eq!(marks[1].label, "bat pass", "{format:?}");
            for p in [&new, &back] {
                fs::remove_file(p).unwrap();
            }
        }
        fs::remove_file(&from).unwrap();
    }
}

#[test]
fn a_float_file_past_full_scale_is_lowered_to_fit_whole_numbers_and_says_so() {
    let from = tagged_wav("loud-float.wav", 48_000, 2, Kind::Float, 2.0);
    let peak = peak(&from, &AtomicBool::new(false)).unwrap();
    assert!(peak > 1.5 && peak < 2.1, "{peak}");
    let gain = fit(peak).unwrap();
    let flac = run(&from, &target(Format::Flac));
    let (got, ..) = frames(&flac);
    let loudest = got.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(loudest <= 1.0 && loudest > 0.99, "{loudest}");
    let vals = values(&flac);
    assert!(vals.contains(&decibels(gain)), "{vals:?}");
    // Floats stay floats where the format keeps them, and nothing is lowered.
    let wav = run(
        &from,
        &Target {
            depth: Depth::AsFile,
            ..target(Format::Wav)
        },
    );
    let w = wav::parse(&mut File::open(&wav).unwrap()).unwrap();
    assert_eq!(w.kind, wav::SampleKind::F32);
    let (original, ..) = frames(&from);
    assert!(frames(&wav).0 == original);
    for p in [&from, &flac, &wav] {
        fs::remove_file(p).unwrap();
    }
}

#[test]
fn widths_and_channel_counts_come_back_exactly() {
    let cases: &[(Kind, usize, u32)] = &[
        (Kind::Int(8), 1, 8_000),
        (Kind::Int(16), 6, 44_100),
        (Kind::Int(24), 8, 96_000),
        (Kind::Int(32), 2, 48_000),
        (Kind::Int(24), 1, 384_000),
        (Kind::Int(16), 3, 705_600),
    ];
    for (i, &(kind, channels, rate)) in cases.iter().enumerate() {
        let s = samples(rate as usize / 4, channels, rate, 0.7);
        let from = temp_file(
            &format!("width-{i}.wav"),
            &wav_file(rate, channels, kind, &s, &[], &[]),
        );
        let (original, ..) = frames(&from);
        for format in [
            Format::Flac,
            Format::M4a,
            Format::Mka,
            Format::Aiff,
            Format::Caf,
            Format::Wav,
        ] {
            if format.most_channels() < channels {
                continue;
            }
            if matches!(format, Format::Flac | Format::Mka) && rate > 655_350 {
                let header = header(&from).unwrap();
                assert!(shape(&header, &target(format)).is_err());
                continue;
            }
            let new = run(&from, &target(format));
            let (got, ch, r) = frames(&new);
            assert_eq!((ch, r), (channels, rate), "{format:?} case {i}");
            assert!(got == original, "{format:?} case {i}: the samples differ");
            fs::remove_file(&new).unwrap();
        }
        fs::remove_file(&from).unwrap();
    }
}

#[test]
fn a_new_name_never_takes_another_file_s() {
    let from = temp_file("naming.wav", b"x");
    let first = path_for(&from, Format::Flac, &HashSet::new());
    assert_eq!(
        first.file_name().unwrap().to_string_lossy(),
        from.file_stem().unwrap().to_string_lossy() + ".flac"
    );
    fs::write(&first, b"taken").unwrap();
    let second = path_for(&from, Format::Flac, &HashSet::new());
    assert!(second.to_string_lossy().ends_with(" 2.flac"), "{second:?}");
    let taken: HashSet<PathBuf> = [second.clone()].into();
    assert!(
        path_for(&from, Format::Flac, &taken)
            .to_string_lossy()
            .ends_with(" 3.flac")
    );
    // A WAV converted to WAV never becomes the original.
    assert_ne!(path_for(&from, Format::Wav, &HashSet::new()), from);
    let temp = temp_file("placing.tmp", b"new");
    let placed = place(&temp, &first).unwrap();
    assert_ne!(placed, first);
    assert_eq!(fs::read(&first).unwrap(), b"taken");
    assert_eq!(fs::read(&placed).unwrap(), b"new");
    for p in [&from, &first, &placed] {
        fs::remove_file(p).unwrap();
    }
}

/// A copy of the file `name` in testdata, for a test to convert.
fn fixture(name: &str) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name);
    temp_file(&format!("convert-{name}"), &fs::read(from).unwrap())
}

#[test]
fn files_tagged_by_other_programs_convert_to_every_format_with_every_value() {
    for name in [
        "tagged.mp3",
        "tagged.aiff",
        "tagged.flac",
        "tagged.m4a",
        "tagged.ogg",
    ] {
        let from = fixture(name);
        let before = values(&from);
        assert!(before.iter().any(|v| v == "Pier"), "{name}: {before:?}");
        for format in Format::ALL {
            let header = header(&from).unwrap();
            if shape(&header, &target(format)).is_err() {
                continue;
            }
            // The conversion reads its result back, and fails on any value
            // missing.
            let new = run(&from, &target(format));
            let after = values(&new);
            for v in &before {
                assert!(
                    after.contains(v),
                    "{name} to {format:?}: {v:?} missing from {after:?}"
                );
            }
            fs::remove_file(&new).unwrap();
        }
        fs::remove_file(&from).unwrap();
    }
}

#[test]
fn two_tags_disagreeing_on_a_field_keep_both_values() {
    use lofty::ape::{ApeItem, ApeTag};
    use lofty::tag::ItemValue;
    let wav = tagged_wav("disagree.wav", 48_000, 1, Kind::Int(16), 0.5);
    let mp3 = run(&wav, &target(Format::Mp3));
    let mut ape = ApeTag::default();
    ape.insert(
        ApeItem::new(
            "Species".into(),
            ItemValue::Text("Pipistrellus pygmaeus".into()),
        )
        .unwrap(),
    );
    ape.save_to_path(&mp3, WriteOptions::default()).unwrap();
    let back = run(&mp3, &target(Format::Wav));
    let id3 = tags::wav_id3v2(&back).unwrap().unwrap();
    let species = id3.get_user_text("Species").unwrap();
    assert!(
        species.contains("Myotis daubentonii") && species.contains("Pipistrellus pygmaeus"),
        "{species:?}"
    );
    let caf = run(&mp3, &target(Format::Caf));
    assert!(
        values(&caf).contains(&"Myotis daubentonii; Pipistrellus pygmaeus".to_owned()),
        "a CAF keeps one value a name"
    );
    for p in [&wav, &mp3, &back, &caf] {
        fs::remove_file(p).unwrap();
    }
}

/// A WAV whose ID3 tag numbers its track and disc as an MP3 tagger does.
fn numbered_wav(name: &str) -> PathBuf {
    let mut tag = Id3v2Tag::default();
    for (key, value) in [("TIT2", "Pier at dusk"), ("TRCK", "3/12"), ("TPOS", "1/2")] {
        tag.insert(tags::new_id3v2_frame(key, value, lofty::TextEncoding::UTF8).unwrap());
    }
    let mut id3 = Vec::new();
    tag.dump_to(&mut id3, WriteOptions::default()).unwrap();
    let s = samples(48_000, 2, 48_000, 0.5);
    temp_file(
        name,
        &wav_file(48_000, 2, Kind::Int(16), &s, &[], &[(b"id3 ", id3)]),
    )
}

/// The values of an MP3's ID3 frame `id`, one by one.
fn id3_text(mp3: &Path, id: &str) -> Vec<String> {
    use lofty::file::AudioFile;
    let mut file = fs::File::open(mp3).unwrap();
    let mpeg =
        lofty::mpeg::MpegFile::read_from(&mut file, lofty::config::ParseOptions::new()).unwrap();
    let id = lofty::id3::v2::FrameId::new(id).unwrap();
    mpeg.id3v2()
        .and_then(|t| t.get_text(&id))
        .map_or_else(Vec::new, |t| t.split('\0').map(str::to_owned).collect())
}

/// The values of the fields of `path` that lofty knows as `key`.
fn known(path: &Path, key: ItemKey) -> Vec<String> {
    let opened = audio::open(path).unwrap();
    carry::read(path, &opened)
        .unwrap()
        .fields
        .into_iter()
        .filter(|f| f.name == carry::Name::Known(key))
        .map(|f| f.value)
        .collect()
}

#[test]
fn track_and_disc_numbers_keep_their_meaning_in_every_format() {
    let wav = numbered_wav("numbered.wav");
    let mut made = Vec::new();
    for format in Format::ALL {
        let to = run(&wav, &target(format));
        for (key, value) in [
            (ItemKey::TrackNumber, "3"),
            (ItemKey::TrackTotal, "12"),
            (ItemKey::DiscNumber, "1"),
            (ItemKey::DiscTotal, "2"),
        ] {
            assert_eq!(known(&to, key), [value], "{format:?} {key:?}");
        }
        let mp3 = run(&to, &target(Format::Mp3));
        assert_eq!(id3_text(&mp3, "TRCK"), ["3/12"], "{format:?}");
        assert_eq!(id3_text(&mp3, "TPOS"), ["1/2"], "{format:?}");
        made.extend([to, mp3]);
    }
    for p in made.iter().chain([&wav]) {
        fs::remove_file(p).unwrap();
    }
}

#[test]
fn a_track_named_or_disagreed_on_keeps_every_number_in_an_m4a() {
    use lofty::ape::{ApeItem, ApeTag};
    use lofty::file::AudioFile;
    use lofty::tag::{Accessor, ItemValue};
    let wav = numbered_wav("vinyl.wav");
    let mp3 = run(&wav, &target(Format::Mp3));
    let mut ape = ApeTag::default();
    for (key, value) in [("Track", "4"), ("Disc", "B")] {
        ape.insert(ApeItem::new(key.into(), ItemValue::Text(value.into())).unwrap());
    }
    ape.save_to_path(&mp3, WriteOptions::default()).unwrap();
    let m4a = run(&mp3, &target(Format::M4a));
    let mut file = fs::File::open(&m4a).unwrap();
    let mp4 =
        lofty::mp4::Mp4File::read_from(&mut file, lofty::config::ParseOptions::new()).unwrap();
    let ilst = mp4.ilst().unwrap();
    assert_eq!(
        (
            ilst.track(),
            ilst.track_total(),
            ilst.disk(),
            ilst.disk_total()
        ),
        (Some(3), Some(12), Some(1), Some(2))
    );
    assert_eq!(known(&m4a, ItemKey::TrackNumber), ["3", "4"]);
    assert_eq!(known(&m4a, ItemKey::DiscNumber), ["1", "B"]);
    let back = run(&m4a, &target(Format::Mp3));
    assert_eq!(id3_text(&back, "TRCK"), ["3/12", "4"]);
    assert_eq!(id3_text(&back, "TPOS"), ["1/2", "B"]);
    for p in [&wav, &mp3, &m4a, &back] {
        fs::remove_file(p).unwrap();
    }
}
