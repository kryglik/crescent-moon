//! Application state and keyboard handling.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};
use std::time::SystemTime;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::dialog::{self, ButtonOutcome, ConfirmAction, Dialog, InputAction, LineEdit};
use crate::hotlist::Hotlist;
use crate::mouse::{HitAreas, LastClick};
use crate::ops::{self, Answer, Job, Msg, Question, Running};
use crate::options::{self, OptionsAutomatic};
use crate::panel::{DirSize, Panel, SortBy};
use crate::platform::{self, OpenError, Shell};
use crate::settings::Settings;
use crate::sizes::Sizer;
use crate::subshell::Subshell;
use crate::util;

/// Something to run outside the TUI; the main loop gives it the terminal.
#[derive(Debug, PartialEq)]
pub enum External {
    Edit(PathBuf),
    View(PathBuf),
    /// A big file, shown in the pager (`less`), which reads only what is on screen.
    Page(PathBuf),
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
    pub settings: Settings,
}

impl Config {
    /// The settings file, the hotlist file, and from them the commands.
    pub fn load() -> Self {
        let settings_file = Settings::default_file();
        let settings = Settings::load(settings_file.as_deref());
        Config::new(settings, Hotlist::default_file(), settings_file)
    }

    /// Commands come from the settings, or when those are empty from the environment
    /// (RMC_EDITOR…), or from what is installed.
    pub fn new(settings: Settings, hotlist_file: Option<PathBuf>, settings_file: Option<PathBuf>) -> Self {
        let auto = Automatic::detect();
        let chosen = |value: &str| (!value.trim().is_empty()).then(|| parse_command(value));
        let editor = chosen(&settings.editor).unwrap_or(auto.editor);
        let viewer = chosen(&settings.viewer).unwrap_or_else(|| viewer_for(&editor));
        let pager = chosen(&settings.pager).or(auto.pager);
        let shell = if settings.shell.trim().is_empty() { auto.shell } else { Shell::from_program(settings.shell.trim()) };
        Config { editor, viewer, pager, shell, big_file: settings.big_file_bytes(), hotlist_file, settings_file, settings }
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
    sizer: Option<Sizer>,
    /// Free space of network/optical drives, arriving while the drive picker is open.
    drive_space: Option<Receiver<(PathBuf, Option<(u64, u64)>)>>,
    history: Vec<String>,
    history_pos: Option<usize>,
}

impl App {
    pub fn new(left: PathBuf, right: PathBuf, config: Config) -> Self {
        let show_hidden = config.settings.show_hidden;
        App {
            mouse: config.settings.mouse,
            panels: [Panel::new(left, show_hidden), Panel::new(right, show_hidden)],
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
            shell_hint_shown: false,
            subshell: None,
            shell_shown: false,
            screen_size: (24, 80),
            sizer: None,
            drive_space: None,
            history: Vec::new(),
            history_pos: None,
        }
    }

    /// Whether background work is running (the main loop then redraws more often).
    pub fn is_busy(&self) -> bool {
        self.working() || self.shell_shown
    }

    /// Work in the background that changes the screen by itself: a copy, move or delete,
    /// directory sizes, or drive space still to arrive.
    pub fn working(&self) -> bool {
        let drives_pending = matches!(&self.dialog, Some(Dialog::Drives { drives, .. }) if drives.iter().any(|d| d.pending));
        self.job.is_some() || self.sizer.as_ref().is_some_and(Sizer::is_busy) || drives_pending
    }

    pub fn active_panel(&self) -> &Panel {
        &self.panels[self.active]
    }

