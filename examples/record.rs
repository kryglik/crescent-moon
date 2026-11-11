//! Records cm (or any terminal program) for the feature page's animations: runs it in a
//! pseudo-console, plays a script of keys, and writes every screen it showed, with times, as
//! JSON for `tools/demo/render.py` to turn into a GIF.
//!
//!     cargo run --example record -- SCRIPT OUT.json
//!
//! A script is one command per line (`#` starts a comment; `${NAME}` is the environment's):
//!
//!     size 100 30            the screen, before `run`
//!     env NAME=VALUE         set (or `unset NAME`) a variable for the program
//!     cwd DIR                the folder it starts in
//!     run PROGRAM ARGS...    start it ("quotes" keep spaces together)
//!     wait MS                keep recording
//!     key SPEC...            press keys: f5, enter, down*3, ctrl+e, alt+b, shift+f6, a...
//!     type TEXT              type the rest of the line, a character at a time
//!     keydelay MS            the pause after each key (350), `typedelay` after each character (110)
//!     until TEXT [MS]        record until the screen shows TEXT (fails after MS, 10000)
//!     gone TEXT [MS]         record until the screen no longer shows TEXT (30000)
//!     cut                    drop what was recorded so far: what came before is setting up
//!     end MS                 hold the last screen MS, then F10, and write the file

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

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

type Parser = vt100::Parser<Queries>;

/// A run of cells with one look: text, foreground, background, attribute bits.
#[derive(PartialEq, Clone)]
struct Run(String, String, String, u8);

#[derive(PartialEq, Clone)]
struct Screen {
    rows: Vec<Vec<Run>>,
    cursor: Option<(u16, u16)>,
}

struct Frame {
    at: u128,
    screen: Screen,
}

fn colour(c: vt100::Color) -> String {
    match c {
        vt100::Color::Default => String::new(),
        vt100::Color::Idx(i) => format!("i{i}"),
        vt100::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

fn snapshot(parser: &Parser) -> Screen {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let mut out = Vec::new();
    for row in 0..rows.get() {
        let mut runs: Vec<Run> = Vec::new();
        for col in 0..cols.get() {
            let Some(cell) = screen.cell(row, col) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let text = if cell.has_contents() { cell.contents().to_string() } else { " ".to_string() };
            let bits = cell.bold() as u8
                | (cell.dim() as u8) << 1
                | (cell.italic() as u8) << 2
                | (cell.underline() as u8) << 3
                | (cell.inverse() as u8) << 4
                | (cell.is_wide() as u8) << 5;
            let (fg, bg) = (colour(cell.fgcolor()), colour(cell.bgcolor()));
            match runs.last_mut() {
                // A wide character keeps a run of its own, so the renderer can give it two cells.
                Some(last) if last.1 == fg && last.2 == bg && last.3 == bits && bits & 32 == 0 => last.0 += &text,
                _ => runs.push(Run(text, fg, bg, bits)),
            }
        }
        out.push(runs);
    }
    let cursor = (!screen.hide_cursor()).then(|| screen.cursor_position());
    Screen { rows: out, cursor }
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            c if (c as u32) < 0x20 => out += &format!("\\u{:04x}", c as u32),
            c => out.push(c),
        }
    }
    out + "\""
}

fn write_json(path: &str, cols: u16, rows: u16, frames: &[Frame]) -> std::io::Result<()> {
    let mut out = format!("{{\"cols\":{cols},\"rows\":{rows},\"frames\":[\n");
    for (i, frame) in frames.iter().enumerate() {
        let cursor = match frame.screen.cursor {
            Some((r, c)) => format!("[{r},{c}]"),
            None => "null".into(),
        };
        let rows: Vec<String> = frame
            .screen
            .rows
            .iter()
            .map(|runs| {
                let runs: Vec<String> = runs
                    .iter()
                    .map(|Run(t, f, b, a)| format!("[{},\"{f}\",\"{b}\",{a}]", json_string(t)))
                    .collect();
                format!("[{}]", runs.join(","))
            })
            .collect();
        let comma = if i + 1 < frames.len() { "," } else { "" };
        out += &format!("{{\"t\":{},\"cursor\":{cursor},\"rows\":[{}]}}{comma}\n", frame.at, rows.join(","));
    }
    out += "]}\n";
    fs::write(path, out)
}

