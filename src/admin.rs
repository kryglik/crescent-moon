//! "As admin": the rest of a delete done by a second rmc with administrator rights, the way
//! Total Commander does it.
//!
//! rmc writes the job into a folder of its own in the temp folder and starts
//! `rmc --admin-job FOLDER` as administrator (Windows asks the user first). That helper turns
//! on the backup and restore privileges, so even TrustedInstaller's files can go, runs the
//! delete, writes its progress to `status.txt` and the outcome to `result.txt`, and stops
//! when a `stop` file appears. Meanwhile rmc shows the progress as for any job.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use crate::ops::{self, Answer, Job, Msg, Progress, Question, Running};

/// The job file: what to do, then one path per line.
fn job_text(paths: &[PathBuf]) -> String {
    let mut text = String::from("delete\n");
    for path in paths {
        text.push_str(&path.to_string_lossy());
        text.push('\n');
    }
    text
}

fn parse_job(text: &str) -> Option<Vec<PathBuf>> {
    let mut lines = text.lines();
    (lines.next()? == "delete").then(|| lines.filter(|l| !l.is_empty()).map(PathBuf::from).collect())
}

fn status_text(p: &Progress) -> String {
    format!("{}\t{}\t{}\t{}\t{}\t{}", p.files_done, p.files_total, p.bytes_done, p.bytes_total, u8::from(p.scanning), p.current)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse_status(text: &str) -> Option<Progress> {
    let mut fields = text.trim_end_matches(['\r', '\n']).splitn(6, '\t');
    let mut number = || fields.next()?.parse::<u64>().ok();
    let (files_done, files_total, bytes_done, bytes_total, scanning) = (number()?, number()?, number()?, number()?, number()?);
    let current = fields.next().unwrap_or_default().to_string();
    Some(Progress { current, files_done, files_total, bytes_done, bytes_total, scanning: scanning == 1 })
}

/// `result.txt`: "done" or "stopped", then one error per line.
fn result_text(errors: &[String], stopped: bool) -> String {
    let mut text = String::from(if stopped { "stopped\n" } else { "done\n" });
    for error in errors {
        // An error may span lines; the summary only shows the first anyway.
        text.push_str(&error.replace(['\r', '\n'], " "));
        text.push('\n');
    }
    text
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse_result(text: &str) -> Option<(Vec<String>, bool)> {
    let mut lines = text.lines();
    let stopped = match lines.next()? {
        "done" => false,
        "stopped" => true,
        _ => return None,
    };
    Some((lines.filter(|l| !l.is_empty()).map(String::from).collect(), stopped))
}

/// Deletes `paths` as administrator. `earlier` are the problems of the part of the job done
/// before; they are reported together at the end.
pub fn start(paths: Vec<PathBuf>, earlier: Vec<String>) -> Running {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    thread::spawn(move || {
        let (errors, cancelled) = match supervise(&paths, &flag, &tx) {
            Ok(outcome) => outcome,
            Err(e) => (vec![format!("Nothing more was deleted: {e}.")], true),
        };
        let errors = earlier.into_iter().chain(errors).collect();
        let _ = tx.send(Msg::Done { errors, cancelled, as_admin: None });
    });
    let progress = Progress { current: "Waiting for Windows to allow administrator rights…".into(), ..Progress::default() };
    Running { title: "Delete as administrator", progress, rx, cancel }
}

/// Starts the helper, passes its progress on, asks it to stop when cancelled, and reads
/// what it reported.
fn supervise(paths: &[PathBuf], cancel: &AtomicBool, tx: &Sender<Msg>) -> io::Result<(Vec<String>, bool)> {
    let dir = job_folder(paths)?;
    fs::write(dir.join("job.txt"), job_text(paths))?;
    let outcome = run_and_watch(&dir, cancel, tx);
    let _ = fs::remove_dir_all(&dir);
    outcome
}

/// A new folder for the job, with a name nobody can guess beforehand, outside everything
/// being deleted (deleting the temp folder itself mustn't take the job's files along).
fn job_folder(paths: &[PathBuf]) -> io::Result<PathBuf> {
    let exe_dir = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf));
    let base = [Some(std::env::temp_dir()), crate::util::config_dir(), exe_dir]
        .into_iter()
        .flatten()
        .find(|base| !paths.iter().any(|path| crate::util::is_inside(base, path)))
        .ok_or_else(|| io::Error::other("there is no folder outside what is being deleted to keep the job in"))?;
    fs::create_dir_all(&base)?;
    for _ in 0..100 {
        let dir = base.join(format!("rmc-admin-{}-{:016x}", std::process::id(), crate::util::random()));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("no folder could be made for the job"))
}

