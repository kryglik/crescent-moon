//! Ctrl+E: the places one keeps going back to in a git repository, kept per repository in
//! `places.ini` beside the settings. A place is a path from the repository's top, so it is
//! the same in every worktree of it; the repository goes by the address of its remote, so it
//! is the same in every clone of it too.
//!
//! The menu adds and forgets places a line at a time (Ins, Del), and F4 opens the file in the
//! editor: whatever the user wrote there, comments too, stays as it was. A `*` in a path stands
//! for any one folder, and makes a place of each folder that has the rest: one entry for each
//! package of a workspace, say. Places that aren't there (a worktree on an older branch) are left out.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::git::Repo;
use crate::util;

/// What the file says first, when rmc makes it.
const HEADER: &str = "\
; rmc's places in git repositories: Ctrl+E lists them for the repository the panel is in.
; A section per repository: the address of its remote (origin), or where it is when it has
; none. A place is `key = label | path`, the path from the top of the repository with /, the
; same in all its worktrees. A path to a file puts the cursor on it, so F4 edits it.
; A * in the path stands for any one folder: each folder that has the rest is a place, with *
; in the label as its name, and the keys counting on from a digit.
; In the menu Ins adds the folder the panel is in (or the file under the cursor) and Del
; forgets a place. r is the repository's top, always there.
";

/// A place in the menu.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    /// Pressed in the menu, it goes there at once; None past the ninth of a `*` line.
    pub key: Option<char>,
    pub label: String,
    /// The folder the panel goes to.
    pub dir: PathBuf,
    /// The file the cursor lands on (so F4 edits it), when there is one.
    pub file: Option<String>,
    /// The panel counts as being at this place anywhere below here: one of the folders a `*`
    /// stands for holds all of what is below it. The menu opens on that place.
    home: PathBuf,
    /// The line of `places.ini` it comes from; None for the root.
    pub line: Option<usize>,
}

impl Place {
    /// Where it goes, as shown in the menu: below the repository's top, with the file; the top
    /// itself is `.`.
    pub fn shown(&self, root: &Path) -> String {
        let target = match &self.file {
            Some(file) => self.dir.join(file),
            None => self.dir.clone(),
        };
        let below = target.strip_prefix(root).unwrap_or(&target);
        if below.as_os_str().is_empty() { ".".into() } else { below.display().to_string() }
    }
}

/// A line of a repository's section: `key = label | path`.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: char,
    pub label: String,
    /// From the repository's top, with `/`.
    pub path: String,
    /// Its line in the file, from 0.
    pub line: usize,
}

/// What a repository is known by in `places.ini`: the address of its remote origin (or of
/// the first remote, without one), the same in every clone and worktree; with no remote,
/// where its main worktree is.
pub fn identity(repo: &Repo) -> String {
    let config = fs::read_to_string(repo.common_dir.join("config")).unwrap_or_default();
    remote_address(&config).unwrap_or_else(|| {
        let main = if repo.common_dir.file_name().is_some_and(|name| name == ".git") { repo.common_dir.parent() } else { None };
        main.unwrap_or(&repo.common_dir).display().to_string()
    })
}

/// The address of remote origin in a repository's `config`, or of the first remote.
fn remote_address(config: &str) -> Option<String> {
    let mut remote: Option<String> = None;
    let mut first = None;
    for line in config.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[') {
            // [remote "origin"]
            remote = header
                .strip_prefix("remote")
                .and_then(|rest| rest.trim().strip_prefix('"'))
                .and_then(|rest| rest.split_once('"'))
                .map(|(name, _)| name.to_string());
            continue;
        }
        let Some(name) = &remote else { continue };
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim().eq_ignore_ascii_case("url") {
            let url = value.trim().trim_matches('"').to_string();
            if name == "origin" {
                return Some(url);
            }
            first.get_or_insert(url);
        }
    }
    first
}

