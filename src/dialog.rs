//! Dialogs and the single-line text editor used by them and by the command line.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

use crate::git::Worktree;
use crate::ops::Answer;
use crate::options::OptionsAutomatic;
use crate::panel::SortBy;
use crate::platform::DriveInfo;
use crate::settings::Settings;
use crate::survey::Survey;
use crate::util;

/// What happens when a yes/no confirmation is answered with Yes.
pub enum ConfirmAction {
    Quit,
    /// A big file in the editor or viewer after all.
    Edit(PathBuf, Option<u64>),
    View(PathBuf, Option<u64>),
    /// Packing into a zip that is there already: replace it.
    Pack(Vec<PathBuf>, PathBuf),
}

/// What happens with the text of an input dialog.
pub enum InputAction {
    Copy(Vec<PathBuf>),
    Move(Vec<PathBuf>),
    Rename(PathBuf),
    Mkdir,
    NewFile,
    Select,
    Unselect,
    /// Ctrl+F: the active panel's file filter.
    Filter,
    /// F5 in an archive, or Alt+F9 on one: these paths inside the zip `archive`, out into the
    /// folder typed.
    Extract { archive: PathBuf, items: Vec<String> },
    /// Alt+F5: these into the zip typed.
    Pack(Vec<PathBuf>),
}

pub enum Dialog {
    Message { title: String, lines: Vec<String>, error: bool },
    Confirm { title: String, lines: Vec<String>, focus: usize, danger: bool, action: ConfirmAction },
    Input { title: String, prompt: String, edit: LineEdit, action: InputAction },
    Drives { side: usize, drives: Vec<DriveInfo>, cursor: usize },
    /// The list itself lives in `App::hotlist`.
    Hotlist { cursor: usize },
    /// Alt+H: folders visited lately (`App::history_rows`), narrowed by what is typed.
    History { filter: LineEdit, cursor: usize },
    /// F12: every command with its key (`COMMANDS`), narrowed by what is typed.
    Commands { filter: LineEdit, cursor: usize },
    /// Alt+W: the worktrees of the repository whose working tree at `here` the panel is in,
    /// narrowed by what is typed (`worktree_rows`).
    Worktrees { filter: LineEdit, cursor: usize, worktrees: Vec<Worktree>, here: PathBuf },
    /// Alt+A: what to set for each of `platform::ATTRIBUTES` on `paths`; None leaves it as each
    /// file has it (they differ).
    Attributes { paths: Vec<PathBuf>, states: Vec<Option<bool>>, cursor: usize },
    Help { scroll: usize },
    /// F9: the sort order of panel `side`, which is sorted by `current` (`reverse`d).
    Sort { side: usize, cursor: usize, current: SortBy, reverse: bool },
    /// F8 / Shift+F8: the question, filled in by `survey` while it counts. The buttons are
    /// Yes, No, and once something needs administrator rights (on Windows) As admin.
    Delete { paths: Vec<PathBuf>, to_trash: bool, focus: usize, survey: Survey },
    /// Ctrl+P: `settings` being edited; `edit` holds the text of the row under the cursor
    /// when that is a text row.
    Options { cursor: usize, edit: LineEdit, settings: Settings, automatic: OptionsAutomatic },
    /// A question from a running job; the answer goes back over `reply`.
    Question {
        title: String,
        lines: Vec<String>,
        buttons: &'static [Button],
        answers: &'static [Answer],
        focus: usize,
        error: bool,
        reply: Sender<Answer>,
    },
}

#[derive(Clone, Copy)]
pub struct Button {
    pub label: &'static str,
    pub hotkey: char,
}

const fn button(label: &'static str, hotkey: char) -> Button {
    Button { label, hotkey }
}

pub const OK: &[Button] = &[button("OK", 'o')];
pub const YES_NO: &[Button] = &[button("Yes", 'y'), button("No", 'n')];
pub const YES_NO_ADMIN: &[Button] = &[button("Yes", 'y'), button("No", 'n'), button("As admin", 'a')];
pub const OVERWRITE: &[Button] =
    &[button("Yes", 'y'), button("No", 'n'), button("All", 'a'), button("None", 'o'), button("Cancel", 'c')];
pub const OVERWRITE_ANSWERS: &[Answer] = &[Answer::Yes, Answer::No, Answer::All, Answer::None, Answer::Cancel];
pub const FAILED: &[Button] =
    &[button("Retry", 'r'), button("Skip", 's'), button("Skip all", 'k'), button("Cancel", 'c')];
