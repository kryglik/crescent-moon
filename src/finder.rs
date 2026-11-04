//! The Find window (Alt+F7): two fields, Text and Files, and the results of the search they
//! describe, which restarts a moment after each change (so typing isn't slowed down).
//! Results are kept sorted by path; the cursor stays on its item while new ones arrive.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::dialog::LineEdit;
use crate::find::{Found, Query, Search};

/// How long after the last change the search restarts.
const SETTLE: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Field {
    Text,
    Names,
}

/// A row of the results: a file (or folder), or one of its matching lines.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Row {
    File(usize),
    Hit(usize, usize),
}

pub struct Finder {
    /// Where the search starts: the active panel's folder when Find was opened.
    pub root: PathBuf,
    pub text: LineEdit,
    pub names: LineEdit,
    pub field: Field,
    /// Hidden files and what .gitignore ignores too.
    pub everything: bool,
    /// Sorted by `keys`.
    pub files: Vec<Found>,
    keys: Vec<String>,
    /// The first row of each file.
    starts: Vec<usize>,
    pub cursor: usize,
    pub offset: usize,
    /// Visible result rows at the last draw.
    pub page: usize,
    search: Option<Search>,
    changed_at: Option<Instant>,
    started: Instant,
    /// How long the last search took, once it finished.
    pub took: Option<Duration>,
    pub literal: bool,
    pub limited: bool,
    pub searched: u64,
}

impl Finder {
    pub fn new(root: PathBuf) -> Self {
        Finder {
            root,
            text: LineEdit::default(),
            names: LineEdit::default(),
            field: Field::Text,
            everything: false,
            files: Vec::new(),
            keys: Vec::new(),
            starts: Vec::new(),
            cursor: 0,
            offset: 0,
            page: 20,
            search: None,
            changed_at: None,
            started: Instant::now(),
            took: None,
            literal: false,
            limited: false,
            searched: 0,
        }
    }

    /// The same query in another folder, searched at once.
    pub fn moved_to(&self, root: PathBuf) -> Self {
        let mut finder = Finder::new(root);
        (finder.text, finder.names, finder.everything) = (self.text.clone(), self.names.clone(), self.everything);
        finder.changed_at = Some(Instant::now().checked_sub(SETTLE).unwrap_or_else(Instant::now));
        finder
    }

    pub fn query(&self) -> Query {
        Query { text: self.text.text().to_string(), names: self.names.text().to_string(), everything: self.everything }
    }

    pub fn field_mut(&mut self) -> &mut LineEdit {
        match self.field {
            Field::Text => &mut self.text,
            Field::Names => &mut self.names,
        }
    }

    /// The query changed: search again once typing pauses.
    pub fn changed(&mut self) {
        self.changed_at = Some(Instant::now());
    }

    /// A search is running or about to start.
    pub fn working(&self) -> bool {
        self.changed_at.is_some() || self.search.is_some()
    }

