//! rmc: a small Midnight Commander-style two-panel file manager for the terminal.

mod admin;
mod app;
mod archive;
mod clipboard;
mod compare;
mod dialog;
mod find;
mod finder;
mod git;
mod history;
mod hotlist;
mod lnk;
mod mouse;
mod ops;
mod options;
mod panel;
mod platform;
mod preview;
mod scheme;
mod settings;
mod sizes;
mod subshell;
mod survey;
#[cfg(test)]
mod testutil;
mod ui;
mod util;
mod watch;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind, MouseEventKind};
use ratatui::crossterm::{cursor, execute, terminal};

use app::{App, Config, External};

/// rmc's terminal: frames go out through `Output`.
type Tui = Terminal<CrosstermBackend<Output>>;

/// What rmc writes to the terminal. ratatui flushes at the end of each frame, and only then
/// does the frame go out, in one write. Rust's stdout alone sends it in 1 KB pieces, and the
/// terminal may show a half-drawn screen in between. Counts the bytes for RMC_TRACE.
#[derive(Default)]
struct Output {
    frame: Vec<u8>,
}

/// Bytes `Output` has sent so far.
static SENT: AtomicU64 = AtomicU64::new(0);

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.frame.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.frame.is_empty() {
            let mut out = io::stdout().lock();
            out.write_all(&self.frame)?;
            out.flush()?;
            SENT.fetch_add(self.frame.len() as u64, Ordering::Relaxed);
            self.frame.clear();
        }
        Ok(())
    }
}

/// RMC_TRACE=FILE: a line per frame, to tell a slow rmc from a slow terminal. It says what
/// woke rmc (k key, m mouse move, c click, w wheel, r resize; nothing for a timer), how long
/// from the first of those events until the frame was sent, how long drawing took, and how
/// many bytes went to the terminal.
struct Trace {
    file: Option<fs::File>,
    start: Instant,
}

impl Trace {
    fn from_env() -> Self {
        let file = std::env::var_os("RMC_TRACE").and_then(|path| fs::File::create(path).ok());
        Trace { file, start: Instant::now() }
    }

    fn frame(&mut self, events: &str, woke: Option<Instant>, draw: Duration, bytes: u64) {
        let Some(file) = &mut self.file else { return };
        let latency = woke.map_or(String::new(), |t| format!(" latency={}us", t.elapsed().as_micros()));
        let at = self.start.elapsed().as_millis();
        let _ = writeln!(file, "{at:>7}ms [{events}]{latency} draw={}us bytes={bytes}", draw.as_micros());
    }

    fn event(&self, events: &mut String, event: &Event) {
        if self.file.is_none() {
            return;
        }
        events.push(match event {
            Event::Key(_) => 'k',
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Moved | MouseEventKind::Drag(_) => 'm',
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => 'w',
                _ => 'c',
            },
            Event::Resize(..) => 'r',
            _ => '?',
        });
    }
}

const USAGE: &str = "\
rmc - two-panel file manager

Usage: rmc [LEFT_DIR [RIGHT_DIR]] [--cwd-file FILE] [--no-mouse]

  --cwd-file FILE   on exit, write the active panel's directory to FILE
                    (lets a shell function cd there)
  --no-mouse        leave the mouse to the terminal (normal text selection)
  -h, --help        show this help
  -V, --version     show the version

Environment: RMC_EDITOR (default nvim), RMC_VIEWER (default nvim -R),
             RMC_SHELL (default pwsh). Press F1 inside rmc for the keys.";

struct Args {
    dirs: Vec<PathBuf>,
    cwd_file: Option<PathBuf>,
    mouse: bool,
}

