//! Application state and keyboard handling.

use std::collections::VecDeque;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Instant, SystemTime};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::admin;
use crate::archive::{self, Archive};
use crate::clipboard::{self, Clipboard, Effect};
use crate::compare;
use crate::dialog::{self, ButtonOutcome, ConfirmAction, Dialog, InputAction, LineEdit, Recall};
use crate::places::{self, Place};
use crate::finder::{Field as FindField, Finder};
use crate::git::{self, Branch, Tracker, Worktree};
use crate::history::History;
use crate::hotlist::Hotlist;
use crate::lfs;
use crate::lnk;
use crate::mouse::{HitAreas, LastClick};
use crate::ops::remote;
use crate::ops::{self, Answer, Job, Msg, Question, Running};
use crate::options::{self, OptionsAutomatic};
use crate::keybar::Held;
use crate::panel::{Came, DirSize, Entry, Panel, RemoteView, SortBy};
use crate::preview::{Key as PreviewKey, Previewer};
use crate::platform::{self, OpenError, Shell};
use crate::scheme::{self, Choice, Colours, Scheme};
use crate::settings::Settings;
use crate::sftp::Attrs;
use crate::sizes::Sizer;
use crate::ssh::{self, Connection, Failure, Login, Target};
use crate::subshell::Subshell;
use crate::survey::{Survey, Tally};
use crate::util;
use crate::watch::Watch;

/// The lists of earlier texts of Find's two fields (see `history::Inputs`).
const FIND_TEXT: &str = "find-text";
const FIND_FILES: &str = "find-files";

/// A viewer that comes back sooner than this went on by itself, and still reads its file.
const VIEWER_STAYS: std::time::Duration = std::time::Duration::from_secs(if cfg!(test) { 0 } else { 3 });

/// What happens to a file taken out of an archive, or fetched from another computer, into
/// cm's temp folder once it is there.
enum AfterJob {
    /// Enter: opened in its app; a zip in the zip opens in the panel, called `shown`.
    Open { shown: PathBuf },
    /// F3 (and F4: files in archives are read-only).
    View,
    /// F4 on another computer: edited here, and put back if it changed. `there` is how the
    /// file looked over there.
    Edit { connection: Arc<Connection>, path: String, there: Attrs },
}

/// A file of another computer in the editor.
pub struct EditedCopy {
    local: PathBuf,
    connection: Arc<Connection>,
    path: String,
    /// The copy as it came, to see if the editor changed it.
    size: u64,
    modified: Option<SystemTime>,
    /// The file over there as it was fetched, to see if it changed meanwhile.
    there: Attrs,
    since: Instant,
    /// A copy kept where it outlives cm, while it isn't sure yours goes back.
    rescued: Option<PathBuf>,
}

/// What a connection being made is for.
enum AfterConnect {
    /// Panel `side` goes to this address.
    Go { side: usize, place: PathBuf },
    /// A copy or move typed with an address on this computer as its destination.
    Transfer { sources: Vec<PathBuf>, text: String, moving: bool, keep_marks: bool },
}

/// A connection being made behind the panels.
pub struct Connecting {
    pub target: Target,
    answer: Receiver<Result<Arc<Connection>, Failure>>,
    then: AfterConnect,
}

/// git switching a worktree to `branch`; keys wait for it.
pub struct Switching {
    pub branch: String,
    answer: Receiver<Result<String, String>>,
}

/// Something to run outside the TUI; the main loop gives it the terminal.
#[derive(Debug, PartialEq)]
pub enum External {
    /// A file in the editor, at a line if given (from Find).
    Edit(PathBuf, Option<u64>),
    View(PathBuf, Option<u64>),
    /// A big file, shown in the pager (`less`), which reads only what is on screen.
    Page(PathBuf, Option<u64>),
    Shell,
    Command(String),
    /// `ssh` wants to ask something, on the terminal.
    SshLogin(Target),
}

/// The default big-file limit: files bigger than this go to the pager instead of the viewer,
/// and the editor asks first, as an editor reads the whole file before showing anything.
#[cfg(test)]
pub const BIG_FILE: u64 = 10 * 1024 * 1024;

pub struct Config {
    pub editor: Vec<String>,
    pub viewer: Vec<String>,
    /// For big files; None when there is no pager.
    pub pager: Option<Vec<String>>,
    pub shell: Shell,
    /// Bytes; see `Settings::big_file_mb`.
    pub big_file: u64,
    pub hotlist_file: Option<PathBuf>,
    /// The options editor saves here; None in tests.
    pub settings_file: Option<PathBuf>,
    /// Folders visited and commands typed, kept between sessions; None in tests.
    pub history_file: Option<PathBuf>,
    /// The user's colour schemes (`schemes` beside the settings); None in tests.
    pub schemes_dir: Option<PathBuf>,
    /// Places in git repositories, for Ctrl+E (`places.ini` beside the settings).
    pub places_file: Option<PathBuf>,
    /// What the terminal does with colours.
    pub colours: Colours,
    pub settings: Settings,
}

impl Config {
    /// The settings file, the hotlist file, and from them the commands.
    pub fn load() -> Self {
        let settings_file = Settings::default_file();
        let mut config = match Settings::load(settings_file.as_deref()) {
            Ok(settings) => Config::new(settings, Hotlist::default_file(), settings_file),
            // There but unreadable: defaults, and the file is left alone (never saved over).
            Err(_) => Config::new(Settings::default(), Hotlist::default_file(), None),
        };
        config.history_file = History::default_file();
        config.schemes_dir = scheme::default_dir();
        config.places_file = places::File::default_path();
        config.colours = Colours::detect();
        config
    }

    /// Commands come from the settings, or when those are empty from the environment
    /// (CM_EDITOR…), or from what is installed.
    pub fn new(settings: Settings, hotlist_file: Option<PathBuf>, settings_file: Option<PathBuf>) -> Self {
        let auto = Automatic::detect();
        let chosen = |value: &str| (!value.trim().is_empty()).then(|| parse_command(value));
        let editor = chosen(&settings.editor).unwrap_or(auto.editor);
        // CM_VIEWER counts when the option is empty; otherwise the editor, read-only.
        let viewer = chosen(&settings.viewer)
            .or_else(|| env_command(&["CM_VIEWER"]))
            .unwrap_or_else(|| viewer_for(&editor));
        let pager = chosen(&settings.pager).or(auto.pager);
        let shell = if settings.shell.trim().is_empty() { auto.shell } else { Shell::from_program(settings.shell.trim()) };
        Config {
            editor,
            viewer,
            pager,
            shell,
            big_file: settings.big_file_bytes(),
            hotlist_file,
            settings_file,
            history_file: None,
            schemes_dir: None,
            places_file: None,
            colours: Colours::default(),
            settings,
        }
    }
}

/// What cm uses when a setting is empty: the environment, or what is installed.
pub struct Automatic {
    pub editor: Vec<String>,
    pub viewer: Vec<String>,
    pub pager: Option<Vec<String>>,
    pub shell: Shell,
}

impl Automatic {
    pub fn detect() -> Self {
        let editor = env_command(&["CM_EDITOR", "VISUAL", "EDITOR"]).unwrap_or_else(|| vec![default_editor()]);
        let viewer = env_command(&["CM_VIEWER"]).unwrap_or_else(|| viewer_for(&editor));
        let pager = env_command(&["CM_PAGER", "PAGER"]).or_else(default_pager);
        Automatic { editor, viewer, pager, shell: Shell::detect() }
    }
}

/// The editor in read-only mode: `-R` for vim and nvim, `-v` for nano.
fn viewer_for(editor: &[String]) -> Vec<String> {
    let mut viewer = editor.to_vec();
    if is_vim(&editor[0]) {
        viewer.push("-R".into());
    } else if is_nano(&editor[0]) {
        viewer.push("-v".into());
    }
    viewer
}

/// `less`; on Windows also the one that comes with Git for Windows, which isn't on the PATH.
fn default_pager() -> Option<Vec<String>> {
    let found = if cfg!(windows) {
        util::find_in_path("less.exe").or_else(|| {
            ["ProgramFiles", "ProgramW6432"]
                .iter()
                .filter_map(std::env::var_os)
                .map(|dir| PathBuf::from(dir).join(r"Git\usr\bin\less.exe"))
                .find(|path| path.is_file())
        })
    } else {
        util::find_in_path("less")
    };
    found.map(|path| vec![path.to_string_lossy().into_owned()])
}

fn env_command(names: &[&str]) -> Option<Vec<String>> {
    names
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| parse_command(&value))
        .find(|parts| !parts.is_empty())
}

/// Splits an editor setting into program and arguments. An unquoted path with spaces
/// (`C:\Program Files\Neovim\bin\nvim.exe`) that names an existing file is kept whole.
fn parse_command(value: &str) -> Vec<String> {
    let value = value.trim();
    if !value.starts_with('"') && Path::new(value).is_file() {
        return vec![value.to_string()];
    }
    util::split_command(value)
}

fn is_vim(program: &str) -> bool {
    let stem = Path::new(program).file_stem().map(|s| s.to_string_lossy().to_lowercase());
    matches!(stem.as_deref(), Some("nvim" | "vim" | "vi" | "gvim"))
}

fn is_nano(program: &str) -> bool {
    Path::new(program).file_stem().is_some_and(|s| s.eq_ignore_ascii_case("nano"))
}

/// nvim, or where it isn't installed (a server, WSL) the first of vim, vi and nano that is.
/// With none of them, still nvim, so the error says what to install.
fn default_editor() -> String {
    let exe = if cfg!(windows) { ".exe" } else { "" };
    ["nvim", "vim", "vi", "nano"]
        .into_iter()
        .find(|name| util::find_in_path(&format!("{name}{exe}")).is_some())
        .unwrap_or("nvim")
        .to_string()
}

pub struct App {
    pub panels: [Panel; 2],
    pub active: usize,
    pub cmdline: LineEdit,
    pub dialog: Option<Dialog>,
    pub job: Option<Running>,
    pub quicksearch: Option<String>,
    pub config: Config,
    pub external: Option<External>,
    /// A built-in scheme F4 has just written out as a file, and the text written. If the file
    /// is still only that when the editor closes, it goes again: a look mustn't leave a copy
    /// that pins the scheme, as the built-in one keeps getting cm's updates.
    scheme_file_written: Option<(PathBuf, String)>,
    /// The folder of a file taken out of an archive for the viewer, removed when it closes.
    viewed_copy: Option<(PathBuf, Instant)>,
    /// A file of another computer in the editor.
    edited_copy: Option<EditedCopy>,
    /// The running job puts this edited copy back: what is left of it when it is over, and
    /// the place it was saved to for safety.
    putting_back: Option<(PathBuf, Option<PathBuf>)>,
    /// A connection being made; keys wait for it.
    pub connecting: Option<Connecting>,
    /// git switching a branch, under the panels; keys wait for it.
    pub switching: Option<Switching>,
    /// What to do once `ssh` has logged in at the terminal.
    login_then: Option<AfterConnect>,
    /// The panel the running job started from: its marks are the ones the job uses up.
    job_side: usize,
    /// When an overwrite question opened, and how long it takes no answer (see `on_key`).
    overwrite_asked: Option<Instant>,
    answer_delay: std::time::Duration,
    /// When a key last came too early for such a question.
    stray_key: Option<Instant>,
    /// Where the cursor goes when the running job ends.
    select_after: Option<PathBuf>,
    /// The running job works on the item under the cursor, whatever is marked: the marks stay.
    keep_marks: bool,
    pub quit: bool,
    pub hotlist: Hotlist,
    /// Screen areas from the last frame, for the mouse.
    pub hit: HitAreas,
    pub(crate) last_click: Option<LastClick>,
    /// When and where a click closed a dialog (picking a drive, say): the second click of a
    /// double-click there mustn't land on the panel that was under it.
    pub(crate) closing_click: Option<(Instant, ratatui::layout::Position)>,
    /// The "how to get back" line has been shown for the one-off Ctrl+O shell.
    pub shell_hint_shown: bool,
    /// The persistent shell, started by the first Ctrl+O (or command typed in cm).
    pub subshell: Option<Subshell>,
    /// The shell is on screen instead of the panels; keys go to it.
    pub shell_shown: bool,
    /// Rows and columns of the whole screen at the last draw; the shell's screen matches it.
    pub screen_size: (u16, u16),
    /// Mouse capture wanted (the options editor can change it; the main loop applies it).
    pub mouse: bool,
    /// False with `--no-mouse`, which holds for the whole run, whatever the options say.
    pub mouse_allowed: bool,
    /// Alt+F7, kept when closed so it comes back as it was.
    pub finder: Option<Finder>,
    pub find_shown: bool,
    /// Alt+P / Alt+N in Find, going through the earlier texts of that field.
    find_recall: Option<(FindField, Recall)>,
    sizer: Option<Sizer>,
    /// Free space of network/optical drives, arriving while the drive picker is open.
    drive_space: Option<Receiver<(PathBuf, Option<(u64, u64)>)>>,
    /// Folders visited and commands typed (Alt+H, Alt+P/N), saved now and then and on exit.
    pub history: History,
    history_changed: bool,
    history_saved: Instant,
    history_pos: Option<usize>,
    /// A short message on the command line ("3 items copied to the clipboard"), gone at the
    /// next key.
    pub notice: Option<String>,
    /// Ctrl+Q: the other panel shows what is under the cursor (see `preview`).
    pub quick_view: bool,
    /// Tab moved into the quick view: the arrows scroll it.
    pub preview_focus: bool,
    pub previewer: Option<Previewer>,
    /// Rows of the quick view at the last draw, for paging through it.
    pub preview_rows: usize,
    pub clipboard: Clipboard,
    /// Text for the terminal to put on its clipboard (OSC 52); the main loop sends it.
    pub terminal_clipboard: Option<String>,
    /// The running job pastes files that were cut: when it is done, the clipboard is emptied.
    pasting_cut: Option<Vec<PathBuf>>,
    /// What to do with a file the running job takes out of an archive, once it is out.
    after_job: Option<(AfterJob, PathBuf)>,
    /// Each panel's folder, watched for changes other programs make (see `watch`).
    watches: [Option<Watch>; 2],
    /// Each side's other tabs, in order; the shown one (`panels[side]`) sits at
    /// `tab_index[side]` among them.
    pub tabs: [Vec<Panel>; 2],
    pub tab_index: [usize; 2],
    /// `git status` of the repositories the panels are in (see `git`).
    pub git: Tracker,
    /// The colour scheme on screen, as the terminal shows it: the one the settings name, or
    /// while Alt+C's list is open the one under its cursor.
    pub scheme: Scheme,
    /// Keys typed for a folder that is on its way (see `on_key_on_the_way`).
    pub(crate) typed_ahead: VecDeque<KeyEvent>,
    /// What is held down, which the key bar shows what it does.
    pub held: Held,
    /// `user@host` for the command line's prompt (see `platform::user_at_host`), and whether
    /// cm runs as root, for whom it ends in `#`.
    pub user_at_host: Option<String>,
    pub as_root: bool,
}

impl App {
    pub fn new(left: PathBuf, right: PathBuf, config: Config) -> Self {
        let show_hidden = config.settings.show_hidden;
        crate::panel::NATURAL_SORT.store(config.settings.natural_sort, Ordering::Relaxed);
        let mut history = History::load(config.history_file.clone());
        let panels = [Panel::new(left, show_hidden), Panel::new(right, show_hidden)];
        history.visit(&panels[1].cwd);
        history.visit(&panels[0].cwd);
        let wanted = &config.settings.scheme;
        let (scheme, notice) = match scheme::find(wanted, config.schemes_dir.as_deref()) {
            Some(choice) => (choice.scheme, None),
            None => (scheme::classic(), Some(format!("There is no colour scheme \"{wanted}\"; classic for now (Alt+C picks one)"))),
        };
        let scheme = scheme.for_terminal(config.colours);
        App {
            user_at_host: crate::platform::user_at_host(),
            as_root: crate::platform::is_root(),
            mouse: config.settings.mouse,
            mouse_allowed: true,
            finder: None,
            find_shown: false,
            find_recall: None,
            panels,
            active: 0,
            cmdline: LineEdit::default(),
            dialog: None,
            job: None,
            quicksearch: None,
            hotlist: Hotlist::load(config.hotlist_file.clone()),
            git: Tracker::new(config.settings.git_status),
            config,
            external: None,
            scheme_file_written: None,
            viewed_copy: None,
            edited_copy: None,
            putting_back: None,
            connecting: None,
            switching: None,
            login_then: None,
            job_side: 0,
            overwrite_asked: None,
            // Longer than a key takes to start repeating when it is held (0.5 s by default).
            answer_delay: std::time::Duration::from_millis(if cfg!(test) { 0 } else { 550 }),
            stray_key: None,
            select_after: None,
            keep_marks: false,
            quit: false,
            hit: HitAreas::default(),
            last_click: None,
            closing_click: None,
            shell_hint_shown: false,
            subshell: None,
            shell_shown: false,
            screen_size: (24, 80),
            sizer: None,
            drive_space: None,
            history,
            history_changed: false,
            history_saved: Instant::now(),
            history_pos: None,
            notice,
            quick_view: false,
            preview_focus: false,
            previewer: None,
            preview_rows: 20,
            clipboard: Clipboard::default(),
            terminal_clipboard: None,
            pasting_cut: None,
            after_job: None,
            watches: [None, None],
            tabs: [Vec::new(), Vec::new()],
            tab_index: [0, 0],
            scheme,
            typed_ahead: VecDeque::new(),
            held: Held::Nothing,
        }
    }

    /// Ctrl+T: a new tab of the active side, on the same folder, right after the shown one.
    fn new_tab(&mut self) {
        let side = self.active;
        let tab = self.panels[side].new_tab();
        let shown = std::mem::replace(&mut self.panels[side], tab);
        self.tabs[side].insert(self.tab_index[side], shown);
        self.tab_index[side] += 1;
    }

    /// Ctrl+W: closes the shown tab; the one after it (or before, at the end) takes its place.
    fn close_tab(&mut self) {
        let side = self.active;
        if self.tabs[side].is_empty() {
            self.notice = Some("The last tab stays (Ctrl+T opens another)".into());
            return;
        }
        let at = self.tab_index[side].min(self.tabs[side].len() - 1);
        self.panels[side] = self.tabs[side].remove(at);
        self.tab_index[side] = at;
        self.panels[side].reload();
    }

    /// Alt+1…9 (or a click on it): tab `to` of `side` is shown.
    pub(crate) fn switch_tab(&mut self, side: usize, to: usize) {
        let (from, count) = (self.tab_index[side], self.tabs[side].len() + 1);
        if to >= count || to == from {
            return;
        }
        let shown = std::mem::replace(&mut self.panels[side], Panel::blank(false));
        let mut all = std::mem::take(&mut self.tabs[side]);
        all.insert(from, shown);
        self.panels[side] = all.remove(to);
        self.tabs[side] = all;
        self.tab_index[side] = to;
        // Things may have changed there meanwhile.
        self.panels[side].reload();
    }

    /// The names of side `side`'s tabs (its folder's name), with the shown one's index; empty
    /// with one tab.
    pub fn tab_names(&self, side: usize) -> Vec<String> {
        if self.tabs[side].is_empty() {
            return Vec::new();
        }
        let name = |panel: &Panel| {
            let location = panel.location();
            location.file_name().map_or_else(|| location.display().to_string(), |n| n.to_string_lossy().into_owned())
        };
        let mut names: Vec<String> = self.tabs[side].iter().map(name).collect();
        names.insert(self.tab_index[side], name(&self.panels[side]));
        names
    }

    /// Saves the history file (on exit, and from `tick` now and then).
    pub fn save_history(&mut self) {
        if self.history_changed {
            let _ = self.history.save();
            self.history_changed = false;
            self.history_saved = Instant::now();
        }
    }

    /// Something will need a look soon (a folder changed and is settling, the quick view is
    /// reading a file), so the main loop should wake up sooner than when idle.
    pub fn wake_soon(&self) -> bool {
        self.watches.iter().flatten().any(Watch::pending)
            || self.previewer.as_ref().is_some_and(Previewer::loading)
            || self.git.running()
            || self.panels.iter().any(Panel::is_reading)
    }

    /// Ctrl+Q: the quick view on or off.
    fn toggle_quick_view(&mut self) {
        self.quick_view = !self.quick_view;
        self.preview_focus = false;
        if self.quick_view {
            self.previewer.get_or_insert_with(Previewer::new);
        } else if let Some(previewer) = &mut self.previewer {
            previewer.show(None);
        }
    }

    /// What the quick view should show: the entry under the active panel's cursor (in an
    /// archive, a file read from the zip; its folders show nothing).
    fn preview_key(&self) -> Option<PreviewKey> {
        let panel = self.active_panel();
        let entry = panel.current().filter(|e| !e.is_parent)?;
        // A file on another computer stays there until it is asked for.
        if panel.remote.is_some() && panel.archive.is_none() {
            return None;
        }
        let archive = match &panel.archive {
            Some(view) => {
                let item = panel.archive_item(entry).filter(|item| !item.is_dir)?;
                Some((view.archive.file.clone(), item.index?))
            }
            None => None,
        };
        Some(PreviewKey { path: entry.path.clone(), size: entry.size, modified: entry.modified, archive })
    }

    /// Keys while the quick view has the focus: the arrows and pages scroll it, Tab and Esc go
    /// back to the panel. Returns false for keys it leaves to the panels.
    fn on_preview_key(&mut self, key: &KeyEvent) -> bool {
        let Some(previewer) = self.previewer.as_mut() else { return false };
        let page = self.preview_rows.saturating_sub(1).max(1) as isize;
        let rows = self.preview_rows;
        match key.code {
            KeyCode::Up => previewer.scroll(-1, rows),
            KeyCode::Down => previewer.scroll(1, rows),
            KeyCode::PageUp => previewer.scroll(-page, rows),
            KeyCode::PageDown => previewer.scroll(page, rows),
            KeyCode::Home => previewer.scroll(isize::MIN / 2, rows),
            KeyCode::End => previewer.scroll(isize::MAX / 2, rows),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => self.preview_focus = false,
            _ => return false,
        }
        true
    }

    /// Reloads a panel whose folder another program changed. Not while a job runs or the
    /// shell is shown: both reload the panels when they end.
    fn follow_changes(&mut self) -> bool {
        // No watches while a job runs (see `start_job`), nor while git switches a branch.
        if self.job.is_some() || self.switching.is_some() {
            return false;
        }
        let now = Instant::now();
        let mut changed = false;
        for side in 0..2 {
            // What another computer does isn't seen from here: Ctrl+R reads it again.
            if self.panels[side].remote.is_some() {
                self.watches[side] = None;
                continue;
            }
            let cwd = &self.panels[side].cwd;
            if self.watches[side].as_ref().is_none_or(|w| w.dir != *cwd) {
                self.watches[side] = Some(Watch::new(cwd));
            }
            let due = self.watches[side].as_mut().is_some_and(|w| w.due(now));
            if due && !self.shell_shown {
                self.panels[side].look_again();
                // Files changed in a repository: its status may have too.
                if let Some(repo) = &self.panels[side].repo {
                    self.git.nudge(&repo.root);
                }
                changed = true;
            }
        }
        changed
    }

    /// Starts `git status` where the shown panels need it, and takes what arrived. A status
    /// can follow a checkout, so the branch is read again with it.
    fn tick_git(&mut self) -> bool {
        let job_running = self.job.is_some() || self.switching.is_some();
        let shown: Vec<&git::Repo> = (0..2)
            // No watches while a job runs (see `start_job`).
            .filter(|&side| !job_running && !(self.quick_view && side != self.active))
            .filter_map(|side| self.panels[side].repo.as_ref())
            .collect();
        let quiet = self.shell_shown || job_running;
        let arrived = self.git.tick(&shown, Instant::now(), quiet);
        for fresh in &arrived {
            for panel in &mut self.panels {
                if let (Some(repo), Some(now)) = (panel.repo.as_mut().filter(|repo| repo.root == fresh.root), &fresh.repo) {
                    *repo = now.clone();
                }
            }
        }
        !arrived.is_empty()
    }

    /// Whether background work is running (the main loop then redraws more often).
    pub fn is_busy(&self) -> bool {
        self.working() || self.shell_shown
    }

    /// Work in the background that changes the screen by itself: a copy, move or delete,
    /// directory sizes, or drive space still to arrive.
    pub fn working(&self) -> bool {
        let drives_pending = matches!(&self.dialog, Some(Dialog::Drives { drives, .. }) if drives.iter().any(|d| d.pending));
        let finding = self.finder.as_ref().is_some_and(Finder::working);
        let surveying = matches!(&self.dialog, Some(Dialog::Delete { survey, .. }) if !survey.is_done());
        let listing = matches!(&self.dialog, Some(Dialog::Branches { coming: Some(_), .. }));
        self.job.is_some()
            || self.connecting.is_some()
            || self.switching.is_some()
            || listing
            || self.sizer.as_ref().is_some_and(Sizer::is_busy)
            || drives_pending
            || finding
            || surveying
    }

    pub fn active_panel(&self) -> &Panel {
        &self.panels[self.active]
    }

    fn active_panel_mut(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
    }

    #[cfg(test)]
    pub fn active_panel_mut_for_tests(&mut self) -> &mut Panel {
        self.active_panel_mut()
    }

    fn error(&mut self, message: impl Into<String>) {
        let lines = message.into().lines().map(String::from).collect();
        self.dialog = Some(Dialog::Message { title: "Error".into(), lines, error: true });
    }

    fn reload_panels(&mut self) {
        for panel in &mut self.panels {
            panel.reload();
        }
        // A command ran, or Ctrl+R: the repositories may have changed too.
        self.git.refresh();
    }

    /// The shell commands are typed into: the running one, or the one the options name.
    fn shell(&self) -> &Shell {
        self.subshell.as_ref().map_or(&self.config.shell, Subshell::shell)
    }

