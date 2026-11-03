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
use crate::panel::{DirSize, Panel};
use crate::platform::{self, Shell};
use crate::sizes::Sizer;
use crate::util;

/// Something to run outside the TUI; the main loop gives it the terminal.
#[derive(Debug, PartialEq)]
pub enum External {
    Edit(PathBuf),
    View(PathBuf),
    Shell,
    Command(String),
}

pub struct Config {
    pub editor: Vec<String>,
    pub viewer: Vec<String>,
    pub shell: Shell,
    pub hotlist_file: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> Self {
        let editor = env_command(&["RMC_EDITOR", "VISUAL", "EDITOR"]).unwrap_or_else(|| vec!["nvim".into()]);
        let viewer = env_command(&["RMC_VIEWER"]).unwrap_or_else(|| {
            let mut viewer = editor.clone();
            if is_vim(&editor[0]) {
                viewer.push("-R".into());
            }
            viewer
        });
        Config { editor, viewer, shell: Shell::detect(), hotlist_file: Hotlist::default_file() }
    }
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
    sizer: Option<Sizer>,
    /// Free space of network/optical drives, arriving while the drive picker is open.
    drive_space: Option<Receiver<(PathBuf, Option<(u64, u64)>)>>,
    history: Vec<String>,
    history_pos: Option<usize>,
}

impl App {
    pub fn new(left: PathBuf, right: PathBuf, config: Config) -> Self {
        App {
            panels: [Panel::new(left, true), Panel::new(right, true)],
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
            sizer: None,
            drive_space: None,
            history: Vec::new(),
            history_pos: None,
        }
    }

