//! Saving edited metadata and markers into a WAV file, and renaming it.
//!
//! A save writes a whole new file next to the original, reads it back, and
//! compares its audio with the original's byte for byte; only then does the
//! new file take the original's place, in one step. However a save ends,
//! the original is either untouched or fully replaced.

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::wav::{self, Chunk, Marker, Wav};

/// What to write in place of each kind of chunk the editor handles. `None`
/// keeps the file's own, byte for byte.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    pub bext: Option<Vec<u8>>,
    pub ixml: Option<String>,
    /// An empty list drops the chunk.
    pub info: Option<Vec<([u8; 4], String)>>,
    /// Written as a `cue ` chunk, even when empty, so that removing every
    /// marker sticks.
    pub markers: Option<Vec<Marker>>,
}

const BLOCK: usize = 1 << 20;

pub fn save(path: &Path, changes: &Changes, progress: &AtomicU32) -> Result<(), String> {
    let temp = temp_path(path);
    // Only ever a save of ours that was cut off.
    let _ = fs::remove_file(&temp);
    let written = (|| {
        let mut original = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
        let w = wav::parse(&mut original).map_err(|e| e.to_string())?;
        write(&mut original, &w, changes, &temp, progress)?;
        verify(&mut original, &w, changes, &temp, progress)?;
        keep_attributes(path, &temp)
    })();
    let placed = written.and_then(|()| {
        fs::rename(&temp, path).map_err(|e| format!("cannot put the new file in place: {e}"))
    });
    if placed.is_err() {
        let _ = fs::remove_file(&temp);
    }
    progress.store(1000, Ordering::Relaxed);
    placed
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.soundcheck-save"))
}

/// The file being written, and how far into it.
struct Out {
    file: BufWriter<File>,
    at: u64,
}

impl Out {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        self.at += bytes.len() as u64;
        Ok(())
    }

    fn chunk(&mut self, id: &[u8; 4], body: &[u8]) -> io::Result<()> {
        self.put(id)?;
        self.put(&(body.len() as u32).to_le_bytes())?;
        self.put(body)?;
        if body.len() % 2 == 1 {
            self.put(&[0])?;
        }
        Ok(())
    }

    /// `body` of `from` under `id`, streamed. `wide` writes the size as
    /// RF64's placeholder, the real one going in `ds64`. Copying the audio
    /// is the first half of a save's progress, reading it back the second.
    fn copy(
        &mut self,
        from: &mut File,
        id: &[u8; 4],
        body: &Range<u64>,
        wide: bool,
        progress: Option<&AtomicU32>,
    ) -> io::Result<()> {
        let len = body.end - body.start;
        let size = if wide {
            u32::MAX
        } else {
            u32::try_from(len).unwrap_or(u32::MAX)
        };
        self.put(id)?;
        self.put(&size.to_le_bytes())?;
        from.seek(SeekFrom::Start(body.start))?;
        let mut block = vec![0; BLOCK];
        let mut left = len;
        while left > 0 {
            let n = left.min(BLOCK as u64) as usize;
            from.read_exact(&mut block[..n])?;
            self.put(&block[..n])?;
            left -= n as u64;
            if let Some(progress) = progress {
                let done = (len - left) as f64 / len as f64;
                progress.store((done * 500.0) as u32, Ordering::Relaxed);
            }
        }
        if len % 2 == 1 {
            self.put(&[0])?;
        }
        Ok(())
    }
}

fn list_type(from: &mut File, chunk: &Chunk) -> io::Result<[u8; 4]> {
    let mut kind = [0; 4];
    if chunk.body.end - chunk.body.start >= 4 {
        from.seek(SeekFrom::Start(chunk.body.start))?;
        from.read_exact(&mut kind)?;
    }
    Ok(kind)
}

fn body_of(from: &mut File, chunk: &Chunk) -> io::Result<Vec<u8>> {
    from.seek(SeekFrom::Start(chunk.body.start))?;
    let mut body = Vec::new();
    from.take(chunk.body.end - chunk.body.start)
        .read_to_end(&mut body)?;
    Ok(body)
}

/// Which of the edited chunks have been written.
#[derive(Default)]
struct Written {
    bext: bool,
    ixml: bool,
    info: bool,
    markers: bool,
}

