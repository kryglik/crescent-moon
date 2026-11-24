//! Small helpers: formatting, path handling, wildcard matching and fitting text into columns.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Formats a size for the 7-column size field: plain bytes while they fit, then K/M/G/T.
pub fn format_size(bytes: u64) -> String {
    if bytes < 10_000_000 {
        return bytes.to_string();
    }
    let mut value = bytes as f64 / 1024.0;
    for unit in ["K", "M", "G", "T", "P"] {
        if value < 999_999.5 {
            return format!("{}{unit}", value.round() as u64);
        }
        value /= 1024.0;
    }
    format!("{}E", value.round() as u64)
}

/// A count with thousands separated: `41,023`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A unix mode as `ls -l` writes it: the type (`d` folder, `l` link, `-` file; `c`, `b`,
/// `p`, `s` for devices, pipes and sockets), then read, write and run for the owner, the group
/// and others. Setuid and setgid show as `s` and the sticky bit as `t` in place of the run
/// bit, capital where that is off.
pub fn mode_text(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o020000 => 'c',
        0o060000 => 'b',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '-',
    };
    let mut out = String::with_capacity(10);
    out.push(kind);
    for (shift, special, mark) in [(6, 0o4000, 's'), (3, 0o2000, 's'), (0, 0o1000, 't')] {
        let bits = (mode >> shift) & 7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        out.push(match (mode & special != 0, bits & 1 != 0) {
            (true, true) => mark,
            (true, false) => mark.to_ascii_uppercase(),
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    out
}

/// Short human-readable size such as `3.4M` or `67G`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

/// mc-style modification time: `Nov  3 21:51` for the last six months, `Nov  3  2024` otherwise.
pub fn format_time(time: SystemTime, now: SystemTime) -> String {
    let Some(local) = local_time(time) else { return "?".into() };
    let recent = match now.duration_since(time) {
        Ok(age) => age < Duration::from_secs(182 * 24 * 3600),
        Err(ahead) => ahead.duration() < Duration::from_secs(3600),
    };
    if recent {
        local.format("%b %e %H:%M").to_string()
    } else {
        local.format("%b %e  %Y").to_string()
    }
}

/// A point in time in the local time zone; None where chrono can't show it (a file dated
/// in the year 3,000,000, which tmpfs and btrfs allow; chrono's own conversion panics).
pub fn local_time(time: SystemTime) -> Option<DateTime<Local>> {
    let (secs, nanos) = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => (i64::try_from(after.as_secs()).ok()?, after.subsec_nanos()),
        Err(before) => {
            let before = before.duration();
            let secs = -i64::try_from(before.as_secs()).ok()?;
            match before.subsec_nanos() {
                0 => (secs, 0),
                nanos => (secs - 1, 1_000_000_000 - nanos),
            }
        }
    };
    Some(DateTime::from_timestamp(secs, nanos)?.with_timezone(&Local))
}

/// Resolves `.` and `..` lexically. Unlike `fs::canonicalize` it never adds `\\?\` prefixes.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some() {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Turns user input (relative, `~`, `e:`, quoted) into a normalized absolute path.
pub fn resolve(base: &Path, input: &str) -> PathBuf {
    let input = strip_quotes(input.trim());
    let expanded = match input.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => match home_dir() {
            Some(home) => format!("{}{rest}", home.display()),
            None => input.to_string(),
        },
        _ => input.to_string(),
    };
    normalize(&base.join(with_drive_root(&expanded)))
}

/// A path the user typed for something new, as Windows makes it: the last name without its
/// trailing dots and spaces (`report.` becomes `report`, as in Explorer). A name that exists
/// as typed is kept (it may be a `foo.` made elsewhere), and elsewhere nothing changes.
pub fn as_windows_makes_it(path: PathBuf) -> PathBuf {
    if !cfg!(windows) || crate::lfs::symlink_metadata(&path).is_ok() {
        return path;
    }
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { return path };
    let trimmed = name.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed == name { path } else { path.with_file_name(trimmed) }
}

pub fn strip_quotes(s: &str) -> &str {
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}

/// `e:` and `e:foo` mean the root of drive E here (cmd would use E's own current directory).
fn with_drive_root(s: &str) -> String {
    let bytes = s.as_bytes();
    if cfg!(windows) && bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let letter = (bytes[0] as char).to_ascii_uppercase();
        let rest = &s[2..];
        if rest.starts_with(['/', '\\']) {
            return format!("{letter}:{rest}");
        }
        return format!("{letter}:\\{rest}");
    }
    s.to_string()
}

