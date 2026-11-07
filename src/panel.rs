//! One file panel: a directory listing with a cursor, marks and sorting.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as Atomic};
use std::time::SystemTime;

use crate::archive::{self, Archive};
use crate::find::NameFilter;
use crate::git::Repo;
use crate::reading::{Answer, Asked, Listing, Purpose, Reading, Refused, Spot};
use crate::ssh::{self, Connection};
use crate::{platform, util};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortBy {
    Name,
    Extension,
    Size,
    Time,
}

impl SortBy {
    /// In the order of the F9 menu.
    pub const ALL: [SortBy; 4] = [SortBy::Name, SortBy::Extension, SortBy::Size, SortBy::Time];

    pub fn label(self) -> &'static str {
        match self {
            SortBy::Name => "Name",
            SortBy::Extension => "Extension",
            SortBy::Size => "Size",
            SortBy::Time => "Modify time",
        }
    }

    /// Its key in the F9 menu.
    pub fn letter(self) -> char {
        match self {
            SortBy::Name => 'n',
            SortBy::Extension => 'e',
            SortBy::Size => 's',
            SortBy::Time => 't',
        }
    }

    /// The order the list is in. Without `reverse` names go A to Z, and sizes and times
    /// biggest and newest first, which is what one usually looks for.
    pub fn direction(self, reverse: bool) -> &'static str {
        match (self, reverse) {
            (SortBy::Name | SortBy::Extension, false) => "A to Z",
            (SortBy::Name | SortBy::Extension, true) => "Z to A",
            (SortBy::Size, false) => "largest first",
            (SortBy::Size, true) => "smallest first",
            (SortBy::Time, false) => "newest first",
            (SortBy::Time, true) => "oldest first",
        }
    }

    /// Whether the list goes from high to low (Z..A, big..small, new..old): the heading's ↓.
    pub fn descending(self, reverse: bool) -> bool {
        matches!(self, SortBy::Size | SortBy::Time) != reverse
    }

    /// Shown in the panel header, as in mc.
    pub fn indicator(self) -> &'static str {
        match self {
            SortBy::Name => ".n",
            SortBy::Extension => ".e",
            SortBy::Size => ".s",
            SortBy::Time => ".t",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_link: bool,
    pub is_exec: bool,
    pub is_parent: bool,
    /// Hidden (Windows' attribute; on Linux a name starting with a dot), shown dimmed.
    pub is_hidden: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Where a link or a junction leads, as the system words it.
    pub leads_to: Option<String>,
    sort_name: String,
}

impl Entry {
    /// The `..` row, leading to `path`.
    pub fn up(path: PathBuf, modified: Option<SystemTime>) -> Entry {
        Entry {
            name: "..".into(),
            path,
            is_dir: true,
            is_link: false,
            is_exec: false,
            is_parent: true,
            is_hidden: false,
            size: 0,
            modified,
            leads_to: None,
            sort_name: String::new(),
        }
    }

    fn extension(&self) -> &str {
        match self.sort_name.rfind('.') {
            Some(i) if i > 0 => &self.sort_name[i + 1..],
            _ => "",
        }
    }
}

/// A directory size from Ctrl+Space: a running total until `done`.
#[derive(Clone, Copy, Debug)]
pub struct DirSize {
    pub bytes: u64,
    pub done: bool,
}

pub struct Panel {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub offset: usize,
    pub marked: HashSet<String>,
    /// Sizes of subdirectories, by name, for the current directory only.
    pub dir_sizes: HashMap<String, DirSize>,
    pub sort: SortBy,
    /// The other way round: Z to A, smallest or oldest first. Directories stay on top.
    pub reverse: bool,
    pub show_hidden: bool,
    /// Ctrl+F: only files matching this are listed (`*.jpg *.cr2`, `!*.tmp`, or part of a
    /// name; see `find::NameFilter`). Folders are always listed.
    pub filter: Option<String>,
    /// Visible list rows, updated on every draw (used for PgUp/PgDn).
    pub page: usize,
    pub space: Option<(u64, u64)>,
    /// Where this panel last was on each drive (keyed by `util::volume_key`).
    drive_dirs: HashMap<String, PathBuf>,
    /// Folders to go back and forward to (Alt+Left / Alt+Right), as in a browser.
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    /// A zip shown as a folder (Enter on it). `cwd` stays the real folder it is in, which
    /// the shell, Find and the drive memory go by.
    pub archive: Option<ArchiveView>,
    /// The git repository the folder is in (not while in a zip), read again at each load.
    pub repo: Option<Repo>,
    /// A folder on another computer shown here. `cwd` stays the folder on this one, which the
    /// shell and the drive memory go by.
    pub remote: Option<RemoteView>,
    /// A reload couldn't reach the other computer, and why; for the user to be told.
    pub lost: Option<String>,
    /// What is being read (see `reading`). Until it is in, the panel shows what it showed.
    reading: Option<Reading>,
    /// Something changed since that reading started: read again once it is in.
    read_again: bool,
    /// Where the cursor goes when the reading is in.
    cursor_to: Option<String>,
    /// How going somewhere ended, for `App` to act on.
    pub came: Option<Came>,
}

/// How going somewhere ended.
#[derive(Debug, PartialEq)]
pub enum Came {
    /// The panel is there.
    There,
    /// The panel shows the zip.
    Zip,
    /// It stayed where it was, and this is why.
    Failed(String),
    /// Alt+Left or Alt+Right with nowhere left to go.
    NoFurther { back: bool },
}

/// The folder of another computer a panel shows.
#[derive(Clone)]
pub struct RemoteView {
    pub connection: Arc<Connection>,
    pub dir: String,
}

impl RemoteView {
    /// The whole address of `path` over there.
    pub fn place(&self, path: &str) -> PathBuf {
        PathBuf::from(self.connection.target.place(path))
    }

