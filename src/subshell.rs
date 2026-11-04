//! The persistent shell behind Ctrl+O. It runs in a pseudo-console (ConPTY) that rmc owns, so
//! it lives on while the panels are shown: variables stay, and a long command keeps running.
//! rmc keeps a copy of its screen (vt100) and draws it; keys typed while it is shown go to it.

use std::fs;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::dialog;
use crate::platform::{self, Shell, ShellReport};
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

/// The shell's debug log, written when `RMC_LOG` names a file: what goes in and out of the
/// pseudo-console, so a shell that gets stuck can be diagnosed.
#[derive(Clone, Default)]
pub struct Log(Option<Arc<Mutex<fs::File>>>);

impl Log {
    fn from_env() -> Self {
        let file = std::env::var_os("RMC_LOG")
            .and_then(|path| fs::OpenOptions::new().create(true).append(true).open(path).ok());
        Log(file.map(|f| Arc::new(Mutex::new(f))))
    }

    fn write(&self, what: &str) {
        if let Some(file) = &self.0
            && let Ok(mut file) = file.lock()
        {
            let now = chrono::Local::now().format("%H:%M:%S%.3f");
            let _ = writeln!(file, "{now} {what}");
        }
    }

    /// Bytes as readable text: control characters escaped, long chunks shortened.
    fn bytes(&self, what: &str, bytes: &[u8]) {
        if self.0.is_none() {
            return;
        }
        let shown: String = String::from_utf8_lossy(&bytes[..bytes.len().min(400)]).escape_debug().collect();
        let more = if bytes.len() > 400 { "…" } else { "" };
        self.write(&format!("{what} {} bytes: {shown}{more}", bytes.len()));
    }
}

/// What the reader thread tells the UI.
#[derive(Default)]
struct Activity {
    /// The shell has written something (its start-up is over).
    output_seen: AtomicBool,
    /// The screen changed since the UI last drew it.
    changed: AtomicBool,
}

/// Feeds the shell's output into the screen copy and answers the terminal queries in it.
fn pump_output(
    mut reader: Box<dyn Read + Send>,
    screen: Arc<Mutex<Screen>>,
    input: Sender<Vec<u8>>,
    activity: Arc<Activity>,
    log: Log,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break log.write("output: end"),
            Ok(n) => n,
            Err(e) => break log.write(&format!("output: read error {e}")),
        };
        log.bytes("output", &buf[..n]);
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
        activity.output_seen.store(true, Ordering::Relaxed);
        activity.changed.store(true, Ordering::Relaxed);
        if !replies.is_empty() {
            log.bytes("reply", replies.as_bytes());
            let _ = input.send(replies.into_bytes());
        }
    }
}

/// Writes keys and replies to the shell on a thread of its own, so a shell that stops
/// reading its input can never freeze rmc.
fn pump_input(mut writer: Box<dyn Write + Send>, chunks: mpsc::Receiver<Vec<u8>>, log: Log) {
    for chunk in chunks {
        if let Err(e) = writer.write_all(&chunk).and_then(|()| writer.flush()) {
            log.write(&format!("input: write error {e}"));
            break;
        }
    }
}

pub struct Subshell {
    screen: Arc<Mutex<Screen>>,
    input: Sender<Vec<u8>>,
    activity: Arc<Activity>,
    started: Instant,
    log: Log,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
    /// Where the shell's prompt reports `<number>|<folder>[|f]` (see `PWSH_SHELL_INIT`).
    report_file: PathBuf,
    /// Where rmc puts the folder for the shell's follow key.
    request_file: PathBuf,
    /// rmc's start-up files for bash and zsh.
    init_dir: PathBuf,
    /// The prompt number when rmc last typed into the shell. A later prompt means it is idle.
    prompt_at_last_input: u64,
    typed_anything: bool,
    /// Whether the shell's prompt writes `report_file` (PowerShell, bash, zsh).
    reports_prompts: bool,
    /// The shell's folder when it was last shown; if it has moved by the time the panels come
    /// back, the panel follows.
    pub cwd_when_shown: Option<PathBuf>,
}

