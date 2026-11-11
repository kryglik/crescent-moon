//! Temporary directories and a settings-free configuration for tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::app::{BIG_FILE, Config};
use crate::platform::Shell;
use crate::settings::Settings;

/// nvim as editor and viewer, no pager, default settings, and no files on disk (so tests
/// never touch the real hotlist or settings). No `git status` either: a test's folder may be
/// inside a repository (the ones under `target/`), and a test that wants git asks for it.
pub fn config() -> Config {
    Config {
        editor: vec!["nvim".into()],
        viewer: vec!["nvim".into(), "-R".into()],
        pager: None,
        shell: Shell::detect(),
        big_file: BIG_FILE,
        hotlist_file: None,
        settings_file: None,
        history_file: None,
        schemes_dir: None,
        places_file: None,
        colours: crate::scheme::Colours::default(),
        settings: Settings { git_status: false, ..Settings::default() },
    }
}

pub struct TempDir(PathBuf);

impl TempDir {
    /// A fresh directory under the system temp folder (usually on C:).
    pub fn new(tag: &str) -> Self {
        Self::under(&std::env::temp_dir(), tag)
    }

    /// A fresh directory under `target/` of this crate (on the drive holding the repository).
    pub fn in_target(tag: &str) -> Self {
        Self::under(&Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp"), tag)
    }

    fn under(base: &Path, tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("rmc-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn file(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    pub fn dir(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
