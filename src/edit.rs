//! What the Metadata and Markers views can change in a WAV file. Values are
//! kept as the text their fields show; saving writes only what was changed,
//! and every field left alone goes back byte for byte.

use std::ops::Range;

use crate::meta::{self, Element, Span};
use crate::save::Changes;
use crate::wav::{Bext, Marker, Wav};

/// The Broadcast WAV text fields: label, and bytes in the chunk.
pub const BEXT_FIELDS: [(&str, Range<usize>); 5] = [
    ("Description", 0..256),
    ("Originator", 256..288),
    ("Reference", 288..320),
    ("Date", 320..330),
    ("Time", 330..338),
];
const TIME_REFERENCE: Range<usize> = 338..346;
const BEXT_SIZE: usize = 602;

#[derive(Debug, Clone, PartialEq)]
pub struct Edits {
    /// In `BEXT_FIELDS` order.
    pub bext: [String; 5],
    /// Time of day at the first sample, `hh:mm:ss.sss`, or empty.
    pub start: String,
    pub coding_history: String,
    /// RIFF INFO: the fields editors know, then any others the file has.
    pub info: Vec<([u8; 4], String)>,
    /// iXML fields: label, and value.
    pub ixml: Vec<(String, String)>,
    pub markers: Vec<Marker>,
    held: Held,
}

/// What the file held, for writing changes into.
#[derive(Debug, Clone, PartialEq)]
struct Held {
    sample_rate: u32,
    bext: Option<Vec<u8>>,
    ixml: Option<String>,
    /// Where each iXML field's text sits in `ixml`, and its element's name.
    ixml_spans: Vec<(Span, String)>,
}

impl Edits {
    pub fn from_wav(w: &Wav) -> Self {
        let bext = w.bext.as_ref();
        let field = |get: fn(&Bext) -> &str| bext.map(get).map(lf).unwrap_or_default();
        let root = w.ixml.as_deref().and_then(meta::parse_xml);
        let mut leaves = Vec::new();
        if let Some(root) = &root {
            for child in numbered(&root.children) {
                collect_leaves(child.0, &child.1, &mut leaves);
            }
        }
        // Each field editors know, with the file's first entry for it, then
        // every entry left in the file's order, a second of a kind included.
        let mut unused: Vec<&([u8; 4], String)> = w.info.iter().collect();
        let mut info: Vec<([u8; 4], String)> = meta::INFO_FIELDS
            .iter()
            .map(|(id, _)| {
                let value = unused
                    .iter()
                    .position(|(k, _)| k == *id)
                    .map(|i| unused.remove(i).1.clone());
                (**id, value.unwrap_or_default())
            })
            .collect();
        info.extend(unused.into_iter().cloned());
        Self {
            bext: [
                field(|b| &b.description),
                field(|b| &b.originator),
                field(|b| &b.originator_reference),
                field(|b| &b.origination_date),
                field(|b| &b.origination_time),
            ],
            start: bext
                .filter(|b| b.time_reference > 0)
                .map(|b| clock(b.time_reference, w.sample_rate))
                .unwrap_or_default(),
            coding_history: field(|b| &b.coding_history),
            info,
            ixml: leaves
                .iter()
                .map(|(l, v, ..)| (l.clone(), v.clone()))
                .collect(),
            // A `cue ` chunk, even an empty one, is where marks are kept;
            // without one, sync points in iXML (the original app's
            // annotations) are shown instead.
            markers: w.cues.clone().unwrap_or_else(|| sync_points(root.as_ref())),
            held: Held {
                sample_rate: w.sample_rate,
                bext: bext.map(|b| b.raw.clone()),
                ixml: w.ixml.clone(),
                ixml_spans: leaves.into_iter().map(|(_, _, s, n)| (s, n)).collect(),
            },
        }
    }

    pub fn next_marker_id(&self) -> u32 {
        self.markers.iter().map(|m| m.id).max().unwrap_or(0) + 1
    }