fn write(
    original: &mut File,
    w: &Wav,
    changes: &Changes,
    temp: &Path,
    progress: &AtomicU32,
) -> Result<(), String> {
    let failed = |e: io::Error| format!("cannot write the new file: {e}");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .map_err(failed)?;
    let mut out = Out {
        file: BufWriter::with_capacity(BLOCK, file),
        at: 0,
    };
    let wide = w.container != "WAV";
    let magic: &[u8; 4] = match w.container {
        "RF64" => b"RF64",
        "BW64" => b"BW64",
        _ => b"RIFF",
    };
    // The file's own INFO entries and marks, so that whatever an edit left
    // alone goes back as it was stored.
    let mut stored_info = Vec::new();
    let mut stored_marks = wav::StoredMarks::default();
    for chunk in &w.chunks {
        let kind = match &chunk.id {
            b"cue " => *b"cue ",
            b"LIST" => list_type(original, chunk).map_err(failed)?,
            _ => continue,
        };
        if !matches!(&kind, b"cue " | b"INFO" | b"adtl") {
            continue;
        }
        let body = body_of(original, chunk).map_err(failed)?;
        match &kind {
            b"cue " => stored_marks.add_cue(&body),
            b"INFO" => stored_info
                .extend(wav::subchunks(&body[4..]).map(|(id, entry)| (id, entry.to_vec()))),
            _ => stored_marks.add_adtl(&body[4..]),
        }
    }
    let mut done = Written::default();
    let put = |out: &mut Out, done: &mut Written, what: &str| -> io::Result<()> {
        match what {
            "bext" if !done.bext => {
                done.bext = true;
                if let Some(bext) = &changes.bext {
                    out.chunk(b"bext", bext)?;
                }
            }
            "iXML" if !done.ixml => {
                done.ixml = true;
                if let Some(ixml) = &changes.ixml {
                    out.chunk(b"iXML", ixml.as_bytes())?;
                }
            }
            "INFO" if !done.info => {
                done.info = true;
                if let Some(info) = changes.info.as_ref().filter(|i| !i.is_empty()) {
                    out.chunk(b"LIST", &wav::info_body(info, &stored_info))?;
                }
            }
            "cue " if !done.markers => {
                done.markers = true;
                if let Some(markers) = &changes.markers {
                    let (cue, adtl) = wav::mark_bodies(markers, &stored_marks);
                    out.chunk(b"cue ", &cue)?;
                    if let Some(adtl) = adtl {
                        out.chunk(b"LIST", &adtl)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    };

    let has_bext = w.chunks.iter().any(|c| &c.id == b"bext");
    (|| -> io::Result<()> {
        out.put(magic)?;
        out.put(&u32::MAX.to_le_bytes())?;
        out.put(b"WAVE")?;
        if wide {
            out.chunk(b"ds64", &[0; 28])?;
        }
        for chunk in &w.chunks {
            // Every INFO list, cue chunk and adtl list is read into the
            // one edited, so the first takes the edit and the rest go; of
            // bext and iXML only the first was read, and a second stays.
            let replaced = match &chunk.id {
                b"ds64" => Some(None),
                b"bext" if changes.bext.is_some() && !done.bext => Some(Some("bext")),
                b"iXML" if changes.ixml.is_some() && !done.ixml => Some(Some("iXML")),
                b"cue " if changes.markers.is_some() => Some(Some("cue ")),
                b"LIST" => match &list_type(original, chunk)? {
                    b"INFO" if changes.info.is_some() => Some(Some("INFO")),
                    // Written along with the cue points.
                    b"adtl" if changes.markers.is_some() => Some(None),
                    _ => None,
                },
                _ => None,
            };
            match replaced {
                Some(Some(what)) => put(&mut out, &mut done, what)?,
                Some(None) => {}
                None if &chunk.id == b"data" => {
                    // A Broadcast WAV header belongs before the audio.
                    if !has_bext {
                        put(&mut out, &mut done, "bext")?;
                    }
                    out.copy(original, b"data", &chunk.body, wide, Some(progress))?;
                }
                None => out.copy(original, &chunk.id, &chunk.body, false, None)?,
            }
        }
        // Whatever the file had no chunk of goes after the audio.
        for what in ["INFO", "cue ", "iXML"] {
            put(&mut out, &mut done, what)?;
        }
        out.file.flush()
    })()
    .map_err(failed)?;

    let total = out.at;
    let mut file = out.file.into_inner().map_err(|e| failed(e.into_error()))?;
    let data = w.data.end - w.data.start;
    (|| -> io::Result<()> {
        if wide {
            file.seek(SeekFrom::Start(20))?;
            file.write_all(&(total - 8).to_le_bytes())?;
            file.write_all(&data.to_le_bytes())?;
            file.write_all(&(data / w.frame_bytes() as u64).to_le_bytes())?;
        } else {
            let size = u32::try_from(total - 8)
                .map_err(|_| io::Error::other("too big for a RIFF file"))?;
            file.seek(SeekFrom::Start(4))?;
            file.write_all(&size.to_le_bytes())?;
        }
        file.sync_all()
    })()
    .map_err(failed)
}

/// Reads the new file back: its audio must be the original's to the byte,
/// and each edited chunk must read as what was meant to go in.
fn verify(
    original: &mut File,
    w: &Wav,
    changes: &Changes,
    temp: &Path,
    progress: &AtomicU32,
) -> Result<(), String> {
    let wrong =
        |what: &str| format!("the new file came out wrong ({what}), so nothing was changed");
    let mut copy = File::open(temp).map_err(|e| wrong(&e.to_string()))?;
    let c = wav::parse(&mut copy).map_err(|e| wrong(&e.to_string()))?;
    if (c.container, c.sample_rate, c.channels, c.kind)
        != (w.container, w.sample_rate, w.channels, w.kind)
    {
        return Err(wrong("format"));
    }
    let same = same_bytes(original, &w.data, &mut copy, &c.data, progress);
    if !same.map_err(|e| wrong(&e.to_string()))? {
        return Err(wrong("audio"));
    }
    let raw = |b: &Option<wav::Bext>| b.as_ref().map(|b| b.raw.clone());
    let bext = changes.bext.clone().or_else(|| raw(&w.bext));
    let markers = changes.markers.clone().or_else(|| w.cues.clone());
    let info = changes.info.clone().unwrap_or_else(|| w.info.clone());
    let ixml = changes.ixml.as_deref().or(w.ixml.as_deref()).map(str::trim);
    if raw(&c.bext) != bext {
        return Err(wrong("Broadcast WAV"));
    }
    if c.cues != markers {
        return Err(wrong("markers"));
    }
    if c.info != info {
        return Err(wrong("RIFF INFO"));
    }
    if c.ixml.as_deref() != ixml {
        return Err(wrong("iXML"));
    }
    Ok(())
}

fn same_bytes(
    a: &mut File,
    a_range: &Range<u64>,
    b: &mut File,
    b_range: &Range<u64>,
    progress: &AtomicU32,
) -> io::Result<bool> {
    let len = a_range.end - a_range.start;
    if b_range.end - b_range.start != len {
        return Ok(false);
    }
    a.seek(SeekFrom::Start(a_range.start))?;
    b.seek(SeekFrom::Start(b_range.start))?;
    let (mut x, mut y) = (vec![0; BLOCK], vec![0; BLOCK]);
    let mut left = len;
    while left > 0 {
        let n = left.min(BLOCK as u64) as usize;
        a.read_exact(&mut x[..n])?;
        b.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
        left -= n as u64;
        let done = (len - left) as f64 / len as f64;
        progress.store(500 + (done * 500.0) as u32, Ordering::Relaxed);
    }
    Ok(true)
}

/// The original's permissions, dates, and on macOS its Finder tags and
/// other extended attributes, onto the copy that replaces it.
fn keep_attributes(original: &Path, copy: &Path) -> Result<(), String> {
    let failed = |e: io::Error| format!("cannot carry over the file's dates and attributes: {e}");
    let meta = fs::metadata(original).map_err(failed)?;
    #[cfg(target_os = "macos")]
    copy_extended_attributes(original, copy).map_err(failed)?;
    fs::set_permissions(copy, meta.permissions()).map_err(failed)?;
    let mut times = FileTimes::new().set_modified(meta.modified().map_err(failed)?);
    if let Ok(accessed) = meta.accessed() {
        times = times.set_accessed(accessed);
    }
    #[cfg(target_os = "macos")]
    if let Ok(created) = meta.created() {
        use std::os::macos::fs::FileTimesExt;
        times = times.set_created(created);
    }
    #[cfg(windows)]
    if let Ok(created) = meta.created() {
        use std::os::windows::fs::FileTimesExt;
        times = times.set_created(created);
    }
    OpenOptions::new()
        .write(true)
        .open(copy)
        .and_then(|f| f.set_times(times))
        .map_err(failed)
}

#[cfg(target_os = "macos")]
fn copy_extended_attributes(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn copyfile(
            from: *const c_char,
            to: *const c_char,
            state: *mut c_void,
            flags: u32,
        ) -> c_int;
    }
    const COPYFILE_ACL: u32 = 1 << 0;
    const COPYFILE_XATTR: u32 = 1 << 2;
    let path = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other);
    let (from, to) = (path(from)?, path(to)?);
    // SAFETY: both paths are NUL-terminated and outlive the call; copyfile
    // accepts a null state.
    let status = unsafe {
        copyfile(
            from.as_ptr(),
            to.as_ptr(),
            std::ptr::null_mut(),
            COPYFILE_ACL | COPYFILE_XATTR,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Renames `path` to `name` in the same folder, never over another file.
pub fn rename(path: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    let reserved: &[char] = if cfg!(windows) {
        &['/', '\\', ':', '<', '>', '"', '|', '?', '*']
    } else {
        &['/', '\\', ':']
    };
    if name.is_empty()
        || name.starts_with('.')
        || name.contains(reserved)
        || name.chars().any(char::is_control)
    {
        return Err(format!(
            "\"{name}\" cannot be a file name: it must not start with a dot or hold any of {}",
            reserved.iter().collect::<String>()
        ));
    }
    let target = path.with_file_name(name);
    if target == path {
        return Ok(target);
    }
    // A change of case only, on a disk that ignores case, finds the file
    // itself under the new name.
    if target.exists() && !same_file(path, &target) {
        return Err(format!("{name} already exists in this folder"));
    }
    fs::rename(path, &target).map_err(|e| format!("cannot rename: {e}"))?;
    Ok(target)
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::temp_file;
    use crate::wav::tests::{build, marker};
    use std::io::Cursor;
    use std::time::{Duration, SystemTime};

    fn samples() -> Vec<i16> {
        (0..999).map(|i| (i * 37 % 2000) as i16 - 1000).collect()
    }

    fn ids(file: &[u8]) -> Vec<[u8; 4]> {
        let w = wav::parse(&mut Cursor::new(file)).unwrap();
        w.chunks.iter().map(|c| c.id).collect()
    }

    fn run(file: &[u8], changes: &Changes, name: &str) -> Vec<u8> {
        let path = temp_file(name, file);
        save(&path, changes, &AtomicU32::new(0)).unwrap();
        let saved = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!temp_path(&path).exists());
        saved
    }

    #[test]
    fn saving_nothing_writes_the_same_file() {
        let file = build(
            false,
            &[(b"zoom", vec![1, 2, 3]), (b"JUNK", vec![0; 28])],
            &samples(),
        );
        assert_eq!(run(&file, &Changes::default(), "same.wav"), file);
    }

    #[test]
    fn changes_land_and_everything_else_stays_as_it_was() {
        let mut bext = vec![0u8; 602];
        bext[..3].copy_from_slice(b"old");
        let (cue, adtl) =
            wav::mark_bodies(&[marker(1, 100, 0, "start")], &wav::StoredMarks::default());
        let file = build(
            false,
            &[
                (b"bext", bext.clone()),
                (b"zoom", vec![9; 5]),
                (b"cue ", cue),
                (b"LIST", adtl.unwrap()),
            ],
            &samples(),
        );
        bext[..3].copy_from_slice(b"new");
        let changes = Changes {
            bext: Some(bext.clone()),
            info: Some(vec![(*b"INAM", "Loobu".into())]),
            markers: Some(vec![marker(1, 100, 0, "start"), marker(2, 400, 50, "owl")]),
            ixml: Some("<BWFXML><NOTE>n</NOTE></BWFXML>".into()),
        };
        let saved = run(&file, &changes, "changes.wav");
        let w = wav::parse(&mut Cursor::new(&saved)).unwrap();
        let o = wav::parse(&mut Cursor::new(&file)).unwrap();
        assert_eq!(w.bext.as_ref().unwrap().raw, bext);
        assert_eq!(w.info, [(*b"INAM", "Loobu".to_owned())]);
        assert_eq!(w.cues.as_deref(), changes.markers.as_deref());
        assert_eq!(w.ixml.as_deref(), Some("<BWFXML><NOTE>n</NOTE></BWFXML>"));
        let audio = |f: &[u8], w: &Wav| f[w.data.start as usize..w.data.end as usize].to_vec();
        assert_eq!(audio(&saved, &w), audio(&file, &o));
        assert_eq!(
            ids(&saved),
            [
                *b"fmt ", *b"bext", *b"zoom", *b"cue ", *b"LIST", *b"data", *b"LIST", *b"iXML"
            ]
        );
        let zoom = |f: &[u8]| {
            let w = wav::parse(&mut Cursor::new(f)).unwrap();
            let z = w
                .chunks
                .iter()
                .find(|c| &c.id == b"zoom")
                .unwrap()
                .body
                .clone();
            f[z.start as usize..z.end as usize].to_vec()
        };
        assert_eq!(zoom(&saved), zoom(&file));
    }

    #[test]
    fn a_new_header_goes_before_the_audio_and_new_marks_after_it() {
        let file = build(false, &[], &samples());
        let changes = Changes {
            bext: Some(vec![0; 602]),
            markers: Some(vec![marker(1, 5, 0, "")]),
            ..Changes::default()
        };
        let saved = run(&file, &changes, "new.wav");
        assert_eq!(ids(&saved), [*b"fmt ", *b"bext", *b"data", *b"cue "]);
        let none = Changes {
            markers: Some(Vec::new()),
            ..Changes::default()
        };
        let cleared = run(&saved, &none, "cleared.wav");
        let w = wav::parse(&mut Cursor::new(&cleared)).unwrap();
        assert_eq!(w.cues, Some(Vec::new()));
    }

    #[test]
    fn a_second_header_stays_and_every_info_list_goes_into_one() {
        let info = |id: &[u8; 4], value: &str| wav::info_body(&[(*id, value.into())], &[]);
        let header = |text: &[u8]| {
            let mut bext = vec![0u8; 602];
            bext[..text.len()].copy_from_slice(text);
            bext
        };
        let file = build(
            false,
            &[
                (b"bext", header(b"first")),
                (b"LIST", info(b"INAM", "a")),
                (b"bext", header(b"second")),
                (b"LIST", info(b"ICMT", "b")),
            ],
            &samples(),
        );
        let changes = Changes {
            bext: Some(header(b"edited")),
            info: Some(vec![(*b"INAM", "a".into()), (*b"ICMT", "c".into())]),
            ..Changes::default()
        };
        let saved = run(&file, &changes, "twice.wav");
        assert_eq!(
            ids(&saved),
            [*b"fmt ", *b"bext", *b"LIST", *b"bext", *b"data"]
        );
        let w = wav::parse(&mut Cursor::new(&saved)).unwrap();
        assert_eq!(w.bext.unwrap().raw, header(b"edited"));
        assert_eq!(
            w.info,
            [(*b"INAM", "a".to_owned()), (*b"ICMT", "c".to_owned())]
        );
        let second = &w.chunks[3].body;
        assert_eq!(
            saved[second.start as usize..second.end as usize],
            header(b"second")
        );
    }

    #[test]
    fn rf64_stays_rf64_with_its_sizes_in_ds64() {
        let file = build(true, &[], &samples());
        let changes = Changes {
            info: Some(vec![(*b"ICMT", "big".into())]),
            ..Changes::default()
        };
        let saved = run(&file, &changes, "wide.wav");
        let w = wav::parse(&mut Cursor::new(&saved)).unwrap();
        assert_eq!((w.container, w.frames()), ("RF64", 999));
        let size = u64::from_le_bytes(saved[20..28].try_into().unwrap());
        assert_eq!(size, saved.len() as u64 - 8);
    }

    #[test]
    fn dates_are_kept() {
        let path = temp_file("dates.wav", &build(false, &[], &samples()));
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(then)
            .unwrap();
        let changes = Changes {
            info: Some(vec![(*b"ICMT", "x".into())]),
            ..Changes::default()
        };
        save(&path, &changes, &AtomicU32::new(0)).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(modified, then);
    }

    #[cfg(unix)]
    #[test]
    fn a_save_that_cannot_finish_leaves_the_original_as_it_was() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("soundcheck-locked-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("locked.wav");
        let file = build(false, &[], &samples());
        fs::write(&path, &file).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let changes = Changes {
            info: Some(vec![(*b"ICMT", "x".into())]),
            ..Changes::default()
        };
        let result = save(&path, &changes, &AtomicU32::new(0));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let after = fs::read(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(result.is_err());
        assert_eq!(after, file);
    }

    #[test]
    fn rename_never_replaces_another_file() {
        let a = temp_file("rename-a.wav", b"a");
        let b = temp_file("rename-b.wav", b"b");
        let b_name = b.file_name().unwrap().to_string_lossy().into_owned();
        assert!(rename(&a, &b_name).unwrap_err().contains("already exists"));
        assert_eq!(
            (fs::read(&a).unwrap(), fs::read(&b).unwrap()),
            (b"a".to_vec(), b"b".to_vec())
        );
        for bad in ["", "  ", "a/b.wav", ".hidden.wav"] {
            assert!(rename(&a, bad).is_err(), "{bad:?}");
        }
        let a_name = a.file_name().unwrap().to_string_lossy().into_owned();
        let moved = rename(&a, &a_name.replace("rename-a", "rename-c")).unwrap();
        assert!(!a.exists() && moved.exists());
        let upper = rename(&moved, &a_name.replace("rename-a", "RENAME-C")).unwrap();
        assert_eq!(fs::read(&upper).unwrap(), b"a");
        fs::remove_file(&upper).unwrap();
        fs::remove_file(&b).unwrap();
    }
}