    /// The address of this folder, as a folder to put things in: with a slash at the end.
    pub fn folder_text(&self) -> String {
        self.connection.target.place(&ssh::join(&self.dir, ""))
    }

    /// What `input` means from this folder.
    pub fn resolve(&self, input: &str) -> String {
        ssh::resolve(&self.dir, &self.connection.home, input)
    }
}

/// The zip a panel shows, and where in it.
pub struct ArchiveView {
    pub archive: Archive,
    /// The folder inside it ("" at the top).
    pub inner: String,
    /// What the title calls the zip: its path, or for one inside another the path through
    /// that (the file itself was taken out into the temp folder).
    pub shown: PathBuf,
    /// The archive this one was opened from, to go back to.
    pub outer: Option<Box<ArchiveView>>,
}

impl ArchiveView {
    /// Where the panel is: `shown`, then the folder inside.
    pub fn location(&self) -> PathBuf {
        self.inner.split('/').filter(|p| !p.is_empty()).fold(self.shown.clone(), |path, part| path.join(part))
    }
}

/// How many folders back the panel remembers.
const BACK_LIMIT: usize = 100;

/// Numbers in names sort by value ("day 2" before "day 10"), as in Explorer; an option.
pub static NATURAL_SORT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn name_order(a: &str, b: &str) -> Ordering {
    if NATURAL_SORT.load(std::sync::atomic::Ordering::Relaxed) { util::natural_cmp(a, b) } else { a.cmp(b) }
}

impl Panel {
    /// Opens `cwd`, falling back to its parents, the home directory or the system root.
    pub fn new(cwd: PathBuf, show_hidden: bool) -> Panel {
        let mut panel = Panel::blank(show_hidden);
        panel.cwd = util::normalize(&cwd);
        panel.begin(vec![Spot::Here(panel.cwd.clone())], true, Purpose::Start);
        panel.wait_a_moment();
        panel
    }

    /// A panel showing nothing, made without touching the disk (a place holder while tabs
    /// change places).
    pub fn blank(show_hidden: bool) -> Panel {
        Panel {
            cwd: PathBuf::new(),
            entries: Vec::new(),
            cursor: 0,
            offset: 0,
            marked: HashSet::new(),
            dir_sizes: HashMap::new(),
            sort: SortBy::Name,
            reverse: false,
            show_hidden,
            filter: None,
            page: 20,
            space: None,
            drive_dirs: HashMap::new(),
            back: Vec::new(),
            forward: Vec::new(),
            archive: None,
            repo: None,
            remote: None,
            lost: None,
            reading: None,
            read_again: false,
            cursor_to: None,
            came: None,
        }
    }

    /// A new tab of this panel: the same folder, order, filter and memory of drives.
    pub fn new_tab(&self) -> Panel {
        let mut tab = Panel::blank(self.show_hidden);
        tab.cwd = self.cwd.clone();
        (tab.sort, tab.reverse, tab.filter) = (self.sort, self.reverse, self.filter.clone());
        tab.drive_dirs = self.drive_dirs.clone();
        tab.remote = self.remote.clone();
        // It shows the folder at once, while that is read again. A zip stays in its own tab.
        if self.archive.is_none() {
            (tab.entries, tab.space, tab.repo) = (self.entries.clone(), self.space, self.repo.clone());
        }
        tab.reload();
        tab
    }

    fn remember_drive(&mut self) {
        if let Some(key) = util::volume_key(&self.cwd) {
            self.drive_dirs.insert(key, self.cwd.clone());
        }
    }

    /// Where to go on the drive of `root`: where this panel last was there if that can still
    /// be read, otherwise the root itself.
    pub fn places_on_drive(&self, root: &Path) -> Vec<PathBuf> {
        let last = util::volume_key(root).and_then(|key| self.drive_dirs.get(&key)).cloned();
        last.into_iter().chain([root.to_path_buf()]).collect()
    }

    /// Starts reading `spots` for `purpose`; whatever was being read goes.
    fn begin(&mut self, spots: Vec<Spot>, refuge: bool, purpose: Purpose) {
        let refreshing = matches!(purpose, Purpose::Refresh);
        self.read_again = false;
        if !refreshing {
            self.cursor_to = None;
        }
        // Sizes are of this folder's folders.
        let sizes = if refreshing && self.sort == SortBy::Size { self.dir_sizes.clone() } else { HashMap::new() };
        let asked = Asked {
            spots,
            refuge,
            show_hidden: self.show_hidden,
            filter: self.filter.clone(),
            sort: self.sort,
            reverse: self.reverse,
            sizes,
        };
        self.reading = Some(Reading::start(asked, purpose));
    }

    /// A folder that is quick is in when this returns.
    fn wait_a_moment(&mut self) {
        if let Some(answer) = self.reading.as_ref().and_then(Reading::wait) {
            self.land(answer);
        }
    }

    /// Takes in what is being read, if that has come. Returns whether the screen should
    /// change: it has, or the wait has gone on long enough to say so.
    pub fn poll(&mut self) -> bool {
        let Some(reading) = self.reading.as_mut() else { return false };
        match reading.take() {
            Some(answer) => {
                self.land(answer);
                true
            }
            None => reading.shows_anew(),
        }
    }

    /// The wait is as old as if it had gone on for a while.
    #[cfg(test)]
    pub fn age_the_wait(&mut self) {
        if let Some(reading) = &mut self.reading {
            crate::reading::tests::age(reading);
        }
    }

    /// Whether something is being read.
    pub fn is_reading(&self) -> bool {
        self.reading.is_some()
    }

