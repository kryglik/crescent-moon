//! Alt+F7: finding text in files under a folder the way ag and ripgrep do it, with ripgrep's
//! own crates: folders walked in parallel; files that .gitignore ignores, hidden files and
//! binary files skipped (unless asked for everything); results streamed back per file while
//! the search runs, and the search stopped as soon as it is dropped.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder};
use ignore::{WalkBuilder, WalkState};

use crate::util;

/// The search stops after this many lines (or names): plenty to look through.
pub const MAX_RESULTS: usize = 10_000;
/// At most this many lines from one file, so a log matching on every line leaves room.
const MAX_LINES_PER_FILE: usize = 1_000;
/// Long lines (minified code) are cut to about this many characters around the first match.
const LINE_CHARS: usize = 300;

/// What the Find window asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    /// A regex, smart case: lowercase ignores case, a capital letter makes it count. Empty
    /// means finding files by name only.
    pub text: String,
    /// Name patterns, see `NameFilter`.
    pub names: String,
    /// Hidden files and what .gitignore ignores too (ag -u).
    pub everything: bool,
}

/// A matching line: its number, its text (tabs and control characters as spaces) and the
/// byte ranges of the matches in it.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub number: u64,
    pub text: String,
    pub ranges: Vec<(usize, usize)>,
}

/// A file (or, in a search by name, a folder) and its matching lines.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub path: PathBuf,
    pub is_dir: bool,
    pub hits: Vec<Hit>,
}

/// The Files field: `*.rs *.toml` keeps only those names, `!*test*` drops names, and a word
/// without wildcards matches anywhere in a name. Case doesn't matter; spaces, commas or
/// semicolons separate patterns.
#[derive(Clone, Debug, Default)]
pub struct NameFilter {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl NameFilter {
    pub fn parse(text: &str) -> Self {
        let mut filter = NameFilter::default();
        for token in text.split(|c: char| c.is_whitespace() || c == ',' || c == ';').filter(|t| !t.is_empty()) {
            let (list, pattern) = match token.strip_prefix('!') {
                Some(rest) => (&mut filter.exclude, rest),
                None => (&mut filter.include, token),
            };
            if !pattern.is_empty() {
                list.push(if pattern.contains(['*', '?']) { pattern.to_string() } else { format!("*{pattern}*") });
            }
        }
        filter
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    pub fn matches(&self, name: &str) -> bool {
        (self.include.is_empty() || self.include.iter().any(|glob| util::glob_match(glob, name)))
            && !self.excludes(name)
    }

    /// A name left out with `!`: for a folder, everything in it is left out too.
    pub fn excludes(&self, name: &str) -> bool {
        self.exclude.iter().any(|glob| util::glob_match(glob, name))
    }
}

/// A file being searched, which stops reading once the search is stopped: a multi-gigabyte
/// log shouldn't keep a worker busy after the query has changed.
struct Stoppable<R> {
    inner: R,
    stop: Arc<AtomicBool>,
}

impl<R: std::io::Read> std::io::Read for Stoppable<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("search stopped"));
        }
        self.inner.read(buf)
    }
}

/// A running search. Dropping it stops it.
pub struct Search {
    pub found: Receiver<Found>,
    /// The text wasn't a valid regex, so it is searched for as typed.
    pub literal: bool,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    searched: Arc<AtomicU64>,
    results: Arc<AtomicUsize>,
}

