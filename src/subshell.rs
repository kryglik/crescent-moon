//! The persistent shell behind Ctrl+O. It runs in a pseudo-console (ConPTY) that rmc owns, so
//! it lives on while the panels are shown: variables stay, and a long command keeps running.
//! rmc keeps a copy of its screen (vt100) and draws it; keys typed while it is shown go to it.

use std::fs;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::dialog;
use crate::platform::{self, Shell};
use crate::util;

/// Lines of output kept for scrolling back (Shift+PgUp, the wheel).
const SCROLLBACK: usize = 5000;

/// Questions the pseudo-console asks its terminal. ConPTY waits for the answer to "where is the
/// cursor?" (`ESC[6n`) before it runs anything, so rmc has to reply like a real terminal.
#[derive(Default)]
struct Queries {
    cursor_position: bool,
    device_attributes: bool,
}

impl vt100::Callbacks for Queries {
    fn unhandled_csi(&mut self, _: &mut vt100::Screen, i1: Option<u8>, _: Option<u8>, params: &[&[u16]], c: char) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (c, i1) {
            ('n', None) if first == 6 => self.cursor_position = true,
            ('c', None) if first == 0 => self.device_attributes = true,
            _ => {}
        }
    }
}

type Screen = vt100::Parser<Queries>;
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// Feeds the shell's output into the screen copy and answers the terminal queries in it.
fn pump_output(mut reader: Box<dyn Read + Send>, screen: Arc<Mutex<Screen>>, writer: Writer) {
    let mut buf = vec![0u8; 16 * 1024];
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 {
            break;
        }
        let Ok(mut parser) = screen.lock() else { break };
        parser.process(&buf[..n]);
        let mut replies = String::new();
        let queries = std::mem::take(parser.callbacks_mut());
        if queries.cursor_position {
            let (row, col) = parser.screen().cursor_position();
            replies += &format!("\x1b[{};{}R", row + 1, col + 1);
        }
        if queries.device_attributes {
            replies += "\x1b[?1;0c";
        }
        drop(parser);
        if !replies.is_empty()
            && let Ok(mut writer) = writer.lock()
        {
            let _ = writer.write_all(replies.as_bytes()).and_then(|()| writer.flush());
        }
    }
}

pub struct Subshell {
    screen: Arc<Mutex<Screen>>,
    writer: Writer,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
    /// Where the shell's prompt reports `<number>|<folder>` (see `PWSH_SHELL_INIT`).
    report_file: PathBuf,
    /// The prompt number when rmc last typed into the shell. A later prompt means it is idle.
    prompt_at_last_input: u64,
    typed_anything: bool,
    /// The shell's folder when it was last shown; if it has moved by the time the panels come
    /// back, the panel follows.
    pub cwd_when_shown: Option<PathBuf>,
}