/// The bytes a terminal sends for a key such as `f5`, `ctrl+e`, `alt+b`, `shift+f6` or `a`.
fn key_bytes(spec: &str) -> Result<Vec<u8>, String> {
    let parts: Vec<&str> = spec.split('+').collect();
    let (name, mods) = parts.split_last().ok_or("empty key")?;
    let (mut shift, mut alt, mut ctrl) = (false, false, false);
    for m in mods {
        match *m {
            "shift" => shift = true,
            "alt" => alt = true,
            "ctrl" => ctrl = true,
            _ => return Err(format!("unknown modifier {m} in {spec}")),
        }
    }
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let esc = |rest: &str| format!("\x1b{rest}").into_bytes();
    let alt_prefix = |bytes: Vec<u8>| if alt { [vec![0x1b], bytes].concat() } else { bytes };
    let csi_letter = |x: char| if m == 1 { esc(&format!("[{x}")) } else { esc(&format!("[1;{m}{x}")) };
    let csi_tilde = |n: u8| if m == 1 { esc(&format!("[{n}~")) } else { esc(&format!("[{n};{m}~")) };
    Ok(match *name {
        "enter" => alt_prefix(b"\r".to_vec()),
        "tab" if shift => esc("[Z"),
        "tab" => b"\t".to_vec(),
        "esc" => b"\x1b".to_vec(),
        // A NUL isn't Ctrl+Space to Windows' pseudo-console: give it the key itself, pressed and
        // let go, as Windows Terminal does (win32-input-mode: Vk;Sc;Uc;Kd;Cs;Rc_).
        "space" if ctrl && cfg!(windows) => esc("[32;57;32;1;8;1_\x1b[32;57;32;0;8;1_"),
        "space" if ctrl => vec![0],
        "space" => alt_prefix(b" ".to_vec()),
        "backspace" if ctrl => vec![8],
        "backspace" => alt_prefix(vec![0x7f]),
        "up" => csi_letter('A'),
        "down" => csi_letter('B'),
        "right" => csi_letter('C'),
        "left" => csi_letter('D'),
        "home" => csi_letter('H'),
        "end" => csi_letter('F'),
        "ins" => csi_tilde(2),
        "del" => csi_tilde(3),
        "pgup" => csi_tilde(5),
        "pgdn" => csi_tilde(6),
        "f1" | "f2" | "f3" | "f4" => {
            let x = ['P', 'Q', 'R', 'S'][(name.as_bytes()[1] - b'1') as usize];
            if m == 1 { esc(&format!("O{x}")) } else { esc(&format!("[1;{m}{x}")) }
        }
        f if f.starts_with('f') && f.len() > 1 => {
            let n: u8 = f[1..].parse().map_err(|_| format!("unknown key {spec}"))?;
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return Err(format!("unknown key {spec}")),
            };
            csi_tilde(code)
        }
        "backslash" if ctrl => vec![0x1c],
        c if c.chars().count() == 1 => {
            let ch = c.chars().next().unwrap();
            let bytes = if ctrl && ch.is_ascii_alphabetic() {
                vec![ch.to_ascii_lowercase() as u8 & 0x1f]
            } else if shift {
                ch.to_uppercase().to_string().into_bytes()
            } else {
                ch.to_string().into_bytes()
            };
            alt_prefix(bytes)
        }
        _ => return Err(format!("unknown key {spec}")),
    })
}

/// `${NAME}` replaced by the variable.
fn expand(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out += &rest[..start];
        let Some(end) = rest[start..].find('}') else { break };
        out += &env::var(&rest[start + 2..start + end]).unwrap_or_default();
        rest = &rest[start + end + 1..];
    }
    out + rest
}

