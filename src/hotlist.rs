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

    /// Reads the list; a missing or unreadable file gives an empty list.
    pub fn load(file: Option<PathBuf>) -> Self {
        let dirs = file
            .as_ref()
            .and_then(|f| fs::read_to_string(f).ok())
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        Hotlist { dirs, file }
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(file) = &self.file else { return Ok(()) };
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        let text: String = self.dirs.iter().map(|d| format!("{}\n", d.display())).collect();
        fs::write(file, text)
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
}