    /// Whether the list is about to go, or the cursor to move: a key now would act on what is
    /// shown instead of on what is coming.
    pub fn is_going(&self) -> bool {
        self.cursor_to.is_some() || self.reading.as_ref().is_some_and(|reading| !matches!(reading.purpose, Purpose::Refresh))
    }

    /// Whether the wait has gone on long enough to show.
    pub fn waits_visibly(&self) -> bool {
        self.reading.as_ref().is_some_and(Reading::is_long)
    }

    /// What the panel is waiting for, once it is long enough to say so.
    pub fn waiting_for(&self) -> Option<String> {
        self.reading.as_ref().and_then(Reading::note)
    }

    /// Esc: the panel stays as it is, and nobody takes what is being read. A panel that has
    /// nothing to stay at goes home instead.
    pub fn stop_reading(&mut self) {
        let Some(reading) = self.reading.take() else { return };
        self.read_again = false;
        self.cursor_to = None;
        if matches!(reading.purpose, Purpose::Start) {
            let next = util::home_dir().filter(|home| *home != self.cwd).unwrap_or_else(system_root);
            if next != self.cwd {
                self.cwd = next;
                self.begin(vec![Spot::Here(self.cwd.clone())], true, Purpose::Start);
                self.wait_a_moment();
            }
        }
    }

    /// What was read is in: the panel takes it, and what it means.
    fn land(&mut self, answer: Answer) {
        let Some(reading) = self.reading.take() else { return };
        let arrived = answer.is_ok();
        match (reading.purpose, answer) {
            (Purpose::Start, Ok(listing)) => {
                self.take_in(listing);
                self.remember_drive();
            }
            // Not even the top of the system: an empty panel.
            (Purpose::Start, Err(_)) => {}
            (Purpose::Refresh, Ok(listing)) => self.refreshed(listing),
            (Purpose::Refresh, Err(refused)) => self.not_refreshed(&refused),
            (Purpose::Go { .. }, Ok(listing)) => {
                let from = self.place();
                self.go_in(listing);
                if !util::same_path(&from, &self.place()) {
                    self.back.push(from);
                    if self.back.len() > BACK_LIMIT {
                        self.back.remove(0);
                    }
                    self.forward.clear();
                }
                self.came = Some(Came::There);
            }
            (Purpose::Go { quiet }, Err(refused)) => {
                if !quiet {
                    self.came = Some(Came::Failed(refused.message()));
                }
            }
            (Purpose::Step { back, depths }, Ok(listing)) => {
                self.steps(back).truncate(depths[listing.tried]);
                let here = self.place();
                self.go_in(listing);
                self.steps(!back).push(here);
                self.came = Some(Came::There);
            }
            (Purpose::Step { back, depths }, Err(refused)) => {
                self.steps(back).truncate(depths[refused.tried]);
                self.came = Some(if refused.error.kind() == io::ErrorKind::TimedOut {
                    Came::Failed(refused.message())
                } else {
                    Came::NoFurther { back }
                });
            }
            (Purpose::OpenZip, Ok(listing)) => {
                self.go_in(listing);
                self.came = Some(Came::Zip);
            }
            (Purpose::OpenZip, Err(refused)) => self.came = Some(Came::Failed(refused.message())),
        }
        // The name may be only in what the next reading finds.
        if let Some(name) = self.cursor_to.take().filter(|_| arrived)
            && !self.put_cursor_on(&name)
            && self.read_again
        {
            self.cursor_to = Some(name);
        }
        if std::mem::take(&mut self.read_again) {
            self.look_again();
        }
    }

    /// The list of folders to go back to, or forward to.
    fn steps(&mut self, back: bool) -> &mut Vec<PathBuf> {
        if back { &mut self.back } else { &mut self.forward }
    }

    /// Puts what was read on show. Marks of entries that still exist are kept.
    fn take_in(&mut self, listing: Listing) {
        match listing.spot {
            Spot::Here(dir) => {
                self.cwd = dir;
                self.remote = None;
                self.archive = None;
            }
            Spot::There(view) => {
                self.remote = Some(view);
                self.archive = None;
            }
            Spot::Zip { shown, .. } => {
                if let Some(archive) = listing.zip {
                    let outer = self.archive.take().map(Box::new);
                    self.archive = Some(ArchiveView { archive, inner: String::new(), shown, outer });
                }
            }
            Spot::ZipAgain { .. } => {
                if let (Some(view), Some(archive)) = (self.archive.as_mut(), listing.zip) {
                    if !view.inner.is_empty() && archive.find(&view.inner).is_none() {
                        view.inner.clear();
                    }
                    view.archive = archive;
                }
            }
        }
        if self.archive.is_some() {
            return self.list_zip();
        }
        self.entries = listing.entries;
        // At the top of the other computer, ".." leads back to this one.
        if let (Some(up), true) = (self.entries.first_mut(), self.remote.is_some())
            && up.is_parent
            && up.path.as_os_str().is_empty()
        {
            up.path = self.cwd.clone();
        }
        self.space = listing.space;
        self.repo = listing.repo;
        self.keep_what_is_still_there();
        if listing.order != (self.sort, self.reverse) {
            self.resort();
        }
    }

    /// What is in the zip's folder on show: all of that is in memory.
    fn list_zip(&mut self) {
        let Some(view) = &self.archive else { return };
        let filter = self.filter.as_deref().map(NameFilter::parse);
        let mut entries = archive_entries(view, filter.as_ref());
        sort_entries(&mut entries, self.sort, self.reverse, &self.dir_sizes);
        // ".." goes up inside it, and out of it at its top.
        let up = view.location().parent().map_or_else(|| self.cwd.clone(), Path::to_path_buf);
        entries.insert(0, Entry::up(up, view.archive.stamp.1));
        self.entries = entries;
        self.repo = None;
        self.keep_what_is_still_there();
    }

