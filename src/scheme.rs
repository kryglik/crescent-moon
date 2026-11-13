//! Colour schemes: the look (colours, bold, dim…) of every part of the screen.
//!
//! The built-in schemes are below. The user's own are `*.ini` files in the `schemes` folder
//! beside the settings (`%APPDATA%\crescent-moon\schemes`, `~/.config/crescent-moon/schemes`), one
//! `part = colour [on colour] [bold]…` per line, on top of a built-in scheme (`base`) or
//! classic; a file named like a built-in scheme takes its place. Alt+C lists them all and
//! shows each on the whole screen; F4 there writes a built-in one out as a file to change.
//!
//! A part drawn on a panel (or in a dialog, or on the command line) has that background
//! unless it names its own. A file's look builds up: the panel's, its kind's (folder,
//! archive…), hidden, ignored by git, marked, and the cursor's on top, so a cursor that only
//! names a background keeps the file's own colour on it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Modifier, Style};

use crate::util;

macro_rules! parts {
    ($($(#[doc = $doc:literal])+ $part:ident,)+) => {
        /// The look of each part of the screen.
        #[derive(Clone, Debug, Default, PartialEq)]
        pub struct Scheme {
            $($(#[doc = $doc])+ pub $part: Style,)+
        }

        impl Scheme {
            /// Every part's name in a scheme file, and what it is.
            pub const PARTS: &'static [(&'static str, &'static str)] = &[$((stringify!($part), concat!($($doc),+)),)+];

            fn part_mut(&mut self, name: &str) -> Option<&mut Style> {
                match name {
                    $(stringify!($part) => Some(&mut self.$part),)+
                    _ => None,
                }
            }

            fn part(&self, name: &str) -> Option<Style> {
                match name {
                    $(stringify!($part) => Some(self.$part),)+
                    _ => None,
                }
            }

            fn each_mut(&mut self, mut f: impl FnMut(&mut Style)) {
                $(f(&mut self.$part);)+
            }
        }
    };
}

parts! {
    /// text and background of the panels
    panel,
    /// their lines: border, column separators
    frame,
    /// the path of the other panel
    title,
    /// the path of the active panel
    title_active,
    /// column headings, the marked files' total, the filter
    heading,
    /// the cursor bar (a file's colour shows through where it names none)
    cursor,
    /// marked files
    marked,
    /// a marked file under the cursor
    marked_cursor,
    /// hints and messages in the panels, Find's line numbers
    dim,
    /// folders
    directory,
    /// programs: .exe, .bat… (on Linux the x bit)
    executable,
    /// hidden files (on Linux the dot files)
    hidden,
    /// archives: .zip, .7z, .tar, .gz…
    archive,
    /// pictures: .jpg, .png, raw photos, .svg…
    image,
    /// sound and video
    media,
    /// source code: .rs, .py, .js, .c…
    source,
    /// documents: .txt, .md, .pdf, .docx…
    document,
    /// leftovers: .bak, .tmp, .swp, names ending in ~
    temp,
    /// the branch with ↑↓ commits ahead and behind, a badge when it has a background
    git_branch,
    /// the same in a linked worktree; the word "worktree" in its badge's colour
    git_worktree,
    /// a rebase or merge under way
    git_state,
    /// changed files: their M, and how many
    git_changed,
    /// new files: A, R, ?, and how many
    git_new,
    /// conflicts: their !, and how many
    git_conflict,
    /// files git ignores
    git_ignored,
    /// Find: the files found
    found_file,
    /// Find: the text that matches (also the shell's scroll-back note)
    found_text,
    /// the command line
    command_line,
    /// user@host before the folder on the command line
    prompt_host,
    /// messages on the command line
    notice,
    /// the keys on the bar at the bottom
    key_number,
    /// what they do
    key_label,
    /// the row of tabs
    tab_bar,
    /// a tab in the background
    tab,
    /// the tab shown on the other side
    tab_shown,
    /// dialogs
    dialog,
    /// their titles, and the headings in help
    dialog_title,
    /// the button or row in focus
    dialog_focus,
    /// hints in dialogs
    dialog_dim,
    /// text fields
    input,
    /// error messages and risky questions
    error,
    /// their titles
    error_title,
    /// progress bars
    gauge,
}

impl Scheme {
    /// A part drawn on a panel: the panel's colours under its own.
    pub fn on_panel(&self, part: Style) -> Style {
        self.panel.patch(part)
    }

    /// A part drawn in a dialog.
    pub fn on_dialog(&self, part: Style) -> Style {
        self.dialog.patch(part)
    }

    /// A part drawn on the command line or the F-key bar.
    pub fn on_bar(&self, part: Style) -> Style {
        self.command_line.patch(part)
    }

    pub fn kind(&self, kind: Kind) -> Style {
        match kind {
            Kind::Archive => self.archive,
            Kind::Image => self.image,
            Kind::Media => self.media,
            Kind::Source => self.source,
            Kind::Document => self.document,
            Kind::Temp => self.temp,
        }
    }

    /// This scheme as the terminal shows it: colours it lacks become the nearest it has; with
    /// colours off (NO_COLOR), mono.
    pub fn for_terminal(&self, colours: Colours) -> Scheme {
        if colours.off {
            return mono();
        }
        let mut scheme = self.clone();
        if colours.depth != Depth::TrueColor {
            scheme.each_mut(|style| {
                style.fg = style.fg.map(|c| reduce(c, colours.depth));
                style.bg = style.bg.map(|c| reduce(c, colours.depth));
            });
        }
        scheme
    }
}

/// What a file is, by its name, for its colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Archive,
    Image,
    Media,
    Source,
    Document,
    Temp,
}

const ARCHIVE: &[&str] = &[
    "7z", "ace", "apk", "arj", "br", "bz2", "cab", "cpio", "crate", "deb", "dmg", "gz", "iso", "jar", "lha", "lz", "lz4",
    "lzh", "lzma", "msi", "msix", "nupkg", "rar", "rpm", "tar", "taz", "tbz", "tbz2", "tgz", "tlz", "txz", "war", "whl",
    "xz", "z", "zip", "zst",
];
const IMAGE: &[&str] = &[
    "arw", "avif", "bmp", "cr2", "cr3", "dng", "gif", "heic", "heif", "ico", "jpe", "jpeg", "jpg", "jxl", "nef", "orf",
    "pef", "png", "psd", "raf", "rw2", "svg", "tga", "tif", "tiff", "webp", "xcf",
];
const MEDIA: &[&str] = &[
    "3gp", "aac", "ac3", "aif", "aiff", "ape", "avi", "flac", "flv", "m2ts", "m4a", "m4v", "mid", "midi", "mka", "mkv",
    "mov", "mp2", "mp3", "mp4", "mpeg", "mpg", "mts", "ogg", "ogv", "opus", "vob", "wav", "webm", "wma", "wmv",
];
const SOURCE: &[&str] = &[
    "asm", "bash", "c", "cc", "cjs", "clj", "cpp", "cs", "css", "cxx", "dart", "el", "erl", "ex", "exs", "fish", "go", "h",
    "hh", "hpp", "hs", "htm", "html", "java", "jl", "js", "jsx", "kt", "kts", "less", "lua", "m", "mjs", "ml", "mm", "nim",
    "php", "pl", "pm", "ps1", "psm1", "py", "pyi", "pyw", "r", "rb", "rs", "s", "sass", "scala", "scss", "sh", "sql",
    "svelte", "swift", "ts", "tsx", "vim", "vue", "zig", "zsh",
];
const DOCUMENT: &[&str] =
    &["csv", "doc", "docx", "epub", "md", "odp", "ods", "odt", "pdf", "ppt", "pptx", "rst", "rtf", "tex", "txt", "xls", "xlsx"];
const TEMP: &[&str] = &["bak", "crdownload", "old", "orig", "part", "rej", "swo", "swp", "temp", "tmp"];

/// A file's kind by its name: its extension, or `name~` and `#name#` for leftovers.
pub fn kind_of(name: &str) -> Option<Kind> {
    if name.ends_with('~') || (name.len() > 1 && name.starts_with('#') && name.ends_with('#')) {
        return Some(Kind::Temp);
    }
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let extension = extension.to_ascii_lowercase();
    [
        (ARCHIVE, Kind::Archive),
        (IMAGE, Kind::Image),
        (MEDIA, Kind::Media),
        (SOURCE, Kind::Source),
        (DOCUMENT, Kind::Document),
        (TEMP, Kind::Temp),
    ]
    .into_iter()
    .find(|(list, _)| list.contains(&extension.as_str()))
    .map(|(_, kind)| kind)
}

/// How many colours the terminal shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// The 16 the terminal's own palette defines.
    Ansi16,
    /// Those and the 240 of xterm's palette.
    Ansi256,
    /// Any (24-bit).
    TrueColor,
}

/// What the terminal does with colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colours {
    pub depth: Depth,
    /// NO_COLOR is set: none at all (crossterm leaves them out), so the mono scheme.
    pub off: bool,
}

impl Default for Colours {
    fn default() -> Self {
        Colours { depth: Depth::TrueColor, off: false }
    }
}

impl Colours {
    pub fn detect() -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Colours {
            depth: depth_from(cfg!(windows), &var("COLORTERM"), &var("TERM"), &var("TERM_PROGRAM"), !var("WT_SESSION").is_empty()),
            off: !var("NO_COLOR").is_empty(),
        }
    }

    /// For the scheme picker.
    pub fn describe(self) -> &'static str {
        match self.depth {
            _ if self.off => "NO_COLOR is set: no colours, so mono",
            Depth::TrueColor => "The terminal shows any colour.",
            Depth::Ansi256 => "256 colours: the others come out near (COLORTERM=truecolor for all)",
            Depth::Ansi16 => "16 colours: classic, dark, terminal and mono come out right",
        }
    }
}

