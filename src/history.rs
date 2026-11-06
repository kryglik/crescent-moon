//! What rmc remembers between sessions, in `history.txt` next to the hotlist: the folders
//! the panels visited (Alt+H lists them) and the commands typed (Alt+P / Alt+N); and in
//! `inputs.txt` what was typed into the fields of dialogs.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::util;

const MAX_DIRS: usize = 300;
const MAX_COMMANDS: usize = 500;
const MAX_INPUTS: usize = 50;

#[derive(Default)]
pub struct History {
    /// Most recent first, each folder once.
    pub dirs: Vec<PathBuf>,
    /// Oldest first, as typed.
    pub commands: Vec<String>,
    /// Commands run here since the last save: another rmc window may have saved its own
    /// meanwhile, and these go after them.
    unsaved: Vec<String>,
    /// Folders forgotten here (Delete in Alt+H), which what is on disk mustn't bring back.
    forgotten: Vec<PathBuf>,
    file: Option<PathBuf>,
    pub inputs: Inputs,
}

impl History {
    /// `%APPDATA%\rmc\history.txt` on Windows, `~/.config/rmc/history.txt` elsewhere.
    pub fn default_file() -> Option<PathBuf> {
        util::config_dir().map(|dir| dir.join("history.txt"))
    }

    /// Reads the file; a missing or unreadable one gives an empty history.
    pub fn load(file: Option<PathBuf>) -> History {
        let inputs = Inputs::load(file.as_ref().map(|f| f.with_file_name("inputs.txt")));
        let mut history = History { file, inputs, ..History::default() };
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

    /// Saves the history together with what other rmc windows saved meanwhile: their
    /// folders after this window's, their commands before the ones run here since the last
    /// save. (This window's own lists stay as they are until it starts again.)
    pub fn save(&mut self) -> io::Result<()> {
        let Some(file) = &self.file else { return Ok(()) };
        let on_disk = History::load(Some(file.clone()));
        let mut dirs = self.dirs.clone();
        for dir in on_disk.dirs {
            if !dirs.iter().chain(&self.forgotten).any(|d| util::same_path(d, &dir)) {
                dirs.push(dir);
            }
        }
        dirs.truncate(MAX_DIRS);
        let mut commands = on_disk.commands;
        if commands.is_empty() {
            // Nothing saved yet (or the file is gone): what this window knows.
            commands = self.commands.clone();
        } else {
            for line in &self.unsaved {
                if commands.last() != Some(line) {
                    commands.push(line.clone());
                }
            }
        }
        let excess = commands.len().saturating_sub(MAX_COMMANDS);
        commands.drain(..excess);

        let mut text = String::from("[folders]\n");
        for dir in &dirs {
            text.push_str(&dir.to_string_lossy());
            text.push('\n');
        }
        text.push_str("[commands]\n");
        for command in &commands {
            text.push_str(command);
            text.push('\n');
        }
        util::write_atomically(file, &text, false)?;
        self.unsaved.clear();
        self.inputs.save()
    }

    /// A panel arrived in `dir`: it moves to the front of the list.
    pub fn visit(&mut self, dir: &Path) {
        self.dirs.retain(|d| !util::same_path(d, dir));
        self.dirs.insert(0, dir.to_path_buf());
        self.dirs.truncate(MAX_DIRS);
    }

    /// `dir` goes from the list, also from what is saved.
    pub fn forget(&mut self, dir: &Path) {
        self.dirs.retain(|d| !util::same_path(d, dir));
        self.forgotten.push(dir.to_path_buf());
    }

    /// A command was run; a repeat of the last one isn't added again.
    pub fn command(&mut self, line: &str) {
        if line.contains(['\n', '\r']) || self.commands.last().is_some_and(|last| last == line) {
            return;
        }
        self.commands.push(line.to_string());
        self.unsaved.push(line.to_string());
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

/// What was typed into the fields of dialogs, a list for each kind of field
/// (`InputAction::kind`). In a file of its own: an older rmc still open in another window
/// would take sections of `history.txt` it doesn't know for commands.
#[derive(Default)]
pub struct Inputs {
    /// By kind, oldest first, each text once.
    lists: BTreeMap<String, Vec<String>>,
    /// Typed here since the last save: another window may have saved its own meanwhile.
    unsaved: Vec<(String, String)>,
    /// Forgotten here, which what is on disk mustn't bring back.
    forgotten: Vec<(String, String)>,
    file: Option<PathBuf>,
}

impl Inputs {
    /// Reads the file: `[kind]` lines, each followed by its texts as `=text` lines (so a text
    /// that looks like a heading stays a text). Missing or unreadable, the lists start empty.
    fn load(file: Option<PathBuf>) -> Inputs {
        let mut inputs = Inputs { file, ..Inputs::default() };
        let Some(text) = inputs.file.as_ref().and_then(|f| fs::read_to_string(f).ok()) else { return inputs };
        let mut kind = None;
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                kind = Some(name.to_string());
            } else if let (Some(kind), Some(entry)) = (&kind, line.strip_prefix('=')) {
                put(inputs.lists.entry(kind.clone()).or_default(), entry);
            }
        }
        inputs
    }

    /// What was entered in this kind of field, oldest first.
    pub fn list(&self, kind: &str) -> &[String] {
        self.lists.get(kind).map_or(&[], Vec::as_slice)
    }

    /// `text` was entered: it is the newest of its kind now. An empty field says nothing.
    pub fn add(&mut self, kind: &str, text: &str) {
        if text.is_empty() || text.contains(['\n', '\r']) {
            return;
        }
        put(self.lists.entry(kind.to_string()).or_default(), text);
        self.unsaved.push((kind.to_string(), text.to_string()));
    }

    /// `text` goes from the list, also from what is saved.
    pub fn forget(&mut self, kind: &str, text: &str) {
        if let Some(list) = self.lists.get_mut(kind) {
            list.retain(|t| t != text);
        }
        self.unsaved.retain(|(k, t)| k != kind || t != text);
        self.forgotten.push((kind.to_string(), text.to_string()));
    }

    /// Saves what other windows saved meanwhile, and after it what was typed here.
    fn save(&mut self) -> io::Result<()> {
        let Some(file) = &self.file else { return Ok(()) };
        if self.unsaved.is_empty() && self.forgotten.is_empty() {
            return Ok(());
        }
        let mut lists = Inputs::load(Some(file.clone())).lists;
        for (kind, text) in &self.forgotten {
            if let Some(list) = lists.get_mut(kind) {
                list.retain(|t| t != text);
            }
        }
        for (kind, text) in &self.unsaved {
            put(lists.entry(kind.clone()).or_default(), text);
        }
        let mut out = String::new();
        for (kind, list) in &lists {
            out.push_str(&format!("[{kind}]\n"));
            for text in list {
                out.push('=');
                out.push_str(text);
                out.push('\n');
            }
        }
        util::write_atomically(file, &out, false)?;
        self.unsaved.clear();
        self.forgotten.clear();
        Ok(())
    }
}

/// `text` goes to the end of `list`, once; the oldest make way.
fn put(list: &mut Vec<String>, text: &str) {
    list.retain(|t| t != text);
    list.push(text.to_string());
    let excess = list.len().saturating_sub(MAX_INPUTS);
    list.drain(..excess);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Each kind has its own list, a text entered again becomes the newest, and two windows
    /// keep what the other saved. A forgotten text stays gone.
    #[test]
    fn what_is_typed_into_fields_is_kept_by_kind() {
        let tmp = TempDir::new("history-inputs");
        let file = tmp.path().join("history.txt");
        let mut a = History::load(Some(file.clone()));
        a.inputs.add("folder", "[odd] one");
        a.inputs.add("folder", "two");
        a.inputs.add("folder", "[odd] one");
        a.inputs.add("pattern", "*.jpg");
        a.inputs.add("pattern", "");
        a.save().unwrap();

        let mut b = History::load(Some(file.clone()));
        assert_eq!(b.inputs.list("folder"), ["two", "[odd] one"]);
        assert_eq!(b.inputs.list("pattern"), ["*.jpg"]);
        assert!(b.inputs.list("nothing yet").is_empty());
        a.inputs.add("folder", "from a");
        b.inputs.add("folder", "from b");
        b.inputs.forget("folder", "two");
        a.save().unwrap();
        b.save().unwrap();
        let both = History::load(Some(file));
        assert_eq!(both.inputs.list("folder"), ["[odd] one", "from a", "from b"]);

        let mut long = History::load(None);
        (0..MAX_INPUTS + 5).for_each(|i| long.inputs.add("x", &i.to_string()));
        assert_eq!(long.inputs.list("x").len(), MAX_INPUTS);
        assert_eq!(long.inputs.list("x").first().map(String::as_str), Some("5"));
    }

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

    /// A folder forgotten stays forgotten, though it is still in the file when saving.
    #[test]
    fn a_forgotten_folder_stays_forgotten() {
        let tmp = TempDir::new("history-forget");
        let file = tmp.path().join("history.txt");
        let mut first = History::load(Some(file.clone()));
        first.visit(Path::new("/keep"));
        first.visit(Path::new("/forget"));
        first.save().unwrap();
        let mut second = History::load(Some(file.clone()));
        second.forget(Path::new("/forget"));
        second.save().unwrap();
        assert_eq!(History::load(Some(file)).dirs, [PathBuf::from("/keep")]);
    }

    /// Two rmc windows open at once: each one's save keeps what the other saved.
    #[test]
    fn two_windows_keep_each_others_history() {
        let tmp = TempDir::new("history-two");
        let file = tmp.path().join("history.txt");
        let mut first = History::load(Some(file.clone()));
        first.command("make");
        first.save().unwrap();
        let mut a = History::load(Some(file.clone()));
        let mut b = History::load(Some(file.clone()));
        a.visit(Path::new("/from/a"));
        a.command("echo a");
        b.visit(Path::new("/from/b"));
        b.command("echo b");
        a.save().unwrap();
        b.save().unwrap();
        a.command("echo a again");
        a.save().unwrap();
        let both = History::load(Some(file));
        assert_eq!(both.commands, ["make", "echo a", "echo b", "echo a again"]);
        assert!(both.dirs.contains(&PathBuf::from("/from/a")) && both.dirs.contains(&PathBuf::from("/from/b")));
        let left: Vec<_> = fs::read_dir(tmp.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "nothing left aside: {left:?}");
    }
}
