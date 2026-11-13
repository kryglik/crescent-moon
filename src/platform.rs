//! Operating-system specific parts: drives, free space, hidden files, opening files and shells.

use std::ffi::OsString;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use crate::util;

/// What the delete confirmation calls the place deleted files go to.
pub const TRASH_NAME: &str = if cfg!(windows) { "the Recycle Bin" } else { "the trash" };

pub struct DriveInfo {
    pub root: PathBuf,
    /// Volume label, or for a network drive the share it is mapped to.
    pub label: String,
    pub kind: &'static str,
    /// Free and total bytes.
    pub space: Option<(u64, u64)>,
    /// The space is still being asked for (network and optical drives).
    pub pending: bool,
}

/// Asks for the free space of slow drives on background threads, so an offline share
/// cannot freeze the screen. Each answer arrives as `(root, space)`.
pub fn query_space_later(roots: Vec<PathBuf>) -> Receiver<(PathBuf, Option<(u64, u64)>)> {
    let (tx, rx) = mpsc::channel();
    for root in roots {
        let tx = tx.clone();
        thread::spawn(move || {
            let space = disk_space(&root);
            let _ = tx.send((root, space));
        });
    }
    rx
}

/// What cm passes to everything it runs (its shell, a command typed in it, the editor): its
/// process id, so a cm started there can tell it would run inside this one.
pub const CM_PID: &str = "CM_PID";

/// The cm this process would run inside of, drawing over it and fighting it for the keys:
/// one whose shell it was started from (Ctrl+O, a command typed in cm), in the terminal cm
/// made for that shell, or one that lent it its own terminal (a command or an editor cm
/// runs). A tab or window opened from there is a terminal of its own and doesn't count.
pub fn running_inside() -> Option<u32> {
    // RMC_PID: an rmc from before the new name, whose shell this may be.
    let cm: u32 = std::env::var(CM_PID).or_else(|_| std::env::var("RMC_PID")).ok()?.parse().ok()?;
    (cm != std::process::id() && in_terminal_of(cm)).then_some(cm)
}

/// Whether, among the processes `sharing` a terminal, `cm` is one or started one.
#[cfg_attr(not(windows), allow(dead_code))]
fn shares_terminal(cm: u32, sharing: &[u32], parent: impl Fn(u32) -> Option<u32>) -> bool {
    sharing.iter().any(|&pid| pid == cm || parent(pid) == Some(cm))
}

/// Windows: the terminal is the console, and the processes attached to it say whose it is.
#[cfg(windows)]
fn in_terminal_of(cm: u32) -> bool {
    use std::collections::HashMap;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    let mut attached = vec![0u32; 16];
    loop {
        let n = unsafe { GetConsoleProcessList(attached.as_mut_ptr(), attached.len() as u32) } as usize;
        if n == 0 {
            return false;
        }
        if n <= attached.len() {
            attached.truncate(n);
            break;
        }
        attached = vec![0; n + 8];
    }
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut parents = HashMap::new();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more {
        parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    // Still running: once it has ended, its number may be another program's.
    parents.contains_key(&cm) && shares_terminal(cm, &attached, |pid| parents.get(&pid).copied())
}

/// Unix: the terminal is the session: cm's own, or the one its shell leads in the terminal
/// cm made for it.
#[cfg(unix)]
fn in_terminal_of(cm: u32) -> bool {
    let session = unsafe { libc::getsid(0) };
    let cm_session = unsafe { libc::getsid(cm as libc::pid_t) };
    cm_session >= 0 && (cm_session == session || parent_of(session as u32) == Some(cm))
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (name) state ppid …", where the name may hold spaces and parentheses.
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn parent_of(_pid: u32) -> Option<u32> {
    None
}

#[cfg(windows)]
pub use win::{
    admin_can_reach, close_elevated, delete_denied, delete_privileged, disk_space, drives, enable_admin_privileges, init,
    long_wide, open_default, run_elevated, wait_elevated, wait_for_input,
};

/// Outside Windows nothing tells of Shift, Ctrl and Alt held on their own.
#[cfg(not(windows))]
pub fn wait_for_input(_timeout: std::time::Duration, _looking: bool) -> Option<crate::keybar::Held> {
    None
}

/// No "As admin" outside Windows.
#[cfg(not(windows))]
pub fn admin_can_reach(_path: &Path) -> bool {
    false
}

/// What Alt+A sets: Windows' attributes.
#[cfg(windows)]
pub const ATTRIBUTES: &[(&str, u32)] = &[("Read-only", 0x1), ("Hidden", 0x2), ("System", 0x4), ("Archive", 0x20)];

/// What Alt+A sets: the permission bits, as mc's chmod.
#[cfg(not(windows))]
pub const ATTRIBUTES: &[(&str, u32)] = &[
    ("Owner may read", 0o400),
    ("Owner may write", 0o200),
    ("Owner may run", 0o100),
    ("Group may read", 0o40),
    ("Group may write", 0o20),
    ("Group may run", 0o10),
    ("Others may read", 0o4),
    ("Others may write", 0o2),
    ("Others may run", 0o1),
];

/// A file's attributes (Windows) or permission bits (elsewhere), as `ATTRIBUTES` counts them.
#[cfg(windows)]
pub fn attributes(path: &Path) -> std::io::Result<u32> {
    use std::os::windows::fs::MetadataExt;
    // The link's own, as those are the ones `set_attributes` changes.
    Ok(crate::lfs::symlink_metadata(path)?.file_attributes())
}

#[cfg(not(windows))]
pub fn attributes(path: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode())
}

/// Sets what `attributes` returned, changed. Only the bits in `ATTRIBUTES` (and on Windows
/// the others a program may set) are taken.
#[cfg(windows)]
pub fn set_attributes(path: &Path, value: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
        FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_PINNED, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
        FILE_ATTRIBUTE_TEMPORARY, FILE_ATTRIBUTE_UNPINNED, SetFileAttributesW,
    };
    // Pinned and unpinned too: OneDrive's "always keep on this device" stays as it was.
    let settable = FILE_ATTRIBUTE_ARCHIVE
        | FILE_ATTRIBUTE_HIDDEN
        | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED
        | FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_PINNED
        | FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_SYSTEM
        | FILE_ATTRIBUTE_TEMPORARY
        | FILE_ATTRIBUTE_UNPINNED;
    let value = match value & settable {
        0 => FILE_ATTRIBUTE_NORMAL,
        bits => bits,
    };
    if unsafe { SetFileAttributesW(long_wide(path).as_ptr(), value) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn set_attributes(path: &Path, value: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(value & 0o7777))
}

/// A process started as administrator (its Windows handle).
#[cfg(windows)]
pub struct Elevated(isize);

/// The mount point holding `path`; on Windows the drive is in the path itself (`util::volume_key`).
#[cfg(windows)]
pub fn mount_point(_path: &Path) -> Option<PathBuf> {
    None
}

/// Where file systems are mounted (see the Linux one). On Windows a mounted folder is a
/// reparse point, which deleting treats as a link anyway.
#[cfg(windows)]
pub fn mount_dirs() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(windows)]
pub fn is_mount(_path: &Path, _meta: &Metadata, _mounts: &[PathBuf]) -> bool {
    false
}

/// On Windows the Recycle Bin says for itself what it won't take.
#[cfg(windows)]
pub fn trash_refusal(_path: &Path) -> Option<String> {
    None
}

/// Whether a process with this id exists (one we may not look at does too).
#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    unsafe { CloseHandle(handle) };
    true
}