    /// Starts a settled query and takes in what the search found. True if anything changed.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        if self.changed_at.is_some_and(|at| at.elapsed() >= SETTLE) {
            self.changed_at = None;
            self.restart();
            changed = true;
        }
        let Some(search) = &self.search else { return changed };
        // Done is set after the last result was sent: seen before draining, nothing is missed.
        let done = search.is_done();
        let mut arrived = Vec::new();
        while let Ok(found) = search.found.try_recv() {
            arrived.push(found);
        }
        (self.searched, self.limited, self.literal) = (search.searched(), search.limited(), search.literal);
        if done {
            self.search = None;
            self.took = Some(self.started.elapsed());
        }
        if !arrived.is_empty() {
            self.take(arrived);
        }
        // The counters move while it runs.
        true
    }

    fn restart(&mut self) {
        self.search = None;
        self.files.clear();
        self.keys.clear();
        self.starts.clear();
        (self.cursor, self.offset, self.took, self.searched, self.limited) = (0, 0, None, 0, false);
        self.started = Instant::now();
        self.search = Search::start(&self.root, &self.query());
        if self.search.is_none() {
            self.literal = false;
        }
    }

    /// Adds found files in path order, keeping the cursor on the item it was on.
    fn take(&mut self, arrived: Vec<Found>) {
        let selected = self.row(self.cursor).map(|row| match row {
            Row::File(f) => (self.keys[f].clone(), None),
            Row::Hit(f, h) => (self.keys[f].clone(), Some(h)),
        });
        for found in arrived {
            let key = sort_key(&found.path);
            let at = self.keys.partition_point(|k| *k < key);
            self.keys.insert(at, key);
            self.files.insert(at, found);
        }
        self.starts.clear();
        let mut row = 0;
        for file in &self.files {
            self.starts.push(row);
            row += 1 + file.hits.len();
        }
        if let Some((key, hit)) = selected
            && let Ok(f) = self.keys.binary_search(&key)
        {
            self.cursor = self.starts[f] + hit.map_or(0, |h| h + 1);
        }
    }

    pub fn rows(&self) -> usize {
        self.starts.last().map_or(0, |&s| s + 1 + self.files.last().map_or(0, |f| f.hits.len()))
    }

    pub fn row(&self, i: usize) -> Option<Row> {
        if i >= self.rows() {
            return None;
        }
        let f = self.starts.partition_point(|&s| s <= i) - 1;
        Some(if i == self.starts[f] { Row::File(f) } else { Row::Hit(f, i - self.starts[f] - 1) })
    }

    pub fn move_by(&mut self, delta: isize) {
        let last = self.rows().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    pub fn move_to(&mut self, row: usize) {
        self.cursor = row.min(self.rows().saturating_sub(1));
    }

    /// Keeps the cursor row on screen in `height` rows.
    pub fn ensure_visible(&mut self, height: usize) {
        self.page = height.max(1);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.page {
            self.offset = self.cursor + 1 - self.page;
        }
        self.offset = self.offset.min(self.rows().saturating_sub(self.page));
    }

    /// The file under the cursor, its line (on a line, or a file's first match), and whether
    /// it is a folder.
    pub fn selected(&self) -> Option<(&Path, Option<u64>, bool)> {
        match self.row(self.cursor)? {
            Row::File(f) => {
                let file = &self.files[f];
                Some((&file.path, file.hits.first().map(|h| h.number), file.is_dir))
            }
            Row::Hit(f, h) => Some((&self.files[f].path, Some(self.files[f].hits[h].number), false)),
        }
    }

    /// The path below the root, as shown.
    pub fn shown_path(&self, f: usize) -> String {
        let path = &self.files[f].path;
        path.strip_prefix(&self.root).unwrap_or(path).display().to_string()
    }

    pub fn total_hits(&self) -> usize {
        self.files.iter().map(|f| f.hits.len()).sum()
    }
}

/// Paths sort case-insensitively, folders before the files in them.
fn sort_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find::Hit;

    fn found(path: &str, lines: &[u64]) -> Found {
        let hits = lines.iter().map(|&number| Hit { number, text: format!("line {number}"), ranges: vec![] }).collect();
        Found { path: PathBuf::from(path), is_dir: false, hits }
    }

    #[test]
    fn results_stay_sorted_and_the_cursor_stays_put() {
        let mut finder = Finder::new(PathBuf::from("/r"));
        finder.take(vec![found("/r/b.rs", &[3, 9])]);
        assert_eq!(finder.rows(), 3);
        finder.move_to(2);
        assert_eq!(finder.selected().unwrap().1, Some(9));
        // An earlier file arrives: rows shift, the cursor follows its line.
        finder.take(vec![found("/r/A.rs", &[1]), found("/r/c.rs", &[5])]);
        assert_eq!(finder.row(0), Some(Row::File(0)));
        assert_eq!(finder.shown_path(0), "A.rs");
        assert_eq!(finder.selected().unwrap().1, Some(9));
        assert_eq!(finder.cursor, 4);
        assert_eq!((finder.rows(), finder.total_hits()), (7, 4));
        assert_eq!(finder.row(6), Some(Row::Hit(2, 0)));
        assert_eq!(finder.row(7), None);
    }
}