/// A text file, whatever wrote it: UTF-8 with or without a BOM, UTF-16 with a BOM (Windows
/// PowerShell's `>`), or else the system's ANSI code page (its `Set-Content`) on Windows.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    Ok(decode_text(&std::fs::read(path)?))
}

/// A file's bytes as text, decoded as `decode_text` does; None when they aren't text: a zero
/// byte outside UTF-16 (which has a byte order mark) is a program, a picture, an archive.
pub fn text_of(bytes: &[u8]) -> Option<String> {
    let utf16 = bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]);
    (utf16 || !bytes.contains(&0)).then(|| decode_text(bytes))
}

pub fn decode_text(bytes: &[u8]) -> String {
    let utf16 = |rest: &[u8], unit: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| unit([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, u16::from_be_bytes);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => ansi_text(bytes),
    }
}

/// Text in the system's ANSI code page (Windows-1250 on a Czech Windows).
#[cfg(windows)]
pub fn ansi_text(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
    if bytes.is_empty() {
        return String::new();
    }
    let mut out = vec![0u16; bytes.len()];
    let n = unsafe { MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), bytes.len() as i32, out.as_mut_ptr(), out.len() as i32) };
    String::from_utf16_lossy(&out[..n.max(0) as usize])
}

#[cfg(not(windows))]
pub fn ansi_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A number nobody can predict, for names of private files (std seeds its hash keys from the
/// system's randomness).
pub fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    hasher.finish()
}

/// A new folder in the temp folder, made fresh with a name nobody can guess and on Linux
/// readable by its owner only: /tmp is shared with other users, who could otherwise read
/// what cm puts there or plant a link under a name cm is about to use.
pub fn private_dir(tag: &str) -> std::io::Result<PathBuf> {
    for _ in 0..100 {
        let dir = std::env::temp_dir().join(format!("cm-{}-{tag}-{:016x}", std::process::id(), random()));
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no private folder could be made in the temp folder"))
}

/// Writes `text` to `file` aside first and renames it over the old one: a crash or a full
/// disk never leaves half a file. `flush` puts it on the disk first, which can take a second
/// on a busy one: for what the user just changed, not what is saved in passing. The name
/// aside is unique, so two cm windows saving at once don't trip over each other.
pub fn write_atomically(file: &Path, text: &str, flush: bool) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // The file a link leads to (settings kept in a dotfiles repository): the link stays.
    let real = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let name = real.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let temp = real.with_file_name(format!(".{name}.{}-{:08x}.tmp", std::process::id(), random() as u32));
    let written = (|| {
        let mut out = std::fs::File::create_new(&temp)?;
        out.write_all(text.as_bytes())?;
        if flush {
            out.sync_all()?;
        }
        drop(out);
        std::fs::rename(&temp, &real)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

static SESSION_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// This cm's own private folder (see `private_dir`), made when first needed: files taken
/// out of archives, the last folder of a one-off shell. `remove_session_dir` clears it.
pub fn session_dir() -> std::io::Result<PathBuf> {
    let mut dir = SESSION_DIR.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = dir.as_ref() {
        return Ok(dir.clone());
    }
    let made = private_dir("session")?;
    *dir = Some(made.clone());
    Ok(made)
}

/// Removes the session folder, if one was made (a file still open in an app stays).
pub fn remove_session_dir() {
    if let Some(dir) = SESSION_DIR.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Clears, in the background, what a cm that ended without tidying up (a closed terminal
/// tab, a lost ssh connection) left in the temp folder: private folders of processes that are
/// gone. Files taken out of archives could otherwise pile up for good.
pub fn sweep_temp() {
    std::thread::spawn(|| {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(pid) = name.to_str().and_then(private_dir_owner)
                && !crate::platform::process_alive(pid)
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    });
}

/// The process a folder name of `private_dir` belongs to: `cm-<pid>-<tag>-<16 hex digits>`,
/// or `rmc-…` from the time cm was called rmc.
pub fn private_dir_owner(name: &str) -> Option<u32> {
    let mut parts = name.strip_prefix("cm-").or_else(|| name.strip_prefix("rmc-"))?.split('-');
    let (pid, tag, random) = (parts.next()?, parts.next()?, parts.next()?);
    let valid = parts.next().is_none()
        && matches!(tag, "session" | "shell")
        && random.len() == 16
        && random.chars().all(|c| c.is_ascii_hexdigit());
    if valid { pid.parse().ok() } else { None }
}

/// Where an internal error on a background thread is written: the per-user temp folder on
/// Windows, `~/.cache/crescent-moon` elsewhere (not /tmp, which other users share).
pub fn panic_log() -> PathBuf {
    if cfg!(windows) {
        return std::env::temp_dir().join("cm-panic.txt");
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".cache")));
    cache.map_or_else(|| std::env::temp_dir().join("cm-panic.txt"), |dir| dir.join(FOLDER).join("panic.txt"))
}

pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The name of cm's folders: its settings, and on Linux its cache.
pub const FOLDER: &str = "crescent-moon";

/// What the settings folder was called when cm was rmc.
const OLD_FOLDER: &str = "rmc";

/// Where the settings folder is: `%APPDATA%` on Windows, `$XDG_CONFIG_HOME` or `~/.config`
/// elsewhere.
fn config_base() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").filter(|v| !v.is_empty()).map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".config")))
    }
}

/// `%APPDATA%\crescent-moon` on Windows, `$XDG_CONFIG_HOME/crescent-moon` or
/// `~/.config/crescent-moon` elsewhere.
pub fn config_dir() -> Option<PathBuf> {
    config_base().map(|dir| dir.join(FOLDER))
}

/// The first time cm starts, the settings folder rmc had (settings, history, hotlist, places,
/// schemes, rescued files) is copied into cm's, so nothing is lost by the new name. The old
/// folder stays as it was for an rmc still in use. The copy is made beside and renamed into
/// place when whole, so one cut short is made again next time rather than half kept.
pub fn carry_over_settings() {
    if let Some(base) = config_base() {
        let _ = carry_over(&base.join(OLD_FOLDER), &base.join(FOLDER));
    }
}

fn carry_over(old: &Path, new: &Path) -> std::io::Result<()> {
    if new.exists() || !old.is_dir() {
        return Ok(());
    }
    let mut part = new.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    if part.exists() {
        std::fs::remove_dir_all(&part)?;
    }
    copy_tree(old, &part)?;
    std::fs::rename(&part, new)
}

/// Folders and files only: anything else in a settings folder (a link) is left behind.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub fn find_in_path(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Comparison key for a path: normalized components, lower-cased on Windows.
fn path_key(path: &Path) -> Vec<String> {
    normalize(path)
        .components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy().into_owned();
            if cfg!(windows) { s.to_lowercase() } else { s }
        })
        .collect()
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