#[cfg(not(windows))]
pub fn process_alive(pid: u32) -> bool {
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    alive || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Who cm runs as and on which computer, for the command line's prompt, as bash's `\u@\h`
/// says it: `me@nas`, the host's name up to its first dot. None when either is unknown.
pub fn user_at_host() -> Option<String> {
    let known = |text: Option<String>| text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    #[cfg(windows)]
    let (user, host) = (std::env::var("USERNAME").ok(), std::env::var("COMPUTERNAME").ok());
    #[cfg(not(windows))]
    let (user, host) = {
        let user = if is_root() {
            Some("root".to_string())
        } else {
            std::env::var("USER").ok().filter(|u| !u.is_empty()).or_else(|| std::env::var("LOGNAME").ok())
        };
        let mut buffer = [0u8; 256];
        let host = (unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0).then(|| {
            let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
            String::from_utf8_lossy(&buffer[..end]).into_owned()
        });
        (user, host)
    };
    let host = known(host)?.split('.').next().unwrap_or_default().to_string();
    Some(format!("{}@{host}", known(user)?)).filter(|_| !host.is_empty())
}

/// Whether cm runs as root, whose prompt ends in `#` as bash's does. (Never on Windows.)
pub fn is_root() -> bool {
    #[cfg(windows)]
    return false;
    #[cfg(not(windows))]
    return unsafe { libc::geteuid() } == 0;
}

#[cfg(windows)]
pub fn is_hidden(_name: &str, meta: Option<&Metadata>) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;
    meta.is_some_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
}

#[cfg(windows)]
pub fn is_executable(name: &str, _meta: Option<&Metadata>) -> bool {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(ext.as_str(), "exe" | "com" | "bat" | "cmd" | "ps1")
}