    pub fn on_key(&mut self, mut key: KeyEvent) {
        // With Caps Lock on, Windows reports Ctrl+O as Ctrl with a capital O (and no Shift):
        // every Ctrl+letter shortcut would be dead, and Ctrl+O would go into the shell.
        if let KeyCode::Char(c) = key.code
            && c.is_ascii_uppercase()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        {
            key.code = KeyCode::Char(c.to_ascii_lowercase());
        }
        // An overwrite question that has only just opened: this key was on its way before it
        // could be read (Enter pressed twice, or held down), and would answer Yes. A key that
        // repeats (held down) puts the wait back; a deliberate second press gets through.
        if self.overwrite_asked.is_some_and(|at| at.elapsed() < self.answer_delay) {
            if self.stray_key.is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(120)) {
                self.overwrite_asked = Some(Instant::now());
            }
            self.stray_key = Some(Instant::now());
            return;
        }
        self.overwrite_asked = None;
        self.notice = None;
        self.git.touch();
        if self.shell_shown {
            self.on_shell_key(key);
        } else if self.connecting.is_some() {
            // The attempt ends by itself, and nobody takes its answer.
            if key.code == KeyCode::Esc {
                self.connecting = None;
            }
        } else if self.switching.is_some() {
            // git is changing the files under the panels, and isn't stopped halfway.
        } else if self.dialog.is_some() {
            self.on_dialog_key(key);
        } else if self.find_shown {
            self.on_find_key(key);
        } else if let Some(job) = &mut self.job {
            match key.code {
                KeyCode::Esc => job.stop(),
                // Nothing else gets out of a job caught in a system call that doesn't return.
                KeyCode::F(10) if job.stuck() => self.quit = true,
                _ => {}
            }
        } else if self.quick_view && self.preview_focus && self.on_preview_key(&key) {
        } else if self.active_panel().is_going() {
            self.on_key_on_the_way(key);
        } else if !self.on_quicksearch_key(&key) {
            self.on_panel_key(key);
        }
    }

    /// A key while the active panel is on its way to another folder: it would act on a list
    /// that is about to go. For a moment the key is typed for what is coming, and waits for it,
    /// as keys wait in the terminal behind a folder that is quick. Once the wait is on the
    /// panel, a key that does nothing can be seen to do nothing, and nothing is left to be
    /// let loose when the folder comes, however many are pressed: Esc stops the wait, Tab
    /// goes to the other panel and F10 leaves.
    fn on_key_on_the_way(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.typed_ahead.clear();
                self.active_panel_mut().stop_reading();
            }
            _ if !self.active_panel().waits_visibly() => self.typed_ahead.push_back(key),
            KeyCode::Tab | KeyCode::F(10) => {
                self.typed_ahead.clear();
                self.on_panel_key(key);
            }
            _ => {}
        }
    }

    fn on_panel_key(&mut self, key: KeyEvent) {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let plain = !ctrl && !alt;
        let cmd_empty = self.cmdline.is_empty();
        match key.code {
            KeyCode::F(1) if alt => self.open_drives(0),
            KeyCode::F(2) if alt => self.open_drives(1),
            KeyCode::F(1) => self.dialog = Some(Dialog::Help { scroll: 0 }),
            KeyCode::F(2) if shift => self.compare_dirs(),
            KeyCode::F(2) => self.start_rename(),
            KeyCode::F(3) => self.view_current(),
            // Making a folder inside an archive: archives open read-only.
            KeyCode::F(7) if !alt && self.panels[self.active].archive.is_some() => {
                self.refuse_in_archive(self.active);
            }
            KeyCode::F(4) if shift => {
                if !self.refuse_in_archive(self.active) && !self.refuse_over_ssh("Shift+F4 makes files on this computer") {
                    self.start_input("New file", "Create and edit file:", "", InputAction::NewFile);
                }
            }
            KeyCode::F(4) => self.edit_current(),
            KeyCode::F(5) if alt => self.start_pack(),
            KeyCode::F(9) if alt => self.start_unpack(),
            KeyCode::F(5) if shift => self.start_in_place(false),
            KeyCode::F(6) if shift => self.start_in_place(true),
            KeyCode::F(5) => self.start_transfer(false),
            KeyCode::F(6) => self.start_transfer(true),
            // Alt+F7 as in Total Commander, Alt+? as in mc.
            KeyCode::F(7) if alt => self.open_find(),
            KeyCode::Char('?') if alt && !ctrl => self.open_find(),
            KeyCode::F(7) => self.start_input("Make directory", "New directory name:", "", InputAction::Mkdir),
            KeyCode::F(8) => self.confirm_delete(!shift),
            KeyCode::Delete if cmd_empty => self.confirm_delete(!shift),
            KeyCode::F(9) => self.open_sort_menu(),
            KeyCode::F(10) => self.request_quit(),
            KeyCode::F(12) => self.dialog = Some(Dialog::Commands { filter: LineEdit::default(), cursor: 0 }),
            KeyCode::Char('p') if ctrl && !alt => self.open_options(),
            KeyCode::Char('q') if ctrl && !alt => self.toggle_quick_view(),
            KeyCode::Char('n') if ctrl && !alt => {
                self.start_input("Connect over SSH", "Connect to (host, user@host:port, host:/path):", "", InputAction::Connect);
            }
            KeyCode::Char('c') if ctrl && !alt => self.copy_to_clipboard(Effect::Copy),
            KeyCode::Char('C') if ctrl && !alt => self.copy_paths(),
            KeyCode::Char('x') if ctrl && !alt => self.copy_to_clipboard(Effect::Move),
            // Windows Terminal keeps Ctrl+V for its own paste, hence Alt+V.
            KeyCode::Char('v') if ctrl != alt => self.paste(),
            // With the quick view on, Tab goes into it (to scroll) rather than to the other panel.
            KeyCode::Tab if self.quick_view => self.preview_focus = true,
            KeyCode::Tab => self.active = 1 - self.active,
            // Back and forward as in a browser or Explorer, and mc's Alt+Y / Alt+U.
            KeyCode::Left | KeyCode::Char('y') if alt && !ctrl => self.step_back(true),
            KeyCode::Right | KeyCode::Char('u') if alt && !ctrl => self.step_back(false),
            KeyCode::Down | KeyCode::Char('h') if alt && !ctrl => self.open_dir_history(),
            KeyCode::Up | KeyCode::Down if shift => {
                let panel = self.active_panel_mut();
                panel.toggle_mark();
                panel.move_by(if key.code == KeyCode::Up { -1 } else { 1 });
            }
            KeyCode::Up => self.active_panel_mut().move_by(-1),
            KeyCode::Down => self.active_panel_mut().move_by(1),
            // Up a folder and into one: Ctrl+PgUp/PgDn as in Total Commander and Far, and with
            // an empty command line Left/Right as mc's "Lynx-like motion" (an option).
            KeyCode::PageUp if ctrl => self.go_up(),
            KeyCode::PageDown if ctrl => self.enter_current_dir(),
            KeyCode::Left if cmd_empty && plain && self.config.settings.arrows_navigate => self.go_up(),
            KeyCode::Right if cmd_empty && plain && self.config.settings.arrows_navigate => self.enter_current_dir(),
            KeyCode::PageUp => self.active_panel_mut().page_up(),
            KeyCode::PageDown => self.active_panel_mut().page_down(),
            KeyCode::Home => self.active_panel_mut().home(),
            KeyCode::End => self.active_panel_mut().end(),
            KeyCode::Insert => {
                let panel = self.active_panel_mut();
                panel.toggle_mark();
                panel.move_by(1);
            }
            KeyCode::Enter if ctrl || alt => self.insert_current_name(),
            KeyCode::Enter if shift && cmd_empty => self.open_with(),
            KeyCode::Enter if !cmd_empty => self.run_cmdline(),
            KeyCode::Enter => self.open_current(),
            KeyCode::Backspace if cmd_empty => self.go_up(),
            KeyCode::Esc if cmd_empty => self.active_panel_mut().stop_reading(),
            KeyCode::Esc => self.cmdline.clear(),
            KeyCode::Char('r') if ctrl && !alt => {
                self.reload_panels();
                self.git.retry();
            }
            KeyCode::Char('h') if ctrl && !alt => self.toggle_hidden(),
            KeyCode::Char('.') if alt && !ctrl => self.toggle_hidden(),
            KeyCode::Char('u') if ctrl && !alt => {
                self.panels.swap(0, 1);
                self.tabs.swap(0, 1);
                self.tab_index.swap(0, 1);
            }
            KeyCode::Char('t') if ctrl && !alt => self.new_tab(),
            KeyCode::Char('w') if ctrl && !alt && cmd_empty => self.close_tab(),
            KeyCode::Char(c @ '1'..='9') if alt && !ctrl => self.switch_tab(self.active, c as usize - '1' as usize),
            KeyCode::Char('o') if ctrl && !alt => self.show_shell(),
            // Ctrl+\ is a dead key on the Czech layout, so Ctrl+D (as in Total Commander) works too.
            // Unix terminals send Ctrl+\ as 0x1C, which crossterm reports as Ctrl+4.
            KeyCode::Char('\\' | 'd' | '\u{1c}' | '4') if ctrl && !alt => self.open_hotlist(),
            KeyCode::Char(' ') if ctrl && !alt => self.calculate_sizes(),
            // Ctrl+S or Alt+S, as in mc (both together is AltGr, which types a character).
            KeyCode::Char('s') if ctrl != alt => self.quicksearch = Some(String::new()),
            KeyCode::Char('i') if alt && !ctrl => {
                let place = self.active_panel().place();
                self.change_dir(1 - self.active, place);
            }
            KeyCode::Char('o') if alt && !ctrl => {
                let target = match self.active_panel().current() {
                    Some(entry) if entry.is_dir && !entry.is_parent => entry.path.clone(),
                    _ => self.active_panel().place(),
                };
                self.change_dir(1 - self.active, target);
            }
            KeyCode::Char('p') if alt && !ctrl => self.history_step(-1),
            KeyCode::Char('n') if alt && !ctrl => self.history_step(1),
            KeyCode::Char('+') if cmd_empty && plain => {
                let prompt = format!("Mark {}", self.what_patterns_mark());
                self.start_input("Select", &prompt, "*", InputAction::Select)
            }
            KeyCode::Char('-') if cmd_empty && plain => {
                let prompt = format!("Unmark {}", self.what_patterns_mark());
                self.start_input("Unselect", &prompt, "*", InputAction::Unselect)
            }
            KeyCode::Char('*') if cmd_empty && plain => self.active_panel_mut().invert_marks(),
            KeyCode::Char('a') if ctrl && !alt && cmd_empty => self.active_panel_mut().mark_all(),
            KeyCode::Char('a') if alt && !ctrl => self.open_attributes(),
            KeyCode::Char('c') if alt && !ctrl => self.open_schemes(None),
            KeyCode::Char('w') if alt && !ctrl => self.open_worktrees(),
            KeyCode::Char('b') if alt && !ctrl => self.open_branches(),
            // With text on the command line Ctrl+E goes to its end, as in bash.
            KeyCode::Char('e') if ctrl && !alt && cmd_empty => self.open_places(),
            KeyCode::Char('f') if ctrl && !alt => {
                let current = self.active_panel().filter.clone().unwrap_or_default();
                self.start_input("Filter", "Show only files matching (empty shows all):", &current, InputAction::Filter);
            }
            _ => {
                self.cmdline.handle_key(&key);
            }
        }
    }

    /// Keys while quick search is on. Returns false for keys that end it and should act normally.
    fn on_quicksearch_key(&mut self, key: &KeyEvent) -> bool {
        let Some(query) = self.quicksearch.as_mut() else { return false };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // The same key again: the next name that starts with what was typed.
            KeyCode::Char('s') if ctrl != alt => {
                let query = query.clone();
                self.active_panel_mut().find_prefix(&query, true);
            }
            KeyCode::Char(c) if dialog::is_text_input(key) => {
                query.push(c);
                let attempt = query.clone();
                if !self.active_panel_mut().find_prefix(&attempt, false)
                    && let Some(query) = self.quicksearch.as_mut()
                {
                    query.pop();
                }
            }
            KeyCode::Backspace => {
                query.pop();
            }
            KeyCode::Esc => self.quicksearch = None,
            _ => {
                self.quicksearch = None;
                return false;
            }
        }
        true
    }

    fn on_dialog_key(&mut self, key: KeyEvent) {
        let Some(dialog) = self.dialog.take() else { return };
        // Handlers may open a new dialog (an error, say); it wins over keeping this one open.
        let keep = match dialog {
            Dialog::Message { .. } if matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ')) => None,
            dialog @ Dialog::Message { .. } => Some(dialog),
            Dialog::Help { scroll } => match key.code {
                KeyCode::Up => Some(Dialog::Help { scroll: scroll.saturating_sub(1) }),
                KeyCode::Down => Some(Dialog::Help { scroll: (scroll + 1).min(dialog::HELP.len()) }),
                KeyCode::PageUp => Some(Dialog::Help { scroll: scroll.saturating_sub(10) }),
                KeyCode::PageDown => Some(Dialog::Help { scroll: (scroll + 10).min(dialog::HELP.len()) }),
                _ => None,
            },
            Dialog::Confirm { title, lines, mut focus, danger, action } => {
                match dialog::handle_buttons(dialog::YES_NO, &mut focus, &key) {
                    ButtonOutcome::Pressed(0) => {
                        self.on_confirm(action);
                        None
                    }
                    ButtonOutcome::Pressed(_) | ButtonOutcome::Cancel => None,
                    ButtonOutcome::Pending => Some(Dialog::Confirm { title, lines, focus, danger, action }),
                }
            }
            Dialog::Input { title, prompt, mut edit, action, mut recall } => {
                let alt = key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Enter => {
                        self.on_input(action, edit.text().to_string());
                        return;
                    }
                    KeyCode::Esc => return,
                    KeyCode::Char('h') | KeyCode::Down if alt => {
                        let back = Box::new(Dialog::Input { title, prompt, edit, action, recall });
                        self.dialog = Some(Dialog::Earlier { filter: LineEdit::default(), cursor: 0, back });
                        return;
                    }
                    KeyCode::Up => recall.older(&mut edit),
                    KeyCode::Down => recall.newer(&mut edit),
                    KeyCode::Char('p') if alt => recall.older(&mut edit),
                    KeyCode::Char('n') if alt => recall.newer(&mut edit),
                    _ => {
                        edit.handle_key(&key);
                    }
                }
                Some(Dialog::Input { title, prompt, edit, action, recall })
            }
            Dialog::Earlier { filter, cursor, back } => self.on_earlier_key(&key, filter, cursor, back),
            Dialog::Drives { side, drives, mut cursor } => match key.code {
                KeyCode::Up => {
                    cursor = cursor.saturating_sub(1);
                    Some(Dialog::Drives { side, drives, cursor })
                }
                KeyCode::Down => {
                    cursor = (cursor + 1).min(drives.len().saturating_sub(1));
                    Some(Dialog::Drives { side, drives, cursor })
                }
                KeyCode::Home | KeyCode::PageUp => Some(Dialog::Drives { side, drives, cursor: 0 }),
                KeyCode::End | KeyCode::PageDown => {
                    cursor = drives.len().saturating_sub(1);
                    Some(Dialog::Drives { side, drives, cursor })
                }
                KeyCode::Enter => {
                    if let Some(drive) = drives.get(cursor) {
                        self.switch_drive(side, &drive.root);
                    }
                    None
                }
                KeyCode::Char(c) if c.is_ascii_alphabetic() => {
                    // `E:\` on Windows; elsewhere the mount whose folder starts with it (/mnt/e).
                    let wanted = format!("{}:", c.to_ascii_uppercase());
                    let starts = |d: &&platform::DriveInfo| {
                        let root = d.root.to_string_lossy();
                        root.starts_with(&wanted)
                            || (!cfg!(windows)
                                && d.root.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().starts_with(c.to_ascii_lowercase())))
                    };
                    match drives.iter().find(starts) {
                        Some(drive) => {
                            self.switch_drive(side, &drive.root);
                            None
                        }
                        None => Some(Dialog::Drives { side, drives, cursor }),
                    }
                }
                KeyCode::Esc => None,
                _ => Some(Dialog::Drives { side, drives, cursor }),
            },
            Dialog::Hotlist { cursor } => self.on_hotlist_key(&key, cursor),
            Dialog::History { filter, cursor } => self.on_history_key(&key, filter, cursor),
            Dialog::Commands { filter, cursor } => self.on_commands_key(&key, filter, cursor),
            Dialog::Worktrees { filter, cursor, worktrees, here } => {
                self.on_worktrees_key(&key, filter, cursor, worktrees, here)
            }
            Dialog::Branches { filter, cursor, branches, here, coming } => {
                self.on_branches_key(&key, filter, cursor, branches, here, coming)
            }
            Dialog::Places { root, id, places, cursor } => self.on_places_key(&key, root, id, places, cursor),
            Dialog::Attributes { paths, states, cursor } => self.on_attributes_key(&key, paths, states, cursor),
            Dialog::Delete { paths, to_trash, mut focus, survey } => {
                let buttons = dialog::delete_buttons(&survey.tally());
                match dialog::handle_buttons(buttons, &mut focus, &key) {
                    ButtonOutcome::Pressed(0) => {
                        drop(survey);
                        self.start_job(Job::Delete { paths, to_trash, admin_when_refused: false });
                        None
                    }
                    // As admin. To the Recycle Bin: whatever it refuses goes to the elevated
                    // helper (permanently), the rest still to the bin. Permanently: all of it.
                    ButtonOutcome::Pressed(2) if to_trash => {
                        drop(survey);
                        self.start_job(Job::Delete { paths, to_trash, admin_when_refused: true });
                        None
                    }
                    ButtonOutcome::Pressed(2) => {
                        let going: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                        self.prepare_for_job(&going);
                        self.job = Some(admin::start(paths, Vec::new()));
                        None
                    }
                    ButtonOutcome::Pressed(_) | ButtonOutcome::Cancel => None,
                    ButtonOutcome::Pending => Some(Dialog::Delete { paths, to_trash, focus, survey }),
                }
            }
            Dialog::Options { cursor, edit, settings, automatic } => {
                self.on_options_key(&key, cursor, edit, settings, automatic)
            }
            Dialog::Schemes { cursor, choices, back } => self.on_schemes_key(&key, cursor, choices, back),
            Dialog::Sort { side, mut cursor, current, reverse } => {
                let last = SortBy::ALL.len() - 1;
                let chosen = match key.code {
                    KeyCode::Up => {
                        cursor = cursor.saturating_sub(1);
                        None
                    }
                    KeyCode::Down => {
                        cursor = (cursor + 1).min(last);
                        None
                    }
                    KeyCode::Home | KeyCode::PageUp => {
                        cursor = 0;
                        None
                    }
                    KeyCode::End | KeyCode::PageDown => {
                        cursor = last;
                        None
                    }
                    // F9 again picks the row under the cursor, which starts on the current
                    // order: F9 F9 flips it.
                    KeyCode::Enter | KeyCode::F(9) => SortBy::ALL.get(cursor).copied(),
                    KeyCode::Char(c) => SortBy::ALL.iter().copied().find(|s| s.letter() == c.to_ascii_lowercase()),
                    KeyCode::Esc | KeyCode::F(10) => return,
                    _ => None,
                };
                match chosen {
                    Some(by) => {
                        self.panels[side].choose_sort(by);
                        None
                    }
                    None => Some(Dialog::Sort { side, cursor, current, reverse }),
                }
            }
            Dialog::Question { title, lines, buttons, answers, mut focus, error, reply } => {
                match dialog::handle_buttons(buttons, &mut focus, &key) {
                    ButtonOutcome::Pressed(i) => {
                        let _ = reply.send(answers[i]);
                        None
                    }
                    ButtonOutcome::Cancel => {
                        let _ = reply.send(Answer::Cancel);
                        None
                    }
                    ButtonOutcome::Pending => {
                        Some(Dialog::Question { title, lines, buttons, answers, focus, error, reply })
                    }
                }
            }
        };
        if self.dialog.is_none() {
            self.dialog = keep;
        }
    }

    /// Keys in the list of what was typed into a field before. Returns the dialog to show: the
    /// list, or the field again.
    fn on_earlier_key(&mut self, key: &KeyEvent, mut filter: LineEdit, mut cursor: usize, mut back: Box<Dialog>) -> Option<Dialog> {
        let alt = key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::CONTROL);
        let rows = dialog::earlier_rows(&back, filter.text());
        let last = rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => return Some(*back),
            KeyCode::Char('h') if alt => return Some(*back),
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(last),
            KeyCode::PageUp => cursor = cursor.saturating_sub(10),
            KeyCode::PageDown => cursor = (cursor + 10).min(last),
            KeyCode::Enter => {
                if let Some(row) = rows.get(cursor) {
                    dialog::take_earlier(&mut back, row);
                }
                return Some(*back);
            }
            KeyCode::Delete => {
                if let (Some(row), Dialog::Input { action, recall, .. }) = (rows.get(cursor), back.as_mut()) {
                    self.history.inputs.forget(action.kind(), row);
                    self.history_changed = true;
                    recall.forget(row);
                    cursor = cursor.min(rows.len().saturating_sub(2));
                }
            }
            _ => {
                let before = filter.text().to_string();
                filter.handle_key(key);
                if filter.text() != before {
                    cursor = 0;
                }
            }
        }
        Some(Dialog::Earlier { filter, cursor, back })
    }

    /// Keys while the shell is shown: Ctrl+O goes back to the panels, Shift+PgUp/PgDn scroll
    /// through its output, everything else is typed into it.
    fn on_shell_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = self.screen_size.0.saturating_sub(2).max(1) as isize;
        let Some(shell) = self.subshell.as_mut() else {
            self.shell_shown = false;
            return;
        };
        match key.code {
            KeyCode::Char('o') if ctrl && !alt => self.hide_shell(),
            KeyCode::PageUp if shift => shell.scroll(page),
            KeyCode::PageDown if shift => shell.scroll(-page),
            _ => shell.send_key(&key),
        }
    }

    /// Starts the persistent shell (in the active panel's folder) unless it is running.
    fn ensure_subshell(&mut self) -> Result<(), String> {
        if self.subshell.is_none() {
            let (rows, cols) = self.screen_size;
            let cwd = self.active_panel().cwd.clone();
            self.subshell = Some(Subshell::start(&self.config.shell, &cwd, rows.max(2), cols.max(10))?);
        }
        Ok(())
    }

    /// Ctrl+O: shows the shell, moved to the panel's folder if it is idle somewhere else.
    /// If the shell can't run inside cm, a one-off shell takes over the console instead.
    fn show_shell(&mut self) {
        if self.ensure_subshell().is_err() {
            self.external = Some(External::Shell);
            return;
        }
        let cwd = self.active_panel().cwd.clone();
        if let Some(shell) = self.subshell.as_mut() {
            shell.follow_panel(&cwd);
            shell.scroll_to_bottom();
            self.shell_shown = true;
        }
    }

    /// Back to the panels. If the user changed folder in the shell, the active panel follows.
    pub(crate) fn hide_shell(&mut self) {
        self.shell_shown = false;
        let moved_to = self.subshell.as_ref().and_then(Subshell::moved_to);
        // The shell may have created, changed or deleted files.
        self.reload_panels();
        if let Some(dir) = moved_to
            && !util::same_path(&dir, &self.active_panel().cwd)
        {
            self.follow_shell(dir);
        }
    }

    /// The shell has moved to `dir`: the active panel follows if it is still there, unless it
    /// shows another computer, whose folder the shell has nothing to do with.
    fn follow_shell(&mut self, dir: PathBuf) {
        if self.active_panel().remote.is_none() {
            self.panels[self.active].go_to_first(&[dir], true);
            self.settle(self.active);
        } else if dir.is_dir() {
            self.active_panel_mut().cwd = dir;
        }
    }

    /// Runs a command line in the persistent shell (in the panel's folder) and shows it.
    fn run_in_shell(&mut self, line: String) {
        if self.ensure_subshell().is_err() {
            self.external = Some(External::Command(line));
            return;
        }
        let cwd = self.active_panel().cwd.clone();
        let Some(shell) = self.subshell.as_mut() else { return };
        if !shell.is_idle() {
            self.error("The shell is still busy with a command.\nPress Ctrl+O to see it.");
            return;
        }
        shell.follow_panel(&cwd);
        shell.send_text(&format!("{line}\r"));
        self.shell_shown = true;
    }

    /// Alt+F7: the Find window for the active panel's folder. It comes back as it was left;
    /// if the panel has moved since, the same query runs in the new folder.
    fn open_find(&mut self) {
        if self.refuse_over_ssh("Find looks through files on this computer") {
            return;
        }
        let cwd = self.active_panel().cwd.clone();
        self.finder = Some(match self.finder.take() {
            Some(finder) if util::same_path(&finder.root, &cwd) => finder,
            Some(finder) => finder.moved_to(cwd),
            None => Finder::new(cwd),
        });
        self.find_shown = true;
    }

    /// Keys in the Find window: typing goes to the field (Tab switches), arrows and pages
    /// move through the results, Enter goes there, F3/F4 open the file at the line,
    /// Alt+U searches everything, Esc closes.
    fn on_find_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let Some(finder) = self.finder.as_mut() else {
            self.find_shown = false;
            return;
        };
        let page = finder.page as isize;
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => self.close_find(),
            KeyCode::F(7) | KeyCode::Char('?') if alt => self.close_find(),
            KeyCode::Char('p') if alt && !ctrl => self.find_recall(true),
            KeyCode::Char('n') if alt && !ctrl => self.find_recall(false),
            KeyCode::Tab | KeyCode::BackTab => {
                finder.field = if finder.field == FindField::Text { FindField::Names } else { FindField::Text };
            }
            KeyCode::Up => finder.move_by(-1),
            KeyCode::Down => finder.move_by(1),
            KeyCode::PageUp => finder.move_by(-page),
            KeyCode::PageDown => finder.move_by(page),
            KeyCode::Home if ctrl => finder.move_to(0),
            KeyCode::End if ctrl => finder.move_to(usize::MAX),
            KeyCode::Char('u') if alt && !ctrl => {
                finder.everything = !finder.everything;
                finder.changed();
            }
            KeyCode::Enter => self.find_go_to(),
            KeyCode::F(3) => self.find_open(false),
            KeyCode::F(4) => self.find_open(true),
            _ => {
                let field = finder.field_mut();
                let before = field.text().to_string();
                field.handle_key(&key);
                if field.text() != before {
                    finder.changed();
                }
            }
        }
    }

    /// Find closes, or something it found is used: what it looked for joins its fields' lists.
    fn remember_find(&mut self) {
        let Some(finder) = &self.finder else { return };
        self.history.inputs.add(FIND_TEXT, finder.text.text().trim());
        self.history.inputs.add(FIND_FILES, finder.names.text().trim());
        self.history_changed = true;
        self.find_recall = None;
    }

    fn close_find(&mut self) {
        self.remember_find();
        self.find_shown = false;
    }

    /// Alt+P / Alt+N in Find: the field goes through what was searched for before. (Up and Down
    /// are the results' there.)
    fn find_recall(&mut self, older: bool) {
        let Some(finder) = self.finder.as_mut() else { return };
        let kind = if finder.field == FindField::Text { FIND_TEXT } else { FIND_FILES };
        if self.find_recall.as_ref().is_none_or(|(field, _)| *field != finder.field) {
            self.find_recall = Some((finder.field, Recall::new(self.history.inputs.list(kind))));
        }
        let Some((_, recall)) = self.find_recall.as_mut() else { return };
        let field = finder.field_mut();
        if older { recall.older(field) } else { recall.newer(field) }
        finder.changed();
    }

    /// Enter in Find: the panel goes to the folder of the result, the cursor on it.
    pub(crate) fn find_go_to(&mut self) {
        let Some(path) = self.finder.as_ref().and_then(Finder::selected).map(|(path, ..)| path.to_path_buf()) else {
            return;
        };
        self.remember_find();
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return };
        self.change_dir(self.active, dir.to_path_buf());
        self.active_panel_mut().select_name(&name.to_string_lossy());
        self.find_shown = false;
    }

    /// F3/F4 in Find: the file at the matching line (the big-file rules apply). The Find
    /// window is still there afterwards.
    fn find_open(&mut self, edit: bool) {
        let Some((path, line, is_dir)) = self.finder.as_ref().and_then(Finder::selected).map(|(p, l, d)| (p.to_path_buf(), l, d))
        else {
            return;
        };
        if is_dir {
            return self.find_go_to();
        }
        self.remember_find();
        let size = fs::metadata(&path).map_or(0, |m| m.len());
        if edit { self.edit(path, size, line) } else { self.view(path, size, line) }
    }

    /// F10: quits right away, unless the settings say to ask, or quitting would kill a
    /// command still running in the shell. (During a copy only Esc gets through.)
    fn request_quit(&mut self) {
        let shell_busy = self.subshell.as_ref().is_some_and(|s| !s.is_idle());
        if !self.config.settings.confirm_quit && !shell_busy {
            self.quit = true;
            return;
        }
        let mut lines = Vec::new();
        if shell_busy {
            lines.push("The shell is still busy; quitting ends what runs in it.".to_string());
        }
        lines.push("Quit cm?".into());
        self.dialog = Some(Dialog::Confirm { title: "Quit".into(), lines, focus: 0, danger: shell_busy, action: ConfirmAction::Quit });
    }

    /// Ctrl+P: the options editor, starting on the first row.
    fn open_options(&mut self) {
        let settings = self.config.settings.clone();
        let edit = LineEdit::default();
        self.dialog = Some(Dialog::Options { cursor: 0, edit, settings, automatic: OptionsAutomatic::detect() });
    }

    /// Keys in the options editor: arrows (and Tab) move, Space or Enter toggles a checkbox,
    /// typing changes a text row, which counts when the cursor leaves it; Esc closes.
    /// Returns the dialog if it stays open.
    fn on_options_key(
        &mut self,
        key: &KeyEvent,
        cursor: usize,
        mut edit: LineEdit,
        mut settings: Settings,
        automatic: OptionsAutomatic,
    ) -> Option<Dialog> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let row = options::ROWS[cursor];
        // The colour scheme is picked from Alt+C's list, which comes back here.
        if row.is_choice() && matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
            self.open_schemes(Some(Box::new(Dialog::Options { cursor, edit, settings, automatic })));
            return None;
        }
        let typed = !row.is_flag() && !row.is_choice();
        let last = options::ROWS.len() - 1;
        let close = matches!(key.code, KeyCode::Esc | KeyCode::F(10)) || (ctrl && key.code == KeyCode::Char('p'));
        let target = match key.code {
            KeyCode::Up | KeyCode::BackTab => Some(cursor.saturating_sub(1)),
            KeyCode::Down | KeyCode::Tab => Some((cursor + 1).min(last)),
            KeyCode::PageUp => Some(0),
            KeyCode::PageDown => Some(last),
            KeyCode::Enter if !row.is_flag() => Some((cursor + 1).min(last)),
            _ => None,
        };
        if row.is_flag() && matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) {
            row.toggle(&mut settings);
            self.apply_settings(settings.clone());
        } else if typed && target.is_none() && !close {
            edit.handle_key(key);
        }
        if (close || target.is_some()) && typed {
            row.set_text(&mut settings, edit.text());
            self.apply_settings(settings.clone());
        }
        if close {
            return None;
        }
        let cursor = target.unwrap_or(cursor);
        if target.is_some() {
            edit = LineEdit::new(options::ROWS[cursor].text(&settings));
        }
        Some(Dialog::Options { cursor, edit, settings, automatic })
    }

    /// A click on row `i` of the options editor: the cursor goes there, and a checkbox toggles.
    pub(crate) fn click_option(&mut self, i: usize) {
        let Some(Dialog::Options { cursor, edit, mut settings, automatic }) = self.dialog.take() else { return };
        let (Some(&from), Some(&to)) = (options::ROWS.get(cursor), options::ROWS.get(i)) else {
            self.dialog = Some(Dialog::Options { cursor, edit, settings, automatic });
            return;
        };
        if !from.is_flag() {
            from.set_text(&mut settings, edit.text());
        }
        if to.is_flag() {
            to.toggle(&mut settings);
        }
        self.apply_settings(settings.clone());
        let edit = LineEdit::new(to.text(&settings));
        if self.dialog.is_none() {
            let options = Dialog::Options { cursor: i, edit, settings, automatic };
            if to.is_choice() {
                self.open_schemes(Some(Box::new(options)));
            } else {
                self.dialog = Some(options);
            }
        }
    }

    /// Takes changed settings: saves them, and applies what can change while running
    /// (hidden files, the mouse, commands, the big-file limit, the colour scheme).
    fn apply_settings(&mut self, settings: Settings) {
        if settings == self.config.settings {
            return;
        }
        let scheme_changed = settings.scheme != self.config.settings.scheme;
        if let Some(file) = &self.config.settings_file
            && let Err(e) = settings.save(file)
        {
            self.error(format!("Cannot save the settings:\n{e}"));
        }
        crate::panel::NATURAL_SORT.store(settings.natural_sort, Ordering::Relaxed);
        if settings.show_hidden != self.config.settings.show_hidden || settings.natural_sort != self.config.settings.natural_sort {
            // The tabs in the background too, or switching to one would bring the old back.
            for panel in self.panels.iter_mut().chain(self.tabs.iter_mut().flatten()) {
                panel.show_hidden = settings.show_hidden;
                panel.reload();
            }
        }
        self.mouse = settings.mouse && self.mouse_allowed;
        self.git.set_enabled(settings.git_status);
        let (hotlist_file, settings_file) = (self.config.hotlist_file.take(), self.config.settings_file.take());
        let history_file = self.config.history_file.take();
        let (schemes_dir, colours) = (self.config.schemes_dir.take(), self.config.colours);
        let places_file = self.config.places_file.take();
        self.config = Config::new(settings, hotlist_file, settings_file);
        self.config.history_file = history_file;
        self.config.schemes_dir = schemes_dir;
        self.config.places_file = places_file;
        self.config.colours = colours;
        if scheme_changed {
            self.scheme = self.saved_scheme();
        }
    }

    /// The colour scheme the settings name, as the terminal shows it; classic if there is no
    /// such scheme.
    fn saved_scheme(&self) -> Scheme {
        scheme::find(&self.config.settings.scheme, self.config.schemes_dir.as_deref())
            .map_or_else(scheme::classic, |choice| choice.scheme)
            .for_terminal(self.config.colours)
    }

    /// Alt+C: every colour scheme, the cursor on the one in use. `back` is the options editor
    /// when it was opened from there.
    fn open_schemes(&mut self, back: Option<Box<Dialog>>) {
        let choices = scheme::choices(self.config.schemes_dir.as_deref());
        let current = &self.config.settings.scheme;
        let cursor = choices.iter().position(|c| c.name.eq_ignore_ascii_case(current)).unwrap_or(0);
        self.dialog = Some(Dialog::Schemes { cursor, choices, back });
    }

    /// Keys in the scheme list. Moving shows the scheme under the cursor on the whole screen;
    /// Enter keeps it (and saves it), Esc goes back to the one before; F4 opens its file in the
    /// editor, writing a built-in one out first; a letter jumps to the next name starting
    /// with it.
    fn on_schemes_key(
        &mut self,
        key: &KeyEvent,
        mut cursor: usize,
        choices: Vec<Choice>,
        back: Option<Box<Dialog>>,
    ) -> Option<Dialog> {
        let last = choices.len().saturating_sub(1);
        let before = cursor;
        match key.code {
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(last),
            KeyCode::Home | KeyCode::PageUp => cursor = 0,
            KeyCode::End | KeyCode::PageDown => cursor = last,
            KeyCode::Enter => {
                if let Some(choice) = choices.get(cursor) {
                    self.scheme = choice.scheme.for_terminal(self.config.colours);
                    let mut settings = self.config.settings.clone();
                    settings.scheme = choice.name.clone();
                    self.apply_settings(settings);
                }
                return self.back_from_schemes(back);
            }
            KeyCode::Esc | KeyCode::F(10) => {
                self.scheme = self.saved_scheme();
                return self.back_from_schemes(back);
            }
            KeyCode::F(4) => {
                if let Some(choice) = choices.get(cursor) {
                    match scheme::file_for(choice, self.config.schemes_dir.as_deref()) {
                        Ok((file, written)) => {
                            self.scheme_file_written = written.map(|text| (file.clone(), text));
                            self.external = Some(External::Edit(file, None));
                        }
                        Err(e) => {
                            self.scheme = self.saved_scheme();
                            self.error(format!("Cannot write the scheme to a file:\n{e}"));
                        }
                    }
                }
            }
            KeyCode::Char(c) if dialog::is_text_input(key) => {
                let c = c.to_lowercase().to_string();
                let starts = |i: &usize| choices[*i].name.to_lowercase().starts_with(&c);
                if let Some(i) = (cursor + 1..choices.len()).chain(0..=cursor).find(starts) {
                    cursor = i;
                }
            }
            _ => {}
        }
        if cursor != before
            && let Some(choice) = choices.get(cursor)
        {
            self.scheme = choice.scheme.for_terminal(self.config.colours);
        }
        Some(Dialog::Schemes { cursor, choices, back })
    }

    /// Where the scheme list goes when it closes: back to the options editor if it came from
    /// there, which then shows the scheme in use.
    fn back_from_schemes(&self, back: Option<Box<Dialog>>) -> Option<Dialog> {
        let mut dialog = *back?;
        if let Dialog::Options { settings, .. } = &mut dialog {
            settings.scheme = self.config.settings.scheme.clone();
        }
        Some(dialog)
    }

    /// Back from the editor with the scheme list open: the schemes are read again, and the
    /// one under the cursor (the one edited) shown as it is now. A file F4 wrote out that
    /// nothing was changed in is removed first, so the built-in scheme is back.
    fn reload_schemes(&mut self) {
        if let Some((file, text)) = self.scheme_file_written.take()
            && fs::read_to_string(&file).is_ok_and(|now| now == text)
        {
            let _ = fs::remove_file(&file);
        }
        let Some(Dialog::Schemes { cursor, choices, .. }) = &mut self.dialog else { return };
        let name = choices.get(*cursor).map(|c| c.name.clone()).unwrap_or_default();
        *choices = scheme::choices(self.config.schemes_dir.as_deref());
        *cursor = choices.iter().position(|c| c.name.eq_ignore_ascii_case(&name)).unwrap_or(0);
        if let Some(choice) = choices.get(*cursor) {
            self.scheme = choice.scheme.for_terminal(self.config.colours);
        }
    }

    /// F9: a menu of sort orders, the cursor on the current one (so F9 F9 reverses it).
    fn open_sort_menu(&mut self) {
        let panel = self.active_panel();
        let (current, reverse) = (panel.sort, panel.reverse);
        let cursor = SortBy::ALL.iter().position(|&s| s == current).unwrap_or(0);
        self.dialog = Some(Dialog::Sort { side: self.active, cursor, current, reverse });
    }

    fn open_hotlist(&mut self) {
        let cwd = self.active_panel().place();
        let cursor = self.hotlist.dirs.iter().position(|d| util::same_path(d, &cwd)).unwrap_or(0);
        self.dialog = Some(Dialog::Hotlist { cursor });
    }

    /// Hotlist keys: Enter or 1-9 jump, Insert/A add the current folder, Delete/D remove,
    /// Shift+Up/Down reorder. Returns the dialog if it stays open.
    fn on_hotlist_key(&mut self, key: &KeyEvent, mut cursor: usize) -> Option<Dialog> {
        let len = self.hotlist.dirs.len();
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up if shift && cursor > 0 && cursor < len => {
                self.hotlist.dirs.swap(cursor, cursor - 1);
                cursor -= 1;
                self.save_hotlist();
            }
            KeyCode::Down if shift && cursor + 1 < len => {
                self.hotlist.dirs.swap(cursor, cursor + 1);
                cursor += 1;
                self.save_hotlist();
            }
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(len.saturating_sub(1)),
            KeyCode::Home | KeyCode::PageUp => cursor = 0,
            KeyCode::End | KeyCode::PageDown => cursor = len.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(dir) = self.hotlist.dirs.get(cursor).cloned() {
                    self.change_dir(self.active, dir);
                }
                return None;
            }
            KeyCode::Char(c @ '1'..='9') if !ctrl => {
                let index = c as usize - '1' as usize;
                if let Some(dir) = self.hotlist.dirs.get(index).cloned() {
                    self.change_dir(self.active, dir);
                    return None;
                }
            }
            KeyCode::Insert | KeyCode::Char('a' | 'A' | '+') if !ctrl => {
                let cwd = self.active_panel().place();
                cursor = self.hotlist.add(&cwd);
                self.save_hotlist();
            }
            KeyCode::Delete | KeyCode::Char('d' | 'D' | '-') if !ctrl && cursor < len => {
                self.hotlist.dirs.remove(cursor);
                cursor = cursor.min(len.saturating_sub(2));
                self.save_hotlist();
            }
            KeyCode::Esc | KeyCode::F(10) => return None,
            KeyCode::Char('\\' | 'd' | '\u{1c}' | '4') if ctrl => return None,
            _ => {}
        }
        Some(Dialog::Hotlist { cursor })
    }

    fn save_hotlist(&mut self) {
        if let Err(e) = self.hotlist.save() {
            self.error(format!("Cannot save the hotlist:\n{e}"));
        }
    }

    /// Ctrl+Space: measures the marked directories, the one under the cursor,
    /// or on ".." every directory in the panel.
    fn calculate_sizes(&mut self) {
        if self.refuse_over_ssh("Ctrl+Space measures folders on this computer") {
            return;
        }
        // In an archive the sizes are all in its index: no measuring needed.
        if let Some(view) = &self.panels[self.active].archive {
            let sizes: Vec<(String, u64)> = view
                .archive
                .children(&view.inner)
                .filter(|item| item.is_dir)
                .map(|item| (item.name().to_string(), view.archive.size_below(&item.path)))
                .collect();
            let panel = &mut self.panels[self.active];
            for (name, bytes) in sizes {
                panel.dir_sizes.insert(name, DirSize { bytes, done: true });
            }
            return;
        }
        let panel = &self.panels[self.active];
        let marked: Vec<_> = panel.entries.iter().filter(|e| e.is_dir && panel.is_marked(e)).collect();
        let targets: Vec<(String, PathBuf)> = if !marked.is_empty() {
            marked.iter().map(|e| (e.name.clone(), e.path.clone())).collect()
        } else {
            match panel.current() {
                Some(current) if current.is_parent => panel
                    .entries
                    .iter()
                    .filter(|e| e.is_dir && !e.is_parent)
                    .map(|e| (e.name.clone(), e.path.clone()))
                    .collect(),
                Some(current) if current.is_dir => vec![(current.name.clone(), current.path.clone())],
                _ => Vec::new(),
            }
        };
        let sizer = self.sizer.get_or_insert_with(Sizer::new);
        let panel = &mut self.panels[self.active];
        for (name, path) in targets {
            if panel.dir_sizes.get(&name).is_some_and(|s| !s.done) {
                continue;
            }
            panel.dir_sizes.insert(name, DirSize { bytes: 0, done: false });
            sizer.request(path);
        }
    }

    fn on_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Quit => self.quit = true,
            ConfirmAction::Edit(path, line) => self.external = Some(External::Edit(path, line)),
            ConfirmAction::View(path, line) => self.external = Some(External::View(path, line)),
            ConfirmAction::Pack(sources, dest) => self.start_job(Job::Pack { sources, dest }),
            ConfirmAction::DeleteThere(connection, paths) => self.start_job(Job::DeleteThere { connection, paths }),
            ConfirmAction::PutBack(copy) => self.put_back(*copy),
            ConfirmAction::Switch { root, branch } => self.switch_branch(root, branch),
        }
    }

    /// Where a zip goes by default, or what comes out of one: the other panel's folder, or this
    /// one's if that shows another computer, whose hidden folder here means nothing.
    fn folder_here(&self) -> PathBuf {
        let other = &self.panels[1 - self.active];
        if other.remote.is_some() { self.active_panel().cwd.clone() } else { other.cwd.clone() }
    }

    /// Alt+F5 (Total Commander's key): the marked files (or the one under the cursor) into a
    /// new zip, in the other panel's folder by default.
    fn start_pack(&mut self) {
        if self.refuse_in_archive(self.active) || self.refuse_over_ssh("Alt+F5 packs files on this computer") {
            return;
        }
        let sources = self.active_panel().selection();
        let name = match sources.as_slice() {
            [] => return,
            [one] if one.is_dir() => one.file_name(),
            [one] => one.file_stem(),
            _ => self.active_panel().cwd.file_name(),
        };
        let name = name.map_or_else(|| "archive".to_string(), |n| n.to_string_lossy().into_owned());
        let default = self.folder_here().join(format!("{name}.zip"));
        let prompt = format!("Pack {} into:", describe(&sources));
        self.start_input("Pack", &prompt, &default.display().to_string(), InputAction::Pack(sources));
    }

    /// Alt+F9 (Total Commander's key): the zip under the cursor, unpacked into the other
    /// panel's folder; one with several things at its top into a folder of its own name, as
    /// 7-Zip's "Extract to name\" does.
    fn start_unpack(&mut self) {
        if self.refuse_in_archive(self.active) || self.refuse_over_ssh("Alt+F9 unpacks zips on this computer") {
            return;
        }
        let Some(entry) = self.active_panel().current().filter(|e| !e.is_dir && archive::is_archive(&e.path)).cloned() else {
            self.notice = Some("Alt+F9 unpacks the zip under the cursor".into());
            return;
        };
        let zip = match Archive::open(&entry.path) {
            Ok(zip) => zip,
            Err(e) => return self.error(format!("Cannot open \"{}\":\n{e}", entry.path.display())),
        };
        self.notice = zip.left_out_note();
        let items: Vec<String> = zip.children("").map(|item| item.path.clone()).collect();
        let other = self.folder_here();
        let stem = entry.path.file_stem().map_or_else(|| "unpacked".into(), |s| s.to_string_lossy().into_owned());
        let dest = if items.len() > 1 { other.join(stem) } else { other };
        let prompt = format!("Unpack \"{}\" to:", entry.name);
        let default = util::with_trailing_separator(&dest);
        self.start_input("Unpack", &prompt, &default, InputAction::Extract { archive: entry.path, items });
    }

    fn on_input(&mut self, action: InputAction, text: String) {
        if action.keeps_the_name(&text) {
            return;
        }
        // A password in an address is refused, and isn't kept either, as nothing that isn't
        // an address cm can read is: what looks like a path may hold one too.
        if ssh::parse_typed(&text).is_ok() {
            self.history.inputs.add(action.kind(), text.trim());
            self.history_changed = true;
        }
        let cwd = self.active_panel().cwd.clone();
        match action {
            InputAction::Copy(sources) => self.transfer(&sources, &text, false, false),
            InputAction::Move(sources) => self.transfer(&sources, &text, true, false),
            InputAction::CopyHere(source) => self.transfer(&[source], &text, false, true),
            InputAction::MoveHere(source) => self.transfer(&[source], &text, true, true),
            InputAction::Rename(from) => self.rename(&from, &text),
            InputAction::Mkdir => self.mkdir(&cwd, &text),
            InputAction::NewFile => self.new_file(&cwd, &text),
            InputAction::Select | InputAction::Unselect => {
                let (mark, folders) = (matches!(action, InputAction::Select), self.config.settings.mark_folders);
                self.active_panel_mut().mark_matching(&text, mark, folders);
            }
            InputAction::Connect => {
                if text.trim().is_empty() {
                    return;
                }
                match ssh::parse_connect(&text) {
                    Ok((target, path)) => self.change_dir(self.active, PathBuf::from(target.place(&path))),
                    Err(e) => self.error(e),
                }
            }
            InputAction::Extract { archive, items } => {
                if text.trim().is_empty() {
                    return;
                }
                if ssh::parse(&text).is_some() {
                    return self.error("Files come out of an archive on this computer: copy them to the other one from here.");
                }
                let dest = util::as_windows_makes_it(util::resolve(&cwd, &text));
                self.start_job(Job::Extract { archive, items, dest });
            }
            InputAction::Pack(sources) => {
                if text.trim().is_empty() {
                    return;
                }
                if ssh::parse(&text).is_some() {
                    return self.error("A zip is made on this computer: copy it to the other one from here.");
                }
                let mut dest = util::as_windows_makes_it(util::resolve(&cwd, &text));
                // A folder, also one typed with a slash at the end that isn't there yet.
                if dest.is_dir() || util::strip_quotes(text.trim()).ends_with(['/', '\\']) {
                    let name = sources[0].file_name().map_or_else(|| "archive".into(), |n| n.to_string_lossy().into_owned());
                    dest = dest.join(format!("{name}.zip"));
                } else if dest.extension().is_none() {
                    dest.set_extension("zip");
                }
                if let Some(inside) = sources.iter().find(|s| s.is_dir() && (util::is_inside(&dest, s) || util::really_inside(&dest, s))) {
                    return self.error(format!("\"{}\" can't hold a zip of itself.", inside.display()));
                }
                if dest.exists() {
                    let name = dest.file_name().map_or_else(|| dest.display().to_string(), |n| n.to_string_lossy().into_owned());
                    self.dialog = Some(Dialog::Confirm {
                        title: "Pack".into(),
                        lines: vec![format!("{name} is there already."), "Replace it?".into()],
                        focus: 1,
                        danger: false,
                        action: ConfirmAction::Pack(sources, dest),
                    });
                    return;
                }
                self.start_job(Job::Pack { sources, dest });
            }
            InputAction::Filter => {
                let text = text.trim();
                let panel = self.active_panel_mut();
                panel.filter = (!text.is_empty() && text != "*").then(|| text.to_string());
                panel.reload();
                if let Some(filter) = &panel.filter {
                    self.notice = Some(format!("Only files matching {filter} are shown (Ctrl+F changes it)"));
                }
            }
        }
    }

    /// F5, F6 and their Shift forms once the destination is typed; nothing typed, nothing done.
    /// `keep_marks`: the job leaves the marks alone, as they aren't what it worked on.
    fn transfer(&mut self, sources: &[PathBuf], text: &str, moving: bool, keep_marks: bool) {
        if text.trim().is_empty() {
            return;
        }
        let typed = match ssh::parse_typed(text) {
            Ok(typed) => typed,
            Err(e) => return self.error(e),
        };
        // `a:b` that is there is that name here, as in scp a `./` would make it.
        let panel = self.active_panel();
        let typed = typed.filter(|_| panel.remote.is_some() || lfs::symlink_metadata(&util::resolve(&panel.cwd, text)).is_err());
        if typed.is_some() || panel.remote.is_some() {
            return self.transfer_over_ssh(sources, text, typed, moving, keep_marks);
        }
        let cwd = &self.active_panel().cwd;
        let planned = if moving { ops::plan_move(sources, text, cwd) } else { ops::plan(sources, text, cwd) };
        let pairs = match planned {
            Ok(pairs) => pairs,
            Err(e) => return self.error(e),
        };
        // One item under another name in its own folder: the cursor goes along.
        if let [(from, to)] = pairs.as_slice()
            && from.parent().zip(to.parent()).is_some_and(|(a, b)| util::same_path(a, b))
        {
            self.select_after = Some(to.clone());
        }
        self.keep_marks = keep_marks;
        self.start_job(if moving { Job::Move(pairs) } else { Job::Copy(pairs) });
    }

    /// F5, F6 and their Shift forms with an end on another computer. An address goes there
    /// (connecting first if need be); on a panel showing another computer a name counts from
    /// the folder there; a whole path is on this computer.
    fn transfer_over_ssh(&mut self, sources: &[PathBuf], text: &str, typed: Option<(Target, String)>, moving: bool, keep_marks: bool) {
        let panel = self.active_panel();
        let view = panel.remote.clone();
        let dest = match typed {
            Some((target, path)) => {
                let Some(connection) = ssh::find(&target) else {
                    let then = AfterConnect::Transfer { sources: sources.to_vec(), text: text.to_string(), moving, keep_marks };
                    return self.connect(target, then);
                };
                let path = ssh::resolve(&connection.home, &connection.home, &path);
                remote::Place::There(connection, path)
            }
            // Shift+F5 and Shift+F6 stay on the panel's computer unless a drive says otherwise;
            // F5 and F6 start on the other panel, so a whole path there may be one of this one.
            None => match &view {
                Some(view) if if keep_marks { !has_drive(text) } else { !is_whole_path(text) } => {
                    remote::Place::There(view.connection.clone(), view.resolve(text))
                }
                _ => remote::Place::Here(util::as_windows_makes_it(util::resolve(&panel.cwd, text))),
            },
        };
        let from: Vec<remote::Place> = sources
            .iter()
            .map(|source| match (&view, ssh::parse_exact(&source.to_string_lossy())) {
                (Some(view), Some((_, path))) => remote::Place::There(view.connection.clone(), path),
                _ => remote::Place::Here(source.clone()),
            })
            .collect();
        let as_folder = util::strip_quotes(text.trim()).ends_with(['/', '\\']);
        let pairs = match remote::plan(&from, &dest, as_folder) {
            Ok(pairs) => pairs,
            Err(e) => return self.error(e),
        };
        if let [(from, to)] = pairs.as_slice()
            && from.is_beside(to)
        {
            self.select_after = Some(to.shown());
        }
        self.keep_marks = keep_marks;
        self.start_job(Job::Transfer { pairs, moving, replace: false });
    }

    /// Starts connecting to `target` behind the panels; `then` follows when that is done.
    fn connect(&mut self, target: Target, then: AfterConnect) {
        let (tx, answer) = mpsc::channel();
        let who = target.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Connection::open(&who, Login::Quiet, || {}));
        });
        self.connecting = Some(Connecting { target, answer, then });
    }

    /// The answer of a connection being made. If `ssh` wants to ask something, it gets the
    /// terminal. Returns whether anything arrived.
    fn tick_connecting(&mut self) -> bool {
        let Some(connecting) = &self.connecting else { return false };
        let answer = match connecting.answer.try_recv() {
            Ok(answer) => answer,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err(Failure::Said("the attempt ended without a word".into())),
        };
        let Some(Connecting { target, then, .. }) = self.connecting.take() else { return false };
        match answer {
            Ok(connection) => {
                ssh::keep(connection);
                self.connected(then);
            }
            Err(Failure::NeedsTheUser) => {
                self.login_then = Some(then);
                self.external = Some(External::SshLogin(target));
            }
            Err(Failure::Said(why)) => self.error(format!("Cannot connect to {target}:\n{why}")),
        }
        true
    }

    fn connected(&mut self, then: AfterConnect) {
        match then {
            AfterConnect::Go { side, place } => self.change_dir(side, place),
            AfterConnect::Transfer { sources, text, moving, keep_marks } => self.transfer(&sources, &text, moving, keep_marks),
        }
    }

    /// The rest of the prompt of + and -, by the option (and a slash at the end for folders).
    /// Short enough for an 80-column terminal, where the field is 49 wide.
    fn what_patterns_mark(&self) -> &'static str {
        if self.config.settings.mark_folders {
            "files and folders (*/ folders only):"
        } else {
            "files matching (*/ for folders):"
        }
    }

    fn start_input(&mut self, title: &str, prompt: &str, text: &str, action: InputAction) {
        self.open_input(title, prompt, LineEdit::new(text), action);
    }

    /// For a name: it is all selected, so typing replaces it and the arrows edit it.
    fn start_name_input(&mut self, title: &str, prompt: &str, name: &str, action: InputAction) {
        self.open_input(title, prompt, LineEdit::selected(name), action);
    }

    fn open_input(&mut self, title: &str, prompt: &str, edit: LineEdit, action: InputAction) {
        let recall = Recall::new(self.history.inputs.list(action.kind()));
        self.dialog = Some(Dialog::Input { title: title.into(), prompt: prompt.into(), edit, action, recall });
    }

    fn start_job(&mut self, job: Job) {
        self.prepare_for_job(&job.takes_away());
        self.job = Some(ops::start(job));
    }

    /// Before any job: it works for the active panel. An open watch keeps Windows from moving
    /// a folder above it (the other panel inside what is moved), and so does a listing under
    /// way in what goes (`going`). A job reloads the panels at its end, so none is needed
    /// until then.
    fn prepare_for_job(&mut self, going: &[&Path]) {
        self.job_side = self.active;
        self.watches = [None, None];
        self.git.close_watches();
        for panel in self.panels.iter_mut().chain(self.tabs.iter_mut().flatten()) {
            panel.let_go_of(going);
        }
    }

    /// Picks up directory sizes, drive space, the end of the shell, and the progress, questions
    /// and end of a running job. Returns whether any of that arrived, so the screen is redrawn:
    /// work can finish between two looks, so "was it running before or after" isn't enough.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        // The shell was closed with `exit`: back to the panels; the next Ctrl+O starts a new one.
        if self.subshell.as_mut().is_some_and(Subshell::has_exited) {
            if self.shell_shown {
                self.hide_shell();
            }
            self.subshell = None;
            changed = true;
        }
        if let Some(answers) = &self.drive_space {
            while let Ok((root, space)) = answers.try_recv() {
                changed = true;
                if let Some(Dialog::Drives { drives, .. }) = &mut self.dialog
                    && let Some(drive) = drives.iter_mut().find(|d| d.root == root)
                {
                    drive.space = space;
                    drive.pending = false;
                }
            }
        }
        if let Some(sizer) = &self.sizer {
            while let Ok(update) = sizer.updates.try_recv() {
                changed = true;
                for panel in &mut self.panels {
                    panel.set_dir_size(&update.dir, update.bytes, update.done);
                }
            }
            // Nobody is waiting for the result any more (the panel moved on): stop measuring.
            if sizer.is_busy() && !self.panels.iter().any(Panel::has_pending_sizes) {
                sizer.cancel();
            }
        }
        if let Some(finder) = &mut self.finder {
            changed |= finder.tick();
        }
        changed |= self.tick_connecting();
        changed |= self.take_branches(false);
        changed |= self.tick_switching();
        changed |= self.tick_readings();
        // Not over a dialog: it may say where a file went. It comes when that one is read.
        for side in 0..2 {
            if self.dialog.is_none()
                && let Some(why) = self.panels[side].lost.take()
            {
                changed = true;
                self.error(why);
            }
        }
        changed |= self.follow_changes();
        changed |= self.tick_git();
        if self.quick_view {
            let key = self.preview_key();
            if let Some(previewer) = &mut self.previewer {
                previewer.show(key);
                changed |= previewer.tick();
            }
        }
        // Now and then rather than only on exit: closing the window ends cm without one.
        if self.history_changed && self.history_saved.elapsed() > std::time::Duration::from_secs(5) {
            self.save_history();
        }
        self.tick_job() || changed
    }

    /// What the panels had read, the hidden tabs' too, and the keys that waited for it.
    /// Returns whether the screen should change.
    fn tick_readings(&mut self) -> bool {
        let mut changed = false;
        for panel in self.panels.iter_mut().chain(self.tabs.iter_mut().flatten()) {
            changed |= panel.poll();
        }
        for side in 0..2 {
            // Bad news that comes by itself doesn't take the keys of someone typing: a box
            // would swallow them, and Enter under it would run what is left of the line.
            let bad_news = matches!(self.panels[side].came, Some(Came::Failed(_) | Came::NoFurther { .. }));
            if !bad_news || !self.is_typing() {
                self.settle(side);
            }
        }
        while !self.active_panel().is_going() && self.external.is_none() && !self.quit {
            let Some(key) = self.typed_ahead.pop_front() else { break };
            self.on_key(key);
            changed = true;
        }
        // An editor has the terminal now, and these were never meant for it.
        if self.external.is_some() {
            self.typed_ahead.clear();
        }
        changed
    }

    /// Whether the keys are going somewhere a box shouldn't come between: a question, a field,
    /// the command line, a search.
    fn is_typing(&self) -> bool {
        self.dialog.is_some() || self.job.is_some() || self.find_shown || self.quicksearch.is_some() || !self.cmdline.is_empty()
    }

    /// How going somewhere ended, once it has.
    fn settle(&mut self, side: usize) {
        match self.panels[side].came.take() {
            Some(Came::There) => self.visited(side),
            Some(Came::Zip) => self.notice = self.panels[side].archive.as_ref().and_then(|view| view.archive.left_out_note()),
            Some(Came::Failed(why)) => {
                self.typed_ahead.clear();
                self.error(why);
            }
            Some(Came::Stayed) => self.typed_ahead.clear(),
            Some(Came::NoFurther { back }) => {
                self.typed_ahead.clear();
                self.notice = Some(if back { "No folder to go back to" } else { "No folder to go forward to" }.into());
            }
            None => {}
        }
    }

    /// The running job's progress, questions and end. Returns whether any of them arrived.
    fn tick_job(&mut self) -> bool {
        let Some(job) = self.job.as_mut() else { return false };
        let mut finished = None;
        let mut changed = false;
        loop {
            let msg = match job.rx.try_recv() {
                Ok(msg) => msg,
                Err(TryRecvError::Empty) => break,
                // The worker is gone without a word (it panicked): don't wait for it forever.
                Err(TryRecvError::Disconnected) => {
                    let log = util::panic_log();
                    let error = format!("The job stopped unexpectedly (an internal error, see {}).", log.display());
                    finished = Some((vec![error], true, None));
                    break;
                }
            };
            changed = true;
            match msg {
                Msg::Progress(progress) => job.update(progress),
                Msg::Ask(question, reply) => {
                    if matches!(question, Question::Overwrite { .. }) {
                        self.overwrite_asked = Some(Instant::now());
                    }
                    self.dialog = Some(question_dialog(question, reply));
                }
                Msg::Done { errors, cancelled, as_admin } => {
                    finished = Some((errors, cancelled, as_admin));
                    break;
                }
            }
        }
        if job.stuck() && !job.stuck_shown {
            job.stuck_shown = true;
            changed = true;
        }
        let Some((errors, cancelled, as_admin)) = finished else { return changed };
        // "As admin": the rest goes to the elevated helper, shown as the same kind of job.
        if let Some(paths) = as_admin {
            self.job = Some(admin::start(paths, errors));
            return true;
        }
        self.job = None;
        // Cut files pasted: the clipboard no longer says where they are (Explorer empties it
        // too). Not if some stayed (answered No), or something else was copied meanwhile.
        if let Some(pasted) = self.pasting_cut.take()
            && errors.is_empty()
            && !cancelled
            && pasted.iter().all(|path| crate::lfs::symlink_metadata(path).is_err())
            && self.clipboard.files().is_some_and(|files| files.paths == pasted)
        {
            self.clipboard.moved();
        }
        // An edited file is back on its computer, or isn't.
        let put_back = self.putting_back.take();
        // A file taken out of an archive: now it can be opened. That changes nothing in the
        // panels, so what is marked and measured there stays.
        let taken_out = self.after_job.take();
        let touched_files = taken_out.is_none();
        if let Some((then, path)) = taken_out
            && errors.is_empty()
            && !cancelled
        {
            self.after_taking_out(then, path);
        }
        let job_side = self.job_side;
        for (side, panel) in self.panels.iter_mut().enumerate() {
            // Done: the marks of the panel it started from have served (the other side's are
            // for another job, as after Shift+F2). Stopped or partly failed: they stay, to try
            // again.
            if touched_files && side == job_side && errors.is_empty() && !cancelled && !self.keep_marks {
                panel.clear_marks();
            }
            // Sizes may be out of date after copying, moving or deleting.
            if touched_files {
                panel.dir_sizes.clear();
            }
            panel.reload();
        }
        self.keep_marks = false;
        // One item renamed or copied beside itself: the cursor goes along.
        if let Some(target) = self.select_after.take() {
            self.select_in_active(&target);
        }
        self.git.refresh();
        let kept_at = put_back.and_then(|(copy, rescued)| {
            if errors.is_empty() && !cancelled {
                let _ = rescued.map(fs::remove_file);
                let _ = copy.parent().map(fs::remove_dir_all);
                None
            } else {
                rescued.or_else(|| rescue(&copy)).or(Some(copy))
            }
        });
        if let Some(kept_at) = &kept_at
            && errors.is_empty()
        {
            self.notice = Some(format!("Stopped: your changes are in {}", kept_at.display()));
        }
        // Esc just stops the job: no "cancelled" box to dismiss as well. Failures from before
        // the stop are still worth knowing about.
        if !errors.is_empty() {
            let mut lines = vec![format!("{} item(s) were not processed:", errors.len())];
            // One line each: the reason, without the advice that may follow it.
            lines.extend(errors.iter().take(8).map(|e| e.lines().next().unwrap_or_default().to_string()));
            if errors.len() > 8 {
                lines.push(format!("… and {} more", errors.len() - 8));
            }
            if let Some(kept_at) = kept_at {
                lines.push(format!("Your changes are in {}", kept_at.display()));
            }
            let title = if cancelled { "Stopped" } else { "Problems" };
            self.dialog = Some(Dialog::Message { title: title.into(), lines, error: true });
        }
        true
    }

    /// Called after an editor, viewer, shell or command has run. `shell_cwd` is where a Ctrl+O
    /// shell was last; the active panel follows it, as mc's panels follow its subshell.
    pub fn after_external(&mut self, result: Result<(), String>, shell_cwd: Option<PathBuf>) {
        self.reload_panels();
        if let Some(dir) = shell_cwd
            && !util::same_path(&dir, &self.active_panel().cwd)
        {
            self.follow_shell(dir);
        }
        // `ssh` has logged in, or hasn't.
        if let Some(then) = self.login_then.take()
            && result.is_ok()
        {
            self.connected(then);
        }
        // A file of another computer: it goes back if it changed.
        if let Some(copy) = self.edited_copy.take() {
            self.after_editing(copy);
        }
        // F4 in the scheme list: the file edited counts at once (one left as it was goes).
        self.reload_schemes();
        // A file taken out of an archive to look at: looked at. A viewer back at once went on
        // by itself (gvim, code) and still needs the copy, which then goes when cm ends.
        if let Some((copy, since)) = self.viewed_copy.take()
            && since.elapsed() >= VIEWER_STAYS
        {
            let _ = fs::remove_dir_all(copy);
        }
        if let Err(e) = result {
            // The error takes the scheme list's place, so the scheme in use is back too.
            if matches!(self.dialog, Some(Dialog::Schemes { .. })) {
                self.scheme = self.saved_scheme();
            }
            self.error(e);
        }
    }

    /// The editor closed on a file of another computer. Changed, it goes back: over there it
    /// may have changed too, and that isn't simply replaced.
    fn after_editing(&mut self, mut copy: EditedCopy) {
        let now = fs::metadata(&copy.local).ok();
        let changed = now.is_some_and(|meta| meta.len() != copy.size || meta.modified().ok() != copy.modified);
        if !changed {
            // An editor back at once went on by itself (gvim, code), and what it saves stays here.
            if copy.since.elapsed() < VIEWER_STAYS {
                self.notice = Some(format!("The editor went on by itself: cm can't bring back what it saves in {}, which goes when cm ends", copy.local.display()));
            } else {
                let _ = copy.local.parent().map(fs::remove_dir_all);
            }
            return;
        }
        let target = copy.connection.target.to_string();
        let name = ssh::name_of(&copy.path).to_string();
        let there = copy.connection.sftp.stat(&copy.path);
        match there {
            // Not by the last access: fetching the file was one.
            Ok(attrs) if attrs.size == copy.there.size && attrs.modified() == copy.there.modified() => self.put_back(copy),
            Ok(_) => {
                copy.rescued = rescue(&copy.local);
                let kept = copy.rescued.as_ref().map_or_else(String::new, |path| format!("If not, yours stays in {}", path.display()));
                self.dialog = Some(Dialog::Confirm {
                    title: "Changed meanwhile".into(),
                    lines: vec![format!("{name} changed on {target} while you edited it."), "Replace it with yours?".into(), kept],
                    focus: 1,
                    danger: true,
                    action: ConfirmAction::PutBack(Box::new(copy)),
                });
            }
            // It is gone, or the connection is: nothing replaces anything yet.
            Err(e) => {
                let kept = rescue(&copy.local).unwrap_or_else(|| copy.local.clone());
                self.error(format!("Cannot save {name} on {target}:\n{e}\nYour changes are in {}", kept.display()));
            }
        }
    }

    fn put_back(&mut self, copy: EditedCopy) {
        let pairs = vec![(remote::Place::Here(copy.local.clone()), remote::Place::There(copy.connection, copy.path))];
        self.putting_back = Some((copy.local, copy.rescued));
        self.keep_marks = true;
        self.start_job(Job::Transfer { pairs, moving: false, replace: true });
    }

    fn change_dir(&mut self, side: usize, dir: PathBuf) {
        // Another computer with no connection yet: that comes first.
        if let Some((target, _)) = ssh::parse_exact(&dir.to_string_lossy())
            && ssh::find(&target).is_none()
        {
            return self.connect(target, AfterConnect::Go { side, place: dir });
        }
        self.go_to_first(side, vec![dir]);
    }

    /// The panel goes to the first of `places` that can be read.
    fn go_to_first(&mut self, side: usize, places: Vec<PathBuf>) {
        self.panels[side].go_to_first(&places, false);
        self.settle(side);
    }

    /// The panel arrived in a folder: it goes to the top of the folder history (Alt+H).
    fn visited(&mut self, side: usize) {
        let cwd = self.panels[side].place();
        self.history.visit(&cwd);
        self.history_changed = true;
    }

    /// Alt+Left / Alt+Right (and mc's Alt+Y / Alt+U): back and forward through the folders
    /// the active panel has been in.
    fn step_back(&mut self, back: bool) {
        let panel = &mut self.panels[self.active];
        if back { panel.go_back() } else { panel.go_forward() }
        self.settle(self.active);
    }

    /// Ctrl+C / Ctrl+X: the marked files (or the one under the cursor) to the clipboard, as
    /// Explorer puts them there, so Explorer or a mail can paste them; their paths go along as
    /// text. Elsewhere than Windows the paths go to the terminal's clipboard (OSC 52).
    fn copy_to_clipboard(&mut self, effect: Effect) {
        if self.refuse_in_archive(self.active) || self.refuse_over_ssh("The clipboard holds files of this computer") {
            return;
        }
        let paths = self.active_panel().selection();
        if paths.is_empty() {
            return;
        }
        match self.clipboard.put(&paths, effect) {
            Ok(text) => {
                self.terminal_clipboard = text;
                let what = describe(&paths);
                self.notice = Some(match effect {
                    // Explorer would read `foo.` as `foo`, and act on that file instead.
                    _ if paths.iter().any(|path| crate::lfs::needs_literal(path)) => {
                        format!("{what} on the clipboard: paste with Alt+V only, other programs would take another file for it")
                    }
                    Effect::Copy => format!("{what} copied to the clipboard (Alt+V pastes here)"),
                    Effect::Move => format!("{what} cut to the clipboard (Alt+V moves it here)"),
                });
            }
            Err(e) => self.error(e),
        }
    }

    /// Ctrl+Shift+C: the full paths of the marked files (or the one under the cursor) to the
    /// clipboard as text.
    fn copy_paths(&mut self) {
        if self.refuse_in_archive(self.active) {
            return;
        }
        let paths = self.active_panel().selection();
        if paths.is_empty() {
            return;
        }
        match self.clipboard.put_text(&paths) {
            Ok(text) => {
                self.terminal_clipboard = text;
                self.notice = Some(match paths.as_slice() {
                    [one] => format!("{} copied to the clipboard as text", one.display()),
                    many => format!("{} paths copied to the clipboard as text", many.len()),
                });
            }
            Err(e) => self.error(e),
        }
    }

    /// Alt+V (or Ctrl+V where the terminal lets it through): the files on the clipboard into
    /// the active panel's folder, copied, or moved if they were cut. A copy into the folder it
    /// came from gets Explorer's "name - Copy" name.
    fn paste(&mut self) {
        if self.refuse_in_archive(self.active) || self.refuse_over_ssh("The clipboard pastes on this computer") {
            return;
        }
        let Some(files) = self.clipboard.files() else {
            self.notice = Some("No files on the clipboard (Ctrl+C copies some, here or in Explorer)".into());
            return;
        };
        let dir = self.active_panel().cwd.clone();
        let mut pairs = Vec::new();
        for source in &files.paths {
            // This folder, by any of its names (a junction, a mapped drive and its share).
            let here = source.parent().is_some_and(|parent| util::same_path(parent, &dir) || util::same_file(parent, &dir));
            if here && files.effect == Effect::Move {
                continue;
            }
            let Some(target) = clipboard::paste_target(source, &dir) else { continue };
            // The checks F5 and F6 make: a link to a folder doesn't get a folder into itself.
            if let Err(e) = ops::check_pair(source, &target) {
                return self.error(e);
            }
            pairs.push((source.clone(), target));
        }
        if pairs.is_empty() {
            return;
        }
        if files.effect == Effect::Move {
            self.pasting_cut = Some(files.paths);
            self.start_job(Job::Move(pairs));
        } else {
            self.start_job(Job::Copy(pairs));
        }
    }

    /// Alt+A: the attributes (Windows) or permissions (elsewhere) of the marked files, or the
    /// one under the cursor. Where they differ, a row shows [?] and is left as each has it.
    fn open_attributes(&mut self) {
        if self.refuse_in_archive(self.active) || self.refuse_over_ssh("Alt+A works on this computer") {
            return;
        }
        let paths = self.active_panel().selection();
        if paths.is_empty() {
            self.notice = Some("Alt+A works on the file under the cursor, or the marked ones".into());
            return;
        }
        let values: Vec<u32> = match paths.iter().map(|p| platform::attributes(p)).collect::<std::io::Result<_>>() {
            Ok(values) => values,
            Err(e) => return self.error(format!("Cannot read the attributes:\n{e}")),
        };
        let states = platform::ATTRIBUTES
            .iter()
            .map(|(_, bit)| match values.iter().filter(|v| *v & bit != 0).count() {
                0 => Some(false),
                n if n == values.len() => Some(true),
                _ => None,
            })
            .collect();
        self.dialog = Some(Dialog::Attributes { paths, states, cursor: 0 });
    }

    fn on_attributes_key(&mut self, key: &KeyEvent, paths: Vec<PathBuf>, mut states: Vec<Option<bool>>, mut cursor: usize) -> Option<Dialog> {
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => return None,
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(states.len().saturating_sub(1)),
            KeyCode::Char(' ') | KeyCode::Insert => toggle_state(&mut states, cursor),
            KeyCode::Enter => {
                self.apply_attributes(&paths, &states);
                return None;
            }
            _ => {}
        }
        Some(Dialog::Attributes { paths, states, cursor })
    }

    /// Sets what the attributes dialog says, file by file (rows left at [?] untouched).
    fn apply_attributes(&mut self, paths: &[PathBuf], states: &[Option<bool>]) {
        let mut errors = Vec::new();
        for path in paths {
            let result = platform::attributes(path).and_then(|old| {
                let mut new = old;
                for ((_, bit), state) in platform::ATTRIBUTES.iter().zip(states) {
                    match state {
                        Some(true) => new |= bit,
                        Some(false) => new &= !bit,
                        None => {}
                    }
                }
                if new == old { Ok(()) } else { platform::set_attributes(path, new) }
            });
            if let Err(e) = result {
                errors.push(format!("{}: {e}", path.display()));
            }
        }
        self.reload_panels();
        if !errors.is_empty() {
            self.error(format!("Some attributes could not be set:\n{}", errors.join("\n")));
        }
    }

    /// F12: the command list. Typing narrows it, Enter runs the command (by pressing its key).
    fn on_commands_key(&mut self, key: &KeyEvent, mut filter: LineEdit, mut cursor: usize) -> Option<Dialog> {
        let rows = dialog::matching_commands(filter.text());
        let last = rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::F(10) | KeyCode::F(12) => return None,
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(last),
            KeyCode::PageUp => cursor = cursor.saturating_sub(10),
            KeyCode::PageDown => cursor = (cursor + 10).min(last),
            KeyCode::Enter => {
                if let Some(command) = rows.get(cursor) {
                    self.on_key(KeyEvent::new(command.code, command.modifiers));
                }
                return None;
            }
            _ => {
                let before = filter.text().to_string();
                filter.handle_key(key);
                if filter.text() != before {
                    cursor = 0;
                }
            }
        }
        Some(Dialog::Commands { filter, cursor })
    }

    /// Shift+F2 (Total Commander's key): marks in each panel what the other one lacks or has
    /// an older copy of, so F5 then brings the other side up to date.
    fn compare_dirs(&mut self) {
        // By what they show: a zip and the folder it sits in are two places to compare.
        if util::same_path(&self.panels[0].location(), &self.panels[1].location()) {
            self.notice = Some("Both panels show the same folder".into());
            return;
        }
        let (left, right) = compare::compare(&self.panels[0].entries, &self.panels[1].entries);
        let counts = (left.len(), right.len());
        for (panel, names) in self.panels.iter_mut().zip([left, right]) {
            panel.clear_marks();
            panel.marked.extend(names);
        }
        self.notice = Some(match counts {
            (0, 0) => "The folders hold the same files".into(),
            (l, r) => format!("Marked what the other side lacks or has older: {l} left, {r} right"),
        });
    }

    /// Alt+H (or Alt+Down, as in Total Commander): the folders visited lately, newest first,
    /// with the current one left out; typing narrows the list.
    fn open_dir_history(&mut self) {
        self.dialog = Some(Dialog::History { filter: LineEdit::default(), cursor: 0 });
    }

    /// The folder history's rows for `filter`, without the folder the panel is in. (Whether
    /// each still exists isn't checked: a network drive could make that slow.)
    pub fn history_rows(&self, filter: &str) -> Vec<PathBuf> {
        let cwd = self.active_panel().place();
        self.history.matching(filter).into_iter().filter(|dir| !util::same_path(dir, &cwd)).cloned().collect()
    }

    fn on_history_key(&mut self, key: &KeyEvent, mut filter: LineEdit, mut cursor: usize) -> Option<Dialog> {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let rows = self.history_rows(filter.text());
        let last = rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => return None,
            KeyCode::Char('h') if alt => return None,
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(last),
            KeyCode::PageUp => cursor = cursor.saturating_sub(10),
            KeyCode::PageDown => cursor = (cursor + 10).min(last),
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => cursor = 0,
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => cursor = last,
            KeyCode::Enter => {
                if let Some(dir) = rows.get(cursor).cloned() {
                    self.change_dir(self.active, dir);
                }
                return None;
            }
            // Forget a folder.
            KeyCode::Delete => {
                if let Some(dir) = rows.get(cursor) {
                    self.history.forget(dir);
                    self.history_changed = true;
                    cursor = cursor.min(rows.len().saturating_sub(2));
                }
            }
            _ => {
                let before = filter.text().to_string();
                filter.handle_key(key);
                if filter.text() != before {
                    cursor = 0;
                }
            }
        }
        Some(Dialog::History { filter, cursor })
    }

    /// Alt+W: the worktrees of the repository the panel is in, to go to another one. The
    /// cursor starts on the first one that isn't this one.
    fn open_worktrees(&mut self) {
        let Some(repo) = self.active_panel().repo.as_ref() else {
            self.notice = Some("Not in a git repository (Alt+W lists its worktrees)".into());
            return;
        };
        let worktrees = repo.worktrees();
        let here = repo.root.clone();
        let cursor = worktrees.iter().position(|w| !util::same_path(&w.path, &here)).unwrap_or(0);
        self.dialog = Some(Dialog::Worktrees { filter: LineEdit::default(), cursor, worktrees, here });
    }

    fn on_worktrees_key(
        &mut self,
        key: &KeyEvent,
        mut filter: LineEdit,
        mut cursor: usize,
        worktrees: Vec<Worktree>,
        here: PathBuf,
    ) -> Option<Dialog> {
        let rows = dialog::worktree_rows(&worktrees, filter.text());
        let last = rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => return None,
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::ALT) => return None,
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(last),
            KeyCode::PageUp => cursor = cursor.saturating_sub(10),
            KeyCode::PageDown => cursor = (cursor + 10).min(last),
            KeyCode::Enter => {
                if let Some(worktree) = rows.get(cursor) {
                    let worktree = (*worktree).clone();
                    self.go_to_worktree(&worktree, &here);
                }
                return None;
            }
            _ => {
                let before = filter.text().to_string();
                filter.handle_key(key);
                if filter.text() != before {
                    cursor = 0;
                }
            }
        }
        Some(Dialog::Worktrees { filter, cursor, worktrees, here })
    }

    /// Alt+B: the branches of the repository the panel is in, newest first, to find one and
    /// switch to it. git lists them on a thread of its own; a quick list is there at once.
    fn open_branches(&mut self) {
        let Some(repo) = self.active_panel().repo.as_ref() else {
            self.notice = Some("Not in a git repository (Alt+B lists its branches)".into());
            return;
        };
        let here = repo.root.clone();
        let coming = Some(git::branches(&here));
        self.dialog = Some(Dialog::Branches { filter: LineEdit::default(), cursor: 0, branches: Vec::new(), here, coming });
        self.take_branches(true);
    }

    /// The branches git has listed for Alt+B, if they are in; with `wait`, after a moment's
    /// wait for them. The cursor starts on the newest one that isn't this worktree's. Returns
    /// whether anything came.
    fn take_branches(&mut self, wait: bool) -> bool {
        let Some(Dialog::Branches { coming: Some(coming), .. }) = &self.dialog else { return false };
        let answer = if wait {
            match coming.recv_timeout(crate::reading::a_moment()) {
                Ok(answer) => answer,
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
                Err(mpsc::RecvTimeoutError::Disconnected) => Err("git ended without a word".into()),
            }
        } else {
            match coming.try_recv() {
                Ok(answer) => answer,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => Err("git ended without a word".into()),
            }
        };
        match answer {
            Ok(list) => {
                if let Some(Dialog::Branches { filter, cursor, branches, here, coming }) = &mut self.dialog {
                    if filter.text().is_empty() {
                        let elsewhere = |b: &Branch| !b.worktree.as_ref().is_some_and(|tree| util::same_path(tree, here));
                        *cursor = list.iter().position(elsewhere).unwrap_or(0);
                    }
                    (*branches, *coming) = (list, None);
                }
            }
            Err(why) => {
                self.dialog = None;
                self.error(format!("Cannot list the branches:\n{why}"));
            }
        }
        true
    }

    fn on_branches_key(
        &mut self,
        key: &KeyEvent,
        mut filter: LineEdit,
        mut cursor: usize,
        branches: Vec<Branch>,
        here: PathBuf,
        coming: Option<Receiver<Result<Vec<Branch>, String>>>,
    ) -> Option<Dialog> {
        let rows = dialog::branch_rows(&branches, filter.text());
        let last = rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => return None,
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => return None,
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => cursor = (cursor + 1).min(last),
            KeyCode::PageUp => cursor = cursor.saturating_sub(10),
            KeyCode::PageDown => cursor = (cursor + 10).min(last),
            KeyCode::Enter => {
                if let Some(branch) = rows.get(cursor) {
                    let branch = (*branch).clone();
                    self.choose_branch(branch, &here);
                }
                return None;
            }
            _ => {
                let before = filter.text().to_string();
                filter.handle_key(key);
                if filter.text() != before {
                    cursor = 0;
                }
            }
        }
        Some(Dialog::Branches { filter, cursor, branches, here, coming })
    }

    /// Enter on a branch: to the worktree that has it checked out, or this worktree switched to
    /// it once the user says so.
    fn choose_branch(&mut self, branch: Branch, here: &Path) {
        match branch.worktree.clone() {
            Some(tree) if util::same_path(&tree, here) => {
                self.notice = Some(format!("This worktree is on {} already", branch.name));
            }
            Some(tree) => self.go_into_worktree(&tree, here),
            None => {
                let mut lines = vec![format!("Switch this worktree to {}?", branch.name), here.display().to_string()];
                if let Some((_, short)) = branch.name.split_once('/').filter(|_| branch.remote) {
                    lines.push(format!("A branch {short} that follows it is made here."));
                }
                lines.push("Changes not committed go along; git refuses if they would be lost.".into());
                let action = ConfirmAction::Switch { root: here.to_path_buf(), branch };
                self.dialog = Some(Dialog::Confirm { title: "Switch branch".into(), lines, focus: 0, danger: false, action });
            }
        }
    }

    /// git switches the worktree at `root` to `branch`, with the watches closed so that none
    /// holds a folder it removes; keys wait meanwhile.
    fn switch_branch(&mut self, root: PathBuf, branch: Branch) {
        self.watches = [None, None];
        self.git.close_watches();
        let answer = git::switch(&root, &branch);
        self.switching = Some(Switching { branch: branch.name, answer });
    }

    /// The end of a switch: the panels read again, and what git said.
    fn tick_switching(&mut self) -> bool {
        let Some(switching) = &self.switching else { return false };
        let answer = match switching.answer.try_recv() {
            Ok(answer) => answer,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("git ended without a word".into()),
        };
        let Some(Switching { branch, .. }) = self.switching.take() else { return false };
        self.reload_panels();
        match answer {
            Ok(said) => self.notice = Some(said.lines().next().map_or_else(|| format!("Switched to {branch}"), String::from)),
            Err(why) => self.error(format!("Cannot switch to {branch}:\n{why}")),
        }
        true
    }

    /// Ctrl+E: the places of the git repository the panel is in, the menu on the one the panel
    /// is at. What is wrong in `places.ini` is said, and the rest works.
    fn open_places(&mut self) {
        let panel = self.active_panel();
        let Some(repo) = panel.repo.as_ref().filter(|_| panel.remote.is_none()) else {
            self.notice = Some("Not in a git repository (Ctrl+E lists the places you keep in one)".into());
            return;
        };
        let (root, id) = (repo.root.clone(), places::identity(repo));
        let (entries, problems) = places::File::load(self.config.places_file.clone()).entries(&id);
        let places = places::places(&root, &entries);
        let cursor = places::nearest(&places, &panel.cwd);
        self.notice = problems.first().map(|problem| format!("places.ini, {problem}"));
        self.dialog = Some(Dialog::Places { root, id, places, cursor });
    }

    /// The menu again after Ins or Del, the cursor on the place on `line` if it is there.
    fn places_again(&mut self, root: PathBuf, id: String, line: Option<usize>, cursor: usize) -> Option<Dialog> {
        let (entries, _) = places::File::load(self.config.places_file.clone()).entries(&id);
        let places = places::places(&root, &entries);
        let at = line.and_then(|line| places.iter().position(|p| p.line == Some(line)));
        let cursor = at.unwrap_or(cursor).min(places.len().saturating_sub(1));
        Some(Dialog::Places { root, id, places, cursor })
    }

    /// Keys in the menu of places: a place's key goes there at once, as do Enter and a click;
    /// Ins adds where the panel is, Del forgets the place under the cursor, F4 edits the file;
    /// Ctrl+E again closes it.
    fn on_places_key(&mut self, key: &KeyEvent, root: PathBuf, id: String, places: Vec<Place>, mut cursor: usize) -> Option<Dialog> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let last = places.len().saturating_sub(1);
        let chosen = match key.code {
            KeyCode::Esc | KeyCode::F(10) => return None,
            KeyCode::Char('e') if ctrl && !alt => return None,
            KeyCode::Insert => {
                let panel = self.active_panel();
                let file = panel.current().filter(|e| !e.is_dir && !e.is_parent).map(|e| e.name.clone());
                let Some((label, path)) = places::new_place(&root, &panel.cwd, file.as_deref()) else {
                    self.notice = Some("The repository's top is always there (r)".into());
                    return Some(Dialog::Places { root, id, places, cursor });
                };
                let mut file = places::File::load(self.config.places_file.clone());
                let taken: Vec<char> = places.iter().filter_map(|p| p.key).collect();
                let Some(key) = file.add(&id, &label, &path, &taken) else {
                    self.notice = Some("Every key is taken: F4 edits the places".into());
                    return Some(Dialog::Places { root, id, places, cursor });
                };
                if let Err(e) = file.save() {
                    self.error(format!("Cannot save the places:\n{e}"));
                    return None;
                }
                self.notice = Some(format!("{label} is place {key} in this repository"));
                let (entries, _) = file.entries(&id);
                let line = entries.iter().find(|e| e.key == key).map(|e| e.line);
                return self.places_again(root, id, line, cursor);
            }
            KeyCode::Delete => {
                let Some(line) = places.get(cursor).and_then(|p| p.line) else {
                    self.notice = Some("The repository's top is always there".into());
                    return Some(Dialog::Places { root, id, places, cursor });
                };
                let mut file = places::File::load(self.config.places_file.clone());
                file.remove(line);
                if let Err(e) = file.save() {
                    self.error(format!("Cannot save the places:\n{e}"));
                    return None;
                }
                return self.places_again(root, id, None, cursor);
            }
            KeyCode::F(4) => {
                match places::File::load(self.config.places_file.clone()).for_editing(&id) {
                    Ok((file, line)) => self.external = Some(External::Edit(file, Some(line))),
                    Err(e) => self.error(format!("Cannot open the places:\n{e}")),
                }
                return None;
            }
            KeyCode::Up => {
                cursor = cursor.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                cursor = (cursor + 1).min(last);
                None
            }
            KeyCode::Home | KeyCode::PageUp => {
                cursor = 0;
                None
            }
            KeyCode::End | KeyCode::PageDown => {
                cursor = last;
                None
            }
            KeyCode::Enter => places.get(cursor),
            KeyCode::Char(c) if !ctrl && !alt => places.iter().find(|p| p.key == Some(c.to_ascii_lowercase())),
            _ => None,
        };
        match chosen {
            Some(place) => {
                let place = place.clone();
                self.go_to_place(&place);
                None
            }
            None => Some(Dialog::Places { root, id, places, cursor }),
        }
    }

    /// Into the place's folder, the cursor on its file.
    fn go_to_place(&mut self, place: &Place) {
        self.change_dir(self.active, place.dir.clone());
        if let Some(file) = place.file.as_deref().filter(|_| self.dialog.is_none()) {
            self.active_panel_mut().select_name(file);
        }
    }

    /// Into `worktree`, to the folder the panel is in within the working tree at `here` when
    /// it has that folder too (src\ui in one, src\ui in the other), else as near it as it has.
    fn go_to_worktree(&mut self, worktree: &Worktree, here: &Path) {
        if worktree.missing {
            let path = worktree.path.display();
            return self.error(format!("The worktree's folder is gone:\n{path}\n(git worktree prune forgets it)"));
        }
        self.go_into_worktree(&worktree.path, here);
    }

    /// Into the worktree at `tree`, to the folder the panel is in within the one at `here`, or
    /// as near it as `tree` has.
    fn go_into_worktree(&mut self, tree: &Path, here: &Path) {
        let inside = self.active_panel().cwd.strip_prefix(here).map(Path::to_path_buf).unwrap_or_default();
        let target = tree.join(inside);
        let places = target.ancestors().take_while(|dir| dir.starts_with(tree)).map(Path::to_path_buf).collect();
        self.go_to_first(self.active, places);
    }

    /// Into the folder under the cursor (on `..`, up); nothing on a file.
    fn enter_current_dir(&mut self) {
        match self.active_panel().current() {
            Some(entry) if entry.is_parent => self.go_up(),
            Some(entry) if entry.is_dir => self.open_current(),
            _ => {}
        }
    }

    fn go_up(&mut self) {
        // In an archive: up inside it, and out of it at its top.
        if self.active_panel().archive.is_some() {
            return self.panels[self.active].archive_up();
        }
        // On another computer: up there, and from its top back to this one.
        if let Some(view) = &self.active_panel().remote {
            let target = ssh::parent(&view.dir).map_or_else(|| self.active_panel().cwd.clone(), |parent| view.place(&parent));
            return self.change_dir(self.active, target);
        }
        match self.active_panel().cwd.parent() {
            Some(parent) => self.change_dir(self.active, parent.to_path_buf()),
            None => self.open_drives(self.active),
        }
    }

    /// The drive picker. Under the drives come the computers connected to lately and the
    /// hosts of `~/.ssh/config`.
    fn open_drives(&mut self, side: usize) {
        let mut drives = platform::drives();
        let earlier = self.history.inputs.list("ssh").iter().rev().filter_map(|text| ssh::parse_connect(text).ok()).map(|(target, _)| target);
        let configured = ssh::configured_hosts().into_iter().filter_map(|host| ssh::parse_connect(&host).ok()).map(|(target, _)| target);
        let mut hosts: Vec<Target> = Vec::new();
        for target in earlier.chain(configured) {
            if !hosts.contains(&target) {
                hosts.push(target);
            }
        }
        drives.extend(hosts.iter().map(|target| platform::DriveInfo {
            root: PathBuf::from(target.place("")),
            label: String::new(),
            kind: "ssh",
            space: None,
            pending: false,
        }));
        let cwd = &self.panels[side].cwd;
        // The most specific drive holding the panel's folder (on Linux everything is under /).
        let cursor = drives
            .iter()
            .enumerate()
            .filter(|(_, d)| util::is_inside(cwd, &d.root))
            .max_by_key(|(_, d)| d.root.components().count())
            .map_or(0, |(i, _)| i);
        let slow: Vec<PathBuf> = drives.iter().filter(|d| d.pending).map(|d| d.root.clone()).collect();
        self.drive_space = (!slow.is_empty()).then(|| platform::query_space_later(slow));
        self.dialog = Some(Dialog::Drives { side, drives, cursor });
    }

    /// Goes to the folder the panel last had open on that drive. Choosing the drive the panel
    /// is already on goes to its root, as in Total Commander.
    fn switch_drive(&mut self, side: usize, root: &Path) {
        if ssh::parse_exact(&root.to_string_lossy()).is_some() {
            return self.change_dir(side, root.to_path_buf());
        }
        let panel = &self.panels[side];
        let here = util::volume_key(root) == util::volume_key(&panel.cwd);
        let places = if here { vec![root.to_path_buf()] } else { panel.places_on_drive(root) };
        self.go_to_first(side, places);
    }

    pub(crate) fn open_current(&mut self) {
        let Some(entry) = self.active_panel().current().cloned() else { return };
        if self.active_panel().archive.is_some() {
            return self.open_in_archive(&entry);
        }
        if entry.is_parent {
            self.go_up();
        } else if entry.is_dir {
            self.change_dir(self.active, entry.path);
        } else if let Some(view) = self.active_panel().remote.clone() {
            if let Some(path) = self.path_there(&view, &entry) {
                self.fetch(&entry, path, AfterJob::Open { shown: entry.path.clone() });
            }
        } else if crate::lfs::needs_literal(&entry.path) {
            // `foo.` would open as `foo` in any program it was given to.
            self.error(crate::lfs::UNOPENABLE);
        } else if archive::is_archive(&entry.path) {
            // A zip opens as a folder, as in Total Commander and mc.
            self.open_zip_here(&entry.path, entry.path.clone());
        } else if let Some(folder) = lnk::is_shortcut(&entry.path).then(|| lnk::target(&entry.path)).flatten().filter(|t| t.is_dir()) {
            // A Windows shortcut to a folder: the panel goes there (not an Explorer window).
            self.change_dir(self.active, folder);
        } else if entry.is_exec && platform::runs_as_program(&entry.path) {
            self.run_in_shell(self.shell().run_file_line(&entry.path));
        } else {
            match platform::open_default(&entry.path) {
                Ok(()) => {}
                // A server or WSL has nothing to open files with: show the file instead.
                Err(OpenError::NoOpener) => self.view(entry.path, entry.size, None),
                Err(OpenError::Failed(e)) => self.error(e),
            }
        }
    }

    /// `path`, a zip, opens in the active panel as `shown`; names in it that aren't safe to
    /// unpack are left out, and said so.
    fn open_zip_here(&mut self, path: &Path, shown: PathBuf) {
        self.panels[self.active].open_archive(path, shown);
        self.settle(self.active);
    }

    /// Shift+Enter: Windows' "How do you want to open this file?" for the file under the cursor.
    fn open_with(&mut self) {
        if self.refuse_in_archive(self.active) || self.refuse_over_ssh("Shift+Enter opens files of this computer") {
            return;
        }
        let Some(entry) = self.active_panel().current().filter(|e| !e.is_parent && !e.is_dir).cloned() else { return };
        if crate::lfs::needs_literal(&entry.path) {
            return self.error(crate::lfs::UNOPENABLE);
        }
        if let Err(e) = platform::open_with(&entry.path) {
            self.error(e);
        }
    }

    /// Enter in an archive: folders open, `..` goes up (out at its top), and a file is taken
    /// out and opened in its app (a zip in the zip opens in the panel).
    fn open_in_archive(&mut self, entry: &Entry) {
        let panel = &mut self.panels[self.active];
        if entry.is_parent {
            panel.archive_up();
        } else if entry.is_dir {
            panel.archive_enter(&entry.name);
        } else {
            let shown = panel.location().join(&entry.name);
            self.take_out(entry, AfterJob::Open { shown });
        }
    }

    /// A file in an archive goes out into cm's temp folder (as a job: it may be big), and
    /// `then` happens to it there.
    fn take_out(&mut self, entry: &Entry, then: AfterJob) {
        let Some(view) = self.active_panel().archive.as_ref() else { return };
        let inner = archive::join(&view.inner, &entry.name);
        let file = view.archive.file.clone();
        let Some(dir) = self.temp_folder() else { return };
        self.after_job = Some((then, dir.join(&entry.name)));
        self.start_job(Job::Extract { archive: file, items: vec![inner], dest: dir });
    }

    /// A new folder in cm's temp folder, for a file that comes here for a while.
    fn temp_folder(&mut self) -> Option<PathBuf> {
        static TAKEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        match archive::temp_dir() {
            Ok(dir) => Some(dir.join(TAKEN.fetch_add(1, Ordering::Relaxed).to_string())),
            Err(e) => {
                self.error(format!("No folder for files taken out for a while:\n{e}"));
                None
            }
        }
    }

    /// Where a file of a panel on another computer really is. To look at it or edit it, a link
    /// is what it leads to: a link brought here would lead to a file of this computer, and
    /// one put back over itself would save nothing.
    fn path_there(&mut self, view: &RemoteView, entry: &Entry) -> Option<String> {
        let path = ssh::join(&view.dir, &entry.name);
        if !entry.is_link {
            return Some(path);
        }
        match view.connection.sftp.realpath(&path) {
            Ok(real) => Some(real),
            Err(e) => {
                self.error(format!("Cannot follow \"{}\":\n{e}", entry.path.display()));
                None
            }
        }
    }

    /// A file at `path` on another computer comes into cm's temp folder (as a job: it may be
    /// big), under the name it has in the panel, and `then` happens to it there.
    fn fetch(&mut self, entry: &Entry, path: String, then: AfterJob) {
        let Some(view) = self.active_panel().remote.clone() else { return };
        let Some(dir) = self.temp_folder() else { return };
        if !remote::Place::Here(dir.clone()).can_hold(&entry.name) {
            return self.error(format!("\"{}\" can't be a name on this computer.", entry.name));
        }
        let local = dir.join(&entry.name);
        self.after_job = Some((then, local.clone()));
        let from = remote::Place::There(view.connection.clone(), path);
        self.start_job(Job::Transfer { pairs: vec![(from, remote::Place::Here(local))], moving: false, replace: false });
    }

    /// The file a job took out of an archive is there: open it, view it, or open it as an
    /// archive itself.
    fn after_taking_out(&mut self, then: AfterJob, path: PathBuf) {
        let size = fs::metadata(&path).map_or(0, |m| m.len());
        match then {
            AfterJob::Open { shown } if archive::is_archive(&path) => self.open_zip_here(&path, shown),
            AfterJob::Open { .. } => {
                // Only a copy, gone when cm ends: read-only, so that its app says so when
                // something is saved, instead of the work being lost with the copy.
                if let Ok(meta) = fs::metadata(&path) {
                    let mut permissions = meta.permissions();
                    permissions.set_readonly(true);
                    let _ = fs::set_permissions(&path, permissions);
                }
                self.notice = Some("Opened a read-only copy: changes have to be saved somewhere else".into());
                match platform::open_default(&path) {
                    Ok(()) => {}
                    Err(OpenError::NoOpener) => self.view(path, size, None),
                    Err(OpenError::Failed(e)) => self.error(e),
                }
            }
            AfterJob::View => {
                self.viewed_copy = path.parent().map(|folder| (folder.to_path_buf(), Instant::now()));
                self.view(path, size, None);
            }
            AfterJob::Edit { connection, path: file, there } => {
                let modified = fs::metadata(&path).ok().and_then(|meta| meta.modified().ok());
                let since = Instant::now();
                self.edited_copy = Some(EditedCopy { local: path.clone(), connection, path: file, size, modified, there, since, rescued: None });
                self.edit(path, size, None);
            }
        }
    }

    /// What works on this computer only: in a panel showing another one, say so and return
    /// true. `why` says what doesn't go.
    fn refuse_over_ssh(&mut self, why: &str) -> bool {
        let over_ssh = self.active_panel().remote.is_some();
        if over_ssh {
            self.notice = Some(format!("{why}, and this panel shows another one"));
        }
        over_ssh
    }

    /// Archives open read-only: in panel `side` one is shown, say so and return true.
    fn refuse_in_archive(&mut self, side: usize) -> bool {
        if self.panels[side].archive.is_none() {
            return false;
        }
        self.error("cm opens archives read-only.\nF5 copies files out of one; Alt+F5 packs files into a new one.");
        true
    }

    fn view_current(&mut self) {
        match self.active_panel().current().cloned() {
            Some(entry) if entry.is_dir => self.open_current(),
            Some(entry) if self.active_panel().archive.is_some() => self.take_out(&entry, AfterJob::View),
            Some(entry) if self.active_panel().remote.is_some() => {
                let Some(view) = self.active_panel().remote.clone() else { return };
                if let Some(path) = self.path_there(&view, &entry) {
                    self.fetch(&entry, path, AfterJob::View);
                }
            }
            Some(entry) => self.view(entry.path, entry.size, None),
            None => {}
        }
    }

    /// F3: the viewer, or for a big file the pager (or, without one, the viewer after asking);
    /// at `line` if given.
    fn view(&mut self, path: PathBuf, size: u64, line: Option<u64>) {
        if size <= self.config.big_file {
            self.external = Some(External::View(path, line));
        } else if self.config.pager.is_some() {
            self.external = Some(External::Page(path, line));
        } else {
            self.confirm_big_file(path, size, false, line);
        }
    }

    fn edit_current(&mut self) {
        let Some(entry) = self.active_panel().current().cloned() else { return };
        if entry.is_dir {
            return;
        }
        // A file in an archive can only be looked at here.
        if self.active_panel().archive.is_some() {
            self.take_out(&entry, AfterJob::View);
            self.notice = Some("Files in archives open read-only".into());
            return;
        }
        if let Some(view) = self.active_panel().remote.clone() {
            let Some(path) = self.path_there(&view, &entry) else { return };
            return match view.connection.sftp.stat(&path) {
                Ok(there) => self.fetch(&entry, path.clone(), AfterJob::Edit { connection: view.connection, path, there }),
                Err(e) => self.error(format!("Cannot read \"{}\":\n{e}", entry.path.display())),
            };
        }
        self.edit(entry.path, entry.size, None);
    }

    /// F4: the editor, after asking for a big file; at `line` if given.
    fn edit(&mut self, path: PathBuf, size: u64, line: Option<u64>) {
        if size > self.config.big_file {
            self.confirm_big_file(path, size, true, line);
        } else {
            self.external = Some(External::Edit(path, line));
        }
    }

    /// "x.log is 250M. Open it in nvim anyway?", with No as the default answer.
    fn confirm_big_file(&mut self, path: PathBuf, size: u64, edit: bool, line: Option<u64>) {
        let program = if edit { &self.config.editor[0] } else { &self.config.viewer[0] };
        let program = Path::new(program).file_stem().map_or_else(|| program.clone(), |s| s.to_string_lossy().into_owned());
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        self.dialog = Some(Dialog::Confirm {
            title: "Big file".into(),
            lines: vec![
                format!("{name} is {}.", util::human_size(size)),
                format!("{program} reads all of it before it shows anything."),
                format!("Open it in {program} anyway?"),
            ],
            focus: 1,
            danger: false,
            action: if edit { ConfirmAction::Edit(path, line) } else { ConfirmAction::View(path, line) },
        });
    }

    fn start_transfer(&mut self, is_move: bool) {
        if self.refuse_in_archive(1 - self.active) || (is_move && self.refuse_in_archive(self.active)) {
            return;
        }
        // Out of an archive: F5 extracts.
        if let Some((archive, items)) = self.active_panel().archive_selection() {
            if items.is_empty() {
                return;
            }
            let default = util::with_trailing_separator(&self.folder_here());
            let what = match items.as_slice() {
                [one] => format!("\"{}\"", one.rsplit('/').next().unwrap_or(one)),
                many => format!("{} items", many.len()),
            };
            let prompt = format!("Copy {what} out of the archive to:");
            return self.start_input("Copy", &prompt, &default, InputAction::Extract { archive, items });
        }
        let sources = self.active_panel().selection();
        if sources.is_empty() {
            return;
        }
        let other = &self.panels[1 - self.active];
        let default = other.remote.as_ref().map_or_else(|| util::with_trailing_separator(&other.cwd), |view| view.folder_text());
        let what = describe(&sources);
        if is_move {
            self.start_input("Move", &format!("Move {what} to:"), &default, InputAction::Move(sources));
        } else {
            self.start_input("Copy", &format!("Copy {what} to:"), &default, InputAction::Copy(sources));
        }
    }

    /// Shift+F5 / Shift+F6, as in mc: the item under the cursor, marks or not, in this folder.
    /// The field starts with its name, selected, and what is typed counts from this folder.
    fn start_in_place(&mut self, is_move: bool) {
        if self.refuse_in_archive(self.active) {
            return;
        }
        let Some(entry) = self.active_panel().current().filter(|e| !e.is_parent) else { return };
        let (name, path) = (entry.name.clone(), entry.path.clone());
        if is_move {
            self.start_name_input("Rename", &format!("Rename \"{name}\" to:"), &name, InputAction::MoveHere(path));
        } else {
            self.start_name_input("Copy", &format!("Copy \"{name}\" to:"), &name, InputAction::CopyHere(path));
        }
    }

    fn start_rename(&mut self) {
        if self.refuse_in_archive(self.active) {
            return;
        }
        let Some(entry) = self.active_panel().current().filter(|e| !e.is_parent) else { return };
        let (name, path) = (entry.name.clone(), entry.path.clone());
        self.start_name_input("Rename", &format!("Rename \"{name}\" to:"), &name, InputAction::Rename(path));
    }

    /// F2 on another computer. As here, what is there stays.
    fn rename_there(&mut self, view: &RemoteView, from: &Path, input: &str) {
        let Some((_, from)) = ssh::parse_exact(&from.to_string_lossy()) else { return };
        let to = view.resolve(input);
        if to == from {
            return;
        }
        let shown = view.place(&from);
        let sftp = &view.connection.sftp;
        if sftp.lstat(&to).is_ok() {
            return self.error(format!("\"{}\" already exists.", view.place(&to).display()));
        }
        match sftp.rename(&from, &to) {
            Ok(true) => {
                self.reload_panels();
                self.select_in_active(&view.place(&to));
            }
            Ok(false) => self.error(format!("Cannot rename \"{}\":\nthe server refused.", shown.display())),
            Err(e) => self.error(format!("Cannot rename \"{}\":\n{e}", shown.display())),
        }
    }

    fn rename(&mut self, from: &Path, input: &str) {
        if input.trim().is_empty() {
            return;
        }
        if let Some(view) = self.active_panel().remote.clone() {
            return self.rename_there(&view, from, input);
        }
        let dir = from.parent().unwrap_or(from);
        // The name as Windows makes it, said here: left to Windows, `foo.` beside `foo` lands
        // on `foo`, and the file there is gone.
        let target = util::as_windows_makes_it(util::resolve(dir, input));
        if target == from {
            return;
        }
        // Another letter case is fine where both names are the one entry (Windows, Linux's
        // /mnt/c); where they are two files (as in a folder WSL made), or one is a link to the
        // other, the other one stays. A rename replaces what it lands on, so anything there
        // (a broken link too) counts.
        if lfs::symlink_metadata(&target).is_ok() && !util::same_entry(from, &target) {
            self.error(format!("\"{}\" already exists.", target.display()));
            return;
        }
        if let Err(e) = lfs::rename(from, &target) {
            self.error(format!("Cannot rename \"{}\":\n{e}", from.display()));
            return;
        }
        self.reload_panels();
        self.select_in_active(&target);
    }

    fn mkdir(&mut self, cwd: &Path, input: &str) {
        if input.trim().is_empty() {
            return;
        }
        if let Some(view) = self.active_panel().remote.clone() {
            let path = view.resolve(input);
            let place = view.place(&path);
            let sftp = &view.connection.sftp;
            if sftp.lstat(&path).is_ok() {
                self.error(format!("\"{}\" already exists.", place.display()));
                return self.select_in_active(&place);
            }
            if let Err(e) = remote::make_folders(sftp, &path) {
                return self.error(format!("Cannot create \"{}\":\n{e}", place.display()));
            }
            self.reload_panels();
            return self.select_in_active(&place);
        }
        let target = util::as_windows_makes_it(util::resolve(cwd, input));
        if lfs::exists(&target) {
            self.error(format!("\"{}\" already exists.", target.display()));
            self.select_in_active(&target);
            return;
        }
        if let Err(e) = lfs::create_dir_all(&target) {
            self.error(format!("Cannot create \"{}\":\n{e}", target.display()));
            return;
        }
        self.reload_panels();
        self.select_in_active(&target);
    }

    fn new_file(&mut self, cwd: &Path, input: &str) {
        if input.trim().is_empty() {
            return;
        }
        let target = util::as_windows_makes_it(util::resolve(cwd, input));
        if !lfs::exists(&target) {
            let created = target
                .parent()
                .map_or(Ok(()), lfs::create_dir_all)
                .and_then(|()| lfs::create_new(&target).map(drop));
            if let Err(e) = created {
                self.error(format!("Cannot create \"{}\":\n{e}", target.display()));
                return;
            }
        }
        self.reload_panels();
        self.select_in_active(&target);
        self.external = Some(External::Edit(target, None));
    }

    /// Puts the cursor on `path` (or the directory leading to it) in the active panel.
    fn select_in_active(&mut self, path: &Path) {
        let panel = self.active_panel_mut();
        if let Some(view) = &panel.remote {
            let Some((_, path)) = ssh::parse_exact(&path.to_string_lossy()) else { return };
            let below = path.strip_prefix(&ssh::join(&view.dir, ""));
            if let Some(name) = below.and_then(|below| below.split('/').next()).map(String::from) {
                panel.select_name(&name);
            }
            return;
        }
        if let Some(name) = util::child_name(&panel.cwd, path) {
            panel.select_name(&name);
        }
    }

    fn confirm_delete(&mut self, to_trash: bool) {
        if self.refuse_in_archive(self.active) {
            return;
        }
        // Over SSH there is no trash, so F8 asks what Shift+F8 asks, and says so.
        if let Some(view) = self.active_panel().remote.clone() {
            let paths = self.active_panel().remote_selection();
            let what = match paths.as_slice() {
                [] => return,
                [one] => format!("\"{}\"", ssh::name_of(one)),
                many => format!("{} items", many.len()),
            };
            let lines = vec![
                format!("Permanently delete {what} on {}?", view.connection.target),
                "There is no trash over SSH. This cannot be undone.".to_string(),
            ];
            let action = ConfirmAction::DeleteThere(view.connection, paths);
            self.dialog = Some(Dialog::Confirm { title: "Delete permanently".into(), lines, focus: 1, danger: true, action });
            return;
        }
        let paths = self.active_panel().selection();
        if paths.is_empty() {
            return;
        }
        // The question comes at once; the survey fills in what it would remove meanwhile.
        let survey = Survey::start(paths.clone());
        // Permanent deletion defaults to No.
        self.dialog = Some(Dialog::Delete { paths, to_trash, focus: if to_trash { 0 } else { 1 }, survey });
    }

    fn toggle_hidden(&mut self) {
        let show = !self.panels[0].show_hidden;
        // The tabs in the background too, or switching to one would bring the old back.
        for panel in self.panels.iter_mut().chain(self.tabs.iter_mut().flatten()) {
            panel.show_hidden = show;
            panel.reload();
        }
    }

    fn insert_current_name(&mut self) {
        let Some(entry) = self.active_panel().current().filter(|e| !e.is_parent) else { return };
        let name = &entry.name;
        // PowerShell also takes ‘ ’ ‚ ‛ as single quotes and “ ” „ as double ones. A name with
        // * or ? (legal on Linux) would match other files unquoted (`rm a*`), ! is bash's
        // history, % cmd's variables, ~ a home folder, \ an escape.
        let needs_quotes = name.chars().any(|c| c.is_whitespace() || "'\"&()[]{};,$`#@|<>‘’‚‛“”„*?!%~\\".contains(c));
        let mut text = if needs_quotes { self.shell().quote(name) } else { name.clone() };
        text.push(' ');
        if !self.cmdline.is_empty() && !self.cmdline.text().ends_with(' ') {
            text.insert(0, ' ');
        }
        self.cmdline.insert_str(&text);
    }

    fn run_cmdline(&mut self) {
        let line = self.cmdline.text().trim().to_string();
        self.cmdline.clear();
        self.history_pos = None;
        if line.is_empty() {
            return;
        }
        // The folder as the shell would read it (quotes, variables): Ctrl+Enter inserts names
        // quoted for the shell, so `cd 'Účty a faktury'` must work.
        let posix = self.shell().is_posix();
        let cd = cd_argument(&line).map(|arg| util::shell_word(arg, posix));
        // Before it joins the history: an address may hold a password.
        if let Some(Some(arg)) = &cd
            && let Err(e) = ssh::parse_typed(arg)
        {
            return self.error(e);
        }
        self.history.command(&line);
        self.history_changed = true;
        match cd {
            // `cd -`: back to the folder before, as in bash and PowerShell.
            Some(Some(arg)) if arg == "-" => self.step_back(true),
            // Another computer, by its address; a folder of that name here stays that folder.
            Some(Some(arg)) if ssh::parse(&arg).is_some() && !lfs::is_dir(&util::resolve(&self.active_panel().cwd, &arg)) => {
                self.change_dir(self.active, PathBuf::from(arg));
            }
            // On a panel showing another computer, folders are the ones there, unless a drive
            // of this computer is named.
            Some(Some(arg)) if self.active_panel().remote.is_some() && util::bare_drive(&arg).is_none() && !has_drive(&arg) => {
                if let Some(view) = self.active_panel().remote.clone() {
                    self.change_dir(self.active, view.place(&view.resolve(&arg)));
                }
            }
            Some(Some(arg)) => {
                let panel = self.active_panel();
                let cwd = panel.cwd.clone();
                let places = match util::bare_drive(&arg) {
                    // "cd e:" returns to where this panel was on E: (and stays put when already there).
                    Some(root) if util::same_volume(&root, &cwd) => return,
                    Some(root) => panel.places_on_drive(&root),
                    None if arg.is_empty() => vec![util::home_dir().unwrap_or(cwd)],
                    None => vec![util::resolve(&cwd, &arg)],
                };
                self.go_to_first(self.active, places);
            }
            // A command would run on this computer, and not in the folder shown: `rm *` there
            // is no place to find out.
            _ if self.active_panel().remote.is_some() => {
                self.error("Commands run on this computer, and this panel shows another one.\nTab goes to the other panel; Ctrl+O is this computer's shell.");
            }
            // Any other command, and a cd only the shell can read (a variable set in it,
            // `cd $(…)`): the shell runs it, and the panel follows when the panels come back.
            _ => self.run_in_shell(line),
        }
    }

    fn history_step(&mut self, direction: isize) {
        let commands = &self.history.commands;
        if commands.is_empty() {
            return;
        }
        let last = commands.len() - 1;
        let pos = match (self.history_pos, direction < 0) {
            (None, true) => Some(last),
            (None, false) => None,
            (Some(p), true) => Some(p.saturating_sub(1)),
            (Some(p), false) if p < last => Some(p + 1),
            (Some(_), false) => None,
        };
        self.history_pos = pos;
        match pos {
            Some(p) => self.cmdline.set(self.history.commands[p].clone()),
            None => self.cmdline.clear(),
        }
    }
}

