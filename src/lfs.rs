//! The file system calls of copying, moving, deleting and listing, with each path handed to
//! Windows literally when it has to be.
//!
//! Windows' usual paths drop a name's trailing dots and spaces and take device names as the
//! devices: `foo.` (which WSL, Linux, a Mac on the network or an archive can make) is read
//! as `foo`, and a file called `nul` or `con` as that device. Deleting `foo.` deleted `foo`.
//! Such paths go as `\\?\C:\…` (`\\?\UNC\server\share\…`), which Windows takes as written;
//! all other paths go exactly as they are. Elsewhere these are std's own calls.

use std::borrow::Cow;
use std::fs::{self, File, Metadata, Permissions, ReadDir};
use std::io;
use std::path::{Path, PathBuf};

/// `path` as Windows has to be given it: literally (`\\?\…`) when a name in it ends in a dot
/// or a space or is a device name, or when it is too long for the usual form.
pub fn literal(path: &Path) -> Cow<'_, Path> {
    #[cfg(windows)]
    if (needs_literal(path) || is_long(path))
        && let Some(literal) = to_literal(path)
    {
        return Cow::Owned(literal);
    }
    Cow::Borrowed(path)
}

/// Too long for Windows' usual paths (260 UTF-16 units; a bit less for folders), counted as
/// Windows counts.
#[cfg(windows)]
fn is_long(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().count() >= 240
}

/// Whether Windows reads `path` as something else than what it says: a name in it ends in a
/// dot or a space, or it is a device (`nul`, and on older Windows `con.txt` too, which on
/// Windows 11 is a file). Windows is asked, so that goes for whichever version this is. Never
/// on other systems.
#[cfg(windows)]
pub fn needs_literal(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;
    if !path.is_absolute() {
        return false;
    }
    let normal = crate::util::normalize(path);
    let wide: Vec<u16> = normal.as_os_str().encode_wide().chain([0]).collect();
    let mut read = vec![0u16; wide.len() + 16];
    let n = unsafe { GetFullPathNameW(wide.as_ptr(), read.len() as u32, read.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    // Only a path that comes back as it went in is read as itself.
    n == 0 || n >= read.len() || read[..n] != wide[..wide.len() - 1]
}

#[cfg(not(windows))]
pub fn needs_literal(_path: &Path) -> bool {
    false
}

/// Why another program can't be given a path that `needs_literal`: editors, viewers and
/// Windows' "open" read it the usual way, as another file.
pub const UNOPENABLE: &str = "Other programs can't open this name: it ends in a dot or a space,\n\
     or is a device name (NUL, CON, COM1…), so they would read another\n\
     file instead. Rename it first (Shift+F6).";

/// The literal form of an absolute path (`\\?\C:\a\b`, `\\?\UNC\server\share\a`), with `.`
/// and `..` resolved and `/` turned into `\`, which the literal form doesn't do. None for a
/// relative path or a device path.
#[cfg(windows)]
pub fn to_literal(path: &Path) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::path::{Component, Prefix};
    let normal = crate::util::normalize(path);
    let mut components = normal.components();
    let Some(Component::Prefix(prefix)) = components.next() else { return None };
    let mut out = match prefix.kind() {
        Prefix::Disk(letter) => OsString::from(format!(r"\\?\{}:", letter as char)),
        Prefix::UNC(server, share) => {
            let mut out = OsString::from(r"\\?\UNC\");
            out.push(server);
            out.push(r"\");
            out.push(share);
            out
        }
        Prefix::Verbatim(_) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(..) => return Some(normal),
        Prefix::DeviceNS(_) => return None,
    };
    let mut names = 0;
    for component in components {
        if let Component::Normal(name) = component {
            out.push(r"\");
            out.push(name);
            names += 1;
        }
    }
    // `\\?\C:` alone is the volume, not its root folder.
    if names == 0 {
        out.push(r"\");
    }
    Some(PathBuf::from(out))
}

pub fn metadata(path: &Path) -> io::Result<Metadata> {
    fs::metadata(literal(path))
}

pub fn symlink_metadata(path: &Path) -> io::Result<Metadata> {
    fs::symlink_metadata(literal(path))
}

/// Whether `path` is there (following links, like `Path::exists`).
pub fn exists(path: &Path) -> bool {
    metadata(path).is_ok()
}

/// Whether `path` is a folder (following links, like `Path::is_dir`).
pub fn is_dir(path: &Path) -> bool {
    metadata(path).is_ok_and(|m| m.is_dir())
}

/// The folder's entries. Their `path()` is the literal one when the folder's is: join the
/// names to the folder's own path instead where the path is shown.
pub fn read_dir(path: &Path) -> io::Result<ReadDir> {
    fs::read_dir(literal(path))
}

pub fn create_dir_all(path: &Path) -> io::Result<()> {
    fs::create_dir_all(literal(path))
}

pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(literal(from), literal(to))
}

pub fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(literal(path))
}

pub fn remove_dir(path: &Path) -> io::Result<()> {
    fs::remove_dir(literal(path))
}

pub fn set_permissions(path: &Path, permissions: Permissions) -> io::Result<()> {
    fs::set_permissions(literal(path), permissions)
}

pub fn read_link(path: &Path) -> io::Result<PathBuf> {
    fs::read_link(literal(path))
}

pub fn open(path: &Path) -> io::Result<File> {
    File::open(literal(path))
}

pub fn create_new(path: &Path) -> io::Result<File> {
    File::create_new(literal(path))
}

/// A new file for a copy of one with `meta`: on Linux no more open to others than that one,
/// from the first byte on (a private key mustn't be readable while it is copied).
pub fn create_new_like(path: &Path, meta: &Metadata) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        File::options().write(true).create_new(true).mode(meta.permissions().mode() & 0o777).open(literal(path))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        create_new(path)
    }
}

/// A new file with these permissions, where the system has them; from another computer there
/// may be no `Metadata` to go by (see `create_new_like`).
pub fn create_new_with_mode(path: &Path, mode: Option<u32>) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        File::options().write(true).create_new(true).mode(mode.unwrap_or(0o666)).open(literal(path))
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        create_new(path)
    }
}