#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SetErrorMode};
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    use super::DriveInfo;
    use crate::keybar::Held;

    fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// The console's input, opened as crossterm opens it: stdin may be something else.
    fn console_input() -> Option<isize> {
        use std::sync::OnceLock;
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
        static INPUT: OnceLock<Option<isize>> = OnceLock::new();
        *INPUT.get_or_init(|| {
            let name = wide("CONIN$");
            let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
            let access = GENERIC_READ | GENERIC_WRITE;
            let handle = unsafe { CreateFileW(name.as_ptr(), access, share, null(), OPEN_EXISTING, 0, null_mut()) };
            (handle != INVALID_HANDLE_VALUE).then_some(handle as isize)
        })
    }

    /// What is down on the keyboard, whichever window has the focus.
    fn held_on_the_keyboard() -> Held {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
        let down = |key: u16| (unsafe { GetAsyncKeyState(key as i32) } as u16) & 0x8000 != 0;
        Held::of(down(VK_SHIFT), down(VK_CONTROL), down(VK_MENU))
    }

    /// Waits up to `timeout` for something to read on the console, and says what is held down
    /// (see `Held`). Every key comes with the state of Shift, Ctrl and Alt, and one of those
    /// pressed alone is a key too, which crossterm reads and drops: so this only looks. A key
    /// let go of in another window (Alt+Tab) is never heard of here, so with `looking`, what
    /// was down is asked of the keyboard. With no console, None.
    pub fn wait_for_input(timeout: std::time::Duration, looking: bool) -> Option<Held> {
        use windows_sys::Win32::System::Console::{
            INPUT_RECORD, KEY_EVENT, LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, PeekConsoleInputW, RIGHT_ALT_PRESSED,
            RIGHT_CTRL_PRESSED, SHIFT_PRESSED,
        };
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        let input = console_input()? as *mut std::ffi::c_void;
        unsafe { WaitForSingleObject(input, timeout.as_millis().min(u32::MAX as u128) as u32) };
        let mut records: [INPUT_RECORD; 16] = unsafe { std::mem::zeroed() };
        let mut count = 0;
        unsafe { PeekConsoleInputW(input, records.as_mut_ptr(), records.len() as u32, &mut count) };
        let keys = records[..count as usize].iter().filter(|record| u32::from(record.EventType) == KEY_EVENT);
        // The union holds a key: the filter says so.
        let state = keys.last().map(|record| unsafe { record.Event.KeyEvent.dwControlKeyState });
        Some(match state {
            Some(state) => {
                let (ctrl, alt) = (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED, LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED);
                Held::of(state & SHIFT_PRESSED != 0, state & ctrl != 0, state & alt != 0)
            }
            None if looking => held_on_the_keyboard(),
            None => Held::Nothing,
        })
    }

    /// A path as a wide string for raw Windows calls, in the literal `\\?\` form when it is
    /// long (those calls stop at 260 characters without it) or has a name Windows would read
    /// otherwise (`foo.`, `nul`): see `lfs::literal`.
    pub fn long_wide(path: &Path) -> Vec<u16> {
        wide(crate::lfs::literal(path).as_os_str())
    }

    /// Whether cm running as administrator can get at `path`. Not on a network share or a
    /// mapped drive (they belong to the user's own logon session, and an administrator's
    /// privileges don't count on another machine), nor on a subst drive (also per session).
    pub fn admin_can_reach(path: &Path) -> bool {
        use std::path::{Component, Prefix};
        use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
        let Some(Component::Prefix(prefix)) = path.components().next() else { return false };
        let letter = match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter as char,
            _ => return false,
        };
        if unsafe { GetDriveTypeW(wide(format!("{letter}:\\")).as_ptr()) } == DRIVE_REMOTE {
            return false;
        }
        // A subst drive's device is a folder ("\??\C:\work") rather than a volume.
        let mut buf = [0u16; 1024];
        let n = unsafe { QueryDosDeviceW(wide(format!("{letter}:")).as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        // Unknown when Windows can't say: not offered.
        n != 0 && !String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]).starts_with(r"\??\")
    }

    /// Stops Windows from popping up "There is no disk in the drive" boxes.
    pub fn init() {
        unsafe { SetErrorMode(SEM_FAILCRITICALERRORS) };
    }

    pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
        let path = wide(path);
        let (mut free, mut total) = (0u64, 0u64);
        let ok = unsafe { GetDiskFreeSpaceExW(path.as_ptr(), &mut free, &mut total, null_mut()) };
        (ok != 0).then_some((free, total))
    }

    fn volume_label(root: &[u16]) -> String {
        let mut buf = [0u16; 261];
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                0,
            )
        };
        if ok == 0 {
            return String::new();
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    /// The share a mapped network drive points to, such as `\\nas\photos`.
    fn remote_name(letter: char) -> String {
        let local = wide(format!("{letter}:"));
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        if unsafe { WNetGetConnectionW(local.as_ptr(), buf.as_mut_ptr(), &mut len) } != 0 {
            return String::new();
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// All drives. Local ones come with their label and free space; for network and optical
    /// drives, which can block for seconds when offline or empty, the space is left `pending`
    /// (see `query_space_later`).
    pub fn drives() -> Vec<DriveInfo> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| {
                let letter = (b'A' + i) as char;
                let root = format!("{letter}:\\");
                let root_w = wide(&root);
                let code = unsafe { GetDriveTypeW(root_w.as_ptr()) };
                let kind = match code {
                    DRIVE_FIXED => "fixed",
                    DRIVE_REMOVABLE => "removable",
                    DRIVE_REMOTE => "network",
                    DRIVE_CDROM => "cd/dvd",
                    DRIVE_RAMDISK => "ram disk",
                    _ => "unknown",
                };
                let local = matches!(code, DRIVE_FIXED | DRIVE_REMOVABLE | DRIVE_RAMDISK);
                let label = match code {
                    _ if local => volume_label(&root_w),
                    DRIVE_REMOTE => remote_name(letter),
                    _ => String::new(),
                };
                DriveInfo {
                    label,
                    space: if local { disk_space(Path::new(&root)) } else { None },
                    pending: !local,
                    root: PathBuf::from(root),
                    kind,
                }
            })
            .collect()
    }

    /// Why `path` can't be deleted, if Windows says so: in use by another program (ResourceBusy),
    /// or no permission (PermissionDenied: neither DELETE on it nor "delete child" on its
    /// folder), naming the owner. None when nothing stands in the way, or it can't tell.
    pub fn delete_denied(path: &Path) -> Option<std::io::Error> {
        use std::io::{Error, ErrorKind};
        let reason = delete_denied_reason(path)?;
        let kind = if reason.starts_with("Access") { ErrorKind::PermissionDenied } else { ErrorKind::ResourceBusy };
        Some(Error::new(kind, reason))
    }

    fn delete_denied_reason(path: &Path) -> Option<String> {
        use windows_sys::Win32::Foundation::{
            CloseHandle, ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, GetLastError, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, DELETE, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        let open = |path: &Path, access: u32| {
            let name = long_wide(path);
            let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
            let flags = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;
            let handle = unsafe { CreateFileW(name.as_ptr(), access, share, null(), OPEN_EXISTING, flags, null_mut()) };
            if handle == INVALID_HANDLE_VALUE {
                Err(unsafe { GetLastError() })
            } else {
                unsafe { CloseHandle(handle) };
                Ok(())
            }
        };
        match open(path, DELETE) {
            Err(ERROR_SHARING_VIOLATION) => return Some("It is in use by another program.".into()),
            Err(ERROR_ACCESS_DENIED) => {}
            _ => return None,
        }
        if path.parent().is_some_and(|parent| open(parent, FILE_DELETE_CHILD).is_ok()) {
            return None;
        }
        Some(match owner(path) {
            Some(owner) if owner.ends_with("\\TrustedInstaller") => format!(
                "Access is denied: it belongs to {owner} (protected Windows files).\n\
                 An administrator has to take ownership of it before it can be deleted."
            ),
            Some(owner) if is_me(&owner) => "Access is denied: it is yours, but you have no permission to delete it.\n\
                 Give yourself full control first (icacls, as administrator)."
                .into(),
            Some(owner) => format!("Access is denied: you may not delete it (it belongs to {owner})."),
            None => "Access is denied: you may not delete it.".into(),
        })
    }

    /// Deletes one file, link or empty folder by handle with backup semantics. With the restore
    /// privilege enabled (the elevated helper does that) Windows skips the permission check,
    /// so even TrustedInstaller's files go; read-only ones too.
    pub fn delete_privileged(path: &Path) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
            FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FileDispositionInfo, FileDispositionInfoEx, OPEN_EXISTING, SetFileInformationByHandle,
        };
        let name = long_wide(path);
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        let flags = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;
        let handle = unsafe { CreateFileW(name.as_ptr(), DELETE, share, null(), OPEN_EXISTING, flags, null_mut()) };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let info = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        };
        let size = std::mem::size_of_val(&info) as u32;
        let mut ok = unsafe { SetFileInformationByHandle(handle, FileDispositionInfoEx, (&raw const info).cast(), size) };
        if ok == 0 {
            // Older Windows or file systems without the "Ex" form.
            let info = FILE_DISPOSITION_INFO { DeleteFile: true };
            let size = std::mem::size_of_val(&info) as u32;
            ok = unsafe { SetFileInformationByHandle(handle, FileDispositionInfo, (&raw const info).cast(), size) };
        }
        let error = std::io::Error::last_os_error();
        unsafe { CloseHandle(handle) };
        if ok == 0 { Err(error) } else { Ok(()) }
    }

    /// Turns on the backup, restore and take-ownership privileges of this process, which an
    /// elevated administrator has but are off by default. True if all of them could be.
    pub fn enable_admin_privileges() -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NOT_ALL_ASSIGNED, GetLastError, LUID};
        use windows_sys::Win32::Security::{
            AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
            TOKEN_PRIVILEGES, TOKEN_QUERY,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let mut all = true;
        for name in ["SeBackupPrivilege", "SeRestorePrivilege", "SeTakeOwnershipPrivilege"] {
            let mut luid = LUID { LowPart: 0, HighPart: 0 };
            let name = wide(name);
            if unsafe { LookupPrivilegeValueW(null(), name.as_ptr(), &mut luid) } == 0 {
                all = false;
                continue;
            }
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            let ok = unsafe { AdjustTokenPrivileges(token, 0, &privileges, 0, null_mut(), null_mut()) };
            // It "succeeds" without the privilege too; only the last error tells.
            if ok == 0 || unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
                all = false;
            }
        }
        unsafe { CloseHandle(token) };
        all
    }

    /// Starts `program arguments` as administrator: Windows asks the user first (UAC).
    /// Returns the process handle, or ErrorKind::PermissionDenied if the user said no.
    pub fn run_elevated(program: &Path, arguments: &str) -> std::io::Result<super::Elevated> {
        use windows_sys::Win32::Foundation::{ERROR_CANCELLED, GetLastError};
        use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
        let verb = wide("runas");
        let file = wide(program);
        let parameters = wide(arguments);
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = parameters.as_ptr();
        info.nShow = SW_HIDE;
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_CANCELLED {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "administrator rights were not given"));
            }
            return Err(std::io::Error::from_raw_os_error(code as i32));
        }
        if info.hProcess.is_null() {
            return Err(std::io::Error::other("Windows did not start the administrator helper"));
        }
        Ok(super::Elevated(info.hProcess as isize))
    }

    /// Waits up to `ms` for the process; true once it has ended.
    pub fn wait_elevated(process: &super::Elevated, ms: u32) -> bool {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        unsafe { WaitForSingleObject(process.0 as _, ms) == WAIT_OBJECT_0 }
    }

    pub fn close_elevated(process: &super::Elevated) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(process.0 as _) };
    }

    /// Whether `DOMAIN\name` is the account cm runs as.
    fn is_me(account: &str) -> bool {
        let me = format!(
            "{}\\{}",
            std::env::var("USERDOMAIN").unwrap_or_default(),
            std::env::var("USERNAME").unwrap_or_default()
        );
        account.eq_ignore_ascii_case(&me)
    }

    /// The owner of a file or folder, as `DOMAIN\name`.
    fn owner(path: &Path) -> Option<String> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{LookupAccountSidW, OWNER_SECURITY_INFORMATION};
        let name = long_wide(path);
        let mut sid = null_mut();
        let mut descriptor = null_mut();
        let status = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut sid,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return None;
        }
        let (mut account, mut account_len) = ([0u16; 256], 256u32);
        let (mut domain, mut domain_len) = ([0u16; 256], 256u32);
        let mut kind = 0;
        let ok = unsafe {
            LookupAccountSidW(
                null(),
                sid,
                account.as_mut_ptr(),
                &mut account_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        };
        unsafe { LocalFree(descriptor) };
        if ok == 0 {
            return None;
        }
        let account = String::from_utf16_lossy(&account[..account_len as usize]);
        let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
        Some(if domain.is_empty() { account } else { format!("{domain}\\{account}") })
    }

    /// Opens a file in its associated Windows application.
    pub fn open_default(path: &Path) -> Result<(), super::OpenError> {
        // Windows' file-opening API stops at 260 characters: a longer path goes by its short
        // (8.3) name, where the drive keeps those.
        let short;
        // In UTF-16 units, as Windows counts: a Czech name takes more bytes than that.
        let path = if path.as_os_str().encode_wide().count() >= 260 {
            short = short_path(path).ok_or_else(|| {
                super::OpenError::Failed(format!(
                    "\"{}\" is too long a path for Windows to open (over 260 characters).",
                    path.display()
                ))
            })?;
            short.as_path()
        } else {
            path
        };
        let file = wide(path);
        let verb = wide("open");
        let dir = path.parent().map(wide);
        let result = unsafe {
            ShellExecuteW(
                null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                null(),
                dir.as_ref().map_or(null(), |d| d.as_ptr()),
                SW_SHOWNORMAL,
            )
        } as isize;
        match result {
            r if r > 32 => Ok(()),
            31 => Err(super::OpenError::Failed(format!("No app is associated with \"{}\".", path.display()))),
            r => Err(super::OpenError::Failed(format!("Windows could not open \"{}\" (error {r}).", path.display()))),
        }
    }

    /// The short (8.3) form of a long path, if it is short enough to pass on.
    fn short_path(path: &Path) -> Option<PathBuf> {
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let long = long_wide(path);
        let mut buf = vec![0u16; 1024];
        let n = unsafe { GetShortPathNameW(long.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 || n >= buf.len() {
            return None;
        }
        let short = String::from_utf16_lossy(&buf[..n]);
        let short = match short.strip_prefix(r"\\?\UNC\") {
            Some(unc) => format!(r"\\{unc}"),
            None => short.strip_prefix(r"\\?\").unwrap_or(&short).to_string(),
        };
        (short.encode_utf16().count() < 260).then(|| PathBuf::from(short))
    }
}

#[cfg(not(windows))]
pub use unix::{
    delete_denied, delete_privileged, disk_space, drives, exec_bits_mean_something, init, is_mount, mount_dirs, mount_point,
    open_default,
    runs_as_program, same_device, trash_refusal,
};

/// On Windows a program is known by its extension (see `is_executable`).
#[cfg(windows)]
pub fn exec_bits_mean_something(_dir: &Path) -> bool {
    true
}

/// Windows' "How do you want to open this file?" for `path`, in a process of its own so cm
/// goes on meanwhile.
#[cfg(windows)]
pub fn open_with(path: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // The rest of the command line is the file name, as it is (quotes would be taken as
    // part of it).
    Command::new("rundll32.exe")
        .raw_arg(format!("shell32.dll,OpenAs_RunDLL {}", path.display()))
        .spawn()
        .map(drop)
        .map_err(|e| format!("Cannot ask Windows how to open it: {e}"))
}

#[cfg(not(windows))]
pub fn open_with(_path: &Path) -> Result<(), String> {
    Err("\"Open with\" is Windows' own; here F3 views a file and Enter opens it in its app.".into())
}

#[cfg(windows)]
pub fn runs_as_program(_path: &Path) -> bool {
    true
}

#[cfg(not(windows))]
mod unix {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use super::{DriveInfo, OpenError};
    use crate::util;

    pub fn init() {}

    /// Why `path` can't be deleted: deleting needs write access to the folder it is in.
    pub fn delete_denied(path: &Path) -> Option<std::io::Error> {
        let parent = path.parent()?;
        let parent = CString::new(parent.as_os_str().as_bytes()).ok()?;
        let writable = unsafe { libc::access(parent.as_ptr(), libc::W_OK | libc::X_OK) } == 0;
        (!writable).then(|| {
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Access is denied: you may not change the folder it is in.")
        })
    }

    /// Only the Windows helper needs more than a plain delete.
    pub fn delete_privileged(path: &Path) -> std::io::Result<()> {
        if std::fs::symlink_metadata(path)?.is_dir() { std::fs::remove_dir(path) } else { std::fs::remove_file(path) }
    }

    /// Free and total bytes of the file system holding `path`.
    pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
            return None;
        }
        let block = if stats.f_frsize > 0 { stats.f_frsize } else { stats.f_bsize } as u64;
        Some((stats.f_bavail as u64 * block, stats.f_blocks as u64 * block))
    }

    struct Mount {
        dir: PathBuf,
        source: String,
        fstype: String,
    }

    /// The mounted file systems, from /proc/self/mounts (Linux). Elsewhere just `/`.
    fn mounts() -> Vec<Mount> {
        let Ok(text) = std::fs::read_to_string("/proc/self/mounts") else {
            return vec![Mount { dir: "/".into(), source: String::new(), fstype: String::new() }];
        };
        text.lines()
            .filter_map(|line| {
                let mut fields = line.split(' ');
                let (source, dir, fstype) = (fields.next()?, fields.next()?, fields.next()?);
                Some(Mount { dir: PathBuf::from(unescape(dir)), source: unescape(source), fstype: fstype.to_string() })
            })
            .collect()
    }

    /// /proc/mounts writes spaces, tabs, newlines and backslashes in octal: `\040`.
    fn unescape(field: &str) -> String {
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let octal = bytes.get(i + 1..i + 4).filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)));
            match octal {
                Some(digits) if bytes[i] == b'\\' => {
                    out.push(digits.iter().fold(0u8, |n, d| n.wrapping_mul(8) + (d - b'0')));
                    i += 4;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The mount point that holds `path`: the longest one it is inside.
    pub fn mount_point(path: &Path) -> Option<PathBuf> {
        mount_of(path).map(|m| m.dir)
    }

    /// Where file systems are mounted.
    pub fn mount_dirs() -> Vec<PathBuf> {
        mounts().into_iter().map(|m| m.dir).collect()
    }

    /// Whether another file system is mounted at folder `path`, which has `meta`: deleting
    /// must never go down into one (a chroot's /dev and /home, an sshfs of a server). A
    /// device of its own shows that, a bind mount of the same disk only in the mount list. A
    /// btrfs subvolume has a device of its own too without being mounted: the root of one is
    /// inode 256, and the list says whether it is.
    pub fn is_mount(path: &Path, meta: &std::fs::Metadata, mounts: &[PathBuf]) -> bool {
        let parent = path.parent().and_then(|parent| std::fs::metadata(parent).ok());
        let subvolume = meta.ino() == 256 && !mounts.is_empty();
        parent.is_some_and(|parent| parent.dev() != meta.dev()) && !subvolume
            || std::fs::canonicalize(path).is_ok_and(|real| mounts.contains(&real))
    }

    /// Why the trash shouldn't be given `path`, if so. The trash crate moves things by a
    /// rename; where that can't work it falls back on copying everything to the home trash
    /// and then deleting it, with no progress, no way to stop it, hanging on a pipe in the
    /// tree, and leaving things half deleted when the folder may not be changed.
    pub fn trash_refusal(path: &Path) -> Option<String> {
        if let Some(denied) = delete_denied(path) {
            return Some(denied.to_string());
        }
        let device = |path: &Path| path.ancestors().find_map(|p| std::fs::symlink_metadata(p).ok()).map(|m| m.dev());
        let data = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| util::home_dir().map(|home| home.join(".local").join("share")))?;
        if device(path) == device(&data.join("Trash")) {
            return None;
        }
        // Another file system: a trash folder there will do, if it can be used or made.
        let real = std::fs::canonicalize(path.parent()?).ok()?;
        let top = mount_point(&real)?;
        let uid = unsafe { libc::geteuid() };
        let writable = |dir: &Path| {
            CString::new(dir.as_os_str().as_bytes())
                .is_ok_and(|dir| unsafe { libc::access(dir.as_ptr(), libc::W_OK | libc::X_OK) } == 0)
        };
        let usable = [top.join(format!(".Trash-{uid}")), top.join(".Trash").join(uid.to_string()), top];
        (!usable.iter().any(|dir| writable(dir))).then(|| {
            "It is on another drive than your home, with no trash of its own you may use.\n\
             Shift+F8 deletes it permanently instead."
                .to_string()
        })
    }

    fn mount_of(path: &Path) -> Option<Mount> {
        mounts().into_iter().filter(|m| util::is_inside(path, &m.dir)).max_by_key(|m| m.dir.components().count())
    }

    /// Whether execute bits mean anything on the file system holding `dir`. WSL's Windows
    /// drives, FAT, exFAT and NTFS disks and SMB shares show every file as executable.
    pub fn exec_bits_mean_something(dir: &Path) -> bool {
        let Some(mount) = mount_of(dir) else { return true };
        let windows_like = ["9p", "drvfs", "vfat", "msdos", "exfat", "ntfs", "ntfs3", "fuseblk", "cifs", "smb3", "smbfs"];
        !windows_like.contains(&mount.fstype.as_str()) && !mount.fstype.starts_with("fuse.ntfs")
    }

    /// Whether the system can run the file as a program: a script (`#!`) or a binary (ELF;
    /// in WSL a Windows .exe too). A text file or a document with execute bits isn't one:
    /// bash would run its lines as commands.
    pub fn runs_as_program(path: &Path) -> bool {
        use std::io::Read;
        let mut head = [0u8; 4];
        let n = std::fs::File::open(path).and_then(|mut file| file.read(&mut head)).unwrap_or(0);
        let head = &head[..n];
        head.starts_with(b"#!") || head.starts_with(b"\x7fELF") || (is_wsl() && head.starts_with(b"MZ"))
    }

    /// Whether two paths live on one file system (so a move is a rename). A path that doesn't
    /// exist yet counts by its nearest existing parent.
    pub fn same_device(a: &Path, b: &Path) -> bool {
        let device = |path: &Path| path.ancestors().find_map(|p| std::fs::symlink_metadata(p).ok()).map(|m| m.dev());
        device(a) == device(b)
    }

    /// What the drive picker calls a mount, or None to leave it out (system and virtual ones).
    fn kind(mount: &Mount) -> Option<&'static str> {
        let dir = mount.dir.to_string_lossy();
        if dir == "/" {
            return Some("root");
        }
        let removable = ["/media/", "/run/media/"].iter().any(|p| dir.starts_with(p));
        let system = ["/proc", "/sys", "/dev", "/run", "/snap", "/boot", "/usr", "/var/lib", "/mnt/wsl", "/mnt/wslg"]
            .iter()
            .any(|p| dir == *p || dir.starts_with(&format!("{p}/")));
        if system && !removable {
            return None;
        }
        Some(match mount.fstype.as_str() {
            // WSL's view of a Windows drive: `C:\` on /mnt/c.
            "9p" | "drvfs" if mount.source.ends_with(":\\") => "windows",
            "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "9p" | "ceph" | "glusterfs" | "davfs" => "network",
            t if t.starts_with("fuse.") && ["sshfs", "rclone", "davfs2", "s3fs"].contains(&&t[5..]) => "network",
            _ if removable => "removable",
            "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "zfs" | "f2fs" | "jfs" | "reiserfs" | "ntfs" | "ntfs3"
            | "fuseblk" | "fuse.ntfs-3g" | "vfat" | "exfat" | "hfsplus" | "apfs" | "iso9660" | "udf" => "disk",
            _ => return None,
        })
    }

    /// The root and the mounted disks, Windows drives (in WSL) and network shares. Network
    /// ones get their space later (`query_space_later`): a dead NFS server can block for minutes.
    pub fn drives() -> Vec<DriveInfo> {
        let mut drives: Vec<DriveInfo> = Vec::new();
        for mount in mounts() {
            let Some(kind) = kind(&mount) else { continue };
            // A later mount on the same folder hides the earlier one.
            drives.retain(|d| d.root != mount.dir);
            let slow = matches!(kind, "network" | "windows");
            drives.push(DriveInfo {
                space: if slow { None } else { disk_space(&mount.dir) },
                pending: slow,
                label: if kind == "root" { String::new() } else { mount.source },
                root: mount.dir,
                kind,
            });
        }
        drives.sort_by(|a, b| a.root.cmp(&b.root));
        if drives.is_empty() {
            drives.push(DriveInfo { root: "/".into(), label: String::new(), kind: "root", space: disk_space(Path::new("/")), pending: false });
        }
        drives
    }

    /// Opens a file with the desktop's default app:
    /// - macOS: `open`;
    /// - WSL: the Windows app for it, via `wslview` or `explorer.exe` (WSLg's DISPLAY would
    ///   make xdg-open look usable, but without a Linux desktop app it falls back to a
    ///   terminal program, which then fights cm for the screen);
    /// - other Linux: `xdg-open`, only in a graphical session, for the same reason.
    ///
    /// Otherwise `NoOpener`, and cm shows the file in the viewer instead.
    pub fn open_default(path: &Path) -> Result<(), OpenError> {
        let found = |program: &str| util::find_in_path(program).is_some();
        let graphical = ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()));
        let (opener, arg) = if cfg!(target_os = "macos") {
            ("open", path.as_os_str().to_owned())
        } else if is_wsl() {
            if found("wslview") {
                ("wslview", path.as_os_str().to_owned())
            } else if found("explorer.exe") {
                ("explorer.exe", windows_path(path).ok_or(OpenError::NoOpener)?)
            } else {
                return Err(OpenError::NoOpener);
            }
        } else if graphical && found("xdg-open") {
            ("xdg-open", path.as_os_str().to_owned())
        } else {
            return Err(OpenError::NoOpener);
        };
        // Its chatter would land on cm's screen; and wait for it so it doesn't linger as a zombie.
        let mut child = Command::new(opener)
            .arg(arg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| OpenError::Failed(format!("Cannot run {opener}: {e}")))?;
        std::thread::spawn(move || child.wait());
        Ok(())
    }

    fn is_wsl() -> bool {
        std::env::var_os("WSL_DISTRO_NAME").is_some() || Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
    }

    /// The Windows form of a path in WSL (`C:\...` or `\\wsl.localhost\...`), from `wslpath -w`.
    fn windows_path(path: &Path) -> Option<std::ffi::OsString> {
        let output = Command::new("wslpath").arg("-w").arg(path).stderr(Stdio::null()).output().ok()?;
        let text = String::from_utf8(output.stdout).ok()?;
        let text = text.trim();
        (output.status.success() && !text.is_empty()).then(|| text.into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Enter runs a file only if it is a program; on a Windows drive in WSL every file has
        /// execute bits, and bash would run a text file's lines.
        #[test]
        fn only_real_programs_run() {
            use std::os::unix::fs::PermissionsExt;
            let tmp = crate::testutil::TempDir::new("programs");
            for (name, text, runs) in [("notes.txt", "echo hi\n", false), ("script", "#!/bin/sh\necho hi\n", true)] {
                let path = tmp.file(name, text);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
                assert_eq!(runs_as_program(&path), runs, "{name}");
            }
            assert!(exec_bits_mean_something(tmp.path()), "the temp folder is a Linux file system");
            if mount_of(Path::new("/mnt/c")).is_some_and(|m| m.fstype == "9p" || m.fstype == "drvfs") {
                assert!(!exec_bits_mean_something(Path::new("/mnt/c/Windows")));
            }
        }

        #[test]
        fn mount_fields_are_unescaped() {
            assert_eq!(unescape(r"/mnt/my\040disk"), "/mnt/my disk");
            assert_eq!(unescape(r"C:\134"), "C:\\");
            assert_eq!(unescape(r"plain\0"), r"plain\0");
        }

        #[test]
        fn drives_list_the_root_with_its_space() {
            let drives = drives();
            let root = drives.iter().find(|d| d.root == Path::new("/")).expect("/ is listed");
            assert!(root.space.is_some_and(|(free, total)| free <= total && total > 0));
            assert!(drives.iter().all(|d| !d.root.starts_with("/proc") && !d.root.starts_with("/sys")));
            assert_eq!(mount_point(Path::new("/proc/self")), Some(PathBuf::from("/proc")));
            assert!(same_device(Path::new("/"), Path::new("/no/such/file")), "a missing path counts by its parent");
        }
    }
}

