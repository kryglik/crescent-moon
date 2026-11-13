//! Git in the panels: the branch of the repository a panel's folder is in, how its files stand
//! (changed, new, ignored), and the repository's worktrees (Alt+W).
//!
//! The branch, an operation under way (a rebase, a merge) and the worktrees come from the
//! repository's own files, so they show the moment a panel enters a repository, with or
//! without git installed. How the files stand comes from `git status`, run on a thread of its
//! own (it can take a while in a big repository or on a network drive) whenever something may
//! have changed: cm reloaded the panels, files in the repository changed (watched), or some
//! time passed.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::util;
use crate::watch::Watch;

/// What HEAD points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    /// A commit rather than a branch (a tag or a commit checked out, a bisect): its short id.
    Detached(String),
    /// Not readable from the files (the reftable format keeps it elsewhere); `git status` says.
    Unknown,
}

/// The repository a folder is in.
#[derive(Clone, Debug, PartialEq)]
pub struct Repo {
    /// The top folder of the working tree, where `.git` is.
    pub root: PathBuf,
    /// This working tree's git folder: `.git`, or for a linked worktree the main one's
    /// `.git/worktrees/<name>`.
    pub git_dir: PathBuf,
    /// The repository's git folder, which all its worktrees share.
    pub common_dir: PathBuf,
    /// A worktree made by `git worktree add`, not the main one.
    pub linked: bool,
    pub head: Head,
    /// An operation under way, as git's own prompt names it: "REBASE 2/5", "MERGING"…
    pub state: Option<String>,
}

impl Repo {
    /// The repository `dir` is in: the nearest `.git` above it, a folder, or a file naming the
    /// git folder (in a linked worktree or a submodule). `GIT_CEILING_DIRECTORIES` is
    /// respected, as git does.
    pub fn find(dir: &Path) -> Option<Repo> {
        let ceilings: Vec<PathBuf> = std::env::var_os("GIT_CEILING_DIRECTORIES")
            .map(|value| std::env::split_paths(&value).filter(|p| !p.as_os_str().is_empty()).collect())
            .unwrap_or_default();
        for (depth, root) in dir.ancestors().enumerate() {
            if depth > 0 && ceilings.iter().any(|ceiling| util::same_path(ceiling, root)) {
                return None;
            }
            let dot_git = root.join(".git");
            let Ok(meta) = fs::metadata(&dot_git) else { continue };
            let git_dir = if meta.is_dir() {
                // A `.git` folder that isn't a repository's: git looks further up then.
                if !dot_git.join("HEAD").exists() {
                    continue;
                }
                dot_git
            } else {
                // A file that names a git folder that isn't there: git gives up.
                let text = fs::read_to_string(&dot_git).ok()?;
                let git_dir = native_path(text.lines().next()?.strip_prefix("gitdir:")?.trim(), root);
                if !git_dir.join("HEAD").exists() {
                    return None;
                }
                git_dir
            };
            let common_dir = fs::read_to_string(git_dir.join("commondir"))
                .map(|text| native_path(text.trim(), &git_dir))
                .unwrap_or_else(|_| git_dir.clone());
            let (head, state) = head_of(&git_dir);
            let linked = common_dir != git_dir;
            return Some(Repo { root: root.to_path_buf(), git_dir, common_dir, linked, head, state });
        }
        None
    }

    /// Reads HEAD and any operation under way again (a checkout changes them).
    #[cfg(test)]
    pub fn reload(&mut self) {
        (self.head, self.state) = head_of(&self.git_dir);
    }

    /// `path`, which is inside the working tree, as git names it: relative to the top, with `/`.
    pub fn relative(&self, path: &Path) -> Option<String> {
        let rest = path.strip_prefix(&self.root).ok()?;
        let parts: Vec<_> = rest.components().map(|c| c.as_os_str().to_string_lossy()).collect();
        Some(parts.join("/"))
    }

    /// The repository's working trees: the main one first, then the linked ones, the most
    /// recently used first.
    pub fn worktrees(&self) -> Vec<Worktree> {
        let mut list = Vec::new();
        // The main one is where the repository's `.git` folder is. A bare repository (`x.git`)
        // has none, only linked ones.
        let main = if self.linked {
            (self.common_dir.file_name().is_some_and(|name| name == ".git")).then(|| self.common_dir.parent()).flatten()
        } else {
            Some(self.root.as_path())
        };
        if let Some(main) = main {
            let (head, state) = head_of(&self.common_dir);
            let used = last_used(&self.common_dir);
            list.push(Worktree { path: main.to_path_buf(), head, state, main: true, locked: false, missing: !main.is_dir(), used });
        }
        let mut linked: Vec<Worktree> = fs::read_dir(self.common_dir.join("worktrees"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let admin = entry.path();
                let dot_git = native_path(fs::read_to_string(admin.join("gitdir")).ok()?.trim(), &admin);
                let path = dot_git.parent()?.to_path_buf();
                let (head, state) = head_of(&admin);
                Some(Worktree {
                    missing: !path.is_dir(),
                    locked: admin.join("locked").exists(),
                    used: last_used(&admin),
                    path,
                    head,
                    state,
                    main: false,
                })
            })
            .collect();
        linked.sort_by(|a, b| b.used.cmp(&a.used).then_with(|| a.path.cmp(&b.path)));
        list.extend(linked);
        list
    }
}

/// A branch, as Alt+B lists them.
#[derive(Clone, Debug, PartialEq)]
pub struct Branch {
    /// `main`, or for a branch only on a remote, `origin/feature`.
    pub name: String,
    /// Only on a remote: switching to it makes a branch here that follows it.
    pub remote: bool,
    /// When its newest commit was made.
    pub when: Option<SystemTime>,
    /// How it stands against the branch it follows: `↑1 ↓2`, `gone`, or nothing.
    pub track: String,
    /// The worktree that has it checked out.
    pub worktree: Option<PathBuf>,
    /// The first line of its newest commit.
    pub subject: String,
}

