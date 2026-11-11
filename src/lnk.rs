//! Windows shortcuts (.lnk files): Enter on a shortcut to a folder opens the folder in the
//! panel, as Total Commander does, instead of an Explorer window.
//!
//! The target is read from the file itself ([MS-SHLLINK]): the LinkInfo structure (a local
//! path or a network share plus a suffix), else the environment-variable block
//! (`%USERPROFILE%\…`), else the relative path.

use std::path::{Path, PathBuf};

const HEADER_SIZE: u32 = 0x4C;
const LINK_CLSID: [u8; 16] = [0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46];

const HAS_ID_LIST: u32 = 0x1;
const HAS_LINK_INFO: u32 = 0x2;
const HAS_NAME: u32 = 0x4;
const HAS_RELATIVE_PATH: u32 = 0x8;
const HAS_WORKING_DIR: u32 = 0x10;
const HAS_ARGUMENTS: u32 = 0x20;
const HAS_ICON_LOCATION: u32 = 0x40;
const IS_UNICODE: u32 = 0x80;
const FORCE_NO_LINK_INFO: u32 = 0x100;
const ENVIRONMENT_BLOCK: u32 = 0xA000_0001;

pub fn is_shortcut(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
}

/// Where the shortcut at `path` points, if the file says.
pub fn target(path: &Path) -> Option<PathBuf> {
    // Shortcuts are small; anything big called .lnk isn't one.
    if std::fs::metadata(path).ok()?.len() > 1 << 20 {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    parse(&data, path.parent())
}

fn u16_at(data: &[u8], i: usize) -> Option<usize> {
    data.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
}

fn u32_at(data: &[u8], i: usize) -> Option<u32> {
    data.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn parse(data: &[u8], dir: Option<&Path>) -> Option<PathBuf> {
    if u32_at(data, 0)? != HEADER_SIZE || data.get(4..20)? != LINK_CLSID {
        return None;
    }
    let flags = u32_at(data, 0x14)?;
    let mut pos = HEADER_SIZE as usize;
    if flags & HAS_ID_LIST != 0 {
        pos += 2 + u16_at(data, pos)?;
    }
    let mut from_info = None;
    if flags & HAS_LINK_INFO != 0 && flags & FORCE_NO_LINK_INFO == 0 {
        let size = u32_at(data, pos)? as usize;
        from_info = data.get(pos..pos + size).and_then(link_info);
        pos += size;
    }
    // The strings, each there if its flag is: name, relative path, working folder,
    // arguments, icon. Only the relative path is of use.
    let unicode = flags & IS_UNICODE != 0;
    let mut relative = None;
    for flag in [HAS_NAME, HAS_RELATIVE_PATH, HAS_WORKING_DIR, HAS_ARGUMENTS, HAS_ICON_LOCATION] {
        if flags & flag == 0 {
            continue;
        }
        let count = u16_at(data, pos)?;
        let len = if unicode { count * 2 } else { count };
        let bytes = data.get(pos + 2..pos + 2 + len)?;
        if flag == HAS_RELATIVE_PATH {
            relative = Some(if unicode { wide(bytes) } else { ansi(bytes) });
        }
        pos += 2 + len;
    }
    // Extra data blocks, until a terminal block (size below 4).
    let mut from_environment = None;
    while let Some(size) = u32_at(data, pos).map(|s| s as usize) {
        if size < 8 {
            break;
        }
        if u32_at(data, pos + 4) == Some(ENVIRONMENT_BLOCK) && size >= 0x314 {
            let text = data.get(pos + 268..pos + 788).map(wide_z).filter(|t| !t.is_empty());
            let text = text.or_else(|| data.get(pos + 8..pos + 268).map(ansi_z));
            from_environment = text.filter(|t| !t.is_empty()).map(|t| expand_environment(&t));
        }
        pos += size;
    }
    from_info
        .or(from_environment)
        .map(PathBuf::from)
        .or_else(|| Some(dir?.join(relative?)))
}

/// The LinkInfo structure: a local path, or a network share, plus a common suffix.
fn link_info(info: &[u8]) -> Option<String> {
    let header = u32_at(info, 4)?;
    let flags = u32_at(info, 8)?;
    let unicode_offsets = header >= 0x24;
    let suffix = match unicode_offsets.then(|| u32_at(info, 32)).flatten().filter(|&o| o != 0) {
        Some(offset) => wide_z(info.get(offset as usize..)?),
        None => ansi_z(info.get(u32_at(info, 24)? as usize..)?),
    };
    if flags & 1 != 0 {
        let base = match unicode_offsets.then(|| u32_at(info, 28)).flatten().filter(|&o| o != 0) {
            Some(offset) => wide_z(info.get(offset as usize..)?),
            None => ansi_z(info.get(u32_at(info, 16)? as usize..)?),
        };
        return Some(join(base, &suffix));
    }
    if flags & 2 != 0 {
        let link = info.get(u32_at(info, 20)? as usize..)?;
        let name_offset = u32_at(link, 8)? as usize;
        let name = if name_offset > 0x14 {
            wide_z(link.get(u32_at(link, 0x14)? as usize..)?)
        } else {
            ansi_z(link.get(name_offset..)?)
        };
        return Some(join(name, &suffix));
    }
    None
}

fn join(mut base: String, suffix: &str) -> String {
    if !suffix.is_empty() {
        if !base.ends_with('\\') {
            base.push('\\');
        }
        base.push_str(suffix);
    }
    base
}

/// UTF-16 text up to its NUL.
fn wide_z(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units)
}

fn wide(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

/// Text in the system's ANSI code page (Windows-1250 on a Czech Windows) up to its NUL.
fn ansi_z(bytes: &[u8]) -> String {
    ansi(&bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())])
}