/// Why a file couldn't be opened in its app.
#[derive(Debug)]
pub enum OpenError {
    /// There is no program for opening files (a server, or WSL without wslview).
    #[cfg_attr(windows, allow(dead_code))]
    NoOpener,
    Failed(String),
}

#[cfg(not(windows))]
pub fn is_hidden(name: &str, _meta: Option<&Metadata>) -> bool {
    name.starts_with('.')
}

#[cfg(not(windows))]
pub fn is_executable(_name: &str, meta: Option<&Metadata>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.is_some_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The key cm presses to make the shell go to the folder in `$CM_REQUEST_FILE`:
/// Ctrl+Alt+Shift+F12, as a terminal sends it. The start-up scripts below bind it.
pub const FOLLOW_KEY: &str = "\x1b[24;8~";

/// Start-up script of the PowerShell behind Ctrl+O.
///
/// - Every prompt writes `<prompt number>|<f or ->|<current folder>` to `$env:CM_CWD_FILE`.
///   The folder lets cm's panel follow `cd`; the number tells cm whether the shell is idle
///   (a prompt has appeared since it last typed anything) or busy with a command; `f` says the
///   follow key below works. The folder comes last, so any name fits.
/// - The user's prompt runs first, so prompt themes still see `$?` of the last command.
/// - Ctrl+Alt+Shift+F12 (`FOLLOW_KEY`) moves the shell to the folder in
///   `$env:CM_REQUEST_FILE` and redraws the prompt in place: no command line, no history entry.
/// - Ctrl+O at the prompt exits, only in the one-off shell used when the persistent one can't
///   start (`CM_ONE_OFF`). In the persistent shell cm keeps Ctrl+O to itself, and a Ctrl+O
///   that slips through (Caps Lock) mustn't end the session.
/// - It goes to `$env:CM_START_DIR` last: the process itself started elsewhere, so as not to
///   hold that folder (see `Subshell::start`).
const PWSH_SHELL_INIT: &str = r#"
$global:__cm_seq = 0
$global:__cm_follow = '-'
function global:__cm_save_cwd {
    $global:__cm_seq++
    $here = (Get-Location -PSProvider FileSystem).ProviderPath
    [IO.File]::WriteAllText($env:CM_CWD_FILE, "$($global:__cm_seq)|$($global:__cm_follow)|$here")
}
$global:__cm_prompt = $function:prompt
function global:prompt { $p = & $global:__cm_prompt; __cm_save_cwd; $p }
if (Get-Command Set-PSReadLineKeyHandler -ErrorAction SilentlyContinue) {
    if ($env:CM_ONE_OFF) {
        Set-PSReadLineKeyHandler -Chord Ctrl+o -BriefDescription 'Back to cm' -ScriptBlock {
            __cm_save_cwd
            [Environment]::Exit(0)
        }
    }
    Set-PSReadLineKeyHandler -Chord Ctrl+Alt+Shift+F12 -BriefDescription 'Go to the cm panel folder' -ScriptBlock {
        try {
            $target = [IO.File]::ReadAllText($env:CM_REQUEST_FILE)
            if (Test-Path -LiteralPath $target -PathType Container) { Set-Location -LiteralPath $target }
        } catch {}
        # InvokePrompt writes through [Console]::OutputEncoding, an old code page by default,
        # which would turn "Účty" into "Úcty"; UTF-8 just for the redraw.
        $encoding = [Console]::OutputEncoding
        try {
            [Console]::OutputEncoding = [Text.Encoding]::UTF8
            [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
        } finally {
            [Console]::OutputEncoding = $encoding
        }
    }
    $global:__cm_follow = 'f'
}
if ($env:CM_START_DIR) {
    Set-Location -LiteralPath $env:CM_START_DIR
    $env:CM_START_DIR = $null
}
"#;

/// Start-up file of the bash behind Ctrl+O (`bash --rcfile`): the user's ~/.bashrc, then the
/// same reports as `PWSH_SHELL_INIT`.
///
/// - `PROMPT_COMMAND` writes `<prompt number>|<f or ->|<folder>` to `$CM_CWD_FILE` (with
///   `>|`, so `set -o noclobber` doesn't stop it), keeping `$?` for whatever runs after it.
/// - The follow key (bash 4.4+) runs `__cm_cd` and then presses Enter on the empty line, as
///   bash can only redraw its prompt by drawing a new one. The next prompt first moves the
///   cursor back over the old prompt, so the new one takes its place. Empty lines don't go
///   into the history.
const BASH_SHELL_INIT: &str = r#"
[ -f ~/.bashrc ] && . ~/.bashrc

__cm_seq=0
__cm_redraw=
__cm_follow=-
__cm_prompt() {
    local ret=$?
    if [ -n "$__cm_redraw" ]; then
        __cm_redraw=
        local prompt="${PS1@P}" nl=$'\n'
        local newlines="${prompt//[!$nl]/}"
        printf '\e[%dA\r\e[J' "$(( ${#newlines} + 1 ))"
    fi
    __cm_seq=$(( __cm_seq + 1 ))
    printf '%s|%s|%s' "$__cm_seq" "$__cm_follow" "$PWD" >| "$CM_CWD_FILE"
    return $ret
}
PROMPT_COMMAND="__cm_prompt${PROMPT_COMMAND:+;$PROMPT_COMMAND}"

if (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4) )); then
    __cm_cd() {
        local target
        { target=$(< "$CM_REQUEST_FILE"); } 2>/dev/null
        [ -d "$target" ] && builtin cd -- "$target"
        __cm_redraw=1
    }
    for __cm_map in emacs vi-insert; do
        bind -m "$__cm_map" -x '"\e[24;7~": __cm_cd'
        bind -m "$__cm_map" '"\e[24;8~": "\e[24;7~\C-m"'
    done
    unset __cm_map
    __cm_follow=f