impl Subshell {
    /// Starts the shell in `cwd` with a `rows` x `cols` screen.
    pub fn start(shell: &Shell, cwd: &Path, rows: u16, cols: u16) -> Result<Self, String> {
        let error = |e: &dyn std::fmt::Display| e.to_string();
        let pair = native_pty_system().openpty(pty_size(rows, cols)).map_err(|e| error(&e))?;
        // Unique per shell: a new one may start while the old one's file is still being removed.
        static STARTED: AtomicU32 = AtomicU32::new(0);
        let n = STARTED.fetch_add(1, Ordering::Relaxed);
        let report_file = std::env::temp_dir().join(format!("rmc-{}-shell-{n}.txt", std::process::id()));
        let _ = fs::remove_file(&report_file);
        let mut command = CommandBuilder::from_argv(shell.interactive_argv());
        command.cwd(cwd);
        command.env("RMC_CWD_FILE", &report_file);
        let child = pair.slave.spawn_command(command).map_err(|e| error(&e))?;
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|e| error(&e))?;
        let writer: Writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(|e| error(&e))?));
        let screen = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            non_zero(rows),
            non_zero(cols),
            SCROLLBACK,
            Queries::default(),
        )));
        let (feed, replies) = (screen.clone(), writer.clone());
        thread::spawn(move || pump_output(reader, feed, replies));

        Ok(Subshell {
            screen,
            writer,
            master: pair.master,
            child,
            size: (rows, cols),
            report_file,
            prompt_at_last_input: 0,
            typed_anything: false,
            cwd_when_shown: None,
        })
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) == self.size || rows == 0 || cols == 0 {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(pty_size(rows, cols));
        if let Ok(mut parser) = self.screen.lock() {
            parser.screen_mut().set_size(non_zero(rows), non_zero(cols));
        }
    }

    /// Runs `f` on the shell's current screen.
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> Option<R> {
        self.screen.lock().ok().map(|parser| f(parser.screen()))
    }

    pub fn send_key(&mut self, key: &KeyEvent) {
        let bytes = encode_key(key);
        if !bytes.is_empty() {
            self.send(&bytes);
        }
    }

    pub fn send_text(&mut self, text: &str) {
        self.send(text.as_bytes());
    }

    fn send(&mut self, bytes: &[u8]) {
        self.scroll_to_bottom();
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(bytes).and_then(|()| writer.flush());
        }
        self.typed_anything = true;
        // Text typed before the first prompt is read at that prompt, so it counts as prompt 1's.
        self.prompt_at_last_input = self.report().map_or(1, |(number, _)| number.max(1));
    }

    /// Scrolls the view back (positive) or forward through the output.
    pub fn scroll(&self, lines: isize) {
        if let Ok(mut parser) = self.screen.lock() {
            let position = parser.screen().scrollback() as isize + lines;
            parser.screen_mut().set_scrollback(position.max(0) as usize);
        }
    }

    pub fn scroll_to_bottom(&self) {
        if let Ok(mut parser) = self.screen.lock() {
            parser.screen_mut().set_scrollback(0);
        }
    }

    pub fn has_exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    /// The number and folder of the shell's last prompt (PowerShell only).
    pub fn report(&self) -> Option<(u64, PathBuf)> {
        platform::parse_cwd_report(&fs::read_to_string(&self.report_file).ok()?)
    }

    /// Waiting at an empty prompt: a prompt has appeared since rmc last typed anything.
    /// A shell that hasn't shown its first prompt yet counts as idle until something is typed.
    pub fn is_idle(&self) -> bool {
        match self.report() {
            Some((number, _)) => number > self.prompt_at_last_input,
            None => !self.typed_anything,
        }
    }

    /// Before the shell is shown: if it sits idle in another folder than the panel's, `cd` it
    /// there, as mc does with its subshell. Remembers where the shell is for `moved_from`.
    pub fn follow_panel(&mut self, dir: &Path, shell: &Shell) {
        let here = self.report().map(|(_, folder)| folder);
        let elsewhere = here.as_deref().is_some_and(|folder| !util::same_path(folder, dir));
        if elsewhere && self.is_idle() && matches!(shell, Shell::PowerShell(_)) {
            self.send_text(&format!("__rmc_cd {}\r", shell.quote(&dir.display().to_string())));
            self.cwd_when_shown = Some(dir.to_path_buf());
        } else {
            // A brand-new shell has no report yet; it started in the panel's folder.
            self.cwd_when_shown = here.or_else(|| Some(dir.to_path_buf()));
        }
    }

    /// Where the shell went since it was shown, if it changed folder.
    pub fn moved_to(&self) -> Option<PathBuf> {
        let (_, here) = self.report()?;
        let moved = !self.cwd_when_shown.as_deref().is_some_and(|before| util::same_path(before, &here));
        moved.then_some(here)
    }
}

impl Drop for Subshell {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = fs::remove_file(&self.report_file);
    }
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