/// `places.ini`, as lines of text: rmc changes it a line at a time, so what the user wrote in it
/// stays as it was.
pub struct File {
    path: Option<PathBuf>,
    lines: Vec<String>,
}

impl File {
    /// `%APPDATA%\rmc\places.ini` on Windows, `~/.config/rmc/places.ini` elsewhere.
    pub fn default_path() -> Option<PathBuf> {
        util::config_dir().map(|dir| dir.join("places.ini"))
    }

    /// The file at `path`; nothing yet when there is none.
    pub fn load(path: Option<PathBuf>) -> File {
        let text = path.as_deref().and_then(|path| util::read_text(path).ok()).unwrap_or_default();
        File { path, lines: text.lines().map(String::from).collect() }
    }

    /// The section of repository `repo`: its header's line, and where it ends.
    fn section(&self, repo: &str) -> Option<(usize, usize)> {
        let header = |line: &str| {
            let line = line.trim();
            line.strip_prefix('[').and_then(|rest| rest.rfind(']').map(|end| rest[..end].trim().to_string()))
        };
        let start = self.lines.iter().position(|line| header(line).as_deref() == Some(repo))?;
        let end = self.lines.iter().skip(start + 1).position(|line| header(line).is_some()).map_or(self.lines.len(), |n| start + 1 + n);
        Some((start, end))
    }

    /// The places of repository `repo`, and what is wrong in its lines ("line 4: …").
    pub fn entries(&self, repo: &str) -> (Vec<Entry>, Vec<String>) {
        let (mut entries, mut problems) = (Vec::new(), Vec::new());
        let Some((start, end)) = self.section(repo) else { return (entries, problems) };
        for (at, line) in self.lines.iter().enumerate().take(end).skip(start + 1) {
            let text = line.trim();
            if text.is_empty() || text.starts_with([';', '#']) {
                continue;
            }
            match parse_entry(text, &entries) {
                Ok((key, label, path)) => entries.push(Entry { key, label, path, line: at }),
                Err(why) => problems.push(format!("line {}: {why}", at + 1)),
            }
        }
        (entries, problems)
    }

    /// Adds a place to repository `repo`'s section (made at the end if it has none), under the
    /// first key of 1-9 and a-z that neither a line nor the menu (`taken`: those a `*` line
    /// counts on to) has. Returns that key, or None with all of them taken.
    pub fn add(&mut self, repo: &str, label: &str, path: &str, taken: &[char]) -> Option<char> {
        let (entries, _) = self.entries(repo);
        let free = |k: &char| *k != 'r' && !taken.contains(k) && entries.iter().all(|e| e.key != *k);
        let key = ('1'..='9').chain('a'..='z').find(free)?;
        let line = format!("{key} = {label} | {path}");
        match self.section(repo) {
            Some((start, end)) => {
                // After the section's last line that says something, so a blank line before the
                // next section stays where it is.
                let last = (start..end).rev().find(|&i| !self.lines[i].trim().is_empty()).unwrap_or(start);
                self.lines.insert(last + 1, line);
            }
            None => {
                self.start_section(repo);
                self.lines.push(line);
            }
        }
        Some(key)
    }

    /// A new section for `repo` at the end, the header first in a new file.
    fn start_section(&mut self, repo: &str) {
        if self.lines.is_empty() {
            self.lines.extend(HEADER.lines().map(String::from));
        }
        if self.lines.last().is_some_and(|line| !line.trim().is_empty()) {
            self.lines.push(String::new());
        }
        self.lines.push(format!("[{repo}]"));
    }

