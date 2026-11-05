//! The clipboard for files, as in Explorer and Total Commander. Ctrl+C (Ctrl+X to move) in rmc
//! puts the marked files on it, so Explorer, a mail or a chat can paste them; Ctrl+Shift+C
//! puts their full paths there as text. Alt+V (or Ctrl+V, where the terminal lets it through)
//! pastes files copied or cut in Explorer or in rmc.
//!
//! Elsewhere than Windows there is no system clipboard for files: rmc keeps its own, and the
//! paths go to the terminal's clipboard as text (OSC 52, which works over ssh too).

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Copy,
    /// Cut: the paste moves the files.
    Move,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Files {
    pub paths: Vec<PathBuf>,
    pub effect: Effect,
}

/// The system's clipboard on Windows; rmc's own elsewhere, and in tests (so `cargo test`
/// never replaces what the user copied).
#[cfg_attr(not(windows), allow(dead_code))]
const SYSTEM: bool = cfg!(all(windows, not(test)));

#[derive(Default)]
pub struct Clipboard {
    /// rmc's own clipboard, where the system has none for files.
    own: Option<Files>,
}

impl Clipboard {
    /// Puts `paths` on the clipboard. Returns text for the terminal to put on its clipboard
    /// (OSC 52), where that is how the paths get there.
    pub fn put(&mut self, paths: &[PathBuf], effect: Effect) -> Result<Option<String>, String> {
        #[cfg(windows)]
        if SYSTEM {
            return win::set_files(paths, effect).map(|()| None);
        }
        self.own = Some(Files { paths: paths.to_vec(), effect });
        Ok(Some(osc52(&as_text(paths))))
    }

    /// The files on the clipboard, if it holds any.
    pub fn files(&self) -> Option<Files> {
        #[cfg(windows)]
        if SYSTEM {
            return win::get_files();
        }
        self.own.clone()
    }

    /// Ctrl+Shift+C: the paths as text, one per line, to paste into anything that takes
    /// text. Returns text for the terminal's clipboard (OSC 52), where that is the way.
    pub fn put_text(&mut self, paths: &[PathBuf]) -> Result<Option<String>, String> {
        let text = as_text(paths);
        #[cfg(windows)]
        if SYSTEM {
            return win::set_text(&text).map(|()| None);
        }
        Ok(Some(osc52(&text)))
    }

    /// After moved files were pasted, the clipboard no longer says where they are; Explorer
    /// empties it then, and so does rmc.
    pub fn moved(&mut self) {
        #[cfg(windows)]
        if SYSTEM {
            win::clear();
        }
        self.own = None;
    }
}

/// The paths as text, one per line.
pub fn as_text(paths: &[PathBuf]) -> String {
    let lines: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    lines.join(if cfg!(windows) { "\r\n" } else { "\n" })
}

/// Asks the terminal to put `text` on the system clipboard (OSC 52).
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Where a pasted copy of `name` goes in `dir`: the same name, or when that is the file
/// itself (pasting into the folder it came from) "name - Copy.ext", "name - Copy (2).ext"…,
/// as Explorer names them.
pub fn paste_target(source: &Path, dir: &Path) -> Option<PathBuf> {
    let name = source.file_name()?;
    let target = dir.join(name);
    if !crate::util::same_path(source, &target) && !crate::util::same_file(source, &target) {
        return Some(target);
    }
    let name = Path::new(name);
    let is_dir = crate::lfs::is_dir(source);
    let stem = if is_dir { name.as_os_str() } else { name.file_stem().unwrap_or(name.as_os_str()) };
    let stem = stem.to_string_lossy();
    let ext = if is_dir { None } else { name.extension().map(|e| e.to_string_lossy()) };
    (1..1000).map(|n| {
        let copy = if n == 1 { format!("{stem} - Copy") } else { format!("{stem} - Copy ({n})") };
        dir.join(match &ext {
            Some(ext) => format!("{copy}.{ext}"),
            None => copy,
        })
    })
    .find(|candidate| crate::lfs::symlink_metadata(candidate).is_err())
}