/// A file's identity, the same whichever path reaches it: the volume and file id on
/// Windows, the device and inode elsewhere. Links are followed.
#[cfg(windows)]
pub fn file_id(path: &Path) -> Option<(u64, u128)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(crate::lfs::literal(path))
        .ok()?;
    let handle = file.as_raw_handle() as _;
    // The whole 128-bit id: ReFS (Dev Drives) has ids of that size, and the 64 bits of the
    // older call can be the same for two different files there.
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<FILE_ID_INFO>() as u32;
    if unsafe { GetFileInformationByHandleEx(handle, FileIdInfo, (&raw mut info).cast(), size) } != 0 {
        return known_id(info.VolumeSerialNumber, u128::from_le_bytes(info.FileId.Identifier));
    }
    // FAT and other file systems without it: the 64-bit index.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return None;
    }
    let index = (info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64;
    known_id(info.dwVolumeSerialNumber as u64, if index == u64::MAX { 0 } else { u128::from(index) })
}

/// Some file systems (network and virtual ones) give every file the id 0 or all ones: that
/// says nothing, and taken at its word any two files would be one.
fn known_id(volume: u64, id: u128) -> Option<(u64, u128)> {
    (id != 0 && id != u128::MAX).then_some((volume, id))
}

#[cfg(not(windows))]
pub fn file_id(path: &Path) -> Option<(u64, u128)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().and_then(|m| known_id(m.dev(), u128::from(m.ino())))
}

/// Whether two existing paths lead to the same file or folder: another letter case on a
/// drive that ignores case (WSL's /mnt/c, a USB stick on Linux), a link, a mapped drive and
/// its share.
pub fn same_file(a: &Path, b: &Path) -> bool {
    matches!((file_id(a), file_id(b)), (Some(x), Some(y)) if x == y)
}