pub const FAILED_ANSWERS: &[Answer] = &[Answer::Retry, Answer::Skip, Answer::SkipAll, Answer::Cancel];
/// A permission error while deleting, which an administrator can get past.
pub const FAILED_ADMIN: &[Button] = &[
    button("Retry", 'r'),
    button("Skip", 's'),
    button("Skip all", 'k'),
    button("As admin", 'a'),
    button("Cancel", 'c'),
];
pub const FAILED_ADMIN_ANSWERS: &[Answer] =
    &[Answer::Retry, Answer::Skip, Answer::SkipAll, Answer::AsAdmin, Answer::Cancel];

/// The delete question's buttons: As admin joins Yes and No (keeping their places) once
/// the survey finds something only an administrator may delete, on Windows, where an
/// administrator can reach it (not on a network drive).
pub fn delete_buttons(tally: &crate::survey::Tally) -> &'static [Button] {
    if admin_offered(tally) { YES_NO_ADMIN } else { YES_NO }
}

pub fn admin_offered(tally: &crate::survey::Tally) -> bool {
    tally.denied.as_ref().is_some_and(|(path, _)| crate::platform::admin_can_reach(path))
}

pub enum ButtonOutcome {
    Pressed(usize),
    Cancel,
    Pending,
}

/// Arrow keys and Tab move between buttons, Enter presses, a hotkey presses directly, Esc cancels.
pub fn handle_buttons(buttons: &[Button], focus: &mut usize, key: &KeyEvent) -> ButtonOutcome {
    let n = buttons.len();
    match key.code {
        KeyCode::Left | KeyCode::BackTab => {
            *focus = (*focus + n - 1) % n;
            ButtonOutcome::Pending
        }
        KeyCode::Right | KeyCode::Tab => {
            *focus = (*focus + 1) % n;
            ButtonOutcome::Pending
        }
        KeyCode::Enter => ButtonOutcome::Pressed(*focus),
        KeyCode::Esc => ButtonOutcome::Cancel,
        KeyCode::Char(c) => buttons
            .iter()
            .position(|b| b.hotkey.eq_ignore_ascii_case(&c))
            .map_or(ButtonOutcome::Pending, ButtonOutcome::Pressed),
        _ => ButtonOutcome::Pending,
    }
}

/// A key that types a character. AltGr arrives as Ctrl+Alt on Windows (needed for @, {, [ on
/// Czech and other keyboards), so that counts as typing too.
pub fn is_text_input(key: &KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    matches!(key.code, KeyCode::Char(_)) && ctrl == alt
}

/// Single-line text input with a cursor (counted in characters).
#[derive(Clone, Debug, Default)]
pub struct LineEdit {
    text: String,
    cursor: usize,
}

impl LineEdit {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        LineEdit { text, cursor }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, text: impl Into<String>) {
        *self = LineEdit::new(text);
    }

    pub fn clear(&mut self) {
        *self = LineEdit::default();
    }

    fn byte_index(&self, chars: usize) -> usize {
        self.text.char_indices().nth(chars).map_or(self.text.len(), |(i, _)| i)
    }

    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_index(self.cursor);
        self.text.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    /// Applies an editing key; false if the key is not an editing key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.text.chars().count();
        match key.code {
            KeyCode::Char(c) if is_text_input(key) => self.insert_str(c.encode_utf8(&mut [0; 4])),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('k') if ctrl => {
                let at = self.byte_index(self.cursor);
                self.text.truncate(at);
            }
            // The word before the cursor, as in bash.
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut start = self.cursor;
                while start > 0 && chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                while start > 0 && !chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                let (from, to) = (self.byte_index(start), self.byte_index(self.cursor));
                self.text.replace_range(from..to, "");
                self.cursor = start;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte_index(self.cursor);
                self.text.remove(at);
            }
            KeyCode::Delete if self.cursor < len => {
                let at = self.byte_index(self.cursor);
                self.text.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// The part of the text to show in a field `width` columns wide, scrolled so the cursor
    /// stays visible, and the cursor's column within it.
    pub fn view(&self, width: usize) -> (String, usize) {
        let chars: Vec<char> = self.text.chars().collect();
        let columns = |range: &[char]| range.iter().map(|c| c.width().unwrap_or(0)).sum::<usize>();
        let mut start = 0;
        while start < self.cursor && columns(&chars[start..self.cursor]) >= width.max(1) {
            start += 1;
        }
        let visible: String = chars[start..].iter().collect();
        (util::take_width(&visible, width).to_string(), columns(&chars[start..self.cursor]))
    }
}

