//! Copy, move and delete jobs. They run on a worker thread and talk to the UI over channels,
//! so the screen stays responsive and the user can answer questions or cancel.

use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::{platform, util};

/// Guards against link loops when copying through directory links.
const MAX_DEPTH: usize = 64;
const BUFFER_SIZE: usize = 1 << 20;

pub enum Job {
    Copy(Vec<(PathBuf, PathBuf)>),
    Move(Vec<(PathBuf, PathBuf)>),
    Delete { paths: Vec<PathBuf>, to_trash: bool },
}

impl Job {
    pub fn title(&self) -> &'static str {
        match self {
            Job::Copy(_) => "Copy",
            Job::Move(_) => "Move",
            Job::Delete { to_trash: true, .. } => "Delete",
            Job::Delete { to_trash: false, .. } => "Delete permanently",
        }
    }
}

/// Pairs each source with its destination for a copy or move typed as `input`.
///
/// The destination is a directory to put the sources into when there are several sources,
/// when it already is a directory, or when it ends with a slash; otherwise it is the new name.
pub fn plan(sources: &[PathBuf], input: &str, base: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("No destination given.".into());
    }
    let dest = util::resolve(base, input);
    let into_dir = sources.len() > 1 || dest.is_dir() || input.ends_with(['/', '\\']);
    let mut pairs = Vec::new();
    for source in sources {
        let target = if into_dir {
            let Some(name) = source.file_name() else {
                return Err(format!("\"{}\" is a drive root and cannot be copied.", source.display()));
            };
            dest.join(name)
        } else {
            dest.clone()
        };
        if util::same_path(source, &target) {
            return Err(format!("\"{}\" is the same as its destination.", source.display()));
        }
        if source.is_dir() && util::is_inside(&target, source) {
            return Err(format!("\"{}\" cannot be put inside itself.", source.display()));
        }
        pairs.push((source.clone(), target));
    }
    Ok(pairs)
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub current: String,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub scanning: bool,
}

#[derive(Debug)]
pub struct FileInfo {
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl From<&Metadata> for FileInfo {
    fn from(meta: &Metadata) -> Self {
        FileInfo { size: meta.len(), modified: meta.modified().ok() }
    }
}

#[derive(Debug)]
pub enum Question {
    Overwrite { target: PathBuf, source: FileInfo, existing: FileInfo },
    /// `as_admin`: the "As admin" answer is offered, with this to say what it will do.
    Failed { path: PathBuf, error: String, as_admin: Option<&'static str> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    All,
    None,
    Retry,
    Skip,
    SkipAll,
    /// Hand the rest of the job to an elevated helper (see `admin`).
    AsAdmin,
    Cancel,
}

pub enum Msg {
    Progress(Progress),
    Ask(Question, Sender<Answer>),
    /// `as_admin`: the user chose to delete these (what was left) as administrator.
    Done { errors: Vec<String>, cancelled: bool, as_admin: Option<Vec<PathBuf>> },
}

pub struct Running {
    pub title: &'static str,
    pub progress: Progress,
    pub rx: Receiver<Msg>,
    pub cancel: Arc<AtomicBool>,
}

pub fn start(job: Job) -> Running {
    run_job(job, false)
}

/// For the elevated helper: deletes with backup semantics (see `platform::delete_privileged`)
/// and never offers "As admin" again.
pub fn start_privileged(job: Job) -> Running {
    run_job(job, true)
}

fn run_job(job: Job, privileged: bool) -> Running {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let title = job.title();
    let flag = cancel.clone();
    thread::spawn(move || {
        let mut worker = Worker::new(tx.clone(), flag);
        worker.privileged = privileged;
        let result = worker.run(job);
        let as_admin = worker.as_admin.take().filter(|_| matches!(result, Err(Stop::AsAdmin)));
        let cancelled = matches!(result, Err(Stop::Cancelled));
        let _ = tx.send(Msg::Done { errors: worker.errors, cancelled, as_admin });
    });
    Running { title, progress: Progress::default(), rx, cancel }
}

/// Why a job stops early: the user cancelled, or chose to go on as administrator.
enum Stop {
    Cancelled,
    AsAdmin,
}
type Flow<T> = Result<T, Stop>;

struct Worker {
    tx: Sender<Msg>,
    cancel: Arc<AtomicBool>,
    progress: Progress,
    last_sent: Instant,
    overwrite_all: Option<bool>,
    skip_all: bool,
    errors: Vec<String>,
    buffer: Vec<u8>,
    /// Running in the elevated helper.
    privileged: bool,
    /// What a permission error may offer to do as administrator (deleting only, on Windows).
    admin_offer: Option<&'static str>,
    /// What is left for the administrator when the user chose that.
    as_admin: Option<Vec<PathBuf>>,
}

impl Worker {
    fn new(tx: Sender<Msg>, cancel: Arc<AtomicBool>) -> Self {
        Worker {
            tx,
            cancel,
            progress: Progress::default(),
            last_sent: Instant::now(),
            overwrite_all: None,
            skip_all: false,
            errors: Vec::new(),
            buffer: Vec::new(),
            privileged: false,
            admin_offer: None,
            as_admin: None,
        }
    }