/// Windows' consoles and Windows Terminal (WT_SESSION reaches WSL too) show any colour; on
/// Unix the terminal says so in COLORTERM (which ssh doesn't pass on), or in TERM.
fn depth_from(windows: bool, colorterm: &str, term: &str, program: &str, windows_terminal: bool) -> Depth {
    let colorterm = colorterm.to_ascii_lowercase();
    if windows
        || windows_terminal
        || colorterm.contains("truecolor")
        || colorterm.contains("24bit")
        || term.ends_with("-direct")
        || matches!(program, "iTerm.app" | "WezTerm" | "vscode" | "ghostty")
    {
        Depth::TrueColor
    } else if term.contains("256") {
        Depth::Ansi256
    } else {
        Depth::Ansi16
    }
}

/// The 16 colours with the values xterm gives them, for finding the nearest.
const ANSI: [(Color, (u8, u8, u8)); 16] = [
    (Color::Black, (0, 0, 0)),
    (Color::Red, (205, 0, 0)),
    (Color::Green, (0, 205, 0)),
    (Color::Yellow, (205, 205, 0)),
    (Color::Blue, (0, 0, 238)),
    (Color::Magenta, (205, 0, 205)),
    (Color::Cyan, (0, 205, 205)),
    (Color::Gray, (229, 229, 229)),
    (Color::DarkGray, (127, 127, 127)),
    (Color::LightRed, (255, 0, 0)),
    (Color::LightGreen, (0, 255, 0)),
    (Color::LightYellow, (255, 255, 0)),
    (Color::LightBlue, (92, 92, 255)),
    (Color::LightMagenta, (255, 0, 255)),
    (Color::LightCyan, (0, 255, 255)),
    (Color::White, (255, 255, 255)),
];

/// The steps of xterm's 6×6×6 colour cube.
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn reduce(colour: Color, depth: Depth) -> Color {
    match (colour, depth) {
        (Color::Rgb(r, g, b), Depth::Ansi256) => Color::Indexed(nearest_256((r, g, b))),
        (Color::Rgb(r, g, b), Depth::Ansi16) => nearest_16((r, g, b)),
        (Color::Indexed(i), Depth::Ansi16) if i >= 16 => nearest_16(indexed_rgb(i)),
        (colour, _) => colour,
    }
}

/// How far apart two colours look ("redmean": red and blue count more in reds).
fn distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> i64 {
    let mean = (i64::from(a.0) + i64::from(b.0)) / 2;
    let (dr, dg, db) = (i64::from(a.0) - i64::from(b.0), i64::from(a.1) - i64::from(b.1), i64::from(a.2) - i64::from(b.2));
    (((512 + mean) * dr * dr) >> 8) + 4 * dg * dg + (((767 - mean) * db * db) >> 8)
}

/// The nearest of xterm's colours 16-255 (0-15 are whatever the terminal's palette makes them).
fn nearest_256(rgb: (u8, u8, u8)) -> u8 {
    let step = |v: u8| (0..6).min_by_key(|&i| (i16::from(CUBE[i]) - i16::from(v)).abs()).unwrap_or(0);
    let (r, g, b) = (step(rgb.0), step(rgb.1), step(rgb.2));
    let cube = (CUBE[r], CUBE[g], CUBE[b]);
    // The grey ramp: 8, 18 … 238.
    let mean = (u16::from(rgb.0) + u16::from(rgb.1) + u16::from(rgb.2)) / 3;
    let grey_step = (mean.saturating_sub(3) / 10).min(23) as u8;
    let grey = 8 + 10 * grey_step;
    if distance(rgb, (grey, grey, grey)) < distance(rgb, cube) {
        232 + grey_step
    } else {
        16 + 36 * r as u8 + 6 * g as u8 + b as u8
    }
}

fn nearest_16(rgb: (u8, u8, u8)) -> Color {
    ANSI.iter().min_by_key(|(_, value)| distance(rgb, *value)).map_or(Color::Reset, |(colour, _)| *colour)
}

/// What xterm shows for colour `i` of its 256.
fn indexed_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0..16 => ANSI[usize::from(i)].1,
        16..232 => {
            let i = i - 16;
            (CUBE[usize::from(i / 36)], CUBE[usize::from(i / 6 % 6)], CUBE[usize::from(i % 6)])
        }
        _ => {
            let grey = 8 + 10 * (i - 232);
            (grey, grey, grey)
        }
    }
}