/// What `git for-each-ref` says of each branch, one per line.
const BRANCH_FORMAT: &str =
    "--format=%(refname)%00%(committerdate:unix)%00%(upstream:short)%00%(upstream:track,nobracket)%00%(worktreepath)%00%(subject)";

impl Branch {
    /// The branches in `output` of `git for-each-ref` with `BRANCH_FORMAT`, newest first: the
    /// ones here, and those of a remote that none here has by name or follows. Paths are read as
    /// seen from `base`.
    fn parse(output: &[u8], base: &Path) -> Vec<Branch> {
        let text = String::from_utf8_lossy(output);
        let (mut here, mut there) = (Vec::new(), Vec::new());
        let mut followed = HashSet::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.split('\0').collect();
            let [refname, date, upstream, track, worktree, subject] = fields[..] else { continue };
            let mut branch = Branch {
                name: String::new(),
                remote: false,
                when: date.parse().ok().map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
                track: track_text(track),
                worktree: (!worktree.is_empty()).then(|| native_path(worktree, base)),
                subject: subject.to_string(),
            };
            if let Some(name) = refname.strip_prefix("refs/heads/") {
                if !upstream.is_empty() {
                    followed.insert(upstream.to_string());
                }
                branch.name = name.to_string();
                here.push(branch);
            } else if let Some(name) = refname.strip_prefix("refs/remotes/").filter(|name| !name.ends_with("/HEAD")) {
                (branch.name, branch.remote, branch.track, branch.worktree) = (name.to_string(), true, String::new(), None);
                there.push(branch);
            }
        }
        let names: HashSet<&str> = here.iter().map(|b| b.name.as_str()).collect();
        there.retain(|b| !followed.contains(&b.name) && !b.name.split_once('/').is_some_and(|(_, short)| names.contains(short)));
        let mut all: Vec<Branch> = here.into_iter().chain(there).collect();
        all.sort_by(|a, b| b.when.cmp(&a.when));
        all
    }
}

/// `ahead 1, behind 2` as the git row shows it: `↑1 ↓2`.
fn track_text(track: &str) -> String {
    track
        .split(", ")
        .filter_map(|part| match part.split_once(' ') {
            Some(("ahead", n)) => Some(format!("↑{n}")),
            Some(("behind", n)) => Some(format!("↓{n}")),
            _ if part == "gone" => Some("gone".to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A git command in the working tree at `root`: in the repository asked about, whatever cm
/// was started with, and never asking anything.
fn git_at(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(root);
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY"] {
        command.env_remove(name);
    }
    command.env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null());
    command
}

/// What git said on its way out, for the user: its first lines.
fn said(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|line| !line.is_empty()).take(8).collect();
    lines.join("\n")
}

/// The branches of the repository at `root`, newest first. git lists them on a thread of its
/// own, as a share that has gone away takes its time here too; the answer comes on the channel.
pub fn branches(root: &Path) -> Receiver<Result<Vec<Branch>, String>> {
    let (tx, answer) = mpsc::channel();
    let root = root.to_path_buf();
    thread::spawn(move || {
        let mut command = git_at(&root);
        command.args(["--no-optional-locks", "for-each-ref", "--sort=-committerdate", BRANCH_FORMAT, "refs/heads", "refs/remotes"]);
        let _ = tx.send(match command.output() {
            Ok(output) if output.status.success() => Ok(Branch::parse(&output.stdout, &root)),
            Ok(output) => Err(said(&output.stderr)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("git isn't installed".into()),
            Err(e) => Err(e.to_string()),
        });
    });
    answer
}

/// Switches the worktree at `root` to `branch`; one only on a remote gets a branch here that
/// follows it. On a thread of its own, and never stopped: a checkout stopped halfway would
/// leave the files of neither branch. What git said comes on the channel.
pub fn switch(root: &Path, branch: &Branch) -> Receiver<Result<String, String>> {
    let (tx, answer) = mpsc::channel();
    let mut command = git_at(root);
    command.arg("switch");
    if branch.remote {
        command.arg("--track");
    }
    command.arg(&branch.name);
    thread::spawn(move || {
        let _ = tx.send(match command.output() {
            Ok(output) if output.status.success() => Ok(said(&output.stderr)),
            Ok(output) => Err(said(&output.stderr)),
            Err(e) => Err(e.to_string()),
        });
    });
    answer
}

/// One of a repository's working trees, as `git worktree list` shows them.
#[derive(Clone, Debug, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Head,
    pub state: Option<String>,
    /// The main working tree, the one holding the repository's `.git` folder.
    pub main: bool,
    /// `git worktree lock`ed (kept even when its folder can't be seen).
    pub locked: bool,
    /// Its folder is gone; `git worktree prune` forgets it.
    pub missing: bool,
    /// When it was last worked in, roughly: when its HEAD, index or HEAD's log last changed.
    pub used: Option<SystemTime>,
}

impl Head {
    /// The branch, or `(1a2b3c4)` for a commit.
    pub fn label(&self) -> String {
        match self {
            Head::Branch(name) => name.clone(),
            Head::Detached(id) => format!("({id})"),
            Head::Unknown => "?".into(),
        }
    }
}

/// HEAD of the working tree whose git folder is `git_dir`, and an operation under way there,
/// worked out from the files as git's own prompt (git-prompt.sh) does.
fn head_of(git_dir: &Path) -> (Head, Option<String>) {
    let mut head = read_head(git_dir);
    let read = |path: PathBuf| fs::read_to_string(path).ok().map(|text| text.trim().to_string());
    let number = |path: PathBuf| read(path).and_then(|text| text.parse::<u32>().ok());
    let progress = |what: &str, step: Option<u32>, total: Option<u32>| match (step, total) {
        (Some(step), Some(total)) => format!("{what} {step}/{total}"),
        _ => what.to_string(),
    };
    // While rebasing HEAD is detached; the branch being rebased is named in the rebase folder.
    let mut branch_from = |file: PathBuf| {
        if let Some(name) = read(file).as_deref().and_then(|name| name.strip_prefix("refs/heads/")) {
            head = Head::Branch(name.to_string());
        }
    };
    let merge = git_dir.join("rebase-merge");
    let apply = git_dir.join("rebase-apply");
    let state = if merge.is_dir() {
        branch_from(merge.join("head-name"));
        Some(progress("REBASE", number(merge.join("msgnum")), number(merge.join("end"))))
    } else if apply.is_dir() {
        let what = if apply.join("rebasing").exists() {
            branch_from(apply.join("head-name"));
            "REBASE"
        } else if apply.join("applying").exists() {
            "AM"
        } else {
            "AM/REBASE"
        };
        Some(progress(what, number(apply.join("next")), number(apply.join("last"))))
    } else if git_dir.join("MERGE_HEAD").exists() {
        Some("MERGING".into())
    } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
        Some("CHERRY-PICKING".into())
    } else if git_dir.join("REVERT_HEAD").exists() {
        Some("REVERTING".into())
    } else if git_dir.join("BISECT_LOG").exists() {
        Some("BISECTING".into())
    } else {
        None
    };
    (head, state)
}