    fn run(&mut self, job: Job) -> Flow<()> {
        match job {
            Job::Copy(pairs) => {
                self.scan_all(pairs.iter().map(|(src, _)| src.as_path()), true)?;
                for (src, dst) in &pairs {
                    if self.prepare_parent(dst)? {
                        self.copy_tree(src, dst, 0)?;
                    }
                }
            }
            Job::Move(pairs) => {
                // Within one drive a move is a rename and needs no scan; across drives it is a copy.
                let cross = pairs.iter().any(|(src, dst)| !util::same_volume(src, dst));
                let totals = if cross {
                    self.scan_all(pairs.iter().map(|(src, _)| src.as_path()), true)?
                } else {
                    self.progress.files_total = pairs.len() as u64;
                    vec![(1, 0); pairs.len()]
                };
                for ((src, dst), total) in pairs.iter().zip(totals) {
                    if self.prepare_parent(dst)? {
                        self.move_entry(src, dst, total)?;
                    }
                }
            }
            Job::Delete { paths, to_trash } => {
                if cfg!(windows) && !self.privileged {
                    self.admin_offer = Some(if to_trash {
                        "As admin: deletes it permanently, as administrator (Windows asks first)."
                    } else {
                        "As admin: deletes the rest as administrator (Windows asks first)."
                    });
                }
                if to_trash {
                    self.progress.files_total = paths.len() as u64;
                } else {
                    self.scan_all(paths.iter().map(PathBuf::as_path), false)?;
                }
                for (i, path) in paths.iter().enumerate() {
                    let result = if to_trash { self.trash(path) } else { self.remove_tree(path, true).map(drop) };
                    if let Err(Stop::AsAdmin) = result {
                        // This item and the ones after it go to the administrator.
                        self.as_admin = Some(paths[i..].to_vec());
                    }
                    result?;
                }
            }
        }
        self.send_progress(true);
        Ok(())
    }

    fn check_cancel(&self) -> Flow<()> {
        if self.cancel.load(Ordering::Relaxed) { Err(Stop::Cancelled) } else { Ok(()) }
    }

    fn send_progress(&mut self, force: bool) {
        if force || self.last_sent.elapsed() >= Duration::from_millis(50) {
            let _ = self.tx.send(Msg::Progress(self.progress.clone()));
            self.last_sent = Instant::now();
        }
    }

