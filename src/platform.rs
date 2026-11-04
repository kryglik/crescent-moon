//! Operating-system specific parts: drives, free space, hidden files, opening files and shells.

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
pub use win::{disk_space, drives, init, open_default};

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

    /// Opens a file in its associated Windows application.
    pub fn open_default(path: &Path) -> Result<(), String> {
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
            31 => Err(format!("No app is associated with \"{}\".", path.display())),
            r => Err(format!("Windows could not open \"{}\" (error {r}).", path.display())),
        }
    }
}

#[cfg(not(windows))]
pub fn init() {}

#[cfg(not(windows))]
pub fn disk_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(not(windows))]
pub fn drives() -> Vec<DriveInfo> {
    vec![DriveInfo { root: PathBuf::from("/"), label: String::new(), kind: "root", space: None, pending: false }]
}

#[cfg(not(windows))]
pub fn open_default(path: &Path) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    Command::new(opener)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Cannot run {opener}: {e}"))
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

/// Start-up script of the PowerShell behind Ctrl+O. Every prompt notes the current folder in
/// `$env:RMC_CWD_FILE` (so rmc's panel can follow `cd`, however the shell ends), and Ctrl+O at
/// the prompt ends the shell at once, back to the panels.
const PWSH_SHELL_INIT: &str = r#"
function global:__rmc_save_cwd {
    [IO.File]::WriteAllText($env:RMC_CWD_FILE, (Get-Location -PSProvider FileSystem).ProviderPath)
}
$global:__rmc_prompt = $function:prompt
function global:prompt { __rmc_save_cwd; & $global:__rmc_prompt }
if (Get-Command Set-PSReadLineKeyHandler -ErrorAction SilentlyContinue) {
    Set-PSReadLineKeyHandler -Chord Ctrl+o -BriefDescription 'Back to rmc' -ScriptBlock {
        __rmc_save_cwd
        [Environment]::Exit(0)
    }
}
"#;

/// PowerShell's -EncodedCommand form of a script: base64 of UTF-16LE. It sidesteps quoting.
fn encode_ps(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    util::base64(&utf16)
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

    fn from_program(program: &str) -> Self {
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

    /// An interactive shell session (Ctrl+O). A PowerShell one reports its folder in
    /// `cwd_file` and goes back to rmc on Ctrl+O; others are left with `exit`.
    pub fn interactive(&self, cwd_file: &Path) -> Command {
        let mut command = match self {
            Shell::PowerShell(program) => {
                let mut command = Command::new(program);
                command.args(["-NoLogo", "-NoExit", "-EncodedCommand", &encode_ps(PWSH_SHELL_INIT)]);
                command
            }
            Shell::Cmd => Command::new("cmd.exe"),
            Shell::Posix(program) => Command::new(program),
        };
        command.env("RMC_CWD_FILE", cwd_file);
        command
    }

    /// Whether Ctrl+O in the interactive shell returns to rmc (otherwise it takes `exit`).
    pub fn ctrl_o_returns(&self) -> bool {
        matches!(self, Shell::PowerShell(_))
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
        // What an interactive session does: the start-up script, a `cd`, a prompt, then exit.
        let script = format!("{PWSH_SHELL_INIT}\nSet-Location -LiteralPath '{}'\n$null = prompt\nexit", target.display());
        let status = Command::new(program)
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand", &encode_ps(&script)])
            .env("RMC_CWD_FILE", &file)
            .status()
            .unwrap();
        assert!(status.success());
        let reported = std::fs::read_to_string(&file).unwrap();
        assert!(util::same_path(Path::new(&reported), &target), "{reported}");
    }
}