fi
"#;

/// The `.zshenv` in the ZDOTDIR cm gives its zsh: the user's own .zshenv (from their ZDOTDIR,
/// which that file may change), then back to cm's folder so zsh reads cm's .zshrc next.
const ZSH_ENV: &str = r#"
__cm_zdotdir=$ZDOTDIR
ZDOTDIR=${CM_USER_ZDOTDIR:-$HOME}
[[ -r $ZDOTDIR/.zshenv ]] && builtin source $ZDOTDIR/.zshenv
__cm_user_zdotdir=$ZDOTDIR
ZDOTDIR=$__cm_zdotdir
"#;

/// cm's `.zshrc`: the user's, then the reports of `PWSH_SHELL_INIT` from a precmd hook, and the
/// follow key as a zle widget, which can redraw the prompt in place (`zle reset-prompt`).
const ZSH_RC: &str = r#"
ZDOTDIR=$__cm_user_zdotdir
unset __cm_zdotdir __cm_user_zdotdir
[[ -r $ZDOTDIR/.zshrc ]] && builtin source $ZDOTDIR/.zshrc

typeset -gi __cm_seq=0 __cm_status=0
__cm_report() {
    local ret=$?
    __cm_status=$ret
    __cm_seq+=1
    print -rn -- "$__cm_seq|f|$PWD" >| $CM_CWD_FILE
    return $ret
}
precmd_functions+=(__cm_report)