/// Closes a file that was written, saying if that failed: a network drive may only now report
/// that it is full or gone, which dropping a `File` ignores.
pub fn close(file: File) -> io::Result<()> {
    #[cfg(unix)]
    let closed = {
        use std::os::fd::IntoRawFd;
        unsafe { libc::close(file.into_raw_fd()) == 0 }
    };
    #[cfg(windows)]
    let closed = {
        use std::os::windows::io::IntoRawHandle;
        unsafe { windows_sys::Win32::Foundation::CloseHandle(file.into_raw_handle() as _) != 0 }
    };
    if closed { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Gives a finished copy (a file or a folder) what the original had besides its contents:
/// Windows' read-only, hidden and system flags; on Linux the mode, and as root the owner. Worth
/// trying, not failing over: FAT and network shares don't keep all of it.
pub fn keep_looks(path: &Path, meta: &Metadata) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_ATTRIBUTE_READONLY,
            FILE_ATTRIBUTE_SYSTEM, SetFileAttributesW,
        };
        let kept = FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
        let name: Vec<u16> = literal(path).as_os_str().encode_wide().chain([0]).collect();
        unsafe { SetFileAttributesW(name.as_ptr(), meta.file_attributes() & kept | FILE_ATTRIBUTE_ARCHIVE) };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Before the mode: changing the owner clears set-uid.
        if unsafe { libc::geteuid() } == 0 {
            let _ = std::os::unix::fs::lchown(literal(path), Some(meta.uid()), Some(meta.gid()));
        }
        if !meta.file_type().is_symlink() {
            let _ = set_permissions(path, meta.permissions());
        }
    }
}

/// A folder's modified time, which writing files into it has changed.
pub fn set_folder_time(path: &Path, time: std::time::SystemTime) -> io::Result<()> {
    #[cfg(windows)]
    let folder = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES};
        File::options().access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(literal(path))?
    };
    #[cfg(not(windows))]
    let folder = File::open(literal(path))?;
    folder.set_modified(time)
}

pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(literal(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn which_paths_go_literally() {
        for odd in [r"C:\a\foo.", r"C:\a\bar ", r"C:\a.\b", r"C:\a\nul", r"\\server\share\a."] {
            assert!(needs_literal(Path::new(odd)), "{odd}");
        }
        for plain in [r"C:\a\foo.txt", r"C:\a\.git", r"C:\a\console", r"C:\a\com10", r"C:\a\nullable", r"C:\a\..\b", "C:\\", r"\\server\share\a"] {
            assert!(!needs_literal(Path::new(plain)), "{plain}");
        }
        assert_eq!(literal(Path::new(r"C:\a\foo.txt")), Path::new(r"C:\a\foo.txt"));
        assert_eq!(literal(Path::new(r"C:\a/b\.\x\..\foo.")), Path::new(r"\\?\C:\a\b\foo."));
        assert_eq!(literal(Path::new(r"\\server\share\dir\foo.")), Path::new(r"\\?\UNC\server\share\dir\foo."));
        assert_eq!(literal(Path::new(r"\\?\C:\x\foo.")), Path::new(r"\\?\C:\x\foo."));
        assert_eq!(to_literal(Path::new(r"C:\")).unwrap(), Path::new(r"\\?\C:\"));
        assert_eq!(literal(Path::new(r"relative\foo.")), Path::new(r"relative\foo."));
        let long = format!(r"C:\{}\file.txt", "d".repeat(250));
        assert!(literal(Path::new(&long)).starts_with(r"\\?\C:\"));
    }

    #[cfg(not(windows))]
    #[test]
    fn paths_go_as_they_are() {
        for path in ["/a/foo.", "/a/nul", "relative/x "] {
            assert!(!needs_literal(Path::new(path)));
            assert_eq!(literal(Path::new(path)), Path::new(path));
        }
    }
}
