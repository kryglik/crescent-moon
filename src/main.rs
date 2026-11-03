//! rmc: a small Midnight Commander-style two-panel file manager for the terminal.

mod app;
mod dialog;
mod hotlist;
mod ops;
mod panel;
mod platform;
mod sizes;
#[cfg(test)]
mod testutil;
mod ui;
mod util;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::{cursor, execute, terminal};

use app::{App, Config, External};

const USAGE: &str = "\
rmc - two-panel file manager

Usage: rmc [LEFT_DIR [RIGHT_DIR]] [--cwd-file FILE]

  --cwd-file FILE   on exit, write the active panel's directory to FILE
                    (lets a shell function cd there)
  -h, --help        show this help
  -V, --version     show the version

Environment: RMC_EDITOR (default nvim), RMC_VIEWER (default nvim -R),
             RMC_SHELL (default pwsh). Press F1 inside rmc for the keys.";

struct Args {
    dirs: Vec<PathBuf>,
    cwd_file: Option<PathBuf>,
}

/// Ok(None) means the program should just exit (help or version printed).
fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args { dirs: Vec::new(), cwd_file: None };
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
    let result = run(&mut terminal, &mut app);
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

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    while !app.quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        let timeout = Duration::from_millis(if app.is_busy() { 50 } else { 500 });
        if event::poll(timeout)? {
            // Handle everything already queued (fast typing, pastes) before drawing again.
            loop {
                // Windows reports key releases too; only presses (and repeats) count.
                if let Event::Key(key) = event::read()?
                    && key.kind != KeyEventKind::Release
                {
                    app.on_key(key);
                }
                if app.quit || app.external.is_some() || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        app.tick();
        if let Some(external) = app.external.take() {
            let result = run_external(terminal, app, external)?;
            app.after_external(result);
        }
    }
    Ok(())
}

/// Hands the terminal to an editor, viewer, shell or command, then takes it back.
fn run_external(terminal: &mut DefaultTerminal, app: &App, external: External) -> io::Result<Result<(), String>> {
    let cwd = app.active_panel().cwd.clone();
    let config = &app.config;
    let (mut command, name) = match &external {
        External::Edit(path) => (program(&config.editor, path), config.editor[0].as_str()),
        External::View(path) => (program(&config.viewer, path), config.viewer[0].as_str()),
        External::Shell => (config.shell.interactive(), "the shell"),
        External::Command(line) => (config.shell.command(line), "the shell"),
    };
    let wait = matches!(external, External::Command(_));
    command.current_dir(&cwd);

    terminal::disable_raw_mode()?;
    execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show)?;
    match &external {
        External::Command(line) => println!("{}> {line}", cwd.display()),
        External::Shell => println!("rmc: type 'exit' to return to the panels."),
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
    terminal.clear()?;
    Ok(result)
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