    /// What saving has to write to turn `saved`, the file as it is, into
    /// this.
    pub fn changes(&self, saved: &Edits) -> Result<Changes, String> {
        let mut changes = Changes::default();
        let bext = (&self.bext, &self.start, &self.coding_history);
        if bext != (&saved.bext, &saved.start, &saved.coding_history) {
            changes.bext = Some(self.bext_chunk(saved)?);
        }
        if self.info != saved.info {
            changes.info = Some(
                self.info
                    .iter()
                    .map(|(id, v)| (*id, v.trim().to_owned()))
                    .filter(|(_, v)| !v.is_empty())
                    .collect(),
            );
        }
        if self.ixml != saved.ixml {
            changes.ixml = Some(self.ixml_text(saved));
        }
        if self.markers != saved.markers {
            let mut markers: Vec<Marker> = self
                .markers
                .iter()
                .map(|m| Marker {
                    label: m.label.trim().to_owned(),
                    note: m.note.trim().to_owned(),
                    ..m.clone()
                })
                .collect();
            markers.sort_by_key(|m| (m.frame, m.id));
            changes.markers = Some(markers);
        }
        Ok(changes)
    }

    fn bext_chunk(&self, saved: &Edits) -> Result<Vec<u8>, String> {
        let mut raw = self.held.bext.clone().unwrap_or_else(|| {
            let mut new = vec![0; BEXT_SIZE];
            new[346..348].copy_from_slice(&1u16.to_le_bytes());
            new
        });
        if raw.len() < BEXT_SIZE {
            raw.resize(BEXT_SIZE, 0);
        }
        for ((label, bytes), (new, old)) in
            BEXT_FIELDS.iter().zip(self.bext.iter().zip(&saved.bext))
        {
            if new == old {
                continue;
            }
            let text = crlf(new);
            if text.len() > bytes.len() {
                return Err(format!(
                    "{label} holds {} bytes and has {}: letters outside plain English take two or more",
                    bytes.len(),
                    text.len()
                ));
            }
            raw[bytes.clone()].fill(0);
            raw[bytes.start..bytes.start + text.len()].copy_from_slice(text.as_bytes());
        }
        if self.start != saved.start {
            let samples = samples(&self.start, self.held.sample_rate)?;
            raw[TIME_REFERENCE].copy_from_slice(&samples.to_le_bytes());
        }
        if self.coding_history != saved.coding_history {
            raw.truncate(BEXT_SIZE);
            raw.extend_from_slice(crlf(&self.coding_history).as_bytes());
        }
        Ok(raw)
    }

    fn ixml_text(&self, saved: &Edits) -> String {
        let mut text = self.held.ixml.clone().unwrap_or_default();
        let mut changed: Vec<(&(Span, String), &str)> = self
            .held
            .ixml_spans
            .iter()
            .zip(self.ixml.iter().zip(&saved.ixml))
            .filter(|(_, (new, old))| new.1 != old.1)
            .map(|(span, (new, _))| (span, new.1.as_str()))
            .collect();
        // From the end back, so the spans still to do stay where they were.
        changed.sort_by_key(|((span, _), _)| std::cmp::Reverse(start(span)));
        for ((span, name), value) in changed {
            match span {
                Span::Content(range) => text.replace_range(range.clone(), &meta::escape(value)),
                Span::Empty(range) => text.replace_range(
                    range.clone(),
                    &format!("<{name}>{}</{name}>", meta::escape(value)),
                ),
            }
        }
        text
    }
}

fn start(span: &Span) -> usize {
    match span {
        Span::Content(r) | Span::Empty(r) => r.start,
    }
}

/// Children paired with their labels; a name several siblings share gets a
/// number, so each field's label is its own.
fn numbered(children: &[Element]) -> Vec<(&Element, String)> {
    children
        .iter()
        .enumerate()
        .map(|(i, child)| {
            let same = children.iter().filter(|c| c.name == child.name).count();
            let label = if same > 1 {
                let nth = children[..i]
                    .iter()
                    .filter(|c| c.name == child.name)
                    .count()
                    + 1;
                format!("{} {nth}", child.name)
            } else {
                child.name.clone()
            };
            (child, label)
        })
        .collect()
}

type Leaf = (String, String, Span, String);

fn collect_leaves(element: &Element, label: &str, out: &mut Vec<Leaf>) {
    if element.children.is_empty() {
        if let Some(span) = &element.span {
            let value = element.text.trim().to_owned();
            out.push((label.to_owned(), value, span.clone(), element.name.clone()));
        }
        return;
    }
    for (child, name) in numbered(&element.children) {
        collect_leaves(child, &format!("{label} / {name}"), out);
    }
}