/// HEAD's branch, or the commit it is on.
fn read_head(git_dir: &Path) -> Head {
    let path = git_dir.join("HEAD");
    // Very old gits made HEAD a link to the branch's file.
    if let Ok(target) = fs::read_link(&path)
        && let Some(name) = target.to_str().and_then(|target| target.strip_prefix("refs/heads/"))
    {
        return Head::Branch(name.to_string());
    }
    let Ok(text) = fs::read_to_string(&path) else { return Head::Unknown };
    let text = text.trim();
    match text.strip_prefix("ref:").map(str::trim) {
        // The reftable format keeps the real HEAD elsewhere and leaves this in the file.
        Some("refs/heads/.invalid") => Head::Unknown,
        Some(target) => Head::Branch(target.strip_prefix("refs/heads/").unwrap_or(target).to_string()),
        None if text.len() >= 7 && text.bytes().all(|b| b.is_ascii_hexdigit()) => Head::Detached(text[..7].to_string()),
        None => Head::Unknown,
    }
}

/// The newest of the times a working tree's HEAD, index and HEAD log changed.
fn last_used(git_dir: &Path) -> Option<SystemTime> {
    ["HEAD", "index", "logs/HEAD"]
        .iter()
        .filter_map(|name| fs::metadata(git_dir.join(name)).and_then(|m| m.modified()).ok())
        .max()
}

/// A path as git writes it into its files: absolute (with `/` even on Windows: `E:/x/.git`),
/// or relative to `base`. A repository on a Windows drive used from both Windows and WSL holds
/// paths of the other system: `E:/x` is /mnt/e/x in WSL, and /mnt/e/x is E:\x on Windows.
fn native_path(text: &str, base: &Path) -> PathBuf {
    let b = text.as_bytes();
    let path = if cfg!(windows) {
        let wsl_drive = text.strip_prefix("/mnt/").filter(|rest| {
            let r = rest.as_bytes();
            !r.is_empty() && r[0].is_ascii_alphabetic() && (r.len() == 1 || r[1] == b'/')
        });
        match wsl_drive {
            Some(rest) => format!("{}:\\{}", rest[..1].to_ascii_uppercase(), rest.get(2..).unwrap_or("").replace('/', "\\")),
            None => text.replace('/', "\\"),
        }
    } else if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || matches!(b[2], b'/' | b'\\')) {
        format!("/mnt/{}/{}", (b[0] as char).to_ascii_lowercase(), text.get(3..).unwrap_or("").replace('\\', "/"))
    } else {
        text.to_string()
    };
    spelled_as(util::normalize(&base.join(path)), base)
}

/// `path` in the spelling of `base`'s folders. git writes paths with their links resolved:
/// on macOS /var and /tmp are links into /private, so a worktree in /tmp/x is /private/tmp/x
/// to git, and wouldn't match the panel's /tmp/x. The deepest folder above `base` whose real
/// path holds `path` gives the start back.
#[cfg(unix)]
fn spelled_as(path: PathBuf, base: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path;
    }
    for above in base.ancestors() {
        if path.starts_with(above) {
            return path;
        }
        let Ok(real) = fs::canonicalize(above) else { continue };
        if let Ok(rest) = path.strip_prefix(&real) {
            return above.join(rest);
        }
    }
    path
}

#[cfg(windows)]
fn spelled_as(path: PathBuf, _base: &Path) -> PathBuf {
    path
}

/// How a file stands with git, from the least to the most worth seeing: a folder shows the
/// weightiest of what is below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mark {
    /// Matched by .gitignore: dimmed.
    Ignored,
    /// Not in git (yet).
    Untracked,
    /// New, staged.
    Added,
    /// Renamed or copied, staged.
    Renamed,
    /// Changed, staged or not; deleted; its type changed.
    Modified,
    /// A merge conflict to resolve.
    Conflict,
}

/// What `git status` says about a working tree.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// Commits ahead of and behind the upstream branch; None without one.
    pub ahead_behind: Option<(u32, u32)>,
    /// Files with changes, staged or not.
    pub changed: usize,
    /// Untracked files (a new folder counts once: git lists it as a whole).
    pub untracked: usize,
    pub conflicts: usize,
    /// The branch, for when HEAD can't be read from the files (see `Head::Unknown`).
    pub branch: Option<String>,
    /// What git listed, by path (relative to the top, `/`, lower-case on Windows).
    entries: HashMap<String, Mark>,
    /// For each folder with changes below it, the weightiest of them.
    inside: HashMap<String, Mark>,
}

/// Paths as keys: lower-case on Windows, where names differing only in case are one file.
fn key(path: &str) -> String {
    if cfg!(windows) { path.to_lowercase() } else { path.to_string() }
}

