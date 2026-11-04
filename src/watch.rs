//! Watching the panels' folders, so a panel shows what other programs change in it (a
//! download finishing, a build writing files) without Ctrl+R, as Explorer and Total
//! Commander do.
//!
//! Windows uses change notifications (FindFirstChangeNotification), Linux inotify. Both are
//! polled from the main loop; neither needs a thread. Removable and optical drives aren't
//! watched on Windows: an open watch would keep Windows from ejecting them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long after the first change the panel reloads: a burst of changes (a copy of many
/// files) gives one reload, not one per file.
const SETTLE: Duration = Duration::from_millis(250);
/// The least time between two reloads while changes keep coming (a file being written).
const GAP: Duration = Duration::from_millis(1000);

pub struct Watch {
    pub dir: PathBuf,
    handle: Option<sys::Handle>,
    /// A change was seen; the panel reloads at this time.
    due: Option<Instant>,
    last: Option<Instant>,
}

impl Watch {
    pub fn new(dir: &Path) -> Watch {
        Watch { dir: dir.to_path_buf(), handle: sys::Handle::open(dir), due: None, last: None }
    }

    /// Whether the folder is really being watched (not on a removable drive, say).
    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.handle.is_some()
    }

    /// Whether the panel should reload now: something changed, things have settled, and the
    /// last reload isn't too recent.
    pub fn due(&mut self, now: Instant) -> bool {
        if self.handle.as_mut().is_some_and(sys::Handle::changed) && self.due.is_none() {
            self.due = Some(now + SETTLE);
        }
        let Some(due) = self.due else { return false };
        let rested = self.last.is_none_or(|last| now >= last + GAP);
        if now >= due && rested {
            self.due = None;
            self.last = Some(now);
            return true;
        }
        false
    }

    /// A change is waiting to be shown (the main loop then wakes up sooner).
    pub fn pending(&self) -> bool {
        self.due.is_some()
    }
}

#[cfg(windows)]
mod sys {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Path, Prefix};

    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FindCloseChangeNotification,
        FindFirstChangeNotificationW, FindNextChangeNotification, GetDriveTypeW,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    use windows_sys::Win32::System::WindowsProgramming::{DRIVE_CDROM, DRIVE_REMOVABLE};

    /// The change-notification handle (as an integer, so the panel stays `Send`).
    pub struct Handle(isize);

    impl Handle {
        pub fn open(dir: &Path) -> Option<Handle> {
            if !watchable(dir) {
                return None;
            }
            let filter = FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_SIZE
                | FILE_NOTIFY_CHANGE_LAST_WRITE
                | FILE_NOTIFY_CHANGE_ATTRIBUTES;
            let path = wide_path(dir);
            let handle = unsafe { FindFirstChangeNotificationW(path.as_ptr(), 0, filter) };
            (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Handle(handle as isize))
        }

        /// Whether a change arrived since the last look; asks for the next one if so.
        pub fn changed(&mut self) -> bool {
            let handle = self.0 as _;
            if unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0 {
                return false;
            }
            unsafe { FindNextChangeNotification(handle) };
            true
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { FindCloseChangeNotification(self.0 as _) };
        }
    }

    /// Not on removable or optical drives, which a watch would keep from being ejected.
    fn watchable(dir: &Path) -> bool {
        let Some(Component::Prefix(prefix)) = dir.components().next() else { return true };
        let letter = match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
            _ => return true,
        };
        let root: Vec<u16> = format!("{}:\\", letter as char).encode_utf16().chain([0]).collect();
        !matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, DRIVE_REMOVABLE | DRIVE_CDROM)
    }

    /// The path as a wide string; long ones get the `\\?\` prefix the API needs for them.
    fn wide_path(dir: &Path) -> Vec<u16> {
        let text = dir.as_os_str();
        let mut wide: Vec<u16> = Vec::new();
        let plain = text.to_string_lossy();
        if text.len() > 240 && !plain.starts_with(r"\\?\") {
            if let Some(unc) = plain.strip_prefix(r"\\") {
                wide.extend(r"\\?\UNC\".encode_utf16());
                wide.extend(unc.encode_utf16());
                wide.push(0);
                return wide;
            }
            wide.extend(r"\\?\".encode_utf16());
        }
        wide.extend(text.encode_wide());
        wide.push(0);
        wide
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// An inotify descriptor watching one folder.
    pub struct Handle(i32);

    impl Handle {
        pub fn open(dir: &Path) -> Option<Handle> {
            let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if fd < 0 {
                return None;
            }
            let handle = Handle(fd);
            let mask = libc::IN_CREATE
                | libc::IN_DELETE
                | libc::IN_MOVED_FROM
                | libc::IN_MOVED_TO
                | libc::IN_MODIFY
                | libc::IN_ATTRIB
                | libc::IN_CLOSE_WRITE
                | libc::IN_DELETE_SELF
                | libc::IN_MOVE_SELF;
            (unsafe { libc::inotify_add_watch(fd, path.as_ptr(), mask) } >= 0).then_some(handle)
        }

        /// Whether events arrived since the last look (they are read and dropped).
        pub fn changed(&mut self) -> bool {
            let mut buf = [0u8; 4096];
            let mut any = false;
            loop {
                let n = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    return any;
                }
                any = true;
            }
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }
}

/// Elsewhere (macOS is untried): no watching, Ctrl+R reloads.
#[cfg(not(any(windows, target_os = "linux")))]
mod sys {
    use std::path::Path;

    pub struct Handle;

    impl Handle {
        pub fn open(_dir: &Path) -> Option<Handle> {
            None
        }

        pub fn changed(&mut self) -> bool {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Waits up to two seconds for the watch to report a change. `clock` is the time the
    /// watch is told; it moves on far enough at each look for the settle time and the gap
    /// to have passed.
    fn wait_for_change(watch: &mut Watch, clock: &mut Instant) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            *clock += Duration::from_secs(10);
            if watch.due(*clock) {
                return true;
            }
            // A change seen just now is due once things have settled.
            if watch.pending() {
                *clock += Duration::from_secs(10);
                return watch.due(*clock);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn notices_files_created_and_deleted_by_others() {
        let tmp = TempDir::new("watch");
        let mut watch = Watch::new(tmp.path());
        assert!(watch.is_active());
        let mut clock = Instant::now();
        assert!(!watch.due(clock), "nothing changed yet");
        let file = tmp.file("new.txt", "x");
        assert!(wait_for_change(&mut watch, &mut clock), "a new file is noticed");
        std::fs::remove_file(&file).unwrap();
        assert!(wait_for_change(&mut watch, &mut clock), "a deleted file is noticed");
    }

    #[test]
    fn waits_for_changes_to_settle_and_spaces_reloads_out() {
        let tmp = TempDir::new("watch-settle");
        let mut watch = Watch::new(tmp.path());
        tmp.file("a.txt", "x");
        let start = Instant::now();
        // Seen at once, but due only after the settle time.
        let mut seen = false;
        while start.elapsed() < Duration::from_secs(2) && !seen {
            seen = watch.handle.as_mut().is_some_and(sys::Handle::changed);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(seen);
        let now = Instant::now();
        watch.due = Some(now + SETTLE);
        assert!(!watch.due(now));
        assert!(watch.pending());
        assert!(watch.due(now + SETTLE));
        // Another change right after a reload waits for the gap.
        watch.due = Some(now + SETTLE);
        assert!(!watch.due(now + SETTLE + Duration::from_millis(10)));
        assert!(watch.due(now + SETTLE + GAP));
    }
}