/// An attributes row clicked or spaced: set if it was clear or mixed, clear if it was set.
pub fn toggle_state(states: &mut [Option<bool>], row: usize) {
    if let Some(state) = states.get_mut(row) {
        *state = Some(!state.unwrap_or(false));
    }
}

/// `cd`, `cd DIR`, and cmd-style `cd..` / `cd\`; None for any other command.
fn cd_argument(line: &str) -> Option<&str> {
    let head = line.get(..2)?;
    if !head.eq_ignore_ascii_case("cd") {
        return None;
    }
    let rest = &line[2..];
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else if rest.starts_with(['.', '\\', '/']) {
        Some(rest)
    } else {
        None
    }
}

/// Whether a path typed names a drive of this computer (`C:\\x`, `e:foo`).
fn has_drive(text: &str) -> bool {
    matches!(Path::new(util::strip_quotes(text.trim())).components().next(), Some(Component::Prefix(_)))
}

/// Whether a path typed starts from the top rather than from a folder.
fn is_whole_path(text: &str) -> bool {
    let text = util::strip_quotes(text.trim());
    has_drive(text) || text.starts_with(['/', '\\'])
}

/// An edited file that can't go back to its computer: it moves out of the temp folder, which
/// goes when cm ends. Returns where it went.
fn rescue(copy: &Path) -> Option<PathBuf> {
    let dir = util::config_dir()?.join("rescued");
    fs::create_dir_all(&dir).ok()?;
    let target = dir.join(format!("{:08x}-{}", util::random() as u32, copy.file_name()?.to_string_lossy()));
    fs::copy(copy, &target).ok()?;
    Some(target)
}