impl Status {
    /// Reads the output of `git status --porcelain=v2 --branch -z`.
    pub fn parse(output: &[u8]) -> Status {
        let text = String::from_utf8_lossy(output);
        let mut status = Status::default();
        let mut records = text.split('\0');
        while let Some(record) = records.next() {
            let (kind, rest) = record.split_once(' ').unwrap_or((record, ""));
            // The fields before the path: an ordinary change has 7, a rename or copy 8 (its
            // original path follows as a record of its own), a conflict 9.
            let path_after = |fields: usize| rest.splitn(fields + 1, ' ').nth(fields);
            match kind {
                "#" => status.header(rest),
                "1" | "2" => {
                    if let Some(path) = path_after(if kind == "1" { 7 } else { 8 }) {
                        let mark = match rest.as_bytes().first() {
                            Some(b'A') => Mark::Added,
                            Some(b'R' | b'C') => Mark::Renamed,
                            _ => Mark::Modified,
                        };
                        status.changed += 1;
                        status.add(path, mark);
                    }
                    if kind == "2" {
                        records.next();
                    }
                }
                "u" => {
                    if let Some(path) = path_after(9) {
                        status.conflicts += 1;
                        status.add(path, Mark::Conflict);
                    }
                }
                "?" => {
                    status.untracked += 1;
                    status.add(rest, Mark::Untracked);
                }
                "!" => status.add(rest, Mark::Ignored),
                _ => {}
            }
        }
        status
    }

    fn header(&mut self, line: &str) {
        if let Some(counts) = line.strip_prefix("branch.ab ") {
            let mut numbers = counts.split(' ').map(|n| n.trim_start_matches(['+', '-']).parse::<u32>().unwrap_or(0));
            self.ahead_behind = Some((numbers.next().unwrap_or(0), numbers.next().unwrap_or(0)));
        } else if let Some(head) = line.strip_prefix("branch.head ") {
            self.branch = (head != "(detached)").then(|| head.to_string());
        }
    }

    fn add(&mut self, path: &str, mark: Mark) {
        // Folders git lists as a whole (untracked or ignored) end in `/`.
        let key = key(path.strip_suffix('/').unwrap_or(path));
        if key.is_empty() {
            return;
        }
        // Each folder above it learns of it (not of ignored files: they are no change).
        if mark != Mark::Ignored {
            let mut end = key.len();
            while let Some(slash) = key[..end].rfind('/') {
                end = slash;
                let folder = self.inside.entry(key[..end].to_string()).or_insert(mark);
                *folder = (*folder).max(mark);
            }
        }
        // A file deleted from the index but still on disk (`git rm --cached`) is listed as
        // deleted, then as untracked, which is what it is on disk: the later listing wins.
        self.entries.insert(key, mark);
    }

    /// How the file or folder at `path` (relative to the top, with `/`) stands, and whether
    /// that is what is inside the folder rather than the folder itself.
    pub fn mark(&self, path: &str) -> Option<(Mark, bool)> {
        let key = key(path);
        // git's own folder (or a worktree's `.git` file) is no part of the working tree.
        if key == ".git" || key.starts_with(".git/") {
            return Some((Mark::Ignored, false));
        }
        if let Some(&mark) = self.entries.get(&key) {
            return Some((mark, false));
        }
        // Inside a folder git lists as a whole, everything is as that folder is.
        let mut end = key.len();
        while let Some(slash) = key[..end].rfind('/') {
            end = slash;
            if let Some(&mark) = self.entries.get(&key[..end])
                && matches!(mark, Mark::Untracked | Mark::Ignored)
            {
                return Some((mark, false));
            }
        }
        self.inside.get(&key).map(|&mark| (mark, true))
    }
}

/// How a run ended.
enum Outcome {
    Done(Status),
    /// git refused (not a repository after all, an owner it doesn't trust…).
    Failed,
    /// It took too long (a huge working tree, a slow network drive): not asked again.
    TooSlow,
    /// git isn't installed.
    NoGit,
}

/// The longest a run may take.
const TIME_LIMIT: Duration = Duration::from_secs(20);

/// `git status` of the working tree at `root`.
fn status_of(root: &Path) -> Outcome {
    // Ignored files too (they show dimmed), in the mode that doesn't look inside ignored
    // folders: the default mode took a minute in a repository with a Python .venv. A
    // repository set not to show untracked files refuses that, so then without.
    match run_status(root, true) {
        Outcome::Failed => run_status(root, false),
        other => other,
    }
}

fn run_status(root: &Path, ignored: bool) -> Outcome {
    let mut command = git_at(root);
    // No optional locks: a status must never write the index, nor get in the way of the
    // user's own git commands.
    command.arg("--no-optional-locks").args(["status", "--porcelain=v2", "--branch", "-z"]);
    if ignored {
        command.arg("--ignored=matching");
    }
    command.env("GIT_OPTIONAL_LOCKS", "0");
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Outcome::NoGit,
        Err(_) => return Outcome::Failed,
    };
    let Some(mut stdout) = child.stdout.take() else { return Outcome::Failed };
    // Read on a thread of its own, so a stuck git can be given up on.
    let (send, output) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = send.send(bytes);
    });
    match output.recv_timeout(TIME_LIMIT) {
        Ok(bytes) => match child.wait() {
            Ok(exit) if exit.success() => Outcome::Done(Status::parse(&bytes)),
            _ => Outcome::Failed,
        },
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Outcome::TooSlow
        }
    }
}

/// Runs `git status` on a thread of its own, one working tree after another.
struct Runner {
    requests: Sender<PathBuf>,
    results: Receiver<(PathBuf, Outcome, Duration, Option<Repo>)>,
}

/// A status has arrived for the working tree at `root`; `repo` is that tree as it was read
/// afterwards, as a status can follow a checkout.
pub struct Arrived {
    pub root: PathBuf,
    pub repo: Option<Repo>,
}