    /// Marks and sizes of entries that are gone go too (looked up by name: a folder of 50,000
    /// photos with all of them marked must not take seconds).
    fn keep_what_is_still_there(&mut self) {
        let names: HashSet<&str> = self.entries.iter().filter(|e| !e.is_parent).map(|e| e.name.as_str()).collect();
        let dirs: HashSet<&str> = self.entries.iter().filter(|e| e.is_dir && !e.is_parent).map(|e| e.name.as_str()).collect();
        self.marked.retain(|name| names.contains(name.as_str()));
        self.dir_sizes.retain(|name, _| dirs.contains(name.as_str()));
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
    }

    /// The panel moves to what was read: a new place, with nothing marked. Coming back up, the
    /// cursor goes on the folder just left.
    fn go_in(&mut self, listing: Listing) {
        let left = match &listing.spot {
            Spot::Here(dir) => util::child_name(dir, &self.cwd),
            Spot::There(view) => self.remote.as_ref().and_then(|left| {
                let below = left.dir.strip_prefix(&ssh::join(&view.dir, ""))?;
                below.split('/').next().map(String::from)
            }),
            Spot::Zip { .. } | Spot::ZipAgain { .. } => None,
        };
        let here = matches!(listing.spot, Spot::Here(_));
        self.marked.clear();
        self.dir_sizes.clear();
        (self.cursor, self.offset) = (0, 0);
        self.take_in(listing);
        if here {
            self.remember_drive();
        }
        if let Some(name) = left {
            self.put_cursor_on(&name);
        }
    }

    /// The place on show, read again. The cursor stays on its name.
    fn refreshed(&mut self, listing: Listing) {
        let same = match &listing.spot {
            Spot::Here(dir) => self.remote.is_none() && util::same_path(dir, &self.cwd),
            Spot::There(view) => self.remote.as_ref().is_some_and(|here| here.dir == view.dir),
            Spot::Zip { .. } | Spot::ZipAgain { .. } => true,
        };
        if same {
            let name = self.current().map(|e| e.name.clone());
            self.take_in(listing);
            if let Some(name) = name {
                self.put_cursor_on(&name);
            }
            return;
        }
        // The folder has gone (deleted, drive removed), and this is the nearest one that hasn't.
        if self.remote.is_none() {
            self.back.push(self.cwd.clone());
        }
        // Marks are by name, and these names mean something else here.
        self.marked.clear();
        self.dir_sizes.clear();
        (self.cursor, self.offset) = (0, 0);
        self.take_in(listing);
        if self.remote.is_none() {
            self.remember_drive();
        }
    }

    /// A folder on another computer can't be read. If the server is only slow the panel stays
    /// as it is. If the connection is gone, back to this computer. `lost` tells the user.
    fn not_refreshed(&mut self, refused: &Refused) {
        let Some(view) = &self.remote else { return };
        let target = view.connection.target.to_string();
        if refused.error.kind() == io::ErrorKind::TimedOut {
            self.lost = Some(format!("{target} doesn't answer: the panel shows what it last read."));
            return;
        }
        self.remote = None;
        self.archive = None;
        self.marked.clear();
        self.dir_sizes.clear();
        self.lost = Some(format!("The connection to {target} is gone."));
        self.reload();
    }

    /// The folder under any zip on show.
    fn under(&self) -> Spot {
        self.remote.clone().map_or_else(|| Spot::Here(self.cwd.clone()), Spot::There)
    }

    /// Sorts by `by`. Choosing the order it already has flips it (A to Z, then Z to A), like
    /// clicking a column heading twice.
    pub fn choose_sort(&mut self, by: SortBy) {
        if self.sort == by {
            self.reverse = !self.reverse;
        } else {
            (self.sort, self.reverse) = (by, false);
        }
        self.resort();
    }

    /// Reads the place again, keeping the cursor on the same name when it still exists. A
    /// folder that is quick is in when this returns.
    pub fn reload(&mut self) {
        if self.look_again() {
            self.wait_a_moment();
        }
    }

    /// Starts reading the place again, for when it comes; if something is being read, that
    /// may be from before the change, so afterwards. Returns whether it started.
    pub fn look_again(&mut self) -> bool {
        if self.reading.is_some() {
            self.read_again = true;
            return false;
        }
        let mut spots = Vec::new();
        // A zip that changed is read again, staying in the same folder inside if that is still
        // there; gone, the panel leaves it.
        if let Some(view) = &self.archive {
            spots.push(Spot::ZipAgain { file: view.archive.file.clone(), stamp: view.archive.stamp });
        }
        spots.push(self.under());
        // A folder gone on another computer: the nearest one above it.
        if let Some(view) = &self.remote {
            let mut dir = view.dir.clone();
            while let Some(parent) = ssh::parent(&dir) {
                spots.push(Spot::There(RemoteView { connection: view.connection.clone(), dir: parent.clone() }));
                dir = parent;
            }
        }
        self.begin(spots, self.remote.is_none(), Purpose::Refresh);
        true
    }

    #[cfg(test)]
    pub fn change_dir(&mut self, dir: PathBuf) {
        self.go_to_first(&[dir], false);
    }

    /// Goes to the first of `dirs`, folders or addresses of ones on another computer, that can
    /// be read; where the panel was is where Alt+Left goes back to. `came` says how it ended,
    /// once it has. `quiet`: nothing is said if none can.
    pub fn go_to_first(&mut self, dirs: &[PathBuf], quiet: bool) {
        let spots: Result<Vec<Spot>, String> = dirs.iter().map(|dir| spot_of(dir)).collect();
        let spots = match spots {
            Ok(spots) => spots,
            Err(why) => {
                self.came = Some(Came::Failed(why));
                return;
            }
        };
        // Asked again while on the way there: nothing new.
        let going_there = |reading: &Reading| !matches!(reading.purpose, Purpose::Refresh) && spots.first().is_some_and(|spot| spot.place() == reading.place);
        if self.reading.as_ref().is_some_and(going_there) {
            return;
        }
        self.begin(spots, false, Purpose::Go { quiet });
        self.wait_a_moment();
    }