/// Whether two existing paths are names of the one entry itself, links not followed: another
/// letter case where case doesn't count, a hard link. A link and the file it leads to are
/// two entries (`same_file` says they are one file).
pub fn same_entry(a: &Path, b: &Path) -> bool {
    match (entry_id(a), entry_id(b)) {
        (Some(x), Some(y)) => x == y,
        _ => one_spelling(a, b),
    }
}

/// For a disk that keeps no file ids: whether `a` and `b` are two spellings of one name in a
/// folder, both there, of which the folder lists at most one.
pub fn one_spelling(a: &Path, b: &Path) -> bool {
    let (Some(one), Some(other)) = (a.file_name(), b.file_name()) else { return false };
    let (Some(folder), Some(other_folder)) = (a.parent(), b.parent()) else { return false };
    if one == other || one.to_string_lossy().to_lowercase() != other.to_string_lossy().to_lowercase() {
        return false;
    }
    let there = |path| crate::lfs::symlink_metadata(path).is_ok();
    same_path(folder, other_folder)
        && there(a)
        && there(b)
        && crate::lfs::read_dir(folder).is_ok_and(|entries| entries.flatten().filter(|e| e.file_name() == one || e.file_name() == other).count() < 2)
}

#[cfg(windows)]
fn entry_id(path: &Path) -> Option<(u64, u128)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(crate::lfs::literal(path))
        .ok()?;
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<FILE_ID_INFO>() as u32;
    let ok = unsafe { GetFileInformationByHandleEx(file.as_raw_handle() as _, FileIdInfo, (&raw mut info).cast(), size) };
    // Without the id (FAT has no links): the file's own identity.
    if ok == 0 { file_id(path) } else { known_id(info.VolumeSerialNumber, u128::from_le_bytes(info.FileId.Identifier)) }
}

#[cfg(not(windows))]
fn entry_id(path: &Path) -> Option<(u64, u128)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().and_then(|m| known_id(m.dev(), u128::from(m.ino())))
}

/// Like `is_inside`, but with links, junctions and mapped drives resolved, so a folder
/// reached again through one counts. `path` need not exist yet.
pub fn really_inside(path: &Path, dir: &Path) -> bool {
    let Ok(dir) = crate::lfs::canonicalize(dir) else { return false };
    let mut existing = path;
    let mut rest = Vec::new();
    while !crate::lfs::exists(existing) {
        let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else { return false };
        rest.push(name);
        existing = parent;
    }
    let Ok(mut real) = crate::lfs::canonicalize(existing) else { return false };
    real.extend(rest.iter().rev());
    is_inside(&real, &dir)
}

/// True when `path` is `dir` itself or somewhere below it.
pub fn is_inside(path: &Path, dir: &Path) -> bool {
    let (path, dir) = (path_key(path), path_key(dir));
    path.len() >= dir.len() && path[..dir.len()] == dir[..]
}

