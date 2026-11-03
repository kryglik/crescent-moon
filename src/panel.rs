//! One file panel: a directory listing with a cursor, marks and sorting.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{platform, util};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortBy {
    Name,
    Extension,
    Size,
    Time,
}

impl SortBy {
    pub fn next(self) -> Self {
        match self {
            SortBy::Name => SortBy::Extension,
            SortBy::Extension => SortBy::Size,
            SortBy::Size => SortBy::Time,
            SortBy::Time => SortBy::Name,
        }
    }

    /// Shown in the panel header, as in mc.
    pub fn indicator(self) -> &'static str {
        match self {
            SortBy::Name => ".n",
            SortBy::Extension => ".e",
            SortBy::Size => ".s",
            SortBy::Time => ".t",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_link: bool,
    pub is_exec: bool,
    pub is_parent: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    sort_name: String,
}

impl Entry {
    fn parent(cwd: &Path) -> Entry {
        Entry {
            name: "..".into(),
            path: cwd.parent().unwrap_or(cwd).to_path_buf(),
            is_dir: true,
            is_link: false,
            is_exec: false,
            is_parent: true,
            size: 0,
            modified: fs::metadata(cwd).and_then(|m| m.modified()).ok(),
            sort_name: String::new(),
        }
    }

    fn extension(&self) -> &str {
        match self.sort_name.rfind('.') {
            Some(i) if i > 0 => &self.sort_name[i + 1..],
            _ => "",
        }
    }
}

/// A directory size from Ctrl+Space: a running total until `done`.
#[derive(Clone, Copy, Debug)]
pub struct DirSize {
    pub bytes: u64,
    pub done: bool,
}

pub struct Panel {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub offset: usize,
    pub marked: HashSet<String>,
    /// Sizes of subdirectories, by name, for the current directory only.
    pub dir_sizes: HashMap<String, DirSize>,
    pub sort: SortBy,
    pub show_hidden: bool,
    /// Visible list rows, updated on every draw (used for PgUp/PgDn).
    pub page: usize,
    pub space: Option<(u64, u64)>,
}

impl Panel {
    /// Opens `cwd`, falling back to its parents, the home directory or the system root.
    pub fn new(cwd: PathBuf, show_hidden: bool) -> Panel {
        let mut panel = Panel {
            cwd: PathBuf::new(),
            entries: Vec::new(),
            cursor: 0,
            offset: 0,
            marked: HashSet::new(),
            dir_sizes: HashMap::new(),
            sort: SortBy::Name,
            show_hidden,
            page: 20,
            space: None,
        };
        let cwd = util::normalize(&cwd);
        let candidates: Vec<PathBuf> = cwd
            .ancestors()
            .map(Path::to_path_buf)
            .chain(util::home_dir())
            .chain([system_root()])
            .collect();
        for dir in candidates {
            panel.cwd = dir;
            if panel.load().is_ok() {
                break;
            }
        }
        panel
    }

    /// Re-reads the directory. Marks of entries that still exist are kept.
    pub fn load(&mut self) -> io::Result<()> {
        let mut entries = read_entries(&self.cwd, self.show_hidden)?;
        sort_entries(&mut entries, self.sort, &self.dir_sizes);
        // On Windows a drive root gets ".." too: it opens the drive picker.
        if self.cwd.parent().is_some() || cfg!(windows) {
            entries.insert(0, Entry::parent(&self.cwd));
        }
        self.entries = entries;
        self.space = platform::disk_space(&self.cwd);
        let entries = &self.entries;
        self.marked.retain(|name| entries.iter().any(|e| !e.is_parent && &e.name == name));
        self.dir_sizes.retain(|name, _| entries.iter().any(|e| e.is_dir && &e.name == name));
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
        Ok(())
    }