/// iXML `SYNC_POINT`s as markers.
fn sync_points(root: Option<&Element>) -> Vec<Marker> {
    let Some(list) = root.and_then(|r| r.child("SYNC_POINT_LIST")) else {
        return Vec::new();
    };
    let points = list.children.iter().filter(|c| c.name == "SYNC_POINT");
    let mut markers: Vec<Marker> = points
        .filter_map(|point| {
            let number = |name| {
                point
                    .child(name)
                    .and_then(|c| c.text.trim().parse::<u64>().ok())
            };
            let frame = (number("SYNC_POINT_HIGH").unwrap_or(0) << 32) | number("SYNC_POINT_LOW")?;
            Some((frame, number("SYNC_POINT_EVENT_DURATION"), point))
        })
        .zip(1..)
        .map(|((frame, length, point), id)| Marker {
            id,
            frame: frame as usize,
            length: length.unwrap_or(0) as usize,
            label: point
                .child("SYNC_POINT_COMMENT")
                .map(|c| c.text.trim().to_owned())
                .unwrap_or_default(),
            note: String::new(),
        })
        .collect();
    markers.sort_by_key(|m| (m.frame, m.id));
    markers
}

/// Line ends as a text field shows them.
fn lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Line ends as Broadcast WAV writes them.
fn crlf(text: &str) -> String {
    lf(text).replace('\n', "\r\n")
}