/// Ok(None) means the program should just exit (help or version printed).
fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args { dirs: Vec::new(), cwd_file: None, mouse: true };
    let mut iter = std::env::args_os().skip(1);
    while let Some(arg) = iter.next() {
        match arg.to_str() {
            Some("-h" | "--help") => {
                println!("{USAGE}");
                return Ok(None);
            }
            Some("-V" | "--version") => {
                println!("rmc {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            Some("--cwd-file") => {
                args.cwd_file = Some(iter.next().ok_or("--cwd-file needs a file name")?.into());
            }
            Some("--no-mouse") => args.mouse = false,
            // Internal: the elevated helper of "As admin" (see admin.rs); no TUI.
            Some("--admin-job") => {
                let dir = PathBuf::from(iter.next().ok_or("--admin-job needs a folder")?);
                std::process::exit(admin::run_helper(&dir));
            }
            Some(s) if s.starts_with('-') => return Err(format!("unknown option {s} (try --help)")),
            _ if args.dirs.len() < 2 => args.dirs.push(arg.into()),
            _ => return Err("at most two directories can be given".into()),
        }
    }
    Ok(Some(args))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rmc: {e}");
            return ExitCode::from(2);
        }
    };
    // Typed in rmc's shell or command line, a second rmc would run inside the first one's
    // screen, both wanting the keys.
    if platform::running_inside().is_some() {
        eprintln!("rmc: already running in this terminal; Ctrl+O goes back to it.");
        return ExitCode::FAILURE;
    }
    platform::init();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dirs: Vec<PathBuf> = args.dirs.iter().map(|d| util::resolve(&cwd, &d.to_string_lossy())).collect();
    let left = dirs.first().cloned().unwrap_or_else(|| cwd.clone());
    let right = dirs.get(1).cloned().unwrap_or_else(|| left.clone());
    let mut app = App::new(left, right, Config::load());
    app.mouse &= args.mouse;
    app.mouse_allowed = args.mouse;

    // A panic must not leave the terminal in raw mode, or reporting mouse movements as text.
    // One on a background thread (a job, a search) ends only that work while the screen goes
    // on: its message goes to a file instead of over the screen.
    let main_thread = std::thread::current().id();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == main_thread {
            Mouse(true).release();
            ratatui::restore();
            previous_hook(info);
        } else {
            let log = std::env::temp_dir().join("rmc-panic.txt");
            let _ = std::fs::write(log, format!("{info}\n{}", std::backtrace::Backtrace::force_capture()));
        }
    }));
    let title = SavedTitle::save();
    let result = init_terminal().and_then(|mut terminal| run(&mut terminal, &mut app));
    Mouse(app.mouse).release();
    ratatui::restore();
    title.restore();
    app.save_history();
    // Files taken out of archives to be viewed (one still open in an app stays).
    let _ = fs::remove_dir_all(archive::temp_dir());

    if let Some(file) = args.cwd_file {
        let _ = std::fs::write(file, app.active_panel().cwd.to_string_lossy().as_bytes());
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rmc: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The window title from before rmc, put back on exit (rmc names the active folder in it).
/// Windows has it as the console's title; other terminals keep a stack of titles.
struct SavedTitle(#[cfg_attr(not(windows), allow(dead_code))] Option<String>);

impl SavedTitle {
    fn save() -> SavedTitle {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::GetConsoleTitleW;
            let mut buf = vec![0u16; 2048];
            let n = unsafe { GetConsoleTitleW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
            SavedTitle((n > 0).then(|| String::from_utf16_lossy(&buf[..n.min(buf.len())])))
        }
        #[cfg(not(windows))]
        {
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[22;0t").and_then(|()| out.flush());
            SavedTitle(None)
        }
    }

    fn restore(self) {
        #[cfg(windows)]
        if let Some(title) = self.0 {
            let _ = execute!(io::stdout(), terminal::SetTitle(title));
        }
        #[cfg(not(windows))]
        {
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[23;0t").and_then(|()| out.flush());
        }
    }
}

/// Mouse capture, unless `--no-mouse` was given. While it is on, the terminal's own
/// text selection needs Shift held down.
#[derive(Clone, Copy)]
struct Mouse(bool);

/// On Unix rmc asks the terminal for clicks and the wheel only: normal tracking (1000) with
/// SGR coordinates (1006). crossterm's EnableMouseCapture also turns on any-motion tracking
/// (1003), so the terminal sends a report for every cell the pointer crosses, which rmc
/// doesn't use. On Windows mouse input is a console mode instead.
#[cfg(unix)]
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1006h";
#[cfg(unix)]
const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1000l";

impl Mouse {
    fn capture(self) -> io::Result<()> {
        if self.0 {
            #[cfg(unix)]
            {
                let mut out = io::stdout();
                out.write_all(MOUSE_ON)?;
                out.flush()?;
            }
            #[cfg(not(unix))]
            execute!(io::stdout(), event::EnableMouseCapture)?;
        }
        Ok(())
    }

    fn release(self) {
        if self.0 {
            #[cfg(unix)]
            {
                let mut out = io::stdout();
                let _ = out.write_all(MOUSE_OFF).and_then(|()| out.flush());
            }
            #[cfg(not(unix))]
            let _ = execute!(io::stdout(), event::DisableMouseCapture);
        }
    }
}

/// Raw mode and the alternate screen, with frames going out through `Output`.
fn init_terminal() -> io::Result<Tui> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(Output::default()))
}

fn run(terminal: &mut Tui, app: &mut App) -> io::Result<()> {
    // The options editor can turn the mouse on and off; this follows `app.mouse`.
    let mut mouse = Mouse(app.mouse);
    mouse.capture()?;
    let mut trace = Trace::from_env();
    let mut events = String::new();
    let mut woke = None;
    let mut dirty = true;
    let mut title = String::new();
    while !app.quit {
        // The window (or tab) title names the active panel's folder.
        let wanted = window_title(app);
        if wanted != title {
            let _ = execute!(io::stdout(), terminal::SetTitle(&wanted));
            title = wanted;
        }
        // Draw only when something changed: every frame re-sends the cursor, which keeps it
        // from blinking (and makes the shell's screen flicker), and is work for the terminal.
        let shell_changed = app.subshell.as_ref().is_some_and(|s| s.take_changed() || s.starting_for().is_some());
        if dirty || shell_changed {
            let (started, sent) = (Instant::now(), SENT.load(Ordering::Relaxed));
            terminal.draw(|frame| ui::draw(frame, app))?;
            trace.frame(&events, woke, started.elapsed(), SENT.load(Ordering::Relaxed) - sent);
        }
        (dirty, woke) = (false, None);
        events.clear();
        // The shell's output arrives on its own thread; check often while it's on screen.
        let timeout = Duration::from_millis(match () {
            _ if app.shell_shown => 10,
            _ if app.is_busy() => 50,
            _ if app.wake_soon() => 100,
            _ => 500,
        });
        if event::poll(timeout)? {
            woke = Some(Instant::now());
            // Handle everything already queued (fast typing, pastes, mouse moves) before drawing again.
            loop {
                let event = event::read()?;
                trace.event(&mut events, &event);
                dirty |= match event {
                    // Windows reports key releases too; only presses (and repeats) count.
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        app.on_key(key);
                        true
                    }
                    Event::Key(_) => false,
                    // Moving the pointer changes nothing (Windows reports it even so).
                    Event::Mouse(event) if event.kind == MouseEventKind::Moved => false,
                    Event::Mouse(event) => {
                        app.on_mouse(event);
                        true
                    }
                    _ => true,
                };
                if app.quit || app.external.is_some() || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        // Paths for the terminal's clipboard (OSC 52; works over ssh too).
        if let Some(text) = app.terminal_clipboard.take() {
            let mut out = io::stdout();
            let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
        }
        // Results of background work (directory sizes, drive space, a copy's progress, the
        // shell exiting) change the screen by themselves; so does time while a job runs.
        let shown = app.shell_shown;
        dirty |= app.tick() || app.working() || app.shell_shown != shown;
        if app.mouse != mouse.0 {
            mouse.release();
            mouse = Mouse(app.mouse);
            mouse.capture()?;
        }
        if let Some(external) = app.external.take() {
            let (result, shell_cwd) = run_external(terminal, app, external, mouse)?;
            app.after_external(result, shell_cwd);
            dirty = true;
            // The program may have set a title of its own.
            title.clear();
        }
    }
    Ok(())
}

/// "photos - rmc": the active panel's folder (the whole path for a drive root).
fn window_title(app: &App) -> String {
    let location = app.active_panel().location();
    let name = location.file_name().map_or_else(|| location.display().to_string(), |n| n.to_string_lossy().into_owned());
    format!("{name} - rmc")
}

/// Hands the terminal to an editor, viewer, shell or command, then takes it back.
/// After a Ctrl+O shell it also returns the folder the shell was last in.
fn run_external(
    terminal: &mut Tui,
    app: &mut App,
    external: External,
    mouse: Mouse,
) -> io::Result<(Result<(), String>, Option<PathBuf>)> {
    let cwd = app.active_panel().cwd.clone();
    let cwd_file = std::env::temp_dir().join(format!("rmc-{}-cwd.txt", std::process::id()));
    let _ = std::fs::remove_file(&cwd_file);
    let config = &app.config;
    let (mut command, name) = match &external {
        External::Edit(path, line) => (program(&config.editor, path, *line), config.editor[0].as_str()),
        External::View(path, line) => (program(&config.viewer, path, *line), config.viewer[0].as_str()),
        External::Page(path, line) => {
            let pager = config.pager.as_deref().unwrap_or(&config.viewer);
            (program(pager, path, *line), pager[0].as_str())
        }
        External::Shell => (config.shell.interactive(&cwd_file), "the shell"),
        External::Command(line) => (config.shell.command(line), "the shell"),
    };
    let wait = matches!(external, External::Command(_));
    command.current_dir(&cwd).env(platform::RMC_PID, std::process::id().to_string());

    mouse.release();
    terminal::disable_raw_mode()?;
    execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show)?;
    match &external {
        External::Command(line) => println!("{}> {line}", cwd.display()),
        // The hint only the first time; later the shell's old output is enough of a clue.
        External::Shell if !app.shell_hint_shown => {
            app.shell_hint_shown = true;
            let way_back = if config.shell.ctrl_o_returns() { "Ctrl+O (or exit)" } else { "exit" };
            println!("rmc: {way_back} returns to the panels.");
        }
        _ => {}
    }
    let result = match run_shielded(&mut command) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(format!("\"{name}\" was not found.\nInstall it, or set RMC_EDITOR, RMC_VIEWER, RMC_PAGER or RMC_SHELL."))
        }
        Err(e) => Err(format!("Cannot run \"{name}\":\n{e}")),
    };
    if wait && result.is_ok() {
        wait_for_key()?;
    }
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen)?;
    mouse.capture()?;
    terminal.clear()?;

    let shell_cwd = std::fs::read_to_string(&cwd_file)
        .ok()
        .and_then(|text| platform::parse_cwd_report(&text))
        .map(|report| report.dir);
    let _ = std::fs::remove_file(&cwd_file);
    Ok((result, shell_cwd))
}

