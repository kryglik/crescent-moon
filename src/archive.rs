//! Zip archives as folders, as in Total Commander and mc: Enter on a .zip opens it in the
//! panel, read-only. F5 copies out of it (extracting), F3 and Enter show a file from it,
//! quick view previews it, and Alt+F5 / Alt+F9 pack files into a new zip and unpack one
//! (see ops.rs).
//!
//! Names are UTF-8 when the archive says so or when they read as UTF-8; otherwise they are
//! in the code page of the DOS era (on Windows, the system's OEM code page: zips made by
//! older Windows on a Czech system use code page 852).

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use zip::ZipArchive;

use crate::lfs;

#[derive(Clone, Debug)]
pub struct Item {
    /// The path inside the archive, `/`-separated, without a leading or trailing slash.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Its place in the zip; None for a folder only implied by the paths of its files.
    pub index: Option<usize>,
}

impl Item {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

pub struct Archive {
    /// The zip on disk.
    pub file: PathBuf,
    pub items: Vec<Item>,
    /// Names in the zip that aren't safe to unpack (`../x`, `C:x`): never listed or written.
    pub left_out: Vec<String>,
    /// When the zip was read: its size and time, to notice it changing.
    pub stamp: (u64, Option<SystemTime>),
}

pub fn is_archive(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

fn stamp(file: &Path) -> (u64, Option<SystemTime>) {
    fs::metadata(file).map_or((0, None), |m| (m.len(), m.modified().ok()))
}

pub fn to_io(error: zip::result::ZipError) -> io::Error {
    match error {
        zip::result::ZipError::Io(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

impl Archive {
    pub fn open(file: &Path) -> io::Result<Archive> {
        Self::open_for_reading(file).map(|(archive, _)| archive)
    }

    /// The list of what's in the zip and the open zip itself, the same one: the numbers in the
    /// list are its entries, even if the file on disk is replaced a moment later.
    pub fn open_for_reading(file: &Path) -> io::Result<(Archive, ZipArchive<File>)> {
        // A named pipe called x.zip would block here for good.
        if !lfs::metadata(file)?.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        let stamp = stamp(file);
        let zip = ZipArchive::new(lfs::open(file)?).map_err(to_io)?;
        let mut items = Vec::new();
        let mut left_out = Vec::new();
        let mut dirs = HashSet::new();
        for i in 0..zip.len() {
            // From the zip's directory alone: quick on a network drive, and a damaged file
            // further on doesn't cost the whole list.
            let entry = zip.by_index_data(i).map_err(to_io)?;
            let raw = decode_name(entry.name_raw());
            let Some(path) = clean_path(&raw) else {
                left_out.push(raw.replace('\0', "\u{fffd}"));
                continue;
            };
            let modified = entry.last_modified().and_then(to_system_time);
            let is_dir = entry.is_dir() || raw.ends_with(['/', '\\']);
            // Folders above it, which zips often leave out.
            let mut end = 0;
            while let Some(slash) = path[end..].find('/') {
                end += slash;
                let parent = &path[..end];
                if dirs.insert(parent.to_string()) {
                    items.push(Item { path: parent.to_string(), is_dir: true, size: 0, modified: None, index: None });
                }
                end += 1;
            }
            if is_dir {
                if dirs.insert(path.clone()) {
                    items.push(Item { path, is_dir: true, size: 0, modified, index: Some(i) });
                } else if let Some(item) = items.iter_mut().find(|item| item.is_dir && item.path == path) {
                    // Implied by a file listed before it; now it has a time of its own.
                    (item.modified, item.index) = (modified, Some(i));
                }
            } else {
                items.push(Item { path, is_dir: false, size: entry.size(), modified, index: Some(i) });
            }
        }
        Ok((Archive { file: file.to_path_buf(), items, left_out, stamp }, zip))
    }

    /// What to tell the user about names left out, if any.
    pub fn left_out_note(&self) -> Option<String> {
        let first = self.left_out.first()?;
        let count = self.left_out.len();
        let names = if count == 1 { "name" } else { "names" };
        Some(format!("Left out {count} {names} that would land outside the folder: {first}"))
    }

    /// Whether the zip on disk changed since it was read.
    pub fn changed(&self) -> bool {
        stamp(&self.file) != self.stamp
    }

    /// The items directly inside folder `inner` ("" for the top).
    pub fn children<'a>(&'a self, inner: &'a str) -> impl Iterator<Item = &'a Item> + 'a {
        self.items.iter().filter(move |item| parent_of(&item.path) == inner)
    }

    pub fn find(&self, inner: &str) -> Option<&Item> {
        self.items.iter().find(|item| item.path == inner)
    }

    /// `inner` and, for a folder, everything below it.
    pub fn below<'a>(&'a self, inner: &'a str) -> impl Iterator<Item = &'a Item> + 'a {
        self.items
            .iter()
            .filter(move |item| item.path == inner || (item.path.starts_with(inner) && item.path[inner.len()..].starts_with('/')))
    }

    /// Bytes of the files at or below `inner`.
    pub fn size_below(&self, inner: &str) -> u64 {
        self.below(inner).filter(|item| !item.is_dir).map(|item| item.size).sum()
    }
}

/// The folder an inner path is in ("" at the top).
pub fn parent_of(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..i])
}

/// The inner path of `name` in folder `inner`.
pub fn join(inner: &str, name: &str) -> String {
    if inner.is_empty() { name.to_string() } else { format!("{inner}/{name}") }
}

/// A name from the zip as a safe relative path: `/`-separated, without `.` or empty parts,
/// a leading drive or a leading slash. None for one that is empty or would land elsewhere
/// than in the folder it is unpacked to: every part must be one plain name.
fn clean_path(name: &str) -> Option<String> {
    let mut parts = Vec::new();
    // Padding after the name, as some tools write it. A NUL anywhere else hides what the part
    // really is (`..\0` is `..` to the system).
    for part in name.trim_end_matches('\0').split(['/', '\\']) {
        match part {
            "" | "." => {}
            part if parts.is_empty() && part.len() == 2 && part.ends_with(':') => {}
            part if is_plain_name(part) => parts.push(part),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// One name and nothing else, whatever the system: joined to a folder it stays below it. Not
/// `..`; on Windows not `C:x`, which replaces the whole path when joined, nor `a:b`, a hidden
/// stream of the file `a`.
pub fn is_plain_name(part: &str) -> bool {
    let mut components = Path::new(part).components();
    matches!((components.next(), components.next()), (Some(std::path::Component::Normal(_)), None))
        && !part.contains('\0')
        && !(cfg!(windows) && part.contains(':'))
}

fn decode_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(text) => text.to_string(),
        Err(_) => legacy_name(raw),
    }
}

#[cfg(windows)]
fn legacy_name(raw: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{CP_OEMCP, MultiByteToWideChar};
    let mut out = vec![0u16; raw.len()];
    let n = unsafe { MultiByteToWideChar(CP_OEMCP, 0, raw.as_ptr(), raw.len() as i32, out.as_mut_ptr(), out.len() as i32) };
    String::from_utf16_lossy(&out[..n.max(0) as usize])
}

/// Code page 437, which zips without the UTF-8 flag are supposed to use.
#[cfg(not(windows))]
fn legacy_name(raw: &[u8]) -> String {
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";
    let high: Vec<char> = HIGH.chars().collect();
    raw.iter().map(|&b| if b < 0x80 { b as char } else { high[(b - 0x80) as usize] }).collect()
}

/// A zip time (local time, two-second steps) as a point in time.
fn to_system_time(time: zip::DateTime) -> Option<SystemTime> {
    use chrono::{Local, NaiveDate};
    let date = NaiveDate::from_ymd_opt(time.year().into(), time.month().into(), time.day().into())?;
    let local = date.and_hms_opt(time.hour().into(), time.minute().into(), time.second().into())?;
    // A time in the hour skipped when the clocks go forward counts as the hour after.
    let exact = local.and_local_timezone(Local).earliest();
    Some(exact.or_else(|| (local + chrono::Duration::hours(1)).and_local_timezone(Local).earliest())?.into())
}

/// A point in time as a zip time (1980 to 2107; outside that, None).
pub fn to_zip_time(time: SystemTime) -> Option<zip::DateTime> {
    use chrono::{Datelike, Timelike};
    let local = crate::util::local_time(time)?;
    zip::DateTime::from_date_and_time(
        u16::try_from(local.year()).ok()?,
        local.month() as u8,
        local.day() as u8,
        local.hour() as u8,
        local.minute() as u8,
        local.second() as u8,
    )
    .ok()
}

/// Up to `limit` bytes from the start of a file in the zip (quick view).
pub fn read_start(file: &Path, index: usize, limit: u64) -> io::Result<Vec<u8>> {
    let mut zip = ZipArchive::new(File::open(file)?).map_err(to_io)?;
    let entry = zip.by_index(index).map_err(to_io)?;
    let mut data = Vec::new();
    entry.take(limit).read_to_end(&mut data)?;
    Ok(data)
}

/// Copies one file out of the zip to `dest`, with its time. (The Extract job does this for
/// rmc, with progress and questions.)
#[cfg(test)]
pub fn extract_file(file: &Path, index: usize, dest: &Path) -> io::Result<()> {
    use std::io::Write;
    let mut zip = ZipArchive::new(File::open(file)?).map_err(to_io)?;
    let mut entry = zip.by_index(index).map_err(to_io)?;
    let modified = entry.last_modified().and_then(to_system_time);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = File::create(dest)?;
    io::copy(&mut entry, &mut out)?;
    out.flush()?;
    if let Some(time) = modified {
        let _ = out.set_modified(time);
    }
    Ok(())
}

/// A folder of rmc's own, for files taken out of archives to be viewed: in the session's
/// private folder, so nobody else on the machine reads them.
pub fn temp_dir() -> std::io::Result<PathBuf> {
    crate::util::session_dir().map(|dir| dir.join("archives"))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// A zip with a folder entry, files in folders that have none, and Czech names.
    pub fn sample_zip(path: &Path) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(zip::DateTime::from_date_and_time(2024, 5, 17, 10, 30, 0).unwrap());
        zip.add_directory("docs/", options).unwrap();
        zip.start_file("docs/readme.txt", options).unwrap();
        zip.write_all(b"hello from the zip\n").unwrap();
        zip.start_file("Účty/2024/faktura.txt", options).unwrap();
        zip.write_all(&[b'x'; 1000]).unwrap();
        zip.start_file("top.txt", options).unwrap();
        zip.write_all(b"top").unwrap();
        zip.start_file("../evil.txt", options).unwrap();
        zip.write_all(b"never").unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn lists_folders_files_and_implied_folders() {
        let tmp = TempDir::new("archive");
        let path = tmp.path().join("a.zip");
        sample_zip(&path);
        let archive = Archive::open(&path).unwrap();
        let top: Vec<(&str, bool)> = archive.children("").map(|i| (i.name(), i.is_dir)).collect();
        assert_eq!(top, [("docs", true), ("Účty", true), ("top.txt", false)], "../evil.txt is left out");
        let inner: Vec<&str> = archive.children("Účty").map(Item::name).collect();
        assert_eq!(inner, ["2024"]);
        assert_eq!(archive.size_below("Účty"), 1000);
        assert_eq!(archive.below("docs").count(), 2);
        let readme = archive.find("docs/readme.txt").unwrap();
        assert_eq!(readme.size, 19);
        let time = to_zip_time(readme.modified.unwrap()).unwrap();
        assert_eq!((time.year(), time.month(), time.day(), time.hour(), time.minute()), (2024, 5, 17, 10, 30));
        assert!(!archive.changed());

        let data = read_start(&path, readme.index.unwrap(), 5).unwrap();
        assert_eq!(data, b"hello");
        let out = tmp.path().join("out").join("readme.txt");
        extract_file(&path, readme.index.unwrap(), &out).unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap(), "hello from the zip\n");
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(clean_path("a/b/../c"), None);
        assert_eq!(clean_path("..\0/..\0/x"), None, "a NUL doesn't hide ..");
        assert_eq!(clean_path("name.txt\0\0"), Some("name.txt".into()), "padding after the name");
        // Past what a zip can say; on Linux a file's time can even be past any calendar.
        let far = if cfg!(unix) { 1 << 55 } else { 1 << 38 };
        assert_eq!(to_zip_time(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(far)), None);
        assert_eq!(clean_path("/abs/./x//y"), Some("abs/x/y".into()));
        assert_eq!(clean_path(r"C:\Windows\x.dll"), Some("Windows/x.dll".into()));
        assert_eq!(clean_path("dir/"), Some("dir".into()));
        assert_eq!(clean_path("/"), None);
        assert_eq!(parent_of("a/b/c"), "a/b");
        assert_eq!(parent_of("a"), "");
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a/b", "c"), "a/b/c");
    }

    #[test]
    fn legacy_names_are_decoded() {
        // "Účty" in code page 852 would be E9 9F 74 79; check what can be checked everywhere:
        // UTF-8 stays UTF-8, and other bytes don't panic.
        assert_eq!(decode_name("Účty".as_bytes()), "Účty");
        assert!(!decode_name(&[0xE9, 0x9F, b't', b'y']).is_empty());
    }
}