    /// Alt+Left: back to the folder before this one (passing over any that are gone).
    pub fn go_back(&mut self) {
        self.step(true);
    }

    /// Alt+Right: forward again, after going back.
    pub fn go_forward(&mut self) {
        self.step(false);
    }

    fn step(&mut self, back: bool) {
        let stack = self.steps(back);
        let reachable = stack.iter().enumerate().rev().filter_map(|(depth, dir)| Some((depth, spot_of(dir).ok()?)));
        let (depths, spots): (Vec<usize>, Vec<Spot>) = reachable.unzip();
        if spots.is_empty() {
            stack.clear();
            self.came = Some(Came::NoFurther { back });
            return;
        }
        self.begin(spots, false, Purpose::Step { back, depths });
        self.wait_a_moment();
    }

    /// Enter on a zip: the panel shows what is in it. `shown` is what the title calls it.
    /// `came` says how it ended, once it has.
    pub fn open_archive(&mut self, file: &Path, shown: PathBuf) {
        self.begin(vec![Spot::Zip { file: file.to_path_buf(), shown }], false, Purpose::OpenZip);
        self.wait_a_moment();
    }

    /// Into folder `name` inside the archive.
    pub fn archive_enter(&mut self, name: &str) {
        if let Some(view) = self.archive.as_mut() {
            view.inner = archive::join(&view.inner, name);
            self.enter_inner();
        }
    }

    /// Up inside the archive; at its top, out of it (to the archive it came from, or the
    /// folder), the cursor on the zip.
    pub fn archive_up(&mut self) {
        let Some(view) = self.archive.as_mut() else { return };
        let left = if view.inner.is_empty() {
            let name = view.shown.file_name().map(|n| n.to_string_lossy().into_owned());
            // Out of the last zip: what is around it is a folder to read.
            if view.outer.is_none() {
                self.begin(vec![self.under()], false, Purpose::Go { quiet: false });
                self.wait_a_moment();
                if let Some(name) = name {
                    self.select_name(&name);
                }
                return;
            }
            self.archive = self.archive.take().and_then(|view| view.outer).map(|outer| *outer);
            name
        } else {
            let name = view.inner.rsplit('/').next().map(str::to_string);
            view.inner = archive::parent_of(&view.inner).to_string();
            name
        };
        self.enter_inner();
        if let Some(name) = left {
            self.put_cursor_on(&name);
        }
    }

    /// A folder inside the zip, which is all in memory. What was being read was for another.
    fn enter_inner(&mut self) {
        self.reading = None;
        self.read_again = false;
        self.cursor_to = None;
        self.marked.clear();
        self.dir_sizes.clear();
        (self.cursor, self.offset) = (0, 0);
        self.list_zip();
    }

    /// Where the panel is: its folder, on another computer its address (`nas:/home/me`).
    pub fn place(&self) -> PathBuf {
        self.remote.as_ref().map_or_else(|| self.cwd.clone(), |view| view.place(&view.dir))
    }

    /// What the panel shows: `place`, or inside a zip (`E:\x\photos.zip\2024`).
    pub fn location(&self) -> PathBuf {
        self.archive.as_ref().map_or_else(|| self.place(), ArchiveView::location)
    }

    /// On another computer: the paths there of the marked entries (or the one under the cursor).
    pub fn remote_selection(&self) -> Vec<String> {
        let Some(view) = &self.remote else { return Vec::new() };
        let names: Vec<&str> = if self.marked.is_empty() {
            self.current().filter(|e| !e.is_parent).map(|e| vec![e.name.as_str()]).unwrap_or_default()
        } else {
            self.entries.iter().filter(|e| self.is_marked(e)).map(|e| e.name.as_str()).collect()
        };
        names.iter().map(|name| ssh::join(&view.dir, name)).collect()
    }

    /// The archive item of an entry of this panel.
    pub fn archive_item(&self, entry: &Entry) -> Option<&archive::Item> {
        let view = self.archive.as_ref()?;
        view.archive.find(&archive::join(&view.inner, &entry.name))
    }