/// A command in the F12 list: what it does, its key as shown, and the key that runs it.
pub struct Command {
    pub label: &'static str,
    pub keys: &'static str,
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

const fn command(label: &'static str, keys: &'static str, code: KeyCode, modifiers: KeyModifiers) -> Command {
    Command { label, keys, code, modifiers }
}

const NONE: KeyModifiers = KeyModifiers::NONE;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;

/// F12: everything rmc does, for finding a command (and learning its key) by typing.
pub const COMMANDS: &[Command] = &[
    command("Keys: all of them (help)", "F1", KeyCode::F(1), NONE),
    command("Rename", "F2", KeyCode::F(2), NONE),
    command("Compare the folders: mark what the other lacks", "Shift+F2", KeyCode::F(2), SHIFT),
    command("View the file", "F3", KeyCode::F(3), NONE),
    command("Edit the file", "F4", KeyCode::F(4), NONE),
    command("New file", "Shift+F4", KeyCode::F(4), SHIFT),
    command("Copy", "F5", KeyCode::F(5), NONE),
    command("Pack into a zip", "Alt+F5", KeyCode::F(5), ALT),
    command("Move", "F6", KeyCode::F(6), NONE),
    command("Make a folder", "F7", KeyCode::F(7), NONE),
    command("Find text in files below (like ag)", "Alt+F7", KeyCode::F(7), ALT),
    command("Delete to the Recycle Bin", "F8", KeyCode::F(8), NONE),
    command("Delete permanently", "Shift+F8", KeyCode::F(8), SHIFT),
    command("Sort by name, extension, size, time", "F9", KeyCode::F(9), NONE),
    command("Unpack the zip under the cursor", "Alt+F9", KeyCode::F(9), ALT),
    command("Quit", "F10", KeyCode::F(10), NONE),
    command("Drive of the left panel", "Alt+F1", KeyCode::F(1), ALT),
    command("Drive of the right panel", "Alt+F2", KeyCode::F(2), ALT),
    command("Favourite folders (hotlist)", "Ctrl+D", KeyCode::Char('d'), CTRL),
    command("Folder history", "Alt+H", KeyCode::Char('h'), ALT),
    command("Back to the previous folder", "Alt+Left", KeyCode::Left, ALT),
    command("Forward again", "Alt+Right", KeyCode::Right, ALT),
    command("Quick view of the file in the other panel", "Ctrl+Q", KeyCode::Char('q'), CTRL),
    command("New tab (this folder)", "Ctrl+T", KeyCode::Char('t'), CTRL),
    command("Close the tab", "Ctrl+W", KeyCode::Char('w'), CTRL),
    command("Filter: show only matching files", "Ctrl+F", KeyCode::Char('f'), CTRL),
    command("Quick search by name", "Ctrl+S", KeyCode::Char('s'), CTRL),
    command("Mark everything", "Ctrl+A", KeyCode::Char('a'), CTRL),
    command("Mark files matching a pattern", "+", KeyCode::Char('+'), NONE),
    command("Unmark files matching a pattern", "-", KeyCode::Char('-'), NONE),
    command("Invert the marks", "*", KeyCode::Char('*'), NONE),
    command("Files to the clipboard (copy)", "Ctrl+C", KeyCode::Char('c'), CTRL),
    command("Files to the clipboard (cut)", "Ctrl+X", KeyCode::Char('x'), CTRL),
    command("Paths to the clipboard as text", "Ctrl+Shift+C", KeyCode::Char('C'), CTRL.union(SHIFT)),
    command("Paste files from the clipboard", "Alt+V", KeyCode::Char('v'), ALT),
    command("Sizes of the folders", "Ctrl+Space", KeyCode::Char(' '), CTRL),
    command("Reload the panels", "Ctrl+R", KeyCode::Char('r'), CTRL),
    command("Hidden files on / off", "Ctrl+H", KeyCode::Char('h'), CTRL),
    command("Swap the panels", "Ctrl+U", KeyCode::Char('u'), CTRL),
    command("Other panel: this folder", "Alt+I", KeyCode::Char('i'), ALT),
    command("Other panel: the folder under the cursor", "Alt+O", KeyCode::Char('o'), ALT),
    command("Git worktrees: go to another one", "Alt+W", KeyCode::Char('w'), ALT),
    command("Open with… (Windows asks which app)", "Shift+Enter", KeyCode::Enter, SHIFT),
    command("Attributes: read-only, hidden… (permissions)", "Alt+A", KeyCode::Char('a'), ALT),
    command("Shell (switch to it and back)", "Ctrl+O", KeyCode::Char('o'), CTRL),
    command("Options", "Ctrl+P", KeyCode::Char('p'), CTRL),
];

/// The commands whose label or key holds every word of `filter` (any case).
pub fn matching_commands(filter: &str) -> Vec<&'static Command> {
    COMMANDS.iter().filter(|c| matches_words(&format!("{} {}", c.label, c.keys), filter)).collect()
}