impl Subshell {
    /// Starts the shell in `cwd` with a `rows` x `cols` screen.
    pub fn start(shell: &Shell, cwd: &Path, rows: u16, cols: u16) -> Result<Self, String> {
        let log = Log::from_env();
        log.write(&format!("start {shell:?} in {} at {rows}x{cols}", cwd.display()));
        let error = |e: &dyn std::fmt::Display| {
            log.write(&format!("start failed: {e}"));
            e.to_string()
        };
        let pair = native_pty_system().openpty(pty_size(rows, cols)).map_err(|e| error(&e))?;
        // Unique per shell: a new one may start while the old one's file is still being removed.
        static STARTED: AtomicU32 = AtomicU32::new(0);
        let n = STARTED.fetch_add(1, Ordering::Relaxed);
        let name = format!("rmc-{}-shell-{n}", std::process::id());
        let file = |what: &str| std::env::temp_dir().join(format!("{name}{what}.txt"));
        let (report_file, request_file) = (file(""), file("-cd"));
        let init_dir = std::env::temp_dir().join(&name);
        let _ = fs::remove_file(&report_file);
        let session = shell.session(&init_dir).map_err(|e| error(&e))?;
        let mut command = CommandBuilder::from_argv(session.argv);
        for (key, value) in session.env {
            command.env(key, value);
        }
        // What rmc's screen copy (vt100) understands; without it readline and zle fall back
        // to a dumb terminal. ConPTY on Windows ignores it.
        if cfg!(unix) {
            command.env("TERM", "xterm-256color");
        }
        command.cwd(cwd);
        command.env("RMC_CWD_FILE", &report_file);
        command.env("RMC_REQUEST_FILE", &request_file);
        let child = pair.slave.spawn_command(command).map_err(|e| error(&e))?;
        log.write(&format!("spawned pid {:?}, report file {}", child.process_id(), report_file.display()));
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|e| error(&e))?;
        let writer = pair.master.take_writer().map_err(|e| error(&e))?;
        let screen = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            non_zero(rows),
            non_zero(cols),
            SCROLLBACK,
            Queries::default(),
        )));
        let activity = Arc::new(Activity::default());
        let (input, chunks) = mpsc::channel();
        let (feed, replies, seen, out_log) = (screen.clone(), input.clone(), activity.clone(), log.clone());
        thread::spawn(move || pump_output(reader, feed, replies, seen, out_log));
        let in_log = log.clone();
        thread::spawn(move || pump_input(writer, chunks, in_log));

        Ok(Subshell {
            screen,
            input,
            activity,
            started: Instant::now(),
            log,
            master: pair.master,
            child,
            size: (rows, cols),
            report_file,
            request_file,
            init_dir,
            prompt_at_last_input: 0,
            typed_anything: false,
            reports_prompts: shell.reports_prompts(),
            cwd_when_shown: None,
        })
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) == self.size || rows == 0 || cols == 0 {
            return;
        }
        self.size = (rows, cols);
        self.log.write(&format!("resize to {rows}x{cols}"));
        let _ = self.master.resize(pty_size(rows, cols));
        if let Ok(mut parser) = self.screen.lock() {
            parser.screen_mut().set_size(non_zero(rows), non_zero(cols));
        }
        self.activity.changed.store(true, Ordering::Relaxed);
    }

    /// How long the shell has been starting without writing anything; None once it has.
    pub fn starting_for(&self) -> Option<Duration> {
        (!self.activity.output_seen.load(Ordering::Relaxed)).then(|| self.started.elapsed())
    }

    /// Whether the screen changed since the last call (so it needs drawing).
    pub fn take_changed(&self) -> bool {
        self.activity.changed.swap(false, Ordering::Relaxed)
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
        // Only the size: what is typed into a shell can include passwords.
        self.log.write(&format!("input {} bytes", bytes.len()));
        let _ = self.input.send(bytes.to_vec());
        self.typed_anything = true;
        // Text typed before the first prompt is read at that prompt, so it counts as prompt 1's.
        self.prompt_at_last_input = self.report().map_or(1, |report| report.prompt.max(1));
    }

    /// Scrolls the view back (positive) or forward through the output.
    pub fn scroll(&self, lines: isize) {
        if let Ok(mut parser) = self.screen.lock() {
            let position = parser.screen().scrollback() as isize + lines;
            parser.screen_mut().set_scrollback(position.max(0) as usize);
        }
        self.activity.changed.store(true, Ordering::Relaxed);
    }

    pub fn scroll_to_bottom(&self) {
        if let Ok(mut parser) = self.screen.lock()
            && parser.screen().scrollback() > 0
        {
            parser.screen_mut().set_scrollback(0);
            self.activity.changed.store(true, Ordering::Relaxed);
        }
    }

    pub fn has_exited(&mut self) -> bool {
        let exited = !matches!(self.child.try_wait(), Ok(None));
        if exited {
            self.log.write("shell exited");
        }
        exited
    }

    /// What the shell's last prompt reported (PowerShell only).
    pub fn report(&self) -> Option<ShellReport> {
        platform::parse_cwd_report(&fs::read_to_string(&self.report_file).ok()?)
    }

    /// Waiting at an empty prompt: a prompt has appeared since rmc last typed anything.
    /// A shell that hasn't shown its first prompt yet counts as idle until something is typed.
    /// A shell that never reports (cmd, bash) always counts as idle: rmc can't tell, and
    /// refusing every command after the first would be worse than typing into a busy one.
    pub fn is_idle(&self) -> bool {
        match self.report() {
            Some(report) => report.prompt > self.prompt_at_last_input,
            None => !self.typed_anything || !self.reports_prompts,
        }
    }

    /// Before the shell is shown: if it sits idle in another folder than the panel's, move it
    /// there, as mc does with its subshell. That is silent: rmc writes the folder to the request
    /// file and presses the follow key, and the shell changes folder and redraws its prompt in
    /// place. (Without the key, e.g. PowerShell without PSReadLine or bash before 4.4, a visible
    /// `cd` is typed instead.) Shells that don't report their folder are left alone.
    /// Remembers where the shell is, for `moved_to`.
    pub fn follow_panel(&mut self, dir: &Path, shell: &Shell) {
        let report = self.report();
        let elsewhere = report.as_ref().is_some_and(|r| !util::same_path(&r.dir, dir));
        if elsewhere && self.is_idle() && self.reports_prompts {
            let silent = report.as_ref().is_some_and(|r| r.can_follow)
                && fs::write(&self.request_file, dir.to_string_lossy().as_bytes()).is_ok();
            if silent {
                self.send_text(platform::FOLLOW_KEY);
            } else if let Some(line) = shell.cd_line(dir) {
                self.send_text(&line);
            }
            self.cwd_when_shown = Some(dir.to_path_buf());
        } else {
            // A brand-new shell has no report yet; it started in the panel's folder.
            self.cwd_when_shown = report.map(|r| r.dir).or_else(|| Some(dir.to_path_buf()));
        }
    }

    /// Where the shell went since it was shown, if it changed folder.
    pub fn moved_to(&self) -> Option<PathBuf> {
        let here = self.report()?.dir;
        let moved = !self.cwd_when_shown.as_deref().is_some_and(|before| util::same_path(before, &here));
        moved.then_some(here)
    }
}