/// The name of the entry directly inside `dir` that leads to `path`.
pub fn child_name(dir: &Path, path: &Path) -> Option<String> {
    if !is_inside(path, dir) || same_path(path, dir) {
        return None;
    }
    let depth = normalize(dir).components().count();
    normalize(path)
        .components()
        .nth(depth)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

/// The drive part of a path (`e:` or `\\server\share`, lower-cased); elsewhere the mount point
/// holding it (`/mnt/e`). Panels remember their last folder per key.
pub fn volume_key(path: &Path) -> Option<String> {
    match normalize(path).components().next() {
        Some(Component::Prefix(p)) => Some(p.as_os_str().to_string_lossy().to_lowercase()),
        _ => crate::platform::mount_point(path).map(|dir| dir.to_string_lossy().into_owned()),
    }
}

/// Whether two paths are on the same drive, so a move can be a plain rename.
#[cfg(windows)]
pub fn same_volume(a: &Path, b: &Path) -> bool {
    volume_key(a) == volume_key(b)
}

/// Whether two paths are on the same file system, so a move can be a plain rename.
#[cfg(not(windows))]
pub fn same_volume(a: &Path, b: &Path) -> bool {
    crate::platform::same_device(a, b)
}

/// `e:` alone (a whole drive rather than a place on it) as that drive's root.
pub fn bare_drive(input: &str) -> Option<PathBuf> {
    let bytes = input.trim().as_bytes();
    (cfg!(windows) && bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        .then(|| PathBuf::from(format!("{}:\\", (bytes[0] as char).to_ascii_uppercase())))
}

pub fn with_trailing_separator(path: &Path) -> String {
    let mut s = path.display().to_string();
    if !s.ends_with(['/', '\\']) {
        s.push(std::path::MAIN_SEPARATOR);
    }
    s
}

/// Case-insensitive wildcard match supporting `*` and `?`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ni < n.len() {
        // The wildcard first: a `*` in the name (allowed on Linux) is not the pattern's `*`.
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// One word of a command line as the shell would read it: quotes taken off (PowerShell's
/// `''` inside single quotes is one quote; bash's `'\''` joins up), escapes (`\` in a POSIX
/// shell, `` ` `` in PowerShell), and environment variables filled in: `$NAME`, `${NAME}`,
/// `$env:NAME`, `%NAME%`. None when only the shell could read it: a variable that isn't in
/// the environment (one set in the shell), a command substitution, an unclosed quote.
pub fn shell_word(word: &str, posix: bool) -> Option<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let (c, next) = (chars[i], chars.get(i + 1).copied());
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some('\''), '\'') if !posix && next == Some('\'') => {
                out.push('\'');
                i += 1;
            }
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => out.push(c),
            // Command substitution: only the shell knows.
            (_, '$') if next == Some('(') => return None,
            (_, '`') if posix => return None,
            (_, '`') if !posix => {
                out.push(next?);
                i += 1;
            }
            (None, '\\') if posix => {
                out.push(next?);
                i += 1;
            }
            (Some('"'), '\\') if posix && matches!(next, Some('$' | '`' | '"' | '\\')) => {
                out.push(next?);
                i += 1;
            }
            (_, '$') => match variable_name(&chars[i + 1..]) {
                Some((name, len)) => {
                    out.push_str(&environment_value(&name)?);
                    i += len;
                }
                None => out.push('$'),
            },
            (_, '%') if !posix => match chars[i + 1..].iter().position(|&c| c == '%') {
                Some(len) if len > 0 => {
                    let name: String = chars[i + 1..i + 1 + len].iter().collect();
                    out.push_str(&environment_value(&name)?);
                    i += len + 1;
                }
                _ => out.push('%'),
            },
            (_, c) => out.push(c),
        }
        i += 1;
    }
    quote.is_none().then_some(out)
}

/// The variable after a `$`: `NAME`, `{NAME}` or `env:NAME`, and how many characters it took.
fn variable_name(rest: &[char]) -> Option<(String, usize)> {
    if rest.first() == Some(&'{') {
        let end = rest.iter().position(|&c| c == '}')?;
        return Some((rest[1..end].iter().collect(), end + 1));
    }
    let prefix: String = rest.iter().take(4).collect();
    let skip = if prefix.eq_ignore_ascii_case("env:") { 4 } else { 0 };
    let len = rest[skip..].iter().take_while(|c| c.is_alphanumeric() || **c == '_').count();
    (len > 0).then(|| (rest[..skip + len].iter().collect(), skip + len))
}

/// An environment variable's value (`env:NAME` as PowerShell writes it); HOME is the home
/// folder on Windows too, as PowerShell's `$HOME` is.
fn environment_value(name: &str) -> Option<String> {
    let name = match name.get(..4) {
        Some(prefix) if prefix.eq_ignore_ascii_case("env:") => &name[4..],
        _ => name,
    };
    if let Ok(value) = std::env::var(name) {
        return Some(value);
    }
    (name == "HOME").then(home_dir).flatten().map(|home| home.to_string_lossy().into_owned())
}

/// Compares names the way Explorer does: runs of digits by their value, so "day 2" comes
/// before "day 10"; everything else character by character.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let digits_a: String = std::iter::from_fn(|| a.next_if(char::is_ascii_digit)).collect();
                let digits_b: String = std::iter::from_fn(|| b.next_if(char::is_ascii_digit)).collect();
                let (value_a, value_b) = (digits_a.trim_start_matches('0'), digits_b.trim_start_matches('0'));
                let order = value_a
                    .len()
                    .cmp(&value_b.len())
                    .then_with(|| value_a.cmp(value_b))
                    .then_with(|| digits_a.len().cmp(&digits_b.len()));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                a.next();
                b.next();
            }
        }
    }
}

/// Splits a command such as `"C:\Program Files\x.exe" -R` into words, honouring double quotes.
pub fn split_command(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_word = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_word = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_word {
                    parts.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            c => {
                current.push(c);
                has_word = true;
            }
        }
    }
    if has_word {
        parts.push(current);
    }
    parts
}