__cm_follow() {
    local target f
    { target=$(<$CM_REQUEST_FILE) } 2>/dev/null
    [[ -d $target ]] && builtin cd -q -- $target
    # What a new prompt would run: themes update from precmd (seeing the last command's
    # status, not this widget's), and it reports the folder.
    for f in precmd $precmd_functions; do
        if (( $+functions[$f] )); then
            () { return $__cm_status }
            $f
        fi
    done
    zle reset-prompt
}
zle -N __cm_follow
for f in emacs viins vicmd; do
    bindkey -M $f '\e[24;8~' __cm_follow
done
unset f
"#;

/// What the shell's prompt reported.
#[derive(Clone, Debug, PartialEq)]
pub struct ShellReport {
    /// Counts prompts: a new one means the shell finished what it was doing.
    pub prompt: u64,
    pub dir: PathBuf,
    /// The shell has the silent "go to this folder" key (`FOLLOW_KEY`).
    pub can_follow: bool,
}

/// Reads what the shell's prompt wrote: `<number>|<f or ->|<folder>`. The folder is the rest
/// as it is: a `|` or spaces at either end belong to it.
pub fn parse_cwd_report(text: &str) -> Option<ShellReport> {
    let text = text.trim_start_matches('\u{feff}').trim_end_matches(['\r', '\n']);
    let mut parts = text.splitn(3, '|');
    let prompt = parts.next()?.trim().parse().unwrap_or(0);
    let can_follow = parts.next()? == "f";
    let dir = parts.next()?;
    (!dir.is_empty()).then(|| ShellReport { prompt, dir: PathBuf::from(dir), can_follow })
}