    /// Reloads and keeps the cursor on the same name when it still exists.
    pub fn reload(&mut self) {
        let name = self.current().map(|e| e.name.clone());
        if self.load().is_err() {
            // The directory vanished (deleted, drive removed): climb to something that exists.
            let cwd = self.cwd.clone();
            let show_hidden = self.show_hidden;
            let sort = self.sort;
            *self = Panel::new(cwd, show_hidden);
            self.sort = sort;
            let _ = self.load();
            return;
        }
        if let Some(name) = name {
            self.select_name(&name);
        }
    }

    pub fn change_dir(&mut self, dir: PathBuf) -> Result<(), String> {
        let dir = util::normalize(&dir);
        let previous = std::mem::replace(&mut self.cwd, dir.clone());
        let marked = std::mem::take(&mut self.marked);
        let sizes = std::mem::take(&mut self.dir_sizes);
        match self.load() {
            Ok(()) => {
                self.cursor = 0;
                self.offset = 0;
                // Coming back up: put the cursor on the directory we just left.
                if let Some(name) = util::child_name(&self.cwd, &previous) {
                    self.select_name(&name);
                }
                Ok(())
            }
            Err(e) => {
                self.cwd = previous;
                self.marked = marked;
                self.dir_sizes = sizes;
                let _ = self.load();
                Err(format!("Cannot open \"{}\":\n{e}", dir.display()))
            }
        }
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    pub fn select_name(&mut self, name: &str) -> bool {
        let found = self.entries.iter().position(|e| e.name == name).or_else(|| {
            let lower = name.to_lowercase();
            self.entries.iter().position(|e| e.name.to_lowercase() == lower)
        });
        if let Some(i) = found {
            self.cursor = i;
        }
        found.is_some()
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
    }

    pub fn page_up(&mut self) {
        self.move_by(-(self.page.max(1) as isize - 1).max(1));
    }

    pub fn page_down(&mut self) {
        self.move_by((self.page.max(1) as isize - 1).max(1));
    }

    /// Scrolls so the cursor is inside a list `height` rows tall.
    pub fn ensure_visible(&mut self, height: usize) {
        self.page = height.max(1);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.page {
            self.offset = self.cursor + 1 - self.page;
        }
        self.offset = self.offset.min(self.entries.len().saturating_sub(self.page));
    }

    /// Jumps to the next entry whose name starts with `prefix` (case-insensitive).
    pub fn find_prefix(&mut self, prefix: &str, skip_current: bool) -> bool {
        let prefix = prefix.to_lowercase();
        let n = self.entries.len();
        let start = if skip_current { 1 } else { 0 };
        for step in start..n + start {
            let i = (self.cursor + step) % n.max(1);
            if self.entries[i].name.to_lowercase().starts_with(&prefix) {
                self.cursor = i;
                return true;
            }
        }
        false
    }

    pub fn is_marked(&self, entry: &Entry) -> bool {
        !entry.is_parent && self.marked.contains(&entry.name)
    }

    pub fn toggle_mark(&mut self) {
        let Some(entry) = self.current() else { return };
        if entry.is_parent {
            return;
        }
        let name = entry.name.clone();
        if !self.marked.remove(&name) {
            self.marked.insert(name);
        }
    }

    /// Marks or unmarks files (not directories) matching a wildcard pattern, like mc's + and -.
    pub fn mark_matching(&mut self, pattern: &str, mark: bool) -> usize {
        let names: Vec<String> = self
            .entries
            .iter()
            .filter(|e| !e.is_parent && !e.is_dir && util::glob_match(pattern, &e.name))
            .map(|e| e.name.clone())
            .collect();
        for name in &names {
            if mark {
                self.marked.insert(name.clone());
            } else {
                self.marked.remove(name);
            }
        }
        names.len()
    }

    /// Inverts the marks on files, like mc's *.
    pub fn invert_marks(&mut self) {
        for entry in self.entries.iter().filter(|e| !e.is_parent && !e.is_dir) {
            if !self.marked.remove(&entry.name) {
                self.marked.insert(entry.name.clone());
            }
        }
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    /// The marked entries, or the entry under the cursor when nothing is marked.
    pub fn selection(&self) -> Vec<PathBuf> {
        if self.marked.is_empty() {
            return self
                .current()
                .filter(|e| !e.is_parent)
                .map(|e| vec![e.path.clone()])
                .unwrap_or_default();
        }
        self.entries
            .iter()
            .filter(|e| self.is_marked(e))
            .map(|e| e.path.clone())
            .collect()
    }

    /// Number of marked entries and their total size (directories count once measured).
    pub fn marked_summary(&self) -> Option<(usize, u64)> {
        if self.marked.is_empty() {
            return None;
        }
        let marked = self.entries.iter().filter(|e| self.is_marked(e));
        Some(marked.fold((0, 0), |(n, bytes), e| (n + 1, bytes + effective_size(e, &self.dir_sizes))))
    }

    pub fn dir_size(&self, entry: &Entry) -> Option<DirSize> {
        if entry.is_dir && !entry.is_parent { self.dir_sizes.get(&entry.name).copied() } else { None }
    }

    pub fn has_pending_sizes(&self) -> bool {
        self.dir_sizes.values().any(|s| !s.done)
    }

    /// Records a size result for `dir` if it is a subdirectory this panel asked about.
    pub fn set_dir_size(&mut self, dir: &Path, bytes: u64, done: bool) {
        if !dir.parent().is_some_and(|parent| util::same_path(parent, &self.cwd)) {
            return;
        }
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
        let Some(size) = self.dir_sizes.get_mut(&name).filter(|s| !s.done) else { return };
        *size = DirSize { bytes, done };
        if done && self.sort == SortBy::Size {
            self.resort();
        }
    }

    /// Size of everything listed, once every subdirectory has been measured.
    pub fn total(&self) -> Option<u64> {
        if self.dir_sizes.is_empty() {
            return None;
        }
        let mut total = 0;
        for entry in self.entries.iter().filter(|e| !e.is_parent) {
            if entry.is_dir && !self.dir_size(entry).is_some_and(|s| s.done) {
                return None;
            }
            total += effective_size(entry, &self.dir_sizes);
        }
        Some(total)
    }

    /// Sorts again without re-reading the directory, keeping the cursor on the same entry.
    fn resort(&mut self) {
        let name = self.current().map(|e| e.name.clone());
        let start = usize::from(self.entries.first().is_some_and(|e| e.is_parent));
        sort_entries(&mut self.entries[start..], self.sort, &self.dir_sizes);
        if let Some(name) = name {
            self.select_name(&name);
        }
    }
}

/// A file's size, or a directory's measured size (0 until measured).
fn effective_size(entry: &Entry, sizes: &HashMap<String, DirSize>) -> u64 {
    if !entry.is_dir {
        return entry.size;
    }
    sizes.get(&entry.name).filter(|s| s.done).map_or(0, |s| s.bytes)
}

fn read_entries(dir: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        let own = item.metadata().ok();
        if !show_hidden && platform::is_hidden(&name, own.as_ref()) {
            continue;
        }
        let is_link = own.as_ref().is_some_and(|m| m.file_type().is_symlink());
        let path = item.path();
        // For links, show what they point to (a broken link falls back to the link itself).
        let target = if is_link { fs::metadata(&path).ok() } else { None };
        let meta = target.as_ref().or(own.as_ref());
        let is_dir = meta.is_some_and(|m| m.is_dir());
        out.push(Entry {
            sort_name: name.to_lowercase(),
            is_exec: !is_dir && platform::is_executable(&name, meta),
            size: if is_dir { 0 } else { meta.map_or(0, |m| m.len()) },
            modified: meta.and_then(|m| m.modified().ok()),
            name,
            path,
            is_dir,
            is_link,
            is_parent: false,
        });
    }
    Ok(out)
}

fn sort_entries(entries: &mut [Entry], sort: SortBy, sizes: &HashMap<String, DirSize>) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| match sort {
                SortBy::Name => Ordering::Equal,
                SortBy::Extension => a.extension().cmp(b.extension()),
                SortBy::Size => effective_size(b, sizes).cmp(&effective_size(a, sizes)),
                SortBy::Time => b.modified.cmp(&a.modified),
            })
            .then_with(|| a.sort_name.cmp(&b.sort_name))
    });
}

