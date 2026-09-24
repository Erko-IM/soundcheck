//! What a recording says about itself: the few facts the header shows, and
//! everything else for the Metadata view.

use crate::wav::{Bext, Wav};

#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub description: Option<String>,
    pub recorder: Option<String>,
    pub start: Option<WallStart>,
    /// Channel names from the recorder's track list, where it wrote any.
    pub channel_names: Vec<Option<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WallStart {
    pub date: Option<String>,
    /// Seconds since local midnight at the first sample.
    pub seconds: f64,
}

/// Everything known about a file, in titled groups of label and value.
#[derive(Debug, Clone, Default)]
pub struct Details {
    pub sections: Vec<(String, Vec<(String, String)>)>,
}

impl Details {
    pub fn add(&mut self, title: &str, rows: Vec<(String, String)>) {
        // Recorders end lines the Windows way, and a lone CR would show as a
        // box.
        let rows: Vec<_> = rows
            .into_iter()
            .map(|(label, value)| (label, value.replace("\r\n", "\n").replace('\r', "\n")))
            .filter(|(_, v)| !v.is_empty())
            .collect();
        if !rows.is_empty() {
            self.sections.push((title.to_owned(), rows));
        }
    }
}

pub fn from_wav(w: &Wav) -> Meta {
    let bext = w.bext.as_ref();
    let ixml = w.ixml.as_deref().and_then(parse_xml);
    let info = |id: &[u8; 4]| {
        w.info
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v.as_str())
    };

    // The first source holding something a person typed wins. iXML NOTE is
    // where field recorders put notes entered on the device; bext
    // Description is often only machine key=value lines; INFO is the
    // generic RIFF fallback.
    let description = ixml
        .as_ref()
        .and_then(|x| x.child("NOTE"))
        .and_then(|n| human_text(&n.text))
        .or_else(|| bext.and_then(|b| human_text(&b.description)))
        .or_else(|| info(b"ICMT").and_then(human_text))
        .or_else(|| info(b"ISBJ").and_then(human_text))
        .or_else(|| info(b"INAM").and_then(human_text));

    Meta {
        description,
        recorder: bext.map(|b| b.originator.clone()).filter(|s| !s.is_empty()),
        start: bext.and_then(|b| wall_start(b, w.sample_rate)),
        channel_names: ixml
            .as_ref()
            .map_or_else(Vec::new, |x| track_names(x, usize::from(w.channels))),
    }
}

/// For tagged formats (FLAC, MP3, ...), candidates in priority order.
pub fn from_tag_text<'a>(candidates: impl IntoIterator<Item = &'a str>) -> Meta {
    Meta {
        description: candidates.into_iter().find_map(human_text),
        ..Meta::default()
    }
}

/// The recorder's own records: `bext`, iXML and `LIST/INFO`.
pub fn wav_details(w: &Wav) -> Details {
    let mut details = Details::default();
    if let Some(b) = &w.bext {
        let time = (b.time_reference > 0).then(|| {
            let seconds = b.time_reference as f64 / f64::from(w.sample_rate);
            format!("{} samples ({})", b.time_reference, time_of_day(seconds))
        });
        details.add(
            "Broadcast WAV",
            vec![
                row("Description", &b.description),
                row("Originator", &b.originator),
                row("Reference", &b.originator_reference),
                row("Date", &b.origination_date),
                row("Time", &b.origination_time),
                row("Time reference", time.as_deref().unwrap_or_default()),
                row("Coding history", &b.coding_history),
            ],
        );
    }
    if let Some(root) = w.ixml.as_deref().and_then(parse_xml) {
        let mut rows = Vec::new();
        for child in &root.children {
            if child.name == "TRACK_LIST" {
                for track in child.children.iter().filter(|t| t.name == "TRACK") {
                    let field = |name| track.child(name).map_or("", |c| c.text.as_str());
                    let label = format!("Track {}", field("CHANNEL_INDEX"));
                    let value = [field("NAME"), field("FUNCTION")]
                        .into_iter()
                        .filter(|v| !v.is_empty())
                        .collect::<Vec<_>>()
                        .join(", ");
                    rows.push((label, value));
                }
            } else {
                flatten(child, "", &mut rows);
            }
        }
        details.add("iXML", rows);
    }
    details.add(
        "RIFF INFO",
        w.info
            .iter()
            .map(|(id, value)| (info_name(id), value.clone()))
            .collect(),
    );
    details
}

fn row(label: &str, value: &str) -> (String, String) {
    (label.to_owned(), value.trim().to_owned())
}