/// Words, with "double quotes" keeping spaces in one.
fn words(s: &str) -> Vec<String> {
    let (mut out, mut word, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for c in s.chars() {
        match c {
            '"' => (quoted, any) = (!quoted, true),
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            c => (word, any) = (word + &c.to_string(), true),
        }
    }
    if any {
        out.push(word);
    }
    out
}

struct Recording {
    parser: Arc<Mutex<Parser>>,
    input: Arc<Mutex<Box<dyn Write + Send>>>,
    start: Instant,
    frames: Vec<Frame>,
}

impl Recording {
    fn sample(&mut self) {
        let screen = snapshot(&self.parser.lock().unwrap());
        if self.frames.last().is_none_or(|f| f.screen != screen) {
            self.frames.push(Frame { at: self.start.elapsed().as_millis(), screen });
        }
    }

    fn record_for(&mut self, ms: u64) {
        let until = Instant::now() + Duration::from_millis(ms);
        loop {
            self.sample();
            let now = Instant::now();
            if now >= until {
                break;
            }
            thread::sleep((until - now).min(Duration::from_millis(25)));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let mut w = self.input.lock().unwrap();
        w.write_all(bytes).and_then(|()| w.flush()).expect("writing to the program");
    }

    fn shows(&self, text: &str) -> bool {
        self.parser.lock().unwrap().screen().contents().contains(text)
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let [_, script, out] = args.as_slice() else {
        eprintln!("usage: record SCRIPT OUT.json");
        std::process::exit(2);
    };
    let text = fs::read_to_string(script).expect("reading the script");
    let (mut cols, mut rows) = (100u16, 30u16);
    let (mut vars, mut unset, mut cwd): (Vec<(String, String)>, Vec<String>, Option<String>) = (vec![], vec![], None);
    let mut rec: Option<Recording> = None;
    let mut child = None;
    let (mut key_delay, mut type_delay) = (350u64, 110u64);

    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = expand(rest.trim());
        let fail = |what: &str| -> ! {
            eprintln!("{script}:{}: {what}: {line}", n + 1);
            std::process::exit(1)
        };
        let number = |s: &str| s.parse::<u64>().unwrap_or_else(|_| fail("not a number"));
        match verb {
            "size" => {
                let w = words(&rest);
                (cols, rows) = (number(&w[0]) as u16, number(&w[1]) as u16);
            }
            "env" => {
                let (k, v) = rest.split_once('=').unwrap_or_else(|| fail("env NAME=VALUE"));
                vars.push((k.to_string(), v.to_string()));
            }
            "unset" => unset.push(rest.clone()),
            "cwd" => cwd = Some(rest.clone()),
            "keydelay" => key_delay = number(&rest),
            "typedelay" => type_delay = number(&rest),
            "run" => {
                let argv = words(&rest);
                let mut cmd = CommandBuilder::from_argv(argv.iter().map(Into::into).collect());
                for name in &unset {
                    cmd.env_remove(name);
                }
                for (k, v) in &vars {
                    cmd.env(k, v);
                }
                if let Some(dir) = &cwd {
                    cmd.cwd(dir);
                }
                let pair = native_pty_system()
                    .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
                    .unwrap_or_else(|e| fail(&e.to_string()));
                child = Some(pair.slave.spawn_command(cmd).unwrap_or_else(|e| fail(&e.to_string())));
                drop(pair.slave);
                let mut reader = pair.master.try_clone_reader().unwrap();
                let input: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
                let parser = Arc::new(Mutex::new(Parser::new_with_callbacks(
                    rows.try_into().unwrap(),
                    cols.try_into().unwrap(),
                    0,
                    Queries::default(),
                )));
                // The program's questions (where is the cursor?) are answered at once, as a
                // terminal would.
                let (feed, replies) = (parser.clone(), input.clone());
                thread::spawn(move || {
                    let mut buf = vec![0u8; 64 * 1024];
                    while let Ok(n) = reader.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let mut p = feed.lock().unwrap();
                        p.process(&buf[..n]);
                        let q = std::mem::take(p.callbacks_mut());
                        let mut reply = String::new();
                        if q.cursor_position {
                            let (r, c) = p.screen().cursor_position();
                            reply += &format!("\x1b[{};{}R", r + 1, c + 1);
                        }
                        if q.device_attributes {
                            reply += "\x1b[?1;0c";
                        }
                        drop(p);
                        if !reply.is_empty() {
                            let mut w = replies.lock().unwrap();
                            let _ = w.write_all(reply.as_bytes()).and_then(|()| w.flush());
                        }
                    }
                });
                // The pseudo-console lives as long as the recording: it ends with the process.
                std::mem::forget(pair.master);
                rec = Some(Recording { parser, input, start: Instant::now(), frames: vec![] });
            }
            _ => {
                let Some(r) = rec.as_mut() else { fail("nothing is running yet") };
                match verb {
                    "wait" => r.record_for(number(&rest)),
                    "key" => {
                        for spec in words(&rest) {
                            let (spec, times) = match spec.split_once('*') {
                                Some((s, t)) => (s.to_string(), number(t)),
                                None => (spec.clone(), 1),
                            };
                            let bytes = key_bytes(&spec).unwrap_or_else(|e| fail(&e));
                            for _ in 0..times {
                                r.send(&bytes);
                                r.record_for(key_delay);
                            }
                        }
                    }
                    "type" => {
                        for c in rest.chars() {
                            r.send(c.to_string().as_bytes());
                            r.record_for(type_delay);
                        }
                    }
                    "until" => {
                        let (what, ms) = match rest.rsplit_once(' ') {
                            Some((w, ms)) if ms.parse::<u64>().is_ok() => (w.to_string(), number(ms)),
                            _ => (rest.clone(), 10_000),
                        };
                        let deadline = Instant::now() + Duration::from_millis(ms);
                        while !r.shows(&what) {
                            if Instant::now() > deadline {
                                eprintln!("{}", r.parser.lock().unwrap().screen().contents());
                                fail("the screen never showed it");
                            }
                            r.record_for(25);
                        }
                    }
                    "gone" => {
                        let (what, ms) = match rest.rsplit_once(' ') {
                            Some((w, ms)) if ms.parse::<u64>().is_ok() => (w.to_string(), number(ms)),
                            _ => (rest.clone(), 30_000),
                        };
                        let deadline = Instant::now() + Duration::from_millis(ms);
                        while r.shows(&what) {
                            if Instant::now() > deadline {
                                fail("the screen kept showing it");
                            }
                            r.record_for(25);
                        }
                    }
                    "cut" => {
                        r.frames.clear();
                        r.start = Instant::now();
                        r.sample();
                    }
                    "end" => {
                        r.record_for(number(&rest));
                        let last = r.frames.last().map(|f| f.at).unwrap_or(0);
                        // The last screen stays for the time asked, from when it came.
                        let hold = r.start.elapsed().as_millis().max(last);
                        r.frames.push(Frame { at: hold, screen: r.frames.last().unwrap().screen.clone() });
                        write_json(out, cols, rows, &r.frames).expect("writing the frames");
                        eprintln!("{out}: {} frames, {:.1} s", r.frames.len(), hold as f64 / 1000.0);
                        r.send(&key_bytes("f10").unwrap());
                        if let Some(mut c) = child.take() {
                            for _ in 0..100 {
                                if let Ok(Some(_)) = c.try_wait() {
                                    std::process::exit(0);
                                }
                                thread::sleep(Duration::from_millis(50));
                            }
                            let _ = c.kill();
                        }
                        std::process::exit(0);
                    }
                    _ => fail("unknown command"),
                }
            }
        }
    }
    eprintln!("{script}: no `end`");
    std::process::exit(1);
}
