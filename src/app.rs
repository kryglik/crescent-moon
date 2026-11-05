//! Application state and keyboard handling.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Instant, SystemTime};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::admin;
use crate::archive::{self, Archive};
use crate::clipboard::{self, Clipboard, Effect};
use crate::compare;
use crate::dialog::{self, ButtonOutcome, ConfirmAction, Dialog, InputAction, LineEdit};
use crate::finder::{Field as FindField, Finder};
use crate::history::History;
use crate::hotlist::Hotlist;
use crate::lnk;
use crate::mouse::{HitAreas, LastClick};
use crate::ops::{self, Answer, Job, Msg, Question, Running};
use crate::options::{self, OptionsAutomatic};
use crate::panel::{DirSize, Entry, Panel, SortBy};
use crate::preview::{Key as PreviewKey, Previewer};
use crate::platform::{self, OpenError, Shell};
use crate::settings::Settings;
use crate::sizes::Sizer;
use crate::subshell::Subshell;
use crate::survey::{Survey, Tally};
use crate::util;
use crate::watch::Watch;

/// What happens to a file taken out of an archive (into rmc's temp folder) once it is out.
#[derive(Debug)]
enum AfterJob {
    /// Enter: opened in its app; a zip in the zip opens in the panel, called `shown`.
    Open { shown: PathBuf },
    /// F3 (and F4: files in archives are read-only).
    View,
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
        config
    }

    /// Commands come from the settings, or when those are empty from the environment
    /// (RMC_EDITOR…), or from what is installed.
    pub fn new(settings: Settings, hotlist_file: Option<PathBuf>, settings_file: Option<PathBuf>) -> Self {
        let auto = Automatic::detect();
        let chosen = |value: &str| (!value.trim().is_empty()).then(|| parse_command(value));
        let editor = chosen(&settings.editor).unwrap_or(auto.editor);
        // RMC_VIEWER counts when the option is empty; otherwise the editor, read-only.
        let viewer = chosen(&settings.viewer)
            .or_else(|| env_command(&["RMC_VIEWER"]))
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
            settings,
        }
    }
}

/// What rmc uses when a setting is empty: the environment, or what is installed.
pub struct Automatic {
    pub editor: Vec<String>,
    pub viewer: Vec<String>,
    pub pager: Option<Vec<String>>,
    pub shell: Shell,
}