impl Runner {
    fn new() -> Runner {
        let (requests, wanted) = mpsc::channel::<PathBuf>();
        let (done, results) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(root) = wanted.recv() {
                let start = Instant::now();
                let outcome = status_of(&root);
                let took = start.elapsed();
                // Here too: a share that has gone away takes its time over every file.
                let repo = Repo::find(&root);
                if done.send((root, outcome, took, repo)).is_err() {
                    break;
                }
            }
        });
        Runner { requests, results }
    }
}

/// How soon a run is wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Urgency {
    /// Files changed: soon, but not over and over while a build writes files.
    Soon,
    /// The panels were reloaded (a command ran, Ctrl+R) or the repository came into view.
    Now,
}

/// A status is kept this long after no panel shows its repository any more.
const KEEP: Duration = Duration::from_secs(60);

/// Keeps the `git status` of the working trees the panels show up to date.
pub struct Tracker {
    runner: Option<Runner>,
    repos: HashMap<PathBuf, Tracked>,
    /// The option is off, or git isn't installed.
    off: bool,
}

struct Tracked {
    status: Option<Status>,
    running: bool,
    wanted: Option<Urgency>,
    finished: Option<Instant>,
    /// How long the last run took: slow repositories are asked less often.
    took: Duration,
    /// Runs in a row that found nothing new: changes seen (a build's) and the timer then ask
    /// less often. A key press starts afresh.
    calm: u32,
    /// The working tree (on Windows one watch covers all of it) and the git folder.
    watches: Vec<Watch>,
    tree_watched: bool,
    /// A panel shows it; it is watched only then.
    shown: bool,
    /// Too slow: no more runs this session.
    given_up: bool,
    /// When a panel last showed it.
    seen: Instant,
}

impl Tracked {
    fn new(now: Instant) -> Tracked {
        Tracked {
            status: None,
            running: false,
            wanted: None,
            finished: None,
            took: Duration::ZERO,
            calm: 0,
            watches: Vec::new(),
            tree_watched: false,
            shown: false,
            given_up: false,
            seen: now,
        }
    }

    /// Starts watching: the whole working tree where that is possible (Windows, a local
    /// drive), and the git folder (a commit, a checkout, staging) unless that is inside it.
    fn watch(&mut self, repo: &Repo) {
        let tree = Watch::tree(&repo.root);
        self.tree_watched = tree.is_active();
        self.watches.clear();
        if self.tree_watched {
            self.watches.push(tree);
        }
        if !self.tree_watched || !util::is_inside(&repo.git_dir, &repo.root) {
            self.watches.push(Watch::new(&repo.git_dir));
        }
    }

    fn want(&mut self, urgency: Urgency) {
        self.wanted = self.wanted.max(Some(urgency));
    }

    /// The least time from the end of one run to the start of the next: longer for slow
    /// repositories, and for changes seen (not reloads) the longer nothing new turned up.
    fn gap(&self, urgency: Urgency) -> Duration {
        let base = (self.took * 10).max(Duration::from_millis(500));
        match urgency {
            Urgency::Now => base,
            Urgency::Soon => (base * 2u32.pow(self.calm.min(4))).min(Duration::from_secs(30)),
        }
    }

    /// How long until a look anyway, for changes no watch sees: on Linux, where only the git
    /// folder is watched, the way edits in subfolders show (within about 10 s in a small
    /// repository, twice the base once nothing changes); on Windows a safety net.
    fn every(&self) -> Duration {
        let base = if self.tree_watched { Duration::from_secs(60) } else { (self.took * 20).max(Duration::from_secs(5)) };
        (base * 2u32.pow(self.calm.min(1))).min(Duration::from_secs(300))
    }
}

impl Tracker {
    pub fn new(enabled: bool) -> Tracker {
        Tracker { runner: None, repos: HashMap::new(), off: !enabled }
    }

    /// A job starts: everything counts as out of view, so the watches close at once (an
    /// open one keeps Windows from moving a folder above it, as the repository's `.git` is
    /// below the repository). They open again when the panels show it again.
    pub fn close_watches(&mut self) {
        for tracked in self.repos.values_mut() {
            tracked.shown = false;
            tracked.watches.clear();
        }
    }

    /// The option was changed.
    pub fn set_enabled(&mut self, on: bool) {
        self.off = !on;
        if !on {
            self.repos.clear();
        }
    }

    /// `git status` runs (the option is on and git is installed): the panels keep a place
    /// for the marks.
    pub fn enabled(&self) -> bool {
        !self.off
    }

    /// The status of the working tree at `root`, once a run has brought it.
    pub fn status(&self, root: &Path) -> Option<&Status> {
        self.repos.get(root)?.status.as_ref()
    }

    /// A run is going on (the main loop then looks for its end more often).
    pub fn running(&self) -> bool {
        self.repos.values().any(|t| t.running)
    }

    /// cm reloaded the panels (a command ran, Ctrl+R, a job ended): everything is looked at
    /// again.
    pub fn refresh(&mut self) {
        for tracked in self.repos.values_mut() {
            tracked.want(Urgency::Now);
        }
    }

    /// Ctrl+R: like `refresh`, and repositories given up on as too slow get another try.
    pub fn retry(&mut self) {
        for tracked in self.repos.values_mut() {
            tracked.given_up = false;
        }
        self.refresh();
    }

    /// A panel saw files change in the working tree at `root`.
    pub fn nudge(&mut self, root: &Path) {
        if let Some(tracked) = self.repos.get_mut(root) {
            tracked.want(Urgency::Soon);
        }
    }

    /// Someone is at the keyboard: changes are looked for at the normal pace again (not in a
    /// repository where git keeps failing, which a key press doesn't mend).
    pub fn touch(&mut self) {
        for tracked in self.repos.values_mut().filter(|t| t.status.is_some()) {
            tracked.calm = 0;
        }
    }