    /// Forgets the place on line `line`.
    pub fn remove(&mut self, line: usize) {
        if line < self.lines.len() {
            self.lines.remove(line);
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let mut text = self.lines.join("\n");
        text.push('\n');
        util::write_atomically(path, &text, false)
    }

    /// For F4: the file, with a section for `repo` in it, and the line (from 1) where that is.
    pub fn for_editing(&mut self, repo: &str) -> io::Result<(PathBuf, u64)> {
        let Some(path) = self.path.clone() else { return Err(io::Error::other("rmc has no folder for its settings here")) };
        if self.section(repo).is_none() {
            self.start_section(repo);
            self.save()?;
        }
        let start = self.section(repo).map_or(0, |(start, _)| start);
        Ok((path, start as u64 + 1))
    }
}

/// `key = label | path`, the key not taken by an earlier line.
fn parse_entry(text: &str, earlier: &[Entry]) -> Result<(char, String, String), String> {
    let (key, rest) = text.split_once('=').ok_or("not `key = label | path`")?;
    let mut chars = key.trim().chars();
    let key = match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => c.to_ascii_lowercase(),
        _ => return Err(format!("the key is one letter or digit, not `{}`", key.trim())),
    };
    if key == 'r' {
        return Err("r is the repository's top already".into());
    }
    if earlier.iter().any(|e| e.key == key) {
        return Err(format!("{key} is taken by an earlier line"));
    }
    let (label, path) = rest.split_once('|').ok_or("no `|` between the label and the path")?;
    let path = path.trim().replace('\\', "/");
    let parts: Vec<&str> = path.split('/').collect();
    if path.starts_with('/') || path.get(1..2) == Some(":") || parts.contains(&"..") {
        return Err(format!("{path} isn't inside the repository"));
    }
    if parts.iter().filter(|part| **part == "*").count() > 1 {
        return Err("one * in a path at most".into());
    }
    Ok((key, label.trim().to_string(), path))
}

/// The menu for the repository at `root`: the top first, then each place of `entries` that is
/// there, those of a `*` line one per folder that has the rest.
pub fn places(root: &Path, entries: &[Entry]) -> Vec<Place> {
    let mut places = vec![Place {
        key: Some('r'),
        label: "Repository top".into(),
        dir: root.to_path_buf(),
        file: None,
        home: root.to_path_buf(),
        line: None,
    }];
    for entry in entries {
        places.extend(expand(root, entry));
    }
    places
}

fn expand(root: &Path, entry: &Entry) -> Vec<Place> {
    let parts: Vec<&str> = entry.path.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
    let Some(star) = parts.iter().position(|part| *part == "*") else {
        return place_at(under(root, &parts), None, entry.key.into(), entry.label.clone(), entry.line).into_iter().collect();
    };
    let above = under(root, &parts[..star]);
    let rest = &parts[star + 1..];
    let mut names: Vec<String> = fs::read_dir(&above)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|item| item.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|item| item.file_name().to_str().map(String::from))
        .filter(|name| fs::metadata(under(&above.join(name), rest)).is_ok())
        .collect();
    names.sort_by(|a, b| util::natural_cmp(a, b));
    names
        .iter()
        .enumerate()
        .filter_map(|(i, name)| {
            // From a digit the keys count on; a letter is the first one's only.
            let key = match entry.key.to_digit(10) {
                Some(digit) => char::from_digit(digit + i as u32, 10).filter(|_| digit + (i as u32) <= 9),
                None => (i == 0).then_some(entry.key),
            };
            let label = if entry.label.contains('*') { entry.label.replace('*', name) } else { format!("{} {name}", entry.label) };
            let home = above.join(name);
            place_at(under(&home, rest), Some(home), key, label, entry.line)
        })
        .collect()
}

/// A place for `target`, if it is there: a folder to go to, or a file to put the cursor on.
fn place_at(target: PathBuf, home: Option<PathBuf>, key: Option<char>, label: String, line: usize) -> Option<Place> {
    let meta = fs::metadata(&target).ok()?;
    let (dir, file) = if meta.is_dir() {
        (target, None)
    } else {
        (target.parent()?.to_path_buf(), Some(target.file_name()?.to_string_lossy().into_owned()))
    };
    Some(Place { key, label, home: home.unwrap_or_else(|| dir.clone()), dir, file, line: Some(line) })
}