fn non_zero(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

/// The bytes a terminal (xterm, Windows Terminal) sends for a key. ConPTY turns them back into
/// Windows key events for the shell.
pub fn encode_key(key: &KeyEvent) -> Vec<u8> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // xterm's modifier parameter: 1 + Shift + 2·Alt + 4·Ctrl.
    let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi = |end: char| if m == 1 { format!("\x1b[{end}") } else { format!("\x1b[1;{m}{end}") };
    let tilde = |n: u8| if m == 1 { format!("\x1b[{n}~") } else { format!("\x1b[{n};{m}~") };
    let text = match key.code {
        // Plain and shifted characters, and AltGr ones (Ctrl+Alt on Windows).
        KeyCode::Char(c) if dialog::is_text_input(key) => c.to_string(),
        KeyCode::Char(c) if ctrl => match control_byte(c) {
            Some(byte) => (byte as char).to_string(),
            None => return Vec::new(),
        },
        KeyCode::Char(c) => format!("\x1b{c}"),
        KeyCode::Enter => "\r".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        // Windows Terminal's convention, which ConPTY expects: DEL is Backspace, BS is Ctrl+Backspace.
        KeyCode::Backspace if ctrl => "\x08".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up => csi('A'),
        KeyCode::Down => csi('B'),
        KeyCode::Right => csi('C'),
        KeyCode::Left => csi('D'),
        KeyCode::Home => csi('H'),
        KeyCode::End => csi('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let end = ['P', 'Q', 'R', 'S'][usize::from(n - 1)];
            if m == 1 { format!("\x1bO{end}") } else { csi(end) }
        }
        KeyCode::F(n) => match n {
            5 => tilde(15),
            6 => tilde(17),
            7 => tilde(18),
            8 => tilde(19),
            9 => tilde(20),
            10 => tilde(21),
            11 => tilde(23),
            12 => tilde(24),
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    text.into_bytes()
}

/// The control code for Ctrl+`c`: Ctrl+A is 1, Ctrl+Space is 0, Ctrl+\ is 0x1C, and so on.
fn control_byte(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
        ' ' | '@' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::time::{Duration, Instant};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode_key(&KeyEvent::new(code, modifiers))
    }

    #[test]
    fn keys_become_terminal_sequences() {
        let none = KeyModifiers::NONE;
        assert_eq!(key(KeyCode::Char('ů'), none), "ů".as_bytes());
        assert_eq!(key(KeyCode::Char('A'), KeyModifiers::SHIFT), b"A");
        assert_eq!(key(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT), b"@", "AltGr");
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), [3]);
        assert_eq!(key(KeyCode::Char(' '), KeyModifiers::CONTROL), [0]);
        assert_eq!(key(KeyCode::Char('f'), KeyModifiers::ALT), b"\x1bf");
        assert_eq!(key(KeyCode::Enter, none), b"\r");
        assert_eq!(key(KeyCode::Backspace, none), [0x7f]);
        assert_eq!(key(KeyCode::Up, none), b"\x1b[A");
        assert_eq!(key(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D");
        assert_eq!(key(KeyCode::Delete, none), b"\x1b[3~");
        assert_eq!(key(KeyCode::PageUp, KeyModifiers::SHIFT), b"\x1b[5;2~");
        assert_eq!(key(KeyCode::F(1), none), b"\x1bOP");
        assert_eq!(key(KeyCode::F(5), none), b"\x1b[15~");
        assert_eq!(key(KeyCode::F(12), KeyModifiers::CONTROL), b"\x1b[24;5~");
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn screen_text(shell: &Subshell) -> String {
        shell.with_screen(|screen| screen.contents()).unwrap_or_default()
    }

    #[test]
    fn a_real_shell_keeps_its_state_and_follows_the_panel() {
        let shell_kind = Shell::detect();
        if !matches!(shell_kind, Shell::PowerShell(_)) {
            eprintln!("no PowerShell here; skipped");
            return;
        }
        let tmp = TempDir::new("subshell");
        let other = tmp.dir("Účty");
        let mut shell = Subshell::start(&shell_kind, tmp.path(), 30, 100).unwrap();
        assert!(shell.is_idle(), "a new shell is ready for typing");

        // A command typed into it runs and its output shows on the screen.
        shell.send_text("$rmcTest = 6 * 7; \"answer=$rmcTest\"\r");
        wait_for("the command's output", || screen_text(&shell).contains("answer=42"));
        wait_for("the next prompt", || shell.is_idle());

        // rmc moves it to the panel's folder; the variable survives.
        shell.follow_panel(&other, &shell_kind);
        wait_for("the cd", || shell.report().is_some_and(|(_, dir)| util::same_path(&dir, &other)));
        assert_eq!(shell.moved_to(), None, "rmc's own cd doesn't count as the user moving");
        shell.send_text("\"still=$rmcTest\"; cd ..\r");
        wait_for("the variable", || screen_text(&shell).contains("still=42"));
        wait_for("the user's cd", || shell.moved_to().is_some_and(|dir| util::same_path(&dir, tmp.path())));

        shell.send_text("exit\r");
        wait_for("exit", || shell.has_exited());
    }
}