    /// Takes the results that arrived and starts the runs that are due for `shown`, the
    /// repositories the panels show. Nothing starts while `quiet` (the shell is on screen, a
    /// job runs). Returns the working trees whose status arrived.
    pub fn tick(&mut self, shown: &[&Repo], now: Instant, quiet: bool) -> Vec<Arrived> {
        let mut arrived = Vec::new();
        if let Some(runner) = &self.runner {
            while let Ok((root, outcome, took, repo)) = runner.results.try_recv() {
                arrived.push(Arrived { root: root.clone(), repo });
                let Some(tracked) = self.repos.get_mut(&root) else { continue };
                tracked.running = false;
                tracked.finished = Some(now);
                tracked.took = took;
                // What git itself did to the files while it looked is no news.
                tracked.watches.iter_mut().for_each(Watch::forget);
                match outcome {
                    Outcome::Done(status) => {
                        tracked.calm = if tracked.status.as_ref() == Some(&status) { tracked.calm + 1 } else { 0 };
                        tracked.status = Some(status);
                    }
                    Outcome::Failed => {
                        tracked.status = None;
                        tracked.calm += 1;
                    }
                    Outcome::TooSlow => {
                        tracked.status = None;
                        tracked.given_up = true;
                    }
                    Outcome::NoGit => self.off = true,
                }
            }
        }
        if self.off {
            self.repos.clear();
            return arrived;
        }
        for repo in shown {
            let tracked = self.repos.entry(repo.root.clone()).or_insert_with(|| Tracked::new(now));
            if !tracked.shown {
                // Into view (again): watched from now on, and looked at.
                tracked.shown = true;
                tracked.watch(repo);
                tracked.want(Urgency::Now);
            }
            tracked.seen = now;
        }
        for (root, tracked) in self.repos.iter_mut() {
            if tracked.shown && !shown.iter().any(|repo| repo.root == *root) {
                // Out of view: the watches close (an open one would keep Windows from
                // deleting the folder, a worktree being removed, say).
                tracked.shown = false;
                tracked.watches.clear();
                continue;
            }
            if !tracked.shown {
                continue;
            }
            let mut changed = false;
            for watch in &mut tracked.watches {
                changed |= watch.due(now);
            }
            if changed || tracked.wanted.is_none() && tracked.finished.is_some_and(|f| now >= f + tracked.every()) {
                tracked.want(Urgency::Soon);
            }
        }
        if !quiet {
            for (root, tracked) in self.repos.iter_mut() {
                if !tracked.shown || tracked.running || tracked.given_up {
                    continue;
                }
                let Some(urgency) = tracked.wanted else { continue };
                if tracked.finished.is_none_or(|f| now >= f + tracked.gap(urgency))
                    && self.runner.get_or_insert_with(Runner::new).requests.send(root.clone()).is_ok()
                {
                    tracked.running = true;
                    tracked.wanted = None;
                }
            }
        }
        self.repos.retain(|_, t| t.shown || t.running || now.duration_since(t.seen) < KEEP);
        arrived
    }