/// Whether `text` holds every word of `filter`, in any case.
fn matches_words(text: &str, filter: &str) -> bool {
    let text = text.to_lowercase();
    filter.split_whitespace().all(|word| text.contains(&word.to_lowercase()))
}

/// The worktrees whose branch or folder holds every word of `filter`.
pub fn worktree_rows<'a>(worktrees: &'a [Worktree], filter: &str) -> Vec<&'a Worktree> {
    worktrees.iter().filter(|w| matches_words(&format!("{} {}", w.head.label(), w.path.display()), filter)).collect()
}

pub const HELP: &[&str] = &[
    "  F12          every command, found by typing part of its name",
    "",
    "Panels",
    "  Arrows, PgUp/PgDn, Home/End  move the cursor",
    "  Enter        open a directory, or a file in its app",
    "               (a Windows shortcut to a folder opens it here)",
    "  Shift+Enter  open with… (Windows asks which app)",
    "  Backspace    parent directory (with an empty command line)",
    "  Tab          switch panel",
    "  Alt+F1/F2    choose the drive of the left/right panel",
    "               (back to the panel's last folder on that drive)",
    "  Ctrl+\\ or Ctrl+D   favourite folders (hotlist)",
    "  Alt+Left/Right   back / forward through folders (or Alt+Y/U)",
    "  Alt+H, Alt+Down  folder history: type to narrow, Enter goes",
    "  Ctrl+Space   size of the directory (on '..': all of them)",
    "  Left/Right   up / into a folder (with an empty command line)",
    "  Ctrl+PgUp/PgDn   up / into a folder",
    "  Insert, Shift+Up/Down   mark files",
    "  + - *        mark by pattern, unmark by pattern, invert",
    "  Ctrl+A       mark everything",
    "  Alt+A        attributes (read-only, hidden…; on Linux permissions)",
    "  Ctrl+F       filter: only files matching *.jpg *.cr2, !*.tmp…",
    "  Ctrl+S, Alt+S   quick search by name (again: next match)",
    "  Alt+F7, Alt+?   find text in files below (regex, like ag)",
    "  F9           sort by name, extension, size or time;",
    "               F9 F9 (or the same one again) flips A-Z / Z-A",
    "  Ctrl+R       reload       Alt+. or Ctrl+H  hidden files",
    "  Ctrl+U       swap panels",
    "  Ctrl+T, Ctrl+W   new tab here, close the tab; Alt+1…9 go to one",
    "  Shift+F2     compare: mark what the other side lacks or has older",
    "  Ctrl+Q       quick view in the other panel (Tab: scroll it)",
    "  Alt+I/Alt+O  other panel: same dir / dir under cursor",
    "",
    "Files",
    "  F2 rename    F3 view    F4 edit    Shift+F4 new file",
    "               over 10 MB: F3 shows it in less, F4 asks first",
    "  F5 copy      F6 move    F7 make directory",
    "  Ctrl+C/Ctrl+X  copy/cut files to the clipboard (Explorer pastes)",
    "  Ctrl+Shift+C   their full paths to the clipboard, as text",
    "  Alt+V        paste files copied or cut here or in Explorer",
    "  F8, Delete               move to the Recycle Bin",
    "  Shift+F8, Shift+Delete   delete permanently",
    "  The question counts what goes; As admin deletes protected files",
    "  (Windows asks for administrator rights first)",
    "  Esc          close a dialog, stop a copy/move/delete",
    "",
    "Zip archives",
    "  Enter        open a zip as a folder (read-only); .. leaves it",
    "  F5           copy out of it    F3, Enter   view, open a file in it",
    "  Alt+F5       pack into a zip   Alt+F9      unpack the zip",
    "",
    "Git (in a repository)",
    "  The row below the path: the branch (cyan in a linked worktree),",
    "  a rebase or merge under way, ↑↓ commits ahead/behind, and how",
    "  many files are in conflict (!), changed (~), untracked (?)",
    "  Beside names: M changed  A added  R renamed  ? untracked",
    "  ! conflict  • changes inside a folder; ignored files are dim",
    "  Alt+W        worktrees of the repository: Enter goes to one",
    "               (to the same folder in it, when it has one)",
    "",
    "Mouse",
    "  click        select       double-click  open",
    "  right-click  mark         wheel         scroll",
    "  click Name / Size / Modify time to sort (again: flip the order)",
    "  click the F-key labels to run them",
    "  Shift+drag selects text; rmc --no-mouse leaves the mouse alone",
    "",
    "Command line",
    "  Type a command, Enter runs it in the shell, in the panel's folder",
    "  cd DIR       change directory (cd e: switches drive, cd - back)",
    "  Ctrl+Enter   put the file name on the command line",
    "  Alt+P/Alt+N  previous/next command (kept between sessions)",
    "  Esc          clear the command line",
    "",
    "Shell",
    "  Ctrl+O       switch between the panels and the shell. It is one",
    "               shell for the whole session; panel and shell follow",
    "               each other's cd",
    "  Shift+PgUp/PgDn, wheel   scroll back through its output",
    "",
    "  Ctrl+P       options (saved in settings.ini)",
    "",
    "Find (Alt+F7)",
    "  Text: a regex; lowercase ignores case, a capital makes it count",
    "  Files: *.rs *.toml, !*test* to leave out, or part of a name",
    "  Tab switches field   Up/Down, PgUp/PgDn move   Alt+U all files",
    "  Enter goes there   F3/F4 view/edit at the line   Esc closes",
    "",
    "  F10          quit",
    "",
    "Editor, viewer, pager, shell: Ctrl+P, or when empty there",
    "  RMC_EDITOR, RMC_VIEWER, RMC_PAGER, RMC_SHELL (nvim, less, pwsh)",
    "Shell: RMC_SHELL (pwsh, then powershell, then cmd; on Linux $SHELL)",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn editing_with_czech_characters() {
        let mut edit = LineEdit::new("kůň");
        edit.handle_key(&key(KeyCode::Left));
        edit.handle_key(&key(KeyCode::Backspace));
        assert_eq!(edit.text(), "kň");
        edit.handle_key(&key(KeyCode::Char('ů')));
        assert_eq!(edit.text(), "kůň");
        edit.handle_key(&key(KeyCode::Home));
        edit.handle_key(&key(KeyCode::Delete));
        assert_eq!(edit.text(), "ůň");
        let altgr = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        edit.handle_key(&altgr);
        assert_eq!(edit.text(), "@ůň");
    }

    /// The help box is 72 wide: 68 columns inside its border and padding.
    #[test]
    fn help_lines_fit_the_box() {
        for line in HELP {
            assert!(util::width(line) <= 68, "{} columns: {line}", util::width(line));
        }
    }

    #[test]
    fn deleting_the_word_before_the_cursor() {
        let mut edit = LineEdit::new("cp a.txt  Účty");
        let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
        edit.handle_key(&ctrl_w);
        assert_eq!(edit.text(), "cp a.txt  ");
        edit.handle_key(&ctrl_w);
        assert_eq!(edit.text(), "cp ");
    }

    #[test]
    fn view_scrolls_to_keep_the_cursor_visible() {
        let edit = LineEdit::new("abcdefghij");
        let (text, cursor) = edit.view(5);
        assert_eq!((text.as_str(), cursor), ("ghij", 4));
        let (text, cursor) = LineEdit::new("abc").view(10);
        assert_eq!((text.as_str(), cursor), ("abc", 3));
    }

    #[test]
    fn buttons_by_hotkey_and_arrows() {
        let mut focus = 0;
        assert!(matches!(handle_buttons(YES_NO, &mut focus, &key(KeyCode::Char('N'))), ButtonOutcome::Pressed(1)));
        assert!(matches!(handle_buttons(YES_NO, &mut focus, &key(KeyCode::Right)), ButtonOutcome::Pending));
        assert!(matches!(handle_buttons(YES_NO, &mut focus, &key(KeyCode::Enter)), ButtonOutcome::Pressed(1)));
        assert!(matches!(handle_buttons(YES_NO, &mut focus, &key(KeyCode::Esc)), ButtonOutcome::Cancel));
    }
}