/// The same from the file's bytes. On Linux a folder name needn't be UTF-8, so it is taken
/// byte for byte.
pub fn parse_cwd_report_bytes(bytes: &[u8]) -> Option<ShellReport> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut parts = bytes.splitn(3, |&b| b == b'|');
        let prompt = std::str::from_utf8(parts.next()?).ok()?.trim().parse().unwrap_or(0);
        let can_follow = parts.next()? == b"f";
        let dir = parts.next()?;
        (!dir.is_empty()).then(|| ShellReport { prompt, dir: PathBuf::from(std::ffi::OsStr::from_bytes(dir)), can_follow })
    }
    #[cfg(not(unix))]
    parse_cwd_report(&String::from_utf8_lossy(bytes))
}

/// PowerShell's -EncodedCommand form of a script: base64 of UTF-16LE. It sidesteps quoting.
fn encode_ps(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    util::base64(&utf16)
}

/// How to start the persistent shell (see `Shell::session`).
pub struct Session {
    pub argv: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

/// The shells cm knows how to hook into.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Flavour {
    PowerShell,
    Cmd,
    Bash,
    Zsh,
    Other,
}

/// The shell that runs command-line input and Ctrl+O sessions.
#[derive(Clone, Debug)]
pub enum Shell {
    PowerShell(PathBuf),
    Cmd,
    Posix(PathBuf),
}

impl Shell {
    /// `CM_SHELL` if set, else pwsh, Windows PowerShell or cmd (or `$SHELL` elsewhere).
    pub fn detect() -> Self {
        if let Ok(custom) = std::env::var("CM_SHELL")
            && !custom.trim().is_empty()
        {
            return Self::from_program(custom.trim());
        }
        Self::default_shell()
    }

    /// bash, zsh, sh and the like: `\` escapes, `'…'` has no escapes inside.
    pub fn is_posix(&self) -> bool {
        matches!(self, Shell::Posix(_))
    }

    /// What the options editor shows for it.
    pub fn program(&self) -> String {
        match self {
            Shell::PowerShell(program) | Shell::Posix(program) => program.display().to_string(),
            Shell::Cmd => "cmd".into(),
        }
    }

    pub fn from_program(program: &str) -> Self {
        let stem = Path::new(program)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        match stem.as_str() {
            "cmd" => Shell::Cmd,
            "pwsh" | "powershell" => Shell::PowerShell(program.into()),
            _ => Shell::Posix(program.into()),
        }
    }

    #[cfg(windows)]
    fn default_shell() -> Self {
        util::find_in_path("pwsh.exe")
            .or_else(|| util::find_in_path("powershell.exe"))
            .map_or(Shell::Cmd, Shell::PowerShell)
    }

    #[cfg(not(windows))]
    fn default_shell() -> Self {
        Shell::Posix(std::env::var_os("SHELL").map_or_else(|| "/bin/sh".into(), PathBuf::from))
    }