fn ansi(bytes: &[u8]) -> String {
    crate::util::ansi_text(bytes)
}

/// `%USERPROFILE%\Documents` with the variables filled in (unknown ones stay as they are).
fn expand_environment(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else { break };
        let name = &rest[start + 1..start + 1 + len];
        out.push_str(&rest[..start]);
        match std::env::var(name) {
            Ok(value) if !name.is_empty() => out.push_str(&value),
            _ => out.push_str(&rest[start..start + len + 2]),
        }
        rest = &rest[start + len + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A shortcut as Windows writes one: header, an ID list (skipped), LinkInfo with both
    /// the ANSI and the Unicode local path, and a relative path.
    pub fn shortcut(target: &str, relative: &str) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend(HEADER_SIZE.to_le_bytes());
        data.extend(LINK_CLSID);
        data.extend((HAS_ID_LIST | HAS_LINK_INFO | HAS_RELATIVE_PATH | IS_UNICODE).to_le_bytes());
        data.resize(HEADER_SIZE as usize, 0);
        data.extend(4u16.to_le_bytes());
        data.extend([0xAA, 0xBB, 0, 0]);
        // LinkInfo: a 0x24-byte header, a VolumeID, the ANSI path, the empty ANSI suffix,
        // then the Unicode path and suffix.
        let mut info = vec![0u8; 0x24];
        let volume = [0x10u8, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0];
        info.extend(volume);
        let ansi_path = info.len();
        info.extend(b"C:\\fallback\0");
        let ansi_suffix = info.len();
        info.push(0);
        let wide_path = info.len();
        info.extend(target.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        let wide_suffix = info.len();
        info.extend([0, 0]);
        let fields = [info.len(), 0x24, 1, 0x24, ansi_path, 0, ansi_suffix, wide_path, wide_suffix];
        for (i, value) in fields.iter().enumerate() {
            info[i * 4..i * 4 + 4].copy_from_slice(&(*value as u32).to_le_bytes());
        }
        data.extend(info);
        let units: Vec<u16> = relative.encode_utf16().collect();
        data.extend((units.len() as u16).to_le_bytes());
        data.extend(units.iter().flat_map(|u| u.to_le_bytes()));
        data.extend(0u32.to_le_bytes());
        data
    }

    #[test]
    fn reads_the_target_of_a_shortcut() {
        let data = shortcut(r"E:\Fotky\Účty a faktury", r"..\Fotky");
        assert_eq!(parse(&data, Some(Path::new(r"E:\x"))), Some(PathBuf::from(r"E:\Fotky\Účty a faktury")));
        assert_eq!(parse(b"not a shortcut at all, just text", None), None);
        assert_eq!(parse(&data[..60], None), None, "cut short");
    }

    /// The Start Menu's own shortcuts (programs, nothing personal): most should read to a
    /// target that exists. Windows only; prints the counts.
    #[cfg(windows)]
    #[test]
    fn reads_real_shortcuts() {
        let Some(start) = std::env::var_os("ProgramData").map(|d| PathBuf::from(d).join(r"Microsoft\Windows\Start Menu\Programs")) else {
            return;
        };
        let (mut total, mut found) = (0, 0);
        let mut stack = vec![start];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if is_shortcut(&path) {
                    total += 1;
                    found += usize::from(target(&path).is_some_and(|t| t.exists()));
                }
            }
        }
        println!("{found} of {total} Start Menu shortcuts lead to something that exists");
        assert!(total == 0 || found * 2 >= total, "{found} of {total}");
    }

    #[test]
    fn network_shares_and_suffixes_join() {
        assert_eq!(join(r"\\nas\share".into(), "photos"), r"\\nas\share\photos");
        assert_eq!(join(r"C:\".into(), "Users"), r"C:\Users");
        assert_eq!(join(r"C:\Users".into(), ""), r"C:\Users");
    }

    #[test]
    fn environment_variables_are_filled_in() {
        // SAFETY: tests that read this variable don't run concurrently with one changing it.
        unsafe { std::env::set_var("CM_LNK_TEST", r"C:\Users\x") };
        assert_eq!(expand_environment(r"%CM_LNK_TEST%\Documents"), r"C:\Users\x\Documents");
        assert_eq!(expand_environment(r"%CM_NO_SUCH_VARIABLE%\a"), r"%CM_NO_SUCH_VARIABLE%\a");
        assert_eq!(expand_environment("100% sure"), "100% sure");
    }
}