    fn ask(&mut self, question: Question) -> Flow<Answer> {
        self.send_progress(true);
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(Msg::Ask(question, reply_tx)).is_err() {
            return Err(Stop::Cancelled);
        }
        match reply_rx.recv() {
            Ok(Answer::Cancel) | Err(_) => Err(Stop::Cancelled),
            Ok(Answer::AsAdmin) => Err(Stop::AsAdmin),
            Ok(answer) => Ok(answer),
        }
    }

    /// Runs `op` until it succeeds, asking the user what to do when it fails.
    /// `Ok(None)` means the user chose to skip it.
    fn attempt<T>(&mut self, path: &Path, mut op: impl FnMut(&mut Self) -> io::Result<T>) -> Flow<Option<T>> {
        loop {
            self.check_cancel()?;
            let error = match op(self) {
                Ok(value) => return Ok(Some(value)),
                Err(error) => error,
            };
            self.check_cancel()?;
            let message = format!("{}: {error}", path.display());
            if self.skip_all {
                self.errors.push(message);
                return Ok(None);
            }
            let as_admin = self.admin_offer.filter(|_| error.kind() == io::ErrorKind::PermissionDenied);
            let question = Question::Failed { path: path.to_path_buf(), error: error.to_string(), as_admin };
            match self.ask(question)? {
                Answer::Retry => continue,
                Answer::SkipAll => self.skip_all = true,
                _ => {}
            }
            self.errors.push(message);
            return Ok(None);
        }
    }

    /// Counts files and bytes below each path; returns the per-path totals.
    fn scan_all<'a>(&mut self, paths: impl Iterator<Item = &'a Path>, follow_links: bool) -> Flow<Vec<(u64, u64)>> {
        self.progress.scanning = true;
        let mut totals = Vec::new();
        for path in paths {
            let before = (self.progress.files_total, self.progress.bytes_total);
            self.scan(path, follow_links, 0)?;
            totals.push((self.progress.files_total - before.0, self.progress.bytes_total - before.1));
        }
        self.progress.scanning = false;
        Ok(totals)
    }

    fn scan(&mut self, path: &Path, follow_links: bool, depth: usize) -> Flow<()> {
        self.check_cancel()?;
        let meta = if follow_links { fs::metadata(path) } else { fs::symlink_metadata(path) };
        match meta {
            Ok(meta) if meta.is_dir() => {
                if depth < MAX_DEPTH
                    && let Ok(children) = fs::read_dir(path)
                {
                    for child in children.flatten() {
                        self.scan(&child.path(), follow_links, depth + 1)?;
                    }
                }
                // Deleting counts directories as items too.
                if !follow_links {
                    self.progress.files_total += 1;
                }
            }
            Ok(meta) => {
                self.progress.files_total += 1;
                self.progress.bytes_total += meta.len();
            }
            Err(_) => self.progress.files_total += 1,
        }
        self.progress.current = path.display().to_string();
        self.send_progress(false);
        Ok(())
    }

    /// Creates the destination's parent directory if needed; false if that was skipped.
    fn prepare_parent(&mut self, dst: &Path) -> Flow<bool> {
        match dst.parent() {
            Some(parent) if !parent.exists() => Ok(self.attempt(parent, |_| fs::create_dir_all(parent))?.is_some()),
            _ => Ok(true),
        }
    }

    fn confirm_overwrite(&mut self, target: &Path, source: &Metadata, existing: &Metadata) -> Flow<bool> {
        if let Some(all) = self.overwrite_all {
            return Ok(all);
        }
        let question = Question::Overwrite {
            target: target.to_path_buf(),
            source: source.into(),
            existing: existing.into(),
        };
        Ok(match self.ask(question)? {
            Answer::Yes => true,
            Answer::All => {
                self.overwrite_all = Some(true);
                true
            }
            Answer::None => {
                self.overwrite_all = Some(false);
                false
            }
            _ => false,
        })
    }

    fn skip_file(&mut self, size: u64) {
        self.progress.files_done += 1;
        self.progress.bytes_done += size;
    }