/// `samples` since midnight as `hh:mm:ss.sss`.
fn clock(samples: u64, rate: u32) -> String {
    let ms = (samples as u128 * 1000 / u128::from(rate.max(1))) as u64 % 86_400_000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// `hh:mm:ss`, with or without a fraction, as samples since midnight.
fn samples(text: &str, rate: u32) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    let wrong = || format!("Start {text} is not a time of day like 13:45:10.250");
    let mut parts = text.split(':');
    let (Some(h), Some(m), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(wrong());
    };
    let (h, m): (u64, u64) = (
        h.parse().map_err(|_| wrong())?,
        m.parse().map_err(|_| wrong())?,
    );
    let s: f64 = s.parse().map_err(|_| wrong())?;
    if h >= 24 || m >= 60 || !(0.0..60.0).contains(&s) {
        return Err(wrong());
    }
    Ok(((h * 3600 + m * 60) as f64 * f64::from(rate) + s * f64::from(rate)).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{self, tests::build};
    use std::io::Cursor;

    fn bext(description: &str, originator: &str, reference: u64) -> Vec<u8> {
        let mut b = vec![0u8; BEXT_SIZE];
        b[..description.len()].copy_from_slice(description.as_bytes());
        b[256..256 + originator.len()].copy_from_slice(originator.as_bytes());
        b[338..346].copy_from_slice(&reference.to_le_bytes());
        b[346] = 1;
        b[400] = 0xAB;
        b
    }

    fn parsed(extra: &[(&[u8; 4], Vec<u8>)]) -> Wav {
        wav::parse(&mut Cursor::new(build(false, extra, &[0; 16]))).unwrap()
    }

    #[test]
    fn nothing_changed_writes_nothing() {
        let ixml = b"<BWFXML><NOTE>x</NOTE></BWFXML>".to_vec();
        let w = parsed(&[(b"bext", bext("d", "o", 5)), (b"iXML", ixml)]);
        let saved = Edits::from_wav(&w);
        assert_eq!(saved.clone().changes(&saved).unwrap(), Changes::default());
    }

    #[test]
    fn a_changed_bext_field_leaves_the_rest_of_the_chunk_alone() {
        let original = bext("sSPEED=048.000-ND\r\nsTAKE=01", "Zoom F3", 48_000 * 3600);
        let w = parsed(&[(b"bext", original.clone())]);
        let saved = Edits::from_wav(&w);
        assert_eq!(saved.bext[0], "sSPEED=048.000-ND\nsTAKE=01");
        assert_eq!(saved.start, "01:00:00.000");
        let mut edits = saved.clone();
        edits.bext[1] = "Erko".into();
        let chunk = edits.changes(&saved).unwrap().bext.unwrap();
        assert_eq!(&chunk[256..260], b"Erko");
        assert!(chunk[260..288].iter().all(|&b| b == 0));
        assert_eq!(chunk[..256], original[..256]);
        assert_eq!(chunk[288..], original[288..]);
    }

    #[test]
    fn text_too_long_for_its_field_is_refused_rather_than_cut() {
        let saved = Edits::from_wav(&parsed(&[]));
        let mut edits = saved.clone();
        edits.bext[1] = "Kõrvemaa helisalvestaja ja töötuba".into();
        let error = edits.changes(&saved).unwrap_err();
        assert!(error.starts_with("Originator holds 32 bytes"), "{error}");
    }

    #[test]
    fn a_start_time_becomes_samples_since_midnight() {
        let saved = Edits::from_wav(&parsed(&[]));
        let mut edits = saved.clone();
        edits.start = "16:04:25.5".into();
        let chunk = edits.changes(&saved).unwrap().bext.unwrap();
        let reference = u64::from_le_bytes(chunk[338..346].try_into().unwrap());
        assert_eq!(reference, (16 * 3600 + 4 * 60 + 25) * 48_000 + 24_000);
        assert_eq!(clock(reference, 48_000), "16:04:25.500");
        edits.start = "25:00:00".into();
        assert!(edits.changes(&saved).is_err());
    }

    #[test]
    fn an_edited_ixml_field_changes_only_its_own_text() {
        let xml = "<BWFXML><!-- kept --><NOTE>old</NOTE><TRACK_LIST><TRACK><NAME>L</NAME></TRACK>\
                   <TRACK><NAME>R</NAME></TRACK></TRACK_LIST><SCENE/></BWFXML>";
        let w = parsed(&[(b"iXML", xml.as_bytes().to_vec())]);
        let saved = Edits::from_wav(&w);
        let labels: Vec<&str> = saved.ixml.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            [
                "NOTE",
                "TRACK_LIST / TRACK 1 / NAME",
                "TRACK_LIST / TRACK 2 / NAME",
                "SCENE"
            ]
        );
        let mut edits = saved.clone();
        edits.ixml[0].1 = "Frogs & rain".into();
        edits.ixml[2].1 = "Right".into();
        edits.ixml[3].1 = "12".into();
        let text = edits.changes(&saved).unwrap().ixml.unwrap();
        assert_eq!(
            text,
            "<BWFXML><!-- kept --><NOTE>Frogs &amp; rain</NOTE><TRACK_LIST><TRACK><NAME>L</NAME></TRACK>\
             <TRACK><NAME>Right</NAME></TRACK></TRACK_LIST><SCENE>12</SCENE></BWFXML>"
        );
    }

    #[test]
    fn without_a_cue_chunk_the_original_app_s_sync_points_show() {
        let xml = "<BWFXML><SYNC_POINT_LIST><SYNC_POINT_COUNT>1</SYNC_POINT_COUNT><SYNC_POINT>\
                   <SYNC_POINT_TYPE>RELATIVE</SYNC_POINT_TYPE><SYNC_POINT_COMMENT>Owl</SYNC_POINT_COMMENT>\
                   <SYNC_POINT_LOW>96000</SYNC_POINT_LOW><SYNC_POINT_HIGH>0</SYNC_POINT_HIGH>\
                   <SYNC_POINT_EVENT_DURATION>4800</SYNC_POINT_EVENT_DURATION></SYNC_POINT>\
                   </SYNC_POINT_LIST></BWFXML>";
        let saved = Edits::from_wav(&parsed(&[(b"iXML", xml.as_bytes().to_vec())]));
        assert_eq!(
            saved.markers,
            [Marker {
                id: 1,
                frame: 96_000,
                length: 4_800,
                label: "Owl".into(),
                note: String::new(),
            }]
        );
        let empty_cue = parsed(&[
            (b"iXML", xml.as_bytes().to_vec()),
            (b"cue ", 0u32.to_le_bytes().to_vec()),
        ]);
        assert!(Edits::from_wav(&empty_cue).markers.is_empty());
    }

    #[test]
    fn info_lists_the_known_fields_then_the_file_s_own() {
        let entries = [
            (*b"ZTST", "vendor".to_owned()),
            (*b"ICMT", "river".to_owned()),
            (*b"ICMT", "rain".to_owned()),
        ];
        let saved = Edits::from_wav(&parsed(&[(b"LIST", wav::info_body(&entries, &[]))]));
        assert_eq!(saved.info.len(), meta::INFO_FIELDS.len() + 2);
        assert_eq!(saved.info[2], entries[1]);
        assert_eq!(
            saved.info[meta::INFO_FIELDS.len()..],
            [entries[0].clone(), entries[2].clone()]
        );
        let mut edits = saved.clone();
        edits.info[0].1 = " Loobu ".into();
        let info = edits.changes(&saved).unwrap().info.unwrap();
        assert_eq!(
            info,
            [
                (*b"INAM", "Loobu".to_owned()),
                entries[1].clone(),
                entries[0].clone(),
                entries[2].clone()
            ]
        );
    }
}