/// Runs a program in the foreground. Ctrl+C there is meant for it (stopping a search in
/// less, say), but the terminal sends it to rmc too, which would end, taking its shell along:
/// rmc ignores it until the program ends.
fn run_shielded(command: &mut Command) -> io::Result<std::process::ExitStatus> {
    let mut child = command.spawn()?;
    let _shield = IgnoreInterrupts::new();
    child.wait()
}

/// Ctrl+C (and Ctrl+\, Ctrl+Break) ignored while it lives. Set after the child has started,
/// so the child doesn't inherit it.
struct IgnoreInterrupts {
    #[cfg(unix)]
    previous: (libc::sighandler_t, libc::sighandler_t),
}

impl IgnoreInterrupts {
    fn new() -> Self {
        #[cfg(unix)]
        {
            let previous = unsafe { (libc::signal(libc::SIGINT, libc::SIG_IGN), libc::signal(libc::SIGQUIT, libc::SIG_IGN)) };
            IgnoreInterrupts { previous }
        }
        #[cfg(windows)]
        {
            unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(ignore_interrupt), 1) };
            IgnoreInterrupts {}
        }
    }
}

impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, self.previous.0);
            libc::signal(libc::SIGQUIT, self.previous.1);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(ignore_interrupt), 0);
        }
    }
}

