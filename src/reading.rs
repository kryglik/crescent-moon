//! Reading a folder for a panel on a thread of its own: a share that takes its time or has gone
//! away, or a server gone quiet, mustn't hold the screen.
//!
//! A panel says what it wants read (`Asked`) and goes on showing what it shows until the answer
//! is in, so its place and its list always change together. Most folders are in within a moment
//! (`Reading::wait`), before the screen is drawn again, and look as if nothing had been on
//! another thread; what takes longer comes in through `Panel::poll`. A reading nobody waits
//! for any more ends by itself, whatever it is stuck in: it only has nobody left to tell.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::archive::Archive;
use crate::find::NameFilter;
use crate::git::Repo;
use crate::panel::{self, DirSize, Entry, RemoteView, SortBy};
use crate::{platform, sftp, ssh, util};

/// How long the screen waits for a folder. Longer than nearly every folder on a disk takes,
/// and too short for anyone to feel. Keys typed meanwhile wait in the terminal, as they always did.
const A_MOMENT: Duration = Duration::from_millis(100);
/// From when a wait is said on the panel. Until then a key is typed ahead, for the folder that
/// is coming; once the wait shows, a key isn't blind any more, and nothing keeps it.
const SHOWN_AFTER: Duration = Duration::from_millis(500);

thread_local! {
    /// How long this thread waits for a reading. Tests wait for it to be done, as what they
    /// look at next is what was read; the ones about waiting say otherwise.
    static PATIENCE: Cell<Duration> = const { Cell::new(if cfg!(test) { Duration::from_secs(60) } else { A_MOMENT }) };
}

/// How long this thread waits for something that may be on a share.
pub fn a_moment() -> Duration {
    PATIENCE.get()
}

/// Somewhere a panel can be.
#[derive(Clone)]
pub enum Spot {
    /// A folder on this computer.
    Here(PathBuf),
    /// A folder on another computer.
    There(RemoteView),
    /// A zip to open; `shown` is what the title calls it.
    Zip { file: PathBuf, shown: PathBuf },
    /// The zip on show, read again if it changed since it was read (`stamp`).
    ZipAgain { file: PathBuf, stamp: (u64, Option<SystemTime>) },
}

impl Spot {
    /// What the user calls it.
    pub fn place(&self) -> PathBuf {
        match self {
            Spot::Here(dir) => dir.clone(),
            Spot::There(view) => view.place(&view.dir),
            Spot::Zip { shown, .. } => shown.clone(),
            Spot::ZipAgain { file, .. } => file.clone(),
        }
    }
}

/// What a panel wants read, and how it lists things.
pub struct Asked {
    /// Tried in turn: the first one that can be read is the answer.
    pub spots: Vec<Spot>,
    /// With none of them readable, the nearest place that is (see `refuge`).
    pub refuge: bool,
    /// Only a folder that isn't there gives way to the next spot. One that is, and can't be
    /// read, is worth saying.
    pub past_gone_only: bool,
    pub show_hidden: bool,
    pub filter: Option<String>,
    pub sort: SortBy,
    pub reverse: bool,
    pub sizes: HashMap<String, DirSize>,
}

/// What was read.
pub struct Listing {
    pub spot: Spot,
    /// How many spots before this one couldn't be read.
    pub tried: usize,
    pub entries: Vec<Entry>,
    pub space: Option<(u64, u64)>,
    pub repo: Option<Repo>,
    /// The zip: always for `Spot::Zip`, for `Spot::ZipAgain` only if it changed.
    pub zip: Option<Archive>,
    /// The order `entries` are in, which may have changed by the time they arrive.
    pub order: (SortBy, bool),
}

/// Nothing could be read.
pub struct Refused {
    pub place: PathBuf,
    pub error: io::Error,
    /// Which of the spots it stopped at.
    pub tried: usize,
    /// It stopped for a reason worth saying, not for having run out of places.
    pub gave_up: bool,
}

impl Refused {
    pub fn message(&self) -> String {
        format!("Cannot open \"{}\":\n{}", self.place.display(), self.error)
    }
}

pub type Answer = Result<Listing, Refused>;

/// What a reading is for, and so what becomes of the panel when it is in.
pub enum Purpose {
    /// Where the panel starts: it has nothing to show before that.
    Start,
    /// The place on show, again.
    Refresh,
    /// Somewhere else. `quiet`: not worth a word if that can't be.
    Go { quiet: bool },
    /// Alt+Left or Alt+Right; each spot's place in that list is in `depths`.
    Step { back: bool, depths: Vec<usize> },
    /// Into a zip.
    OpenZip,
}

/// A reading under way.
pub struct Reading {
    pub purpose: Purpose,
    /// Where to, as the user knows it.
    pub place: PathBuf,
    pub started: Instant,
    answer: Receiver<Answer>,
    stop: Stop,
    /// The seconds on the panel (see `Panel::poll`).
    said: Option<u64>,
}

/// Tells the thread, when the reading goes, that nobody is waiting for it any more.
struct Stop(Arc<AtomicBool>);

impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Reading {
    pub fn start(asked: Asked, purpose: Purpose) -> Reading {
        let place = asked.spots.first().map_or_else(PathBuf::new, Spot::place);
        let (tx, answer) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        // The screen isn't waiting for this one, and a disk spinning up over there takes a while.
        let patience = sftp::patience().map(|limit| limit * 3);
        thread::spawn(move || {
            if let Some(limit) = patience {
                sftp::be_impatient(limit);
            }
            // Nobody takes it if the panel has moved on.
            let _ = tx.send(read(&asked, &stopping));
        });
        Reading { purpose, place, started: Instant::now(), answer, stop: Stop(stop), said: None }
    }

    /// The answer, if it comes within a moment.
    pub fn wait(&self) -> Option<Answer> {
        match self.answer.recv_timeout(a_moment()) {
            Ok(answer) => Some(answer),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Err(self.ended_without_a_word())),
        }
    }

    /// The answer, if it is in.
    pub fn take(&self) -> Option<Answer> {
        match self.answer.try_recv() {
            Ok(answer) => Some(answer),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(self.ended_without_a_word())),
        }
    }

    /// Over, and a moment for the thread to let go of the folder.
    pub fn stop_and_wait(self) {
        self.stop.0.store(true, Ordering::Relaxed);
        let _ = self.answer.recv_timeout(a_moment());
    }

    fn ended_without_a_word(&self) -> Refused {
        Refused { place: self.place.clone(), error: io::Error::other("the reading ended without a word"), tried: 0, gave_up: true }
    }

    /// Whether the wait has gone on long enough to be said.
    pub fn is_long(&self) -> bool {
        self.started.elapsed() >= SHOWN_AFTER
    }

    /// Whether what the panel says about the wait has changed since this was last asked.
    pub fn shows_anew(&mut self) -> bool {
        let seconds = self.is_long().then(|| self.started.elapsed().as_secs());
        std::mem::replace(&mut self.said, seconds) != seconds
    }

    /// What to say about the wait, once it is long enough.
    pub fn note(&self) -> Option<String> {
        if !self.is_long() {
            return None;
        }
        let name = self.place.file_name().map_or_else(|| self.place.display().to_string(), |n| n.to_string_lossy().into_owned());
        Some(match self.started.elapsed().as_secs() {
            0 => format!("Reading {name}… Esc stops"),
            seconds => format!("Reading {name}… {seconds} s, Esc stops"),
        })
    }
}

fn read(asked: &Asked, stop: &AtomicBool) -> Answer {
    let mut refused = None;
    for (tried, spot) in asked.spots.iter().enumerate() {
        match list(spot, asked, stop) {
            Ok(listing) => return Ok(Listing { tried, ..listing }),
            Err(error) => {
                let over = no_use_going_on(spot, &error) || (asked.past_gone_only && is_there(spot));
                refused = Some(Refused { place: spot.place(), error, tried, gave_up: over });
                if over {
                    break;
                }
            }
        }
    }
    let gone = asked.spots.iter().rev().find_map(|spot| match spot {
        Spot::Here(dir) => Some(dir),
        _ => None,
    });
    if let (true, Some(gone)) = (asked.refuge, gone) {
        for dir in refuge(gone) {
            let spot = Spot::Here(dir);
            if let Ok(listing) = list(&spot, asked, stop) {
                return Ok(Listing { tried: asked.spots.len(), ..listing });
            }
        }
    }
    Err(refused.unwrap_or_else(|| Refused { place: PathBuf::new(), error: io::Error::other("nowhere to read"), tried: 0, gave_up: true }))
}

/// Whether a spot that couldn't be read exists all the same.
fn is_there(spot: &Spot) -> bool {
    match spot {
        Spot::Here(dir) => crate::lfs::is_dir(dir),
        _ => true,
    }
}

/// A server that doesn't answer, or a connection that has gone, won't do better for the folder
/// above; and a reading nobody waits for is over.
fn no_use_going_on(spot: &Spot, error: &io::Error) -> bool {
    match spot {
        Spot::There(view) => error.kind() == io::ErrorKind::TimedOut || view.connection.sftp.lost().is_some(),
        _ => error.kind() == io::ErrorKind::Interrupted,
    }
}

/// Where a panel goes when its folder `gone` can't be read: the nearest one above that can,
/// home, or the top of the system. With the whole drive gone (a share offline) each step up
/// would wait for it again, so home at once.
fn refuge(gone: &Path) -> Vec<PathBuf> {
    let drive = if cfg!(windows) { gone.ancestors().last().map(Path::to_path_buf) } else { platform::mount_point(gone) };
    let above = match (gone.parent(), drive) {
        (Some(parent), Some(root)) if fs::metadata(&root).is_ok() => parent.ancestors().map(Path::to_path_buf).collect(),
        _ => Vec::new(),
    };
    above.into_iter().chain(util::home_dir()).chain([panel::system_root()]).collect()
}

fn list(spot: &Spot, asked: &Asked, stop: &AtomicBool) -> io::Result<Listing> {
    let listing = list_at_once(spot, asked, stop);
    #[cfg(test)]
    tests::while_held(spot);
    listing
}

