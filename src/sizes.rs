//! Directory sizes (Ctrl+Space), measured one directory after another on a background thread.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// A running total (`done: false`) or the final size of a directory.
pub struct SizeUpdate {
    pub dir: PathBuf,
    pub bytes: u64,
    pub done: bool,
}

pub struct Sizer {
    requests: Sender<(PathBuf, u64)>,
    pub updates: Receiver<SizeUpdate>,
    /// Requests from an older generation are dropped; `cancel` starts a new one.
    generation: Arc<AtomicU64>,
    outstanding: Arc<AtomicUsize>,
}

impl Sizer {
    pub fn new() -> Self {
        let (requests, request_rx) = mpsc::channel::<(PathBuf, u64)>();
        let (update_tx, updates) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let outstanding = Arc::new(AtomicUsize::new(0));
        let (worker_generation, worker_outstanding) = (generation.clone(), outstanding.clone());
        thread::spawn(move || {
            while let Ok((dir, wanted)) = request_rx.recv() {
                let cancelled = || worker_generation.load(Ordering::Relaxed) != wanted;
                if !cancelled() {
                    let report = |bytes| {
                        let _ = update_tx.send(SizeUpdate { dir: dir.clone(), bytes, done: false });
                    };
                    if let Some(bytes) = measure(&dir, cancelled, report) {
                        let _ = update_tx.send(SizeUpdate { dir, bytes, done: true });
                    }
                }
                worker_outstanding.fetch_sub(1, Ordering::Relaxed);
            }
        });
        Sizer { requests, updates, generation, outstanding }
    }

    pub fn request(&self, dir: PathBuf) {
        self.outstanding.fetch_add(1, Ordering::Relaxed);
        let _ = self.requests.send((dir, self.generation.load(Ordering::Relaxed)));
    }

    /// Abandons the directory being measured and everything queued.
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn is_busy(&self) -> bool {
        self.outstanding.load(Ordering::Relaxed) > 0
    }
}

/// Adds up the file sizes below `dir` without following links or junctions
/// (so nothing is counted twice). Unreadable folders are skipped. None if cancelled.
pub fn measure(dir: &Path, cancelled: impl Fn() -> bool, mut report: impl FnMut(u64)) -> Option<u64> {
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    let mut last_report = Instant::now();
    while let Some(current) = pending.pop() {
        if cancelled() {
            return None;
        }
        let Ok(entries) = fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            // DirEntry::metadata describes the entry itself, never a link's target.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                pending.push(entry.path());
            } else if !meta.file_type().is_symlink() {
                total += meta.len();
            }
        }
        if last_report.elapsed() >= Duration::from_millis(250) {
            report(total);
            last_report = Instant::now();
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn measures_nested_files() {
        let tmp = TempDir::new("sizes");
        tmp.file("a.bin", "12345");
        tmp.file("sub/b.bin", "123");
        tmp.file("sub/deeper/c.bin", "12");
        assert_eq!(measure(tmp.path(), || false, |_| {}), Some(10));
        assert_eq!(measure(tmp.path(), || true, |_| {}), None);
    }

    #[test]
    fn sizer_reports_and_can_be_cancelled() {
        let tmp = TempDir::new("sizer");
        tmp.file("x/1.bin", "1234");
        let sizer = Sizer::new();
        sizer.request(tmp.path().join("x"));
        let update = loop {
            let update = sizer.updates.recv_timeout(Duration::from_secs(10)).unwrap();
            if update.done {
                break update;
            }
        };
        assert_eq!(update.bytes, 4);
        let deadline = Instant::now() + Duration::from_secs(5);
        while sizer.is_busy() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!sizer.is_busy());

        // A request made after a cancel still runs.
        sizer.cancel();
        sizer.request(tmp.path().join("x"));
        let update = sizer.updates.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(update.done && update.bytes == 4);
    }
}