fn describe(paths: &[PathBuf]) -> String {
    match paths {
        [one] => format!("\"{}\"", one.file_name().unwrap_or(one.as_os_str()).to_string_lossy()),
        many => format!("{} items", many.len()),
    }
}

/// The title and lines of the delete question, with what the survey has found so far.
pub fn delete_question(paths: &[PathBuf], to_trash: bool, tally: &Tally) -> (&'static str, Vec<String>) {
    let what = describe(paths);
    let mut lines =
        vec![if to_trash { format!("Move {what} to {}?", platform::TRASH_NAME) } else { format!("Permanently delete {what}?") }];
    // A single file needs no counting.
    if tally.folders > 0 || tally.files > 1 {
        let plural = |n: u64, word: &str| format!("{} {word}{}", util::thousands(n), if n == 1 { "" } else { "s" });
        let counts = format!("{} in {}, {}", plural(tally.files, "file"), plural(tally.folders, "folder"), util::human_size(tally.bytes));
        lines.push(if tally.done { format!("{counts}.") } else { format!("Counting… {counts} so far") });
    }
    if let Some((path, reason)) = &tally.denied {
        lines.push(String::new());
        if dialog::admin_offered(tally) {
            lines.push(format!("Needs administrator rights: {}", path.display()));
        } else {
            lines.push(format!("Cannot be deleted: {}", path.display()));
        }
        // "it belongs to …" as a sentence of its own.
        let why = reason.lines().next().unwrap_or_default().trim_start_matches("Access is denied: ");
        let mut chars = why.chars();
        lines.push(chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default());
        if dialog::admin_offered(tally) {
            lines.push(if to_trash {
                "As admin deletes it permanently, as administrator (Windows asks first).".into()
            } else {
                "As admin deletes it as administrator (Windows asks first).".into()
            });
        }
    }
    if !to_trash {
        lines.push("This cannot be undone.".into());
    }
    (if to_trash { "Delete" } else { "Delete permanently" }, lines)
}