impl Drop for Subshell {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = fs::remove_file(&self.report_file);
        let _ = fs::remove_file(&self.request_file);
        let _ = fs::remove_dir_all(&self.init_dir);
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

        // rmc moves it to the panel's folder silently (no command line); the variable survives.
        assert!(shell.report().is_some_and(|r| r.can_follow), "PSReadLine's follow key is there");
        shell.follow_panel(&other, &shell_kind);
        wait_for("the cd", || shell.report().is_some_and(|r| util::same_path(&r.dir, &other)));
        wait_for("the redrawn prompt", || screen_text(&shell).contains("Účty>"));
        let screen = screen_text(&shell);
        assert!(!screen.contains("Set-Location") && !screen.contains("__rmc"), "no visible cd:\n{screen}");
        assert_eq!(screen.matches("PS ").count(), 2, "the prompt is redrawn in place:\n{screen}");
        assert!(shell.is_idle(), "still idle after the silent cd");
        assert_eq!(shell.moved_to(), None, "rmc's own cd doesn't count as the user moving");
        shell.send_text("\"still=$rmcTest\"; cd ..\r");
        wait_for("the variable", || screen_text(&shell).contains("still=42"));
        wait_for("the user's cd", || shell.moved_to().is_some_and(|dir| util::same_path(&dir, tmp.path())));