impl Search {
    /// Starts searching below `root`; None if the query asks for nothing.
    pub fn start(root: &Path, query: &Query) -> Option<Search> {
        let names = NameFilter::parse(&query.names);
        if query.text.is_empty() && names.is_empty() {
            return None;
        }
        let (matcher, literal) = if query.text.is_empty() {
            (None, false)
        } else {
            let (matcher, literal) = build_matcher(&query.text)?;
            (Some(matcher), literal)
        };
        let strict = !query.everything;
        let walker = WalkBuilder::new(root)
            .hidden(strict)
            .ignore(strict)
            .git_ignore(strict)
            .git_global(strict)
            .git_exclude(strict)
            .parents(strict)
            // Like ag: .gitignore counts outside a git repository too.
            .require_git(false)
            .follow_links(false)
            .build_parallel();

        let (tx, found) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let searched = Arc::new(AtomicU64::new(0));
        let results = Arc::new(AtomicUsize::new(0));
        let search = Search {
            found,
            literal,
            stop: stop.clone(),
            done: done.clone(),
            searched: searched.clone(),
            results: results.clone(),
        };
        thread::spawn(move || {
            walker.run(|| {
                let (tx, names, matcher) = (tx.clone(), names.clone(), matcher.clone());
                let (stop, searched, results) = (stop.clone(), searched.clone(), results.clone());
                // A line longer than this (a minified file's one line of hundreds of
                // megabytes) gives up on that file instead of on the memory.
                let mut searcher = SearcherBuilder::new()
                    .binary_detection(BinaryDetection::quit(0))
                    .line_number(true)
                    .heap_limit(Some(16 << 20))
                    .build();
                Box::new(move |entry| {
                    if stop.load(Ordering::Relaxed) {
                        return WalkState::Quit;
                    }
                    let Ok(entry) = entry else { return WalkState::Continue };
                    if entry.depth() == 0 {
                        return WalkState::Continue;
                    }
                    let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                    let is_file = entry.file_type().is_some_and(|t| t.is_file());
                    if matcher.is_none() {
                        searched.fetch_add(1, Ordering::Relaxed);
                    }
                    let name = entry.file_name().to_string_lossy();
                    // `!node_modules` leaves the folder out with all it holds, as rg -g does.
                    if is_dir && names.excludes(&name) {
                        return WalkState::Skip;
                    }
                    if !names.matches(&name) {
                        return WalkState::Continue;
                    }
                    let found = match &matcher {
                        // By name: files and folders alike.
                        None => Found { path: entry.into_path(), is_dir, hits: Vec::new() },
                        Some(matcher) if is_file => {
                            searched.fetch_add(1, Ordering::Relaxed);
                            let hits = search_file(&mut searcher, matcher, entry.path(), &stop);
                            if hits.is_empty() {
                                return WalkState::Continue;
                            }
                            Found { path: entry.into_path(), is_dir: false, hits }
                        }
                        Some(_) => return WalkState::Continue,
                    };
                    let count = found.hits.len().max(1);
                    if results.fetch_add(count, Ordering::Relaxed) + count >= MAX_RESULTS {
                        stop.store(true, Ordering::Relaxed);
                    }
                    if tx.send(found).is_err() {
                        stop.store(true, Ordering::Relaxed);
                        return WalkState::Quit;
                    }
                    WalkState::Continue
                })
            });
            // Release: every result sent before it is there for whoever sees it done.
            done.store(true, Ordering::Release);
        });
        Some(search)
    }

    /// The walk has finished (or was stopped); results may still be waiting in `found`.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Files looked into (in a search by name: names looked at).
    pub fn searched(&self) -> u64 {
        self.searched.load(Ordering::Relaxed)
    }