/// The place a panel in `cwd` is at (the innermost), so the menu opens on it. Elsewhere the
/// first, the top.
pub fn nearest(places: &[Place], cwd: &Path) -> usize {
    places
        .iter()
        .enumerate()
        .filter(|(_, place)| util::is_inside(cwd, &place.home))
        .max_by_key(|(_, place)| place.home.components().count())
        .map_or(0, |(i, _)| i)
}

/// What Ins adds for a panel in `cwd` with `file` under the cursor: its label and its path
/// from the top at `root`. None at the top itself, which is always there.
pub fn new_place(root: &Path, cwd: &Path, file: Option<&str>) -> Option<(String, String)> {
    let below = cwd.strip_prefix(root).ok()?;
    let mut parts: Vec<String> = below.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    parts.extend(file.map(String::from));
    let label = parts.last()?.clone();
    Some((label, parts.join("/")))
}

/// `root` with `parts` below it, joined one by one so the separators are the system's own.
fn under(root: &Path, parts: &[&str]) -> PathBuf {
    parts.iter().fold(root.to_path_buf(), |path, part| path.join(part))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const ADDRESS: &str = "git@example.com:team/workspace.git";

    /// A repository in `tmp` with a remote, some packages, and a file of places for it: the
    /// packages by a `*`, a folder and a file.
    pub(crate) fn fake_workspace(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let root = tmp.dir("workspace");
        crate::git::tests::fake_repo(&root, "main");
        tmp.file("workspace/.git/config", &format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {ADDRESS}\n"));
        tmp.file("workspace/packages/app/package.json", "{}");
        tmp.file("workspace/packages/blog/package.json", "{}");
        tmp.dir("workspace/packages/app/src/components");
        tmp.dir("workspace/packages/shared");
        tmp.dir("workspace/node_modules/react");
        tmp.dir("workspace/scripts");
        tmp.file("workspace/docs/guide.md", "");
        let file = tmp.file(
            "places.ini",
            &format!(
                "; mine\n[{ADDRESS}]\n1 = * package | packages/*/package.json\ns = Scripts | scripts\n; the guide\nd = Docs | docs/guide.md\n\n[elsewhere]\n1 = Other | x\n"
            ),
        );
        (root, file)
    }

    #[test]
    fn a_repository_goes_by_its_remote() {
        let tmp = TempDir::new("places-identity");
        let (root, _) = fake_workspace(&tmp);
        let repo = Repo::find(&root.join("packages")).unwrap();
        assert_eq!(identity(&repo), ADDRESS);
        assert_eq!(remote_address("[remote \"upstream\"]\n url = a\n[remote \"origin\"]\n url = b\n"), Some("b".into()));
        assert_eq!(remote_address("[remote \"upstream\"]\n url = a\n"), Some("a".into()), "the first one without origin");
        fs::write(root.join(".git/config"), "[core]\n").unwrap();
        assert_eq!(identity(&repo), root.display().to_string(), "with none, where it is");
    }

    #[test]
    fn the_top_then_each_place_that_is_there() {
        let tmp = TempDir::new("places-list");
        let (root, file) = fake_workspace(&tmp);
        tmp.file("workspace/packages/cli/package.json", "");
        let (entries, problems) = File::load(Some(file)).entries(ADDRESS);
        assert!(problems.is_empty(), "{problems:?}");
        let places = places(&root, &entries);
        let rows: Vec<(Option<char>, &str, String)> =
            places.iter().map(|p| (p.key, p.label.as_str(), p.shown(&root).replace('\\', "/"))).collect();
        assert_eq!(
            rows,
            [
                (Some('r'), "Repository top", ".".to_string()),
                (Some('1'), "app package", "packages/app/package.json".to_string()),
                (Some('2'), "blog package", "packages/blog/package.json".to_string()),
                (Some('3'), "cli package", "packages/cli/package.json".to_string()),
                (Some('s'), "Scripts", "scripts".to_string()),
                (Some('d'), "Docs", "docs/guide.md".to_string()),
            ]
        );
        // A worktree on an older branch: what isn't there is left out.
        fs::remove_dir_all(root.join("docs")).unwrap();
        let labels: Vec<String> = super::places(&root, &entries).into_iter().map(|p| p.label).collect();
        assert!(!labels.contains(&"Docs".to_string()));
    }

    #[test]
    fn the_menu_opens_on_the_place_the_panel_is_at() {
        let tmp = TempDir::new("places-nearest");
        let (root, file) = fake_workspace(&tmp);
        let (entries, _) = File::load(Some(file)).entries(ADDRESS);
        let places = places(&root, &entries);
        let at = |relative: &str| places[nearest(&places, &root.join(relative))].label.clone();
        assert_eq!(at("packages/app/src/components"), "app package", "anywhere in app");
        assert_eq!(at("docs"), "Docs");
        assert_eq!(at("node_modules"), "Repository top");
    }

    #[test]
    fn wrong_lines_are_said_and_the_rest_works() {
        let tmp = TempDir::new("places-wrong");
        let file = tmp.file(
            "places.ini",
            "[repo]\nab = Two letters | x\nr = Top | x\n1 = One | x\n1 = Again | y\n2 = No bar\n3 = Out | ../x\n4 = Fine | y\n",
        );
        let (entries, problems) = File::load(Some(file)).entries("repo");
        assert_eq!(entries.iter().map(|e| e.key).collect::<String>(), "14");
        assert_eq!(problems.len(), 5, "{problems:?}");
        assert!(problems[0].starts_with("line 2:"));
    }

    /// Ins and Del change a line each: comments, other repositories and blank lines stay.
    #[test]
    fn adding_and_forgetting_keep_the_rest_of_the_file() {
        let tmp = TempDir::new("places-edit");
        let (_, path) = fake_workspace(&tmp);
        let mut file = File::load(Some(path.clone()));
        assert_eq!(file.add(ADDRESS, "src", "packages/app/src", &['1', '2']), Some('3'));
        file.save().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("d = Docs | docs/guide.md\n3 = src | packages/app/src\n\n[elsewhere]"), "{text}");
        assert!(text.starts_with("; mine\n") && text.contains("; the guide\n"));

        let mut file = File::load(Some(path.clone()));
        let (entries, _) = file.entries(ADDRESS);
        let scripts = entries.iter().find(|e| e.key == 's').unwrap().line;
        file.remove(scripts);
        file.save().unwrap();
        let (entries, _) = File::load(Some(path.clone())).entries(ADDRESS);
        assert_eq!(entries.iter().map(|e| e.key).collect::<String>(), "1d3");

        // A repository with no section yet gets one at the end; a new file gets the header.
        let mut fresh = File::load(Some(tmp.path().join("new.ini")));
        assert_eq!(fresh.add("other", "docs", "docs", &[]), Some('1'));
        fresh.save().unwrap();
        let text = fs::read_to_string(tmp.path().join("new.ini")).unwrap();
        assert!(text.starts_with("; rmc's places") && text.ends_with("\n[other]\n1 = docs | docs\n"), "{text}");
    }

    #[test]
    fn f4_opens_the_file_on_the_repositorys_section() {
        let tmp = TempDir::new("places-f4");
        let (_, path) = fake_workspace(&tmp);
        let mut file = File::load(Some(path.clone()));
        assert_eq!(file.for_editing(ADDRESS).unwrap(), (path.clone(), 2));
        let (_, line) = file.for_editing("new one").unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("[new one]"));
        assert_eq!(fs::read_to_string(&path).unwrap().lines().nth(line as usize - 1), Some("[new one]"));
    }

    #[test]
    fn what_ins_adds() {
        let root = Path::new("repo");
        assert_eq!(new_place(root, &root.join("a").join("b"), None), Some(("b".into(), "a/b".into())));
        assert_eq!(new_place(root, &root.join("a"), Some("x.txt")), Some(("x.txt".into(), "a/x.txt".into())));
        assert_eq!(new_place(root, root, None), None, "the top is always there");
    }
}