fn system_root() -> PathBuf {
    if cfg!(windows) {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        PathBuf::from(format!("{drive}\\"))
    } else {
        PathBuf::from("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn names(panel: &Panel) -> Vec<&str> {
        panel.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn lists_parent_then_directories_then_files() {
        let tmp = TempDir::new("panel-list");
        tmp.file("b.txt", "bb");
        tmp.file("A.rs", "a");
        tmp.dir("zeta");
        tmp.dir("Alpha");
        let panel = Panel::new(tmp.path().to_path_buf(), true);
        assert_eq!(names(&panel), ["..", "Alpha", "zeta", "A.rs", "b.txt"]);
        assert_eq!(panel.entries[4].size, 2);
    }

    #[test]
    fn sorting_by_size_and_extension() {
        let tmp = TempDir::new("panel-sort");
        tmp.file("small.txt", "1");
        tmp.file("big.md", "1234567");
        tmp.file("mid.rs", "123");
        let mut panel = Panel::new(tmp.path().to_path_buf(), true);
        panel.sort = SortBy::Size;
        panel.reload();
        assert_eq!(names(&panel), ["..", "big.md", "mid.rs", "small.txt"]);
        panel.sort = SortBy::Extension;
        panel.reload();
        assert_eq!(names(&panel), ["..", "big.md", "mid.rs", "small.txt"]);
    }

    #[test]
    fn going_up_selects_the_directory_we_left() {
        let tmp = TempDir::new("panel-up");
        tmp.dir("one");
        tmp.dir("two");
        let mut panel = Panel::new(tmp.path().join("two"), true);
        let parent = panel.cwd.parent().unwrap().to_path_buf();
        panel.change_dir(parent).unwrap();
        assert_eq!(panel.current().unwrap().name, "two");
    }

    #[test]
    fn failed_change_dir_keeps_the_old_directory() {
        let tmp = TempDir::new("panel-fail");
        let mut panel = Panel::new(tmp.path().to_path_buf(), true);
        assert!(panel.change_dir(tmp.path().join("missing")).is_err());
        assert!(util::same_path(&panel.cwd, tmp.path()));
    }

    #[test]
    fn marking_and_selection() {
        let tmp = TempDir::new("panel-mark");
        tmp.file("a.txt", "");
        tmp.file("b.txt", "");
        tmp.file("c.rs", "");
        tmp.dir("dir.txt");
        let mut panel = Panel::new(tmp.path().to_path_buf(), true);
        assert_eq!(panel.mark_matching("*.txt", true), 2, "directories are not marked by pattern");
        assert_eq!(panel.selection().len(), 2);
        panel.invert_marks();
        let selected: Vec<_> = panel.selection().iter().map(|p| p.file_name().unwrap().to_owned()).collect();
        assert_eq!(selected, ["c.rs"]);
        panel.clear_marks();
        panel.home();
        assert!(panel.selection().is_empty(), "'..' is never selected");
        panel.move_by(1);
        assert_eq!(panel.selection(), [tmp.path().join("dir.txt")]);
    }

    #[test]
    fn quick_search_finds_by_prefix() {
        let tmp = TempDir::new("panel-search");
        tmp.file("apple", "");
        tmp.file("banana", "");
        tmp.file("Blueberry", "");
        let mut panel = Panel::new(tmp.path().to_path_buf(), true);
        assert!(panel.find_prefix("b", false));
        assert_eq!(panel.current().unwrap().name, "banana");
        assert!(panel.find_prefix("b", true));
        assert_eq!(panel.current().unwrap().name, "Blueberry");
        assert!(!panel.find_prefix("x", false));
    }
}