    fn active_panel_mut(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
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

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.shell_shown {
            self.on_shell_key(key);
        } else if self.dialog.is_some() {
            self.on_dialog_key(key);
        } else if let Some(job) = &self.job {
            if key.code == KeyCode::Esc {
                job.cancel.store(true, Ordering::Relaxed);
            }
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
            KeyCode::F(2) => self.start_rename(),
            KeyCode::F(3) => self.view_current(),
            KeyCode::F(4) if shift => self.start_input("New file", "Create and edit file:", "", InputAction::NewFile),
            KeyCode::F(4) => self.edit_current(),
            KeyCode::F(5) => self.start_transfer(false),
            KeyCode::F(6) => self.start_transfer(true),
            KeyCode::F(7) => self.start_input("Make directory", "New directory name:", "", InputAction::Mkdir),
            KeyCode::F(8) => self.confirm_delete(!shift),
            KeyCode::Delete if cmd_empty => self.confirm_delete(!shift),
            KeyCode::F(9) => self.open_sort_menu(),
            KeyCode::F(10) => self.request_quit(),
            KeyCode::Char('p') if ctrl && !alt => self.open_options(),
            KeyCode::Tab => self.active = 1 - self.active,
            KeyCode::Up | KeyCode::Down if shift => {
                let panel = self.active_panel_mut();
                panel.toggle_mark();
                panel.move_by(if key.code == KeyCode::Up { -1 } else { 1 });
            }
            KeyCode::Up => self.active_panel_mut().move_by(-1),
            KeyCode::Down => self.active_panel_mut().move_by(1),
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
            shell.follow_panel(&cwd, &self.config.shell);
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
        shell.follow_panel(&cwd, &self.config.shell);
        shell.send_text(&format!("{line}\r"));
        self.shell_shown = true;
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
        if settings.show_hidden != self.config.settings.show_hidden {
            for panel in &mut self.panels {
                panel.show_hidden = settings.show_hidden;
                panel.reload();
            }
        }
        self.mouse = settings.mouse;
        let (hotlist_file, settings_file) = (self.config.hotlist_file.take(), self.config.settings_file.take());
        self.config = Config::new(settings, hotlist_file, settings_file);
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
            ConfirmAction::Delete { paths, to_trash } => self.start_job(Job::Delete { paths, to_trash }),
            ConfirmAction::Edit(path) => self.external = Some(External::Edit(path)),
            ConfirmAction::View(path) => self.external = Some(External::View(path)),
        }
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
        self.tick_job() || changed
    }

    /// The running job's progress, questions and end. Returns whether any of them arrived.
    fn tick_job(&mut self) -> bool {
        let Some(job) = self.job.as_mut() else { return false };
        let mut finished = None;
        let mut changed = false;
        while let Ok(msg) = job.rx.try_recv() {
            changed = true;
            match msg {
                Msg::Progress(progress) => job.progress = progress,
                Msg::Ask(question, reply) => self.dialog = Some(question_dialog(question, reply)),
                Msg::Done { errors, cancelled } => {
                    finished = Some((errors, cancelled));
                    break;
                }
            }
        }
        let Some((errors, cancelled)) = finished else { return changed };
        self.job = None;
        for panel in &mut self.panels {
            panel.clear_marks();
            // Sizes may be out of date after copying, moving or deleting.
            panel.dir_sizes.clear();
            panel.reload();
        }
        // Esc just stops the job: no "cancelled" box to dismiss as well. Failures from before
        // the stop are still worth knowing about.
        if !errors.is_empty() {
            let mut lines = vec![format!("{} item(s) were not processed:", errors.len())];
            lines.extend(errors.iter().take(8).cloned());
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
        if let Err(e) = self.panels[side].change_dir(dir) {
            self.error(e);
        }
    }

    fn go_up(&mut self) {
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
        if entry.is_parent {
            self.go_up();
        } else if entry.is_dir {
            self.change_dir(self.active, entry.path);
        } else if entry.is_exec {
            self.run_in_shell(self.config.shell.run_file_line(&entry.path));
        } else {
            match platform::open_default(&entry.path) {
                Ok(()) => {}
                // A server or WSL has nothing to open files with: show the file instead.
                Err(OpenError::NoOpener) => self.view(entry.path, entry.size),
                Err(OpenError::Failed(e)) => self.error(e),
            }
        }
    }

    fn view_current(&mut self) {
        match self.active_panel().current().cloned() {
            Some(entry) if entry.is_dir => self.open_current(),
            Some(entry) => self.view(entry.path, entry.size),
            None => {}
        }
    }

    /// F3: the viewer, or for a big file the pager (or, without one, the viewer after asking).
    fn view(&mut self, path: PathBuf, size: u64) {
        if size <= self.config.big_file {
            self.external = Some(External::View(path));
        } else if self.config.pager.is_some() {
            self.external = Some(External::Page(path));
        } else {
            self.confirm_big_file(path, size, false);
        }
    }

    fn edit_current(&mut self) {
        let Some(entry) = self.active_panel().current().cloned() else { return };
        if entry.is_dir {
            return;
        }
        if entry.size > self.config.big_file {
            self.confirm_big_file(entry.path, entry.size, true);
        } else {
            self.external = Some(External::Edit(entry.path));
        }
    }

    /// "x.log is 250M. Open it in nvim anyway?", with No as the default answer.
    fn confirm_big_file(&mut self, path: PathBuf, size: u64, edit: bool) {
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
            action: if edit { ConfirmAction::Edit(path) } else { ConfirmAction::View(path) },
        });
    }