#[cfg(windows)]
fn run_and_watch(dir: &Path, cancel: &AtomicBool, tx: &Sender<Msg>) -> io::Result<(Vec<String>, bool)> {
    use crate::platform;
    let exe = std::env::current_exe()?;
    let process = platform::run_elevated(&exe, &format!("--admin-job \"{}\"", dir.display()))?;
    loop {
        let ended = platform::wait_elevated(&process, 100);
        if let Some(progress) = fs::read_to_string(dir.join("status.txt")).ok().as_deref().and_then(parse_status) {
            let _ = tx.send(Msg::Progress(progress));
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = fs::write(dir.join("stop"), b"");
        }
        if ended {
            break;
        }
    }
    platform::close_elevated(&process);
    let report = fs::read_to_string(dir.join("result.txt")).unwrap_or_default();
    // It ran but didn't say how it went: some of it may be gone.
    Ok(parse_result(&report).unwrap_or_else(|| {
        (vec!["The administrator's rmc ended without a report: what it deleted is unknown.".into()], true)
    }))
}

#[cfg(not(windows))]
fn run_and_watch(_dir: &Path, _cancel: &AtomicBool, _tx: &Sender<Msg>) -> io::Result<(Vec<String>, bool)> {
    Err(io::Error::other("deleting as administrator is only there on Windows"))
}

/// `rmc --admin-job FOLDER`: the elevated side. Deletes what the job file lists, answering
/// questions itself (failures are skipped and reported), until done or told to stop.
pub fn run_helper(dir: &Path) -> i32 {
    // A plain folder of rmc's, not a junction or link leading somewhere else.
    if fs::symlink_metadata(dir).map_or(true, |meta| !meta.is_dir()) {
        return 2;
    }
    #[cfg(windows)]
    let privileged = crate::platform::enable_admin_privileges();
    #[cfg(not(windows))]
    let privileged = true;
    let Some(paths) = fs::read_to_string(dir.join("job.txt")).ok().as_deref().and_then(parse_job) else {
        let _ = fs::write(dir.join("result.txt"), result_text(&["The job file could not be read.".into()], true));
        return 2;
    };
    let running = ops::start_privileged(Job::Delete { paths, to_trash: false, admin_when_refused: false });
    let (mut errors, stopped) = loop {
        match running.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Msg::Progress(progress)) => {
                // Written aside and renamed, so rmc never reads half a line.
                let temp = dir.join("status.tmp");
                if fs::write(&temp, status_text(&progress)).is_ok() {
                    let _ = fs::rename(&temp, dir.join("status.txt"));
                }
            }
            Ok(Msg::Ask(question, reply)) => {
                let _ = reply.send(match question {
                    Question::Overwrite { .. } => Answer::No,
                    Question::Failed { .. } => Answer::SkipAll,
                });
            }
            Ok(Msg::Done { errors, cancelled, .. }) => break (errors, cancelled),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break (vec!["The delete ended unexpectedly.".into()], true),
        }
        if dir.join("stop").exists() {
            running.cancel.store(true, Ordering::Relaxed);
        }
    };
    if !privileged && !errors.is_empty() {
        errors.insert(0, "The backup and restore privileges could not be turned on.".into());
    }
    let _ = fs::write(dir.join("result.txt"), result_text(&errors, stopped));
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn files_between_rmc_and_the_helper_read_back() {
        let paths = vec![PathBuf::from(r"T:\Windows\WinSxS"), PathBuf::from("/tmp/Účty a faktury")];
        assert_eq!(parse_job(&job_text(&paths)), Some(paths));
        let progress = Progress { current: "a\tb".into(), files_done: 3, files_total: 9, bytes_done: 5, bytes_total: 7, scanning: true };
        let read = parse_status(&status_text(&progress)).unwrap();
        assert_eq!((read.files_done, read.files_total, read.bytes_done, read.bytes_total), (3, 9, 5, 7));
        assert!(read.scanning);
        assert_eq!(read.current, "a\tb");
        let errors = vec!["x: denied\nadvice".to_string()];
        assert_eq!(parse_result(&result_text(&errors, true)), Some((vec!["x: denied advice".to_string()], true)));
        assert_eq!(parse_result("garbage"), None);
    }

    /// The helper's side, run without elevation on files of our own: it deletes what the job
    /// lists and reports.
    #[test]
    fn the_helper_deletes_what_the_job_lists() {
        let tmp = TempDir::new("admin-helper");
        let doomed = tmp.dir("doomed");
        tmp.file("doomed/inner/file.txt", "x");
        let readonly = tmp.file("doomed/ro.txt", "x");
        let mut permissions = fs::metadata(&readonly).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&readonly, permissions).unwrap();
        let keep = tmp.file("keep.txt", "x");
        let job = tmp.dir("job");
        fs::write(job.join("job.txt"), job_text(std::slice::from_ref(&doomed))).unwrap();

        assert_eq!(run_helper(&job), 0);
        let (errors, stopped) = parse_result(&fs::read_to_string(job.join("result.txt")).unwrap()).unwrap();
        assert!(errors.is_empty() && !stopped, "{errors:?}");
        assert!(!doomed.exists());
        assert!(keep.exists());
        assert!(parse_status(&fs::read_to_string(job.join("status.txt")).unwrap()).is_some());
    }
}