/// Leaves as `PARENT / CHILD` labels, so nested groups such as SPEED or
/// LOCATION keep their context.
fn flatten(element: &Element, prefix: &str, rows: &mut Vec<(String, String)>) {
    let label = if prefix.is_empty() {
        element.name.clone()
    } else {
        format!("{prefix} / {}", element.name)
    };
    if element.children.is_empty() {
        rows.push((label, element.text.trim().to_owned()));
    } else {
        for child in &element.children {
            flatten(child, &label, rows);
        }
    }
}

/// The RIFF INFO fields editors know, in the order the Metadata view lists
/// them.
pub const INFO_FIELDS: [(&[u8; 4], &str); 13] = [
    (b"INAM", "Title"),
    (b"IART", "Artist"),
    (b"ICMT", "Comment"),
    (b"ISBJ", "Subject"),
    (b"IKEY", "Keywords"),
    (b"ICRD", "Date"),
    (b"IGNR", "Genre"),
    (b"ICOP", "Copyright"),
    (b"IENG", "Engineer"),
    (b"ITCH", "Technician"),
    (b"ISRC", "Source"),
    (b"IPRD", "Product"),
    (b"ISFT", "Software"),
];

pub fn info_name(id: &[u8; 4]) -> String {
    INFO_FIELDS
        .iter()
        .find(|(known, _)| *known == id)
        .map_or_else(
            || String::from_utf8_lossy(id).into_owned(),
            |(_, name)| (*name).to_owned(),
        )
}

/// Channel names from iXML TRACK_LIST, by 1-based CHANNEL_INDEX.
fn track_names(root: &Element, channels: usize) -> Vec<Option<String>> {
    let mut names = vec![None; channels];
    let tracks = root
        .child("TRACK_LIST")
        .map_or(&[][..], |list| &list.children);
    for track in tracks.iter().filter(|t| t.name == "TRACK") {
        let index = track
            .child("CHANNEL_INDEX")
            .and_then(|c| c.text.trim().parse::<usize>().ok());
        let name = track
            .child("NAME")
            .map(|n| n.text.trim().to_owned())
            .filter(|n| !n.is_empty());
        if let (Some(index), Some(name)) = (index, name)
            && let Some(slot) = index.checked_sub(1).and_then(|i| names.get_mut(i))
        {
            *slot = Some(name);
        }
    }
    names
}

/// Drops the `sKEY=value` lines recorders write into description fields for
/// other machines; whatever is left is what a person wrote.
fn human_text(s: &str) -> Option<String> {
    let kept: Vec<&str> = s
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_machine_line(line))
        .collect();
    (!kept.is_empty()).then(|| kept.join(" "))
}

/// `sSPEED=048.000-ND`, `zTRK1=Mid`: a lowercase vendor letter, then an
/// uppercase key. Ordinary prose such as `Wind=strong` does not match.
fn is_machine_line(line: &str) -> bool {
    let Some((key, _)) = line.split_once('=') else {
        return false;
    };
    let mut chars = key.chars();
    (2..=16).contains(&key.len())
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// An XML element, with its own text and its child elements. Attributes,
/// comments and processing instructions are dropped: iXML keeps everything
/// in element text.
#[derive(Debug, Default, PartialEq)]
pub struct Element {
    pub name: String,
    pub text: String,
    pub children: Vec<Element>,
    /// Where the element's content sits in the source, between its start
    /// and end tags; for `<NAME/>`, the whole tag. `None` for an element
    /// that was never closed.
    pub span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Span {
    Content(std::ops::Range<usize>),
    Empty(std::ops::Range<usize>),
}

impl Element {
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }
}

/// The root element of `xml`, or `None` if there is none. Recorders write
/// iXML by hand-rolled code, so this is lenient: an unclosed element closes
/// with its parent, and a stray end tag is ignored.
pub fn parse_xml(xml: &str) -> Option<Element> {
    // The element being read, and where its content began.
    let mut stack: Vec<(Element, usize)> = vec![(Element::default(), 0)];
    let mut rest = xml;
    let at = |rest: &str| xml.len() - rest.len();
    while let Some(open) = rest.find('<') {
        let text = &rest[..open];
        if let Some((top, _)) = stack.last_mut() {
            top.text.push_str(&unescape(text));
        }
        rest = &rest[open..];
        if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
            let end = cdata.find("]]>").unwrap_or(cdata.len());
            if let Some((top, _)) = stack.last_mut() {
                top.text.push_str(&cdata[..end]);
            }
            rest = cdata.get(end + 3..).unwrap_or_default();
            continue;
        }
        let (skip_to, closer) = if rest.starts_with("<!--") {
            ("-->", true)
        } else {
            (">", false)
        };
        let Some(close) = rest.find(skip_to) else {
            break;
        };
        let tag_start = at(rest);
        let tag = &rest[1..close];
        rest = &rest[close + skip_to.len()..];
        if closer || tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            let name = name.trim();
            if let Some(depth) = stack.iter().rposition(|(e, _)| e.name == name)
                && depth > 0
            {
                // Only the element this tag closes gets a span: any left
                // open inside it were never closed.
                while stack.len() > depth {
                    let closed = stack.len() == depth + 1;
                    let (mut done, from) = stack.pop().unwrap_or_default();
                    if closed {
                        done.span = Some(Span::Content(from..tag_start));
                    }
                    if let Some((parent, _)) = stack.last_mut() {
                        parent.children.push(done);
                    }
                }
            }
            continue;
        }
        let empty = tag.ends_with('/');
        let name = tag
            .trim_end_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        let mut element = Element {
            name,
            ..Element::default()
        };
        if empty {
            element.span = Some(Span::Empty(tag_start..at(rest)));
            if let Some((top, _)) = stack.last_mut() {
                top.children.push(element);
            }
        } else {
            stack.push((element, at(rest)));
        }
    }
    while stack.len() > 1 {
        let (done, _) = stack.pop().unwrap_or_default();
        if let Some((parent, _)) = stack.last_mut() {
            parent.children.push(done);
        }
    }
    stack.pop()?.0.children.into_iter().next()
}

