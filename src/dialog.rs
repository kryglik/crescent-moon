//! Dialogs and the single-line text editor used by them and by the command line.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

use crate::ops::Answer;
use crate::platform::DriveInfo;
use crate::util;

/// What happens when a yes/no confirmation is answered with Yes.
pub enum ConfirmAction {
    Quit,
    Delete { paths: Vec<PathBuf>, to_trash: bool },
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
}

pub enum Dialog {
    Message { title: String, lines: Vec<String>, error: bool },
    Confirm { title: String, lines: Vec<String>, focus: usize, danger: bool, action: ConfirmAction },
    Input { title: String, prompt: String, edit: LineEdit, action: InputAction },
    Drives { side: usize, drives: Vec<DriveInfo>, cursor: usize },
    /// The list itself lives in `App::hotlist`.
    Hotlist { cursor: usize },
    Help { scroll: usize },
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
pub const OVERWRITE: &[Button] =
    &[button("Yes", 'y'), button("No", 'n'), button("All", 'a'), button("None", 'o'), button("Cancel", 'c')];
pub const OVERWRITE_ANSWERS: &[Answer] = &[Answer::Yes, Answer::No, Answer::All, Answer::None, Answer::Cancel];
pub const FAILED: &[Button] =
    &[button("Retry", 'r'), button("Skip", 's'), button("Skip all", 'k'), button("Cancel", 'c')];
pub const FAILED_ANSWERS: &[Answer] = &[Answer::Retry, Answer::Skip, Answer::SkipAll, Answer::Cancel];

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

pub const HELP: &[&str] = &[
    "Panels",
    "  Arrows, PgUp/PgDn, Home/End  move the cursor",
    "  Enter        open a directory, or a file in its app",
    "  Backspace    parent directory (with an empty command line)",
    "  Tab          switch panel",
    "  Alt+F1/F2    choose the drive of the left/right panel",
    "               (back to the panel's last folder on that drive)",
    "  Ctrl+\\ or Ctrl+D   favourite folders (hotlist)",
    "  Ctrl+Space   size of the directory (on '..': all of them)",
    "  Insert, Shift+Up/Down   mark files",
    "  + - *        mark by pattern, unmark by pattern, invert",
    "  Ctrl+S       quick search by name",
    "  F9           sort by name, extension, size, time",
    "  Ctrl+R       reload       Alt+. or Ctrl+H  hidden files",
    "  Ctrl+U       swap panels",
    "  Alt+I/Alt+O  other panel: same dir / dir under cursor",
    "",
    "Files",
    "  F2 rename    F3 view    F4 edit    Shift+F4 new file",
    "  F5 copy      F6 move    F7 make directory",
    "  F8, Delete               move to the Recycle Bin",
    "  Shift+F8, Shift+Delete   delete permanently",
    "",
    "Mouse",
    "  click        select       double-click  open",
    "  right-click  mark         wheel         scroll",
    "  click Name / Size / Modify time to sort, F-key labels to run",
    "  Shift+drag selects text; rmc --no-mouse leaves the mouse alone",
    "",
    "Command line",
    "  Type a command, Enter runs it in the shell, in the panel's folder",
    "  cd DIR       change directory (cd e: switches drive)",
    "  Ctrl+Enter   put the file name on the command line",
    "  Alt+P/Alt+N  previous/next command",
    "  Esc          clear the command line",
    "",
    "Shell",
    "  Ctrl+O       switch between the panels and the shell. It is one",
    "               pwsh for the whole session; panel and shell follow",
    "               each other's cd",
    "  Shift+PgUp/PgDn, wheel   scroll back through its output",
    "",
    "  F10          quit",
    "",
    "Editor and viewer: RMC_EDITOR, RMC_VIEWER (nvim, nvim -R)",
    "Shell: RMC_SHELL (pwsh, then powershell, then cmd)",
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