#[cfg(windows)]
mod win {
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use std::ptr::null_mut;
    use std::thread;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL, POINT};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
    use windows_sys::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_MOVE};
    use windows_sys::Win32::UI::Shell::{DROPFILES, DragQueryFileW, HDROP};

    use super::{Effect, Files};

    /// The clipboard, open until dropped. Another program may hold it for a moment, so
    /// opening tries a few times.
    struct Open;

    impl Open {
        fn new() -> Result<Open, String> {
            // With no owner window, EmptyClipboard leaves the clipboard without an owner, and
            // what is put on it afterwards doesn't outlive rmc reliably; the console window
            // will do as the owner.
            let owner = unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() };
            for _ in 0..25 {
                if unsafe { OpenClipboard(owner) } != 0 {
                    return Ok(Open);
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err("The clipboard is in use by another program; try again.".into())
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            unsafe { CloseClipboard() };
        }
    }

    /// The format Explorer uses to say whether files were copied or cut.
    fn drop_effect_format() -> u32 {
        let name: Vec<u16> = "Preferred DropEffect".encode_utf16().chain([0]).collect();
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    }

    /// Hands `bytes` to the clipboard (which then owns the memory).
    fn put(format: u32, bytes: &[u8]) -> Result<(), String> {
        let failed = || format!("Cannot put the files on the clipboard: {}.", io::Error::last_os_error());
        unsafe {
            let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
            if mem.is_null() {
                return Err(failed());
            }
            let ptr = GlobalLock(mem);
            if ptr.is_null() {
                GlobalFree(mem);
                return Err(failed());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
            GlobalUnlock(mem);
            if SetClipboardData(format, mem).is_null() {
                let error = failed();
                GlobalFree(mem);
                return Err(error);
            }
        }
        Ok(())
    }

    fn wide_bytes(units: &[u16]) -> Vec<u8> {
        units.iter().flat_map(|u| u.to_le_bytes()).collect()
    }

    /// The files as Explorer puts them there: a DROPFILES list (CF_HDROP), copied or cut
    /// ("Preferred DropEffect"). Not as text as well: a Remote Desktop session that shares
    /// the clipboard sends text to the remote side, which can send it straight back, and
    /// mstsc then replaces the clipboard with that text, files gone.
    pub fn set_files(paths: &[PathBuf], effect: Effect) -> Result<(), String> {
        let header = size_of::<DROPFILES>();
        let files = DROPFILES { pFiles: header as u32, pt: POINT { x: 0, y: 0 }, fNC: 0, fWide: 1 };
        let mut hdrop = vec![0u8; header];
        unsafe { std::ptr::copy_nonoverlapping((&raw const files).cast::<u8>(), hdrop.as_mut_ptr(), header) };
        let mut names: Vec<u16> = Vec::new();
        for path in paths {
            names.extend(path.as_os_str().encode_wide());
            names.push(0);
        }
        names.push(0);
        hdrop.extend(wide_bytes(&names));
        let effect = if effect == Effect::Move { DROPEFFECT_MOVE } else { DROPEFFECT_COPY };
        let _open = Open::new()?;
        unsafe { EmptyClipboard() };
        put(CF_HDROP as u32, &hdrop)?;
        put(drop_effect_format(), &effect.to_le_bytes())
    }

    /// Text, for pasting paths anywhere.
    pub fn set_text(text: &str) -> Result<(), String> {
        let text: Vec<u16> = text.encode_utf16().chain([0]).collect();
        let _open = Open::new()?;
        unsafe { EmptyClipboard() };
        put(CF_UNICODETEXT as u32, &wide_bytes(&text))
    }

    pub fn get_files() -> Option<Files> {
        if unsafe { IsClipboardFormatAvailable(CF_HDROP as u32) } == 0 {
            return None;
        }
        let _open = Open::new().ok()?;
        let handle = unsafe { GetClipboardData(CF_HDROP as u32) };
        if handle.is_null() {
            return None;
        }
        let hdrop = handle as HDROP;
        let count = unsafe { DragQueryFileW(hdrop, u32::MAX, null_mut(), 0) };
        let mut paths = Vec::new();
        for i in 0..count {
            let len = unsafe { DragQueryFileW(hdrop, i, null_mut(), 0) };
            let mut buf = vec![0u16; len as usize + 1];
            let got = unsafe { DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32) };
            buf.truncate(got as usize);
            if !buf.is_empty() {
                paths.push(PathBuf::from(OsString::from_wide(&buf)));
            }
        }
        (!paths.is_empty()).then(|| Files { paths, effect: read_effect() })
    }

    /// Cut or copied: Explorer says DROPEFFECT_MOVE for cut, COPY (with LINK) for copy.
    fn read_effect() -> Effect {
        let handle = unsafe { GetClipboardData(drop_effect_format()) };
        if handle.is_null() || unsafe { GlobalSize(handle) } < 4 {
            return Effect::Copy;
        }
        let mut value = [0u8; 4];
        unsafe {
            let ptr = GlobalLock(handle);
            if ptr.is_null() {
                return Effect::Copy;
            }
            std::ptr::copy_nonoverlapping(ptr.cast::<u8>(), value.as_mut_ptr(), 4);
            GlobalUnlock(handle);
        }
        let value = u32::from_le_bytes(value);
        if value & DROPEFFECT_MOVE != 0 && value & DROPEFFECT_COPY == 0 { Effect::Move } else { Effect::Copy }
    }

    pub fn clear() {
        if let Ok(_open) = Open::new() {
            unsafe { EmptyClipboard() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn base64_as_terminals_expect() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("Účty".as_bytes()), "w5rEjXR5");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn pasting_into_the_same_folder_makes_a_copy_beside_it() {
        let tmp = TempDir::new("paste-target");
        let file = tmp.file("report.txt", "x");
        let other = tmp.dir("other");
        assert_eq!(paste_target(&file, &other), Some(other.join("report.txt")));
        assert_eq!(paste_target(&file, tmp.path()), Some(tmp.path().join("report - Copy.txt")));
        tmp.file("report - Copy.txt", "x");
        assert_eq!(paste_target(&file, tmp.path()), Some(tmp.path().join("report - Copy (2).txt")));
        let dir = tmp.dir("photos.2024");
        assert_eq!(paste_target(&dir, tmp.path()), Some(tmp.path().join("photos.2024 - Copy")));
    }

    /// The real Windows clipboard: what rmc puts there reads back, cut included. (It replaces
    /// whatever was on the clipboard, so it runs only when asked: RMC_CLIPBOARD_TEST=1.)
    #[cfg(windows)]
    #[test]
    fn files_go_through_the_windows_clipboard() {
        if std::env::var_os("RMC_CLIPBOARD_TEST").is_none() {
            return;
        }
        let tmp = TempDir::new("clipboard");
        let paths = vec![tmp.file("a.txt", "a"), tmp.file("Účty a faktury.txt", "b")];
        win::set_files(&paths, Effect::Move).unwrap();
        assert_eq!(win::get_files(), Some(Files { paths: paths.clone(), effect: Effect::Move }));
        win::set_files(&paths[..1], Effect::Copy).unwrap();
        assert_eq!(win::get_files().map(|f| (f.paths.len(), f.effect)), Some((1, Effect::Copy)));
        win::clear();
        assert_eq!(win::get_files(), None);
        // RMC_CLIPBOARD_LEAVE: leave two cut files on it, for another program to read.
        if std::env::var_os("RMC_CLIPBOARD_LEAVE").is_some() {
            let kept = std::env::temp_dir().join("rmc-clipboard-check");
            let _ = std::fs::create_dir_all(&kept);
            let files = [kept.join("a.txt"), kept.join("Účty a faktury.txt")];
            for file in &files {
                std::fs::write(file, "x").unwrap();
            }
            win::set_files(&files, Effect::Move).unwrap();
        }
    }
}