        shell.send_text("exit\r");
        wait_for("exit", || shell.has_exited());
    }

    /// bash and zsh, with rmc's start-up files: the same as the PowerShell test above.
    #[cfg(unix)]
    fn posix_shell_keeps_its_state_and_follows_the_panel(program: &str, set_prompt: &str) {
        let shell_kind = Shell::Posix(program.into());
        let tmp = TempDir::new("subshell-posix");
        let other = tmp.dir("Účty");
        let mut shell = Subshell::start(&shell_kind, tmp.path(), 30, 100).unwrap();
        assert!(shell.is_idle(), "a new shell is ready for typing");

        // A plain prompt showing the folder's name, "[name]> "; a variable to check later.
        shell.send_text(&format!("{set_prompt}; x=$((6 * 7)); echo answer=$x\r"));
        wait_for("the command's output", || screen_text(&shell).contains("answer=42"));
        wait_for("the next prompt", || shell.is_idle());

        assert!(shell.report().is_some_and(|r| r.can_follow), "the follow key is bound");
        shell.follow_panel(&other, &shell_kind);
        wait_for("the cd", || shell.report().is_some_and(|r| util::same_path(&r.dir, &other)));
        wait_for("the redrawn prompt", || screen_text(&shell).contains("[Účty]> "));
        let screen = screen_text(&shell);
        assert!(!screen.contains("cd --") && !screen.contains("__rmc"), "no visible cd:\n{screen}");
        assert_eq!(screen.matches("]> ").count(), 1, "the prompt is redrawn in place:\n{screen}");
        assert!(shell.is_idle(), "still idle after the silent cd");
        assert_eq!(shell.moved_to(), None, "rmc's own cd doesn't count as the user moving");

        shell.send_text("echo still=$x; cd ..\r");
        wait_for("the variable", || screen_text(&shell).contains("still=42"));
        wait_for("the user's cd", || shell.moved_to().is_some_and(|dir| util::same_path(&dir, tmp.path())));
        shell.send_text("exit\r");
        wait_for("exit", || shell.has_exited());
    }

    // The prompts are set as '[...]''> ' so the typed command itself doesn't contain "]> ".
    #[cfg(unix)]
    #[test]
    fn bash_keeps_its_state_and_follows_the_panel() {
        let Some(bash) = util::find_in_path("bash") else {
            eprintln!("no bash here; skipped");
            return;
        };
        posix_shell_keeps_its_state_and_follows_the_panel(&bash.to_string_lossy(), r"PS1='[\W]''> '");
    }

    /// RMC_TEST_ZSH can point at a zsh that isn't on the PATH.
    #[cfg(unix)]
    #[test]
    fn zsh_keeps_its_state_and_follows_the_panel() {
        let zsh = std::env::var_os("RMC_TEST_ZSH").map(PathBuf::from).or_else(|| util::find_in_path("zsh"));
        let Some(zsh) = zsh else {
            eprintln!("no zsh here; skipped");
            return;
        };
        // The binary may be called zsh-static; rmc goes by the name, so give it a zsh one.
        let tmp = TempDir::new("zsh-link");
        let link = tmp.path().join("zsh");
        std::os::unix::fs::symlink(&zsh, &link).unwrap();
        posix_shell_keeps_its_state_and_follows_the_panel(&link.to_string_lossy(), "PROMPT='[%1~]''> '");
    }

    /// cmd (and sh, fish) don't report their prompts: rmc can't see when a command ends,
    /// so it must not refuse the second command as "still busy".
    #[test]
    fn a_shell_without_prompt_reports_takes_one_command_after_another() {
        let shell_kind = if cfg!(windows) { Shell::Cmd } else { Shell::Posix("/bin/sh".into()) };
        let tmp = TempDir::new("subshell-plain");
        let mut shell = Subshell::start(&shell_kind, tmp.path(), 30, 100).unwrap();
        // Each word shows twice once the command has run: in the command line and as its output.
        shell.send_text("echo first-one\r");
        wait_for("the first command", || screen_text(&shell).matches("first-one").count() >= 2);
        assert!(shell.is_idle(), "the second command is not refused");
        shell.send_text("echo second-one\r");
        wait_for("the second command", || screen_text(&shell).matches("second-one").count() >= 2);
        assert_eq!(shell.moved_to(), None, "no reports, so no folder to follow");
        shell.send_text("exit\r");
        wait_for("exit", || shell.has_exited());
    }
}