pub fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Longest prefix of `s` that fits in `max` columns.
pub fn take_width(s: &str, max: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            return &s[..i];
        }
        used += w;
    }
    s
}

/// Longest suffix of `s` that fits in `max` columns.
pub fn take_width_end(s: &str, max: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            return &s[i + c.len_utf8()..];
        }
        used += w;
    }
    s
}

/// `s` in lines of at most `w` columns, broken between words where it can be (a word wider
/// than a line is cut), and at its own line breaks.
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut lines = Vec::new();
    for paragraph in s.lines() {
        let mut line = String::new();
        for word in paragraph.split(' ').filter(|word| !word.is_empty()) {
            let mut word = word;
            if !line.is_empty() && width(&line) + 1 + width(word) <= w {
                line.push(' ');
                line.push_str(word);
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            while width(word) > w {
                let head = take_width(word, w);
                let head = if head.is_empty() { &word[..word.chars().next().map_or(0, char::len_utf8)] } else { head };
                lines.push(head.to_string());
                word = &word[head.len()..];
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

pub fn pad(s: &str, w: usize) -> String {
    let mut out = s.to_string();
    out.extend(std::iter::repeat_n(' ', w.saturating_sub(width(s))));
    out
}

pub fn pad_left(s: &str, w: usize) -> String {
    let mut out: String = std::iter::repeat_n(' ', w.saturating_sub(width(s))).collect();
    out.push_str(s);
    out
}

/// Fits `s` into exactly `w` columns. Long text is cut in the middle with `~`, like mc,
/// so the end of a file name (its extension) stays visible.
pub fn fit(s: &str, w: usize) -> String {
    if width(s) <= w {
        return pad(s, w);
    }
    if w < 4 {
        return pad(take_width(s, w), w);
    }
    let extension_w = s.rfind('.').map_or(0, |i| width(&s[i..]));
    let tail_w = ((w - 1) / 3).max(extension_w).min((w - 1) / 2);
    let head = take_width(s, w - 1 - tail_w);
    let tail = take_width_end(s, tail_w);
    pad(&format!("{head}~{tail}"), w)
}

/// Keeps the end of `s` (the deepest part of a path) when it is too long.
pub fn fit_left(s: &str, w: usize) -> String {
    if width(s) <= w {
        s.to_string()
    } else if w == 0 {
        String::new()
    } else {
        format!("…{}", take_width_end(s, w - 1))
    }
}

pub fn center(s: &str, w: usize) -> String {
    let sw = width(s);
    if sw >= w {
        return fit(s, w);
    }
    pad(&pad_left(s, sw + (w - sw) / 2), w)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rmc's settings folder comes over whole the first time, and then never again: the new one
    /// is cm's from then on, and the old one is left as it was.
    #[test]
    fn the_settings_of_rmc_are_carried_over_once() {
        let tmp = crate::testutil::TempDir::new("carry-over");
        let (old, new) = (tmp.path().join("rmc"), tmp.path().join(FOLDER));
        std::fs::create_dir_all(old.join("schemes")).unwrap();
        std::fs::write(old.join("settings.ini"), "mouse = false\n").unwrap();
        std::fs::write(old.join("schemes").join("nord.ini"), "[nord]\n").unwrap();
        // What a copy cut short left behind is made again.
        std::fs::create_dir_all(tmp.path().join(format!("{FOLDER}.part"))).unwrap();
        std::fs::write(tmp.path().join(format!("{FOLDER}.part")).join("half"), "").unwrap();

        carry_over(&old, &new).unwrap();
        assert_eq!(std::fs::read_to_string(new.join("settings.ini")).unwrap(), "mouse = false\n");
        assert_eq!(std::fs::read_to_string(new.join("schemes").join("nord.ini")).unwrap(), "[nord]\n");
        assert!(!new.join("half").exists() && !tmp.path().join(format!("{FOLDER}.part")).exists());
        assert!(old.join("settings.ini").is_file(), "the old folder stays");

        std::fs::write(new.join("settings.ini"), "mouse = true\n").unwrap();
        std::fs::write(old.join("settings.ini"), "changed by an old rmc\n").unwrap();
        carry_over(&old, &new).unwrap();
        assert_eq!(std::fs::read_to_string(new.join("settings.ini")).unwrap(), "mouse = true\n", "only once");

        let (none, fresh) = (tmp.path().join("nothing"), tmp.path().join("fresh"));
        carry_over(&none, &fresh).unwrap();
        assert!(!fresh.exists(), "no rmc before: nothing is made");
    }

    #[test]
    fn modes_as_ls_writes_them() {
        assert_eq!(mode_text(0o100644), "-rw-r--r--");
        assert_eq!(mode_text(0o040755), "drwxr-xr-x");
        assert_eq!(mode_text(0o120777), "lrwxrwxrwx");
        assert_eq!(mode_text(0o010600), "prw-------");
        assert_eq!(mode_text(0o104755), "-rwsr-xr-x", "setuid on a program");
        assert_eq!(mode_text(0o102644), "-rw-r-Sr--", "setgid without the run bit");
        assert_eq!(mode_text(0o041777), "drwxrwxrwt", "/tmp");
        assert_eq!(mode_text(0o041770), "drwxrwx--T");
        assert_eq!(mode_text(0o644), "-rw-r--r--", "a server that leaves the type out");
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0");
        assert_eq!(format_size(9_999_999), "9999999");
        assert_eq!(format_size(10_000_000), "9766K");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5120M");
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(3_565_158), "3.4M");
        assert_eq!(human_size(67 * 1024 * 1024 * 1024), "67G");
    }

    #[test]
    fn normalizing() {
        assert_eq!(normalize(Path::new("a/./b/../c")), PathBuf::from("a").join("c"));
        assert_eq!(normalize(Path::new("../x")), PathBuf::from("..").join("x"));
    }

    #[cfg(windows)]
    #[test]
    fn resolving_windows_paths() {
        let base = Path::new(r"E:\devel\project");
        assert_eq!(resolve(base, "e:"), PathBuf::from(r"E:\"));
        assert_eq!(resolve(base, "c:windows"), PathBuf::from(r"C:\windows"));
        assert_eq!(resolve(base, ".."), PathBuf::from(r"E:\devel"));
        assert_eq!(resolve(base, r"..\other\.\x"), PathBuf::from(r"E:\devel\other\x"));
        assert_eq!(resolve(base, r"\tmp"), PathBuf::from(r"E:\tmp"));
        assert_eq!(resolve(base, "\"sub dir\""), PathBuf::from(r"E:\devel\project\sub dir"));
        assert_eq!(resolve(Path::new(r"C:\"), ".."), PathBuf::from(r"C:\"));
        assert!(same_path(Path::new(r"c:\Users"), Path::new(r"C:\users\")));
        assert!(is_inside(Path::new(r"C:\a\b\c"), Path::new(r"c:\A")));
        assert!(!is_inside(Path::new(r"C:\ab"), Path::new(r"C:\a")));
        assert_eq!(child_name(Path::new(r"C:\a"), Path::new(r"C:\a\b\c")).as_deref(), Some("b"));
        assert!(same_volume(Path::new(r"c:\x"), Path::new(r"C:\y")));
        assert!(!same_volume(Path::new(r"C:\x"), Path::new(r"E:\y")));
    }

    #[test]
    fn globbing() {
        assert!(glob_match("*", "anything.txt"));
        assert!(glob_match("*.TXT", "notes.txt"));
        assert!(glob_match("a?c*", "abcdef"));
        assert!(!glob_match("*.rs", "main.rsx"));
        assert!(glob_match("*ůň*", "Příliš žluťoučký kůň.txt"));
        // A star in the name itself (Linux allows it) is just a character.
        assert!(glob_match("*.txt", "*draft.txt"));
        assert!(glob_match("a*", "a*b"));
    }

    /// chrono panics on times beyond a quarter of a million years; tmpfs and btrfs allow them.
    #[test]
    fn far_off_times_show_a_question_mark() {
        if let Some(far) = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(100_000_000_000_000)) {
            assert_eq!(format_time(far, SystemTime::now()), "?");
        }
        let before_1970 = SystemTime::UNIX_EPOCH - Duration::from_millis(1500);
        assert_eq!(local_time(before_1970).map(|t| t.timestamp_millis()), Some(-1500));
    }

    #[test]
    fn numbers_in_names_sort_by_value() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("day 2", "day 10"), Less);
        assert_eq!(natural_cmp("img_0010.jpg", "img_0009.jpg"), Greater);
        assert_eq!(natural_cmp("a10b2", "a10b10"), Less);
        assert_eq!(natural_cmp("abc", "abd"), Less);
        assert_eq!(natural_cmp("7", "007"), Less, "the same value: fewer zeros first");
        assert_eq!(natural_cmp("x", "x1"), Less);
        assert_eq!(natural_cmp("99999999999999999999999", "100000000000000000000000"), Less, "no overflow");
    }

    #[test]
    fn words_as_the_shell_reads_them() {
        // PowerShell: '' is a quote inside single quotes, ` escapes.
        assert_eq!(shell_word("'Účty a faktury'", false).as_deref(), Some("Účty a faktury"));
        assert_eq!(shell_word("'it''s'", false).as_deref(), Some("it's"));
        assert_eq!(shell_word("\"C:\\Program Files\"", false).as_deref(), Some(r"C:\Program Files"));
        // POSIX: '\'' joins up, \ escapes outside quotes.
        assert_eq!(shell_word(r"'it'\''s'", true).as_deref(), Some("it's"));
        assert_eq!(shell_word(r"My\ Folder", true).as_deref(), Some("My Folder"));
        // Variables from the environment, in each shell's way of writing them.
        // SAFETY: no other test reads or writes this variable.
        unsafe { std::env::set_var("CM_WORD_TEST", "/data") };
        assert_eq!(shell_word("$CM_WORD_TEST/x", true).as_deref(), Some("/data/x"));
        assert_eq!(shell_word("\"${CM_WORD_TEST}\"/x", true).as_deref(), Some("/data/x"));
        assert_eq!(shell_word("$env:CM_WORD_TEST\\x", false).as_deref(), Some(r"/data\x"));
        assert_eq!(shell_word("%CM_WORD_TEST%\\x", false).as_deref(), Some(r"/data\x"));
        assert_eq!(shell_word("'$CM_WORD_TEST'", true).as_deref(), Some("$CM_WORD_TEST"), "single quotes keep it");
        // Only the shell knows these.
        assert_eq!(shell_word("$cm_shell_only_variable", false), None);
        assert_eq!(shell_word("$(pwd)", true), None);
        assert_eq!(shell_word("'unclosed", true), None);
        assert_eq!(shell_word("100%", false).as_deref(), Some("100%"));
    }

    #[test]
    fn text_in_any_encoding() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFabc"), "abc");
        assert_eq!(decode_text(&[0xFF, 0xFE, b'a', 0, 0x0D, 0x01]), "ač");
        assert_eq!(decode_text(&[0xFE, 0xFF, 0, b'a']), "a");
        assert_eq!(decode_text("Účty".as_bytes()), "Účty");
        assert!(!decode_text(&[0xDA, 0xE8, b't', b'y']).is_empty(), "ANSI text reads somehow");
    }

    #[test]
    fn fitting_czech_text() {
        let name = "Příliš žluťoučký kůň.txt";
        assert_eq!(width(&fit(name, 30)), 30);
        let short = fit(name, 12);
        assert_eq!(width(&short), 12);
        assert!(short.ends_with(".txt"), "{short}");
        assert!(short.contains('~'));
        assert_eq!(fit_left(r"C:\a\very\long\path", 8), "…ng\\path");
        assert_eq!(center("ab", 6), "  ab  ");
    }

    #[test]
    fn wrapping_words() {
        assert_eq!(wrap("The text of \"a.txt\" is on the clipboard", 20), ["The text of \"a.txt\"", "is on the clipboard"]);
        assert_eq!(wrap("short", 20), ["short"]);
        assert_eq!(wrap("a  b", 20), ["a b"], "spaces between words count once");
        assert_eq!(wrap("in C:\\a\\very\\long\\path here", 8), ["in", "C:\\a\\ver", "y\\long\\p", "ath here"]);
        assert_eq!(wrap("one\ntwo", 20), ["one", "two"]);
        assert_eq!(wrap("žluťoučký kůň", 9), ["žluťoučký", "kůň"], "columns, not bytes");
        assert_eq!(wrap("日本語", 3), ["日", "本", "語"], "wide letters");
        assert_eq!(wrap("", 5), Vec::<String>::new());
    }

    #[test]
    fn splitting_commands() {
        assert_eq!(split_command("nvim -R"), ["nvim", "-R"]);
        assert_eq!(
            split_command(r#""C:\Program Files\Neovim\bin\nvim.exe" -R"#),
            [r"C:\Program Files\Neovim\bin\nvim.exe", "-R"]
        );
        assert!(split_command("   ").is_empty());
    }

    #[test]
    fn base64_encoding() {
        assert_eq!(base64(b"Hi"), "SGk=");
        assert_eq!(base64(b"Hey"), "SGV5");
        assert_eq!(base64(b"H"), "SA==");
    }
}