const fn rgb(value: u32) -> Color {
    Color::Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

/// mc's blue panels, cm's own look from the start.
pub fn classic() -> Scheme {
    let s = Style::new;
    Scheme {
        panel: s().gray().on_blue(),
        frame: s(),
        title: s(),
        title_active: s().black().on_cyan(),
        heading: s().yellow().bold(),
        cursor: s().black().on_cyan(),
        marked: s().yellow().bold(),
        marked_cursor: s().yellow().on_cyan().bold(),
        dim: s().gray().dim(),
        directory: s().white(),
        executable: s().light_green(),
        hidden: s(),
        // mc's are bright magenta, which Windows Terminal's palette makes too dark on blue.
        archive: s().light_red(),
        image: s().light_cyan(),
        media: s().green(),
        source: s(),
        document: s(),
        temp: s().dark_gray(),
        git_branch: s().black().on_green(),
        git_worktree: s().black().on_light_cyan(),
        git_state: s().yellow().bold(),
        git_changed: s().yellow(),
        git_new: s().light_green(),
        git_conflict: s().light_red().bold(),
        git_ignored: s().dark_gray(),
        found_file: s().light_cyan().bold(),
        found_text: s().black().on_yellow(),
        command_line: s(),
        prompt_host: s().light_green().bold(),
        notice: s().yellow(),
        key_number: s().black().on_cyan(),
        key_label: s(),
        tab_bar: s().on_black(),
        tab: s().gray().on_dark_gray(),
        tab_shown: s().white().on_blue(),
        dialog: s().black().on_gray(),
        dialog_title: s().blue(),
        dialog_focus: s().black().on_cyan(),
        dialog_dim: s().dark_gray(),
        input: s().black().on_cyan(),
        error: s().white().on_red(),
        error_title: s().bold(),
        gauge: s().cyan().on_black(),
    }
}

/// Classic as Windows Terminal draws it, in exact colours, but on a darker, calmer blue and with
/// a yellow cursor bar: marked files full yellow, and in the panels' blue on the bar.
fn classic_v2() -> Scheme {
    // Windows Terminal's default palette (Campbell), so the rest looks as classic does there.
    const BLACK: Color = rgb(0x0c0c0c);
    const RED: Color = rgb(0xc50f1f);
    const GREEN: Color = rgb(0x13a10e);
    const GOLD: Color = rgb(0xc19c00);
    const CYAN: Color = rgb(0x3a96dd);
    const GRAY: Color = rgb(0xcccccc);
    const DARK_GRAY: Color = rgb(0x767676);
    const LIGHT_RED: Color = rgb(0xe74856);
    const LIGHT_GREEN: Color = rgb(0x16c60c);
    const LIGHT_YELLOW: Color = rgb(0xf9f1a5);
    const LIGHT_CYAN: Color = rgb(0x61d6d6);
    const WHITE: Color = rgb(0xf2f2f2);
    // Campbell's blue (#0037da) a little darker and less saturated, and the yellow of the bar.
    const BLUE: Color = rgb(0x143daa);
    const YELLOW: Color = rgb(0xffde5f);
    let s = Style::new;
    Scheme {
        panel: s().fg(GRAY).bg(BLUE),
        frame: s(),
        title: s(),
        title_active: s().fg(BLACK).bg(YELLOW),
        heading: s().fg(LIGHT_YELLOW).bold(),
        cursor: s().fg(BLACK).bg(YELLOW),
        marked: s().fg(YELLOW).bold(),
        // The other way round from a marked file elsewhere.
        marked_cursor: s().fg(BLUE).bg(YELLOW).bold(),
        dim: s().fg(GRAY).dim(),
        directory: s().fg(WHITE),
        executable: s().fg(LIGHT_GREEN),
        hidden: s(),
        archive: s().fg(LIGHT_RED),
        image: s().fg(LIGHT_CYAN),
        media: s().fg(GREEN),
        source: s(),
        document: s(),
        temp: s().fg(DARK_GRAY),
        git_branch: s().fg(BLACK).bg(GREEN),
        git_worktree: s().fg(BLACK).bg(LIGHT_CYAN),
        git_state: s().fg(LIGHT_YELLOW).bold(),
        git_changed: s().fg(GOLD),
        git_new: s().fg(LIGHT_GREEN),
        git_conflict: s().fg(LIGHT_RED).bold(),
        git_ignored: s().fg(DARK_GRAY),
        found_file: s().fg(LIGHT_CYAN).bold(),
        // Not yellow: Find's matches have to show on the cursor bar too.
        found_text: s().fg(BLACK).bg(LIGHT_CYAN),
        command_line: s(),
        prompt_host: s().fg(LIGHT_GREEN).bold(),
        notice: s().fg(GOLD),
        key_number: s().fg(BLACK).bg(CYAN),
        key_label: s().fg(WHITE),
        tab_bar: s().bg(BLACK),
        tab: s().fg(GRAY).bg(DARK_GRAY),
        tab_shown: s().fg(WHITE).bg(BLUE),
        dialog: s().fg(BLACK).bg(GRAY),
        dialog_title: s().fg(BLUE),
        dialog_focus: s().fg(BLACK).bg(YELLOW),
        dialog_dim: s().fg(DARK_GRAY),
        input: s().fg(BLACK).bg(CYAN),
        error: s().fg(WHITE).bg(RED),
        error_title: s().bold(),
        gauge: s().fg(CYAN).bg(BLACK),
    }
}

/// Classic on black, with dimmer lines and hidden files.
fn dark() -> Scheme {
    let s = Style::new;
    Scheme {
        panel: s().gray().on_black(),
        frame: s().dark_gray(),
        title: s().gray(),
        hidden: s().dark_gray(),
        dim: s().dark_gray(),
        tab_shown: s().white().on_black(),
        dialog: s().white().on_dark_gray(),
        dialog_title: s().light_yellow().bold(),
        dialog_dim: s().gray(),
        ..classic()
    }
}

/// The terminal's own background and palette, with ls's colours for kinds of files.
fn terminal() -> Scheme {
    let s = Style::new;
    Scheme {
        panel: s(),
        frame: s().dark_gray(),
        title: s(),
        title_active: s().black().on_cyan().bold(),
        heading: s().yellow().bold(),
        // Not reverse video, which would take each file's colour.
        cursor: s().black().on_cyan(),
        marked: s().yellow().bold(),
        marked_cursor: s().black().on_yellow().bold(),
        dim: s().dark_gray(),
        directory: s().blue().bold(),
        executable: s().green().bold(),
        hidden: s(),
        archive: s().red().bold(),
        image: s().magenta().bold(),
        media: s().cyan(),
        source: s(),
        document: s(),
        temp: s().dark_gray(),
        git_branch: s().black().on_green(),
        git_worktree: s().black().on_cyan(),
        git_state: s().yellow().bold(),
        git_changed: s().yellow(),
        git_new: s().green(),
        git_conflict: s().red().bold(),
        git_ignored: s().dark_gray(),
        found_file: s().cyan().bold(),
        found_text: s().black().on_yellow(),
        command_line: s(),
        prompt_host: s().green().bold(),
        notice: s().yellow(),
        key_number: s().reversed(),
        key_label: s(),
        tab_bar: s(),
        tab: s().dark_gray(),
        tab_shown: s().bold(),
        dialog: s(),
        dialog_title: s().bold(),
        dialog_focus: s().black().on_cyan(),
        dialog_dim: s().dark_gray(),
        input: s().reversed(),
        error: s().white().on_red(),
        error_title: s().bold(),
        gauge: s().cyan(),
    }
}

/// No colours at all, only bold, dim, underline and reverse video: what NO_COLOR gets. Every
/// part keeps the terminal's colours, as crossterm, told NO_COLOR, turns a colour change into a
/// reset that would drop the reverse video too.
pub fn mono() -> Scheme {
    let s = Style::new;
    Scheme {
        title_active: s().reversed().bold(),
        heading: s().bold(),
        cursor: s().reversed(),
        marked: s().bold(),
        marked_cursor: s().reversed().bold(),
        dim: s().dim(),
        hidden: s().dim(),
        temp: s().dim(),
        git_branch: s().reversed(),
        git_worktree: s().reversed().underlined(),
        git_state: s().bold(),
        git_conflict: s().bold(),
        git_ignored: s().dim(),
        found_file: s().bold(),
        found_text: s().reversed(),
        prompt_host: s().bold(),
        notice: s().bold(),
        key_number: s().reversed(),
        tab_shown: s().underlined(),
        dialog_title: s().bold(),
        dialog_focus: s().reversed(),
        dialog_dim: s().dim(),
        input: s().reversed(),
        error_title: s().bold().reversed(),
        ..Scheme::default()
    }
}

/// The colours a true-colour scheme is made of.
struct Palette {
    /// Panels and the command line.
    bg: Color,
    /// Dialogs, tabs.
    raised: Color,
    /// Text fields, the keys on the bottom bar, the tab shown on the other side, the progress
    /// bar's track.
    field: Color,
    frame: Color,
    text: Color,
    /// Hints; hidden, ignored and leftover files; the other panel's path.
    faint: Color,
    /// The keys on the bottom bar, typed text.
    bright: Color,
    /// The cursor bar's text and background (the focus in dialogs too).
    cursor: (Color, Color),
    /// Titles, headings, what Find found.
    accent: Color,
    directory: Color,
    executable: Color,
    /// Marked files, and the matches in Find.
    marked: Color,
    archive: Color,
    image: Color,
    media: Color,
    /// Changed files, messages.
    yellow: Color,
    /// Conflicts.
    red: Color,
    /// New files; the branch's badge.
    green: Color,
    /// Worktrees: their branch's badge.
    cyan: Color,
    /// A rebase or merge under way.
    orange: Color,
    /// Errors' text and background.
    error: (Color, Color),
}

fn from_palette(p: &Palette) -> Scheme {
    let s = Style::new;
    Scheme {
        panel: s().fg(p.text).bg(p.bg),
        frame: s().fg(p.frame),
        title: s().fg(p.faint),
        title_active: s().fg(p.bg).bg(p.accent).bold(),
        heading: s().fg(p.accent).bold(),
        cursor: s().fg(p.cursor.0).bg(p.cursor.1),
        marked: s().fg(p.marked).bold(),
        marked_cursor: s().fg(p.bg).bg(p.marked).bold(),
        dim: s().fg(p.faint),
        directory: s().fg(p.directory),
        executable: s().fg(p.executable),
        hidden: s().fg(p.faint),
        archive: s().fg(p.archive),
        image: s().fg(p.image),
        media: s().fg(p.media),
        source: s(),
        document: s(),
        temp: s().fg(p.faint),
        git_branch: s().fg(p.bg).bg(p.green).bold(),
        git_worktree: s().fg(p.bg).bg(p.cyan).bold(),
        git_state: s().fg(p.orange).bold(),
        git_changed: s().fg(p.yellow),
        git_new: s().fg(p.green),
        git_conflict: s().fg(p.red).bold(),
        git_ignored: s().fg(p.faint),
        found_file: s().fg(p.accent).bold(),
        found_text: s().fg(p.bg).bg(p.marked),
        command_line: s().fg(p.text).bg(p.bg),
        prompt_host: s().fg(p.green).bold(),
        notice: s().fg(p.yellow),
        key_number: s().fg(p.bright).bg(p.field),
        key_label: s().fg(p.text),
        tab_bar: s().bg(p.bg),
        tab: s().fg(p.faint).bg(p.raised),
        tab_shown: s().fg(p.bright).bg(p.field),
        dialog: s().fg(p.text).bg(p.raised),
        dialog_title: s().fg(p.accent).bold(),
        dialog_focus: s().fg(p.cursor.0).bg(p.cursor.1),
        dialog_dim: s().fg(p.faint),
        input: s().fg(p.bright).bg(p.field),
        error: s().fg(p.error.0).bg(p.error.1),
        error_title: s().bold(),
        gauge: s().fg(p.accent).bg(p.field),
    }
}

/// Black on white, red marks, Windows' blue for the cursor.
fn light() -> Scheme {
    from_palette(&Palette {
        bg: rgb(0xffffff),
        raised: rgb(0xececec),
        field: rgb(0xdceaf7),
        frame: rgb(0xa0a0a0),
        text: rgb(0x1e1e1e),
        faint: rgb(0x8c8c8c),
        bright: rgb(0x000000),
        cursor: (rgb(0xffffff), rgb(0x0063b1)),
        accent: rgb(0x0063b1),
        directory: rgb(0x0b3d91),
        executable: rgb(0x107c10),
        marked: rgb(0xc50f1f),
        archive: rgb(0x8e24aa),
        image: rgb(0x00796b),
        media: rgb(0xb35900),
        yellow: rgb(0x9d5d00),
        red: rgb(0xc50f1f),
        green: rgb(0x107c10),
        cyan: rgb(0x007a99),
        orange: rgb(0xb35900),
        error: (rgb(0xffffff), rgb(0xc50f1f)),
    })
}

/// Nord (nordtheme.com).
fn nord() -> Scheme {
    from_palette(&Palette {
        bg: rgb(0x2e3440),
        raised: rgb(0x3b4252),
        field: rgb(0x434c5e),
        frame: rgb(0x4c566a),
        text: rgb(0xd8dee9),
        faint: rgb(0x616e88),
        bright: rgb(0xeceff4),
        cursor: (rgb(0x2e3440), rgb(0x88c0d0)),
        accent: rgb(0x88c0d0),
        directory: rgb(0x81a1c1),
        executable: rgb(0xa3be8c),
        marked: rgb(0xebcb8b),
        archive: rgb(0xb48ead),
        image: rgb(0x8fbcbb),
        media: rgb(0xd08770),
        yellow: rgb(0xebcb8b),
        red: rgb(0xbf616a),
        green: rgb(0xa3be8c),
        cyan: rgb(0x88c0d0),
        orange: rgb(0xd08770),
        error: (rgb(0xeceff4), rgb(0xbf616a)),
    })
}

/// Gruvbox, dark (github.com/morhetz/gruvbox).
fn gruvbox() -> Scheme {
    from_palette(&Palette {
        bg: rgb(0x282828),
        raised: rgb(0x3c3836),
        field: rgb(0x504945),
        frame: rgb(0x665c54),
        text: rgb(0xebdbb2),
        faint: rgb(0x928374),
        bright: rgb(0xfbf1c7),
        cursor: (rgb(0x282828), rgb(0x83a598)),
        accent: rgb(0xfabd2f),
        directory: rgb(0x83a598),
        executable: rgb(0xb8bb26),
        marked: rgb(0xfabd2f),
        archive: rgb(0xd3869b),
        image: rgb(0x8ec07c),
        media: rgb(0xfe8019),
        yellow: rgb(0xfabd2f),
        red: rgb(0xfb4934),
        green: rgb(0xb8bb26),
        cyan: rgb(0x8ec07c),
        orange: rgb(0xfe8019),
        error: (rgb(0xfbf1c7), rgb(0xcc241d)),
    })
}

/// Dracula (draculatheme.com).
fn dracula() -> Scheme {
    from_palette(&Palette {
        bg: rgb(0x282a36),
        raised: rgb(0x343746),
        field: rgb(0x44475a),
        frame: rgb(0x6272a4),
        text: rgb(0xf8f8f2),
        faint: rgb(0x6272a4),
        bright: rgb(0xffffff),
        cursor: (rgb(0x282a36), rgb(0xbd93f9)),
        accent: rgb(0xbd93f9),
        directory: rgb(0x8be9fd),
        executable: rgb(0x50fa7b),
        marked: rgb(0xf1fa8c),
        archive: rgb(0xff79c6),
        image: rgb(0xffb86c),
        media: rgb(0xbd93f9),
        yellow: rgb(0xf1fa8c),
        red: rgb(0xff5555),
        green: rgb(0x50fa7b),
        cyan: rgb(0x8be9fd),
        orange: rgb(0xffb86c),
        error: (rgb(0x282a36), rgb(0xff5555)),
    })
}

/// Tokyo Night (github.com/enkia/tokyo-night-vscode-theme).
fn tokyo_night() -> Scheme {
    from_palette(&Palette {
        bg: rgb(0x1a1b26),
        raised: rgb(0x24283b),
        field: rgb(0x292e42),
        frame: rgb(0x3b4261),
        text: rgb(0xc0caf5),
        faint: rgb(0x565f89),
        bright: rgb(0xc0caf5),
        cursor: (rgb(0x1a1b26), rgb(0x7aa2f7)),
        accent: rgb(0x7aa2f7),
        directory: rgb(0x7dcfff),
        executable: rgb(0x9ece6a),
        marked: rgb(0xe0af68),
        archive: rgb(0xbb9af7),
        image: rgb(0x73daca),
        media: rgb(0xff9e64),
        yellow: rgb(0xe0af68),
        red: rgb(0xf7768e),
        green: rgb(0x9ece6a),
        cyan: rgb(0x7dcfff),
        orange: rgb(0xff9e64),
        error: (rgb(0x1a1b26), rgb(0xf7768e)),
    })
}

/// Catppuccin Mocha (catppuccin.com).
fn catppuccin() -> Scheme {
    from_palette(&Palette {
        bg: rgb(0x1e1e2e),
        raised: rgb(0x313244),
        field: rgb(0x45475a),
        frame: rgb(0x585b70),
        text: rgb(0xcdd6f4),
        faint: rgb(0x7f849c),
        bright: rgb(0xf5e0dc),
        cursor: (rgb(0x1e1e2e), rgb(0xcba6f7)),
        accent: rgb(0xcba6f7),
        directory: rgb(0x89b4fa),
        executable: rgb(0xa6e3a1),
        marked: rgb(0xf9e2af),
        archive: rgb(0xf5c2e7),
        image: rgb(0x94e2d5),
        media: rgb(0xfab387),
        yellow: rgb(0xf9e2af),
        red: rgb(0xf38ba8),
        green: rgb(0xa6e3a1),
        cyan: rgb(0x89dceb),
        orange: rgb(0xfab387),
        error: (rgb(0x1e1e2e), rgb(0xf38ba8)),
    })
}

/// One Dark, Atom's.
fn one_dark() -> Scheme {
    from_palette(&Palette {
        bg: rgb(0x282c34),
        raised: rgb(0x353b45),
        field: rgb(0x3e4451),
        frame: rgb(0x4b5263),
        text: rgb(0xabb2bf),
        faint: rgb(0x5c6370),
        bright: rgb(0xdcdfe4),
        cursor: (rgb(0x282c34), rgb(0x61afef)),
        accent: rgb(0x61afef),
        directory: rgb(0x61afef),
        executable: rgb(0x98c379),
        marked: rgb(0xe5c07b),
        archive: rgb(0xc678dd),
        image: rgb(0x56b6c2),
        media: rgb(0xd19a66),
        yellow: rgb(0xe5c07b),
        red: rgb(0xe06c75),
        green: rgb(0x98c379),
        cyan: rgb(0x56b6c2),
        orange: rgb(0xd19a66),
        error: (rgb(0x282c34), rgb(0xe06c75)),
    })
}

/// Solarized (ethanschoonover.com/solarized), dark.
fn solarized_dark() -> Scheme {
    from_palette(&Palette {
        bg: rgb(0x002b36),
        raised: rgb(0x073642),
        field: rgb(0x586e75),
        frame: rgb(0x586e75),
        text: rgb(0x839496),
        faint: rgb(0x586e75),
        bright: rgb(0xeee8d5),
        cursor: (rgb(0x002b36), rgb(0x268bd2)),
        accent: rgb(0xb58900),
        directory: rgb(0x268bd2),
        executable: rgb(0x859900),
        marked: rgb(0xb58900),
        archive: rgb(0xd33682),
        image: rgb(0x2aa198),
        media: rgb(0xcb4b16),
        yellow: rgb(0xb58900),
        red: rgb(0xdc322f),
        green: rgb(0x859900),
        cyan: rgb(0x2aa198),
        orange: rgb(0xcb4b16),
        error: (rgb(0xfdf6e3), rgb(0xdc322f)),
    })
}

/// Solarized, light.
fn solarized_light() -> Scheme {
    from_palette(&Palette {
        bg: rgb(0xfdf6e3),
        raised: rgb(0xeee8d5),
        field: rgb(0xddd6c1),
        frame: rgb(0x93a1a1),
        text: rgb(0x657b83),
        faint: rgb(0x93a1a1),
        bright: rgb(0x073642),
        cursor: (rgb(0x002b36), rgb(0x268bd2)),
        accent: rgb(0x268bd2),
        directory: rgb(0x268bd2),
        executable: rgb(0x859900),
        marked: rgb(0xdc322f),
        archive: rgb(0xd33682),
        image: rgb(0x2aa198),
        media: rgb(0xcb4b16),
        yellow: rgb(0xb58900),
        red: rgb(0xdc322f),
        green: rgb(0x859900),
        cyan: rgb(0x2aa198),
        orange: rgb(0xcb4b16),
        error: (rgb(0xfdf6e3), rgb(0xdc322f)),
    })
}

/// The built-in schemes, in the picker's order: name, what it is, the scheme.
const BUILT_IN: &[(&str, &str, fn() -> Scheme)] = &[
    ("classic", "mc's blue panels", classic),
    ("classic-v2", "classic on a darker blue, yellow cursor bar", classic_v2),
    ("dark", "classic on black", dark),
    ("terminal", "the terminal's own background and palette", terminal),
    ("light", "black on white, red marks", light),
    ("nord", "Nord: arctic blue-grey", nord),
    ("gruvbox", "Gruvbox dark: warm, retro", gruvbox),
    ("dracula", "Dracula: purple and pink on charcoal", dracula),
    ("tokyo-night", "Tokyo Night: deep blue, neon", tokyo_night),
    ("catppuccin", "Catppuccin Mocha: pastels", catppuccin),
    ("one-dark", "One Dark, Atom's", one_dark),
    ("solarized-dark", "Solarized, dark", solarized_dark),
    ("solarized-light", "Solarized, light", solarized_light),
    ("mono", "no colours: bold and reverse video", mono),
];

/// The built-in scheme called `name` (in any case).
pub fn built_in(name: &str) -> Option<Scheme> {
    BUILT_IN.iter().find(|(n, ..)| n.eq_ignore_ascii_case(name)).map(|(.., make)| make())
}

/// One scheme in the picker.
#[derive(Clone, Debug)]
pub struct Choice {
    pub name: String,
    pub about: String,
    /// The user's file; None for a built-in scheme.
    pub file: Option<PathBuf>,
    pub scheme: Scheme,
    /// What in the file couldn't be read ("line 7: …"); those lines are left out.
    pub problems: Vec<String>,
}

/// `schemes` in the config folder.
pub fn default_dir() -> Option<PathBuf> {
    util::config_dir().map(|dir| dir.join("schemes"))
}

/// Every scheme: the built-in ones (a file of the same name in its place), then the user's
/// others by name.
pub fn choices(dir: Option<&Path>) -> Vec<Choice> {
    let mut list: Vec<Choice> = BUILT_IN
        .iter()
        .map(|(name, about, make)| Choice {
            name: (*name).to_string(),
            about: (*about).to_string(),
            file: None,
            scheme: make(),
            problems: Vec::new(),
        })
        .collect();
    let mut files: Vec<PathBuf> = dir
        .and_then(|dir| fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .filter_map(|item| item.ok().map(|item| item.path()))
        .filter(|path| path.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ini")))
        .collect();
    files.sort_by_key(|path| path.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    let mut own = Vec::new();
    for file in files {
        let choice = read_file(&file);
        match list.iter_mut().find(|c| c.file.is_none() && c.name.eq_ignore_ascii_case(&choice.name)) {
            Some(slot) => *slot = choice,
            None => own.push(choice),
        }
    }
    list.extend(own);
    list
}

/// The scheme called `name` (in any case), if there is one.
pub fn find(name: &str, dir: Option<&Path>) -> Option<Choice> {
    choices(dir).into_iter().find(|c| c.name.eq_ignore_ascii_case(name.trim()))
}

fn read_file(path: &Path) -> Choice {
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let (scheme, about, problems) = match util::read_text(path) {
        Ok(text) => parse_file(&text),
        Err(e) => (classic(), None, vec![format!("cannot read it: {e}")]),
    };
    Choice { name, about: about.unwrap_or_else(|| "your scheme".into()), file: Some(path.to_path_buf()), scheme, problems }
}

/// A scheme file's text: its scheme (`base`, or classic, with the file's parts on top), its
/// description, and the lines that couldn't be read.
fn parse_file(text: &str) -> (Scheme, Option<String>, Vec<String>) {
    let mut problems: Vec<(usize, String)> = Vec::new();
    let mut lines = Vec::new();
    for (i, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        // `;` starts a comment anywhere, `#` at the start of a line (colours start with one).
        let line = line.split(';').next().unwrap_or_default().trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once('=') {
            Some((name, value)) => lines.push((i + 1, name.trim().to_ascii_lowercase().replace('-', "_"), value.trim())),
            None => problems.push((i + 1, "no \"=\" in it".into())),
        }
    }
    let mut scheme = classic();
    let mut about = None;
    if let Some((n, _, value)) = lines.iter().rev().find(|(_, name, _)| name == "base") {
        match built_in(value) {
            Some(base) => scheme = base,
            None => problems.push((*n, format!("there is no built-in scheme \"{value}\""))),
        }
    }
    for (n, name, value) in &lines {
        match name.as_str() {
            "base" => {}
            "description" => about = Some((*value).to_string()),
            _ => match scheme.part_mut(name) {
                Some(style) => match parse_style(value) {
                    Ok(parsed) => *style = parsed,
                    Err(e) => problems.push((*n, e)),
                },
                None => problems.push((*n, format!("no part is called \"{name}\""))),
            },
        }
    }
    problems.sort_by_key(|(n, _)| *n);
    (scheme, about, problems.into_iter().map(|(n, problem)| format!("line {n}: {problem}")).collect())
}

const MODIFIERS: [(Modifier, &str); 5] = [
    (Modifier::BOLD, "bold"),
    (Modifier::DIM, "dim"),
    (Modifier::ITALIC, "italic"),
    (Modifier::UNDERLINED, "underline"),
    (Modifier::REVERSED, "reverse"),
];

/// `colour [on colour] [bold] [dim] [italic] [underline] [reverse]`; empty for nothing.
pub fn parse_style(text: &str) -> Result<Style, String> {
    let mut style = Style::new();
    let mut words = text.split_whitespace();
    while let Some(word) = words.next() {
        let lower = word.to_ascii_lowercase();
        if lower == "on" {
            let colour = words.next().ok_or("a colour should follow \"on\"")?;
            style = style.bg(parse_colour(colour)?);
        } else if let Some((modifier, _)) = MODIFIERS.iter().find(|(_, name)| lower == *name || lower == format!("{name}d")) {
            style = style.add_modifier(*modifier);
        } else if style.fg.is_none() {
            style = style.fg(parse_colour(word)?);
        } else {
            return Err(format!("\"{word}\" is not bold, dim, italic, underline or reverse"));
        }
    }
    Ok(style)
}

/// `#rrggbb`, `#rgb`, 0-255, `default`, or one of the 16 names.
pub fn parse_colour(word: &str) -> Result<Color, String> {
    let lower = word.to_ascii_lowercase().replace(['-', '_'], "");
    if let Some(hex) = lower.strip_prefix('#') {
        let digits: Vec<u8> =
            hex.chars().map(|c| c.to_digit(16).map(|d| d as u8)).collect::<Option<_>>().unwrap_or_default();
        return match digits[..] {
            [r, g, b] => Ok(Color::Rgb(r * 17, g * 17, b * 17)),
            [r1, r2, g1, g2, b1, b2] => Ok(Color::Rgb(r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2)),
            _ => Err(format!("\"{word}\" is not a colour (#rrggbb)")),
        };
    }
    if let Ok(index) = lower.parse::<u8>() {
        return Ok(Color::Indexed(index));
    }
    let light = lower.strip_prefix("light").or_else(|| lower.strip_prefix("bright"));
    Ok(match (lower.as_str(), light) {
        ("default" | "reset", _) => Color::Reset,
        ("black", _) => Color::Black,
        ("red", _) => Color::Red,
        ("green", _) => Color::Green,
        ("yellow" | "brown", _) => Color::Yellow,
        ("blue", _) => Color::Blue,
        ("magenta", _) => Color::Magenta,
        ("cyan", _) => Color::Cyan,
        ("gray" | "grey", _) | (_, Some("gray" | "grey")) => Color::Gray,
        ("darkgray" | "darkgrey", _) | (_, Some("black")) => Color::DarkGray,
        (_, Some("red")) => Color::LightRed,
        (_, Some("green")) => Color::LightGreen,
        (_, Some("yellow")) => Color::LightYellow,
        (_, Some("blue")) => Color::LightBlue,
        (_, Some("magenta")) => Color::LightMagenta,
        (_, Some("cyan")) => Color::LightCyan,
        ("white", _) | (_, Some("white")) => Color::White,
        _ => return Err(format!("\"{word}\" is not a colour")),
    })
}

fn colour_text(colour: Color) -> String {
    match colour {
        Color::Reset => "default".into(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => i.to_string(),
        named => format!("{named:?}").to_ascii_lowercase(),
    }
}

/// A style as a scheme file writes it.
pub fn style_text(style: Style) -> String {
    let mut words = Vec::new();
    if let Some(fg) = style.fg {
        words.push(colour_text(fg));
    }
    if let Some(bg) = style.bg {
        words.push(format!("on {}", colour_text(bg)));
    }
    for (modifier, name) in MODIFIERS {
        if style.add_modifier.contains(modifier) {
            words.push(name.to_string());
        }
    }
    words.join(" ")
}

/// The headings a scheme file has, each before its first part.
const GROUPS: &[(&str, &str)] = &[
    ("panel", "Panels"),
    ("directory", "Files by kind (marks and the cursor go on top)"),
    ("git_branch", "Git"),
    ("found_file", "Find (Alt+F7)"),
    ("command_line", "Command line, F-keys, tabs"),
    ("dialog", "Dialogs"),
];

/// A scheme file for `choice`, every part spelled out, on top of the built-in one of that name.
pub fn file_text(choice: &Choice) -> String {
    let mut text = format!(
        "# cm colour scheme \"{name}\". Alt+C in cm picks a scheme and shows it on the whole\n\
         # screen; F4 there opens this file, which cm reads again when the editor closes.\n\
         #\n\
         # Each line gives a part of the screen its look:\n\
         #     part = colour [on colour] [bold] [dim] [italic] [underline] [reverse]\n\
         # A colour is #rrggbb, a number 0-255 (the terminal's palette), default (the\n\
         # terminal's own), or black, red, green, yellow, blue, magenta, cyan, gray, darkgray,\n\
         # lightred, lightgreen, lightyellow, lightblue, lightmagenta, lightcyan, white.\n\
         # A part drawn on a panel, in a dialog or on the command line has their background\n\
         # unless it names one. A part left out comes from the base scheme; an empty one adds\n\
         # nothing. Text after ; is a comment.\n\
         \n\
         description = {about}\n",
        name = choice.name,
        about = choice.about
    );
    if built_in(&choice.name).is_some() {
        text.push_str(&format!("base = {}\n", choice.name.to_ascii_lowercase()));
    }
    for (name, about) in Scheme::PARTS {
        if let Some((_, heading)) = GROUPS.iter().find(|(first, _)| first == name) {
            text.push_str(&format!("\n# {heading}\n"));
        }
        let value = style_text(choice.scheme.part(name).unwrap_or_default());
        text.push_str(&format!("{name:<13} = {value:<28} ; {}\n", about.trim()));
    }
    text
}

/// F4 in the picker: the file of a scheme to change. A built-in one is written out first, as
/// `<name>.ini` in `dir` (which then takes its place); one already there is left as it is.
/// The text comes along when the file was written just now.
pub fn file_for(choice: &Choice, dir: Option<&Path>) -> io::Result<(PathBuf, Option<String>)> {
    if let Some(file) = &choice.file {
        return Ok((file.clone(), None));
    }
    let dir = dir.ok_or_else(|| io::Error::other("there is no folder for settings (APPDATA, or HOME on Linux)"))?;
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.ini", choice.name));
    if path.exists() {
        return Ok((path, None));
    }
    let text = file_text(choice);
    fs::write(&path, &text)?;
    Ok((path, Some(text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn styles_read_and_write_back() {
        let style = parse_style("#2E3440 on lightcyan bold underline").unwrap();
        assert_eq!(style, Style::new().fg(rgb(0x2e3440)).bg(Color::LightCyan).bold().underlined());
        assert_eq!(style_text(style), "#2e3440 on lightcyan bold underline");
        assert_eq!(parse_style("on 236").unwrap(), Style::new().bg(Color::Indexed(236)));
        assert_eq!(parse_style("bold").unwrap(), Style::new().bold());
        assert_eq!(parse_style("").unwrap(), Style::new());
        assert_eq!(parse_style("#abc reversed").unwrap(), Style::new().fg(Color::Rgb(0xaa, 0xbb, 0xcc)).reversed());
        assert_eq!(parse_style("default on default").unwrap(), Style::new().fg(Color::Reset).bg(Color::Reset));
        // mc's names: brightred, lightgray; and grey.
        assert_eq!(parse_colour("brightred"), Ok(Color::LightRed));
        assert_eq!(parse_colour("lightgray"), Ok(Color::Gray));
        assert_eq!(parse_colour("dark-grey"), Ok(Color::DarkGray));
        assert!(parse_style("blu").unwrap_err().contains("\"blu\" is not a colour"));
        assert!(parse_style("red green").is_err(), "one colour before \"on\"");
        assert!(parse_style("red on").is_err());
        assert!(parse_style("#12345").is_err());
    }

    /// Every built-in scheme, written out as a file and read back, is the same scheme.
    #[test]
    fn built_in_schemes_survive_a_file() {
        for (name, ..) in BUILT_IN {
            let choice = choices(None).into_iter().find(|c| c.name == *name).unwrap();
            let (scheme, about, problems) = parse_file(&file_text(&choice));
            assert!(problems.is_empty(), "{name}: {problems:?}");
            assert_eq!(scheme, choice.scheme, "{name}");
            assert_eq!(about.as_deref(), Some(choice.about.as_str()));
        }
    }

    #[test]
    fn a_file_starts_from_its_base_and_says_what_it_cannot_read() {
        let text = "\u{feff}# mine\ndescription = Nord, my way\nbase = Nord\n\npanel = white on #000000 ; comment\n\
                    cursor = on #444444\ntitle-active = bold\nmarked = sparkly\nshiny = red\nno equals sign\n";
        let (scheme, about, problems) = parse_file(text);
        assert_eq!(about.as_deref(), Some("Nord, my way"));
        assert_eq!(scheme.panel, Style::new().white().bg(rgb(0x000000)));
        assert_eq!(scheme.cursor, Style::new().bg(rgb(0x444444)), "a cursor that only names a background");
        assert_eq!(scheme.title_active, Style::new().bold());
        assert_eq!(scheme.directory, nord().directory, "the rest is Nord's");
        assert_eq!(scheme.marked, nord().marked, "a line it couldn't read changes nothing");
        assert_eq!(
            problems,
            ["line 8: \"sparkly\" is not a colour", "line 9: no part is called \"shiny\"", "line 10: no \"=\" in it"]
        );
        // Without a base: classic.
        let (scheme, _, problems) = parse_file("base = nope\nheading = red\n");
        assert_eq!(problems, ["line 1: there is no built-in scheme \"nope\""]);
        assert_eq!(scheme, Scheme { heading: Style::new().red(), ..classic() });
    }

    /// The user's files: one named like a built-in scheme takes its place, others follow by
    /// name; F4 writes a built-in one out once, and the file then counts.
    #[test]
    fn files_join_and_replace_the_built_in_schemes() {
        let tmp = TempDir::new("schemes");
        let dir = tmp.path().join("schemes");
        assert_eq!(choices(Some(&dir)).len(), BUILT_IN.len(), "no folder yet");
        let nord = choices(None).into_iter().find(|c| c.name == "nord").unwrap();
        let (file, written) = file_for(&nord, Some(&dir)).unwrap();
        assert_eq!(file, dir.join("nord.ini"));
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(written.as_deref(), Some(text.as_str()), "written just now");
        assert!(text.contains("base = nord\n") && text.contains("\n# Git\n"), "{text}");
        let line = format!("{:<13} = {:<28} ; {}", "panel", "#d8dee9 on #2e3440", "text and background of the panels");
        assert!(text.contains(&line), "{text}");
        fs::write(&file, text.replace("panel         = #d8dee9", "panel = #ffffff")).unwrap();
        tmp.file("schemes/zebra.ini", "heading = white\n");
        tmp.file("schemes/Alpha.INI", "description = the first\n");
        tmp.file("schemes/notes.txt", "not a scheme");
        let list = choices(Some(&dir));
        let names: Vec<&str> = list.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(&names[BUILT_IN.len()..], ["Alpha", "zebra"]);
        let mine = &list[names.iter().position(|n| *n == "nord").unwrap()];
        assert_eq!(mine.file.as_deref(), Some(file.as_path()));
        assert_eq!(mine.scheme.panel, Style::new().fg(rgb(0xffffff)).bg(rgb(0x2e3440)));
        assert_eq!(list[BUILT_IN.len()].about, "the first");
        assert_eq!(list[BUILT_IN.len() + 1].about, "your scheme");
        assert_eq!(find("ZEBRA", Some(&dir)).unwrap().scheme.heading, Style::new().white());
        // Written once: F4 again opens the file as it is.
        assert_eq!(file_for(mine, Some(&dir)).unwrap(), (file.clone(), None));
        assert!(fs::read_to_string(&file).unwrap().contains("panel = #ffffff"));
    }

    #[test]
    fn colours_come_out_as_near_as_the_terminal_allows() {
        let true_colour = Colours::default();
        let scheme = nord();
        assert_eq!(scheme.for_terminal(true_colour), scheme);
        let reduced = scheme.for_terminal(Colours { depth: Depth::Ansi256, off: false });
        // #2e3440 is nearest grey 237 (#3a3a3a); #88c0d0 the cube's (135, 175, 215).
        assert_eq!(reduced.panel.bg, Some(Color::Indexed(237)));
        assert_eq!(reduced.cursor.bg, Some(Color::Indexed(16 + 36 * 2 + 6 * 3 + 4)));
        assert_eq!(nearest_256((0, 0, 0)), 16);
        assert_eq!(nearest_256((255, 255, 255)), 231);
        assert_eq!(nearest_256((128, 128, 128)), 244);
        // On 16 colours: the nearest of them; named ones stay.
        let sixteen = Colours { depth: Depth::Ansi16, off: false };
        assert_eq!(scheme.for_terminal(sixteen).panel.bg, Some(Color::Black));
        assert_eq!(scheme.for_terminal(sixteen).panel.fg, Some(Color::Gray));
        assert_eq!(classic().for_terminal(sixteen), classic());
        assert_eq!(reduce(Color::Indexed(196), Depth::Ansi16), Color::LightRed);
        // NO_COLOR: mono, which names no colour at all.
        let off = Colours { off: true, ..true_colour };
        assert_eq!(scheme.for_terminal(off), mono());
        let mut colours = Vec::new();
        mono().each_mut(|style| colours.extend([style.fg, style.bg]));
        assert!(colours.iter().all(Option::is_none));
    }

    #[test]
    fn the_terminal_says_how_many_colours() {
        assert_eq!(depth_from(true, "", "", "", false), Depth::TrueColor, "Windows");
        assert_eq!(depth_from(false, "truecolor", "xterm-256color", "", false), Depth::TrueColor);
        assert_eq!(depth_from(false, "", "xterm-256color", "", true), Depth::TrueColor, "Windows Terminal (WSL)");
        assert_eq!(depth_from(false, "", "xterm-256color", "", false), Depth::Ansi256, "ssh");
        assert_eq!(depth_from(false, "", "xterm-direct", "", false), Depth::TrueColor);
        assert_eq!(depth_from(false, "", "linux", "", false), Depth::Ansi16);
    }

    #[test]
    fn files_have_kinds() {
        assert_eq!(kind_of("photos.ZIP"), Some(Kind::Archive));
        assert_eq!(kind_of("backup.tar.gz"), Some(Kind::Archive));
        assert_eq!(kind_of("IMG_0001.CR3"), Some(Kind::Image));
        assert_eq!(kind_of("clip.mkv"), Some(Kind::Media));
        assert_eq!(kind_of("main.rs"), Some(Kind::Source));
        assert_eq!(kind_of("README.md"), Some(Kind::Document));
        assert_eq!(kind_of("notes.txt~"), Some(Kind::Temp));
        assert_eq!(kind_of("#draft#"), Some(Kind::Temp));
        assert_eq!(kind_of("settings.bak"), Some(Kind::Temp));
        assert_eq!(kind_of(".gitignore"), None, "a dot file has no extension");
        assert_eq!(kind_of("Makefile"), None);
        assert_eq!(kind_of("#"), None);
    }
}
