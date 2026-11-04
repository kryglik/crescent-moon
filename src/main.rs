//! rmc: a small Midnight Commander-style two-panel file manager for the terminal.

mod app;
mod dialog;
mod hotlist;
mod mouse;
mod ops;
mod panel;
mod platform;
mod sizes;
mod subshell;
#[cfg(test)]
mod testutil;
mod ui;
mod util;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::crossterm::{cursor, execute, terminal};

use app::{App, Config, External};

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
    platform::init();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dirs: Vec<PathBuf> = args.dirs.iter().map(|d| util::resolve(&cwd, &d.to_string_lossy())).collect();
    let left = dirs.first().cloned().unwrap_or_else(|| cwd.clone());
    let right = dirs.get(1).cloned().unwrap_or_else(|| left.clone());
    let mut app = App::new(left, right, Config::from_env());

    let mut terminal = ratatui::init();
    let mouse = Mouse(args.mouse);
    let result = mouse.capture().and_then(|()| run(&mut terminal, &mut app, mouse));
    mouse.release();
    ratatui::restore();

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

/// Mouse capture, unless `--no-mouse` was given. While it is on, the terminal's own
/// text selection needs Shift held down.
#[derive(Clone, Copy)]
struct Mouse(bool);

impl Mouse {
    fn capture(self) -> io::Result<()> {
        if self.0 {
            execute!(io::stdout(), EnableMouseCapture)?;
        }
        Ok(())
    }

    fn release(self) {
        if self.0 {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
    }
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, mouse: Mouse) -> io::Result<()> {
    // A panic must not leave the terminal reporting mouse movements as text.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        mouse.release();
        previous_hook(info);
    }));

    let mut dirty = true;
    while !app.quit {
        // The panels are cheap to redraw every time. The shell's screen is drawn only when it
        // changed: redrawing it 100 times a second re-sends the cursor and makes it flicker.
        let shell_changed = app.subshell.as_ref().is_some_and(|s| s.take_changed() || s.starting_for().is_some());
        if dirty || !app.shell_shown || shell_changed {
            terminal.draw(|frame| ui::draw(frame, app))?;
        }
        dirty = false;
        // The shell's output arrives on its own thread; check often while it's on screen.
        let timeout = Duration::from_millis(match () {
            _ if app.shell_shown => 10,
            _ if app.is_busy() => 50,
            _ => 500,
        });
        if event::poll(timeout)? {
            dirty = true;
            // Handle everything already queued (fast typing, pastes, mouse moves) before drawing again.
            loop {
                match event::read()? {
                    // Windows reports key releases too; only presses (and repeats) count.
                    Event::Key(key) if key.kind != KeyEventKind::Release => app.on_key(key),
                    Event::Mouse(event) => app.on_mouse(event),
                    _ => {}
                }
                if app.quit || app.external.is_some() || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        let shown = app.shell_shown;
        app.tick();
        dirty |= app.shell_shown != shown;
        if let Some(external) = app.external.take() {
            let (result, shell_cwd) = run_external(terminal, app, external, mouse)?;
            app.after_external(result, shell_cwd);
            dirty = true;
        }
    }
    Ok(())
}

/// Hands the terminal to an editor, viewer, shell or command, then takes it back.
/// After a Ctrl+O shell it also returns the folder the shell was last in.
fn run_external(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    external: External,
    mouse: Mouse,
) -> io::Result<(Result<(), String>, Option<PathBuf>)> {
    let cwd = app.active_panel().cwd.clone();
    let cwd_file = std::env::temp_dir().join(format!("rmc-{}-cwd.txt", std::process::id()));
    let _ = std::fs::remove_file(&cwd_file);
    let config = &app.config;
    let (mut command, name) = match &external {
        External::Edit(path) => (program(&config.editor, path), config.editor[0].as_str()),
        External::View(path) => (program(&config.viewer, path), config.viewer[0].as_str()),
        External::Shell => (config.shell.interactive(&cwd_file), "the shell"),
        External::Command(line) => (config.shell.command(line), "the shell"),
    };
    let wait = matches!(external, External::Command(_));
    command.current_dir(&cwd);

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
    let result = match command.status() {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(format!("\"{name}\" was not found.\nInstall it, or set RMC_EDITOR / RMC_VIEWER / RMC_SHELL."))
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
        .map(|(_, dir)| dir);
    let _ = std::fs::remove_file(&cwd_file);
    Ok((result, shell_cwd))
}

fn program(parts: &[String], path: &std::path::Path) -> Command {
    let mut command = Command::new(&parts[0]);
    command.args(&parts[1..]).arg(path);
    command
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
