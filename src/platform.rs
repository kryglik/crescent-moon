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

#[cfg(windows)]
pub use win::{delete_denied, disk_space, drives, init, open_default};

/// The mount point holding `path`; on Windows the drive is in the path itself (`util::volume_key`).
#[cfg(windows)]
pub fn mount_point(_path: &Path) -> Option<PathBuf> {
    None
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

    fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
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

    /// Why `path` can't be deleted, if Windows says so: in use by another program, or no
    /// permission (neither DELETE on it nor "delete child" on its folder), naming the owner.
    /// None when nothing stands in the way, or it can't tell.
    pub fn delete_denied(path: &Path) -> Option<String> {
        use windows_sys::Win32::Foundation::{
            CloseHandle, ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, GetLastError, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, DELETE, FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        let open = |path: &Path, access: u32| {
            let name = wide(path);
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
            Some(owner) => format!("Access is denied: you may not delete it (it belongs to {owner})."),
            None => "Access is denied: you may not delete it.".into(),
        })
    }

    /// The owner of a file or folder, as `DOMAIN\name`.
    fn owner(path: &Path) -> Option<String> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{LookupAccountSidW, OWNER_SECURITY_INFORMATION};
        let name = wide(path);
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
}

#[cfg(not(windows))]
pub use unix::{delete_denied, disk_space, drives, init, mount_point, open_default, same_device};

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
    pub fn delete_denied(path: &Path) -> Option<String> {
        let parent = path.parent()?;
        let parent = CString::new(parent.as_os_str().as_bytes()).ok()?;
        let writable = unsafe { libc::access(parent.as_ptr(), libc::W_OK | libc::X_OK) } == 0;
        (!writable).then(|| "Access is denied: you may not change the folder it is in.".to_string())
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
        mounts()
            .into_iter()
            .map(|m| m.dir)
            .filter(|dir| util::is_inside(path, dir))
            .max_by_key(|dir| dir.components().count())
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
    ///   terminal program, which then fights rmc for the screen);
    /// - other Linux: `xdg-open`, only in a graphical session, for the same reason.
    ///
    /// Otherwise `NoOpener`, and rmc shows the file in the viewer instead.
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
        // Its chatter would land on rmc's screen; and wait for it so it doesn't linger as a zombie.
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

/// The key rmc presses to make the shell go to the folder in `$RMC_REQUEST_FILE`:
/// Ctrl+Alt+Shift+F12, as a terminal sends it. The start-up scripts below bind it.
pub const FOLLOW_KEY: &str = "\x1b[24;8~";

/// Start-up script of the PowerShell behind Ctrl+O.
///
/// - Every prompt writes `<prompt number>|<current folder>[|f]` to `$env:RMC_CWD_FILE`. The
///   folder lets rmc's panel follow `cd`; the number tells rmc whether the shell is idle (a
///   prompt has appeared since it last typed anything) or busy with a command; `f` says the
///   follow key below works.
/// - Ctrl+Alt+Shift+F12 (`FOLLOW_KEY`) moves the shell to the folder in
///   `$env:RMC_REQUEST_FILE` and redraws the prompt in place: no command line, no history entry.
/// - Ctrl+O at the prompt exits, for the one-off shell used when the persistent one can't start.
///   (In the persistent shell rmc keeps Ctrl+O to itself.)
const PWSH_SHELL_INIT: &str = r#"
$global:__rmc_seq = 0
$global:__rmc_follow = ''
function global:__rmc_save_cwd {
    $global:__rmc_seq++
    $here = (Get-Location -PSProvider FileSystem).ProviderPath
    [IO.File]::WriteAllText($env:RMC_CWD_FILE, "$($global:__rmc_seq)|$here$($global:__rmc_follow)")
}
$global:__rmc_prompt = $function:prompt
function global:prompt { __rmc_save_cwd; & $global:__rmc_prompt }
if (Get-Command Set-PSReadLineKeyHandler -ErrorAction SilentlyContinue) {
    Set-PSReadLineKeyHandler -Chord Ctrl+o -BriefDescription 'Back to rmc' -ScriptBlock {
        __rmc_save_cwd
        [Environment]::Exit(0)
    }
    Set-PSReadLineKeyHandler -Chord Ctrl+Alt+Shift+F12 -BriefDescription 'Go to the rmc panel folder' -ScriptBlock {
        try {
            $target = [IO.File]::ReadAllText($env:RMC_REQUEST_FILE)
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
    $global:__rmc_follow = '|f'
}
"#;

/// Start-up file of the bash behind Ctrl+O (`bash --rcfile`): the user's ~/.bashrc, then the
/// same reports as `PWSH_SHELL_INIT`.
///
/// - `PROMPT_COMMAND` writes `<prompt number>|<folder>[|f]` to `$RMC_CWD_FILE`, keeping `$?`
///   for whatever runs after it.
/// - The follow key (bash 4.4+) runs `__rmc_cd` and then presses Enter on the empty line, as
///   bash can only redraw its prompt by drawing a new one. The next prompt first moves the
///   cursor back over the old prompt, so the new one takes its place. Empty lines don't go
///   into the history.
const BASH_SHELL_INIT: &str = r#"
[ -f ~/.bashrc ] && . ~/.bashrc

__rmc_seq=0
__rmc_redraw=
__rmc_follow=
__rmc_prompt() {
    local ret=$?
    if [ -n "$__rmc_redraw" ]; then
        __rmc_redraw=
        local prompt="${PS1@P}" nl=$'\n'
        local newlines="${prompt//[!$nl]/}"
        printf '\e[%dA\r\e[J' "$(( ${#newlines} + 1 ))"
    fi
    __rmc_seq=$(( __rmc_seq + 1 ))
    printf '%s|%s%s' "$__rmc_seq" "$PWD" "$__rmc_follow" > "$RMC_CWD_FILE"
    return $ret
}
PROMPT_COMMAND="__rmc_prompt${PROMPT_COMMAND:+;$PROMPT_COMMAND}"

if (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4) )); then
    __rmc_cd() {
        local target
        { target=$(< "$RMC_REQUEST_FILE"); } 2>/dev/null
        [ -d "$target" ] && builtin cd -- "$target"
        __rmc_redraw=1
    }
    for __rmc_map in emacs vi-insert; do
        bind -m "$__rmc_map" -x '"\e[24;7~": __rmc_cd'
        bind -m "$__rmc_map" '"\e[24;8~": "\e[24;7~\C-m"'
    done
    unset __rmc_map
    __rmc_follow='|f'
fi
"#;

/// The `.zshenv` in the ZDOTDIR rmc gives its zsh: the user's own .zshenv (from their ZDOTDIR,
/// which that file may change), then back to rmc's folder so zsh reads rmc's .zshrc next.
const ZSH_ENV: &str = r#"
__rmc_zdotdir=$ZDOTDIR
ZDOTDIR=${RMC_USER_ZDOTDIR:-$HOME}
[[ -r $ZDOTDIR/.zshenv ]] && builtin source $ZDOTDIR/.zshenv
__rmc_user_zdotdir=$ZDOTDIR
ZDOTDIR=$__rmc_zdotdir
"#;

/// rmc's `.zshrc`: the user's, then the reports of `PWSH_SHELL_INIT` from a precmd hook, and the
/// follow key as a zle widget, which can redraw the prompt in place (`zle reset-prompt`).
const ZSH_RC: &str = r#"
ZDOTDIR=$__rmc_user_zdotdir
unset __rmc_zdotdir __rmc_user_zdotdir
[[ -r $ZDOTDIR/.zshrc ]] && builtin source $ZDOTDIR/.zshrc

typeset -gi __rmc_seq=0
__rmc_report() {
    local ret=$?
    __rmc_seq+=1
    print -rn -- "$__rmc_seq|$PWD|f" >| $RMC_CWD_FILE
    return $ret
}
precmd_functions+=(__rmc_report)

__rmc_follow() {
    local target f
    { target=$(<$RMC_REQUEST_FILE) } 2>/dev/null
    [[ -d $target ]] && builtin cd -q -- $target
    # What a new prompt would run: themes update from precmd, and it reports the folder.
    for f in precmd $precmd_functions; do
        (( $+functions[$f] )) && $f
    done
    zle reset-prompt
}
zle -N __rmc_follow
for f in emacs viins vicmd; do
    bindkey -M $f '\e[24;8~' __rmc_follow
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

/// Reads what the shell's prompt wrote: `<number>|<folder>[|f]`.
pub fn parse_cwd_report(text: &str) -> Option<ShellReport> {
    let mut parts = text.trim_start_matches('\u{feff}').trim().split('|');
    let prompt = parts.next()?.trim().parse().unwrap_or(0);
    let dir = parts.next()?.trim();
    let can_follow = parts.next() == Some("f");
    (!dir.is_empty()).then(|| ShellReport { prompt, dir: PathBuf::from(dir), can_follow })
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

/// The shells rmc knows how to hook into.
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
    /// `RMC_SHELL` if set, else pwsh, Windows PowerShell or cmd (or `$SHELL` elsewhere).
    pub fn detect() -> Self {
        if let Ok(custom) = std::env::var("RMC_SHELL")
            && !custom.trim().is_empty()
        {
            return Self::from_program(custom.trim());
        }
        Self::default_shell()
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

    /// A one-off interactive session in rmc's own console (the fallback for Ctrl+O). A
    /// PowerShell one reports its folder in `cwd_file` and goes back to rmc on Ctrl+O.
    pub fn interactive(&self, cwd_file: &Path) -> Command {
        let argv = self.interactive_argv();
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]).env("RMC_CWD_FILE", cwd_file);
        command
    }

    /// Whether Ctrl+O in the interactive shell returns to rmc (otherwise it takes `exit`).
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

    /// Whether its prompt reports to rmc (its number and folder), so rmc can tell when it is
    /// idle and where it is. cmd, sh, fish and others don't.
    pub fn reports_prompts(&self) -> bool {
        matches!(self.flavour(), Flavour::PowerShell | Flavour::Bash | Flavour::Zsh)
    }

    /// Program, arguments and extra environment of the persistent shell. bash and zsh get
    /// rmc's start-up files (which load the user's own first), written into `dir`.
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
                    env.push(("RMC_USER_ZDOTDIR".into(), user));
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
            Shell::PowerShell(_) => format!("'{}'", s.replace('\'', "''")),
            Shell::Cmd => format!("\"{s}\""),
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
            .env("RMC_CWD_FILE", &file)
            .status()
            .unwrap();
        assert!(status.success());
        let report = parse_cwd_report(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(report.prompt, 2, "one report per prompt");
        assert!(util::same_path(&report.dir, &target), "{}", report.dir.display());
    }

    /// Only opens handles; nothing is deleted. RMC_PROBE=PATH also prints the reason for PATH.
    #[test]
    fn says_why_something_cannot_be_deleted() {
        let tmp = TempDir::new("denied");
        let own = tmp.file("mine.txt", "x");
        assert_eq!(delete_denied(&own), None, "your own file can go");
        // Windows' own files belong to TrustedInstaller: without elevation they can't go.
        #[cfg(windows)]
        if let Some(reason) = delete_denied(Path::new(r"C:\Windows\System32\drivers")) {
            assert!(reason.contains("TrustedInstaller"), "{reason}");
        }
        if let Some(path) = std::env::var_os("RMC_PROBE") {
            println!("{}: {:?}", Path::new(&path).display(), delete_denied(Path::new(&path)));
        }
    }

    #[test]
    fn parsing_reports() {
        let report = |prompt, dir: &str, can_follow| Some(ShellReport { prompt, dir: PathBuf::from(dir), can_follow });
        assert_eq!(parse_cwd_report("7|E:\\devel\r\n"), report(7, r"E:\devel", false));
        assert_eq!(parse_cwd_report("\u{feff}1|C:\\|f"), report(1, r"C:\", true));
        assert_eq!(parse_cwd_report("no separator"), None);
    }
}
