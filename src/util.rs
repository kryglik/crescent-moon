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
    let local: DateTime<Local> = time.into();
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

fn strip_quotes(s: &str) -> &str {
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

pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `%APPDATA%\rmc` on Windows, `$XDG_CONFIG_HOME/rmc` or `~/.config/rmc` elsewhere.
pub fn config_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home_dir().map(|h| h.join(".config")))
    };
    base.map(|dir| dir.join("rmc"))
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

/// The drive part of a path (`e:` or `\\server\share`, lower-cased); None without one.
pub fn volume_key(path: &Path) -> Option<String> {
    match normalize(path).components().next() {
        Some(Component::Prefix(p)) => Some(p.as_os_str().to_string_lossy().to_lowercase()),
        _ => None,
    }
}

/// Whether two paths are on the same drive, so a move can be a plain rename.
pub fn same_volume(a: &Path, b: &Path) -> bool {
    volume_key(a) == volume_key(b)
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
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
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
