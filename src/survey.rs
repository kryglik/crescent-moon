//! What a delete would remove, counted in the background while the delete question is open:
//! files, folders and bytes, and whether any folder may not be deleted without
//! administrator rights. It stops as soon as the question is answered.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::{lfs, platform};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tally {
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    /// The first item found that the user may not delete, and why.
    pub denied: Option<(PathBuf, String)>,
    pub done: bool,
}

/// A survey running on its own thread; dropping it stops it.
pub struct Survey {
    tally: Arc<Mutex<Tally>>,
    stop: Arc<AtomicBool>,
}

impl Survey {
    pub fn start(paths: Vec<PathBuf>) -> Survey {
        let tally = Arc::new(Mutex::new(Tally::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopping) = (tally.clone(), stop.clone());
        thread::spawn(move || walk(paths, &shared, &stopping));
        Survey { tally, stop }
    }

    /// The counts so far.
    pub fn tally(&self) -> Tally {
        self.tally.lock().map(|t| t.clone()).unwrap_or_default()
    }

    pub fn is_done(&self) -> bool {
        self.tally.lock().is_ok_and(|t| t.done)
    }
}

impl Drop for Survey {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Counts below `paths` without following links. The items themselves and every folder are
/// checked for permission until the first refusal (files inside a folder share its fate
/// often enough, and one refusal is all the question needs to know).
fn walk(paths: Vec<PathBuf>, tally: &Mutex<Tally>, stop: &AtomicBool) {
    let mut local = Tally::default();
    let mut pending: Vec<(PathBuf, bool)> = paths.into_iter().map(|p| (p, true)).collect();
    let mut since_update = 0;
    let mounts = platform::mount_dirs();
    while let Some((path, top)) = pending.pop() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(meta) = lfs::symlink_metadata(&path) else { continue };
        let is_dir = meta.is_dir();
        if local.denied.is_none()
            && (top || is_dir)
            && let Some(denied) = platform::delete_denied(&path)
            && denied.kind() == std::io::ErrorKind::PermissionDenied
        {
            local.denied = Some((path.clone(), denied.to_string()));
        }
        if is_dir {
            local.folders += 1;
            // What's mounted in a folder isn't deleted with it.
            if !platform::is_mount(&path, &meta, &mounts)
                && let Ok(entries) = lfs::read_dir(&path)
            {
                pending.extend(entries.flatten().map(|e| (path.join(e.file_name()), false)));
            }
        } else {
            local.files += 1;
            local.bytes += meta.len();
        }
        since_update += 1;
        if since_update >= 500 {
            since_update = 0;
            if let Ok(mut shared) = tally.lock() {
                *shared = local.clone();
            }
        }
    }
    local.done = true;
    if let Ok(mut shared) = tally.lock() {
        *shared = local;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::time::{Duration, Instant};

    #[test]
    fn counts_what_a_delete_would_remove() {
        let tmp = TempDir::new("survey");
        let a = tmp.dir("a");
        tmp.file("a/one.txt", "12345");
        tmp.file("a/deeper/two.txt", "123");
        let loose = tmp.file("loose.txt", "12");
        let survey = Survey::start(vec![a, loose]);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !survey.is_done() {
            assert!(Instant::now() < deadline, "survey did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        let tally = survey.tally();
        assert_eq!((tally.files, tally.folders, tally.bytes), (3, 2, 10));
        assert_eq!(tally.denied, None);
    }
}
