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

/// Whether a name in `path` ends in a dot or a space, or is a device name (`nul`, `con.txt`,
/// `COM1`), which Windows' usual paths can't reach. Never on other systems.
#[cfg(windows)]
pub fn needs_literal(path: &Path) -> bool {
    path.components().any(|c| match c {
        std::path::Component::Normal(name) => {
            let name = name.to_string_lossy();
            name.ends_with(['.', ' ']) || is_device_name(&name)
        }
        _ => false,
    })
}

#[cfg(not(windows))]
pub fn needs_literal(_path: &Path) -> bool {
    false
}

/// Why another program can't be given `path` (one that `needs_literal`): editors, viewers and
/// Windows' "open" read it the usual way, as another file.
pub fn unopenable(path: &Path) -> String {
    format!(
        "\"{}\" has a name other programs can't open on Windows: it ends in a dot or a space, \
         or is a device name (NUL, CON, COM1…), so they would read another file instead.\n\
         Rename it first (Shift+F6).",
        path.display()
    )
}

/// CON, PRN, AUX, NUL, COM0-9, LPT0-9 (also with ¹²³), CONIN$ and CONOUT$, in any case,
/// alone or before an extension: Windows' reserved device names.
#[cfg(windows)]
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ').to_ascii_uppercase();
    let chars: Vec<char> = stem.chars().collect();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (chars.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(chars[3], '0'..='9' | '¹' | '²' | '³'))
}

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

#[cfg(unix)]
pub fn read_link(path: &Path) -> io::Result<PathBuf> {
    fs::read_link(literal(path))
}

pub fn open(path: &Path) -> io::Result<File> {
    File::open(literal(path))
}

pub fn create_new(path: &Path) -> io::Result<File> {
    File::create_new(literal(path))
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
        for odd in [r"C:\a\foo.", r"C:\a\bar ", r"C:\a.\b", r"C:\a\nul", r"C:\a\con.txt", r"C:\a\COM1", r"C:\a\lpt²"] {
            assert!(needs_literal(Path::new(odd)), "{odd}");
        }
        for plain in [r"C:\a\foo.txt", r"C:\a\.git", r"C:\a\console", r"C:\a\com10", r"C:\a\nullable", r"C:\a\..\b"] {
            assert!(!needs_literal(Path::new(plain)), "{plain}");
            assert_eq!(literal(Path::new(plain)), Path::new(plain));
        }
        assert_eq!(literal(Path::new(r"C:\a/b\.\x\..\foo.")), Path::new(r"\\?\C:\a\b\foo."));
        assert_eq!(literal(Path::new(r"\\server\share\dir\nul")), Path::new(r"\\?\UNC\server\share\dir\nul"));
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
