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
    /// `admin_when_refused`: an item Windows refuses to put in the Recycle Bin goes to the
    /// administrator (permanently) without asking, and the rest still goes to the bin.
    Delete { paths: Vec<PathBuf>, to_trash: bool, admin_when_refused: bool },
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
        // The same path, or the same file under another name (another letter case on a
        // drive that ignores case, a link): copying would empty it, moving would delete it.
        if util::same_path(source, &target) || util::same_file(source, &target) {
            return Err(format!("\"{}\" is the same as its destination.", source.display()));
        }
        if source.is_dir() && (util::is_inside(&target, source) || util::really_inside(&target, source)) {
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
        let cancelled = matches!(result, Err(Stop::Cancelled));
        // What goes to the administrator: the rest of a permanent delete, or what the
        // Recycle Bin refused.
        let as_admin = worker.as_admin.take().filter(|paths| !cancelled && !paths.is_empty());
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
    /// The user chose "As admin" for a refused item of a Recycle Bin delete: further refused
    /// items go the same way without asking.
    admin_when_refused: bool,
    /// An overwrite the user already agreed to (a move to another drive asks once, then copies).
    confirmed: Option<PathBuf>,
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
            admin_when_refused: false,
            confirmed: None,
        }
    }

    fn run(&mut self, job: Job) -> Flow<()> {
        match job {
            Job::Copy(pairs) => {
                self.scan_all(pairs.iter().map(|(src, _)| src.as_path()), true)?;
                for (src, dst) in &pairs {
                    if self.prepare_parent(dst)? {
                        self.copy_tree(src, dst, &mut Vec::new())?;
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
                for ((src, dst), (files, bytes)) in pairs.iter().zip(totals) {
                    let before = (self.progress.files_done, self.progress.bytes_done);
                    if self.prepare_parent(dst)? {
                        self.move_entry(src, dst, &mut Vec::new())?;
                    }
                    // However it went (renamed, copied, partly skipped), this item is done.
                    (self.progress.files_done, self.progress.bytes_done) = (before.0 + files, before.1 + bytes);
                    self.send_progress(false);
                }
            }
            Job::Delete { paths, to_trash, admin_when_refused } => {
                if cfg!(windows) && !self.privileged {
                    self.admin_offer = Some(if to_trash {
                        "As admin: deletes it permanently, as administrator (Windows asks first)."
                    } else {
                        "As admin: deletes the rest as administrator (Windows asks first)."
                    });
                }
                self.admin_when_refused = admin_when_refused;
                if to_trash {
                    self.progress.files_total = paths.len() as u64;
                } else {
                    self.scan_all(paths.iter().map(PathBuf::as_path), false)?;
                }
                for (i, path) in paths.iter().enumerate() {
                    let result = if to_trash { self.trash(path) } else { self.remove_tree(path, true).map(drop) };
                    match result {
                        // The Recycle Bin refused it: it alone goes to the administrator, the
                        // rest still to the bin (later refusals follow without asking).
                        Err(Stop::AsAdmin) if to_trash => {
                            self.admin_when_refused = true;
                            self.as_admin.get_or_insert_with(Vec::new).push(path.clone());
                            self.advance((1, 0));
                        }
                        // A permanent delete: this item and the ones after it.
                        Err(Stop::AsAdmin) => {
                            self.as_admin = Some(paths[i..].to_vec());
                            return Err(Stop::AsAdmin);
                        }
                        other => other?,
                    }
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
            // "As admin" only where the administrator's rmc can reach the path: a mapped
            // network drive or a subst drive belongs to the user's own logon session.
            let as_admin = self
                .admin_offer
                .filter(|_| error.kind() == io::ErrorKind::PermissionDenied && platform::admin_can_reach(path));
            if as_admin.is_some() && self.admin_when_refused {
                return Err(Stop::AsAdmin);
            }
            if self.skip_all {
                self.errors.push(message);
                return Ok(None);
            }
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

    /// Counts files and bytes below each path; returns the per-path totals. `copying` walks
    /// the way a copy does (see `followed_metadata`), otherwise links are never followed.
    fn scan_all<'a>(&mut self, paths: impl Iterator<Item = &'a Path>, copying: bool) -> Flow<Vec<(u64, u64)>> {
        self.progress.scanning = true;
        let mut totals = Vec::new();
        for path in paths {
            let before = (self.progress.files_total, self.progress.bytes_total);
            self.scan(path, copying, &mut Vec::new())?;
            totals.push((self.progress.files_total - before.0, self.progress.bytes_total - before.1));
        }
        self.progress.scanning = false;
        Ok(totals)
    }

    fn scan(&mut self, path: &Path, copying: bool, ancestors: &mut Vec<PathBuf>) -> Flow<()> {
        self.check_cancel()?;
        let meta = if copying { followed_metadata(path) } else { fs::symlink_metadata(path) };
        match meta {
            Ok(meta) if meta.is_dir() => {
                let looped = copying && is_link(path) && loops_back(path, ancestors);
                if ancestors.len() < MAX_DEPTH
                    && !looped
                    && let Ok(children) = fs::read_dir(path)
                {
                    ancestors.push(path.to_path_buf());
                    for child in children.flatten() {
                        self.scan(&child.path(), copying, ancestors)?;
                    }
                    ancestors.pop();
                }
                // Deleting counts directories as items too.
                if !copying {
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

    fn confirm_overwrite(&mut self, target: &Path, source: FileInfo, existing: &Metadata) -> Flow<bool> {
        if self.confirmed.as_deref() == Some(target) {
            self.confirmed = None;
            return Ok(true);
        }
        if let Some(all) = self.overwrite_all {
            return Ok(all);
        }
        let question = Question::Overwrite { target: target.to_path_buf(), source, existing: existing.into() };
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
    /// `ancestors` are the folders being copied above it, to notice a link that loops back.
    fn copy_tree(&mut self, src: &Path, dst: &Path, ancestors: &mut Vec<PathBuf>) -> Flow<bool> {
        self.check_cancel()?;
        let Some(meta) = self.attempt(src, |_| followed_metadata(src))? else { return Ok(false) };
        #[cfg(unix)]
        if meta.file_type().is_symlink() {
            return self.copy_link(src, dst);
        }
        if meta.is_dir() {
            if ancestors.len() >= MAX_DEPTH {
                self.errors.push(format!("{}: nested too deeply, skipped", src.display()));
                return Ok(false);
            }
            if is_link(src) && loops_back(src, ancestors) {
                self.errors.push(format!("{}: a link back to a folder above it, skipped", src.display()));
                return Ok(false);
            }
            if fs::symlink_metadata(dst).is_ok_and(|m| !m.is_dir()) {
                self.errors.push(format!("{}: a file with this name already exists", dst.display()));
                return Ok(false);
            }
            if self.attempt(dst, |_| fs::create_dir_all(dst))?.is_none() {
                return Ok(false);
            }
            let Some(children) = self.attempt(src, |_| child_names(src))? else { return Ok(false) };
            ancestors.push(src.to_path_buf());
            let mut complete = true;
            for name in children {
                complete &= self.copy_tree(&src.join(&name), &dst.join(&name), ancestors)?;
            }
            ancestors.pop();
            return Ok(complete);
        }

        self.progress.current = src.display().to_string();
        // A pipe would block the copy for good, a device would be read forever.
        if !meta.is_file() {
            self.errors.push(format!("{}: not a regular file (a pipe, socket or device), skipped", src.display()));
            self.skip_file(0);
            return Ok(false);
        }
        if let Ok(existing) = fs::symlink_metadata(dst) {
            if util::same_file(src, dst) {
                self.errors.push(format!("{}: the same file as its destination, skipped", src.display()));
                self.skip_file(meta.len());
                return Ok(false);
            }
            if existing.is_dir() {
                self.errors.push(format!("{}: a directory with this name already exists", dst.display()));
                self.skip_file(meta.len());
                return Ok(false);
            }
            if !self.confirm_overwrite(dst, (&meta).into(), &existing)? {
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

    /// A symbolic link is copied as a link (as `cp -a` and mc do), even a broken one.
    #[cfg(unix)]
    fn copy_link(&mut self, src: &Path, dst: &Path) -> Flow<bool> {
        self.progress.current = src.display().to_string();
        if let Ok(existing) = fs::symlink_metadata(dst) {
            if existing.is_dir() {
                self.errors.push(format!("{}: a directory with this name already exists", dst.display()));
                self.skip_file(0);
                return Ok(false);
            }
            let info = FileInfo { size: 0, modified: fs::symlink_metadata(src).ok().and_then(|m| m.modified().ok()) };
            if !self.confirm_overwrite(dst, info, &existing)? {
                self.skip_file(0);
                return Ok(false);
            }
        }
        // Made aside and renamed over the target, so a failure leaves the old one.
        let temp = temp_name(dst);
        let made = self
            .attempt(src, |_| {
                let target = fs::read_link(src)?;
                std::os::unix::fs::symlink(&target, &temp)?;
                fs::rename(&temp, dst).inspect_err(|_| {
                    let _ = fs::remove_file(&temp);
                })
            })?
            .is_some();
        if made {
            self.progress.files_done += 1;
        } else {
            self.skip_file(0);
        }
        Ok(made)
    }

    /// Copies one file in chunks (so progress and cancelling work), keeping its time and
    /// read-only flag. It is written under a temporary name beside the target and renamed over
    /// it at the end, so a failed or cancelled copy leaves an existing target as it was.
    fn copy_file(&mut self, src: &Path, dst: &Path, meta: &Metadata) -> io::Result<()> {
        let start = self.progress.bytes_done;
        let temp = temp_name(dst);
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.resize(BUFFER_SIZE, 0);
        let result = self.copy_contents(src, &temp, meta, &mut buffer).and_then(|()| replace_with(&temp, dst));
        self.buffer = buffer;
        if result.is_err() {
            self.progress.bytes_done = start;
            let _ = fs::remove_file(&temp);
            return result;
        }
        // The read-only flag (and on Linux the mode) goes on last: the rename needed it off.
        let _ = fs::set_permissions(dst, meta.permissions());
        result
    }

    fn copy_contents(&mut self, src: &Path, dst: &Path, meta: &Metadata, buffer: &mut [u8]) -> io::Result<()> {
        let mut input = File::open(src)?;
        let mut output = File::create_new(dst)?;
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
        // Keeping the time is worth trying, not failing over: FAT can't store dates before
        // 1980, and a share may not let a non-owner set times.
        if let Ok(modified) = meta.modified() {
            let _ = output.set_modified(modified);
        }
        Ok(())
    }

    /// Moves `src` to `dst`: a rename on one drive, a copy and delete across drives. Onto an
    /// existing folder it merges. Returns whether all of it moved.
    fn move_entry(&mut self, src: &Path, dst: &Path, ancestors: &mut Vec<PathBuf>) -> Flow<bool> {
        self.check_cancel()?;
        self.progress.current = src.display().to_string();
        self.send_progress(false);
        let Some(src_meta) = self.attempt(src, |_| fs::symlink_metadata(src))? else { return Ok(false) };
        let mut overwrite = false;
        // The same file under another name (another letter case where case doesn't count) is
        // simply renamed.
        if let Ok(existing) = fs::symlink_metadata(dst)
            && !util::same_file(src, dst)
        {
            if existing.is_dir() && src_meta.is_dir() {
                return self.merge(src, dst, ancestors);
            }
            if existing.is_dir() || src_meta.is_dir() {
                self.errors.push(format!("{}: cannot replace a file with a directory or the other way round", dst.display()));
                return Ok(false);
            }
            if !self.confirm_overwrite(dst, (&src_meta).into(), &existing)? {
                return Ok(false);
            }
            overwrite = true;
        }
        // The rename replaces an existing file in one step: no moment without either.
        match self.attempt(src, |_| rename_over(src, dst, overwrite))? {
            Some(true) => Ok(true),
            Some(false) => {
                // Another drive: copied, then deleted. The overwrite was agreed to already.
                self.confirmed = overwrite.then(|| dst.to_path_buf());
                let moved = self.move_across(src, dst, ancestors);
                self.confirmed = None;
                moved
            }
            None => Ok(false),
        }
    }

    /// Moves what folder `src` holds into the existing folder `dst` item by item, then
    /// removes `src` if that emptied it.
    fn merge(&mut self, src: &Path, dst: &Path, ancestors: &mut Vec<PathBuf>) -> Flow<bool> {
        if ancestors.len() >= MAX_DEPTH {
            self.errors.push(format!("{}: nested too deeply, skipped", src.display()));
            return Ok(false);
        }
        let Some(children) = self.attempt(src, |_| child_names(src))? else { return Ok(false) };
        ancestors.push(src.to_path_buf());
        let mut complete = true;
        for name in children {
            complete &= self.move_entry(&src.join(&name), &dst.join(&name), ancestors)?;
        }
        ancestors.pop();
        Ok(complete && self.attempt(src, |_| remove_dir(src))?.is_some())
    }

    /// A move to another drive. Each file is deleted as soon as its copy is complete, and a
    /// folder only once it is empty, so a file that appears in it meanwhile (a download, a
    /// log) is never deleted without having been copied.
    fn move_across(&mut self, src: &Path, dst: &Path, ancestors: &mut Vec<PathBuf>) -> Flow<bool> {
        self.check_cancel()?;
        let Some(meta) = self.attempt(src, |_| fs::symlink_metadata(src))? else { return Ok(false) };
        if !meta.is_dir() {
            // A file or a link (on Windows a junction's folder is copied, then the junction goes).
            if !self.copy_tree(src, dst, ancestors)? {
                return Ok(false);
            }
            return Ok(self.attempt(src, |_| remove_file_or_link(src))?.is_some());
        }
        if ancestors.len() >= MAX_DEPTH {
            self.errors.push(format!("{}: nested too deeply, skipped", src.display()));
            return Ok(false);
        }
        if fs::symlink_metadata(dst).is_ok_and(|m| !m.is_dir()) {
            self.errors.push(format!("{}: a file with this name already exists", dst.display()));
            return Ok(false);
        }
        if self.attempt(dst, |_| fs::create_dir_all(dst))?.is_none() {
            return Ok(false);
        }
        let Some(children) = self.attempt(src, |_| child_names(src))? else { return Ok(false) };
        ancestors.push(src.to_path_buf());
        let mut complete = true;
        for name in children {
            complete &= self.move_across(&src.join(&name), &dst.join(&name), ancestors)?;
        }
        ancestors.pop();
        Ok(complete && self.attempt(src, |_| remove_dir(src))?.is_some())
    }

    fn advance(&mut self, (files, bytes): (u64, u64)) {
        self.progress.files_done += files;
        self.progress.bytes_done += bytes;
        self.send_progress(false);
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

/// What copying works with: on Windows what a link points to (the folder of a junction is
/// copied), elsewhere the link itself (it is copied as a link).
fn followed_metadata(path: &Path) -> io::Result<Metadata> {
    if cfg!(windows) { fs::metadata(path) } else { fs::symlink_metadata(path) }
}

/// A symbolic link or (on Windows) a junction.
fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Whether the folder a link leads to is one of the folders being copied above it.
fn loops_back(dir: &Path, ancestors: &[PathBuf]) -> bool {
    let Some(id) = util::file_id(dir) else { return false };
    ancestors.iter().any(|a| util::file_id(a) == Some(id))
}

/// A name beside `target` to write a copy under before it replaces the target.
fn temp_name(target: &Path) -> PathBuf {
    static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(".rmc-{}-{n}.part", std::process::id()))
}

/// Puts the finished copy `temp` in place of `target` (replacing an existing file).
fn replace_with(temp: &Path, target: &Path) -> io::Result<()> {
    match fs::rename(temp, target) {
        // Windows won't replace a read-only file.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && target.exists() => {
            make_writable(target);
            fs::rename(temp, target)
        }
        other => other,
    }
}

/// Renames `src` to `dst`, replacing an existing file when `overwrite`; Ok(false) when they
/// are on different drives.
fn rename_over(src: &Path, dst: &Path, overwrite: bool) -> io::Result<bool> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(true),
        Err(e) if is_cross_device(&e) => Ok(false),
        Err(e) if overwrite && e.kind() == io::ErrorKind::PermissionDenied => {
            make_writable(dst);
            fs::rename(src, dst).map(|()| true)
        }
        Err(e) => Err(e),
    }
}

/// Clears the read-only attribute, which on Windows stops a file being deleted or replaced.
/// Elsewhere that depends on the folder, so a file's mode is left alone.
#[allow(clippy::permissions_set_readonly_false)]
fn make_writable(path: &Path) {
    if cfg!(windows)
        && let Ok(meta) = fs::symlink_metadata(path)
    {
        let mut permissions = meta.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

fn remove_dir(path: &Path) -> io::Result<()> {
    fs::remove_dir(path).or_else(|e| {
        if e.kind() != io::ErrorKind::PermissionDenied || !cfg!(windows) {
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
    if error.kind() == io::ErrorKind::PermissionDenied && cfg!(windows) {
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

        let running = start(Job::Delete { paths: vec![folder.clone(), later.clone()], to_trash: false, admin_when_refused: false });
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

    /// The same file under another name (here a hard link; on WSL's /mnt drives also another
    /// letter case) is never copied or moved onto itself: that would empty or delete it.
    #[test]
    fn the_same_file_by_another_name_is_left_alone() {
        let tmp = TempDir::new("ops-same");
        let a = tmp.file("a.txt", "precious");
        let b = tmp.path().join("b.txt");
        fs::hard_link(&a, &b).unwrap();
        assert!(util::same_file(&a, &b));
        assert!(plan(std::slice::from_ref(&a), "b.txt", tmp.path()).is_err());
        let (errors, _, questions) = run(Job::Copy(vec![(a.clone(), b.clone())]), Answer::Yes);
        assert_eq!((errors.len(), questions), (1, 0), "{errors:?}");
        assert_eq!(fs::read_to_string(&a).unwrap(), "precious");
        let (_, _, _) = run(Job::Move(vec![(a.clone(), b.clone())]), Answer::Yes);
        assert_eq!(fs::read_to_string(&b).unwrap(), "precious");
    }

    /// A copy that fails before writing anything (the source is locked) leaves the file it was
    /// to replace as it was, and no temporary file behind.
    #[test]
    fn a_failed_overwrite_keeps_the_old_file() {
        let tmp = TempDir::new("ops-keep");
        let src = tmp.file("from/x.txt", "new");
        let dst = tmp.file("to/x.txt", "old");
        #[cfg(windows)]
        let _lock = {
            use std::os::windows::fs::OpenOptionsExt;
            File::options().read(true).share_mode(0).open(&src).unwrap()
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&src, fs::Permissions::from_mode(0o000)).unwrap();
            if File::open(&src).is_ok() {
                return; // root reads anything
            }
        }
        let (errors, _, questions) = run(Job::Copy(vec![(src, dst.clone())]), Answer::Yes);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(questions >= 1);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "old");
        let left: Vec<_> = fs::read_dir(tmp.path().join("to")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "no temporary file left: {left:?}");
    }

    #[test]
    fn moving_onto_a_folder_merges_into_it() {
        let tmp = TempDir::new("ops-merge");
        tmp.file("a/x/one.txt", "1");
        tmp.file("a/x/sub/deep.txt", "d");
        tmp.file("b/x/two.txt", "2");
        let (errors, _, _) = run(Job::Move(vec![(tmp.path().join("a/x"), tmp.path().join("b/x"))]), Answer::Yes);
        assert!(errors.is_empty(), "{errors:?}");
        for name in ["one.txt", "two.txt", "sub/deep.txt"] {
            assert!(tmp.path().join("b/x").join(name).exists(), "{name}");
        }
        assert!(!tmp.path().join("a/x").exists(), "the emptied folder is gone");
    }

    /// Links are copied as links, so a link to / doesn't copy the whole disk; a pipe is
    /// skipped instead of blocking the copy for good.
    #[cfg(unix)]
    #[test]
    fn links_are_copied_as_links_and_pipes_are_skipped() {
        let tmp = TempDir::new("ops-links");
        let src = tmp.dir("src");
        std::os::unix::fs::symlink("/", src.join("root")).unwrap();
        std::os::unix::fs::symlink("nowhere", src.join("broken")).unwrap();
        let fifo = std::ffi::CString::new(src.join("pipe").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        let dst = tmp.path().join("dst");
        let (errors, cancelled, _) = run(Job::Copy(vec![(src, dst.clone())]), Answer::Skip);
        assert!(!cancelled);
        assert_eq!(errors.len(), 1, "only the pipe: {errors:?}");
        assert_eq!(fs::read_link(dst.join("root")).unwrap(), Path::new("/"));
        assert_eq!(fs::read_link(dst.join("broken")).unwrap(), Path::new("nowhere"));
    }

    /// A junction leading back to a folder above it is copied once, not 64 levels deep.
    #[cfg(windows)]
    #[test]
    fn a_junction_back_up_the_tree_is_not_followed_round() {
        let tmp = TempDir::new("ops-loop");
        let src = tmp.dir("src");
        tmp.file("src/a.txt", "a");
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(src.join("loop"))
            .arg(&src)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        let dst = tmp.path().join("dst");
        let (errors, cancelled, _) = run(Job::Copy(vec![(src.clone(), dst.clone())]), Answer::Skip);
        assert!(!cancelled);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(dst.join("a.txt").exists() && !dst.join("loop").join("loop").exists());
        fs::remove_dir(src.join("loop")).unwrap();
    }

    /// In a Recycle Bin delete, "As admin" takes only what the bin refuses: the item asked
    /// about, then (without asking again) any other refused item.
    #[cfg(windows)]
    #[test]
    fn as_admin_in_a_recycle_bin_delete_takes_only_the_refused_items() {
        let tmp = TempDir::new("ops-admin-trash");
        let first = tmp.file("parent/first/x.txt", "x");
        let second = tmp.file("parent/second/y.txt", "y");
        let (first, second) = (first.parent().unwrap().to_path_buf(), second.parent().unwrap().to_path_buf());
        let parent = tmp.path().join("parent");
        let user = std::env::var("USERNAME").unwrap();
        let icacls = |path: &Path, args: &[&str]| {
            let status = std::process::Command::new("icacls").arg(path).args(args).output().unwrap().status;
            assert!(status.success(), "icacls {} {args:?}", path.display());
        };
        // Neither item may be deleted, nor anything in their folder.
        for dir in [&first, &second] {
            icacls(dir, &["/deny", &format!("{user}:(D)")]);
        }
        icacls(&parent, &["/deny", &format!("{user}:(DC)")]);
        let running = start(Job::Delete { paths: vec![first.clone(), second.clone()], to_trash: true, admin_when_refused: false });
        let mut questions = 0;
        let handed_over = loop {
            match running.rx.recv_timeout(Duration::from_secs(20)).expect("job hung") {
                Msg::Progress(_) => {}
                Msg::Ask(_, reply) => {
                    questions += 1;
                    reply.send(Answer::AsAdmin).unwrap();
                }
                Msg::Done { as_admin, cancelled, .. } => {
                    assert!(!cancelled);
                    break as_admin;
                }
            }
        };
        for dir in [&first, &second, &parent] {
            icacls(dir, &["/remove:d", &user]);
        }
        assert_eq!(questions, 1, "asked once");
        assert_eq!(handed_over, Some(vec![first.clone(), second.clone()]));
        assert!(first.exists() && second.exists(), "nothing deleted here");
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
        let (errors, _, _) =
            run(Job::Delete { paths: vec![dir.clone()], to_trash: false, admin_when_refused: false }, Answer::Cancel);
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