    fn start_transfer(&mut self, is_move: bool) {
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
        self.external = Some(External::Edit(target));
    }

    /// Puts the cursor on `path` (or the directory leading to it) in the active panel.
    fn select_in_active(&mut self, path: &Path) {
        let panel = self.active_panel_mut();
        if let Some(name) = util::child_name(&panel.cwd, path) {
            panel.select_name(&name);
        }
    }

    fn confirm_delete(&mut self, to_trash: bool) {
        let paths = self.active_panel().selection();
        if paths.is_empty() {
            return;
        }
        let what = describe(&paths);
        let (title, lines) = if to_trash {
            ("Delete", vec![format!("Move {what} to {}?", platform::TRASH_NAME)])
        } else {
            ("Delete permanently", vec![format!("Permanently delete {what}?"), "This cannot be undone.".into()])
        };
        self.dialog = Some(Dialog::Confirm {
            title: title.into(),
            lines,
            // Permanent deletion defaults to No.
            focus: if to_trash { 0 } else { 1 },
            danger: !to_trash,
            action: ConfirmAction::Delete { paths, to_trash },
        });
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
        let needs_quotes = name.chars().any(|c| c.is_whitespace() || "'\"&()[]{};,$`#@|<>".contains(c));
        let mut text = if needs_quotes { self.config.shell.quote(name) } else { name.clone() };
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
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        match cd_argument(&line) {
            Some(arg) => {
                let panel = self.active_panel();
                let cwd = panel.cwd.clone();
                let target = match util::bare_drive(arg) {
                    // "cd e:" returns to where this panel was on E: (and stays put when already there).
                    Some(root) if util::same_volume(&root, &cwd) => return,
                    Some(root) => panel.dir_on_drive(&root),
                    None if arg.is_empty() => util::home_dir().unwrap_or(cwd),
                    None => util::resolve(&cwd, arg),
                };
                self.change_dir(self.active, target);
            }
            None => self.run_in_shell(line),
        }
    }

    fn history_step(&mut self, direction: isize) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() - 1;
        let pos = match (self.history_pos, direction < 0) {
            (None, true) => Some(last),
            (None, false) => None,
            (Some(p), true) => Some(p.saturating_sub(1)),
            (Some(p), false) if p < last => Some(p + 1),
            (Some(_), false) => None,
        };
        self.history_pos = pos;
        match pos {
            Some(p) => self.cmdline.set(self.history[p].clone()),
            None => self.cmdline.clear(),
        }
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
        Question::Failed { path, error } => Dialog::Question {
            title: "Error".into(),
            lines: vec![path.display().to_string(), error],
            buttons: dialog::FAILED,
            answers: dialog::FAILED_ANSWERS,
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
        assert_eq!(app.external.take(), Some(External::Edit(file.clone())));
        app.on_key(key(KeyCode::F(3)));
        assert_eq!(app.external.take(), Some(External::View(file)));
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
        let saved = Settings::load(Some(&file));
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
        assert_eq!(app.external.take(), Some(External::Edit(big.clone())));

        // F3 shows it in the pager; without a pager it asks too.
        app.config.pager = Some(vec!["less".into()]);
        app.on_key(key(KeyCode::F(3)));
        assert_eq!(app.external.take(), Some(External::Page(big.clone())));
        app.config.pager = None;
        app.on_key(key(KeyCode::F(3)));
        assert!(matches!(app.dialog, Some(Dialog::Confirm { .. })));
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.external.take(), Some(External::View(big)));
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