    /// In an archive: the zip on disk and the paths inside it of the marked entries (or the
    /// one under the cursor).
    pub fn archive_selection(&self) -> Option<(PathBuf, Vec<String>)> {
        let view = self.archive.as_ref()?;
        let names: Vec<&str> = if self.marked.is_empty() {
            self.current().filter(|e| !e.is_parent).map(|e| vec![e.name.as_str()]).unwrap_or_default()
        } else {
            self.entries.iter().filter(|e| self.is_marked(e)).map(|e| e.name.as_str()).collect()
        };
        Some((view.archive.file.clone(), names.iter().map(|name| archive::join(&view.inner, name)).collect()))
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    /// Puts the cursor on `name`; with something being read, as soon as that is in. Returns
    /// whether it is there now.
    pub fn select_name(&mut self, name: &str) -> bool {
        if self.reading.is_some() {
            self.cursor_to = Some(name.to_string());
            return false;
        }
        self.put_cursor_on(name)
    }

    fn put_cursor_on(&mut self, name: &str) -> bool {
        let found = self.entries.iter().position(|e| e.name == name).or_else(|| {
            let lower = name.to_lowercase();
            self.entries.iter().position(|e| e.name.to_lowercase() == lower)
        });
        if let Some(i) = found {
            self.cursor = i;
        }
        found.is_some()
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
    }

    pub fn page_up(&mut self) {
        self.move_by(-(self.page.max(1) as isize - 1).max(1));
    }

    pub fn page_down(&mut self) {
        self.move_by((self.page.max(1) as isize - 1).max(1));
    }

    /// Scrolls so the cursor is inside a list `height` rows tall.
    pub fn ensure_visible(&mut self, height: usize) {
        self.page = height.max(1);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.page {
            self.offset = self.cursor + 1 - self.page;
        }
        self.offset = self.offset.min(self.entries.len().saturating_sub(self.page));
    }

    /// Jumps to the next entry whose name starts with `prefix` (case-insensitive).
    pub fn find_prefix(&mut self, prefix: &str, skip_current: bool) -> bool {
        let prefix = prefix.to_lowercase();
        let n = self.entries.len();
        let start = if skip_current { 1 } else { 0 };
        for step in start..n + start {
            let i = (self.cursor + step) % n.max(1);
            if self.entries[i].name.to_lowercase().starts_with(&prefix) {
                self.cursor = i;
                return true;
            }
        }
        false
    }

    pub fn is_marked(&self, entry: &Entry) -> bool {
        !entry.is_parent && self.marked.contains(&entry.name)
    }

    pub fn toggle_mark(&mut self) {
        let Some(entry) = self.current() else { return };
        if entry.is_parent {
            return;
        }
        let name = entry.name.clone();
        if !self.marked.remove(&name) {
            self.marked.insert(name);
        }
    }

    /// Marks or unmarks files (not directories) matching a wildcard pattern, like mc's + and -.
    pub fn mark_matching(&mut self, pattern: &str, mark: bool) -> usize {
        let names: Vec<String> = self
            .entries
            .iter()
            .filter(|e| !e.is_parent && !e.is_dir && util::glob_match(pattern, &e.name))
            .map(|e| e.name.clone())
            .collect();
        for name in &names {
            if mark {
                self.marked.insert(name.clone());
            } else {
                self.marked.remove(name);
            }
        }
        names.len()
    }

    /// Ctrl+A: everything marked, files and folders.
    pub fn mark_all(&mut self) {
        let names: Vec<String> = self.entries.iter().filter(|e| !e.is_parent).map(|e| e.name.clone()).collect();
        self.marked.extend(names);
    }

    /// Inverts the marks on files, like mc's *.
    pub fn invert_marks(&mut self) {
        for entry in self.entries.iter().filter(|e| !e.is_parent && !e.is_dir) {
            if !self.marked.remove(&entry.name) {
                self.marked.insert(entry.name.clone());
            }
        }
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    /// The marked entries, or the entry under the cursor when nothing is marked.
    pub fn selection(&self) -> Vec<PathBuf> {
        if self.marked.is_empty() {
            return self
                .current()
                .filter(|e| !e.is_parent)
                .map(|e| vec![e.path.clone()])
                .unwrap_or_default();
        }
        self.entries
            .iter()
            .filter(|e| self.is_marked(e))
            .map(|e| e.path.clone())
            .collect()
    }

    /// Number of marked entries and their total size (directories count once measured).
    pub fn marked_summary(&self) -> Option<(usize, u64)> {
        if self.marked.is_empty() {
            return None;
        }
        let marked = self.entries.iter().filter(|e| self.is_marked(e));
        Some(marked.fold((0, 0), |(n, bytes), e| (n + 1, bytes + effective_size(e, &self.dir_sizes))))
    }

    pub fn dir_size(&self, entry: &Entry) -> Option<DirSize> {
        if entry.is_dir && !entry.is_parent { self.dir_sizes.get(&entry.name).copied() } else { None }
    }

    pub fn has_pending_sizes(&self) -> bool {
        self.dir_sizes.values().any(|s| !s.done)
    }

    /// Records a size result for `dir` if it is a subdirectory this panel asked about.
    pub fn set_dir_size(&mut self, dir: &Path, bytes: u64, done: bool) {
        if !dir.parent().is_some_and(|parent| util::same_path(parent, &self.cwd)) {
            return;
        }
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
        let Some(size) = self.dir_sizes.get_mut(&name).filter(|s| !s.done) else { return };
        *size = DirSize { bytes, done };
        if done && self.sort == SortBy::Size {
            self.resort();
        }
    }

    /// Size of everything listed, once every subdirectory has been measured.
    pub fn total(&self) -> Option<u64> {
        if self.dir_sizes.is_empty() {
            return None;
        }
        let mut total = 0;
        for entry in self.entries.iter().filter(|e| !e.is_parent) {
            if entry.is_dir && !self.dir_size(entry).is_some_and(|s| s.done) {
                return None;
            }
            total += effective_size(entry, &self.dir_sizes);
        }
        Some(total)
    }

    /// Sorts again without re-reading the directory, keeping the cursor on the same entry.
    fn resort(&mut self) {
        let name = self.current().map(|e| e.name.clone());
        let start = usize::from(self.entries.first().is_some_and(|e| e.is_parent));
        sort_entries(&mut self.entries[start..], self.sort, self.reverse, &self.dir_sizes);
        if let Some(name) = name {
            self.put_cursor_on(&name);
        }
    }
}

/// What `dir`, a folder or the address of one on another computer, is to a reading.
fn spot_of(dir: &Path) -> Result<Spot, String> {
    if let Some((target, path)) = ssh::parse_exact(&dir.to_string_lossy()) {
        let connection = ssh::find(&target).ok_or_else(|| format!("Not connected to {target}."))?;
        let dir = ssh::absolute(&connection.home, &path);
        return Ok(Spot::There(RemoteView { connection, dir }));
    }
    Ok(Spot::Here(util::normalize(dir)))
}

/// A file's size, or a directory's measured size (0 until measured).
fn effective_size(entry: &Entry, sizes: &HashMap<String, DirSize>) -> u64 {
    if !entry.is_dir {
        return entry.size;
    }
    sizes.get(&entry.name).filter(|s| s.done).map_or(0, |s| s.bytes)
}

/// The entries of the folder a panel shows inside a zip.
fn archive_entries(view: &ArchiveView, filter: Option<&NameFilter>) -> Vec<Entry> {
    let location = view.location();
    view.archive
        .children(&view.inner)
        .filter(|item| item.is_dir || filter.is_none_or(|f| f.matches(item.name())))
        .map(|item| Entry {
            name: item.name().to_string(),
            path: location.join(item.name()),
            is_dir: item.is_dir,
            is_link: false,
            is_exec: false,
            is_parent: false,
            is_hidden: item.name().starts_with('.'),
            size: item.size,
            modified: item.modified,
            leads_to: None,
            sort_name: item.name().to_lowercase(),
        })
        .collect()
}

/// The entries of a folder on another computer. A link shows what it points to, as here.
pub fn remote_entries(view: &RemoteView, show_hidden: bool, filter: Option<&NameFilter>) -> io::Result<Vec<Entry>> {
    let sftp = &view.connection.sftp;
    let mut items = sftp.read_dir(&view.dir)?;
    items.retain(|item| show_hidden || !item.name.starts_with('.'));
    // What the links lead to, all at once: one by one, a folder of them takes seconds.
    let links: Vec<usize> = (0..items.len()).filter(|&i| items[i].attrs.is_link()).collect();
    let paths: Vec<String> = links.iter().map(|&i| ssh::join(&view.dir, &items[i].name)).collect();
    let led_to: HashMap<usize, _> = links.into_iter().zip(sftp.stat_all(&paths)).collect();
    let mut out = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        let is_hidden = item.name.starts_with('.');
        let path = ssh::join(&view.dir, &item.name);
        let is_link = item.attrs.is_link();
        let attrs = led_to.get(&i).and_then(|attrs| attrs.as_ref().ok()).copied().unwrap_or(item.attrs);
        let is_dir = attrs.is_dir();
        if !is_dir && filter.is_some_and(|f| !f.matches(&item.name)) {
            continue;
        }
        out.push(Entry {
            sort_name: item.name.to_lowercase(),
            is_exec: attrs.is_executable(),
            size: if is_dir { 0 } else { attrs.size.unwrap_or(0) },
            modified: attrs.modified(),
            path: view.place(&path),
            name: item.name,
            is_dir,
            is_link,
            is_parent: false,
            is_hidden,
            leads_to: None,
        });
    }
    Ok(out)
}

/// The entries of a folder here; given up on once `stop` says nobody is waiting for them.
pub fn read_entries(dir: &Path, show_hidden: bool, filter: Option<&NameFilter>, stop: &AtomicBool) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    // On a Windows drive in WSL every file looks executable.
    let exec_bits = platform::exec_bits_mean_something(dir);
    for item in crate::lfs::read_dir(dir)? {
        if stop.load(Atomic::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        let own = item.metadata().ok();
        let is_hidden = platform::is_hidden(&name, own.as_ref());
        if !show_hidden && is_hidden {
            continue;
        }
        let is_link = own.as_ref().is_some_and(|m| m.file_type().is_symlink());
        let path = dir.join(item.file_name());
        // For links, show what they point to (a broken link falls back to the link itself).
        let target = if is_link { crate::lfs::metadata(&path).ok() } else { None };
        let meta = target.as_ref().or(own.as_ref());
        let is_dir = meta.is_some_and(|m| m.is_dir());
        if !is_dir && filter.is_some_and(|f| !f.matches(&name)) {
            continue;
        }
        // Read here and not when it is on screen, where a share may not answer.
        let leads_to = is_link.then(|| crate::lfs::read_link(&path).ok()).flatten().map(|target| {
            let target = target.display().to_string();
            target.strip_prefix(r"\\?\").map(String::from).unwrap_or(target)
        });
        out.push(Entry {
            sort_name: name.to_lowercase(),
            is_exec: !is_dir && exec_bits && platform::is_executable(&name, meta),
            size: if is_dir { 0 } else { meta.map_or(0, |m| m.len()) },
            modified: meta.and_then(|m| m.modified().ok()),
            leads_to,
            name,
            path,
            is_dir,
            is_link,
            is_parent: false,
            is_hidden,
        });
    }
    Ok(out)
}

/// Directories first, then by `sort` (names break ties); `reverse` turns the order round but
/// keeps directories on top, as mc and Total Commander do.
pub fn sort_entries(entries: &mut [Entry], sort: SortBy, reverse: bool, sizes: &HashMap<String, DirSize>) {
    entries.sort_by(|a, b| {
        let order = match sort {
            SortBy::Name => Ordering::Equal,
            SortBy::Extension => a.extension().cmp(b.extension()),
            SortBy::Size => effective_size(b, sizes).cmp(&effective_size(a, sizes)),
            SortBy::Time => b.modified.cmp(&a.modified),
        }
        .then_with(|| name_order(&a.sort_name, &b.sort_name));
        b.is_dir.cmp(&a.is_dir).then(if reverse { order.reverse() } else { order })
    });
}

pub fn system_root() -> PathBuf {
    if cfg!(windows) {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        PathBuf::from(format!("{drive}\\"))
    } else {
        PathBuf::from("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn names(panel: &Panel) -> Vec<&str> {
        panel.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn lists_parent_then_directories_then_files() {
        let tmp = TempDir::new("panel-list");
        tmp.file("b.txt", "bb");
        tmp.file("A.rs", "a");
        tmp.dir("zeta");
        tmp.dir("Alpha");
        let panel = Panel::new(tmp.path().to_path_buf(), true);
        assert_eq!(names(&panel), ["..", "Alpha", "zeta", "A.rs", "b.txt"]);
        assert_eq!(panel.entries[4].size, 2);
    }

    #[test]
    fn sorting_by_size_and_extension() {
        let tmp = TempDir::new("panel-sort");
        tmp.file("small.txt", "1");
        tmp.file("big.md", "1234567");
        tmp.file("mid.rs", "123");
        let mut panel = Panel::new(tmp.path().to_path_buf(), true);
        panel.sort = SortBy::Size;
        panel.reload();
        assert_eq!(names(&panel), ["..", "big.md", "mid.rs", "small.txt"]);
        panel.sort = SortBy::Extension;
        panel.reload();
        assert_eq!(names(&panel), ["..", "big.md", "mid.rs", "small.txt"]);
    }

    #[test]
    fn choosing_the_same_order_again_reverses_it() {
        let tmp = TempDir::new("panel-reverse");
        tmp.dir("Alpha");
        tmp.dir("zeta");
        tmp.file("a.txt", "1");
        tmp.file("B.md", "123");
        tmp.file("c.rs", "12");
        let mut panel = Panel::new(tmp.path().to_path_buf(), true);
        assert!(panel.select_name("B.md"));
        assert_eq!(names(&panel), ["..", "Alpha", "zeta", "a.txt", "B.md", "c.rs"]);

        // Name again: Z to A, directories still on top, the cursor stays on its file.
        panel.choose_sort(SortBy::Name);
        assert!(panel.reverse);
        assert_eq!(names(&panel), ["..", "zeta", "Alpha", "c.rs", "B.md", "a.txt"]);
        assert_eq!(panel.current().unwrap().name, "B.md");

        // Another order starts the usual way round (largest first), and again flips it.
        panel.choose_sort(SortBy::Size);
        assert_eq!((panel.sort, panel.reverse), (SortBy::Size, false));
        assert_eq!(names(&panel), ["..", "Alpha", "zeta", "B.md", "c.rs", "a.txt"]);
        panel.choose_sort(SortBy::Size);
        assert_eq!(names(&panel), ["..", "zeta", "Alpha", "a.txt", "c.rs", "B.md"]);

        // A reload keeps the order.
        panel.reload();
        assert_eq!(names(&panel), ["..", "zeta", "Alpha", "a.txt", "c.rs", "B.md"]);
    }

    #[test]
    fn going_up_selects_the_directory_we_left() {
        let tmp = TempDir::new("panel-up");
        tmp.dir("one");
        tmp.dir("two");
        let mut panel = Panel::new(tmp.path().join("two"), true);
        let parent = panel.cwd.parent().unwrap().to_path_buf();
        panel.change_dir(parent);
        assert_eq!(panel.current().unwrap().name, "two");
    }

    #[cfg(windows)]
    #[test]
    fn remembers_the_last_folder_on_each_drive() {
        let tmp = TempDir::new("panel-drives");
        let deep = tmp.dir("deep/er");
        let mut panel = Panel::new(deep.clone(), true);
        let root = PathBuf::from(format!("{}\\", util::volume_key(&deep).unwrap().to_uppercase()));
        // Somewhere on another (non-existent here) drive and back: the folder is remembered.
        assert!(util::same_path(&panel.places_on_drive(&root)[0], &deep));
        assert_eq!(panel.places_on_drive(Path::new(r"Q:\")), [PathBuf::from(r"Q:\")], "unknown drive: root");
        panel.change_dir(tmp.path().to_path_buf());
        assert!(util::same_path(&panel.places_on_drive(&root)[0], tmp.path()), "the latest visit wins");
        // A remembered folder that was deleted falls back to the root.
        let places = panel.places_on_drive(&root);
        panel.change_dir(deep.clone());
        drop(tmp);
        panel.go_to_first(&places, false);
        assert_eq!(panel.cwd, root);
    }

    #[test]
    fn failed_change_dir_keeps_the_old_directory() {
        let tmp = TempDir::new("panel-fail");
        let mut panel = Panel::new(tmp.path().to_path_buf(), true);
        panel.change_dir(tmp.path().join("missing"));
        assert!(matches!(panel.came, Some(Came::Failed(_))));
        assert!(util::same_path(&panel.cwd, tmp.path()));
    }

    #[test]
    fn marking_and_selection() {
        let tmp = TempDir::new("panel-mark");
        tmp.file("a.txt", "");
        tmp.file("b.txt", "");
        tmp.file("c.rs", "");
        tmp.dir("dir.txt");
        let mut panel = Panel::new(tmp.path().to_path_buf(), true);
        assert_eq!(panel.mark_matching("*.txt", true), 2, "directories are not marked by pattern");
        assert_eq!(panel.selection().len(), 2);
        panel.invert_marks();
        let selected: Vec<_> = panel.selection().iter().map(|p| p.file_name().unwrap().to_owned()).collect();
        assert_eq!(selected, ["c.rs"]);
        panel.clear_marks();
        panel.home();
        assert!(panel.selection().is_empty(), "'..' is never selected");
        panel.move_by(1);
        assert_eq!(panel.selection(), [tmp.path().join("dir.txt")]);
    }

    #[test]
    fn quick_search_finds_by_prefix() {
        let tmp = TempDir::new("panel-search");
        tmp.file("apple", "");
        tmp.file("banana", "");
        tmp.file("Blueberry", "");
        let mut panel = Panel::new(tmp.path().to_path_buf(), true);
        assert!(panel.find_prefix("b", false));
        assert_eq!(panel.current().unwrap().name, "banana");
        assert!(panel.find_prefix("b", true));
        assert_eq!(panel.current().unwrap().name, "Blueberry");
        assert!(!panel.find_prefix("x", false));
    }
}