    /// Whether background work is running (the main loop then redraws more often).
    pub fn is_busy(&self) -> bool {
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
        if self.dialog.is_some() {
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
            KeyCode::F(9) => {
                let panel = self.active_panel_mut();
                panel.sort = panel.sort.next();
                panel.reload();
            }
            KeyCode::F(10) => {
                self.dialog = Some(Dialog::Confirm {
                    title: "Quit".into(),
                    lines: vec!["Quit rmc?".into()],
                    focus: 0,
                    danger: false,
                    action: ConfirmAction::Quit,
                })
            }
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
            KeyCode::Char('o') if ctrl && !alt => self.external = Some(External::Shell),
            // Ctrl+\ is a dead key on the Czech layout, so Ctrl+D (as in Total Commander) works too.
            KeyCode::Char('\\' | 'd' | '\u{1c}') if ctrl && !alt => self.open_hotlist(),
            KeyCode::Char(' ') if ctrl && !alt => self.calculate_sizes(),
            KeyCode::Char('s') if ctrl && !alt => self.quicksearch = Some(String::new()),
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
        match key.code {
            KeyCode::Char('s') if ctrl => {
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
                    let wanted = format!("{}:", c.to_ascii_uppercase());
                    match drives.iter().find(|d| d.root.to_string_lossy().starts_with(&wanted)) {
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
            KeyCode::Char('\\' | 'd' | '\u{1c}') if ctrl => return None,
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

    /// Picks up directory sizes and the progress, questions and end of a running job.
    pub fn tick(&mut self) {
        if let Some(answers) = &self.drive_space {
            while let Ok((root, space)) = answers.try_recv() {
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
                for panel in &mut self.panels {
                    panel.set_dir_size(&update.dir, update.bytes, update.done);
                }
            }
            // Nobody is waiting for the result any more (the panel moved on): stop measuring.
            if sizer.is_busy() && !self.panels.iter().any(Panel::has_pending_sizes) {
                sizer.cancel();
            }
        }

        let Some(job) = self.job.as_mut() else { return };
        let mut finished = None;
        while let Ok(msg) = job.rx.try_recv() {
            match msg {
                Msg::Progress(progress) => job.progress = progress,
                Msg::Ask(question, reply) => self.dialog = Some(question_dialog(question, reply)),
                Msg::Done { errors, cancelled } => {
                    finished = Some((errors, cancelled));
                    break;
                }
            }
        }
        let Some((errors, cancelled)) = finished else { return };
        self.job = None;
        for panel in &mut self.panels {
            panel.clear_marks();
            // Sizes may be out of date after copying, moving or deleting.
            panel.dir_sizes.clear();
            panel.reload();
        }
        if cancelled {
            self.dialog = Some(Dialog::Message { title: "Cancelled".into(), lines: vec!["Stopped.".into()], error: false });
        } else if !errors.is_empty() {
            let mut lines = vec![format!("{} item(s) were not processed:", errors.len())];
            lines.extend(errors.iter().take(8).cloned());
            if errors.len() > 8 {
                lines.push(format!("… and {} more", errors.len() - 8));
            }
            self.dialog = Some(Dialog::Message { title: "Problems".into(), lines, error: true });
        }
    }

    /// Called after an editor, viewer, shell or command has run.
    pub fn after_external(&mut self, result: Result<(), String>) {
        self.reload_panels();
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
        let cwd = self.panels[side].cwd.to_string_lossy().to_lowercase();
        let cursor = drives
            .iter()
            .position(|d| cwd.starts_with(&d.root.to_string_lossy().to_lowercase()))
            .unwrap_or(0);
        let slow: Vec<PathBuf> = drives.iter().filter(|d| d.pending).map(|d| d.root.clone()).collect();
        self.drive_space = (!slow.is_empty()).then(|| platform::query_space_later(slow));
        self.dialog = Some(Dialog::Drives { side, drives, cursor });
    }

    /// Goes to the folder the panel last had open on that drive. Choosing the drive the panel
    /// is already on goes to its root, as in Total Commander.
    fn switch_drive(&mut self, side: usize, root: &Path) {
        let panel = &self.panels[side];
        let target = if util::same_volume(root, &panel.cwd) { root.to_path_buf() } else { panel.dir_on_drive(root) };
        self.change_dir(side, target);
    }

    pub(crate) fn open_current(&mut self) {
        let Some(entry) = self.active_panel().current().cloned() else { return };
        if entry.is_parent {
            self.go_up();
        } else if entry.is_dir {
            self.change_dir(self.active, entry.path);
        } else if entry.is_exec {
            self.external = Some(External::Command(self.config.shell.run_file_line(&entry.path)));
        } else if let Err(e) = platform::open_default(&entry.path) {
            self.error(e);
        }
    }

    fn view_current(&mut self) {
        match self.active_panel().current() {
            Some(entry) if entry.is_dir => self.open_current(),
            Some(entry) => self.external = Some(External::View(entry.path.clone())),
            None => {}
        }
    }

    fn edit_current(&mut self) {
        if let Some(entry) = self.active_panel().current()
            && !entry.is_dir
        {
            self.external = Some(External::Edit(entry.path.clone()));
        }
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
            None => self.external = Some(External::Command(line)),
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
        Config {
            editor: vec!["nvim".into()],
            viewer: vec!["nvim".into(), "-R".into()],
            shell: Shell::detect(),
            hotlist_file,
        }
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
        typing(&mut app, "cd deep\\inside");
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
        while app.is_busy() || app.active_panel().has_pending_sizes() {
            assert!(Instant::now() < deadline, "sizes did not arrive");
            std::thread::sleep(Duration::from_millis(10));
            app.tick();
        }
        let panel = app.active_panel();
        let size_of = |name: &str| panel.dir_size(panel.entries.iter().find(|e| e.name == name).unwrap()).unwrap();
        assert_eq!(size_of("big").bytes, 8000);
        assert_eq!(size_of("small").bytes, 3);
        assert_eq!(panel.total(), Some(8005));

        // Sorting by size (F9 twice: extension, then size) uses the measured sizes.
        app.on_key(key(KeyCode::F(9)));
        app.on_key(key(KeyCode::F(9)));
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
    fn command_line_cd_and_external_commands() {
        let tmp = TempDir::new("app-cmd");
        tmp.dir("sub dir");
        let mut app = test_app(tmp.path(), tmp.path());

        typing(&mut app, "cd \"sub dir\"");
        app.on_key(key(KeyCode::Enter));
        assert!(app.active_panel().cwd.ends_with("sub dir"));

        typing(&mut app, "cd..");
        app.on_key(key(KeyCode::Enter));
        assert!(util::same_path(&app.active_panel().cwd, tmp.path()));

        typing(&mut app, "git status");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.external.take(), Some(External::Command("git status".into())));
        assert!(app.cmdline.is_empty());

        // Alt+P brings it back.
        app.on_key(with_mods(KeyCode::Char('p'), KeyModifiers::ALT));
        assert_eq!(app.cmdline.text(), "git status");
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
}