/// `text` as XML character data.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// XML character data as text, entity and character references resolved.
/// Anything malformed is kept as written.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let reference = rest[1..]
            .split_once(';')
            .and_then(|(name, after)| Some((reference(name)?, after)));
        match reference {
            Some((c, after)) => {
                out.push(c);
                rest = after;
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn reference(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let code = name.strip_prefix('#')?;
            let value = match code.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16),
                None => code.parse(),
            };
            char::from_u32(value.ok()?)
        }
    }
}

fn wall_start(b: &Bext, sample_rate: u32) -> Option<WallStart> {
    let seconds = if b.time_reference > 0 {
        b.time_reference as f64 / f64::from(sample_rate)
    } else {
        clock_seconds(&b.origination_time)?
    };
    let date = b.origination_date.trim().replace([':', '/', '.'], "-");
    Some(WallStart {
        date: (date.len() == 10).then_some(date),
        seconds,
    })
}

/// `hh:mm:ss`, tolerating the `-` and `.` separators some recorders use.
fn clock_seconds(s: &str) -> Option<f64> {
    let mut parts = s
        .split([':', '-', '.'])
        .map(|p| p.trim().parse::<u32>().ok());
    let (h, m, sec) = (parts.next()??, parts.next()??, parts.next()??);
    (h < 24 && m < 60 && sec < 60).then(|| f64::from(h * 3600 + m * 60 + sec))
}