    /// Copies `src` to `dst`, recursing into directories. Returns whether everything was copied.
    fn copy_tree(&mut self, src: &Path, dst: &Path, depth: usize) -> Flow<bool> {
        self.check_cancel()?;
        let Some(meta) = self.attempt(src, |_| fs::metadata(src))? else { return Ok(false) };
        if meta.is_dir() {
            if depth >= MAX_DEPTH {
                self.errors.push(format!("{}: nested too deeply, skipped", src.display()));
                return Ok(false);
            }
            if self.attempt(dst, |_| fs::create_dir_all(dst))?.is_none() {
                return Ok(false);
            }
            let Some(children) = self.attempt(src, |_| child_names(src))? else { return Ok(false) };
            let mut complete = true;
            for name in children {
                complete &= self.copy_tree(&src.join(&name), &dst.join(&name), depth + 1)?;
            }
            return Ok(complete);
        }

        self.progress.current = src.display().to_string();
        if let Ok(existing) = fs::symlink_metadata(dst) {
            if existing.is_dir() {
                self.errors.push(format!("{}: a directory with this name already exists", dst.display()));
                self.skip_file(meta.len());
                return Ok(false);
            }
            if !self.confirm_overwrite(dst, &meta, &existing)? {
                self.skip_file(meta.len());
                return Ok(false);
            }
        }
        let copied = self.attempt(src, |worker| worker.copy_file(src, dst, &meta))?.is_some();
        if copied {
            self.progress.files_done += 1;
        } else {
            self.skip_file(meta.len());
        }
        self.send_progress(false);
        Ok(copied)
    }

    /// Copies one file in chunks (so progress and cancelling work), keeping its time and read-only flag.
    fn copy_file(&mut self, src: &Path, dst: &Path, meta: &Metadata) -> io::Result<()> {
        let start = self.progress.bytes_done;
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.resize(BUFFER_SIZE, 0);
        let result = self.copy_contents(src, dst, meta, &mut buffer);
        self.buffer = buffer;
        if result.is_err() {
            self.progress.bytes_done = start;
            let _ = fs::remove_file(dst);
        }
        result
    }

    fn copy_contents(&mut self, src: &Path, dst: &Path, meta: &Metadata, buffer: &mut [u8]) -> io::Result<()> {
        let mut input = File::open(src)?;
        make_writable(dst);
        let mut output = File::create(dst)?;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let n = match input.read(buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            output.write_all(&buffer[..n])?;
            self.progress.bytes_done += n as u64;
            self.send_progress(false);
        }
        if let Ok(modified) = meta.modified() {
            output.set_modified(modified)?;
        }
        drop(output);
        fs::set_permissions(dst, meta.permissions())
    }

    fn move_entry(&mut self, src: &Path, dst: &Path, total: (u64, u64)) -> Flow<()> {
        self.check_cancel()?;
        self.progress.current = src.display().to_string();
        // A destination that differs only in letter case is the same file on Windows: just rename.
        if let Ok(existing) = fs::symlink_metadata(dst)
            && !util::same_path(src, dst)
        {
            let Some(src_meta) = self.attempt(src, |_| fs::symlink_metadata(src))? else {
                return Ok(self.advance(total));
            };
            if existing.is_dir() && src_meta.is_dir() {
                // Merge into the existing directory.
                return self.copy_then_remove(src, dst);
            }
            if existing.is_dir() || src_meta.is_dir() {
                self.errors.push(format!("{}: cannot replace a file with a directory or the other way round", dst.display()));
                return Ok(self.advance(total));
            }
            if !self.confirm_overwrite(dst, &src_meta, &existing)? {
                return Ok(self.advance(total));
            }
            make_writable(dst);
            if self.attempt(dst, |_| fs::remove_file(dst))?.is_none() {
                return Ok(self.advance(total));
            }
        }
        let renamed = self.attempt(src, |_| match fs::rename(src, dst) {
            Ok(()) => Ok(true),
            Err(e) if is_cross_device(&e) => Ok(false),
            Err(e) => Err(e),
        })?;
        match renamed {
            Some(false) => self.copy_then_remove(src, dst),
            _ => Ok(self.advance(total)),
        }
    }

