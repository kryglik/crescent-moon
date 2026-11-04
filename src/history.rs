//! What rmc remembers between sessions, in `history.txt` next to the hotlist: the folders
//! the panels visited (Alt+H lists them) and the commands typed (Alt+P / Alt+N).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::util;

const MAX_DIRS: usize = 300;
const MAX_COMMANDS: usize = 500;

#[derive(Default)]
pub struct History {
    /// Most recent first, each folder once.
    pub dirs: Vec<PathBuf>,
    /// Oldest first, as typed.
    pub commands: Vec<String>,
    file: Option<PathBuf>,
}

impl History {
    /// `%APPDATA%\rmc\history.txt` on Windows, `~/.config/rmc/history.txt` elsewhere.
    pub fn default_file() -> Option<PathBuf> {
        util::config_dir().map(|dir| dir.join("history.txt"))
    }

    /// Reads the file; a missing or unreadable one gives an empty history.
    pub fn load(file: Option<PathBuf>) -> History {
        let mut history = History { file, ..History::default() };
        let Some(text) = history.file.as_ref().and_then(|f| fs::read_to_string(f).ok()) else { return history };
        let mut section = "";
        for line in text.lines() {
            match line.trim_end_matches('\r') {
                "[folders]" => section = "folders",
                "[commands]" => section = "commands",
                "" => {}
                line if section == "folders" => history.dirs.push(PathBuf::from(line)),
                line if section == "commands" => history.commands.push(line.to_string()),
                _ => {}
            }
        }
        history.dirs.truncate(MAX_DIRS);
        let excess = history.commands.len().saturating_sub(MAX_COMMANDS);
        history.commands.drain(..excess);
        history
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(file) = &self.file else { return Ok(()) };
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut text = String::from("[folders]\n");
        for dir in &self.dirs {
            text.push_str(&dir.to_string_lossy());
            text.push('\n');
        }
        text.push_str("[commands]\n");
        for command in &self.commands {
            text.push_str(command);
            text.push('\n');
        }
        // Written aside and renamed, so a crash never leaves half a file.
        let temp = file.with_extension("tmp");
        fs::write(&temp, text)?;
        fs::rename(&temp, file)
    }

    /// A panel arrived in `dir`: it moves to the front of the list.
    pub fn visit(&mut self, dir: &Path) {
        self.dirs.retain(|d| !util::same_path(d, dir));
        self.dirs.insert(0, dir.to_path_buf());
        self.dirs.truncate(MAX_DIRS);
    }

    /// A command was run; a repeat of the last one isn't added again.
    pub fn command(&mut self, line: &str) {
        if line.contains(['\n', '\r']) || self.commands.last().is_some_and(|last| last == line) {
            return;
        }
        self.commands.push(line.to_string());
        let excess = self.commands.len().saturating_sub(MAX_COMMANDS);
        self.commands.drain(..excess);
    }

    /// The folders matching `filter` (each word somewhere in the path, ignoring case), most
    /// recent first.
    pub fn matching(&self, filter: &str) -> Vec<&PathBuf> {
        let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
        self.dirs
            .iter()
            .filter(|dir| {
                let text = dir.to_string_lossy().to_lowercase();
                words.iter().all(|w| text.contains(w.as_str()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn remembers_folders_and_commands_between_sessions() {
        let tmp = TempDir::new("history");
        let file = tmp.path().join("config").join("history.txt");
        let mut history = History::load(Some(file.clone()));
        assert!(history.dirs.is_empty() && history.commands.is_empty());
        history.visit(Path::new(r"C:\Users"));
        history.visit(Path::new(r"E:\Fotky\Účty"));
        history.visit(Path::new(r"C:\Users"));
        history.command("cargo test");
        history.command("cargo test");
        history.command("git status");
        history.save().unwrap();

        let loaded = History::load(Some(file));
        assert_eq!(loaded.dirs, [PathBuf::from(r"C:\Users"), PathBuf::from(r"E:\Fotky\Účty")]);
        assert_eq!(loaded.commands, ["cargo test", "git status"]);
        assert_eq!(loaded.matching("fot úč"), [&PathBuf::from(r"E:\Fotky\Účty")]);
        assert_eq!(loaded.matching("").len(), 2);
    }
}