fn time_of_day(seconds: f64) -> String {
    let s = seconds.rem_euclid(86_400.0);
    let whole = s as u64;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        whole / 3600,
        whole / 60 % 60,
        whole % 60,
        ((s - whole as f64) * 1000.0) as u64
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav;

    #[test]
    fn machine_lines_are_recognised_and_prose_is_not() {
        assert!(is_machine_line("sSPEED=048.000-ND"));
        assert!(is_machine_line("zTRK1=Mid"));
        assert!(!is_machine_line("Wind=strong from the north"));
        assert!(!is_machine_line("a=b"));
        assert!(!is_machine_line("Nightingale near the pond"));
    }

    #[test]
    fn machine_lines_are_stripped_from_mixed_text() {
        let raw = "sSPEED=048.000-ND\r\nDawn chorus, east meadow\r\nsTAKE=03\r\n";
        assert_eq!(human_text(raw).as_deref(), Some("Dawn chorus, east meadow"));
        assert_eq!(human_text("sSPEED=048.000-ND\r\nsTAKE=03"), None);
    }

    #[test]
    fn xml_reads_nested_elements_and_text() {
        let x = r#"<?xml version="1.0"?><!-- written by a recorder -->
            <BWFXML><IXML_VERSION>1.61</IXML_VERSION><NOTE>Frogs &amp; rain,
            caf&#xE9; <![CDATA[a < b]]></NOTE><SPEED><FILE_SAMPLE_RATE>48000</FILE_SAMPLE_RATE></SPEED>
            <EMPTY/></BWFXML>"#;
        let root = parse_xml(x).unwrap();
        assert_eq!(root.name, "BWFXML");
        assert_eq!(
            root.child("NOTE").unwrap().text,
            "Frogs & rain,\n            café a < b"
        );
        assert_eq!(
            root.child("SPEED")
                .and_then(|s| s.child("FILE_SAMPLE_RATE"))
                .map(|r| r.text.as_str()),
            Some("48000")
        );
        assert!(root.child("EMPTY").is_some());
    }

    #[test]
    fn xml_spans_point_at_each_element_s_own_content() {
        let x = "<BWFXML><NOTE>a &amp; b</NOTE><EMPTY/><SPEED><RATE>48</RATE></SPEED></BWFXML>";
        let root = parse_xml(x).unwrap();
        let content = |e: &Element| match &e.span {
            Some(Span::Content(r)) => &x[r.clone()],
            Some(Span::Empty(r)) => &x[r.clone()],
            None => "",
        };
        assert_eq!(content(root.child("NOTE").unwrap()), "a &amp; b");
        assert_eq!(content(root.child("EMPTY").unwrap()), "<EMPTY/>");
        let rate = root.child("SPEED").and_then(|s| s.child("RATE")).unwrap();
        assert_eq!(content(rate), "48");
        let unclosed = parse_xml("<A><B>1</A>").unwrap();
        assert_eq!(unclosed.child("B").unwrap().span, None);
    }

    #[test]
    fn xml_survives_what_hand_rolled_writers_produce() {
        let root = parse_xml("<BWFXML><NOTE>unclosed<TAKE>3</TAKE></NOTEX></BWFXML>").unwrap();
        assert_eq!(root.name, "BWFXML");
        let note = root.child("NOTE").unwrap();
        assert_eq!(note.child("TAKE").unwrap().text, "3");
        assert_eq!(parse_xml("no markup at all"), None);
    }

    #[test]
    fn track_names_follow_their_channel_index() {
        let x = "<BWFXML><TRACK_LIST><TRACK_COUNT>2</TRACK_COUNT>\
                 <TRACK><CHANNEL_INDEX>2</CHANNEL_INDEX><NAME>Side</NAME></TRACK>\
                 <TRACK><CHANNEL_INDEX>1</CHANNEL_INDEX><NAME>Mid</NAME></TRACK>\
                 </TRACK_LIST></BWFXML>";
        let root = parse_xml(x).unwrap();
        assert_eq!(
            track_names(&root, 3),
            [Some("Mid".into()), Some("Side".into()), None]
        );
    }

    #[test]
    fn origination_time_is_the_fallback_for_a_zero_time_reference() {
        let b = Bext {
            origination_time: "17-04-14".into(),
            ..Bext::default()
        };
        assert_eq!(wall_start(&b, 48_000).unwrap().seconds, 61_454.0);
        let b = Bext {
            time_reference: 48_000 * 3600,
            ..Bext::default()
        };
        assert_eq!(wall_start(&b, 48_000).unwrap().seconds, 3600.0);
        assert_eq!(wall_start(&Bext::default(), 48_000), None);
    }

    #[test]
    fn details_break_lines_where_the_recorder_did() {
        let mut details = Details::default();
        details.add(
            "Broadcast WAV",
            vec![row("Coding history", "A=PCM\r\nA=PCM,F=48000\r")],
        );
        details.add("Empty", vec![row("Nothing", "  ")]);
        assert_eq!(details.sections.len(), 1);
        assert_eq!(details.sections[0].1[0].1, "A=PCM\nA=PCM,F=48000");
    }

    #[test]
    fn ixml_note_outranks_a_machine_only_bext_and_everything_is_listed() {
        let mut bext = vec![0u8; 602];
        bext[..17].copy_from_slice(b"sSPEED=048.000-ND");
        bext[256..278].copy_from_slice(b"TASCAM Portacapture X8");
        bext[320..330].copy_from_slice(b"2026:09:08");
        bext[338..346].copy_from_slice(&(61_454u64 * 48_000).to_le_bytes());
        let ixml = b"<BWFXML><PROJECT>Herons</PROJECT><NOTE>Heron colony at dusk</NOTE>\
                     <SPEED><FILE_SAMPLE_RATE>48000</FILE_SAMPLE_RATE></SPEED></BWFXML>"
            .to_vec();
        let file = wav::tests::build(false, &[(b"bext", bext), (b"iXML", ixml)], &[0; 4]);
        let w = wav::parse(&mut std::io::Cursor::new(&file)).unwrap();
        let m = from_wav(&w);
        assert_eq!(m.description.as_deref(), Some("Heron colony at dusk"));
        assert_eq!(m.recorder.as_deref(), Some("TASCAM Portacapture X8"));
        assert_eq!(
            m.start,
            Some(WallStart {
                date: Some("2026-09-08".into()),
                seconds: 61_454.0
            })
        );
        let details = wav_details(&w);
        let titles: Vec<&str> = details.sections.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(titles, ["Broadcast WAV", "iXML"]);
        let ixml_rows = &details.sections[1].1;
        assert!(ixml_rows.contains(&("PROJECT".into(), "Herons".into())));
        assert!(ixml_rows.contains(&("SPEED / FILE_SAMPLE_RATE".into(), "48000".into())));
        let bext_rows = &details.sections[0].1;
        assert!(bext_rows.contains(&(
            "Time reference".into(),
            "2949792000 samples (17:04:14.000)".into()
        )));
    }
}
