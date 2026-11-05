//! Favourite directories (Ctrl+\ or Ctrl+D), one per line in a text file in the config directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::util;

pub struct Hotlist {
    pub dirs: Vec<PathBuf>,
    file: Option<PathBuf>,
}

impl Hotlist {
    /// `%APPDATA%\rmc\hotlist.txt` on Windows.
    pub fn default_file() -> Option<PathBuf> {
        util::config_dir().map(|dir| dir.join("hotlist.txt"))
    }

    /// Reads the list (in whatever encoding an editor saved it); a missing file gives an empty
    /// list. A file that is there but can't be read is never saved over: the list starts
    /// empty and stays unsaved.
    pub fn load(file: Option<PathBuf>) -> Self {
        let text = match file.as_deref().map(util::read_text) {
            Some(Ok(text)) => text,
            Some(Err(e)) if e.kind() != io::ErrorKind::NotFound => return Hotlist { dirs: Vec::new(), file: None },
            _ => String::new(),
        };
        let dirs = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(PathBuf::from)
            .collect();
        Hotlist { dirs, file }
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(file) = &self.file else { return Ok(()) };
        let text: String = self.dirs.iter().map(|d| format!("{}\n", d.display())).collect();
        // Never half a hotlist, whatever happens while it is written.
        util::write_atomically(file, &text)
    }

    /// Adds `dir` unless it is already listed; returns its position.
    pub fn add(&mut self, dir: &Path) -> usize {
        if let Some(i) = self.dirs.iter().position(|d| util::same_path(d, dir)) {
            return i;
        }
        self.dirs.push(dir.to_path_buf());
        self.dirs.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn saves_and_loads_without_duplicates() {
        let tmp = TempDir::new("hotlist");
        let file = tmp.path().join("config").join("hotlist.txt");
        let mut list = Hotlist::load(Some(file.clone()));
        assert!(list.dirs.is_empty());
        assert_eq!(list.add(Path::new(r"C:\Users")), 0);
        assert_eq!(list.add(Path::new(r"E:\devel\Účty")), 1);
        assert_eq!(list.add(Path::new(r"C:\Users")), 0, "no duplicates");
        list.save().unwrap();
        let loaded = Hotlist::load(Some(file));
        assert_eq!(loaded.dirs, [PathBuf::from(r"C:\Users"), PathBuf::from(r"E:\devel\Účty")]);
    }

    /// Edited by hand and saved with a BOM, or as UTF-16 (Windows PowerShell's `>`): it
    /// still reads, and the first entry doesn't start with an invisible character.
    #[test]
    fn reads_files_saved_by_other_tools() {
        let tmp = TempDir::new("hotlist-encodings");
        let file = tmp.path().join("hotlist.txt");
        fs::write(&file, "\u{feff}C:\\Users\r\nE:\\Účty\r\n").unwrap();
        assert_eq!(Hotlist::load(Some(file.clone())).dirs, [PathBuf::from(r"C:\Users"), PathBuf::from(r"E:\Účty")]);
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("D:\\Fotky\r\n".encode_utf16().flat_map(u16::to_le_bytes));
        fs::write(&file, utf16).unwrap();
        assert_eq!(Hotlist::load(Some(file)).dirs, [PathBuf::from(r"D:\Fotky")]);
    }
}
