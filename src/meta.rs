//! What a recording says about itself, reduced to what the header shows.

use crate::wav::{Bext, Wav};

#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub description: Option<String>,
    pub recorder: Option<String>,
    pub start: Option<WallStart>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WallStart {
    pub date: Option<String>,
    /// Seconds since local midnight at the first sample.
    pub seconds: f64,
}

pub fn from_wav(w: &Wav) -> Meta {
    let bext = w.bext.as_ref();
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
    let description = w
        .ixml
        .as_deref()
        .and_then(|x| xml_text(x, "NOTE"))
        .or_else(|| bext.and_then(|b| human_text(&b.description)))
        .or_else(|| info(b"ICMT").and_then(human_text))
        .or_else(|| info(b"ISBJ").and_then(human_text))
        .or_else(|| info(b"INAM").and_then(human_text));

    Meta {
        description,
        recorder: bext.map(|b| b.originator.clone()).filter(|s| !s.is_empty()),
        start: bext.and_then(|b| wall_start(b, w.sample_rate)),
    }
}

/// For tagged formats (FLAC, MP3, ...), candidates in priority order.
pub fn from_tag_text<'a>(candidates: impl IntoIterator<Item = &'a str>) -> Meta {
    Meta {
        description: candidates.into_iter().find_map(human_text),
        ..Meta::default()
    }
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

/// Text of the first `<tag>…</tag>`, unescaped.
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    human_text(&unescape(&xml[start..end]))
}

/// XML character data as text: CDATA sections kept verbatim, and entity and
/// character references resolved. Anything malformed is kept as written.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find(['&', '<']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
            let end = cdata.find("]]>").unwrap_or(cdata.len());
            out.push_str(&cdata[..end]);
            rest = cdata.get(end + 3..).unwrap_or_default();
            continue;
        }
        let reference = rest[1..]
            .split_once(';')
            .and_then(|(name, after)| Some((reference(name)?, after)));
        match reference {
            Some((c, after)) => {
                out.push(c);
                rest = after;
            }
            None => {
                out.push_str(&rest[..1]);
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
    fn ixml_note_is_extracted_and_unescaped() {
        let x = "<BWFXML><PROJECT>p</PROJECT><NOTE>Frogs &amp; rain</NOTE></BWFXML>";
        assert_eq!(xml_text(x, "NOTE").as_deref(), Some("Frogs & rain"));
        assert_eq!(xml_text(x, "SCENE"), None);
    }

    #[test]
    fn character_references_and_cdata_become_text() {
        assert_eq!(
            unescape("J&#228;rv, caf&#xE9; &lt;shore&gt;"),
            "Järv, café <shore>"
        );
        assert_eq!(unescape("<![CDATA[a & <b>]]> c"), "a & <b> c");
        assert_eq!(unescape("AT&T &bogus; &#xZZ;"), "AT&T &bogus; &#xZZ;");
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
    fn ixml_note_outranks_a_machine_only_bext() {
        let mut bext = vec![0u8; 602];
        bext[..17].copy_from_slice(b"sSPEED=048.000-ND");
        bext[256..278].copy_from_slice(b"TASCAM Portacapture X8");
        bext[320..330].copy_from_slice(b"2026:09:08");
        bext[338..346].copy_from_slice(&(61_454u64 * 48_000).to_le_bytes());
        let ixml = b"<BWFXML><NOTE>Heron colony at dusk</NOTE></BWFXML>".to_vec();
        let file = wav::tests::build(false, &[(b"bext", bext), (b"iXML", ixml)], &[0; 4]);
        let m = from_wav(&wav::parse(&mut std::io::Cursor::new(&file)).unwrap());
        assert_eq!(m.description.as_deref(), Some("Heron colony at dusk"));
        assert_eq!(m.recorder.as_deref(), Some("TASCAM Portacapture X8"));
        assert_eq!(
            m.start,
            Some(WallStart {
                date: Some("2026-09-08".into()),
                seconds: 61_454.0
            })
        );
    }
}
