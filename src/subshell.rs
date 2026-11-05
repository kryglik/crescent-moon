//! The persistent shell behind Ctrl+O. It runs in a pseudo-console (ConPTY) that rmc owns, so
//! it lives on while the panels are shown: variables stay, and a long command keeps running.
//! rmc keeps a copy of its screen (vt100) and draws it; keys typed while it is shown go to it.

use std::fs;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Where the shell's prompt reports `<number>|<f or ->|<folder>` (see `PWSH_SHELL_INIT`).
    report_file: PathBuf,
    /// Where rmc puts the folder for the shell's follow key.
    request_file: PathBuf,
    /// A folder of this shell's own (private, with a name nobody can guess): the two files
    /// above and rmc's start-up files for bash and zsh.
    init_dir: PathBuf,
    /// The prompt number at which the line rmc last typed will be read. A later prompt means
    /// the shell is idle: that line has run and the new prompt's line is empty.
    prompt_at_last_input: u64,
    /// The prompt at which rmc last pressed Enter: until the next prompt, a command runs, and
    /// anything typed meanwhile waits for the prompt after it.
    submitted_at: Option<u64>,
    typed_anything: bool,
    /// The shell it runs, for quoting and the lines rmc types into it (the options may name
    /// another one meanwhile).
    shell: Shell,
    /// Whether the shell's prompt writes `report_file` (PowerShell, bash, zsh).
    reports_prompts: bool,
    /// The shell's folder when it was last shown; if it has moved by the time the panels come
    /// back, the panel follows.
    pub cwd_when_shown: Option<PathBuf>,
    /// The report (prompt number, folder) from before rmc last pressed the follow key. Until a
    /// newer one comes, or if the shell couldn't go there, it hasn't moved by itself.
    followed_from: Option<(u64, PathBuf)>,
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
        let init_dir = private_dir().map_err(|e| error(&e))?;
        let (report_file, request_file) = (init_dir.join("report.txt"), init_dir.join("cd.txt"));
        let session = shell.session(&init_dir).map_err(|e| error(&e))?;
        let mut command = CommandBuilder::from_argv(session.argv);
        for (key, value) in session.env {
            command.env(key, value);
        }
        // Ctrl+O exits only the one-off shell of the fallback, never this one.
        command.env_remove("RMC_ONE_OFF");
        // What rmc's screen copy (vt100) understands; without it readline and zle fall back
        // to a dumb terminal. ConPTY on Windows ignores it.
        if cfg!(unix) {
            command.env("TERM", "xterm-256color");
        }
        command.cwd(cwd);
        command.env("RMC_CWD_FILE", &report_file);
        command.env("RMC_REQUEST_FILE", &request_file);
        command.env(crate::platform::RMC_PID, std::process::id().to_string());
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
            submitted_at: None,
            typed_anything: false,
            shell: shell.clone(),
            reports_prompts: shell.reports_prompts(),
            cwd_when_shown: None,
            followed_from: None,
        })
    }

    /// The shell this is (which may differ from the one the options now name).
    pub fn shell(&self) -> &Shell {
        &self.shell
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
        // Programs like less and htop ask for "application cursor keys".
        let application_cursor = self.with_screen(vt100::Screen::application_cursor).unwrap_or(false);
        let bytes = encode_key(key, application_cursor);
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
        let prompt = self.report().map_or(1, |report| report.prompt.max(1));
        // Typed while a command runs (Enter was pressed at this prompt and no new one came):
        // it waits for the next prompt, whose line it then is. That prompt is not idle, or
        // the follow key (which presses Enter in bash) would run it.
        let line_prompt = if self.submitted_at == Some(prompt) { prompt + 1 } else { prompt };
        self.prompt_at_last_input = line_prompt;
        if bytes.contains(&b'\r') {
            self.submitted_at = Some(line_prompt);
        }
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

    /// What the shell's last prompt reported (PowerShell, bash, zsh).
    pub fn report(&self) -> Option<ShellReport> {
        platform::parse_cwd_report_bytes(&fs::read(&self.report_file).ok()?)
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
    pub fn follow_panel(&mut self, dir: &Path) {
        let report = self.report();
        let elsewhere = report.as_ref().is_some_and(|r| !util::same_path(&r.dir, dir));
        let follow = elsewhere && self.is_idle() && self.reports_prompts;
        self.followed_from = None;
        match report {
            Some(report) if follow => {
                let silent = report.can_follow && fs::write(&self.request_file, path_bytes(dir)).is_ok();
                if silent {
                    self.send_text(platform::FOLLOW_KEY);
                } else if let Some(line) = self.shell.cd_line(dir) {
                    self.send_text(&line);
                }
                self.followed_from = Some((report.prompt, report.dir));
                self.cwd_when_shown = Some(dir.to_path_buf());
            }
            // A brand-new shell has no report yet; it started in the panel's folder.
            report => self.cwd_when_shown = report.map(|r| r.dir).or_else(|| Some(dir.to_path_buf())),
        }
    }

    /// Where the shell went since it was shown, if it changed folder. A report from before
    /// the follow key took effect (Ctrl+O pressed twice quickly), or the shell still where it
    /// was because it couldn't go to the panel's folder, isn't a move of the user's.
    pub fn moved_to(&self) -> Option<PathBuf> {
        let report = self.report()?;
        // The follow key gives exactly one new prompt; any later one is the user's doing.
        if let Some((prompt, from)) = &self.followed_from
            && (report.prompt <= *prompt || (report.prompt == prompt + 1 && util::same_path(&report.dir, from)))
        {
            return None;
        }
        let moved = !self.cwd_when_shown.as_deref().is_some_and(|before| util::same_path(before, &report.dir));
        moved.then_some(report.dir)
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

/// A new folder in the temp folder for one shell's files, made fresh with a name nobody can
/// guess, and on Linux readable by its owner only: the shell runs the start-up file in it,
/// and /tmp is shared with other users.
fn private_dir() -> std::io::Result<PathBuf> {
    for _ in 0..100 {
        let dir = std::env::temp_dir().join(format!("rmc-{}-shell-{:016x}", std::process::id(), util::random()));
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no folder could be made for the shell's files"))
}

/// A path as the bytes to write to a file the shell reads (on Linux not necessarily UTF-8).
fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    path.to_string_lossy().as_bytes().to_vec()
}

fn non_zero(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

/// The bytes a terminal (xterm, Windows Terminal) sends for a key. ConPTY turns them back into
/// Windows key events for the shell. `application_cursor`: the program asked for the cursor
/// keys' "application" form (less, htop and other full-screen programs on Linux).
pub fn encode_key(key: &KeyEvent, application_cursor: bool) -> Vec<u8> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // xterm's modifier parameter: 1 + Shift + 2·Alt + 4·Ctrl.
    let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi = |end: char| if m == 1 { format!("\x1b[{end}") } else { format!("\x1b[1;{m}{end}") };
    let cursor = |end: char| if m == 1 && application_cursor { format!("\x1bO{end}") } else { csi(end) };
    let tilde = |n: u8| if m == 1 { format!("\x1b[{n}~") } else { format!("\x1b[{n};{m}~") };
    let text = match key.code {
        // Plain and shifted characters, and AltGr ones, which Windows reports as Ctrl+Alt.
        KeyCode::Char(c) if dialog::is_text_input(key) && (cfg!(windows) || !(ctrl && alt)) => c.to_string(),
        // Ctrl+key; with Alt too (outside Windows), ESC first, as for any Alt key.
        KeyCode::Char(c) if ctrl => match control_byte(c) {
            Some(byte) if alt => format!("\x1b{}", byte as char),
            Some(byte) => (byte as char).to_string(),
            None => return Vec::new(),
        },
        KeyCode::Char(c) => format!("\x1b{c}"),
        KeyCode::Enter if alt => "\x1b\r".into(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        // Windows Terminal's convention, which ConPTY expects: DEL is Backspace, BS is Ctrl+Backspace.
        KeyCode::Backspace if ctrl => "\x08".into(),
        // Alt+Backspace deletes a word in bash, zsh and PSReadLine's Emacs mode.
        KeyCode::Backspace if alt => "\x1b\x7f".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Esc if alt => "\x1b\x1b".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up => cursor('A'),
        KeyCode::Down => cursor('B'),
        KeyCode::Right => cursor('C'),
        KeyCode::Left => cursor('D'),
        KeyCode::Home => cursor('H'),
        KeyCode::End => cursor('F'),
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
        encode_key(&KeyEvent::new(code, modifiers), false)
    }

    #[test]
    fn alt_keys_and_application_cursor_keys() {
        assert_eq!(key(KeyCode::Backspace, KeyModifiers::ALT), b"\x1b\x7f", "Alt+Backspace deletes a word");
        assert_eq!(key(KeyCode::Enter, KeyModifiers::ALT), b"\x1b\r");
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(encode_key(&up, true), b"\x1bOA", "less and htop ask for these");
        assert_eq!(encode_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), true), b"\x1b[1;5A");
        // Ctrl+Alt+E is AltGr on Windows, Alt + Ctrl+E elsewhere.
        let ctrl_alt_e = key(KeyCode::Char('e'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(ctrl_alt_e, if cfg!(windows) { b"e".to_vec() } else { b"\x1b\x05".to_vec() });
    }

    #[test]
    fn keys_become_terminal_sequences() {
        let none = KeyModifiers::NONE;
        assert_eq!(key(KeyCode::Char('ů'), none), "ů".as_bytes());
        assert_eq!(key(KeyCode::Char('A'), KeyModifiers::SHIFT), b"A");
        #[cfg(windows)]
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
        shell.follow_panel(&other);
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
        shell.follow_panel(&other);
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

    /// Text typed while a command runs waits at the next prompt: that prompt isn't idle, so
    /// rmc neither types a command after it nor presses the follow key (bash's ends in Enter,
    /// which would run the text).
    #[test]
    fn text_typed_ahead_keeps_the_shell_busy() {
        let (shell_kind, sleep) = if cfg!(windows) {
            match Shell::detect() {
                shell @ Shell::PowerShell(_) => (shell, "Start-Sleep -Milliseconds 700\r"),
                _ => return eprintln!("no PowerShell here; skipped"),
            }
        } else {
            match util::find_in_path("bash") {
                Some(bash) => (Shell::Posix(bash), "sleep 0.7\r"),
                None => return eprintln!("no bash here; skipped"),
            }
        };
        let tmp = TempDir::new("subshell-ahead");
        let mut shell = Subshell::start(&shell_kind, tmp.path(), 30, 100).unwrap();
        wait_for("the first prompt", || shell.report().is_some());
        let first = shell.report().unwrap().prompt;
        shell.send_text(sleep);
        shell.send_text("echo typed-ahead");
        wait_for("the prompt after the sleep", || shell.report().is_some_and(|r| r.prompt > first));
        assert!(!shell.is_idle(), "the new prompt holds the typed-ahead text");
        shell.send_text("\r");
        wait_for("the typed-ahead command", || screen_text(&shell).matches("typed-ahead").count() >= 2);
        wait_for("idle again", || shell.is_idle());
        shell.send_text("exit\r");
        wait_for("exit", || shell.has_exited());
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