    fn advance(&mut self, (files, bytes): (u64, u64)) {
        self.progress.files_done += files;
        self.progress.bytes_done += bytes;
        self.send_progress(false);
    }

    fn copy_then_remove(&mut self, src: &Path, dst: &Path) -> Flow<()> {
        if self.copy_tree(src, dst, 0)? {
            self.remove_tree(src, false)?;
        }
        Ok(())
    }

    /// Deletes `path` and everything below it, without following links. Returns whether it is gone.
    fn remove_tree(&mut self, path: &Path, count: bool) -> Flow<bool> {
        self.check_cancel()?;
        let Some(meta) = self.attempt(path, |_| fs::symlink_metadata(path))? else { return Ok(false) };
        let removed = if meta.is_dir() {
            let Some(children) = self.attempt(path, |_| child_names(path))? else { return Ok(false) };
            let mut complete = true;
            for name in children {
                complete &= self.remove_tree(&path.join(name), count)?;
            }
            let privileged = self.privileged;
            complete
                && self
                    .attempt(path, |_| if privileged { platform::delete_privileged(path) } else { remove_dir(path) })?
                    .is_some()
        } else {
            self.progress.current = path.display().to_string();
            let privileged = self.privileged;
            self.attempt(path, |_| if privileged { platform::delete_privileged(path) } else { remove_file_or_link(path) })?
                .is_some()
        };
        if count {
            self.advance((1, if meta.is_dir() { 0 } else { meta.len() }));
        }
        Ok(removed)
    }