fn list_at_once(spot: &Spot, asked: &Asked, stop: &AtomicBool) -> io::Result<Listing> {
    let filter = asked.filter.as_deref().map(NameFilter::parse);
    let mut listing = Listing {
        spot: spot.clone(),
        tried: 0,
        entries: Vec::new(),
        space: None,
        repo: None,
        zip: None,
        order: (asked.sort, asked.reverse),
    };
    match spot {
        Spot::Here(dir) => {
            listing.entries = panel::read_entries(dir, asked.show_hidden, filter.as_ref(), stop)?;
            panel::sort_entries(&mut listing.entries, asked.sort, asked.reverse, &asked.sizes);
            // On Windows a drive root gets ".." too: it opens the drive picker.
            if dir.parent().is_some() || cfg!(windows) {
                let modified = fs::metadata(dir).and_then(|m| m.modified()).ok();
                listing.entries.insert(0, Entry::up(dir.parent().unwrap_or(dir).to_path_buf(), modified));
            }
            listing.space = platform::disk_space(dir);
            listing.repo = Repo::find(dir);
        }
        Spot::There(view) => {
            listing.entries = panel::remote_entries(view, asked.show_hidden, filter.as_ref())?;
            panel::sort_entries(&mut listing.entries, asked.sort, asked.reverse, &asked.sizes);
            // At the top of the other computer, ".." leads back to this one: `Panel::take_in`
            // knows which folder that is.
            let up = ssh::parent(&view.dir).map_or_else(PathBuf::new, |parent| view.place(&parent));
            listing.entries.insert(0, Entry::up(up, None));
            listing.space = view.connection.sftp.space(&view.dir).map(|space| (space.free, space.total));
        }
        Spot::Zip { file, .. } => listing.zip = Some(Archive::open(file)?),
        Spot::ZipAgain { file, stamp } => {
            if crate::archive::stamp(file) != *stamp {
                listing.zip = Some(Archive::open(file)?);
            }
        }
    }
    Ok(listing)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::app::App;
    use crate::dialog::{Dialog, InputAction};
    use crate::panel::{Came, Panel};
    use crate::testutil::{TempDir, config};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::{Condvar, Mutex};

    static HELD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static FREED: Condvar = Condvar::new();

    /// What is read of `place` comes as late as this lives, as from a slow share: it is as it
    /// was when it was read, and the thread is still at it.
    pub struct Hold(PathBuf);

    pub fn hold(place: &Path) -> Hold {
        HELD.lock().unwrap().push(place.to_path_buf());
        Hold(place.to_path_buf())
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            HELD.lock().unwrap().retain(|place| *place != self.0);
            FREED.notify_all();
        }
    }

    pub(super) fn while_held(spot: &Spot) {
        let place = spot.place();
        let mut held = HELD.lock().unwrap();
        while held.contains(&place) {
            held = FREED.wait(held).unwrap();
        }
    }

    /// This thread doesn't wait for readings at all: everything comes in through `poll`.
    pub fn wait_for_nothing() {
        PATIENCE.set(Duration::ZERO);
    }

    /// The wait is older than it is: it shows on the panel.
    pub fn age(reading: &mut Reading) {
        reading.started -= SHOWN_AFTER;
    }

    /// An app with its left panel in `dir`, which from now on doesn't wait for any folder.
    fn app_in(dir: &Path) -> (App, TempDir) {
        let other = TempDir::new("reading-other");
        let app = App::new(dir.to_path_buf(), other.path().to_path_buf(), config());
        wait_for_nothing();
        (app, other)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn press_alt(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::ALT));
    }

    /// As the main loop would, until nothing is being read.
    fn settle(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            app.tick();
            if !app.panels.iter().chain(app.tabs.iter().flatten()).any(Panel::is_reading) {
                break;
            }
            assert!(Instant::now() < deadline, "a reading never came in");
            thread::sleep(Duration::from_millis(2));
        }
        app.tick();
    }

    fn names(app: &App) -> Vec<String> {
        app.active_panel().entries.iter().map(|e| e.name.clone()).collect()
    }

    fn on(app: &App) -> String {
        app.active_panel().current().map(|e| e.name.clone()).unwrap_or_default()
    }

    fn here(app: &App) -> PathBuf {
        app.active_panel().cwd.clone()
    }

    fn select(app: &mut App, name: &str) {
        assert!(app.active_panel_mut_for_tests().select_name(name), "{name}");
    }

    /// The screen goes on while a folder is read, and the panel shows what it showed; then
    /// place, list, cursor and memory change together.
    #[test]
    fn a_slow_folder_leaves_the_panel_as_it_is_until_it_is_in() {
        let tmp = TempDir::new("reading-slow");
        let slow = tmp.dir("slow");
        tmp.file("slow/inside.txt", "x");
        tmp.file("stays.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        select(&mut app, "slow");
        app.active_panel_mut_for_tests().toggle_mark();
        let held = hold(&slow);
        press(&mut app, KeyCode::Enter);
        assert!(app.active_panel().is_going());
        assert_eq!(here(&app), tmp.path());
        assert_eq!(names(&app), ["..", "slow", "stays.txt"]);
        assert_eq!(on(&app), "slow");
        assert!(app.active_panel().marked_summary().is_some(), "marks stay until the panel moves");
        drop(held);
        settle(&mut app);
        assert_eq!(here(&app), slow);
        assert_eq!(names(&app), ["..", "inside.txt"]);
        assert!(app.active_panel().marked_summary().is_none());
        assert_eq!(app.history.dirs.first(), Some(&slow), "it is in the folder history");

        press_alt(&mut app, KeyCode::Left);
        settle(&mut app);
        assert_eq!(here(&app), tmp.path(), "back where it was");
        assert_eq!(on(&app), "slow", "on the folder it left");
        press_alt(&mut app, KeyCode::Right);
        settle(&mut app);
        assert_eq!(here(&app), slow, "and forward again");
    }

    #[test]
    fn esc_stops_going_to_a_slow_folder() {
        let tmp = TempDir::new("reading-esc");
        let slow = tmp.dir("slow");
        tmp.file("slow/inside.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        select(&mut app, "slow");
        let held = hold(&slow);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Esc);
        assert!(!app.active_panel().is_reading());
        drop(held);
        settle(&mut app);
        assert_eq!(here(&app), tmp.path(), "what came late has nobody to go to");
        assert_eq!(on(&app), "slow", "nor do the keys typed for it");
        assert!(app.history.dirs.first() != Some(&slow));

        press(&mut app, KeyCode::Enter);
        settle(&mut app);
        assert_eq!(here(&app), slow, "and it can be asked for again");
    }

    /// A stuck reading is left behind: the panel goes where it was last sent.
    #[test]
    fn the_last_place_asked_for_wins() {
        let tmp = TempDir::new("reading-last");
        let (first, second) = (tmp.dir("first"), tmp.dir("second"));
        let (mut app, _other) = app_in(tmp.path());
        let (held_first, held_second) = (hold(&first), hold(&second));
        app.panels[0].go_to_first(std::slice::from_ref(&first), false);
        app.panels[0].go_to_first(std::slice::from_ref(&first), false);
        app.panels[0].go_to_first(std::slice::from_ref(&second), false);
        drop(held_first);
        thread::sleep(Duration::from_millis(30));
        app.tick();
        assert_eq!(here(&app), tmp.path());
        drop(held_second);
        settle(&mut app);
        assert_eq!(here(&app), second);
        press_alt(&mut app, KeyCode::Left);
        settle(&mut app);
        assert_eq!(here(&app), tmp.path(), "the first one never happened");
    }

    /// As keys wait in the terminal behind a quick folder.
    #[test]
    fn keys_typed_at_once_are_for_the_folder_that_is_coming() {
        let tmp = TempDir::new("reading-ahead");
        let slow = tmp.dir("slow");
        tmp.file("slow/a.txt", "x");
        tmp.file("slow/b.txt", "x");
        tmp.file("c.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        select(&mut app, "slow");
        let held = hold(&slow);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(on(&app), "slow", "not for what is on show");
        drop(held);
        settle(&mut app);
        assert_eq!(here(&app), slow);
        assert_eq!(on(&app), "b.txt");
    }

    /// One of them may go to yet another slow folder: the rest wait for that one.
    #[test]
    fn keys_typed_ahead_wait_through_a_second_slow_folder() {
        let tmp = TempDir::new("reading-twice");
        let (one, two) = (tmp.dir("one"), tmp.dir("one/two"));
        tmp.file("one/two/a.txt", "x");
        tmp.file("one/two/b.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        select(&mut app, "one");
        let (held_one, held_two) = (hold(&one), hold(&two));
        press(&mut app, KeyCode::Enter);
        for code in [KeyCode::Down, KeyCode::Enter, KeyCode::Down, KeyCode::Down] {
            press(&mut app, code);
        }
        drop(held_one);
        let deadline = Instant::now() + Duration::from_secs(10);
        while here(&app) != one {
            assert!(Instant::now() < deadline);
            app.tick();
        }
        assert_eq!(names(&app), ["..", "two"]);
        drop(held_two);
        settle(&mut app);
        assert_eq!(here(&app), two);
        assert_eq!(on(&app), "b.txt");
    }

    /// Once the wait is on the panel, nothing piles up behind it to be let loose when it ends.
    #[test]
    fn with_the_wait_on_the_panel_keys_do_nothing_but_what_it_says() {
        let tmp = TempDir::new("reading-shown");
        let slow = tmp.dir("slow");
        tmp.file("slow/a.txt", "x");
        tmp.file("slow/b.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        select(&mut app, "slow");
        let held = hold(&slow);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.active_panel().waiting_for(), None, "a moment isn't worth a word");
        app.panels[0].age_the_wait();
        assert_eq!(app.active_panel().waiting_for().as_deref(), Some("Reading slow… Esc stops"));
        assert!(app.tick(), "the wait is drawn");
        assert!(!app.tick(), "once");
        for code in [KeyCode::Down, KeyCode::F(8), KeyCode::F(7), KeyCode::Char('x'), KeyCode::Enter] {
            press(&mut app, code);
        }
        assert!(app.dialog.is_none());
        assert!(app.cmdline.is_empty());

        // The other panel works, with nothing waiting there.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, 1);
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.cmdline.text(), "x");
        press(&mut app, KeyCode::Backspace);

        drop(held);
        settle(&mut app);
        assert_eq!(app.panels[0].cwd, slow);
        assert_eq!(app.panels[0].current().map(|e| e.name.as_str()), Some(".."), "no key was kept for it");
        assert_eq!(app.active, 1);
    }

    #[test]
    fn a_folder_that_fails_late_says_so_and_keys_typed_for_it_go() {
        let tmp = TempDir::new("reading-fail");
        tmp.file("a.txt", "x");
        tmp.file("b.txt", "x");
        let missing = tmp.path().join("missing");
        let (mut app, _other) = app_in(tmp.path());
        select(&mut app, "a.txt");
        let held = hold(&missing);
        app.panels[0].go_to_first(std::slice::from_ref(&missing), false);
        press(&mut app, KeyCode::Down);
        drop(held);
        settle(&mut app);
        assert_eq!(here(&app), tmp.path());
        assert_eq!(on(&app), "a.txt");
        assert!(matches!(&app.dialog, Some(Dialog::Message { error: true, .. })));
    }

    /// The other panel's bad news doesn't take away what is being typed.
    #[test]
    fn a_failure_waits_for_the_dialog_that_is_open() {
        let tmp = TempDir::new("reading-dialog");
        let missing = tmp.path().join("missing");
        let (mut app, _other) = app_in(tmp.path());
        let held = hold(&missing);
        app.panels[1].go_to_first(std::slice::from_ref(&missing), false);
        press(&mut app, KeyCode::F(7));
        drop(held);
        settle(&mut app);
        assert!(matches!(&app.dialog, Some(Dialog::Input { action: InputAction::Mkdir, .. })));
        press(&mut app, KeyCode::Esc);
        app.tick();
        assert!(matches!(&app.dialog, Some(Dialog::Message { error: true, .. })));
    }

    /// A reading that started before a change doesn't show it: another one follows.
    #[test]
    fn a_change_during_a_reading_is_read_afterwards() {
        let tmp = TempDir::new("reading-again");
        tmp.file("old.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        let held = hold(tmp.path());
        app.panels[0].reload();
        thread::sleep(Duration::from_millis(50));
        tmp.file("new.txt", "x");
        app.panels[0].reload();
        assert!(app.active_panel().is_reading());
        drop(held);
        settle(&mut app);
        assert_eq!(names(&app), ["..", "new.txt", "old.txt"]);
    }

    /// The cursor goes to a name that isn't listed yet, however many readings that takes, and
    /// keys wait for it to be there.
    #[test]
    fn the_cursor_waits_for_the_name_it_is_sent_to() {
        let tmp = TempDir::new("reading-cursor");
        tmp.file("old.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        let held = hold(tmp.path());
        app.panels[0].reload();
        thread::sleep(Duration::from_millis(50));
        tmp.file("new.txt", "x");
        app.panels[0].reload();
        app.panels[0].select_name("new.txt");
        assert!(app.active_panel().is_going());
        press(&mut app, KeyCode::Down);
        drop(held);
        settle(&mut app);
        assert_eq!(names(&app), ["..", "new.txt", "old.txt"]);
        assert_eq!(on(&app), "old.txt", "the key was for after the name");
    }

    /// A reload behind the user's back leaves the keys alone, and Esc can stop it.
    #[test]
    fn a_reload_that_takes_its_time_is_no_hindrance() {
        let tmp = TempDir::new("reading-reload");
        tmp.file("a.txt", "x");
        tmp.file("b.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        let held = hold(tmp.path());
        app.panels[0].reload();
        assert!(!app.active_panel().is_going());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(on(&app), "b.txt");
        tmp.file("0.txt", "x");
        press(&mut app, KeyCode::Esc);
        assert!(!app.active_panel().is_reading());
        drop(held);
        settle(&mut app);
        assert_eq!(names(&app), ["..", "a.txt", "b.txt"], "stopped");

        app.panels[0].reload();
        settle(&mut app);
        assert_eq!(names(&app), ["..", "0.txt", "a.txt", "b.txt"]);
        assert_eq!(on(&app), "b.txt", "the cursor stays on its name");
    }

    /// A hidden tab takes in what it was reading, for when it is shown.
    #[test]
    fn a_tab_put_away_goes_on_reading() {
        let tmp = TempDir::new("reading-tab");
        let slow = tmp.dir("slow");
        tmp.file("slow/inside.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        let held = hold(&slow);
        app.panels[0].go_to_first(std::slice::from_ref(&slow), false);
        // Ctrl+T waits with the other keys, so the tab is put away by a click.
        app.tabs[0].push(Panel::new(tmp.path().to_path_buf(), true));
        app.switch_tab(0, 1);
        assert_eq!(app.tab_index[0], 1);
        drop(held);
        settle(&mut app);
        assert_eq!(app.tabs[0][0].cwd, slow);
        assert_eq!(app.tabs[0][0].entries.len(), 2);
    }

    /// There is nothing to show before the first folder is in.
    #[test]
    fn a_panel_starting_in_a_slow_folder_shows_it_when_it_comes() {
        let tmp = TempDir::new("reading-start");
        let slow = tmp.dir("slow");
        tmp.file("slow/inside.txt", "x");
        wait_for_nothing();
        let held = hold(&slow);
        let mut panel = Panel::new(slow.clone(), true);
        assert_eq!(panel.cwd, slow);
        assert!(panel.entries.is_empty() && panel.current().is_none());
        assert!(panel.is_going());
        drop(held);
        let deadline = Instant::now() + Duration::from_secs(10);
        while panel.is_reading() {
            assert!(Instant::now() < deadline);
            panel.poll();
        }
        assert_eq!(panel.entries.len(), 2);
    }

    /// Esc on a panel with nothing to stay at: home.
    #[test]
    fn esc_on_a_panel_that_has_nothing_to_show_goes_home() {
        let tmp = TempDir::new("reading-start-esc");
        let slow = tmp.dir("slow");
        wait_for_nothing();
        let _held = hold(&slow);
        let mut panel = Panel::new(slow.clone(), true);
        panel.stop_reading();
        let deadline = Instant::now() + Duration::from_secs(10);
        while panel.is_reading() {
            assert!(Instant::now() < deadline);
            panel.poll();
        }
        assert_eq!(Some(panel.cwd.clone()), util::home_dir());
        assert!(!panel.entries.is_empty());
    }

    /// From a folder that can't be read, whatever is nearest; nothing asks anything.
    #[test]
    fn a_panel_starting_in_a_missing_folder_finds_the_nearest_one() {
        let tmp = TempDir::new("reading-missing");
        let panel = Panel::new(tmp.path().join("gone/deep"), true);
        assert_eq!(panel.cwd, tmp.path());
    }

    /// Opening a zip reads the file, which on a share takes time like any folder.
    #[test]
    fn a_zip_on_a_slow_share_opens_like_a_folder() {
        let tmp = TempDir::new("reading-zip");
        let zip = tmp.path().join("photos.zip");
        crate::archive::tests::sample_zip(&zip);
        tmp.file("other.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        select(&mut app, "photos.zip");
        let held = hold(&zip);
        press(&mut app, KeyCode::Enter);
        assert!(app.active_panel().archive.is_none(), "not until it is in");
        assert!(app.active_panel().is_going());
        drop(held);
        settle(&mut app);
        assert!(app.active_panel().archive.is_some());
        assert!(names(&app).contains(&"docs".to_string()));
        assert_eq!(on(&app), "..");
        assert!(app.notice.as_deref().is_some_and(|note| note.contains("Left out")), "{:?}", app.notice);

        // Inside it everything is in memory; out of it there is a folder to read.
        select(&mut app, "docs");
        press(&mut app, KeyCode::Enter);
        assert!(!app.active_panel().is_going());
        assert_eq!(on(&app), "..");
        press(&mut app, KeyCode::Enter);
        assert_eq!(on(&app), "docs");
        select(&mut app, "..");
        let held = hold(tmp.path());
        press(&mut app, KeyCode::Enter);
        assert!(app.active_panel().archive.is_some(), "still in it");
        drop(held);
        settle(&mut app);
        assert!(app.active_panel().archive.is_none());
        assert_eq!(on(&app), "photos.zip");
    }

    /// A zip that can't be read is said, and the panel stays.
    #[test]
    fn a_zip_that_fails_late_says_so() {
        let tmp = TempDir::new("reading-badzip");
        let zip = tmp.file("bad.zip", "not a zip");
        let (mut app, _other) = app_in(tmp.path());
        select(&mut app, "bad.zip");
        let held = hold(&zip);
        press(&mut app, KeyCode::Enter);
        drop(held);
        settle(&mut app);
        assert!(app.active_panel().archive.is_none());
        assert!(matches!(&app.dialog, Some(Dialog::Message { error: true, .. })));
    }

    /// Picking a drive: the folder this panel last had there, or the root if that is gone.
    #[test]
    fn a_folder_that_cant_be_read_gives_way_to_the_next() {
        let tmp = TempDir::new("reading-first");
        let (mut app, _other) = app_in(tmp.path());
        let gone = tmp.path().join("gone");
        let there = tmp.dir("there");
        app.panels[0].go_to_first(&[gone, there.clone()], false);
        settle(&mut app);
        assert_eq!(here(&app), there);
        assert!(app.dialog.is_none());
    }

    #[test]
    fn a_shell_folder_that_is_gone_is_not_worth_a_word() {
        let tmp = TempDir::new("reading-quiet");
        let (mut app, _other) = app_in(tmp.path());
        app.panels[0].go_to_first(&[tmp.path().join("gone")], true);
        settle(&mut app);
        assert_eq!(here(&app), tmp.path());
        assert_eq!(app.panels[0].came, None);
        assert!(app.dialog.is_none());
    }

    /// Alt+Left passes over folders that are gone, and says when none is left.
    #[test]
    fn going_back_passes_over_what_is_gone() {
        let tmp = TempDir::new("reading-back");
        let (one, two) = (tmp.dir("one"), tmp.dir("two"));
        let (mut app, _other) = app_in(tmp.path());
        app.panels[0].go_to_first(std::slice::from_ref(&one), false);
        settle(&mut app);
        app.panels[0].go_to_first(std::slice::from_ref(&two), false);
        settle(&mut app);
        fs::remove_dir(&one).unwrap();
        press_alt(&mut app, KeyCode::Left);
        settle(&mut app);
        assert_eq!(here(&app), tmp.path());
        press_alt(&mut app, KeyCode::Left);
        settle(&mut app);
        assert_eq!(here(&app), tmp.path());
        assert_eq!(app.notice.as_deref(), Some("No folder to go back to"));
        assert!(app.panels[0].came.is_none());
        press_alt(&mut app, KeyCode::Right);
        settle(&mut app);
        assert_eq!(here(&app), two, "forward is still there");
    }

    /// The folder shown has gone: the nearest one above it, with the way back kept.
    #[test]
    fn a_folder_that_vanishes_gives_way_to_the_one_above() {
        let tmp = TempDir::new("reading-gone");
        let deep = tmp.dir("a/b");
        tmp.file("a/b/x.txt", "x");
        let (mut app, _other) = app_in(&deep);
        select(&mut app, "x.txt");
        app.active_panel_mut_for_tests().toggle_mark();
        fs::remove_dir_all(tmp.path().join("a")).unwrap();
        app.panels[0].reload();
        settle(&mut app);
        assert_eq!(here(&app), tmp.path());
        assert!(app.dialog.is_none());
        assert!(app.active_panel().marked_summary().is_none());
    }

    #[test]
    fn what_comes_in_is_put_in_the_order_chosen_meanwhile() {
        let tmp = TempDir::new("reading-order");
        tmp.file("a.txt", "1");
        tmp.file("b.txt", "22");
        let (mut app, _other) = app_in(tmp.path());
        let held = hold(tmp.path());
        app.panels[0].reload();
        app.panels[0].choose_sort(SortBy::Name);
        assert!(app.panels[0].reverse);
        drop(held);
        settle(&mut app);
        assert_eq!(names(&app), ["..", "b.txt", "a.txt"]);
    }

    /// Over SSH the screen doesn't wait either, and the panel moves when the listing is in.
    #[test]
    fn a_slow_server_doesnt_hold_the_screen() {
        let there = TempDir::new("reading-ssh-there");
        let home = TempDir::new("reading-ssh-here");
        there.dir("slow");
        there.file("slow/inside.txt", "x");
        let Some(connection) = crate::ssh::tests::local_connection(there.path()) else { return };
        let top = PathBuf::from(connection.target.place(&connection.home));
        let mut app = App::new(home.path().to_path_buf(), home.path().to_path_buf(), config());
        app.panels[0].go_to_first(&[top], false);
        assert!(app.panels[0].remote.is_some(), "{:?}", app.panels[0].came);
        wait_for_nothing();
        let address = connection.target.place(&ssh::join(&connection.home, "slow"));
        select(&mut app, "slow");
        let held = hold(Path::new(&address));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.panels[0].remote.as_ref().map(|view| view.dir.as_str()), Some(connection.home.as_str()));
        assert!(app.active_panel().is_going());
        drop(held);
        settle(&mut app);
        assert_eq!(app.panels[0].remote.as_ref().map(|view| view.dir.ends_with("slow")), Some(true));
        assert_eq!(names(&app), ["..", "inside.txt"]);
        crate::ssh::tests::forget(&connection);
    }

    /// Keys typed for a folder that quietly can't be read aren't for the folder left.
    #[test]
    fn keys_typed_for_a_folder_that_quietly_fails_go() {
        let tmp = TempDir::new("reading-quiet-keys");
        tmp.file("a.txt", "x");
        tmp.file("b.txt", "x");
        let missing = tmp.path().join("missing");
        let (mut app, _other) = app_in(tmp.path());
        select(&mut app, "a.txt");
        let held = hold(&missing);
        app.panels[0].go_to_first(std::slice::from_ref(&missing), true);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::F(8));
        drop(held);
        settle(&mut app);
        assert_eq!(here(&app), tmp.path());
        assert_eq!(on(&app), "a.txt");
        assert!(app.dialog.is_none(), "F8 would have asked about a file in the folder left");
    }

    /// A job ended on the other panel, and Esc stops the wait: what the job did still shows.
    #[test]
    fn esc_keeps_the_look_that_was_owed() {
        let tmp = TempDir::new("reading-esc-owed");
        let slow = tmp.dir("slow");
        tmp.file("old.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        let held = hold(&slow);
        app.panels[0].go_to_first(std::slice::from_ref(&slow), false);
        tmp.file("new.txt", "x");
        app.panels[0].reload();
        press(&mut app, KeyCode::Esc);
        drop(held);
        settle(&mut app);
        assert_eq!(here(&app), tmp.path());
        assert!(names(&app).contains(&"new.txt".to_string()), "{:?}", names(&app));
    }

    /// A look under way, then a folder that can't be read: the look isn't lost with it.
    #[test]
    fn a_failed_go_keeps_the_look_under_way() {
        let tmp = TempDir::new("reading-go-owed");
        let missing = tmp.path().join("missing");
        tmp.file("old.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        let held = hold(tmp.path());
        app.panels[0].reload();
        thread::sleep(Duration::from_millis(50));
        tmp.file("new.txt", "x");
        assert!(app.panels[0].is_reading());
        app.panels[0].go_to_first(std::slice::from_ref(&missing), false);
        drop(held);
        settle(&mut app);
        assert!(matches!(&app.dialog, Some(Dialog::Message { error: true, .. })));
        assert!(names(&app).contains(&"new.txt".to_string()), "{:?}", names(&app));
    }

    /// The connection goes while the folder here is slow: until that is in, the panel stays as
    /// it was, its place and its list together.
    #[test]
    fn a_lost_connection_keeps_place_and_list_together() {
        let there = TempDir::new("reading-lost-there");
        let home = TempDir::new("reading-lost-here");
        there.file("over-there.txt", "x");
        let Some(connection) = crate::ssh::tests::local_connection(there.path()) else { return };
        let top = PathBuf::from(connection.target.place(&connection.home));
        let mut app = App::new(home.path().to_path_buf(), home.path().to_path_buf(), config());
        app.panels[0].go_to_first(&[top], false);
        assert!(app.panels[0].remote.is_some());
        wait_for_nothing();
        crate::ssh::tests::kill(&connection);
        let deadline = Instant::now() + Duration::from_secs(10);
        while connection.sftp.lost().is_none() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        let held = hold(home.path());
        app.panels[0].reload();
        // The list over there has failed, and the folder here is on its way.
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.panels[0].lost.is_none() {
            assert!(Instant::now() < deadline);
            app.panels[0].poll();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(app.panels[0].remote.is_some());
        assert!(names(&app).contains(&"over-there.txt".to_string()));
        drop(held);
        settle(&mut app);
        crate::ssh::tests::forget(&connection);
        assert!(app.panels[0].remote.is_none());
        assert_eq!(app.active_panel().location(), home.path());
        assert!(!names(&app).contains(&"over-there.txt".to_string()), "{:?}", names(&app));
    }

    /// The other panel's bad news waits for the command being typed on this one.
    #[test]
    fn a_late_failure_leaves_the_command_line_alone() {
        let tmp = TempDir::new("reading-cmdline");
        let missing = tmp.path().join("missing");
        let (mut app, _other) = app_in(tmp.path());
        let held = hold(&missing);
        app.panels[1].go_to_first(std::slice::from_ref(&missing), false);
        let typing = |app: &mut App, text: &str| {
            for c in text.chars() {
                press(app, KeyCode::Char(c));
            }
        };
        typing(&mut app, "tool push ");
        drop(held);
        settle(&mut app);
        typing(&mut app, "--dry-run origin");
        assert_eq!(app.cmdline.text(), "tool push --dry-run origin");
        assert!(app.dialog.is_none());
        press(&mut app, KeyCode::Esc);
        app.tick();
        assert!(matches!(&app.dialog, Some(Dialog::Message { error: true, .. })), "and comes when the line is empty");
    }

    /// A listing keeps its folder open, which on Windows keeps what is above it from moving:
    /// a job moving that lets go of it.
    #[test]
    fn a_job_moving_a_folder_lets_go_of_what_is_read_below_it() {
        let tmp = TempDir::new("reading-job");
        let (top, deep) = (tmp.dir("top"), tmp.dir("top/deep"));
        let elsewhere = tmp.dir("elsewhere");
        tmp.file("top/deep/x.txt", "x");
        let (mut app, _other) = app_in(tmp.path());
        let held = hold(&deep);
        app.panels[1].go_to_first(std::slice::from_ref(&deep), false);
        assert!(app.panels[1].is_reading());
        app.panels[1].let_go_of(&[elsewhere.as_path()]);
        assert!(app.panels[1].is_reading(), "this job has nothing to do with it");
        app.panels[1].let_go_of(&[top.as_path()]);
        assert!(!app.panels[1].is_reading());
        drop(held);
    }

    /// `git status` ending says nothing about how quick the files are: what follows it is read
    /// where the status was, not on the screen's thread.
    #[cfg(unix)]
    #[test]
    fn a_status_arriving_doesnt_hold_the_screen() {
        let tmp = TempDir::new("reading-git");
        let made = std::process::Command::new("git").arg("init").arg("-q").arg(tmp.path()).status().unwrap();
        assert!(made.success());
        let mut settings = config();
        settings.settings.git_status = true;
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), settings);
        wait_for_nothing();
        assert!(app.panels[0].repo.is_some());
        // A file that doesn't answer for a while, as on a share gone away.
        let merge = tmp.path().join(".git/rebase-merge");
        fs::create_dir(&merge).unwrap();
        let hanging = merge.join("msgnum");
        let name = std::ffi::CString::new(hanging.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o644) }, 0);
        let answers = hanging.clone();
        thread::spawn(move || {
            for _ in 0..3 {
                thread::sleep(Duration::from_millis(900));
                let _ = fs::write(&answers, b"1");
            }
        });
        let begun = Instant::now();
        let mut longest = Duration::ZERO;
        while begun.elapsed() < Duration::from_millis(1500) {
            let tick = Instant::now();
            app.tick();
            longest = longest.max(tick.elapsed());
            thread::sleep(Duration::from_millis(10));
        }
        assert!(longest < Duration::from_millis(500), "one tick held the screen for {longest:?}");
    }

    /// A folder that can't be read isn't gone: Alt+Left says why it stayed.
    #[cfg(unix)]
    #[test]
    fn alt_left_says_why_a_folder_cant_be_read() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("reading-back-denied");
        let (one, two) = (tmp.dir("one"), tmp.dir("two"));
        let (mut app, _other) = app_in(tmp.path());
        app.panels[0].go_to_first(std::slice::from_ref(&one), false);
        settle(&mut app);
        app.panels[0].go_to_first(std::slice::from_ref(&two), false);
        settle(&mut app);
        fs::set_permissions(&one, fs::Permissions::from_mode(0o000)).unwrap();
        press_alt(&mut app, KeyCode::Left);
        settle(&mut app);
        let (place, said) = (here(&app), app.dialog.is_some());
        fs::set_permissions(&one, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!((place, said), (two.clone(), true));
    }

    #[test]
    fn came_says_how_going_somewhere_ended() {
        let tmp = TempDir::new("reading-came");
        let mut panel = Panel::new(tmp.path().to_path_buf(), true);
        panel.go_to_first(&[tmp.path().join("missing")], false);
        assert!(matches!(panel.came.take(), Some(Came::Failed(why)) if why.starts_with("Cannot open")));
        panel.go_back();
        assert_eq!(panel.came.take(), Some(Came::NoFurther { back: true }));
    }
}