    /// A command that runs one line of user input.
    pub fn command(&self, line: &str) -> Command {
        match self {
            Shell::PowerShell(program) => {
                let mut command = Command::new(program);
                command.args(["-NoLogo", "-EncodedCommand", &encode_ps(line)]);
                command
            }
            Shell::Cmd => {
                let mut command = Command::new("cmd.exe");
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    command.raw_arg(format!("/S /C \"{line}\""));
                }
                #[cfg(not(windows))]
                command.args(["/C", line]);
                command
            }
            Shell::Posix(program) => {
                let mut command = Command::new(program);
                command.arg("-c").arg(line);
                command
            }
        }
    }

    /// Program and arguments of an interactive session. PowerShell gets `PWSH_SHELL_INIT`.
    pub fn interactive_argv(&self) -> Vec<OsString> {
        match self {
            Shell::PowerShell(program) => vec![
                program.into(),
                "-NoLogo".into(),
                "-NoExit".into(),
                "-EncodedCommand".into(),
                encode_ps(PWSH_SHELL_INIT).into(),
            ],
            Shell::Cmd => vec!["cmd.exe".into()],
            Shell::Posix(program) => vec![program.into()],
        }
    }

    /// A one-off interactive session in cm's own console (the fallback for Ctrl+O). A
    /// PowerShell one reports its folder in `cwd_file` and goes back to cm on Ctrl+O.
    pub fn interactive(&self, cwd_file: &Path) -> Command {
        let argv = self.interactive_argv();
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]).env("CM_CWD_FILE", cwd_file).env("CM_ONE_OFF", "1");
        command
    }

    /// Whether Ctrl+O in the interactive shell returns to cm (otherwise it takes `exit`).
    pub fn ctrl_o_returns(&self) -> bool {
        matches!(self, Shell::PowerShell(_))
    }

    fn flavour(&self) -> Flavour {
        match self {
            Shell::PowerShell(_) => Flavour::PowerShell,
            Shell::Cmd => Flavour::Cmd,
            Shell::Posix(program) => match program.file_stem().map(|s| s.to_string_lossy().to_lowercase()).as_deref() {
                Some("bash") => Flavour::Bash,
                Some("zsh") => Flavour::Zsh,
                _ => Flavour::Other,
            },
        }
    }

    /// Whether its prompt reports to cm (its number and folder), so cm can tell when it is
    /// idle and where it is. cmd, sh, fish and others don't.
    pub fn reports_prompts(&self) -> bool {
        matches!(self.flavour(), Flavour::PowerShell | Flavour::Bash | Flavour::Zsh)
    }

    /// Program, arguments and extra environment of the persistent shell. bash and zsh get
    /// cm's start-up files (which load the user's own first), written into `dir`.
    pub fn session(&self, dir: &Path) -> std::io::Result<Session> {
        match (self, self.flavour()) {
            (Shell::Posix(program), Flavour::Bash) => {
                std::fs::create_dir_all(dir)?;
                let rc = dir.join("bashrc");
                std::fs::write(&rc, BASH_SHELL_INIT)?;
                let argv = vec![program.into(), "--rcfile".into(), rc.into(), "-i".into()];
                Ok(Session { argv, env: Vec::new() })
            }
            (Shell::Posix(program), Flavour::Zsh) => {
                std::fs::create_dir_all(dir)?;
                std::fs::write(dir.join(".zshenv"), ZSH_ENV)?;
                std::fs::write(dir.join(".zshrc"), ZSH_RC)?;
                let mut env = vec![("ZDOTDIR".into(), dir.into())];
                if let Some(user) = std::env::var_os("ZDOTDIR") {
                    env.push(("CM_USER_ZDOTDIR".into(), user));
                }
                Ok(Session { argv: vec![program.into(), "-i".into()], env })
            }
            _ => Ok(Session { argv: self.interactive_argv(), env: Vec::new() }),
        }
    }

    /// A command line that moves the shell to `dir` where everyone can see it, for when the
    /// silent follow key isn't there.
    pub fn cd_line(&self, dir: &Path) -> Option<String> {
        let quoted = self.quote(&dir.display().to_string());
        match self.flavour() {
            Flavour::PowerShell => Some(format!("Set-Location -LiteralPath {quoted}\r")),
            // The leading space keeps it out of the history (ignorespace, HIST_IGNORE_SPACE).
            Flavour::Bash | Flavour::Zsh => Some(format!(" cd -- {quoted}\r")),
            Flavour::Cmd | Flavour::Other => None,
        }
    }

    pub fn quote(&self, s: &str) -> String {
        match self {
            // PowerShell takes ‘ ’ ‚ ‛ as single quotes as well; each is doubled like '.
            Shell::PowerShell(_) => {
                let mut quoted = String::from("'");
                for c in s.chars() {
                    if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
                        quoted.push(c);
                    }
                    quoted.push(c);
                }
                quoted.push('\'');
                quoted
            }
            // cmd expands %NAME% inside quotes too. After each %, the quotes close for a ^
            // before the next character (`"a%"^b"c%"`): cmd then finds no variable "^b…",
            // drops the ^, and the name arrives as it is. What separates arguments has to
            // stay inside the quotes, so it is the character after that (`"50% "^o"ff.txt"`).
            Shell::Cmd => {
                let mut quoted = String::from("\"");
                let mut chars = s.chars().peekable();
                while let Some(c) = chars.next() {
                    quoted.push(c);
                    if c == '%' {
                        while let Some(separator) = chars.next_if(|&n| n.is_whitespace() || ",;=".contains(n)) {
                            quoted.push(separator);
                        }
                        if let Some(next) = chars.next_if(|&n| n != '"') {
                            quoted.push_str(&format!("\"^{next}\""));
                        }
                    }
                }
                quoted.push('"');
                quoted
            }
            Shell::Posix(_) => format!("'{}'", s.replace('\'', "'\\''")),
        }
    }

    /// A command line that runs the program or script at `path`.
    pub fn run_file_line(&self, path: &Path) -> String {
        let quoted = self.quote(&path.display().to_string());
        match self {
            Shell::PowerShell(_) => format!("& {quoted}"),
            _ => quoted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// cmd expands %NAME% inside quotes too: a quoted name with % in it must reach the
    /// program as it is, never as a variable's value.
    #[cfg(windows)]
    #[test]
    fn cmd_quoting_keeps_percent_signs() {
        let tmp = TempDir::new("cmd-percent");
        let names = [
            "pct%USERNAME%x.txt", "a%%b.txt", "end%.txt", "%PATH%", "x%^y & z.txt", "%%%", "50% off.txt", "a%,b.txt", "a%;b.txt",
            "a%=b.txt", "100% of %PATH%.txt", "% %USERNAME% %",
        ];
        for name in names {
            let path = tmp.file(name, name);
            let line = format!("type {}", Shell::Cmd.quote(&path.display().to_string()));
            let out = Shell::Cmd.command(&line).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), name, "{line}: {}", String::from_utf8_lossy(&out.stderr));
        }
    }

    /// A cm started where another one is drawn is told; one in a new window isn't.
    #[test]
    fn a_cm_in_the_terminal_of_another_is_told() {
        let parents = |pairs: &'static [(u32, u32)]| move |pid: u32| pairs.iter().find(|(p, _)| *p == pid).map(|(_, q)| *q);
        // A command or an editor cm (7) runs: cm lends it its own terminal.
        assert!(shares_terminal(7, &[7, 30], parents(&[(30, 7)])));
        // In its shell (20, which cm started) and so in the terminal cm made for it.
        assert!(shares_terminal(7, &[20, 30], parents(&[(20, 7), (30, 20)])));
        // A new window opened from that shell: its processes are the shell's, none cm's.
        assert!(!shares_terminal(7, &[40, 50], parents(&[(40, 20), (50, 40)])));
    }

    /// Unix, for real: a terminal is a session. The process that started these tests is in
    /// theirs; one this process started in a session of its own isn't.
    #[cfg(unix)]
    #[test]
    fn a_session_is_a_terminal() {
        use std::os::unix::process::CommandExt;
        assert!(in_terminal_of(unsafe { libc::getppid() } as u32));
        let mut command = Command::new("sleep");
        command.arg("5");
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        assert!(!in_terminal_of(child.id()));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn ctrl_o_shell_reports_its_folder_at_each_prompt() {
        let Shell::PowerShell(program) = Shell::detect() else {
            eprintln!("no PowerShell here; skipped");
            return;
        };
        let tmp = TempDir::new("shell-init");
        let target = tmp.dir("Účty a faktury");
        let file = tmp.path().join("cwd.txt");
        // What an interactive session does: the start-up script, prompts, a `cd`, then exit.
        let script = format!(
            "{PWSH_SHELL_INIT}\n$null = prompt\nSet-Location -LiteralPath '{}'\n$null = prompt\nexit",
            target.display()
        );
        let status = Command::new(program)
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand", &encode_ps(&script)])
            .env("CM_CWD_FILE", &file)
            .status()
            .unwrap();
        assert!(status.success());
        let report = parse_cwd_report(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(report.prompt, 2, "one report per prompt");
        assert!(util::same_path(&report.dir, &target), "{}", report.dir.display());
    }

    /// Only opens handles; nothing is deleted. CM_PROBE=PATH also prints the reason for PATH.
    #[test]
    fn says_why_something_cannot_be_deleted() {
        let tmp = TempDir::new("denied");
        let own = tmp.file("mine.txt", "x");
        assert!(delete_denied(&own).is_none(), "your own file can go");
        // Windows' own files belong to TrustedInstaller: without elevation they can't go.
        #[cfg(windows)]
        if let Some(denied) = delete_denied(Path::new(r"C:\Windows\System32\drivers")) {
            assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(denied.to_string().contains("TrustedInstaller"), "{denied}");
        }
        if let Some(path) = std::env::var_os("CM_PROBE") {
            println!("{}: {:?}", Path::new(&path).display(), delete_denied(Path::new(&path)));
        }
    }

    #[test]
    fn parsing_reports() {
        let report = |prompt, dir: &str, can_follow| Some(ShellReport { prompt, dir: PathBuf::from(dir), can_follow });
        assert_eq!(parse_cwd_report("7|-|E:\\devel\r\n"), report(7, r"E:\devel", false));
        assert_eq!(parse_cwd_report("\u{feff}1|f|C:\\"), report(1, r"C:\", true));
        assert_eq!(parse_cwd_report("no separator"), None);
        // On Linux a folder may hold a `|` or end in spaces: it is all the folder.
        assert_eq!(parse_cwd_report("3|f|/tmp/a|b "), report(3, "/tmp/a|b ", true));
        assert_eq!(parse_cwd_report_bytes(b"3|f|/tmp/a|b"), report(3, "/tmp/a|b", true));
    }

    /// PowerShell takes ‘ ’ ‚ ‛ as single quotes too: a name with them can't end the quoting.
    #[test]
    fn powershell_quoting_covers_its_other_quotes() {
        let shell = Shell::PowerShell(PathBuf::from("pwsh"));
        assert_eq!(shell.quote("Rock’n’Roll"), "'Rock’’n’’Roll'");
        assert_eq!(shell.quote("it's"), "'it''s'");
        assert_eq!(shell.quote("x‘;Write-Output INJECTED;’ .txt"), "'x‘‘;Write-Output INJECTED;’’ .txt'");
    }
}