    fn trash(&mut self, path: &Path) -> Flow<()> {
        self.progress.current = path.display().to_string();
        self.send_progress(true);
        self.attempt(path, |_| trash::delete(path).map_err(|e| trash_error(path, &e)))?;
        self.advance((1, 0));
        Ok(())
    }
}

fn child_names(dir: &Path) -> io::Result<Vec<std::ffi::OsString>> {
    fs::read_dir(dir)?.map(|entry| entry.map(|e| e.file_name())).collect()
}

/// The trash crate's error in words. On Windows a refusal arrives as "Some operations were
/// aborted" with no reason (the Recycle Bin is asked not to show any UI, such as the
/// administrator prompt): usually a lack of permission, so rmc checks that itself.
fn trash_error(path: &Path, error: &trash::Error) -> io::Error {
    if let Some(denied) = platform::delete_denied(path) {
        return denied;
    }
    let detail = match error {
        trash::Error::Unknown { description } | trash::Error::Os { description, .. } => description.clone(),
        other => other.to_string(),
    };
    io::Error::other(format!(
        "It could not be moved to {}: {}.\nShift+F8 deletes it permanently instead.",
        platform::TRASH_NAME,
        detail.trim_end_matches('.').to_lowercase()
    ))
}

fn is_cross_device(error: &io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE on Windows.
    error.kind() == io::ErrorKind::CrossesDevices || (cfg!(windows) && error.raw_os_error() == Some(17))
}

#[allow(clippy::permissions_set_readonly_false)]
fn make_writable(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        let mut permissions = meta.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

fn remove_dir(path: &Path) -> io::Result<()> {
    fs::remove_dir(path).or_else(|e| {
        if e.kind() != io::ErrorKind::PermissionDenied {
            return Err(e);
        }
        make_writable(path);
        fs::remove_dir(path)
    })
}

fn remove_file_or_link(path: &Path) -> io::Result<()> {
    let Err(error) = fs::remove_file(path) else { return Ok(()) };
    // Directory symlinks and junctions on Windows are removed as (empty) directories.
    if fs::remove_dir(path).is_ok() {
        return Ok(());
    }
    if error.kind() == io::ErrorKind::PermissionDenied {
        make_writable(path);
        return fs::remove_file(path);
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Runs a job to completion, answering every question with `answer`.
    fn run(job: Job, answer: Answer) -> (Vec<String>, bool, usize) {
        let running = start(job);
        let mut questions = 0;
        loop {
            match running.rx.recv_timeout(Duration::from_secs(20)).expect("job hung") {
                Msg::Progress(_) => {}
                Msg::Ask(_, reply) => {
                    questions += 1;
                    reply.send(answer).unwrap();
                }
                Msg::Done { errors, cancelled, .. } => return (errors, cancelled, questions),
            }
        }
    }

    #[test]
    fn plan_rules() {
        let tmp = TempDir::new("ops-plan");
        let a = tmp.file("a.txt", "a");
        let b = tmp.file("b.txt", "b");
        let dir = tmp.dir("dir");
        let base = tmp.path();

        let single = plan(std::slice::from_ref(&a), "renamed.txt", base).unwrap();
        assert_eq!(single, [(a.clone(), base.join("renamed.txt"))]);

        let into = plan(&[a.clone(), b.clone()], "dir", base).unwrap();
        assert_eq!(into[1], (b.clone(), dir.join("b.txt")));

        let new_dir = plan(std::slice::from_ref(&a), "newdir/", base).unwrap();
        assert_eq!(new_dir[0].1, base.join("newdir").join("a.txt"));

        assert!(plan(std::slice::from_ref(&a), "a.txt", base).is_err(), "onto itself");
        assert!(plan(std::slice::from_ref(&dir), "dir/inner", base).is_err(), "into itself");
        assert!(plan(std::slice::from_ref(&a), "  ", base).is_err());
    }

    #[test]
    fn copies_a_tree_with_contents_and_times() {
        let tmp = TempDir::new("ops-copy");
        let src = tmp.dir("src");
        tmp.file("src/one.txt", "one");
        tmp.file("src/sub/Příliš žluťoučký.txt", "kůň");
        let old = SystemTime::now() - Duration::from_secs(86_400 * 30);
        File::options().write(true).open(src.join("one.txt")).unwrap().set_modified(old).unwrap();

        let dst = tmp.path().join("dst");
        let (errors, cancelled, _) = run(Job::Copy(vec![(src.clone(), dst.clone())]), Answer::Cancel);
        assert!(errors.is_empty() && !cancelled, "{errors:?}");
        assert_eq!(fs::read_to_string(dst.join("sub/Příliš žluťoučký.txt")).unwrap(), "kůň");
        let copied_time = fs::metadata(dst.join("one.txt")).unwrap().modified().unwrap();
        assert_eq!(copied_time, old);
        assert!(src.join("one.txt").exists(), "copy keeps the source");
    }

    #[test]
    fn overwrite_question_respects_the_answer() {
        let tmp = TempDir::new("ops-overwrite");
        let src = tmp.file("from/x.txt", "new");
        let dst = tmp.file("to/x.txt", "old");

        let (_, _, questions) = run(Job::Copy(vec![(src.clone(), dst.clone())]), Answer::No);
        assert_eq!(questions, 1);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "old");

        let (_, _, _) = run(Job::Copy(vec![(src, dst.clone())]), Answer::Yes);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "new");
    }

    /// A permission error while deleting offers "As admin"; choosing it stops the job and hands
    /// over this item and the ones after it. (Nothing elevated starts here; app.rs does that.)
    #[cfg(windows)]
    #[test]
    fn as_admin_hands_over_what_is_left() {
        let tmp = TempDir::new("ops-admin");
        let locked = tmp.file("a/locked.txt", "x");
        let folder = tmp.path().join("a");
        let later = tmp.file("b.txt", "y");
        let user = std::env::var("USERNAME").unwrap();
        let icacls = |path: &Path, args: &[&str]| {
            let status = std::process::Command::new("icacls").arg(path).args(args).output().unwrap().status;
            assert!(status.success(), "icacls {} {args:?}", path.display());
        };
        // Neither the file itself nor anything in its folder may be deleted.
        icacls(&locked, &["/deny", &format!("{user}:(D)")]);
        icacls(&folder, &["/deny", &format!("{user}:(DC)")]);

        let running = start(Job::Delete { paths: vec![folder.clone(), later.clone()], to_trash: false });
        let mut offered = false;
        let handed_over = loop {
            match running.rx.recv_timeout(Duration::from_secs(20)).expect("job hung") {
                Msg::Progress(_) => {}
                Msg::Ask(Question::Failed { as_admin, error, .. }, reply) => {
                    offered = as_admin.is_some();
                    assert!(error.contains("denied"), "{error}");
                    reply.send(Answer::AsAdmin).unwrap();
                }
                Msg::Ask(other, _) => panic!("unexpected {other:?}"),
                Msg::Done { as_admin, cancelled, .. } => {
                    assert!(!cancelled);
                    break as_admin;
                }
            }
        };
        icacls(&locked, &["/remove:d", &user]);
        icacls(&folder, &["/remove:d", &user]);
        assert!(offered, "a permission error offers As admin");
        assert_eq!(handed_over, Some(vec![folder, later.clone()]));
        assert!(later.exists(), "what is handed over is left alone");
    }

    #[test]
    fn cancelling_from_a_question_stops_the_job() {
        let tmp = TempDir::new("ops-cancel");
        let src = tmp.file("a/x.txt", "new");
        let dst = tmp.file("b/x.txt", "old");
        let (_, cancelled, _) = run(Job::Copy(vec![(src, dst.clone())]), Answer::Cancel);
        assert!(cancelled);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "old");
    }