/// Ctrl+C and Ctrl+Break are handled (ignored); closing the window goes on as usual.
#[cfg(windows)]
unsafe extern "system" fn ignore_interrupt(kind: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    i32::from(kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT)
}

/// The editor, viewer or pager command for `path`, at `line` (`+42`) for the programs that
/// take one that way; others just open the file.
fn program(parts: &[String], path: &std::path::Path, line: Option<u64>) -> Command {
    let mut command = Command::new(&parts[0]);
    command.args(&parts[1..]);
    if let Some(line) = line
        && takes_plus_line(&parts[0])
    {
        command.arg(format!("+{line}"));
    }
    command.arg(path);
    command
}

fn takes_plus_line(program: &str) -> bool {
    let stem = std::path::Path::new(program).file_stem().map(|s| s.to_string_lossy().to_lowercase());
    matches!(
        stem.as_deref(),
        Some("nvim" | "vim" | "vi" | "gvim" | "view" | "nano" | "less" | "emacs" | "emacsclient" | "kak" | "joe" | "mcedit" | "ne")
    )
}

/// Lets the user read a command's output before the panels come back.
fn wait_for_key() -> io::Result<()> {
    print!("\nrmc: press any key to return to the panels…");
    io::stdout().flush()?;
    terminal::enable_raw_mode()?;
    loop {
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            break;
        }
    }
    println!();
    Ok(())
}