impl Automatic {
    pub fn detect() -> Self {
        let editor = env_command(&["RMC_EDITOR", "VISUAL", "EDITOR"]).unwrap_or_else(|| vec![default_editor()]);
        let viewer = env_command(&["RMC_VIEWER"]).unwrap_or_else(|| viewer_for(&editor));
        let pager = env_command(&["RMC_PAGER", "PAGER"]).or_else(default_pager);
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
    /// The persistent shell, started by the first Ctrl+O (or command typed in rmc).
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
    pasting_cut: bool,
    /// What to do with a file the running job takes out of an archive, once it is out.
    after_job: Option<(AfterJob, PathBuf)>,
    /// Each panel's folder, watched for changes other programs make (see `watch`).
    watches: [Option<Watch>; 2],
}

impl App {
    pub fn new(left: PathBuf, right: PathBuf, config: Config) -> Self {
        let show_hidden = config.settings.show_hidden;
        crate::panel::NATURAL_SORT.store(config.settings.natural_sort, Ordering::Relaxed);
        let mut history = History::load(config.history_file.clone());
        let panels = [Panel::new(left, show_hidden), Panel::new(right, show_hidden)];
        history.visit(&panels[1].cwd);
        history.visit(&panels[0].cwd);
        App {
            mouse: config.settings.mouse,
            mouse_allowed: true,
            finder: None,
            find_shown: false,
            panels,
            active: 0,
            cmdline: LineEdit::default(),
            dialog: None,
            job: None,
            quicksearch: None,
            hotlist: Hotlist::load(config.hotlist_file.clone()),
            config,
            external: None,
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
            notice: None,
            quick_view: false,
            preview_focus: false,
            previewer: None,
            preview_rows: 20,
            clipboard: Clipboard::default(),
            terminal_clipboard: None,
            pasting_cut: false,
            after_job: None,
            watches: [None, None],
        }
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
        self.watches.iter().flatten().any(Watch::pending) || self.previewer.as_ref().is_some_and(Previewer::loading)
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
        let now = Instant::now();
        let mut changed = false;
        for side in 0..2 {
            let cwd = &self.panels[side].cwd;
            if self.watches[side].as_ref().is_none_or(|w| w.dir != *cwd) {
                self.watches[side] = Some(Watch::new(cwd));
            }
            let due = self.watches[side].as_mut().is_some_and(|w| w.due(now));
            if due && self.job.is_none() && !self.shell_shown {
                self.panels[side].reload();
                changed = true;
            }
        }
        changed
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
        self.job.is_some() || self.sizer.as_ref().is_some_and(Sizer::is_busy) || drives_pending || finding || surveying
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
        self.notice = None;
        if self.shell_shown {
            self.on_shell_key(key);
        } else if self.dialog.is_some() {
            self.on_dialog_key(key);
        } else if self.find_shown {
            self.on_find_key(key);
        } else if let Some(job) = &self.job {
            if key.code == KeyCode::Esc {
                job.cancel.store(true, Ordering::Relaxed);
            }
        } else if self.quick_view && self.preview_focus && self.on_preview_key(&key) {
        } else if !self.on_quicksearch_key(&key) {
            self.on_panel_key(key);
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
                if !self.refuse_in_archive(self.active) {
                    self.start_input("New file", "Create and edit file:", "", InputAction::NewFile);
                }
            }
            KeyCode::F(4) => self.edit_current(),
            KeyCode::F(5) if alt => self.start_pack(),
            KeyCode::F(9) if alt => self.start_unpack(),
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
            KeyCode::Esc => self.cmdline.clear(),
            KeyCode::Char('r') if ctrl && !alt => self.reload_panels(),
            KeyCode::Char('h') if ctrl && !alt => self.toggle_hidden(),
            KeyCode::Char('.') if alt && !ctrl => self.toggle_hidden(),
            KeyCode::Char('u') if ctrl && !alt => self.panels.swap(0, 1),
            KeyCode::Char('o') if ctrl && !alt => self.show_shell(),
            // Ctrl+\ is a dead key on the Czech layout, so Ctrl+D (as in Total Commander) works too.
            // Unix terminals send Ctrl+\ as 0x1C, which crossterm reports as Ctrl+4.
            KeyCode::Char('\\' | 'd' | '\u{1c}' | '4') if ctrl && !alt => self.open_hotlist(),
            KeyCode::Char(' ') if ctrl && !alt => self.calculate_sizes(),
            // Ctrl+S or Alt+S, as in mc (both together is AltGr, which types a character).
            KeyCode::Char('s') if ctrl != alt => self.quicksearch = Some(String::new()),
            KeyCode::Char('i') if alt && !ctrl => {
                let cwd = self.active_panel().cwd.clone();
                self.change_dir(1 - self.active, cwd);
            }
            KeyCode::Char('o') if alt && !ctrl => {
                let target = match self.active_panel().current() {
                    Some(entry) if entry.is_dir && !entry.is_parent => entry.path.clone(),
                    _ => self.active_panel().cwd.clone(),
                };
                self.change_dir(1 - self.active, target);
            }
            KeyCode::Char('p') if alt && !ctrl => self.history_step(-1),
            KeyCode::Char('n') if alt && !ctrl => self.history_step(1),
            KeyCode::Char('+') if cmd_empty && plain => {
                self.start_input("Select", "Mark files matching:", "*", InputAction::Select)
            }
            KeyCode::Char('-') if cmd_empty && plain => {
                self.start_input("Unselect", "Unmark files matching:", "*", InputAction::Unselect)
            }
            KeyCode::Char('*') if cmd_empty && plain => self.active_panel_mut().invert_marks(),
            KeyCode::Char('a') if ctrl && !alt && cmd_empty => self.active_panel_mut().mark_all(),
            KeyCode::Char('a') if alt && !ctrl => self.open_attributes(),
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
            Dialog::Input { title, prompt, mut edit, action } => match key.code {
                KeyCode::Enter => {
                    self.on_input(action, edit.text().to_string());
                    None
                }
                KeyCode::Esc => None,
                _ => {
                    edit.handle_key(&key);
                    Some(Dialog::Input { title, prompt, edit, action })
                }
            },
            Dialog::Drives { side, drives, mut cursor } => match key.code {
                KeyCode::Up => {
                    cursor = cursor.saturating_sub(1);
                    Some(Dialog::Drives { side, drives, cursor })
                }
                KeyCode::Down => {
                    cursor = (cursor + 1).min(drives.len().saturating_sub(1));
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
    /// If the shell can't run inside rmc, a one-off shell takes over the console instead.
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
            && dir.is_dir()
            && !util::same_path(&dir, &self.active_panel().cwd)
        {
            self.change_dir(self.active, dir);
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
            KeyCode::Esc | KeyCode::F(10) => self.find_shown = false,
            KeyCode::F(7) | KeyCode::Char('?') if alt => self.find_shown = false,
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

    /// Enter in Find: the panel goes to the folder of the result, the cursor on it.
    pub(crate) fn find_go_to(&mut self) {
        let Some(path) = self.finder.as_ref().and_then(Finder::selected).map(|(path, ..)| path.to_path_buf()) else {
            return;
        };
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
        lines.push("Quit rmc?".into());
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
        } else if !row.is_flag() && target.is_none() && !close {
            edit.handle_key(key);
        }
        if (close || target.is_some()) && !row.is_flag() {
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
            self.dialog = Some(Dialog::Options { cursor: i, edit, settings, automatic });
        }
    }

    /// Takes changed settings: saves them, and applies what can change while running
    /// (hidden files, the mouse, commands, the big-file limit).
    fn apply_settings(&mut self, settings: Settings) {
        if settings == self.config.settings {
            return;
        }
        if let Some(file) = &self.config.settings_file
            && let Err(e) = settings.save(file)
        {
            self.error(format!("Cannot save the settings:\n{e}"));
        }
        crate::panel::NATURAL_SORT.store(settings.natural_sort, Ordering::Relaxed);
        if settings.show_hidden != self.config.settings.show_hidden || settings.natural_sort != self.config.settings.natural_sort {
            for panel in &mut self.panels {
                panel.show_hidden = settings.show_hidden;
                panel.reload();
            }
        }
        self.mouse = settings.mouse && self.mouse_allowed;
        let (hotlist_file, settings_file) = (self.config.hotlist_file.take(), self.config.settings_file.take());
        let history_file = self.config.history_file.take();
        self.config = Config::new(settings, hotlist_file, settings_file);
        self.config.history_file = history_file;
    }

    /// F9: a menu of sort orders, the cursor on the current one (so F9 F9 reverses it).
    fn open_sort_menu(&mut self) {
        let panel = self.active_panel();
        let (current, reverse) = (panel.sort, panel.reverse);
        let cursor = SortBy::ALL.iter().position(|&s| s == current).unwrap_or(0);
        self.dialog = Some(Dialog::Sort { side: self.active, cursor, current, reverse });
    }

    fn open_hotlist(&mut self) {
        let cwd = &self.active_panel().cwd;
        let cursor = self.hotlist.dirs.iter().position(|d| util::same_path(d, cwd)).unwrap_or(0);
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
                let cwd = self.active_panel().cwd.clone();
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
        }
    }

    /// Alt+F5 (Total Commander's key): the marked files (or the one under the cursor) into a
    /// new zip, in the other panel's folder by default.
    fn start_pack(&mut self) {
        if self.refuse_in_archive(self.active) {
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
        let default = self.panels[1 - self.active].cwd.join(format!("{name}.zip"));
        let prompt = format!("Pack {} into:", describe(&sources));
        self.start_input("Pack", &prompt, &default.display().to_string(), InputAction::Pack(sources));
    }

    /// Alt+F9 (Total Commander's key): the zip under the cursor, unpacked into the other
    /// panel's folder; one with several things at its top into a folder of its own name, as
    /// 7-Zip's "Extract to name\" does.
    fn start_unpack(&mut self) {
        if self.refuse_in_archive(self.active) {
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
        let items: Vec<String> = zip.children("").map(|item| item.path.clone()).collect();
        let other = self.panels[1 - self.active].cwd.clone();
        let stem = entry.path.file_stem().map_or_else(|| "unpacked".into(), |s| s.to_string_lossy().into_owned());
        let dest = if items.len() > 1 { other.join(stem) } else { other };
        let prompt = format!("Unpack \"{}\" to:", entry.name);
        let default = util::with_trailing_separator(&dest);
        self.start_input("Unpack", &prompt, &default, InputAction::Extract { archive: entry.path, items });
    }

    fn on_input(&mut self, action: InputAction, text: String) {
        let cwd = self.active_panel().cwd.clone();
        match action {
            InputAction::Copy(sources) => match ops::plan(&sources, &text, &cwd) {
                Ok(pairs) => self.start_job(Job::Copy(pairs)),
                Err(e) => self.error(e),
            },
            InputAction::Move(sources) => match ops::plan(&sources, &text, &cwd) {
                Ok(pairs) => self.start_job(Job::Move(pairs)),
                Err(e) => self.error(e),
            },
            InputAction::Rename(from) => self.rename(&from, &text),
            InputAction::Mkdir => self.mkdir(&cwd, &text),
            InputAction::NewFile => self.new_file(&cwd, &text),
            InputAction::Select => {
                self.active_panel_mut().mark_matching(&text, true);
            }
            InputAction::Unselect => {
                self.active_panel_mut().mark_matching(&text, false);
            }
            InputAction::Extract { archive, items } => {
                if text.trim().is_empty() {
                    return;
                }
                let dest = util::resolve(&cwd, &text);
                self.start_job(Job::Extract { archive, items, dest });
            }
            InputAction::Pack(sources) => {
                if text.trim().is_empty() {
                    return;
                }
                let mut dest = util::resolve(&cwd, &text);
                if dest.is_dir() {
                    let name = sources[0].file_name().map_or_else(|| "archive".into(), |n| n.to_string_lossy().into_owned());
                    dest = dest.join(format!("{name}.zip"));
                } else if dest.extension().is_none() {
                    dest.set_extension("zip");
                }
                if let Some(inside) = sources.iter().find(|s| s.is_dir() && util::is_inside(&dest, s)) {
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

    fn start_input(&mut self, title: &str, prompt: &str, text: &str, action: InputAction) {
        self.dialog = Some(Dialog::Input {
            title: title.into(),
            prompt: prompt.into(),
            edit: LineEdit::new(text),
            action,
        });
    }

    fn start_job(&mut self, job: Job) {
        self.job = Some(ops::start(job));
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
        changed |= self.follow_changes();
        if self.quick_view {
            let key = self.preview_key();
            if let Some(previewer) = &mut self.previewer {
                previewer.show(key);
                changed |= previewer.tick();
            }
        }
        // Now and then rather than only on exit: closing the window ends rmc without one.
        if self.history_changed && self.history_saved.elapsed() > std::time::Duration::from_secs(5) {
            self.save_history();
        }
        self.tick_job() || changed
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
                    let log = std::env::temp_dir().join("rmc-panic.txt");
                    let error = format!("The job stopped unexpectedly (an internal error, see {}).", log.display());
                    finished = Some((vec![error], true, None));
                    break;
                }
            };
            changed = true;
            match msg {
                Msg::Progress(progress) => job.update(progress),
                Msg::Ask(question, reply) => self.dialog = Some(question_dialog(question, reply)),
                Msg::Done { errors, cancelled, as_admin } => {
                    finished = Some((errors, cancelled, as_admin));
                    break;
                }
            }
        }
        let Some((errors, cancelled, as_admin)) = finished else { return changed };
        // "As admin": the rest goes to the elevated helper, shown as the same kind of job.
        if let Some(paths) = as_admin {
            self.job = Some(admin::start(paths, errors));
            return true;
        }
        self.job = None;
        // Cut files pasted: the clipboard no longer says where they are (Explorer empties it too).
        if std::mem::take(&mut self.pasting_cut) && errors.is_empty() && !cancelled {
            self.clipboard.moved();
        }
        // A file taken out of an archive: now it can be opened.
        if let Some((then, path)) = self.after_job.take()
            && errors.is_empty()
            && !cancelled
        {
            self.after_taking_out(then, path);
        }
        for panel in &mut self.panels {
            // Done: the marks have served. Stopped or partly failed: they stay, to try again.
            if errors.is_empty() && !cancelled {
                panel.clear_marks();
            }
            // Sizes may be out of date after copying, moving or deleting.
            panel.dir_sizes.clear();
            panel.reload();
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
            && dir.is_dir()
            && !util::same_path(&dir, &self.active_panel().cwd)
        {
            self.change_dir(self.active, dir);
        }
        if let Err(e) = result {
            self.error(e);
        }
    }

    fn change_dir(&mut self, side: usize, dir: PathBuf) {
        match self.panels[side].change_dir(dir) {
            Ok(()) => self.visited(side),
            Err(e) => self.error(e),
        }
    }

    /// The panel arrived in a folder: it goes to the top of the folder history (Alt+H).
    fn visited(&mut self, side: usize) {
        let cwd = self.panels[side].cwd.clone();
        self.history.visit(&cwd);
        self.history_changed = true;
    }

    /// Alt+Left / Alt+Right (and mc's Alt+Y / Alt+U): back and forward through the folders
    /// the active panel has been in.
    fn step_back(&mut self, back: bool) {
        let panel = &mut self.panels[self.active];
        let stepped = if back { panel.go_back() } else { panel.go_forward() };
        match stepped {
            Ok(true) => self.visited(self.active),
            Ok(false) => {
                self.notice = Some(if back { "No folder to go back to" } else { "No folder to go forward to" }.into());
            }
            Err(e) => self.error(e),
        }
    }

    /// Ctrl+C / Ctrl+X: the marked files (or the one under the cursor) to the clipboard, as
    /// Explorer puts them there, so Explorer or a mail can paste them; their paths go along as
    /// text. Elsewhere than Windows the paths go to the terminal's clipboard (OSC 52).
    fn copy_to_clipboard(&mut self, effect: Effect) {
        if self.refuse_in_archive(self.active) {
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
        if self.refuse_in_archive(self.active) {
            return;
        }
        let Some(files) = self.clipboard.files() else {
            self.notice = Some("No files on the clipboard (Ctrl+C copies some, here or in Explorer)".into());
            return;
        };
        let dir = self.active_panel().cwd.clone();
        let mut pairs = Vec::new();
        for source in &files.paths {
            let here = source.parent().is_some_and(|parent| util::same_path(parent, &dir));
            if here && files.effect == Effect::Move {
                continue;
            }
            let Some(target) = clipboard::paste_target(source, &dir) else { continue };
            if source.is_dir() && util::is_inside(&target, source) {
                return self.error(format!("\"{}\" cannot be put inside itself.", source.display()));
            }
            pairs.push((source.clone(), target));
        }
        if pairs.is_empty() {
            return;
        }
        if files.effect == Effect::Move {
            self.pasting_cut = true;
            self.start_job(Job::Move(pairs));
        } else {
            self.start_job(Job::Copy(pairs));
        }
    }

    /// Alt+A: the attributes (Windows) or permissions (elsewhere) of the marked files, or the
    /// one under the cursor. Where they differ, a row shows [?] and is left as each has it.
    fn open_attributes(&mut self) {
        if self.refuse_in_archive(self.active) {
            return;
        }
        let paths = self.active_panel().selection();
        if paths.is_empty() {
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
        if util::same_path(&self.panels[0].cwd, &self.panels[1].cwd) {
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
        let cwd = &self.active_panel().cwd;
        self.history.matching(filter).into_iter().filter(|dir| !util::same_path(dir, cwd)).cloned().collect()
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
                    self.history.dirs.retain(|d| !util::same_path(d, dir));
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
        match self.active_panel().cwd.parent() {
            Some(parent) => self.change_dir(self.active, parent.to_path_buf()),
            None => self.open_drives(self.active),
        }
    }

    fn open_drives(&mut self, side: usize) {
        let drives = platform::drives();
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
        let panel = &self.panels[side];
        let here = util::volume_key(root) == util::volume_key(&panel.cwd);
        let target = if here { root.to_path_buf() } else { panel.dir_on_drive(root) };
        self.change_dir(side, target);
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
        } else if archive::is_archive(&entry.path) {
            // A zip opens as a folder, as in Total Commander and mc.
            if let Err(e) = self.panels[self.active].open_archive(&entry.path, entry.path.clone()) {
                self.error(e);
            }
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

    /// Shift+Enter: Windows' "How do you want to open this file?" for the file under the cursor.
    fn open_with(&mut self) {
        let Some(entry) = self.active_panel().current().filter(|e| !e.is_parent && !e.is_dir).cloned() else { return };
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

    /// A file in an archive goes out into rmc's temp folder (as a job: it may be big), and
    /// `then` happens to it there.
    fn take_out(&mut self, entry: &Entry, then: AfterJob) {
        let Some(view) = self.active_panel().archive.as_ref() else { return };
        let inner = archive::join(&view.inner, &entry.name);
        let file = view.archive.file.clone();
        static TAKEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = archive::temp_dir().join(TAKEN.fetch_add(1, Ordering::Relaxed).to_string());
        self.after_job = Some((then, dir.join(&entry.name)));
        self.start_job(Job::Extract { archive: file, items: vec![inner], dest: dir });
    }

    /// The file a job took out of an archive is there: open it, view it, or open it as an
    /// archive itself.
    fn after_taking_out(&mut self, then: AfterJob, path: PathBuf) {
        let size = fs::metadata(&path).map_or(0, |m| m.len());
        match then {
            AfterJob::Open { shown } if archive::is_archive(&path) => {
                if let Err(e) = self.panels[self.active].open_archive(&path, shown) {
                    self.error(e);
                }
            }
            AfterJob::Open { .. } => match platform::open_default(&path) {
                Ok(()) => {}
                Err(OpenError::NoOpener) => self.view(path, size, None),
                Err(OpenError::Failed(e)) => self.error(e),
            },
            AfterJob::View => self.view(path, size, None),
        }
    }

    /// Archives open read-only: in panel `side` one is shown, say so and return true.
    fn refuse_in_archive(&mut self, side: usize) -> bool {
        if self.panels[side].archive.is_none() {
            return false;
        }
        self.error("rmc opens archives read-only.\nF5 copies files out of one; Alt+F5 packs files into a new one.");
        true
    }

    fn view_current(&mut self) {
        match self.active_panel().current().cloned() {
            Some(entry) if entry.is_dir => self.open_current(),
            Some(entry) if self.active_panel().archive.is_some() => self.take_out(&entry, AfterJob::View),
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
            let default = util::with_trailing_separator(&self.panels[1 - self.active].cwd);
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
        let default = util::with_trailing_separator(&self.panels[1 - self.active].cwd);
        let what = describe(&sources);
        if is_move {
            self.start_input("Move", &format!("Move {what} to:"), &default, InputAction::Move(sources));
        } else {
            self.start_input("Copy", &format!("Copy {what} to:"), &default, InputAction::Copy(sources));
        }
    }

    fn start_rename(&mut self) {
        if self.refuse_in_archive(self.active) {
            return;
        }
        let Some(entry) = self.active_panel().current().filter(|e| !e.is_parent) else { return };
        let (name, path) = (entry.name.clone(), entry.path.clone());
        self.start_input("Rename", &format!("Rename \"{name}\" to:"), &name, InputAction::Rename(path));
    }

    fn rename(&mut self, from: &Path, input: &str) {
        if input.trim().is_empty() {
            return;
        }
        let dir = from.parent().unwrap_or(from);
        let target = util::resolve(dir, input);
        // A change of letter case only is fine on Windows, where both names are the same file.
        if target.exists() && !util::same_path(from, &target) {
            self.error(format!("\"{}\" already exists.", target.display()));
            return;
        }
        if let Err(e) = fs::rename(from, &target) {
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
        let target = util::resolve(cwd, input);
        if target.exists() {
            self.error(format!("\"{}\" already exists.", target.display()));
            self.select_in_active(&target);
            return;
        }
        if let Err(e) = fs::create_dir_all(&target) {
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
        let target = util::resolve(cwd, input);
        if !target.exists() {
            let created = target
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| fs::File::create_new(&target).map(drop));
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
        if let Some(name) = util::child_name(&panel.cwd, path) {
            panel.select_name(&name);
        }
    }

    fn confirm_delete(&mut self, to_trash: bool) {
        if self.refuse_in_archive(self.active) {
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
        for panel in &mut self.panels {
            panel.show_hidden = show;
            panel.reload();
        }
    }

    fn insert_current_name(&mut self) {
        let Some(entry) = self.active_panel().current().filter(|e| !e.is_parent) else { return };
        let name = &entry.name;
        // PowerShell also takes ‘ ’ ‚ ‛ as single quotes and “ ” „ as double ones.
        let needs_quotes = name.chars().any(|c| c.is_whitespace() || "'\"&()[]{};,$`#@|<>‘’‚‛“”„".contains(c));
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
        self.history.command(&line);
        self.history_changed = true;
        // The folder as the shell would read it (quotes, variables): Ctrl+Enter inserts names
        // quoted for the shell, so `cd 'Účty a faktury'` must work.
        let posix = self.shell().is_posix();
        match cd_argument(&line).map(|arg| util::shell_word(arg, posix)) {
            // `cd -`: back to the folder before, as in bash and PowerShell.
            Some(Some(arg)) if arg == "-" => self.step_back(true),
            Some(Some(arg)) => {
                let panel = self.active_panel();
                let cwd = panel.cwd.clone();
                let target = match util::bare_drive(&arg) {
                    // "cd e:" returns to where this panel was on E: (and stays put when already there).
                    Some(root) if util::same_volume(&root, &cwd) => return,
                    Some(root) => panel.dir_on_drive(&root),
                    None if arg.is_empty() => util::home_dir().unwrap_or(cwd),
                    None => util::resolve(&cwd, &arg),
                };
                self.change_dir(self.active, target);
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
        while !names(&app).contains(&"new.txt".to_string()) {
            assert!(Instant::now() < deadline, "the new file never showed up: {:?}", names(&app));
            std::thread::sleep(Duration::from_millis(20));
            app.tick();
        }
        assert!(names(&app).contains(&"new.txt".to_string()) && app.panels[1].entries.iter().any(|e| e.name == "new.txt"));
        assert_eq!(app.active_panel().current().unwrap().name, "b.txt");
        assert_eq!(app.active_panel().marked_summary(), Some((1, 1)));
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
    /// and Alt+V move them, and then the clipboard is empty. (Tests use rmc's own clipboard,
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
        assert!(app.subshell.is_none(), "cd is handled by rmc itself");

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

        // A command typed in rmc runs in the shell, which comes up to show it.
        typing(&mut app, "$rmcX = 'kept'; \"out=$(6*7)\"");
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
        wait_until("rmc's cd", &mut app, |app| shell_dir(app).is_some_and(|d| util::same_path(&d, tmp.path())));
        typing(&mut app, "\"var=$rmcX\"");
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
        let file = tmp.path().join("rmc").join("settings.ini");
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