    #[test]
    fn moves_within_a_drive() {
        let tmp = TempDir::new("ops-move");
        let src = tmp.file("a/file.txt", "data");
        tmp.file("a/folder/inner.txt", "inner");
        let target_dir = tmp.dir("b");
        let pairs = plan(&[src.clone(), tmp.path().join("a/folder")], target_dir.to_str().unwrap(), tmp.path()).unwrap();
        let (errors, _, _) = run(Job::Move(pairs), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(target_dir.join("folder/inner.txt")).unwrap(), "inner");
    }

    #[test]
    fn moves_across_drives_by_copying() {
        let from = TempDir::new("ops-xmove-src");
        let to = TempDir::in_target("ops-xmove-dst");
        if util::same_volume(from.path(), to.path()) {
            eprintln!("temp dir and target dir are on one drive; cross-drive move not tested");
            return;
        }
        let src = from.dir("tree");
        from.file("tree/a.txt", "a");
        from.file("tree/deep/b.txt", "b");
        let dst = to.path().join("tree");
        let (errors, _, _) = run(Job::Move(vec![(src.clone(), dst.clone())]), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!src.exists(), "source removed after the copy");
        assert_eq!(fs::read_to_string(dst.join("deep/b.txt")).unwrap(), "b");
    }

    #[test]
    fn permanent_delete_handles_read_only_files() {
        let tmp = TempDir::new("ops-delete");
        let dir = tmp.dir("victim");
        let file = tmp.file("victim/locked.txt", "x");
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).unwrap();
        let (errors, _, _) = run(Job::Delete { paths: vec![dir.clone()], to_trash: false }, Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!dir.exists());
    }

    #[test]
    fn failures_can_be_skipped() {
        let tmp = TempDir::new("ops-skip");
        let missing = tmp.path().join("nope.txt");
        let (errors, cancelled, questions) =
            run(Job::Copy(vec![(missing, tmp.path().join("out.txt"))]), Answer::Skip);
        assert_eq!((questions, errors.len(), cancelled), (1, 1, false));
    }
}