fn question_dialog(question: Question, reply: Sender<Answer>) -> Dialog {
    let now = SystemTime::now();
    let describe_file = |info: &ops::FileInfo| {
        let time = info.modified.map(|t| util::format_time(t, now)).unwrap_or_default();
        format!("{:>8}  {time}", util::human_size(info.size))
    };
    match question {
        Question::Overwrite { target, source, existing } => Dialog::Question {
            title: "File exists".into(),
            lines: vec![
                "This file already exists:".into(),
                target.display().to_string(),
                String::new(),
                format!("New:      {}", describe_file(&source)),
                format!("Existing: {}", describe_file(&existing)),
                String::new(),
                "Overwrite it?".into(),
            ],
            buttons: dialog::OVERWRITE,
            answers: dialog::OVERWRITE_ANSWERS,
            focus: 0,
            error: false,
            reply,
        },
        Question::Failed { path, error, as_admin } => Dialog::Question {
            title: "Error".into(),
            lines: std::iter::once(path.display().to_string())
                .chain(error.lines().map(String::from))
                .chain(as_admin.map(String::from))
                .collect(),
            buttons: if as_admin.is_some() { dialog::FAILED_ADMIN } else { dialog::FAILED },
            answers: if as_admin.is_some() { dialog::FAILED_ADMIN_ANSWERS } else { dialog::FAILED_ANSWERS },
            focus: 1,
            error: true,
            reply,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::time::{Duration, Instant};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with_mods(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn typing(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    fn wait_for_job(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.job.is_some() {
            assert!(Instant::now() < deadline, "job did not finish");
            std::thread::sleep(Duration::from_millis(10));
            app.tick();
        }
    }

    fn config(hotlist_file: Option<PathBuf>) -> Config {
        Config { hotlist_file, ..crate::testutil::config() }
    }

    fn test_app(left: &Path, right: &Path) -> App {
        App::new(left.to_path_buf(), right.to_path_buf(), config(None))
    }

    fn ctrl(c: char) -> KeyEvent {
        with_mods(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// A file another program creates shows up in the panel by itself, with the cursor and
    /// the marks where they were.
    #[test]
    fn panels_follow_changes_made_by_other_programs() {
        let tmp = TempDir::new("app-watch");
        tmp.file("a.txt", "a");
        tmp.file("b.txt", "b");
        let mut app = test_app(tmp.path(), tmp.path());
        app.active_panel_mut().select_name("b.txt");
        app.active_panel_mut().toggle_mark();
        app.tick();
        let names = |app: &App| app.panels[0].entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        tmp.file("new.txt", "n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !names(&app).contains(&"new.txt".to_string()) || !app.panels[1].entries.iter().any(|e| e.name == "new.txt") {
            assert!(Instant::now() < deadline, "the new file never showed up: {:?}", names(&app));
            std::thread::sleep(Duration::from_millis(20));
            app.tick();
        }
        assert_eq!(app.active_panel().current().unwrap().name, "b.txt");
        assert_eq!(app.active_panel().marked_summary(), Some((1, 1)));
    }

    /// Alt+W lists the repository's worktrees; Enter goes to the same subfolder in the one
    /// chosen, or as near it as that one has. Outside a repository it says so.
    /// Alt+B: the branches newest first, one of a remote only when none here has it, and typing
    /// narrows them. Enter asks, then git switches; what git refuses is said; a branch another
    /// worktree has checked out takes the panel there.
    #[test]
    fn alt_b_finds_a_branch_and_switches_to_it() {
        use crate::git::Head;
        use crate::git::tests::{git_available, run_git};
        if !git_available() {
            eprintln!("git isn't installed: skipped");
            return;
        }
        let tmp = TempDir::new("app-branches");
        let upstream = tmp.dir("upstream");
        run_git(&upstream, &["init", "-q", "-b", "main"], None);
        fs::write(upstream.join("a.txt"), "a").unwrap();
        run_git(&upstream, &["add", "."], None);
        run_git(&upstream, &["commit", "-q", "--no-verify", "-m", "first"], Some("2026-01-01T12:00:00"));
        run_git(&upstream, &["branch", "old"], None);
        run_git(&upstream, &["switch", "-q", "-c", "feature"], None);
        fs::write(upstream.join("f.txt"), "f").unwrap();
        run_git(&upstream, &["add", "."], None);
        run_git(&upstream, &["commit", "-q", "--no-verify", "-m", "the feature"], Some("2026-01-03T12:00:00"));
        run_git(&upstream, &["switch", "-q", "main"], None);
        run_git(tmp.path(), &["clone", "-q", "upstream", "repo"], None);
        let repo = tmp.path().join("repo");
        run_git(&repo, &["worktree", "add", "-q", "-b", "elsewhere", "../wt"], None);
        let mut app = test_app(&repo, &repo);
        let alt_b = with_mods(KeyCode::Char('b'), KeyModifiers::ALT);

        app.on_key(alt_b);
        let Some(Dialog::Branches { branches, cursor, .. }) = &app.dialog else { panic!("no branch list") };
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["origin/feature", "elsewhere", "main", "origin/old"], "origin/main is main's");
        assert_eq!(*cursor, 0, "on the newest one this worktree isn't on");
        assert!(branches[2].worktree.as_ref().is_some_and(|tree| util::same_path(tree, &repo)));
        assert!(branches[1].worktree.as_ref().is_some_and(|tree| util::same_path(tree, &tmp.path().join("wt"))));

        // In the way of the switch: git says so, and nothing changes.
        fs::write(repo.join("f.txt"), "mine").unwrap();
        typing(&mut app, "feat");
        app.on_key(key(KeyCode::Enter));
        let Some(Dialog::Confirm { lines, .. }) = &app.dialog else { panic!("no question") };
        assert_eq!(lines[0], "Switch this worktree to origin/feature?");
        assert!(lines.iter().any(|line| line == "A branch feature that follows it is made here."), "{lines:?}");
        app.on_key(key(KeyCode::Enter));
        let wait_for_git = |app: &mut App| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while app.switching.is_some() {
                assert!(Instant::now() < deadline, "git never finished");
                std::thread::sleep(Duration::from_millis(20));
                app.tick();
            }
        };
        wait_for_git(&mut app);
        let Some(Dialog::Message { lines, error: true, .. }) = &app.dialog else { panic!("no error: {:?}", app.notice) };
        assert!(lines[0].starts_with("Cannot switch to origin/feature"), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("f.txt")), "{lines:?}");
        assert_eq!(fs::read_to_string(repo.join("f.txt")).unwrap(), "mine");
        app.on_key(key(KeyCode::Enter));

        // Out of the way: switched, and the panel shows the branch's files.
        fs::remove_file(repo.join("f.txt")).unwrap();
        app.on_key(alt_b);
        typing(&mut app, "feat");
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));
        wait_for_git(&mut app);
        assert!(app.dialog.is_none(), "{:?}", app.notice);
        assert_eq!(app.active_panel().repo.as_ref().map(|repo| repo.head.clone()), Some(Head::Branch("feature".into())));
        assert!(app.active_panel().entries.iter().any(|e| e.name == "f.txt"));
        assert!(app.notice.as_deref().is_some_and(|said| said.contains("feature")), "{:?}", app.notice);

        // A branch another worktree is on: the panel goes there, nothing asked.
        app.on_key(alt_b);
        typing(&mut app, "elsewhere");
        app.on_key(key(KeyCode::Enter));
        assert!(app.dialog.is_none());
        assert!(util::same_path(&app.active_panel().cwd, &tmp.path().join("wt")));
        // And the one it is on is said to be that.
        app.on_key(alt_b);
        typing(&mut app, "elsewhere");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.notice.as_deref(), Some("This worktree is on elsewhere already"));
    }

    #[test]
    fn alt_w_goes_to_the_same_folder_in_another_worktree() {
        use crate::git::tests::{fake_repo, fake_worktree};
        let tmp = TempDir::new("app-worktrees");
        let main = tmp.dir("app");
        fake_repo(&main, "main");
        tmp.dir("app/src/ui");
        let login = tmp.path().join("app-login");
        fake_worktree(&main, "app-login", &login, "login");
        tmp.dir("app-login/src/ui");
        let mut app = test_app(&main.join("src").join("ui"), tmp.path());
        assert_eq!(app.active_panel().repo.as_ref().map(|r| r.head.label()).as_deref(), Some("main"));

        app.on_key(alt(KeyCode::Char('w')));
        let Some(Dialog::Worktrees { worktrees, cursor, .. }) = &app.dialog else { panic!("no worktree list") };
        assert_eq!(worktrees.len(), 2);
        assert_eq!(*cursor, 1, "the cursor starts on the other one");
        app.on_key(key(KeyCode::Enter));
        assert!(app.dialog.is_none());
        assert_eq!(app.active_panel().cwd, login.join("src").join("ui"));
        let repo = app.active_panel().repo.as_ref().unwrap();
        assert_eq!((repo.linked, repo.head.label()), (true, "login".to_string()));

        // From a folder only this worktree has, back to the main one by typing its branch.
        let only_here = tmp.dir("app-login/src/ui/new-widget");
        app.change_dir(0, only_here);
        app.on_key(alt(KeyCode::Char('w')));
        typing(&mut app, "main");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.active_panel().cwd, main.join("src").join("ui"));

        // Outside a repository, a word on the command line.
        app.active = 1;
        app.on_key(alt(KeyCode::Char('w')));
        assert!(app.dialog.is_none());
        assert!(app.notice.as_deref().is_some_and(|n| n.starts_with("Not in a git repository")));
    }

    /// Ctrl+E in a git repository: its places from `places.ini`, the menu on the one the panel
    /// is at, and a key goes there with the cursor on its file. Ins adds where the panel is and
    /// Del forgets it. Outside a repository it says so, and with text on the command line
    /// Ctrl+E goes to its end, as in bash.
    #[test]
    fn ctrl_e_goes_to_the_places_of_a_repository() {
        let tmp = TempDir::new("app-places");
        let (checkout, file) = crate::places::tests::fake_workspace(&tmp);
        let app_folder = checkout.join("packages").join("app");
        let mut app = test_app(&app_folder.join("src").join("components"), tmp.path());
        app.config.places_file = Some(file.clone());

        app.on_key(ctrl('e'));
        let Some(Dialog::Places { places, cursor, .. }) = &app.dialog else { panic!("no places") };
        assert_eq!(places[*cursor].label, "app package", "it opens on the place the panel is at");
        app.on_key(key(KeyCode::Enter));
        assert!(app.dialog.is_none());
        assert_eq!(app.active_panel().cwd, app_folder);
        assert_eq!(app.active_panel().current().unwrap().name, "package.json");

        app.on_key(ctrl('e'));
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.active_panel().cwd, checkout.join("docs"));
        assert_eq!(app.active_panel().current().unwrap().name, "guide.md");
        app.on_key(alt(KeyCode::Left));
        assert_eq!(app.active_panel().cwd, app_folder, "Alt+Left comes back");

        app.on_key(ctrl('e'));
        app.on_key(key(KeyCode::Char('S')));
        assert_eq!(app.active_panel().cwd, checkout.join("scripts"));
        app.on_key(ctrl('e'));
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.active_panel().cwd, checkout.join("packages").join("blog"));

        // Back to the top, the cursor on the folder it came up from.
        app.on_key(ctrl('e'));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(app.active_panel().cwd, checkout);
        assert_eq!(app.active_panel().current().unwrap().name, "packages");

        // Ins adds where the panel is, under a key no place has; Del forgets it again, and the
        // rest of the file stays.
        app.change_dir(0, app_folder.join("src"));
        app.on_key(ctrl('e'));
        app.on_key(key(KeyCode::Insert));
        assert_eq!(app.notice.as_deref(), Some("src is place 3 in this repository"));
        assert!(fs::read_to_string(&file).unwrap().contains("3 = src | packages/app/src\n"));
        let Some(Dialog::Places { places, cursor, .. }) = &app.dialog else { panic!("the menu stays") };
        assert_eq!(places[*cursor].label, "src", "the cursor on the new place");
        app.on_key(key(KeyCode::Delete));
        let text = fs::read_to_string(&file).unwrap();
        assert!(!text.contains("packages/app/src") && text.contains("; the guide") && text.contains("[elsewhere]"), "{text}");
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Delete));
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("always there")), "the top can't go");

        // A key that is no place keeps the menu; Ctrl+E again closes it.
        app.on_key(key(KeyCode::Char('x')));
        assert!(matches!(app.dialog, Some(Dialog::Places { .. })));
        app.on_key(ctrl('e'));
        assert!(app.dialog.is_none());

        // F12 finds it.
        app.on_key(key(KeyCode::F(12)));
        typing(&mut app, "places");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.dialog, Some(Dialog::Places { .. })));
        app.on_key(key(KeyCode::Esc));

        // Typing a command: Ctrl+E is the end of the line.
        typing(&mut app, "ls");
        app.on_key(key(KeyCode::Home));
        app.on_key(ctrl('e'));
        assert!(app.dialog.is_none());
        typing(&mut app, " -l");
        assert_eq!(app.cmdline.text(), "ls -l");
        app.on_key(key(KeyCode::Esc));

        // Outside a repository, a word on the command line.
        app.active = 1;
        app.on_key(ctrl('e'));
        assert!(app.dialog.is_none());
        assert!(app.notice.as_deref().is_some_and(|n| n.starts_with("Not in a git repository")));
    }

    /// `cd` reads quotes as the shell does (Ctrl+Enter inserts names quoted that way), and
    /// F7 on a name that exists says so.
    #[test]
    fn cd_with_quotes_and_mkdir_of_an_existing_folder() {
        let tmp = TempDir::new("app-cd-quotes");
        let target = tmp.dir("Účty a faktury");
        let mut app = test_app(tmp.path(), tmp.path());
        let quoted = app.shell().quote("Účty a faktury");
        typing(&mut app, &format!("cd {quoted}"));
        app.on_key(key(KeyCode::Enter));
        assert!(app.dialog.is_none(), "{:?}", app.dialog.as_ref().map(|_| "a dialog"));
        assert!(util::same_path(&app.active_panel().cwd, &target));
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::F(7)));
        typing(&mut app, "Účty a faktury");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(&app.dialog, Some(Dialog::Message { lines, .. }) if lines[0].contains("already exists")));
    }

    fn alt(code: KeyCode) -> KeyEvent {
        with_mods(code, KeyModifiers::ALT)
    }

    /// Alt+Left / Alt+Right (and Alt+Y / Alt+U) go back and forward like a browser; `cd -`
    /// goes back too.
    #[test]
    fn back_and_forward_through_visited_folders() {
        let tmp = TempDir::new("app-back");
        let (a, b) = (tmp.dir("a"), tmp.dir("a/b"));
        let mut app = test_app(&a, &a);
        app.change_dir(0, b.clone());
        let here = |app: &App| app.active_panel().cwd.clone();
        app.on_key(alt(KeyCode::Left));
        assert!(util::same_path(&here(&app), &a));
        app.on_key(alt(KeyCode::Char('u')));
        assert!(util::same_path(&here(&app), &b));
        typing(&mut app, "cd -");
        app.on_key(key(KeyCode::Enter));
        assert!(util::same_path(&here(&app), &a));
        app.on_key(alt(KeyCode::Left));
        app.on_key(alt(KeyCode::Left));
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("back")), "nothing further back: {:?}", app.notice);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.notice, None, "the notice goes with the next key");
    }

    /// Alt+H lists the folders visited (the current one left out); typing narrows the list,
    /// Enter goes there, and it is all kept for the next session with the commands typed.
    #[test]
    fn folder_history_lists_narrows_and_persists() {
        let tmp = TempDir::new("app-history");
        let store = TempDir::new("app-history-store");
        let file = store.path().join("history.txt");
        let (photos, docs) = (tmp.dir("Fotky 2024"), tmp.dir("documents"));
        let config = Config { history_file: Some(file.clone()), ..crate::testutil::config() };
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), config);
        app.change_dir(0, photos.clone());
        app.change_dir(0, docs.clone());
        typing(&mut app, "echo remembered");
        app.history.command("echo remembered");
        app.cmdline.clear();
        app.on_key(alt(KeyCode::Char('h')));
        assert!(matches!(app.dialog, Some(Dialog::History { .. })));
        let rows = app.history_rows("");
        assert!(util::same_path(&rows[0], &photos), "newest first, the current folder left out: {rows:?}");
        typing(&mut app, "FOT");
        let Some(Dialog::History { filter, .. }) = &app.dialog else { panic!() };
        assert_eq!(app.history_rows(filter.text()).len(), 1);
        app.on_key(key(KeyCode::Enter));
        assert!(util::same_path(&app.active_panel().cwd, &photos));
        app.history_changed = true;
        app.save_history();

        let config = Config { history_file: Some(file), ..crate::testutil::config() };
        let next = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), config);
        assert!(next.history.dirs.iter().any(|d| util::same_path(d, &docs)), "{:?}", next.history.dirs);
        assert_eq!(next.history.commands, ["echo remembered"]);
    }

    /// Ctrl+C and Alt+V copy files across (and into the same folder as "name - Copy"); Ctrl+X
    /// and Alt+V move them, and then the clipboard is empty. (Tests use cm's own clipboard,
    /// not the system's.)
    #[test]
    fn files_go_through_the_clipboard() {
        let (left, right) = (TempDir::new("app-clip-left"), TempDir::new("app-clip-right"));
        left.file("report.txt", "r");
        right.file("cut-me.txt", "c");
        let mut app = test_app(left.path(), right.path());
        let alt_v = with_mods(KeyCode::Char('v'), KeyModifiers::ALT);
        app.active_panel_mut().select_name("report.txt");
        app.on_key(ctrl('c'));
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("copied")), "{:?}", app.notice);
        app.on_key(alt_v);
        wait_for_job(&mut app);
        assert!(left.path().join("report - Copy.txt").exists(), "pasted where it came from: a copy beside it");
        app.on_key(key(KeyCode::Tab));
        app.on_key(alt_v);
        wait_for_job(&mut app);
        assert!(right.path().join("report.txt").exists() && left.path().join("report.txt").exists());

        app.active_panel_mut().select_name("cut-me.txt");
        app.on_key(ctrl('x'));
        app.on_key(key(KeyCode::Tab));
        app.on_key(alt_v);
        wait_for_job(&mut app);
        assert!(left.path().join("cut-me.txt").exists() && !right.path().join("cut-me.txt").exists());
        assert_eq!(app.clipboard.files(), None, "cut and pasted: nothing left to paste");
        app.on_key(alt_v);
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("No files")));
    }

    /// Enter on a zip opens it as a folder: in and out of its folders, `..` at its top back
    /// out with the cursor on it. Nothing in it can be changed; F5 copies out of it, and F3
    /// views a file from it (taken out into the temp folder).
    #[test]
    fn zips_open_as_folders_and_copy_out() {
        let tmp = TempDir::new("app-zip");
        let out = TempDir::new("app-zip-out");
        crate::archive::tests::sample_zip(&tmp.path().join("photos.zip"));
        let mut app = test_app(tmp.path(), out.path());
        app.active_panel_mut().select_name("photos.zip");
        app.on_key(key(KeyCode::Enter));
        let names = |app: &App| app.active_panel().entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&app), ["..", "docs", "Účty", "top.txt"]);
        assert!(app.active_panel().location().ends_with("photos.zip"));
        app.active_panel_mut().select_name("Účty");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(names(&app), ["..", "2024"]);
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.active_panel().current().unwrap().name, "Účty", "back on the folder it came from");

        // Read-only: deleting is refused.
        app.on_key(key(KeyCode::F(8)));
        assert!(matches!(&app.dialog, Some(Dialog::Message { lines, .. }) if lines[0].contains("read-only")));
        app.dialog = None;

        // F5 copies the folder out, with what is in it.
        app.on_key(key(KeyCode::F(5)));
        app.on_key(key(KeyCode::Enter));
        wait_for_job(&mut app);
        assert_eq!(fs::read(out.path().join("Účty").join("2024").join("faktura.txt")).unwrap().len(), 1000);

        // F3 takes a file out and views it.
        app.active_panel_mut().select_name("top.txt");
        app.on_key(key(KeyCode::F(3)));
        wait_for_job(&mut app);
        let Some(External::View(path, None)) = &app.external else { panic!("{:?}", app.external) };
        assert_eq!(fs::read_to_string(path).unwrap(), "top");
        app.external = None;

        // `..` at the top leaves it, the cursor on the zip.
        app.active_panel_mut().home();
        app.on_key(key(KeyCode::Enter));
        assert!(app.active_panel().archive.is_none());
        assert_eq!(app.active_panel().current().unwrap().name, "photos.zip");
    }

    /// Alt+F5 packs into a zip (in the other panel by default), Alt+F9 unpacks one; packing
    /// a folder into itself is refused.
    #[test]
    fn packing_and_unpacking() {
        let tmp = TempDir::new("app-pack");
        let other = TempDir::new("app-pack-other");
        tmp.file("trip/day 1/a.jpg", "aaa");
        tmp.file("trip/notes.txt", "Účty");
        let mut app = test_app(tmp.path(), other.path());
        app.active_panel_mut().select_name("trip");
        app.on_key(with_mods(KeyCode::F(5), KeyModifiers::ALT));
        let Some(Dialog::Input { edit, .. }) = &app.dialog else { panic!() };
        assert!(edit.text().ends_with("trip.zip"), "{}", edit.text());
        app.on_key(key(KeyCode::Enter));
        wait_for_job(&mut app);
        let zip = other.path().join("trip.zip");
        let packed = Archive::open(&zip).unwrap();
        assert_eq!(packed.size_below("trip"), 3 + "Účty".len() as u64);
        assert!(packed.find("trip/day 1/a.jpg").is_some());

        // Unpacking it next to itself: one folder at its top, so straight in.
        app.on_key(key(KeyCode::Tab));
        app.active_panel_mut().select_name("trip.zip");
        app.on_key(with_mods(KeyCode::F(9), KeyModifiers::ALT));
        app.on_key(ctrl('a'));
        app.on_key(ctrl('k'));
        typing(&mut app, &other.path().join("unpacked").display().to_string());
        app.on_key(key(KeyCode::Enter));
        wait_for_job(&mut app);
        assert_eq!(fs::read_to_string(other.path().join("unpacked/trip/notes.txt")).unwrap(), "Účty");
        assert_eq!(fs::read_to_string(other.path().join("unpacked/trip/day 1/a.jpg")).unwrap(), "aaa");

        // A folder can't hold a zip of itself.
        app.on_key(key(KeyCode::Tab));
        app.active_panel_mut().select_name("trip");
        app.on_key(with_mods(KeyCode::F(5), KeyModifiers::ALT));
        app.on_key(ctrl('a'));
        app.on_key(ctrl('k'));
        typing(&mut app, &tmp.path().join("trip").join("self.zip").display().to_string());
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(&app.dialog, Some(Dialog::Message { lines, .. }) if lines[0].contains("zip of itself")));
    }

    /// Ctrl+F shows only matching files (folders stay); Ctrl+A marks all; Left/Right go up and
    /// into folders; Ctrl+PgUp/PgDn too.
    #[test]
    fn filter_mark_all_and_arrow_navigation() {
        let tmp = TempDir::new("app-filter");
        tmp.dir("sub");
        tmp.file("a.jpg", "a");
        tmp.file("b.cr2", "b");
        tmp.file("c.jpg", "c");
        let mut app = test_app(tmp.path(), tmp.path());
        app.on_key(ctrl('f'));
        typing(&mut app, "*.jpg");
        app.on_key(key(KeyCode::Enter));
        let names: Vec<String> = app.active_panel().entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["..", "sub", "a.jpg", "c.jpg"]);
        app.on_key(ctrl('a'));
        assert_eq!(app.active_panel().marked.len(), 3, "sub, a.jpg and c.jpg");
        app.active_panel_mut().clear_marks();

        app.active_panel_mut().select_name("sub");
        app.on_key(key(KeyCode::Right));
        assert!(app.active_panel().cwd.ends_with("sub"));
        assert_eq!(app.active_panel().filter.as_deref(), Some("*.jpg"), "the filter stays");
        app.on_key(key(KeyCode::Left));
        assert!(util::same_path(&app.active_panel().cwd, tmp.path()));
        assert_eq!(app.active_panel().current().unwrap().name, "sub", "back on the folder it came from");
        app.on_key(with_mods(KeyCode::PageDown, KeyModifiers::CONTROL));
        assert!(app.active_panel().cwd.ends_with("sub"));
        app.on_key(with_mods(KeyCode::PageUp, KeyModifiers::CONTROL));
        assert!(util::same_path(&app.active_panel().cwd, tmp.path()));

        // An empty filter shows everything again; with text on the command line the arrows
        // move in it.
        app.on_key(ctrl('f'));
        app.on_key(ctrl('a'));
        app.on_key(ctrl('k'));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.active_panel().entries.len(), 5);
        typing(&mut app, "ls");
        app.on_key(key(KeyCode::Left));
        assert!(util::same_path(&app.active_panel().cwd, tmp.path()));
        assert_eq!(app.cmdline.text(), "ls");
    }

    /// Enter on a Windows shortcut to a folder opens the folder in the panel.
    #[test]
    fn a_shortcut_to_a_folder_opens_it() {
        let tmp = TempDir::new("app-lnk");
        let target = tmp.dir("Fotky 2024");
        let link = tmp.path().join("Fotky.lnk");
        fs::write(&link, crate::lnk::tests::shortcut(&target.to_string_lossy(), r".\Fotky 2024")).unwrap();
        let mut app = test_app(tmp.path(), tmp.path());
        app.active_panel_mut().select_name("Fotky.lnk");
        app.on_key(key(KeyCode::Enter));
        assert!(util::same_path(&app.active_panel().cwd, &target), "{}", app.active_panel().cwd.display());
    }

    /// Shift+F2 marks on each side what the other lacks or has older; F5 then copies it over.
    #[test]
    fn comparing_folders_marks_what_to_copy() {
        let (left, right) = (TempDir::new("app-cmp-left"), TempDir::new("app-cmp-right"));
        left.file("both.txt", "same");
        right.file("both.txt", "same");
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        for dir in [&left, &right] {
            fs::File::options().write(true).open(dir.path().join("both.txt")).unwrap().set_modified(old).unwrap();
        }
        left.file("only-left.jpg", "l");
        right.file("only-right.jpg", "r");
        let mut app = test_app(left.path(), right.path());
        app.on_key(with_mods(KeyCode::F(2), KeyModifiers::SHIFT));
        let marked = |side: usize| app.panels[side].marked.iter().cloned().collect::<Vec<_>>();
        assert_eq!((marked(0), marked(1)), (vec!["only-left.jpg".to_string()], vec!["only-right.jpg".to_string()]));
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("1 left, 1 right")), "{:?}", app.notice);
    }

    /// Ctrl+T opens a tab on the same folder, Alt+N switches, Ctrl+W closes; each tab keeps
    /// its own folder.
    #[test]
    fn tabs_open_switch_and_close() {
        let tmp = TempDir::new("app-tabs");
        let (a, b) = (tmp.dir("a"), tmp.dir("b"));
        let mut app = test_app(&a, &a);
        app.on_key(ctrl('t'));
        assert_eq!((app.tabs[0].len(), app.tab_index[0]), (1, 1));
        app.change_dir(0, b.clone());
        assert_eq!(app.tab_names(0), ["a", "b"]);
        app.on_key(with_mods(KeyCode::Char('1'), KeyModifiers::ALT));
        assert!(util::same_path(&app.active_panel().cwd, &a));
        app.on_key(with_mods(KeyCode::Char('2'), KeyModifiers::ALT));
        assert!(util::same_path(&app.active_panel().cwd, &b));
        // Ctrl+W on the last tab: the one before takes its place.
        app.on_key(ctrl('w'));
        assert!(app.tabs[0].is_empty() && util::same_path(&app.active_panel().cwd, &a));
        app.on_key(ctrl('w'));
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("last tab")));
        // With text on the command line Ctrl+W deletes a word instead.
        typing(&mut app, "ls -la");
        app.on_key(ctrl('w'));
        assert_eq!(app.cmdline.text(), "ls ");
        // The tab row shows above the panels.
        app.on_key(ctrl('t'));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        let top: String = (0..80).map(|x| terminal.backend().buffer()[(x, 0)].symbol().to_string()).collect();
        assert!(top.contains("1:a") && top.contains("2:a"), "{top}");
    }

    /// Alt+A sets attributes: read-only and hidden on Windows, the permission bits elsewhere;
    /// a row where the marked files differ shows [?] and is left alone.
    #[test]
    fn attributes_are_shown_and_set() {
        let tmp = TempDir::new("app-attributes");
        tmp.file("a.txt", "a");
        tmp.file("b.txt", "b");
        let mut app = test_app(tmp.path(), tmp.path());
        let first_row = if cfg!(windows) { "Read-only" } else { "Owner may read" };
        assert_eq!(platform::ATTRIBUTES[0].0, first_row);
        // One file: set the first row's attribute, then clear it again.
        app.active_panel_mut().select_name("a.txt");
        app.on_key(with_mods(KeyCode::Char('a'), KeyModifiers::ALT));
        let Some(Dialog::Attributes { states, .. }) = &app.dialog else { panic!() };
        let before = states[0];
        // On Windows: make it hidden (row 1); elsewhere: take away "others may read" (row 6).
        let row = if cfg!(windows) { 1 } else { 6 };
        for _ in 0..row {
            app.on_key(key(KeyCode::Down));
        }
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Enter));
        let bit = platform::ATTRIBUTES[row].1;
        let value = platform::attributes(&tmp.path().join("a.txt")).unwrap();
        assert_eq!(value & bit != 0, cfg!(windows), "toggled");
        assert_eq!(Some(platform::attributes(&tmp.path().join("a.txt")).unwrap() & platform::ATTRIBUTES[0].1 != 0), before);
        // Both files: that row now differs.
        app.on_key(ctrl('a'));
        app.on_key(with_mods(KeyCode::Char('a'), KeyModifiers::ALT));
        let Some(Dialog::Attributes { states, paths, .. }) = &app.dialog else { panic!() };
        assert_eq!(paths.len(), 2);
        assert_eq!(states[row], None, "differs between the two");
        app.on_key(key(KeyCode::Esc));
        platform::set_attributes(&tmp.path().join("a.txt"), value ^ bit).unwrap();
    }

    /// F12 lists the commands; typing narrows them, Enter runs one.
    #[test]
    fn the_command_list_finds_and_runs_commands() {
        let tmp = TempDir::new("app-commands");
        let mut app = test_app(tmp.path(), tmp.path());
        app.on_key(key(KeyCode::F(12)));
        typing(&mut app, "zip");
        let Some(Dialog::Commands { filter, .. }) = &app.dialog else { panic!() };
        let labels: Vec<&str> = dialog::matching_commands(filter.text()).iter().map(|c| c.label).collect();
        assert_eq!(labels, ["Pack into a zip", "Unpack the zip under the cursor"]);
        app.on_key(ctrl('a'));
        app.on_key(ctrl('k'));
        typing(&mut app, "quick view");
        app.on_key(key(KeyCode::Enter));
        assert!(app.dialog.is_none() && app.quick_view, "it ran");
        // Every command's key does something that isn't typing into the command line.
        for command in dialog::COMMANDS {
            let mut app = test_app(tmp.path(), tmp.path());
            app.on_key(KeyEvent::new(command.code, command.modifiers));
            assert!(app.cmdline.is_empty() || command.keys.len() == 1, "{} ({}) typed into the command line", command.label, command.keys);
        }
    }

    /// With Caps Lock on, Windows reports Ctrl+D as Ctrl with a capital D: it still works.
    #[test]
    fn ctrl_shortcuts_work_with_caps_lock() {
        let tmp = TempDir::new("app-caps");
        let mut app = test_app(tmp.path(), tmp.path());
        app.on_key(ctrl('D'));
        assert!(matches!(app.dialog, Some(Dialog::Hotlist { .. })));
    }

    #[test]
    fn hotlist_adds_jumps_and_persists() {
        let tmp = TempDir::new("app-hotlist");
        let settings = TempDir::new("app-hotlist-config");
        let file = settings.path().join("hotlist.txt");
        tmp.dir("deep/inside");
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), config(Some(file.clone())));

        // Ctrl+\ opens the list, A adds the current folder.
        app.on_key(ctrl('\\'));
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::Esc));
        assert!(app.dialog.is_none());
        assert_eq!(app.hotlist.dirs.len(), 1);

        // Ctrl+D works too; add a second folder from elsewhere.
        typing(&mut app, &format!("cd deep{}inside", std::path::MAIN_SEPARATOR));
        app.on_key(key(KeyCode::Enter));
        app.on_key(ctrl('d'));
        app.on_key(key(KeyCode::Insert));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.hotlist.dirs.len(), 2);

        // 1 jumps to the first favourite.
        app.on_key(ctrl('\\'));
        app.on_key(key(KeyCode::Char('1')));
        assert!(util::same_path(&app.active_panel().cwd, tmp.path()));

        // Shift+Down moves it, D removes it; the file follows every change.
        app.on_key(ctrl('\\'));
        app.on_key(key(KeyCode::Home));
        app.on_key(with_mods(KeyCode::Down, KeyModifiers::SHIFT));
        let saved = fs::read_to_string(&file).unwrap();
        assert!(saved.lines().next().unwrap().ends_with("inside"), "{saved}");
        app.on_key(key(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(Hotlist::load(Some(file)).dirs.len(), 1);
    }

    #[test]
    fn ctrl_space_measures_directories() {
        let tmp = TempDir::new("app-sizes");
        tmp.file("big/a.bin", &"x".repeat(5000));
        tmp.file("big/sub/b.bin", &"x".repeat(3000));
        tmp.file("small/c.bin", "xyz");
        tmp.file("loose.txt", "12");
        let mut app = test_app(tmp.path(), tmp.path());

        // On ".." every directory is measured.
        app.on_key(key(KeyCode::Home));
        app.on_key(ctrl(' '));
        let deadline = Instant::now() + Duration::from_secs(20);
        // The measuring can finish completely between two ticks; the tick that picks up the
        // results must still say the screen changed (or the sizes would never be drawn).
        while app.working() {
            assert!(Instant::now() < deadline, "sizes did not arrive");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.tick(), "picking up the results asks for a redraw");
        assert!(!app.tick(), "nothing new: no redraw");
        let panel = app.active_panel();
        let size_of = |name: &str| panel.dir_size(panel.entries.iter().find(|e| e.name == name).unwrap()).unwrap();
        assert_eq!(size_of("big").bytes, 8000);
        assert_eq!(size_of("small").bytes, 3);
        assert_eq!(panel.total(), Some(8005));

        // Sorting by size (F9, s) uses the measured sizes.
        app.on_key(key(KeyCode::F(9)));
        app.on_key(key(KeyCode::Char('s')));
        let order: Vec<&str> = app.active_panel().entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(order, ["..", "big", "small", "loose.txt"]);

        // Leaving the folder forgets its sizes.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert!(app.active_panel().dir_sizes.is_empty());
    }

    #[test]
    fn editor_settings_with_spaces() {
        let tmp = TempDir::new("app-editor");
        let program = tmp.file("My Editor/edit.exe", "");
        let unquoted = program.display().to_string();
        assert_eq!(parse_command(&unquoted), [unquoted.clone()]);
        assert_eq!(parse_command(&format!("\"{unquoted}\" -R")), [unquoted, "-R".into()]);
        assert_eq!(parse_command("nvim -R"), ["nvim", "-R"]);
    }

    #[test]
    fn cd_parsing() {
        assert_eq!(cd_argument("cd"), Some(""));
        assert_eq!(cd_argument("cd  e:"), Some("e:"));
        assert_eq!(cd_argument("CD.."), Some(".."));
        assert_eq!(cd_argument("cd\\"), Some("\\"));
        assert_eq!(cd_argument("cdx"), None);
        assert_eq!(cd_argument("dir"), None);
    }

    #[test]
    fn keyboard_flow_mkdir_navigate_copy_delete() {
        let left = TempDir::new("app-left");
        let right = TempDir::new("app-right");
        left.file("readme.txt", "hello");
        let mut app = test_app(left.path(), right.path());

        // F7 makes a directory and puts the cursor on it.
        app.on_key(key(KeyCode::F(7)));
        typing(&mut app, "projects");
        app.on_key(key(KeyCode::Enter));
        assert!(left.path().join("projects").is_dir());
        assert_eq!(app.active_panel().current().unwrap().name, "projects");

        // Enter goes in, Backspace comes back with the cursor on the directory.
        app.on_key(key(KeyCode::Enter));
        assert!(app.active_panel().cwd.ends_with("projects"));
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.active_panel().current().unwrap().name, "projects");

        // Mark readme.txt with Insert and copy it to the other panel with F5 + Enter.
        app.on_key(key(KeyCode::End));
        app.on_key(key(KeyCode::Insert));
        app.on_key(key(KeyCode::F(5)));
        app.on_key(key(KeyCode::Enter));
        wait_for_job(&mut app);
        assert!(app.dialog.is_none(), "no error dialog");
        assert_eq!(fs::read_to_string(right.path().join("readme.txt")).unwrap(), "hello");

        // Tab to the right panel, Shift+F8 then y deletes permanently.
        app.on_key(key(KeyCode::Tab));
        let active = app.active;
        assert!(app.panels[active].select_name("readme.txt"));
        app.on_key(with_mods(KeyCode::F(8), KeyModifiers::SHIFT));
        app.on_key(key(KeyCode::Char('y')));
        wait_for_job(&mut app);
        assert!(!right.path().join("readme.txt").exists());
    }

    #[test]
    fn command_line_cd_and_history() {
        let tmp = TempDir::new("app-cmd");
        tmp.dir("sub dir");
        let mut app = test_app(tmp.path(), tmp.path());

        typing(&mut app, "cd \"sub dir\"");
        app.on_key(key(KeyCode::Enter));
        assert!(app.active_panel().cwd.ends_with("sub dir"));

        typing(&mut app, "cd..");
        app.on_key(key(KeyCode::Enter));
        assert!(util::same_path(&app.active_panel().cwd, tmp.path()));
        assert!(app.cmdline.is_empty());
        assert!(app.subshell.is_none(), "cd is handled by cm itself");

        // Alt+P brings commands back.
        app.on_key(with_mods(KeyCode::Char('p'), KeyModifiers::ALT));
        assert_eq!(app.cmdline.text(), "cd..");
    }

    #[test]
    fn panel_follows_the_one_off_shell() {
        let tmp = TempDir::new("app-shell");
        let sub = tmp.dir("sub");
        let mut app = test_app(tmp.path(), tmp.path());
        app.after_external(Ok(()), Some(sub.clone()));
        assert!(util::same_path(&app.active_panel().cwd, &sub));
        // A folder that no longer exists is ignored.
        app.after_external(Ok(()), Some(tmp.path().join("gone")));
        assert!(util::same_path(&app.active_panel().cwd, &sub));
    }

    fn wait_until(what: &str, app: &mut App, mut done: impl FnMut(&mut App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(app) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
            app.tick();
        }
    }

    fn shell_screen(app: &App) -> String {
        app.subshell.as_ref().and_then(|s| s.with_screen(|screen| screen.contents())).unwrap_or_default()
    }

    fn shell_dir(app: &App) -> Option<PathBuf> {
        app.subshell.as_ref().and_then(Subshell::report).map(|report| report.dir)
    }

    #[test]
    fn one_shell_for_the_whole_session() {
        if !matches!(Shell::detect(), Shell::PowerShell(_)) {
            eprintln!("no PowerShell here; skipped");
            return;
        }
        let tmp = TempDir::new("app-subshell");
        let sub = tmp.dir("Účty");
        let mut app = test_app(tmp.path(), tmp.path());

        // A command typed in cm runs in the shell, which comes up to show it.
        typing(&mut app, "$cmX = 'kept'; \"out=$(6*7)\"");
        app.on_key(key(KeyCode::Enter));
        assert!(app.shell_shown);
        wait_until("the output", &mut app, |app| shell_screen(app).contains("out=42"));

        // Keys go to the shell; cd there, then Ctrl+O: the panel follows.
        wait_until("the prompt", &mut app, |app| app.subshell.as_ref().is_some_and(Subshell::is_idle));
        typing(&mut app, "cd Účty");
        app.on_key(key(KeyCode::Enter));
        wait_until("the shell's cd", &mut app, |app| shell_dir(app).is_some_and(|d| util::same_path(&d, &sub)));
        app.on_key(ctrl('o'));
        assert!(!app.shell_shown);
        assert!(util::same_path(&app.active_panel().cwd, &sub));

        // Move the panel up; Ctrl+O takes the same shell (variable intact) there.
        app.on_key(key(KeyCode::Backspace));
        app.on_key(ctrl('o'));
        wait_until("cm's cd", &mut app, |app| shell_dir(app).is_some_and(|d| util::same_path(&d, tmp.path())));
        typing(&mut app, "\"var=$cmX\"");
        app.on_key(key(KeyCode::Enter));
        wait_until("the variable", &mut app, |app| shell_screen(app).contains("var=kept"));

        // `exit` ends it: back to the panels, and the next Ctrl+O would start a new shell.
        wait_until("the prompt", &mut app, |app| app.subshell.as_ref().is_some_and(Subshell::is_idle));
        typing(&mut app, "exit");
        app.on_key(key(KeyCode::Enter));
        wait_until("the shell to end", &mut app, |app| app.subshell.is_none());
        assert!(!app.shell_shown);
    }

    #[test]
    fn f3_and_f4_hand_files_to_the_editor() {
        let tmp = TempDir::new("app-edit");
        let file = tmp.file("notes.md", "# hi");
        let mut app = test_app(tmp.path(), tmp.path());
        app.on_key(key(KeyCode::End));
        app.on_key(key(KeyCode::F(4)));
        assert_eq!(app.external.take(), Some(External::Edit(file.clone(), None)));
        app.on_key(key(KeyCode::F(3)));
        assert_eq!(app.external.take(), Some(External::View(file, None)));
    }

    #[test]
    fn f10_quits_at_once_unless_the_settings_say_to_ask() {
        let tmp = TempDir::new("app-quit");
        let mut app = test_app(tmp.path(), tmp.path());
        app.on_key(key(KeyCode::F(10)));
        assert!(app.quit, "no question by default");

        let mut app = test_app(tmp.path(), tmp.path());
        app.config.settings.confirm_quit = true;
        app.on_key(key(KeyCode::F(10)));
        assert!(!app.quit);
        assert!(matches!(app.dialog, Some(Dialog::Confirm { .. })));
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.quit);
    }

    #[test]
    fn alt_f7_finds_text_and_goes_to_it() {
        let tmp = TempDir::new("app-find");
        tmp.file("src/lib.rs", "pub fn alpha() {}\n\npub fn beta() {}\n");
        tmp.file("notes.txt", "nothing here\n");
        let mut app = test_app(tmp.path(), tmp.path());
        app.on_key(with_mods(KeyCode::F(7), KeyModifiers::ALT));
        assert!(app.find_shown);
        typing(&mut app, "fn beta");
        wait_until("the results", &mut app, |app| {
            app.finder.as_ref().is_some_and(|f| !f.working() && f.rows() == 2)
        });
        let finder = app.finder.as_ref().unwrap();
        assert_eq!(finder.files[0].hits[0].number, 3);

        // F4 opens it at the line; the Find window stays.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::F(4)));
        let lib = tmp.path().join("src").join("lib.rs");
        assert_eq!(app.external.take(), Some(External::Edit(lib.clone(), Some(3))));
        assert!(app.find_shown);

        // Enter: the panel goes there, Find closes; Alt+F7 brings it back as it was.
        app.on_key(key(KeyCode::Enter));
        assert!(!app.find_shown);
        assert!(app.active_panel().cwd.ends_with("src"));
        assert_eq!(app.active_panel().current().unwrap().name, "lib.rs");
        app.on_key(with_mods(KeyCode::F(7), KeyModifiers::ALT));
        let finder = app.finder.as_ref().unwrap();
        assert_eq!(finder.text.text(), "fn beta", "the query is kept");
        assert!(util::same_path(&finder.root, &tmp.path().join("src")), "and runs in the panel's new folder");

        // Tab to Files: names alone.
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Esc));
        assert!(!app.find_shown);
    }

    #[test]
    fn options_editor_changes_applies_and_saves_settings() {
        let tmp = TempDir::new("app-options");
        let file = tmp.path().join("cm").join("settings.ini");
        let config = Config { settings_file: Some(file.clone()), ..crate::testutil::config() };
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), config);

        app.on_key(ctrl('p'));
        assert!(matches!(app.dialog, Some(Dialog::Options { cursor: 0, .. })));
        // Checkboxes: Enter or Space toggles, and it counts at once.
        app.on_key(key(KeyCode::Enter));
        assert!(app.config.settings.confirm_quit);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.panels[0].show_hidden && !app.panels[1].show_hidden);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.mouse);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.config.settings.arrows_navigate);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.config.settings.natural_sort);
        app.on_key(key(KeyCode::Char(' ')));
        assert!(app.config.settings.natural_sort);
        // Git status (off in tests): on, and off again.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(app.config.settings.git_status);
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.config.settings.git_status);
        // + and - take folders too (on): files only.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.config.settings.mark_folders);
        // user@host on the command line (on): off.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char(' ')));
        assert!(!app.config.settings.prompt_host);
        // The colour scheme isn't typed: letters do nothing there (Enter opens the list).
        app.on_key(key(KeyCode::Down));
        typing(&mut app, "x");
        assert!(matches!(app.dialog, Some(Dialog::Options { .. })) && app.config.settings.scheme == "classic");

        // Text rows: type, and it counts when the cursor moves on.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Backspace));
        typing(&mut app, "25");
        assert_eq!(app.config.big_file, BIG_FILE, "not yet");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.config.big_file, 25 * 1024 * 1024);
        typing(&mut app, "micro");
        app.on_key(key(KeyCode::Esc));
        assert!(app.dialog.is_none());
        assert_eq!(app.config.editor, ["micro"]);
        assert_eq!(app.config.viewer, ["micro"], "an empty viewer follows the editor");

        // F10 now asks; and it was all saved.
        app.on_key(key(KeyCode::F(10)));
        assert!(!app.quit && matches!(app.dialog, Some(Dialog::Confirm { .. })));
        let saved = Settings::load(Some(&file)).unwrap();
        assert_eq!(saved, app.config.settings);
        assert_eq!((saved.big_file_mb, saved.editor.as_str(), saved.mouse), (25, "micro", false));
    }

    /// + and - take folders as well as files, unless the option says files only; a slash at
    /// the end of the pattern takes folders only either way.
    #[test]
    fn plus_and_minus_take_folders_unless_the_option_says_files_only() {
        let tmp = TempDir::new("app-plus");
        tmp.file("notes.txt", "");
        tmp.file("main.rs", "");
        tmp.dir("src");
        tmp.dir("old.txt");
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), crate::testutil::config());
        app.reload_panels();
        let marked = |app: &App| {
            let mut names: Vec<String> = app.active_panel().marked.iter().cloned().collect();
            names.sort();
            names
        };
        let pattern = |app: &mut App, start: char, text: &str| {
            app.on_key(key(KeyCode::Char(start)));
            app.on_key(key(KeyCode::Backspace));
            typing(app, text);
            app.on_key(key(KeyCode::Enter));
        };

        app.on_key(key(KeyCode::Char('+')));
        let Some(Dialog::Input { prompt, .. }) = &app.dialog else { panic!("no dialog") };
        assert_eq!(prompt, "Mark files and folders (*/ folders only):");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(marked(&app), ["main.rs", "notes.txt", "old.txt", "src"], "* takes everything");
        pattern(&mut app, '-', "*.txt");
        assert_eq!(marked(&app), ["main.rs", "src"], "the folder old.txt too");
        pattern(&mut app, '-', "*/");
        assert_eq!(marked(&app), ["main.rs"], "folders only");

        // Files only, as in mc.
        app.config.settings.mark_folders = false;
        pattern(&mut app, '+', "*.txt");
        assert_eq!(marked(&app), ["main.rs", "notes.txt"]);
        app.on_key(key(KeyCode::Char('-')));
        let Some(Dialog::Input { prompt, .. }) = &app.dialog else { panic!("no dialog") };
        assert_eq!(prompt, "Unmark files matching (*/ for folders):");
        app.on_key(key(KeyCode::Esc));
        pattern(&mut app, '+', "*.txt/");
        assert_eq!(marked(&app), ["main.rs", "notes.txt", "old.txt"], "a slash: the folder, files only or not");
    }

    /// A config whose settings (loaded from there, as cm starts) and schemes live in `dir`.
    fn scheme_config(dir: &Path) -> Config {
        let cm = dir.join("cm");
        let file = cm.join("settings.ini");
        let settings = Settings { git_status: false, ..Settings::load(Some(&file)).unwrap() };
        Config { settings, settings_file: Some(file), schemes_dir: Some(cm.join("schemes")), ..crate::testutil::config() }
    }

    /// Alt+C: moving through the list shows each scheme; Esc brings back the one before, Enter
    /// keeps the one under the cursor, saved for next time.
    #[test]
    fn alt_c_shows_each_scheme_and_keeps_the_one_chosen() {
        let tmp = TempDir::new("app-schemes");
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), scheme_config(tmp.path()));
        assert_eq!(app.scheme, scheme::classic());
        app.on_key(alt(KeyCode::Char('c')));
        assert!(matches!(&app.dialog, Some(Dialog::Schemes { cursor: 0, back: None, .. })));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.scheme, scheme::built_in("classic-v2").unwrap(), "shown at once");
        assert_eq!(app.config.settings.scheme, "classic", "but not kept yet");
        app.on_key(key(KeyCode::Esc));
        assert!(app.dialog.is_none());
        assert_eq!(app.scheme, scheme::classic());

        // A letter jumps to the next name with it; Enter keeps it.
        app.on_key(alt(KeyCode::Char('c')));
        typing(&mut app, "n");
        assert_eq!(app.scheme, scheme::built_in("nord").unwrap());
        app.on_key(key(KeyCode::Enter));
        assert!(app.dialog.is_none());
        assert_eq!(app.scheme, scheme::built_in("nord").unwrap());
        assert_eq!(Settings::load(app.config.settings_file.as_deref()).unwrap().scheme, "nord");
        // Next time cm starts in it, with the list's cursor on it.
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), scheme_config(tmp.path()));
        assert_eq!(app.scheme, scheme::built_in("nord").unwrap());
        app.on_key(alt(KeyCode::Char('c')));
        let Some(Dialog::Schemes { cursor, choices, .. }) = &app.dialog else { panic!("no list") };
        assert_eq!(choices[*cursor].name, "nord");
        // A click picks a row.
        app.on_key(key(KeyCode::Down));
        let hit = crate::mouse::DialogHit {
            rows: Some((ratatui::layout::Rect::new(10, 5, 40, 13), 0)),
            area: ratatui::layout::Rect::new(9, 4, 42, 20),
            ..Default::default()
        };
        app.hit.dialog = Some(hit);
        app.on_mouse(ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
            column: 12,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.dialog.is_none());
        assert_eq!((app.config.settings.scheme.as_str(), &app.scheme), ("classic", &scheme::classic()));
    }

    /// F4 in the list writes a built-in scheme out as a file and opens it in the editor; back
    /// from there the file counts at once.
    #[test]
    fn f4_in_the_scheme_list_edits_a_file_that_counts_when_back() {
        let tmp = TempDir::new("app-scheme-edit");
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), scheme_config(tmp.path()));
        app.on_key(alt(KeyCode::Char('c')));
        typing(&mut app, "n");
        app.on_key(key(KeyCode::F(4)));
        let file = tmp.path().join("cm").join("schemes").join("nord.ini");
        assert_eq!(app.external.take(), Some(External::Edit(file.clone(), None)));
        // What the user does in the editor.
        let text = fs::read_to_string(&file).unwrap().replace("\nheading ", "\nheading = red ;").replace("\nmarked ", "\nmarked = blu ;");
        fs::write(&file, text).unwrap();
        app.after_external(Ok(()), None);
        let Some(Dialog::Schemes { cursor, choices, .. }) = &app.dialog else { panic!("the list is still open") };
        assert_eq!((choices[*cursor].name.as_str(), choices[*cursor].file.as_deref()), ("nord", Some(file.as_path())));
        assert_eq!(choices[*cursor].problems.len(), 1, "{:?}", choices[*cursor].problems);
        assert_eq!(app.scheme.heading, ratatui::style::Style::new().red());
        assert_eq!(app.scheme.marked, scheme::built_in("nord").unwrap().marked, "the line it couldn't read");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.config.settings.scheme, "nord");
        assert_eq!(app.scheme.heading, ratatui::style::Style::new().red());
    }

    /// F4 just to look: the file F4 wrote, closed as it was, goes again, so it doesn't pin
    /// the scheme. A changed one stays, and so does one that was there before F4.
    #[test]
    fn a_scheme_file_left_as_f4_wrote_it_goes_again() {
        let tmp = TempDir::new("app-scheme-untouched");
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), scheme_config(tmp.path()));
        let file = tmp.path().join("cm").join("schemes").join("nord.ini");
        app.on_key(alt(KeyCode::Char('c')));
        typing(&mut app, "n");
        app.on_key(key(KeyCode::F(4)));
        assert_eq!(app.external.take(), Some(External::Edit(file.clone(), None)));
        assert!(file.exists());
        app.after_external(Ok(()), None);
        assert!(!file.exists(), "nothing was changed in it");
        let Some(Dialog::Schemes { cursor, choices, .. }) = &app.dialog else { panic!("the list is still open") };
        assert_eq!((choices[*cursor].name.as_str(), choices[*cursor].file.as_deref()), ("nord", None));
        assert_eq!(app.scheme, scheme::built_in("nord").unwrap());

        // Changed in the editor: it stays.
        app.on_key(key(KeyCode::F(4)));
        app.external.take();
        fs::write(&file, fs::read_to_string(&file).unwrap().replace("\nheading ", "\nheading = red ;")).unwrap();
        app.after_external(Ok(()), None);
        assert!(file.exists());
        // There before F4: it stays even when nothing changes.
        app.on_key(key(KeyCode::F(4)));
        assert_eq!(app.external.take(), Some(External::Edit(file.clone(), None)));
        app.after_external(Ok(()), None);
        assert!(file.exists());
        assert_eq!(app.scheme.heading, ratatui::style::Style::new().red());
    }

    /// The options' Colour scheme row opens the list, which comes back to the options with
    /// the scheme chosen in it.
    #[test]
    fn the_options_pick_a_scheme_from_the_list() {
        let tmp = TempDir::new("app-options-scheme");
        let mut app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), scheme_config(tmp.path()));
        app.on_key(ctrl('p'));
        let row = options::ROWS.iter().position(|r| r.is_choice()).unwrap();
        for _ in 0..row {
            app.on_key(key(KeyCode::Down));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(&app.dialog, Some(Dialog::Schemes { back: Some(_), .. })));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        let Some(Dialog::Options { cursor, settings, .. }) = &app.dialog else { panic!("not back in the options") };
        assert_eq!((*cursor, settings.scheme.as_str()), (row, "classic-v2"));
        // Toggling something else there keeps the scheme.
        app.on_key(key(KeyCode::PageUp));
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(app.config.settings.scheme, "classic-v2");
        assert_eq!(app.scheme, scheme::built_in("classic-v2").unwrap());
        // Esc in the list goes back to the options too, with nothing changed.
        for _ in 0..row {
            app.on_key(key(KeyCode::Down));
        }
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(&app.dialog, Some(Dialog::Options { .. })));
        assert_eq!(app.scheme, scheme::built_in("classic-v2").unwrap());
    }

    /// A scheme that isn't there (a file deleted): classic, and a word about it. NO_COLOR:
    /// mono, whatever the settings say.
    #[test]
    fn a_missing_scheme_is_classic_and_no_color_is_mono() {
        let tmp = TempDir::new("app-scheme-missing");
        let mut config = scheme_config(tmp.path());
        config.settings.scheme = "mine".into();
        let app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), config);
        assert_eq!(app.scheme, scheme::classic());
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("no colour scheme \"mine\"")), "{:?}", app.notice);
        let mut config = scheme_config(tmp.path());
        config.settings.scheme = "nord".into();
        config.colours.off = true;
        let app = App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), config);
        assert_eq!(app.scheme, scheme::mono());
    }

    /// Shift+F6 to another letter case where that names another file (a folder that respects
    /// case, as WSL makes them): refused, and both files stay. Where the two names are one
    /// file (the usual Windows folder), it renames.
    #[test]
    fn a_rename_never_lands_on_another_file_that_differs_in_case() {
        let tmp = TempDir::new("app-rename-case");
        let dir = tmp.dir("strict");
        #[cfg(windows)]
        {
            let made = std::process::Command::new("fsutil").args(["file", "setCaseSensitiveInfo"]).arg(&dir).arg("enable").output().unwrap();
            if !made.status.success() {
                return;
            }
        }
        let lower = tmp.file("strict/readme.md", "lower");
        let upper = tmp.file("strict/README.md", "upper");
        let mut app = test_app(&dir, &dir);
        app.rename(&lower, "README.md");
        assert!(matches!(app.dialog, Some(Dialog::Message { .. })), "an error: it exists");
        assert_eq!((fs::read_to_string(&lower).unwrap(), fs::read_to_string(&upper).unwrap()), ("lower".into(), "upper".into()));

        #[cfg(windows)]
        {
            let one = tmp.file("Plain.txt", "one");
            let mut app = test_app(tmp.path(), tmp.path());
            app.rename(&one, "PLAIN.txt");
            assert!(app.dialog.is_none(), "renamed: {:?}", app.dialog.is_some());
            assert!(fs::read_dir(tmp.path()).unwrap().flatten().any(|e| e.file_name() == "PLAIN.txt"));
        }
    }

    /// F2 and Enter on `foo.` beside `foo`: no rename, and above all none onto `foo`. Another
    /// name typed with a dot at the end is that name without it, as in Explorer.
    #[cfg(windows)]
    #[test]
    fn renaming_a_name_ending_in_a_dot_spares_the_one_without() {
        let tmp = TempDir::new("rename-dot");
        let plain = tmp.file("foo", "plain");
        let dotted = tmp.path().join("foo.");
        let literal = lfs::to_literal(&dotted).unwrap();
        fs::write(&literal, "dot").unwrap();
        let mut app = test_app(tmp.path(), tmp.path());
        app.rename(&dotted, "foo.");
        assert!(app.dialog.is_none());
        assert_eq!((fs::read_to_string(&plain).unwrap(), fs::read_to_string(&literal).unwrap()), ("plain".into(), "dot".into()));

        app.rename(&dotted, "FOO.");
        assert!(app.dialog.is_none());
        assert_eq!(fs::read_to_string(&plain).unwrap(), "plain", "only the letter case");
        assert!(fs::read_dir(tmp.path()).unwrap().flatten().any(|e| e.file_name() == "FOO."));

        app.rename(&tmp.path().join("FOO."), "bar.");
        assert!(app.dialog.is_none());
        assert_eq!(fs::read_to_string(tmp.path().join("bar")).unwrap(), "dot");

        let other = tmp.file("other", "other");
        app.rename(&other, "foo.");
        assert!(matches!(app.dialog, Some(Dialog::Message { .. })), "`foo` is there");
        assert_eq!(fs::read_to_string(&plain).unwrap(), "plain");
    }

    /// New names typed with a dot at the end are made as Windows makes them, and are found.
    #[cfg(windows)]
    #[test]
    fn new_names_are_what_windows_makes_them() {
        let tmp = TempDir::new("new-dot");
        let mut app = test_app(tmp.path(), tmp.path());
        app.mkdir(tmp.path(), "folder.");
        assert!(tmp.path().join("folder").is_dir());
        assert_eq!(app.active_panel().current().map(|e| e.name.as_str()), Some("folder"));
        app.new_file(tmp.path(), "report.");
        assert!(tmp.path().join("report").is_file());
        assert!(matches!(app.external, Some(External::Edit(ref path, _)) if path.ends_with("report")));
    }

    /// Enter pressed twice, or held: the overwrite question that opens under it isn't answered
    /// by a key that was on its way. Each such key puts the wait back; then Enter counts.
    #[test]
    fn an_overwrite_question_isnt_answered_by_a_stray_key() {
        let tmp = TempDir::new("app-stray-enter");
        let dir_a = tmp.dir("a");
        let dir_b = tmp.dir("b");
        tmp.file("a/x.txt", "new");
        let old = tmp.file("b/x.txt", "old");
        let mut app = test_app(&dir_a, &dir_b);
        app.answer_delay = std::time::Duration::from_secs(60);
        app.active_panel_mut().select_name("x.txt");
        app.start_job(Job::Copy(vec![(dir_a.join("x.txt"), old.clone())]));
        while !matches!(app.dialog, Some(Dialog::Question { .. })) {
            app.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for _ in 0..3 {
            app.on_key(key(KeyCode::Enter));
        }
        assert!(matches!(app.dialog, Some(Dialog::Question { .. })), "still asking");
        assert_eq!(fs::read_to_string(&old).unwrap(), "old");
        app.overwrite_asked = None;
        app.on_key(key(KeyCode::Enter));
        wait_for_job(&mut app);
        assert_eq!(fs::read_to_string(&old).unwrap(), "new");
    }

    /// A job caught in a system call that doesn't return: Esc can't reach it, so after a while
    /// F10 leaves cm. Before that F10 does nothing, as during any job.
    #[test]
    fn a_job_that_doesnt_stop_can_be_left() {
        let tmp = TempDir::new("app-stuck");
        let mut app = test_app(tmp.path(), tmp.path());
        let (_tx, rx) = std::sync::mpsc::channel();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.job = Some(Running::new("Copy", ops::Progress::default(), rx, cancel));
        app.on_key(key(KeyCode::F(10)));
        assert!(!app.quit);
        app.on_key(key(KeyCode::Esc));
        assert!(app.job.as_ref().is_some_and(|job| job.stopping.is_some() && !job.stuck()));
        app.on_key(key(KeyCode::F(10)));
        assert!(!app.quit, "it has only just been asked to stop");
        let long_ago = Instant::now() - std::time::Duration::from_secs(60);
        app.job.as_mut().unwrap().stopping = Some(long_ago);
        assert!(app.tick(), "the screen says so");
        app.on_key(key(KeyCode::F(10)));
        assert!(app.quit);
    }

    /// A job uses up the marks of the panel it started from. After Shift+F2 the other side's
    /// marks wait for their own F5; looking at a file in a zip touches none of them.
    #[test]
    fn marks_stay_until_their_own_job() {
        let tmp = TempDir::new("app-marks");
        let (left, right) = (tmp.dir("left"), tmp.dir("right"));
        tmp.file("left/only-left.txt", "l");
        tmp.file("right/only-right.txt", "r");
        let mut app = test_app(&left, &right);
        app.on_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::SHIFT));
        assert!(!app.panels[0].marked.is_empty() && !app.panels[1].marked.is_empty());
        app.on_key(key(KeyCode::F(5)));
        app.on_key(key(KeyCode::Enter));
        wait_for_job(&mut app);
        assert!(right.join("only-left.txt").exists());
        assert!(app.panels[0].marked.is_empty(), "these went");
        assert!(!app.panels[1].marked.is_empty(), "those wait for F5 on their side");
    }

    /// F3 on a file in a zip: its copy is gone when the viewer closes, and the marks stay.
    #[test]
    fn a_file_viewed_from_a_zip_leaves_nothing_behind() {
        let tmp = TempDir::new("app-zip-view");
        let zip = tmp.path().join("box.zip");
        crate::archive::tests::sample_zip(&zip);
        let mut app = test_app(tmp.path(), tmp.path());
        app.active_panel_mut().select_name("box.zip");
        app.open_current();
        app.active_panel_mut().select_name("top.txt");
        app.on_key(key(KeyCode::Insert));
        assert!(!app.active_panel().marked.is_empty());
        app.active_panel_mut().select_name("top.txt");
        app.on_key(key(KeyCode::F(3)));
        wait_for_job(&mut app);
        assert!(!app.active_panel().marked.is_empty(), "the marks stay");
        let Some(External::View(copy, _)) = app.external.take() else { panic!("the viewer opens") };
        assert!(copy.exists());
        app.after_external(Ok(()), None);
        assert!(!copy.exists() && !copy.parent().unwrap().exists());
    }

    /// Alt+V goes through the checks of F5, links seen through: pasting a folder into itself
    /// by way of a link to it is refused.
    #[test]
    fn a_paste_sees_through_links() {
        let tmp = TempDir::new("app-paste-alias");
        let project = tmp.dir("project");
        let backup = tmp.dir("project/backup");
        let alias = tmp.path().join("alias");
        #[cfg(windows)]
        {
            let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&alias).arg(&project).output().unwrap();
            assert!(made.status.success());
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&project, &alias).unwrap();
        let mut app = test_app(&alias.join("backup"), &backup);
        app.clipboard.put(std::slice::from_ref(&project), Effect::Copy).unwrap();
        app.paste();
        assert!(matches!(app.dialog, Some(Dialog::Message { .. })), "refused");
        assert!(app.job.is_none());
        #[cfg(windows)]
        fs::remove_dir(&alias).unwrap();
    }

    /// Cut, pasted, answered No: the files are still where they were, so they stay on the
    /// clipboard. Moved: it is emptied.
    #[test]
    fn the_clipboard_is_emptied_only_when_the_cut_files_moved() {
        let tmp = TempDir::new("app-cut-no");
        let (from, to) = (tmp.dir("from"), tmp.dir("to"));
        let file = tmp.file("from/x.txt", "new");
        tmp.file("to/x.txt", "old");
        let mut app = test_app(&to, &from);
        app.clipboard.put(std::slice::from_ref(&file), Effect::Move).unwrap();
        app.paste();
        while !matches!(app.dialog, Some(Dialog::Question { .. })) {
            app.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        app.on_key(key(KeyCode::Char('n')));
        wait_for_job(&mut app);
        assert!(file.exists());
        assert!(app.clipboard.files().is_some(), "still cut");

        fs::remove_file(to.join("x.txt")).unwrap();
        app.paste();
        wait_for_job(&mut app);
        assert!(!file.exists());
        assert!(app.clipboard.files().is_none(), "moved");
    }

    /// F6 to a new name in the same folder: the cursor goes along with the file.
    #[test]
    fn a_rename_by_f6_keeps_the_cursor_on_the_file() {
        let tmp = TempDir::new("app-f6-cursor");
        tmp.file("a.txt", "a");
        tmp.file("m.txt", "m");
        let mut app = test_app(tmp.path(), tmp.path());
        app.active_panel_mut().select_name("a.txt");
        app.on_input(InputAction::Move(vec![tmp.path().join("a.txt")]), "z.txt".into());
        wait_for_job(&mut app);
        assert_eq!(app.active_panel().current().map(|e| e.name.as_str()), Some("z.txt"));
    }

    /// Hidden files shown or not: the tabs in the background follow too.
    #[test]
    fn showing_hidden_files_reaches_every_tab() {
        let tmp = TempDir::new("app-hidden-tabs");
        let mut app = test_app(tmp.path(), tmp.path());
        app.on_key(ctrl('t'));
        app.on_key(ctrl('t'));
        assert_eq!(app.tabs[app.active].len(), 2);
        let before = app.panels[0].show_hidden;
        app.toggle_hidden();
        assert!(app.panels.iter().chain(app.tabs.iter().flatten()).all(|p| p.show_hidden == !before));
    }

    #[test]
    fn big_files_go_to_the_pager_and_the_editor_asks_first() {
        let tmp = TempDir::new("app-big");
        let big = tmp.path().join("huge.log");
        fs::File::create(&big).unwrap().set_len(BIG_FILE + 1).unwrap();
        let mut app = test_app(tmp.path(), tmp.path());
        assert!(app.active_panel_mut().select_name("huge.log"));

        // F4 asks, and Enter on the default answer (No) leaves it alone.
        app.on_key(key(KeyCode::F(4)));
        assert!(matches!(app.dialog, Some(Dialog::Confirm { focus: 1, .. })));
        app.on_key(key(KeyCode::Enter));
        assert!(app.dialog.is_none());
        assert_eq!(app.external, None);
        // Y opens it after all.
        app.on_key(key(KeyCode::F(4)));
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.external.take(), Some(External::Edit(big.clone(), None)));

        // F3 shows it in the pager; without a pager it asks too.
        app.config.pager = Some(vec!["less".into()]);
        app.on_key(key(KeyCode::F(3)));
        assert_eq!(app.external.take(), Some(External::Page(big.clone(), None)));
        app.config.pager = None;
        app.on_key(key(KeyCode::F(3)));
        assert!(matches!(app.dialog, Some(Dialog::Confirm { .. })));
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.external.take(), Some(External::View(big, None)));
    }

    #[test]
    fn quick_search_and_rename() {
        let tmp = TempDir::new("app-rename");
        tmp.file("alpha.txt", "");
        tmp.file("beta.txt", "");
        let mut app = test_app(tmp.path(), tmp.path());
        app.on_key(with_mods(KeyCode::Char('s'), KeyModifiers::CONTROL));
        typing(&mut app, "be");
        assert_eq!(app.active_panel().current().unwrap().name, "beta.txt");
        app.on_key(key(KeyCode::Esc));

        app.on_key(key(KeyCode::F(2)));
        for _ in 0.."beta.txt".len() {
            app.on_key(key(KeyCode::Backspace));
        }
        typing(&mut app, "Gamma.txt");
        app.on_key(key(KeyCode::Enter));
        assert!(tmp.path().join("Gamma.txt").exists());
        assert_eq!(app.active_panel().current().unwrap().name, "Gamma.txt");
    }

    /// A panel on another computer: a real server program, without `ssh`.
    struct OverThere {
        there: TempDir,
        here: TempDir,
        connection: Arc<Connection>,
        app: App,
    }

    fn over_there(tag: &str) -> Option<OverThere> {
        let there = TempDir::new(&format!("{tag}-there"));
        let here = TempDir::new(&format!("{tag}-here"));
        let connection = crate::ssh::tests::local_connection(there.path())?;
        let mut app = test_app(here.path(), here.path());
        app.on_key(ctrl('n'));
        typing(&mut app, &connection.target.place(&connection.home));
        app.on_key(key(KeyCode::Enter));
        assert!(app.dialog.is_none(), "connected");
        assert!(app.active_panel().remote.is_some());
        Some(OverThere { there, here, connection, app })
    }

    fn names(app: &App) -> Vec<String> {
        app.active_panel().entries.iter().map(|e| e.name.clone()).collect()
    }

    fn field(app: &App) -> String {
        match &app.dialog {
            Some(Dialog::Input { edit, .. }) => edit.text().to_string(),
            _ => panic!("no field"),
        }
    }

    /// Browsing another computer: its folders open, `..` goes up and from the top back here,
    /// `cd` goes by its paths, and the address shows where the panel is.
    #[test]
    fn a_panel_on_another_computer_is_browsed_like_any_other() {
        let Some(mut over) = over_there("app-ssh-browse") else { return };
        over.there.file("folder/inner.txt", "inner");
        over.there.file("report.txt", "report");
        over.there.file(".hidden", "h");
        let home = over.connection.home.clone();
        let app = &mut over.app;
        app.on_key(ctrl('r'));
        assert_eq!(names(app), ["..", "folder", ".hidden", "report.txt"]);
        let shown = |app: &App| app.active_panel().location().to_string_lossy().into_owned();
        assert_eq!(shown(app), over.connection.target.place(&home));
        assert!(app.active_panel().space.is_some(), "the server tells the free space");
        assert!(app.active_panel().repo.is_none());

        app.active_panel_mut().select_name("folder");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(names(app), ["..", "inner.txt"]);
        assert_eq!(shown(app), over.connection.target.place(&format!("{home}/folder")));
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.active_panel().current().unwrap().name, "folder", "the cursor on the folder left");

        // Sorting, filters and hidden files work on what was read.
        app.on_key(ctrl('h'));
        assert_eq!(names(app), ["..", "folder", "report.txt"]);
        app.on_key(ctrl('h'));

        for line in ["cd folder", "cd ..", "cd folder/../folder"] {
            typing(app, line);
            app.on_key(key(KeyCode::Enter));
        }
        assert_eq!(names(app), ["..", "inner.txt"]);
        typing(app, "cd");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(shown(app), over.connection.target.place(&home), "cd alone goes home");
        typing(app, "cd missing");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.dialog, Some(Dialog::Message { .. })), "a folder that isn't there is said");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(shown(app), over.connection.target.place(&home));

        // A command doesn't run here in another folder than the one shown.
        typing(app, "echo hello");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.dialog, Some(Dialog::Message { .. })) && app.external.is_none());
        app.on_key(key(KeyCode::Enter));

        // What needs this computer says so.
        app.on_key(with_mods(KeyCode::F(7), KeyModifiers::ALT));
        assert!(!app.find_shown && app.notice.as_deref().is_some_and(|n| n.contains("another one")), "{:?}", app.notice);

        // Back and forward go across computers, and so do tabs, Alt+I and the history.
        app.on_key(alt(KeyCode::Char('i')));
        assert!(app.panels[1].remote.is_some(), "Alt+I: the other panel comes along");
        assert!(app.history_rows("").is_empty() || !app.history.dirs.is_empty());
        assert!(app.history.dirs.iter().any(|dir| dir.to_string_lossy() == shown(app)), "in the folder history");
        let (local, address) = (app.panels[0].cwd.clone(), shown(app));
        app.on_key(ctrl('t'));
        assert_eq!(shown(app), address, "a new tab stays over there");
        app.on_key(ctrl('w'));

        // Up from the top comes back to this computer.
        typing(app, "cd /");
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Backspace));
        assert!(app.active_panel().remote.is_none());
        assert!(util::same_path(&app.active_panel().cwd, &local));
        app.on_key(alt(KeyCode::Left));
        assert!(app.active_panel().remote.is_some(), "Alt+Left goes back there");
    }

    /// F5 and F6 between a panel here and a panel there, and with an address typed.
    #[test]
    fn copying_between_the_panels_over_ssh() {
        let Some(mut over) = over_there("app-ssh-copy") else { return };
        over.there.file("from-there.txt", "there");
        over.here.file("from-here.txt", "here");
        over.here.file("typed.txt", "typed");
        let home = over.connection.home.clone();
        let app = &mut over.app;
        app.on_key(ctrl('r'));

        // There to here: the field starts on the other panel.
        app.active_panel_mut().select_name("from-there.txt");
        app.on_key(key(KeyCode::F(5)));
        assert_eq!(field(app), util::with_trailing_separator(over.here.path()));
        app.on_key(key(KeyCode::Enter));
        wait_for_job(app);
        assert!(app.dialog.is_none());
        assert_eq!(fs::read_to_string(over.here.path().join("from-there.txt")).unwrap(), "there");

        // Here to there: the field is the address.
        app.on_key(key(KeyCode::Tab));
        app.active_panel_mut().select_name("from-here.txt");
        app.on_key(key(KeyCode::F(6)));
        assert_eq!(field(app), over.connection.target.place(&format!("{home}/")));
        app.on_key(key(KeyCode::Enter));
        wait_for_job(app);
        assert_eq!(fs::read_to_string(over.there.path().join("from-here.txt")).unwrap(), "here");
        assert!(!over.here.path().join("from-here.txt").exists());
        assert!(app.panels[0].entries.iter().any(|e| e.name == "from-here.txt"), "the other panel shows it");

        // With the panels both here: an address in the field goes there, a name stays here.
        app.panels[0].change_dir(over.here.path().to_path_buf());
        app.active_panel_mut().select_name("typed.txt");
        app.on_key(with_mods(KeyCode::F(5), KeyModifiers::SHIFT));
        typing(app, &over.connection.target.place(&format!("{home}/sub/typed copy.txt")));
        app.on_key(key(KeyCode::Enter));
        wait_for_job(app);
        assert_eq!(fs::read_to_string(over.there.path().join("sub/typed copy.txt")).unwrap(), "typed");
        assert!(app.history.inputs.list("name").iter().any(|text| text.contains("typed copy")));
    }

    /// F7, F2, Shift+F5, Shift+F6 and F8 on the other computer. F8 can't go to a trash, and
    /// says so first.
    #[test]
    fn working_on_files_over_ssh() {
        let Some(mut over) = over_there("app-ssh-files") else { return };
        over.there.file("a.txt", "a");
        over.there.file("taken.txt", "taken");
        let app = &mut over.app;
        app.on_key(ctrl('r'));

        app.on_key(key(KeyCode::F(7)));
        typing(app, "new/deep");
        app.on_key(key(KeyCode::Enter));
        assert!(over.there.path().join("new/deep").is_dir());
        assert_eq!(app.active_panel().current().unwrap().name, "new", "the cursor on what was made");
        app.on_key(key(KeyCode::F(7)));
        typing(app, "new");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.dialog, Some(Dialog::Message { .. })), "it is there already");
        app.on_key(key(KeyCode::Enter));

        // F2 leaves what is there alone.
        app.active_panel_mut().select_name("a.txt");
        app.on_key(key(KeyCode::F(2)));
        assert_eq!(field(app), "a.txt");
        app.on_key(ctrl('a'));
        app.on_key(ctrl('k'));
        typing(app, "taken.txt");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.dialog, Some(Dialog::Message { .. })));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(fs::read_to_string(over.there.path().join("taken.txt")).unwrap(), "taken");
        app.on_key(key(KeyCode::F(2)));
        app.on_key(ctrl('a'));
        app.on_key(ctrl('k'));
        typing(app, "b.txt");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.active_panel().current().unwrap().name, "b.txt");

        // In place: the name offered, the new one lands in that folder, the cursor goes along.
        app.on_key(with_mods(KeyCode::F(5), KeyModifiers::SHIFT));
        assert_eq!(field(app), "b.txt");
        typing(app, "c.txt");
        app.on_key(key(KeyCode::Enter));
        wait_for_job(app);
        assert_eq!(fs::read_to_string(over.there.path().join("c.txt")).unwrap(), "a");
        assert_eq!(app.active_panel().current().unwrap().name, "c.txt");
        app.on_key(with_mods(KeyCode::F(6), KeyModifiers::SHIFT));
        typing(app, "d.txt");
        app.on_key(key(KeyCode::Enter));
        wait_for_job(app);
        assert!(!over.there.path().join("c.txt").exists() && over.there.path().join("d.txt").exists());
        assert_eq!(app.active_panel().current().unwrap().name, "d.txt");

        // F8: for good, said so, with No as the answer Enter gives.
        app.on_key(key(KeyCode::F(8)));
        let Some(Dialog::Confirm { lines, focus, danger, .. }) = &app.dialog else { panic!("no question") };
        assert!(lines[0].contains("Permanently delete \"d.txt\"") && lines[1].contains("no trash"), "{lines:?}");
        assert_eq!((*focus, *danger), (1, true));
        app.on_key(key(KeyCode::Enter));
        assert!(over.there.path().join("d.txt").exists());
        app.on_key(key(KeyCode::F(8)));
        app.on_key(key(KeyCode::Char('y')));
        wait_for_job(app);
        assert!(!over.there.path().join("d.txt").exists());

        // The marks: everything marked goes, folders with what is in them.
        app.active_panel_mut().select_name("new");
        app.active_panel_mut().toggle_mark();
        app.active_panel_mut().select_name("taken.txt");
        app.active_panel_mut().toggle_mark();
        app.on_key(key(KeyCode::Delete));
        app.on_key(key(KeyCode::Char('y')));
        wait_for_job(app);
        assert_eq!(fs::read_dir(over.there.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>(), ["b.txt"]);
        assert_eq!(app.active_panel().marked_summary(), None);
    }

    /// F3 brings a copy here for the viewer. F4 brings one for the editor, and a changed file
    /// goes back; unchanged, nothing is written.
    #[test]
    fn viewing_and_editing_over_ssh() {
        let Some(mut over) = over_there("app-ssh-edit") else { return };
        let file = over.there.file("notes.txt", "old notes");
        let before = fs::metadata(&file).unwrap().modified().unwrap();
        let app = &mut over.app;
        app.on_key(ctrl('r'));
        app.active_panel_mut().select_name("notes.txt");

        app.on_key(key(KeyCode::F(3)));
        wait_for_job(app);
        let Some(External::View(path, None)) = app.external.take() else { panic!("no viewer") };
        assert_eq!(fs::read_to_string(&path).unwrap(), "old notes");
        app.after_external(Ok(()), None);
        assert!(!path.exists(), "gone with the viewer");

        // F4, closed without a change.
        app.on_key(key(KeyCode::F(4)));
        wait_for_job(app);
        let Some(External::Edit(path, None)) = app.external.take() else { panic!("no editor") };
        app.after_external(Ok(()), None);
        assert!(app.job.is_none() && !path.exists());
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), before);

        // F4, saved: it goes back.
        app.on_key(key(KeyCode::F(4)));
        wait_for_job(app);
        let Some(External::Edit(path, None)) = app.external.take() else { panic!("no editor") };
        fs::write(&path, "new notes").unwrap();
        app.after_external(Ok(()), None);
        wait_for_job(app);
        assert!(app.dialog.is_none(), "no question: nothing else changed it");
        assert_eq!(fs::read_to_string(&file).unwrap(), "new notes");
        assert!(!path.exists());
    }

    /// The file changes over there while it is in the editor: nothing is replaced unasked, and
    /// the edit survives either answer.
    #[test]
    fn a_file_changed_meanwhile_is_not_simply_replaced() {
        let Some(mut over) = over_there("app-ssh-conflict") else { return };
        let file = over.there.file("notes.txt", "old notes");
        let config = TempDir::new("app-ssh-conflict-config");
        let app = &mut over.app;
        app.on_key(ctrl('r'));
        app.active_panel_mut().select_name("notes.txt");

        let edit = |app: &mut App, text: &str| {
            app.on_key(key(KeyCode::F(4)));
            wait_for_job(app);
            let Some(External::Edit(path, None)) = app.external.take() else { panic!("no editor") };
            fs::write(&path, text).unwrap();
            fs::write(&file, format!("changed there, then {text}")).unwrap();
            app.after_external(Ok(()), None);
        };
        // SAFETY: only this test reads APPDATA or XDG_CONFIG_HOME while it runs, for `rescue`.
        unsafe { std::env::set_var(if cfg!(windows) { "APPDATA" } else { "XDG_CONFIG_HOME" }, config.path()) };
        edit(app, "mine");
        let Some(Dialog::Confirm { lines, focus, .. }) = &app.dialog else { panic!("no question") };
        assert_eq!(*focus, 1, "No is what Enter gives");
        assert!(lines[0].contains("changed on"), "{lines:?}");
        let rescued: Vec<_> = fs::read_dir(config.path().join(util::FOLDER).join("rescued")).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(fs::read_to_string(&rescued[0]).unwrap(), "mine");
        app.on_key(key(KeyCode::Enter));
        wait_for_job(app);
        assert_eq!(fs::read_to_string(over.there.path().join("notes.txt")).unwrap(), "changed there, then mine", "No leaves it");
        assert_eq!(fs::read_to_string(&rescued[0]).unwrap(), "mine", "and mine is kept");

        edit(app, "mine again");
        app.on_key(key(KeyCode::Char('y')));
        wait_for_job(app);
        assert_eq!(fs::read_to_string(over.there.path().join("notes.txt")).unwrap(), "mine again");
    }

    /// The connection goes: the panel comes back to this computer and says so.
    #[test]
    fn a_lost_connection_brings_the_panel_back() {
        let Some(mut over) = over_there("app-ssh-lost") else { return };
        crate::ssh::tests::kill(&over.connection);
        let deadline = Instant::now() + Duration::from_secs(5);
        while over.connection.sftp.lost().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let app = &mut over.app;
        app.on_key(ctrl('r'));
        app.tick();
        assert!(app.active_panel().remote.is_none());
        assert!(util::same_path(&app.active_panel().cwd, over.here.path()));
        let Some(Dialog::Message { lines, .. }) = &app.dialog else { panic!("nothing said") };
        assert!(lines[0].contains("is gone"), "{lines:?}");
    }

    /// A name ending in a space is that name, however it travels: read as typed, it would be
    /// the file beside it.
    #[cfg(unix)]
    #[test]
    fn names_ending_in_a_space_stay_themselves_over_ssh() {
        let Some(mut over) = over_there("app-ssh-space") else { return };
        over.there.file("name", "plain");
        over.there.file("name ", "with a space");
        over.there.file("folder ", "x");
        fs::remove_file(over.there.path().join("folder ")).unwrap();
        over.there.file("folder /inside.txt", "inside");
        over.there.dir("folder");
        let app = &mut over.app;
        app.on_key(ctrl('r'));
        app.active_panel_mut().select_name("name ");
        app.on_key(key(KeyCode::F(6)));
        app.on_key(key(KeyCode::Enter));
        wait_for_job(app);
        assert_eq!(fs::read_to_string(over.here.path().join("name ")).unwrap(), "with a space");
        assert_eq!(fs::read_to_string(over.there.path().join("name")).unwrap(), "plain", "the file beside it stays");

        app.active_panel_mut().select_name("name");
        app.on_key(key(KeyCode::F(2)));
        app.on_key(ctrl('a'));
        app.on_key(ctrl('k'));
        typing(app, "renamed");
        app.on_key(key(KeyCode::Enter));
        assert!(over.there.path().join("renamed").exists());

        app.active_panel_mut().select_name("folder ");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(names(app), ["..", "inside.txt"], "into the folder of that name");
    }

    /// A link over there, for F3 and F4, is what it leads to: nothing of this computer comes into
    /// it, and what is saved reaches the file, with the link still a link.
    #[cfg(unix)]
    #[test]
    fn looking_at_a_link_over_ssh_goes_to_what_it_leads_to() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let Some(mut over) = over_there("app-ssh-link") else { return };
        let real = over.there.file("available/site", "the real one");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o664)).unwrap();
        let enabled = over.there.dir("enabled");
        symlink("../available/site", enabled.join("site")).unwrap();
        let app = &mut over.app;
        app.on_key(ctrl('r'));
        typing(app, "cd enabled");
        app.on_key(key(KeyCode::Enter));
        app.active_panel_mut().select_name("site");

        app.on_key(key(KeyCode::F(3)));
        wait_for_job(app);
        let Some(External::View(path, None)) = app.external.take() else { panic!("no viewer") };
        assert!(!fs::symlink_metadata(&path).unwrap().file_type().is_symlink(), "a copy, not a link here");
        assert_eq!(fs::read_to_string(&path).unwrap(), "the real one");
        app.after_external(Ok(()), None);

        app.active_panel_mut().toggle_mark();
        app.on_key(key(KeyCode::F(4)));
        wait_for_job(app);
        let Some(External::Edit(path, None)) = app.external.take() else { panic!("no editor") };
        fs::write(&path, "edited").unwrap();
        app.after_external(Ok(()), None);
        wait_for_job(app);
        assert!(app.dialog.is_none());
        assert_eq!(fs::read_to_string(&real).unwrap(), "edited");
        assert!(fs::symlink_metadata(enabled.join("site")).unwrap().file_type().is_symlink(), "the link stays");
        assert_eq!(fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o664, "as it was");
        assert_eq!(app.active_panel().marked_summary(), Some((1, 6)), "the marks stay");
    }

    /// Brackets are at home in names, and in addresses after the colon.
    #[test]
    fn names_with_brackets_over_ssh() {
        let Some(mut over) = over_there("app-ssh-brackets") else { return };
        over.there.file("[slug]/report[1].txt", "report");
        let app = &mut over.app;
        app.on_key(ctrl('r'));
        app.active_panel_mut().select_name("[slug]");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(names(app), ["..", "report[1].txt"]);
        app.active_panel_mut().select_name("report[1].txt");
        app.on_key(key(KeyCode::F(5)));
        app.on_key(key(KeyCode::Enter));
        wait_for_job(app);
        assert!(app.dialog.is_none());
        assert_eq!(fs::read_to_string(over.here.path().join("report[1].txt")).unwrap(), "report");
        app.on_key(key(KeyCode::F(2)));
        app.on_key(ctrl('a'));
        app.on_key(ctrl('k'));
        typing(app, "plain.txt");
        app.on_key(key(KeyCode::Enter));
        assert!(over.there.path().join("[slug]/plain.txt").exists());
    }

    /// F2 offers the name selected too: typing replaces it.
    #[test]
    fn f2_replaces_the_name_by_typing() {
        let tmp = TempDir::new("app-f2-selected");
        tmp.file("old name.txt", "x");
        let mut app = test_app(tmp.path(), tmp.path());
        app.active_panel_mut().select_name("old name.txt");
        app.on_key(key(KeyCode::F(2)));
        assert_eq!(field(&app), "old name.txt");
        typing(&mut app, "new.txt");
        app.on_key(key(KeyCode::Enter));
        assert!(tmp.path().join("new.txt").exists() && !tmp.path().join("old name.txt").exists());
    }

    /// The drive picker goes to its ends by Home and End, and by the page keys.
    #[test]
    fn home_and_end_in_the_drive_picker() {
        let tmp = TempDir::new("app-drives");
        let mut app = test_app(tmp.path(), tmp.path());
        app.history.inputs.add("ssh", "first-host");
        app.history.inputs.add("ssh", "second-host");
        let cursor = |app: &App| match &app.dialog {
            Some(Dialog::Drives { cursor, drives, .. }) => (*cursor, drives.len()),
            _ => panic!("no drives"),
        };
        app.on_key(with_mods(KeyCode::F(1), KeyModifiers::ALT));
        let (_, count) = cursor(&app);
        assert!(count >= 3, "a drive and two hosts");
        for (press, at) in [
            (KeyCode::End, count - 1),
            (KeyCode::Home, 0),
            (KeyCode::PageDown, count - 1),
            (KeyCode::PageUp, 0),
        ] {
            app.on_key(key(press));
            assert_eq!(cursor(&app).0, at, "{press:?}");
        }
    }

    /// Shift+F5 and Shift+F6 stay on the panel's computer, also for a whole path. F5 starts on
    /// the other panel, where a whole path is one of this computer.
    #[test]
    fn a_whole_path_in_place_stays_on_the_panels_computer() {
        let Some(mut over) = over_there("app-ssh-whole-path") else { return };
        over.there.file("a.txt", "a");
        let home = over.connection.home.clone();
        let app = &mut over.app;
        app.on_key(ctrl('r'));
        app.active_panel_mut().select_name("a.txt");
        app.on_key(with_mods(KeyCode::F(6), KeyModifiers::SHIFT));
        typing(app, &format!("{home}/moved/b.txt"));
        app.on_key(key(KeyCode::Enter));
        wait_for_job(app);
        assert_eq!(fs::read_to_string(over.there.path().join("moved/b.txt")).unwrap(), "a");
    }

    /// What a dialog says stays until it is read: it may say where a file went. The panel's
    /// news comes after it, and marks by a name of the other computer don't land on this one's.
    #[test]
    fn a_lost_connection_leaves_marks_and_messages_alone() {
        let Some(mut over) = over_there("app-ssh-lost-marks") else { return };
        over.there.file("report.txt", "there");
        over.here.file("report.txt", "here");
        let app = &mut over.app;
        app.on_key(ctrl('r'));
        app.active_panel_mut().select_name("report.txt");
        app.active_panel_mut().toggle_mark();
        crate::ssh::tests::kill(&over.connection);
        let deadline = Instant::now() + Duration::from_secs(5);
        while over.connection.sftp.lost().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        app.dialog = Some(Dialog::Message { title: "Saved".into(), lines: vec!["Your changes are in X".into()], error: false });
        app.reload_panels();
        app.tick();
        assert!(matches!(&app.dialog, Some(Dialog::Message { title, .. }) if title == "Saved"), "not taken away");
        assert_eq!(app.active_panel().marked_summary(), None, "nothing marked here");
        app.on_key(key(KeyCode::Enter));
        app.tick();
        let Some(Dialog::Message { lines, .. }) = &app.dialog else { panic!("nothing said") };
        assert!(lines[0].contains("is gone"), "{lines:?}");
    }

    /// With the other panel on another computer, zips stay on this one by default.
    #[test]
    fn defaults_stay_on_this_computer() {
        let Some(mut over) = over_there("app-ssh-defaults") else { return };
        over.here.file("a.txt", "a");
        let app = &mut over.app;
        app.on_key(key(KeyCode::Tab));
        app.on_key(ctrl('r'));
        let place = over.connection.target.place(&format!("{}/", over.connection.home));
        assert!(app.panels[1 - app.active].remote.is_some());
        app.active_panel_mut().select_name("a.txt");
        app.on_key(with_mods(KeyCode::F(5), KeyModifiers::ALT));
        assert_eq!(field(app), over.here.path().join("a.zip").display().to_string());
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::F(5)));
        assert_eq!(field(app), place, "F5 does go there");
    }

    /// Hosts connected to come back in the drive picker, and a password never gets in anywhere.
    #[test]
    fn addresses_are_remembered_but_passwords_are_not() {
        let Some(mut over) = over_there("app-ssh-hosts") else { return };
        let address = over.connection.target.place(&over.connection.home);
        let app = &mut over.app;
        assert_eq!(app.history.inputs.list("ssh"), [address]);
        app.on_key(alt(KeyCode::F(1)));
        let Some(Dialog::Drives { drives, .. }) = &app.dialog else { panic!("no drives") };
        let row = drives.iter().find(|drive| drive.kind == "ssh").expect("a row for the host");
        assert_eq!(row.root, PathBuf::from(over.connection.target.place("")));
        app.dialog = None;

        app.on_key(ctrl('n'));
        typing(app, "ssh://me:secret@nas/x");
        app.on_key(key(KeyCode::Enter));
        let Some(Dialog::Message { lines, .. }) = &app.dialog else { panic!("not refused") };
        assert!(lines[0].contains("no password"), "{lines:?}");
        assert_eq!(app.history.inputs.list("ssh").len(), 1, "not kept");
        app.on_key(key(KeyCode::Enter));

        // With a slash in it the address can't be read, and that keeps it out too.
        app.on_key(ctrl('n'));
        typing(app, "ssh://me:se/cret@nas/x");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.dialog, Some(Dialog::Message { .. })));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.history.inputs.list("ssh").len(), 1, "not kept");

        // From the command line neither.
        typing(app, "cd ssh://me:secret@nas/x");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.dialog, Some(Dialog::Message { .. })));
        assert!(app.history.commands.iter().all(|line| !line.contains("secret")), "{:?}", app.history.commands);
    }

    /// A field goes back through what was typed into its kind before, between sessions too,
    /// and past the newest it holds what it started with.
    #[test]
    fn dialog_fields_remember_what_was_typed() {
        let tmp = TempDir::new("app-inputs");
        let left = tmp.dir("left");
        let right = tmp.dir("right");
        let history_file = Some(tmp.path().join("history.txt"));
        let start = |file: &Option<PathBuf>| App::new(left.clone(), right.clone(), Config { history_file: file.clone(), ..crate::testutil::config() });
        let field = |app: &App| match &app.dialog {
            Some(Dialog::Input { edit, .. }) => edit.text().to_string(),
            _ => panic!("no field"),
        };
        let mut app = start(&history_file);
        for name in ["first", "second"] {
            app.on_key(key(KeyCode::F(7)));
            typing(&mut app, name);
            app.on_key(key(KeyCode::Enter));
        }
        app.save_history();

        let mut app = start(&history_file);
        app.on_key(key(KeyCode::F(7)));
        typing(&mut app, "half typ");
        app.on_key(key(KeyCode::Up));
        assert_eq!(field(&app), "second");
        app.on_key(with_mods(KeyCode::Char('p'), KeyModifiers::ALT));
        assert_eq!(field(&app), "first");
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(field(&app), "half typ", "what was being typed is still there");
        app.on_key(key(KeyCode::Esc));

        // Another kind of field has its own list.
        app.on_key(key(KeyCode::Char('+')));
        app.on_key(key(KeyCode::Up));
        assert_eq!(field(&app), "*");
        app.on_key(key(KeyCode::Esc));

        // F5 starts on the other panel, and comes back to it.
        tmp.file("left/a.txt", "a");
        app.reload_panels();
        app.active_panel_mut().select_name("a.txt");
        app.on_key(key(KeyCode::F(5)));
        let default = field(&app);
        app.history.inputs.add("destination", "somewhere/else/");
        app.dialog = None;
        app.on_key(key(KeyCode::F(5)));
        app.on_key(key(KeyCode::Up));
        assert_eq!(field(&app), "somewhere/else/");
        app.on_key(key(KeyCode::Down));
        assert_eq!(field(&app), default);
    }

    /// Alt+H in a field lists what was typed there: typing narrows it, Enter takes a row back
    /// to the field, Delete forgets one, Esc goes back with the field as it was.
    #[test]
    fn alt_h_in_a_field_lists_what_was_typed() {
        let tmp = TempDir::new("app-earlier");
        let mut app = test_app(tmp.path(), tmp.path());
        for name in ["reports 2025", "reports 2026", "photos"] {
            app.history.inputs.add("folder", name);
        }
        let alt_h = with_mods(KeyCode::Char('h'), KeyModifiers::ALT);
        let rows = |app: &App| match &app.dialog {
            Some(Dialog::Earlier { filter, back, .. }) => dialog::earlier_rows(back, filter.text()),
            _ => panic!("no list"),
        };
        let field = |app: &App| match &app.dialog {
            Some(Dialog::Input { edit, .. }) => edit.text().to_string(),
            _ => panic!("no field"),
        };

        app.on_key(key(KeyCode::F(7)));
        typing(&mut app, "x");
        app.on_key(alt_h);
        assert_eq!(rows(&app), ["photos", "reports 2026", "reports 2025"], "newest first");
        typing(&mut app, "rep");
        assert_eq!(rows(&app), ["reports 2026", "reports 2025"]);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(field(&app), "reports 2025");

        app.on_key(alt_h);
        app.on_key(key(KeyCode::Delete));
        assert_eq!(rows(&app), ["reports 2026", "reports 2025"]);
        assert_eq!(app.history.inputs.list("folder"), ["reports 2025", "reports 2026"]);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(field(&app), "reports 2025");
        app.on_key(key(KeyCode::Esc));
        assert!(app.dialog.is_none());
    }

    /// Find keeps what was searched for, each field its own, for Alt+P to bring back.
    #[test]
    fn find_remembers_what_was_searched_for() {
        let tmp = TempDir::new("app-find-recall");
        let mut app = test_app(tmp.path(), tmp.path());
        let alt = |c| with_mods(KeyCode::Char(c), KeyModifiers::ALT);
        app.on_key(with_mods(KeyCode::F(7), KeyModifiers::ALT));
        typing(&mut app, "needle");
        app.on_key(key(KeyCode::Tab));
        typing(&mut app, "*.rs");
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.history.inputs.list(FIND_TEXT), ["needle"]);
        assert_eq!(app.history.inputs.list(FIND_FILES), ["*.rs"]);

        app.finder = None;
        app.on_key(with_mods(KeyCode::F(7), KeyModifiers::ALT));
        app.on_key(alt('p'));
        assert_eq!(app.finder.as_ref().unwrap().text.text(), "needle");
        app.on_key(key(KeyCode::Tab));
        app.on_key(alt('p'));
        assert_eq!(app.finder.as_ref().unwrap().names.text(), "*.rs");
        app.on_key(alt('n'));
        assert_eq!(app.finder.as_ref().unwrap().names.text(), "");
    }

    /// Shift+F5 and Shift+F6 take the file under the cursor, whatever is marked, and stay in
    /// its folder: the field starts on its name, selected (not on the other panel), the cursor
    /// follows the new name, the marks stay for the next job.
    #[test]
    fn shift_f5_and_shift_f6_work_in_place() {
        let left = TempDir::new("app-here-left");
        let right = TempDir::new("app-here-right");
        left.file("a.txt", "a");
        left.file("b.txt", "b");
        let mut app = test_app(left.path(), right.path());
        assert!(app.active_panel_mut().select_name("a.txt"));
        app.active_panel_mut().toggle_mark();
        assert!(app.active_panel_mut().select_name("b.txt"));
        let shift = |n| with_mods(KeyCode::F(n), KeyModifiers::SHIFT);

        app.on_key(shift(5));
        let Some(Dialog::Input { edit, prompt, .. }) = &app.dialog else { panic!("no dialog") };
        assert_eq!((edit.text(), prompt.as_str()), ("b.txt", "Copy \"b.txt\" to:"));
        assert!(edit.is_selected(), "typing replaces it");
        typing(&mut app, "b copy.txt");
        app.on_key(key(KeyCode::Enter));
        wait_for_job(&mut app);
        assert_eq!(fs::read_to_string(left.path().join("b copy.txt")).unwrap(), "b");
        assert!(left.path().join("b.txt").exists() && fs::read_dir(right.path()).unwrap().next().is_none());
        assert_eq!(app.active_panel().current().unwrap().name, "b copy.txt");
        assert_eq!(app.active_panel().marked_summary(), Some((1, 1)), "the marks are for another job");

        app.on_key(shift(6));
        typing(&mut app, "c.txt");
        app.on_key(key(KeyCode::Enter));
        wait_for_job(&mut app);
        assert!(left.path().join("c.txt").exists() && !left.path().join("b copy.txt").exists());
        assert_eq!(app.active_panel().current().unwrap().name, "c.txt");
        assert_eq!(app.active_panel().marked_summary(), Some((1, 1)));

        // Enter on the name as it is, or on an emptied field: nothing to do, and no box to say so.
        let remembered = app.history.inputs.list("name").len();
        for n in [5, 6] {
            app.on_key(shift(n));
            app.on_key(key(KeyCode::Enter));
            assert!(app.dialog.is_none() && app.job.is_none());
            assert_eq!(app.history.inputs.list("name").len(), remembered, "nor anything to remember");
            app.on_key(shift(n));
            app.on_key(key(KeyCode::Backspace));
            app.on_key(key(KeyCode::Enter));
            assert!(app.dialog.is_none() && app.job.is_none());
        }

        // The arrows edit the name instead of replacing it.
        app.on_key(shift(6));
        app.on_key(key(KeyCode::Home));
        typing(&mut app, "old ");
        app.on_key(key(KeyCode::Enter));
        wait_for_job(&mut app);
        assert!(left.path().join("old c.txt").exists());
        app.active_panel_mut().select_name("old c.txt");
        app.on_key(shift(6));
        typing(&mut app, "c.txt");
        app.on_key(key(KeyCode::Enter));
        wait_for_job(&mut app);

        // A name that is there: the usual question, here too.
        app.active_panel_mut().select_name("c.txt");
        app.on_key(shift(6));
        typing(&mut app, "b.txt");
        app.on_key(key(KeyCode::Enter));
        wait_until("the overwrite question", &mut app, |app| matches!(app.dialog, Some(Dialog::Question { .. })));
        app.on_key(key(KeyCode::Esc));
        wait_for_job(&mut app);
        assert_eq!(fs::read_to_string(left.path().join("b.txt")).unwrap(), "b");
    }

    #[test]
    fn esc_on_a_job_question_stops_it_without_another_box() {
        let left = TempDir::new("app-esc-left");
        let right = TempDir::new("app-esc-right");
        left.file("x.txt", "new");
        right.file("x.txt", "old");
        let mut app = test_app(left.path(), right.path());
        assert!(app.active_panel_mut().select_name("x.txt"));
        app.on_key(key(KeyCode::F(5)));
        app.on_key(key(KeyCode::Enter));
        wait_until("the overwrite question", &mut app, |app| matches!(app.dialog, Some(Dialog::Question { .. })));

        app.on_key(key(KeyCode::Esc));
        wait_for_job(&mut app);
        assert!(app.dialog.is_none(), "nothing more to dismiss");
        assert_eq!(fs::read_to_string(right.path().join("x.txt")).unwrap(), "old");
    }

    #[test]
    fn f9_menu_sorts_and_f9_twice_reverses() {
        let tmp = TempDir::new("app-sort");
        tmp.file("a.txt", "1");
        tmp.file("b.txt", "12");
        tmp.file("c.txt", "123");
        let mut app = test_app(tmp.path(), tmp.path());
        let names = |app: &App| app.active_panel().entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        let order = |app: &App| (app.active_panel().sort, app.active_panel().reverse);

        // F9 F9: the cursor starts on the current order (Name), so it flips to Z to A.
        app.on_key(key(KeyCode::F(9)));
        assert!(matches!(app.dialog, Some(Dialog::Sort { cursor: 0, .. })));
        app.on_key(key(KeyCode::F(9)));
        assert!(app.dialog.is_none());
        assert_eq!(names(&app), ["..", "c.txt", "b.txt", "a.txt"]);

        // A letter picks an order, the usual way round; the same letter again flips it.
        app.on_key(key(KeyCode::F(9)));
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(order(&app), (SortBy::Size, false));
        assert_eq!(names(&app), ["..", "c.txt", "b.txt", "a.txt"], "largest first");
        app.on_key(key(KeyCode::F(9)));
        app.on_key(key(KeyCode::Char('S')));
        assert_eq!(order(&app), (SortBy::Size, true));

        // Arrows and Enter; Esc leaves the order alone.
        app.on_key(key(KeyCode::F(9)));
        app.on_key(key(KeyCode::Home));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(order(&app), (SortBy::Name, false));
        app.on_key(key(KeyCode::F(9)));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Esc));
        assert!(app.dialog.is_none());
        assert_eq!(order(&app), (SortBy::Name, false));
    }

    #[test]
    fn alt_s_is_quick_search_too() {
        let tmp = TempDir::new("app-alt-s");
        tmp.file("bar.txt", "");
        tmp.file("baz.txt", "");
        tmp.file("foo.txt", "");
        let mut app = test_app(tmp.path(), tmp.path());
        app.on_key(with_mods(KeyCode::Char('s'), KeyModifiers::ALT));
        typing(&mut app, "ba");
        assert_eq!(app.active_panel().current().unwrap().name, "bar.txt");
        // Alt+S again: the next match.
        app.on_key(with_mods(KeyCode::Char('s'), KeyModifiers::ALT));
        assert_eq!(app.active_panel().current().unwrap().name, "baz.txt");
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.quicksearch, None);
        assert!(app.cmdline.is_empty());

        // AltGr (Ctrl+Alt) is a character on some layouts, not quick search.
        app.on_key(with_mods(KeyCode::Char('s'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert_eq!(app.quicksearch, None);
    }
}