    /// It stopped at `MAX_RESULTS`.
    pub fn limited(&self) -> bool {
        self.results.load(Ordering::Relaxed) >= MAX_RESULTS
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The text as a regex with smart case; if it isn't a valid regex, as plain text (true).
fn build_matcher(text: &str) -> Option<(RegexMatcher, bool)> {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_smart(true).line_terminator(Some(b'\n'));
    if let Ok(matcher) = builder.build(text) {
        return Some((matcher, false));
    }
    builder.fixed_strings(true).build(text).ok().map(|matcher| (matcher, true))
}

/// The matching lines of one file (none for a binary or unreadable one). Reading stops as
/// soon as the search is stopped.
fn search_file(searcher: &mut Searcher, matcher: &RegexMatcher, path: &Path, stop: &Arc<AtomicBool>) -> Vec<Hit> {
    let mut hits = Vec::new();
    let Ok(file) = std::fs::File::open(path) else { return hits };
    let _ = searcher.search_reader(
        matcher,
        Stoppable { inner: file, stop: stop.clone() },
        Lossy(|number, line| {
            let line = line.trim_end_matches(['\n', '\r']);
            let mut ranges = Vec::new();
            let _ = matcher.find_iter(line.as_bytes(), |m| {
                ranges.push((m.start(), m.end()));
                true
            });
            hits.push(shorten(number, line, ranges));
            Ok(hits.len() < MAX_LINES_PER_FILE && !stop.load(Ordering::Relaxed))
        }),
    );
    hits
}

/// A long line is cut around its first match, marked with "…" (cut first: a minified file's
/// line can be megabytes). Control characters then become spaces, as many as their bytes,
/// so the match ranges stay right.
fn shorten(number: u64, line: &str, ranges: Vec<(usize, usize)>) -> Hit {
    let short = line.len() <= LINE_CHARS || line.chars().nth(LINE_CHARS).is_none();
    let (from, to) = if short {
        (0, line.len())
    } else {
        // About 40 characters before the first match, LINE_CHARS from there.
        let mut first = ranges.first().map_or(0, |r| r.0).min(line.len());
        while !line.is_char_boundary(first) {
            first -= 1;
        }
        let from = line[..first].char_indices().rev().nth(39).map_or(0, |(i, _)| i);
        let to = line[from..].char_indices().nth(LINE_CHARS).map_or(line.len(), |(i, _)| from + i);
        (from, to)
    };
    let lead = if from > 0 { "…" } else { "" };
    let tail = if to < line.len() { "…" } else { "" };
    let mut text = String::with_capacity(to - from + lead.len() + tail.len());
    text.push_str(lead);
    for c in line[from..to].chars() {
        if c.is_control() {
            text.extend(std::iter::repeat_n(' ', c.len_utf8()));
        } else {
            text.push(c);
        }
    }
    text.push_str(tail);
    let shift = |b: usize| b.clamp(from, to) - from + lead.len();
    let ranges = ranges.iter().filter(|r| r.1 > from && r.0 < to).map(|r| (shift(r.0), shift(r.1))).collect();
    Hit { number, text, ranges }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::time::{Duration, Instant};

    /// Runs a search to the end: (relative path, line numbers) sorted by path.
    fn run(root: &Path, text: &str, names: &str, everything: bool) -> (Vec<(String, Vec<u64>)>, bool) {
        let query = Query { text: text.into(), names: names.into(), everything };
        let search = Search::start(root, &query).expect("something to search for");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut found = Vec::new();
        loop {
            while let Ok(f) = search.found.try_recv() {
                let name = f.path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                found.push((name, f.hits.iter().map(|h| h.number).collect()));
            }
            if search.is_done() && search.found.try_recv().is_err() {
                break;
            }
            assert!(Instant::now() < deadline, "search did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        found.sort();
        (found, search.literal)
    }

    fn sample() -> TempDir {
        let tmp = TempDir::new("find");
        tmp.file("src/main.rs", "use std::io;\nfn main() {\n\tprintln!(\"hi\");\n}\n");
        tmp.file("notes.txt", "Main idea\nthe main point\n");
        tmp.file(".gitignore", "build/\n");
        tmp.file("build/out.rs", "fn main() {}\n");
        tmp.file(".hidden/secret.rs", "fn main() {}\n");
        std::fs::write(tmp.path().join("data.bin"), b"main\0\x01\x02 binary").unwrap();
        tmp
    }

    #[test]
    fn finds_lines_like_ag() {
        let tmp = sample();
        let (found, literal) = run(tmp.path(), "main", "", false);
        // Lowercase ignores case; ignored, hidden and binary files are skipped.
        assert_eq!(found, [("notes.txt".into(), vec![1, 2]), ("src/main.rs".into(), vec![2])]);
        assert!(!literal);
        // A capital letter makes case count.
        assert_eq!(run(tmp.path(), "Main", "", false).0, [("notes.txt".into(), vec![1])]);
        // Names narrow it down.
        assert_eq!(run(tmp.path(), "main", "*.rs", false).0, [("src/main.rs".into(), vec![2])]);
        assert_eq!(run(tmp.path(), "main", "!*.rs", false).0, [("notes.txt".into(), vec![1, 2])]);
    }

    #[test]
    fn everything_includes_ignored_and_hidden_but_not_binary() {
        let tmp = sample();
        let names: Vec<String> = run(tmp.path(), "fn main", "", true).0.into_iter().map(|f| f.0).collect();
        assert_eq!(names, [".hidden/secret.rs", "build/out.rs", "src/main.rs"]);
    }

    /// `!dir` leaves the folder out with everything in it, as `rg -g '!dir'` does; a name
    /// search counts every name it looked at.
    #[test]
    fn excluding_a_folder_leaves_out_its_contents() {
        let tmp = sample();
        tmp.file("tests/main_test.rs", "fn main() {}\n");
        let (found, _) = run(tmp.path(), "main", "!tests", false);
        assert!(found.iter().all(|(name, _)| !name.starts_with("tests")), "{found:?}");
        let names: Vec<String> = run(tmp.path(), "", "!src *.rs", false).0.into_iter().map(|f| f.0).collect();
        assert_eq!(names, ["tests/main_test.rs"]);
        let search = Search::start(tmp.path(), &Query { names: "*.rs".into(), ..Query::default() }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !search.is_done() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(search.searched() > 2, "all names looked at, not just the {} found", search.searched());
    }

    /// A long line is cut around its first match, the match marks still on it.
    #[test]
    fn very_long_lines_and_wide_control_characters() {
        let line = format!("{}needle{}", "é".repeat(5000), "x\u{85}y".repeat(5000));
        let start = line.find("needle").unwrap();
        let hit = shorten(7, &line, vec![(start, start + 6)]);
        assert!(hit.text.starts_with('…') && hit.text.ends_with('…'));
        // LINE_CHARS characters of the line (a two-byte control character shows as two spaces).
        assert!(hit.text.chars().count() <= 2 * LINE_CHARS + 2);
        let (a, b) = hit.ranges[0];
        assert_eq!(&hit.text[a..b], "needle");
        // A two-byte control character (U+0085) becomes two spaces: later ranges stay right.
        let short = shorten(1, "a\u{85}b needle", vec![(5, 11)]);
        assert_eq!(&short.text[5..11], "needle");
    }

    #[test]
    fn a_broken_regex_is_searched_as_typed() {
        let tmp = sample();
        let (found, literal) = run(tmp.path(), "main(", "", false);
        assert!(literal);
        assert_eq!(found, [("src/main.rs".into(), vec![2])]);
    }

    #[test]
    fn finds_names_alone() {
        let tmp = sample();
        let (found, _) = run(tmp.path(), "", "main", false);
        assert_eq!(found, [("src/main.rs".into(), vec![])]);
        let (found, _) = run(tmp.path(), "", "s*", false);
        assert_eq!(found.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), ["src"], "folders too");
    }

    #[test]
    fn long_lines_are_cut_around_the_match() {
        let line = format!("{}needle{}", "a".repeat(1000), "b".repeat(1000));
        let hit = shorten(7, &line, vec![(1000, 1006)]);
        assert!(hit.text.starts_with('…') && hit.text.ends_with('…'));
        let (start, end) = hit.ranges[0];
        assert_eq!(&hit.text[start..end], "needle");
        let tabs = shorten(1, "\tx\ty", vec![(3, 4)]);
        assert_eq!((tabs.text.as_str(), tabs.ranges[0]), (" x y", (3, 4)));
    }
}