    /// A status as if git had said it.
    #[cfg(test)]
    pub fn set_status(&mut self, root: &Path, status: Status) {
        let tracked = self.repos.entry(root.to_path_buf()).or_insert_with(|| Tracked::new(Instant::now()));
        tracked.status = Some(status);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// A repository's files, made by hand (no git needed): `.git` with HEAD on `branch`.
    pub fn fake_repo(dir: &Path, branch: &str) {
        fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        fs::write(dir.join(".git/HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    }

    /// A linked worktree of the repository at `main`, made by hand as `git worktree add` makes
    /// it: its folder `path` with a `.git` file, and its admin folder in the main one.
    pub fn fake_worktree(main: &Path, name: &str, path: &Path, branch: &str) {
        let admin = main.join(".git/worktrees").join(name);
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        let slashes = |p: &Path| p.display().to_string().replace('\\', "/");
        fs::write(admin.join("gitdir"), format!("{}/.git\n", slashes(path))).unwrap();
        fs::create_dir_all(path).unwrap();
        fs::write(path.join(".git"), format!("gitdir: {}\n", slashes(&admin))).unwrap();
    }

    pub fn git_available() -> bool {
        Command::new("git").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    /// Runs git in `dir` as a test sets a repository up: nobody's own settings, and commits
    /// dated `date` when one is given, so that they come in a known order.
    pub fn run_git(dir: &Path, args: &[&str], date: Option<&str>) {
        let mut command = Command::new("git");
        command
            .args(["-c", "user.name=cm", "-c", "user.email=cm@example.com", "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(date) = date {
            command.env("GIT_AUTHOR_DATE", date).env("GIT_COMMITTER_DATE", date);
        }
        assert!(command.status().unwrap().success(), "git {args:?}");
    }

    /// What `git for-each-ref` says becomes the list: newest first, a remote branch only when
    /// none here has it, and where each one is checked out.
    #[test]
    fn branches_are_read_newest_first() {
        let base = Path::new(if cfg!(windows) { r"E:\repo" } else { "/repo" });
        let line = |fields: [&str; 6]| fields.join("\0");
        let output = [
            line(["refs/heads/main", "100", "origin/main", "ahead 1, behind 2", "E:/repo", "first"]),
            line(["refs/heads/topic", "300", "", "", "", "newest"]),
            line(["refs/heads/gone", "50", "origin/gone", "gone", "", "old"]),
            line(["refs/remotes/origin/HEAD", "100", "", "", "", "first"]),
            line(["refs/remotes/origin/main", "100", "", "", "", "first"]),
            line(["refs/remotes/origin/topic", "290", "", "", "", "theirs"]),
            line(["refs/remotes/origin/fresh", "200", "", "", "", "from there"]),
        ]
        .join("\n");
        let branches = Branch::parse(output.as_bytes(), base);
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["topic", "origin/fresh", "main", "gone"]);
        assert_eq!(branches[2].track, "↑1 ↓2");
        assert_eq!(branches[3].track, "gone");
        assert!(branches[1].remote && !branches[0].remote);
        assert_eq!(branches[2].worktree.as_deref(), Some(Path::new(if cfg!(windows) { r"E:\repo" } else { "/mnt/e/repo" })));
        assert_eq!(branches[1].subject, "from there");
    }

    #[test]
    fn finds_the_branch_from_a_folder_deep_inside() {
        let tmp = TempDir::new("git-find");
        fake_repo(tmp.path(), "feature/login");
        let deep = tmp.dir("src/ui/widgets");
        let repo = Repo::find(&deep).unwrap();
        assert_eq!(repo.root, tmp.path());
        assert_eq!(repo.head, Head::Branch("feature/login".into()));
        assert!(!repo.linked);
        assert_eq!(repo.state, None);
        assert_eq!(repo.relative(&deep.join("button.rs")).as_deref(), Some("src/ui/widgets/button.rs"));
        // A `.git` folder that isn't a repository's doesn't count.
        let other = TempDir::new("git-none");
        other.dir(".git");
        assert_eq!(Repo::find(other.path()), None);
    }

    #[test]
    fn detached_head_and_operations_under_way() {
        let tmp = TempDir::new("git-state");
        fake_repo(tmp.path(), "main");
        let git = tmp.path().join(".git");
        fs::write(git.join("HEAD"), "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d\n").unwrap();
        let mut repo = Repo::find(tmp.path()).unwrap();
        assert_eq!(repo.head, Head::Detached("1a2b3c4".into()));
        assert_eq!(repo.head.label(), "(1a2b3c4)");

        // An interactive rebase of "topic", at its second step of five.
        tmp.file(".git/rebase-merge/head-name", "refs/heads/topic\n");
        tmp.file(".git/rebase-merge/msgnum", "2\n");
        tmp.file(".git/rebase-merge/end", "5\n");
        repo.reload();
        assert_eq!((repo.head.clone(), repo.state.as_deref()), (Head::Branch("topic".into()), Some("REBASE 2/5")));

        fs::remove_dir_all(git.join("rebase-merge")).unwrap();
        tmp.file(".git/MERGE_HEAD", "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d\n");
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        repo.reload();
        assert_eq!((repo.head.clone(), repo.state.as_deref()), (Head::Branch("main".into()), Some("MERGING")));

        // reftable leaves a placeholder in HEAD: the branch then comes from git status.
        fs::write(git.join("HEAD"), "ref: refs/heads/.invalid\n").unwrap();
        repo.reload();
        assert_eq!(repo.head, Head::Unknown);
    }

    #[test]
    fn linked_worktrees_and_the_list_of_them() {
        let tmp = TempDir::new("git-worktrees");
        let main = tmp.dir("app");
        fake_repo(&main, "main");
        let first = tmp.path().join("app-login");
        fake_worktree(&main, "app-login", &first, "login");
        let gone = tmp.path().join("app-old");
        fake_worktree(&main, "app-old", &gone, "old");
        fs::write(main.join(".git/worktrees/app-old/locked"), "on a USB stick\n").unwrap();
        fs::remove_dir_all(&gone).unwrap();
        // The one used last comes first among the linked ones.
        let earlier = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options().write(true).open(main.join(".git/worktrees/app-old/HEAD")).unwrap().set_modified(earlier).unwrap();

        let src = first.join("src");
        fs::create_dir_all(&src).unwrap();
        let repo = Repo::find(&src).unwrap();
        assert_eq!(repo.root, first);
        assert!(repo.linked);
        assert_eq!(repo.head, Head::Branch("login".into()));
        assert_eq!(repo.common_dir, main.join(".git"));
        // Its git folder is the admin folder in the main repository.
        assert_eq!(repo.git_dir, main.join(".git").join("worktrees").join("app-login"));

        let list = repo.worktrees();
        let shown: Vec<(String, bool, bool, bool)> =
            list.iter().map(|w| (w.head.label(), w.main, w.locked, w.missing)).collect();
        assert_eq!(
            shown,
            [("main".into(), true, false, false), ("login".into(), false, false, false), ("old".into(), false, true, true)]
        );
        assert_eq!(list[0].path, main);
        assert_eq!(list[1].path, first);
        // The same list from the main working tree.
        assert_eq!(Repo::find(&main).unwrap().worktrees(), list);
    }

    #[test]
    fn worktree_paths_written_relative_or_by_the_other_system() {
        let tmp = TempDir::new("git-relative");
        let main = tmp.dir("app");
        fake_repo(&main, "main");
        // Newer gits can write relative paths (worktree.useRelativePaths).
        let admin = main.join(".git/worktrees/rel");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/rel\n").unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(admin.join("gitdir"), "../../../../app-rel/.git\n").unwrap();
        tmp.file("app-rel/.git", "gitdir: ../app/.git/worktrees/rel\n");
        let repo = Repo::find(&tmp.path().join("app-rel")).unwrap();
        assert_eq!((repo.linked, repo.head.label()), (true, "rel".to_string()));
        assert_eq!(repo.worktrees()[1].path, tmp.path().join("app-rel"));

        if cfg!(windows) {
            assert_eq!(native_path("E:/devel/x/.git", Path::new(r"C:\")), PathBuf::from(r"E:\devel\x\.git"));
            assert_eq!(native_path("/mnt/e/devel/x", Path::new(r"C:\")), PathBuf::from(r"E:\devel\x"));
        } else {
            assert_eq!(native_path("E:/devel/x/.git", Path::new("/")), PathBuf::from("/mnt/e/devel/x/.git"));
            assert_eq!(native_path("/srv/x/.git", Path::new("/home")), PathBuf::from("/srv/x/.git"));
        }
    }

    /// git writes real paths; read through a link (macOS's /tmp and /var are links into
    /// /private) they come back in the link's spelling, which the panels use.
    #[cfg(unix)]
    #[test]
    fn paths_git_resolved_are_spelled_as_the_panel_does() {
        let tmp = TempDir::new("git-spelling");
        let real = fs::canonicalize(tmp.dir("real")).unwrap();
        tmp.dir("real/repo");
        tmp.dir("real/wt");
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();
        let repo = tmp.path().join("link/repo");
        assert_eq!(native_path(&real.join("wt").to_string_lossy(), &repo), tmp.path().join("link/wt"));
        assert_eq!(native_path("../wt", &repo), tmp.path().join("link/wt"), "relative: as it is");
        assert_eq!(native_path("/srv/x", &repo), PathBuf::from("/srv/x"), "elsewhere: as git says");
    }

    #[test]
    fn reads_the_status_and_marks_files_and_folders() {
        let output = [
            "# branch.oid 1a2b3c4d",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 .M N... 100644 100644 100644 aaaa aaaa src/app.rs",
            "1 A. N... 000000 100644 100644 0000 bbbb src/new file.rs",
            "2 R. N... 100644 100644 100644 cccc cccc R100 docs/guide.md",
            "docs/old guide.md",
            "u UU N... 100644 100644 100644 100644 dddd eeee ffff conflict.txt",
            "? notes.txt",
            "? drafts/",
            "! target/",
            "! .env",
            "",
        ]
        .join("\0");
        let status = Status::parse(output.as_bytes());
        assert_eq!(status.ahead_behind, Some((2, 1)));
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!((status.changed, status.untracked, status.conflicts), (3, 2, 1));
        assert_eq!(status.mark("src/app.rs"), Some((Mark::Modified, false)));
        assert_eq!(status.mark("src/new file.rs"), Some((Mark::Added, false)));
        assert_eq!(status.mark("src"), Some((Mark::Modified, true)), "the weightiest change inside");
        assert_eq!(status.mark("docs/guide.md"), Some((Mark::Renamed, false)));
        assert_eq!(status.mark("docs"), Some((Mark::Renamed, true)));
        assert_eq!(status.mark("docs/old guide.md"), None, "the old name is gone");
        assert_eq!(status.mark("conflict.txt"), Some((Mark::Conflict, false)));
        assert_eq!(status.mark("notes.txt"), Some((Mark::Untracked, false)));
        assert_eq!(status.mark("drafts"), Some((Mark::Untracked, false)));
        assert_eq!(status.mark("drafts/2024/plan.md"), Some((Mark::Untracked, false)), "inside a new folder");
        assert_eq!(status.mark("target/debug/cm.exe"), Some((Mark::Ignored, false)));
        assert_eq!(status.mark(".env"), Some((Mark::Ignored, false)));
        assert_eq!(status.mark(".git"), Some((Mark::Ignored, false)), "git's own folder");
        assert_eq!(status.mark("Cargo.toml"), None);
        if cfg!(windows) {
            assert_eq!(status.mark("SRC/App.rs"), Some((Mark::Modified, false)), "names in any case");
        }

        let clean = Status::parse(b"# branch.oid 1a2b\0# branch.head (detached)\0");
        assert_eq!((clean.changed, clean.untracked, clean.conflicts), (0, 0, 0));
        assert_eq!((clean.branch, clean.ahead_behind), (None, None));
    }

    /// Waits for a status of `repo` to arrive, ticking the way the main loop does.
    fn wait_for_status(tracker: &mut Tracker, repo: &Repo) -> Status {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let arrived = tracker.tick(&[repo], Instant::now(), false);
            if arrived.iter().any(|fresh| fresh.root == repo.root)
                && let Some(status) = tracker.status(&repo.root)
            {
                return status.clone();
            }
            assert!(Instant::now() < deadline, "no git status arrived");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// With git installed: a real repository, its status from git, and a real worktree.
    #[test]
    fn git_status_of_a_real_repository() {
        if !git_available() {
            eprintln!("git isn't installed: skipped");
            return;
        }
        let tmp = TempDir::new("git-real");
        let dir = tmp.dir("repo");
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args(["-c", "user.name=cm", "-c", "user.email=cm@example.com", "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
                .args(args)
                .current_dir(&dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(dir.join(".gitignore"), "build/\n").unwrap();
        fs::write(dir.join("readme.md"), "hello\n").unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "--no-verify", "-m", "first"]);
        fs::write(dir.join("src/main.rs"), "fn main() { println!(); }\n").unwrap();
        fs::write(dir.join("todo.txt"), "x\n").unwrap();
        fs::create_dir_all(dir.join("build/out")).unwrap();
        fs::write(dir.join("build/out/app"), "x").unwrap();

        let repo = Repo::find(&dir.join("src")).unwrap();
        assert_eq!(repo.head, Head::Branch("main".into()));
        let mut tracker = Tracker::new(true);
        let status = wait_for_status(&mut tracker, &repo);
        assert_eq!((status.changed, status.untracked, status.conflicts), (1, 1, 0));
        assert_eq!(status.mark("src/main.rs"), Some((Mark::Modified, false)));
        assert_eq!(status.mark("src"), Some((Mark::Modified, true)));
        assert_eq!(status.mark("todo.txt"), Some((Mark::Untracked, false)));
        assert_eq!(status.mark("build/out/app"), Some((Mark::Ignored, false)));
        assert_eq!(status.mark("readme.md"), None);
        assert_eq!(status.ahead_behind, None, "no upstream");

        // Committed: clean, after the panels are reloaded.
        git(&["commit", "-q", "--no-verify", "-am", "second"]);
        tracker.refresh();
        let deadline = Instant::now() + Duration::from_secs(30);
        while tracker.status(&repo.root).is_none_or(|s| s.changed != 0) {
            tracker.tick(&[&repo], Instant::now(), false);
            assert!(Instant::now() < deadline, "the commit wasn't seen");
            thread::sleep(Duration::from_millis(20));
        }

        // A worktree made by git itself.
        let other = tmp.path().join("repo-feature");
        git(&["worktree", "add", "-q", "-b", "feature", other.to_str().unwrap()]);
        let linked = Repo::find(&other).unwrap();
        assert!(linked.linked);
        assert_eq!(linked.head, Head::Branch("feature".into()));
        let list = linked.worktrees();
        assert_eq!(list.len(), 2);
        assert!(list[0].main && util::same_path(&list[0].path, &dir));
        assert!(util::same_path(&list[1].path, &other));
    }
}
