//! Drawing: two panels, the command line, the function-key bar, dialogs and job progress.
//! Every frame also records where things are (`HitAreas`) so mouse clicks can find them.

use std::time::SystemTime;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Gauge, Padding, Paragraph, Widget};

use crate::app::App;
use crate::dialog::{self, Button, Dialog};
use crate::hotlist::Hotlist;
use crate::mouse::{DialogHit, HitAreas, PanelHit};
use crate::ops::Running;
use crate::panel::{Entry, Panel, SortBy};
use crate::platform::DriveInfo;
use crate::subshell::Subshell;
use crate::util;

pub const KEYS: [&str; 10] =["Help", "Rename", "View", "Edit", "Copy", "Move", "Mkdir", "Delete", "Sort", "Quit"];

fn panel_style() -> Style {
    Style::new().fg(Color::Gray).bg(Color::Blue)
}

fn cursor_style() -> Style {
    Style::new().fg(Color::Black).bg(Color::Cyan)
}

fn dialog_style(error: bool) -> Style {
    if error { Style::new().fg(Color::White).bg(Color::Red) } else { Style::new().fg(Color::Black).bg(Color::Gray) }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    // The shell's screen always matches the terminal, shown or not, so its output wraps right.
    app.screen_size = (area.height, area.width);
    if let Some(shell) = app.subshell.as_mut() {
        shell.resize(area.height, area.width);
    }
    if app.shell_shown
        && let Some(shell) = &app.subshell
    {
        if let Some(cursor) = draw_shell(frame.buffer_mut(), area, shell) {
            frame.set_cursor_position(cursor);
        }
        return;
    }

    let [panels_area, cmd_area, keys_area] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(panels_area);

    let mut hit = HitAreas { keybar: keys_area, ..HitAreas::default() };
    let active = app.active;
    let search = app.quicksearch.clone();
    let mut search_cursor = None;
    for (side, rect) in [left, right].into_iter().enumerate() {
        let query = if side == active { search.as_deref() } else { None };
        let (cursor, panel_hit) = draw_panel(frame.buffer_mut(), rect, &mut app.panels[side], side == active, query);
        hit.panels[side] = panel_hit;
        search_cursor = search_cursor.or(cursor);
    }
    let cmd_cursor = draw_cmdline(frame, cmd_area, app);
    draw_keybar(frame.buffer_mut(), keys_area);

    if let Some(job) = &app.job {
        draw_progress(frame, area, job);
    }
    if let Some(dialog) = &app.dialog {
        let (cursor, dialog_hit) = draw_dialog(frame, area, dialog, &app.hotlist);
        if let Some(position) = cursor {
            frame.set_cursor_position(position);
        }
        hit.dialog = Some(dialog_hit);
    } else if app.job.is_none() {
        frame.set_cursor_position(search_cursor.unwrap_or(cmd_cursor));
    }
    app.hit = hit;
}

/// Draws one panel. Returns the cursor position while quick search is typing into it,
/// and where the panel's parts are for the mouse.
fn draw_panel(
    buf: &mut Buffer,
    area: Rect,
    panel: &mut Panel,
    active: bool,
    search: Option<&str>,
) -> (Option<Position>, PanelHit) {
    let base = panel_style();
    let inner_w = area.width.saturating_sub(2) as usize;
    let path = util::fit_left(&panel.cwd.display().to_string(), inner_w.saturating_sub(4));
    let title_style = if active { cursor_style() } else { base };
    let mut block = Block::bordered()
        .border_style(base)
        .style(base)
        .title(Line::from(Span::styled(format!(" {path} "), title_style)));
    if let Some((free, total)) = panel.space {
        let percent = free.saturating_mul(100) / total.max(1);
        let text = format!(" {} / {} ({percent}%) ", util::human_size(free), util::human_size(total));
        if util::width(&text) + 2 < inner_w {
            block = block.title_bottom(Line::from(text).right_aligned());
        }
    }
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 4 || inner.width < 8 {
        return (None, PanelHit { area, ..PanelHit::default() });
    }

    let list_h = (inner.height - 3) as usize;
    panel.ensure_visible(list_h);
    let details = inner.width >= 34;
    let (size_w, time_w) = (7usize, 12usize);
    let name_w = if details { inner_w - size_w - time_w - 2 } else { inner_w };
    let size_x = inner.x + name_w as u16 + 1;
    let time_x = size_x + size_w as u16 + 1;
    let hit = PanelHit {
        area,
        list: Rect::new(inner.x, inner.y + 1, inner.width, list_h as u16),
        header_y: inner.y,
        size_x,
        time_x,
        details,
    };

    // Column headings, with the sort indicator in the corner like mc, and an arrow on the
    // sorted column: ↑ for A to Z (small to big, old to new), ↓ the other way.
    let heading = base.fg(Color::Yellow).add_modifier(Modifier::BOLD);
    buf.set_string(inner.x, inner.y, panel.sort.indicator(), base);
    let arrow = if panel.sort.descending(panel.reverse) { "↓" } else { "↑" };
    let title = |text: &str, sorted: bool| if sorted { format!("{text}{arrow}") } else { text.to_string() };
    let by_name = matches!(panel.sort, SortBy::Name | SortBy::Extension);
    buf.set_string(inner.x + 2, inner.y, util::center(&title("Name", by_name), name_w.saturating_sub(2)), heading);
    if details {
        buf.set_string(size_x - 1, inner.y, "│", base);
        buf.set_string(size_x, inner.y, util::center(&title("Size", panel.sort == SortBy::Size), size_w), heading);
        buf.set_string(time_x - 1, inner.y, "│", base);
        let time = title("Modify time", panel.sort == SortBy::Time);
        buf.set_string(time_x, inner.y, util::center(&time, time_w), heading);
    }

    let now = SystemTime::now();
    for row in 0..list_h {
        let y = inner.y + 1 + row as u16;
        let index = panel.offset + row;
        let entry = panel.entries.get(index);
        let marked = entry.is_some_and(|e| panel.is_marked(e));
        let on_cursor = active && index == panel.cursor && entry.is_some();
        let style = entry_style(entry, marked, on_cursor);
        let (name, size, time) = match entry {
            Some(e) => (
                format!("{}{}", prefix(e), e.name),
                size_text(panel, e),
                e.modified.map(|t| util::format_time(t, now)).unwrap_or_default(),
            ),
            None => Default::default(),
        };
        buf.set_string(inner.x, y, util::fit(&name, name_w), style);
        if details {
            let separator = if on_cursor { style } else { base };
            buf.set_string(size_x - 1, y, "│", separator);
            buf.set_string(size_x, y, util::pad_left(&size, size_w), style);
            buf.set_string(time_x - 1, y, "│", separator);
            buf.set_string(time_x, y, util::fit(&time, time_w), style);
        }
    }

    // Separator line joined to the frame, with the marked-files summary on it.
    let sep_y = inner.y + 1 + list_h as u16;
    buf.set_string(area.x, sep_y, "├", base);
    buf.set_string(inner.x, sep_y, "─".repeat(inner_w), base);
    buf.set_string(area.right() - 1, sep_y, "┤", base);
    let summary = match (panel.marked_summary(), panel.total()) {
        (Some((count, bytes)), _) => {
            Some(format!(" {} in {count} item{} ", util::human_size(bytes), if count == 1 { "" } else { "s" }))
        }
        (None, Some(total)) => Some(format!(" {} in this folder ", util::human_size(total))),
        (None, None) => None,
    };
    if let Some(text) = summary {
        let w = util::width(&text) as u16;
        if w < inner.width {
            buf.set_string(inner.x + (inner.width - w) / 2, sep_y, &text, heading);
        }
    }

    let status_y = sep_y + 1;
    if let Some(query) = search {
        let text = format!("Search: {query}");
        buf.set_string(inner.x, status_y, util::fit(&text, inner_w), cursor_style());
        let x = util::width(&text).min(inner_w.saturating_sub(1)) as u16;
        return (Some(Position::new(inner.x + x, status_y)), hit);
    }
    let status = match panel.current() {
        Some(e) if e.is_parent => "UP--DIR".to_string(),
        Some(e) => e.name.clone(),
        None => String::new(),
    };
    buf.set_string(inner.x, status_y, util::fit(&status, inner_w), base);
    (None, hit)
}

/// The persistent shell, full screen. Returns its cursor (none while scrolled back).
fn draw_shell(buf: &mut Buffer, area: Rect, shell: &Subshell) -> Option<Position> {
    // Until the shell writes anything, say what's happening instead of showing a blank screen.
    if let Some(waiting) = shell.starting_for() {
        let seconds = waiting.as_secs();
        let lines = if seconds < 3 {
            vec!["Starting the shell…".to_string(), "Ctrl+O goes back to the panels.".to_string()]
        } else {
            vec![
                format!("The shell hasn't answered for {seconds} s."),
                "Ctrl+O goes back to the panels.".to_string(),
                "To find out why, start rmc with RMC_LOG set to a file name (e.g. %TEMP%\\rmc.log).".to_string(),
            ]
        };
        let top = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
        for (i, line) in lines.iter().enumerate() {
            let y = top + i as u16;
            if y < area.bottom() {
                let text = util::center(line, area.width as usize);
                buf.set_string(area.x, y, text, Style::new().fg(if i == 0 { Color::Yellow } else { Color::Gray }));
            }
        }
        return None;
    }
    shell
        .with_screen(|screen| {
            let (rows, cols) = screen.size();
            for row in 0..area.height.min(rows.get()) {
                for col in 0..area.width.min(cols.get()) {
                    // The second half of a wide character belongs to the cell before it.
                    let Some(cell) = screen.cell(row, col).filter(|c| !c.is_wide_continuation()) else {
                        continue;
                    };
                    let target = &mut buf[(area.x + col, area.y + row)];
                    target.set_symbol(if cell.has_contents() { cell.contents() } else { " " });
                    target.set_style(shell_cell_style(cell));
                }
            }
            let scrolled = screen.scrollback();
            if scrolled > 0 {
                let note = format!(" ↑ {scrolled} lines back · Shift+PgDn or any key returns ");
                let w = util::width(&note).min(area.width as usize) as u16;
                let style = Style::new().fg(Color::Black).bg(Color::Yellow);
                buf.set_string(area.right() - w, area.y, util::take_width(&note, w as usize), style);
                return None;
            }
            if screen.hide_cursor() {
                return None;
            }
            let (row, col) = screen.cursor_position();
            Some(Position::new(area.x + col.min(area.width - 1), area.y + row.min(area.height - 1)))
        })
        .flatten()
}

fn shell_cell_style(cell: &vt100::Cell) -> Style {
    fn color(c: vt100::Color) -> Color {
        match c {
            vt100::Color::Default => Color::Reset,
            vt100::Color::Idx(i) => Color::Indexed(i),
            vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }
    let mut style = Style::new().fg(color(cell.fgcolor())).bg(color(cell.bgcolor()));
    for (on, modifier) in [
        (cell.bold(), Modifier::BOLD),
        (cell.dim(), Modifier::DIM),
        (cell.italic(), Modifier::ITALIC),
        (cell.underline(), Modifier::UNDERLINED),
        (cell.inverse(), Modifier::REVERSED),
    ] {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    style
}

/// mc's type markers: `/` directory, `~` link to a directory, `@` link, `*` executable.
fn prefix(entry: &Entry) -> &'static str {
    match entry {
        e if e.is_dir && e.is_link => "~",
        e if e.is_dir => "/",
        e if e.is_link => "@",
        e if e.is_exec => "*",
        _ => " ",
    }
}

/// File size; for directories `<DIR>`, or their measured size (`4.5G…` while still counting).
fn size_text(panel: &Panel, entry: &Entry) -> String {
    if entry.is_parent {
        return "UP--DIR".into();
    }
    if !entry.is_dir {
        return util::format_size(entry.size);
    }
    match panel.dir_size(entry) {
        Some(size) if size.done => util::format_size(size.bytes),
        Some(size) if size.bytes > 0 => format!("{}…", util::human_size(size.bytes)),
        Some(_) => "…".into(),
        None => "<DIR>".into(),
    }
}

fn entry_style(entry: Option<&Entry>, marked: bool, on_cursor: bool) -> Style {
    let fg = match entry {
        _ if marked => Color::Yellow,
        Some(e) if e.is_dir => Color::White,
        Some(e) if e.is_exec => Color::LightGreen,
        _ => Color::Gray,
    };
    // Folders stand out by their brighter white and the "/" before the name; only marked
    // entries are bold.
    let mut style = Style::new().fg(fg).bg(Color::Blue);
    if marked {
        style = style.add_modifier(Modifier::BOLD);
    }
    if on_cursor {
        style = style.bg(Color::Cyan).fg(if marked { Color::Yellow } else { Color::Black });
    }
    style
}

fn draw_cmdline(frame: &mut Frame, area: Rect, app: &App) -> Position {
    let cwd = app.active_panel().cwd.display().to_string();
    let prompt = format!("{}>", util::fit_left(&cwd, (area.width as usize / 3).max(10)));
    let prompt_w = util::width(&prompt) + 1;
    let (text, cursor) = app.cmdline.view((area.width as usize).saturating_sub(prompt_w + 1));
    frame.render_widget(Paragraph::new(Line::from(vec![Span::raw(prompt), Span::raw(" "), Span::raw(text)])), area);
    Position::new(area.x + (prompt_w + cursor) as u16, area.y)
}

fn draw_keybar(buf: &mut Buffer, area: Rect) {
    let slot = area.width / KEYS.len() as u16;
    for (i, label) in KEYS.iter().enumerate() {
        let x = area.x + slot * i as u16;
        let w = if i == KEYS.len() - 1 { area.width - slot * i as u16 } else { slot } as usize;
        let number = (i + 1).to_string();
        if w <= number.len() {
            continue;
        }
        buf.set_string(x, area.y, &number, Style::new().fg(Color::White));
        buf.set_string(x + number.len() as u16, area.y, util::fit(label, w - number.len()), cursor_style());
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (w, h) = (width.min(area.width), height.min(area.height));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

/// Clears a centred box and draws a dialog frame; returns the box and the area inside it.
fn dialog_frame(frame: &mut Frame, area: Rect, width: u16, height: u16, title: &str, error: bool) -> (Rect, Rect) {
    let rect = centered(area, width, height);
    let style = dialog_style(error);
    let title_style = if error { style.add_modifier(Modifier::BOLD) } else { style.fg(Color::Blue) };
    let block = Block::bordered()
        .style(style)
        .padding(Padding::horizontal(1))
        .title(Line::from(Span::styled(format!(" {title} "), title_style)).centered());
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    (rect, inner)
}

/// The buttons centred in `width` columns (by hand, so their positions are known exactly),
/// and each button's offset and width for the mouse.
fn buttons_line(buttons: &[Button], focus: usize, base: Style, width: u16) -> (Line<'static>, Vec<(u16, u16)>) {
    let widths: Vec<u16> = buttons.iter().map(|b| util::width(b.label) as u16 + 4).collect();
    let total = widths.iter().sum::<u16>() + widths.len().saturating_sub(1) as u16;
    let mut x = width.saturating_sub(total) / 2;
    let mut spans = vec![Span::styled(" ".repeat(x as usize), base)];
    let mut places = Vec::new();
    for (i, button) in buttons.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ", base));
            x += 1;
        }
        places.push((x, widths[i]));
        x += widths[i];
        let style = if i == focus { cursor_style() } else { base };
        let hot = button.label.char_indices().find(|(_, c)| c.eq_ignore_ascii_case(&button.hotkey));
        spans.push(Span::styled("[ ", style));
        match hot {
            Some((at, c)) => {
                let after = at + c.len_utf8();
                spans.push(Span::styled(button.label[..at].to_string(), style));
                spans.push(Span::styled(button.label[at..after].to_string(), style.add_modifier(Modifier::UNDERLINED)));
                spans.push(Span::styled(button.label[after..].to_string(), style));
            }
            None => spans.push(Span::styled(button.label, style)),
        }
        spans.push(Span::styled(" ]", style));
    }
    (Line::from(spans), places)
}

/// A message with buttons under it (messages, confirmations and job questions).
fn draw_message(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: &[String],
    buttons: &[Button],
    focus: usize,
    error: bool,
) -> DialogHit {
    let widest = lines.iter().map(|l| util::width(l)).max().unwrap_or(0).max(util::width(title) + 4);
    let width = (widest as u16 + 4).clamp(34, area.width.saturating_sub(4).max(10));
    let height = lines.len() as u16 + 4;
    let (outer, inner) = dialog_frame(frame, area, width, height, title, error);
    let style = dialog_style(error);
    let mut text: Vec<Line> = lines.iter().map(|l| Line::from(util::fit_left(l, inner.width as usize))).collect();
    text.push(Line::default());
    let button_y = inner.y + text.len() as u16;
    let (line, places) = buttons_line(buttons, focus, style, inner.width);
    text.push(line);
    frame.render_widget(Paragraph::new(text).style(style), inner);
    let buttons = if button_y < inner.bottom() {
        places.into_iter().enumerate().map(|(i, (x, w))| (Rect::new(inner.x + x, button_y, w, 1), i)).collect()
    } else {
        Vec::new()
    };
    DialogHit { area: outer, buttons, rows: None }
}

/// Draws the open dialog; returns where the text cursor goes, if it has one, and where
/// its buttons and rows are.
fn draw_dialog(frame: &mut Frame, area: Rect, dialog: &Dialog, hotlist: &Hotlist) -> (Option<Position>, DialogHit) {
    match dialog {
        Dialog::Message { title, lines, error } => (None, draw_message(frame, area, title, lines, dialog::OK, 0, *error)),
        Dialog::Confirm { title, lines, focus, danger, .. } => {
            (None, draw_message(frame, area, title, lines, dialog::YES_NO, *focus, *danger))
        }
        Dialog::Question { title, lines, buttons, focus, error, .. } => {
            (None, draw_message(frame, area, title, lines, buttons, *focus, *error))
        }
        Dialog::Input { title, prompt, edit, .. } => {
            let width = (area.width * 2 / 3).clamp(40, 90).min(area.width);
            let (outer, inner) = dialog_frame(frame, area, width, 6, title, false);
            let style = dialog_style(false);
            let field_w = inner.width as usize;
            let (text, cursor) = edit.view(field_w.saturating_sub(1));
            let lines = vec![
                Line::from(util::fit_left(prompt, field_w)),
                Line::from(Span::styled(util::pad(&text, field_w), cursor_style())),
                Line::default(),
                Line::from(Span::styled("Enter = OK   Esc = cancel", style.fg(Color::DarkGray))).right_aligned(),
            ];
            frame.render_widget(Paragraph::new(lines).style(style), inner);
            let cursor = Position::new(inner.x + cursor as u16, inner.y + 1);
            (Some(cursor), DialogHit { area: outer, ..DialogHit::default() })
        }
        Dialog::Drives { drives, cursor, side, .. } => (None, draw_drives(frame, area, drives, *cursor, *side)),
        Dialog::Sort { cursor, current, reverse, .. } => (None, draw_sort(frame, area, *cursor, *current, *reverse)),
        Dialog::Hotlist { cursor } => (None, draw_hotlist(frame, area, hotlist, *cursor)),
        Dialog::Help { scroll } => {
            let height = (dialog::HELP.len() as u16 + 2).min(area.height.saturating_sub(2));
            // 72 columns: room for the longest help line (68) inside the border and padding.
            let (outer, inner) = dialog_frame(frame, area, 72, height, "rmc keys (Esc closes)", false);
            let visible = inner.height as usize;
            let start = (*scroll).min(dialog::HELP.len().saturating_sub(visible));
            let lines: Vec<Line> = dialog::HELP[start..]
                .iter()
                .take(visible)
                .map(|l| {
                    let heading = !l.starts_with(' ') && !l.is_empty();
                    let style = if heading { dialog_style(false).fg(Color::Blue).add_modifier(Modifier::BOLD) } else { dialog_style(false) };
                    Line::from(Span::styled(*l, style))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines).style(dialog_style(false)), inner);
            (None, DialogHit { area: outer, ..DialogHit::default() })
        }
    }
}

/// The F9 menu: the four orders with their keys; the current one says which way round it is.
fn draw_sort(frame: &mut Frame, area: Rect, cursor: usize, current: SortBy, reverse: bool) -> DialogHit {
    let rows = SortBy::ALL.len() as u16;
    let (outer, inner) = dialog_frame(frame, area, 36, rows + 4, "Sort by", false);
    let style = dialog_style(false);
    let mut lines: Vec<Line> = SortBy::ALL
        .iter()
        .enumerate()
        .map(|(i, &by)| {
            let order = if by == current { by.direction(reverse) } else { "" };
            let text = format!("{}  {:<12}{order}", by.letter(), by.label());
            let row_style = if i == cursor { cursor_style() } else { style };
            Line::from(Span::styled(util::pad(&text, inner.width as usize), row_style))
        })
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("again: the other way round", style.fg(Color::DarkGray))).right_aligned());
    frame.render_widget(Paragraph::new(lines).style(style), inner);
    let list = Rect::new(inner.x, inner.y, inner.width, rows.min(inner.height));
    DialogHit { area: outer, buttons: Vec::new(), rows: Some((list, 0)) }
}

/// The drive picker: letter, label (or network share), type and free space. Slow drives
/// show "checking…" until their answer arrives.
fn draw_drives(frame: &mut Frame, area: Rect, drives: &[DriveInfo], cursor: usize, side: usize) -> DialogHit {
    let label_w = drives.iter().map(|d| util::width(&d.label)).max().unwrap_or(0).min(32);
    let texts: Vec<String> = drives
        .iter()
        .map(|drive| {
            let space = match (drive.space, drive.pending) {
                (Some((free, total)), _) => format!("{} free of {}", util::human_size(free), util::human_size(total)),
                (None, true) => "checking…".into(),
                (None, false) if drive.kind == "network" => "not reachable".into(),
                (None, false) => String::new(),
            };
            let label = if label_w > 0 { format!("{} ", util::fit(&drive.label, label_w)) } else { String::new() };
            format!("{}  {label}{} {space}", drive.root.display(), util::pad(drive.kind, 9))
        })
        .collect();
    // Room for "4.5T free of 10.9T" even before the answers arrive, so the box doesn't grow.
    let widest = texts.iter().map(|t| util::width(t)).max().unwrap_or(0).max(label_w + 37);
    let width = (widest as u16 + 4).max(30);
    let title = if side == 0 { "Left drive" } else { "Right drive" };
    let (outer, inner) = dialog_frame(frame, area, width, drives.len() as u16 + 2, title, false);
    let style = dialog_style(false);
    let lines: Vec<Line> = texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let row_style = if i == cursor { cursor_style() } else { style };
            Line::from(Span::styled(util::fit(text, inner.width as usize), row_style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).style(style), inner);
    let rows = Rect::new(inner.x, inner.y, inner.width, (drives.len() as u16).min(inner.height));
    DialogHit { area: outer, buttons: Vec::new(), rows: Some((rows, 0)) }
}

fn draw_hotlist(frame: &mut Frame, area: Rect, hotlist: &Hotlist, cursor: usize) -> DialogHit {
    let rows = hotlist.dirs.len().max(1) as u16;
    let height = (rows + 4).min(area.height.saturating_sub(2)).max(6);
    let width = (area.width * 2 / 3).clamp(50, 100).min(area.width);
    let (outer, inner) = dialog_frame(frame, area, width, height, "Directory hotlist", false);
    let style = dialog_style(false);
    let list_h = inner.height.saturating_sub(2) as usize;
    let field_w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    if hotlist.dirs.is_empty() {
        lines.push(Line::from("No favourite folders yet. Press Insert or A to add the current one."));
    }
    let offset = (cursor + 1).saturating_sub(list_h);
    for (i, dir) in hotlist.dirs.iter().enumerate().skip(offset).take(list_h) {
        let number = if i < 9 { format!("{} ", i + 1) } else { "  ".into() };
        let text = format!("{number}{}", util::fit_left(&dir.display().to_string(), field_w.saturating_sub(2)));
        let row_style = if i == cursor { cursor_style() } else { style };
        lines.push(Line::from(Span::styled(util::pad(&text, field_w), row_style)));
    }
    while lines.len() < list_h + 1 {
        lines.push(Line::default());
    }
    lines.truncate(list_h + 1);
    let hint = "Enter/1-9 go  Ins add here  Del remove  Shift+↑↓ move";
    lines.push(Line::from(Span::styled(util::fit_left(hint, field_w), style.fg(Color::DarkGray))).right_aligned());
    frame.render_widget(Paragraph::new(lines).style(style), inner);
    let shown = hotlist.dirs.len().saturating_sub(offset).min(list_h) as u16;
    let rows = (shown > 0).then(|| (Rect::new(inner.x, inner.y, inner.width, shown), offset));
    DialogHit { area: outer, buttons: Vec::new(), rows }
}

fn draw_progress(frame: &mut Frame, area: Rect, job: &Running) {
    let width = area.width.saturating_sub(4).clamp(30, 72);
    let (_, inner) = dialog_frame(frame, area, width, 8, job.title, false);
    if inner.height < 6 {
        return;
    }
    let p = &job.progress;
    let style = dialog_style(false);
    let row = |n: u16| Rect::new(inner.x, inner.y + n, inner.width, 1);
    let current = if p.scanning { format!("Scanning… {}", p.files_total) } else { p.current.clone() };
    frame.render_widget(Paragraph::new(util::fit_left(&current, inner.width as usize)).style(style), row(0));
    let ratio = if p.bytes_total > 0 {
        p.bytes_done as f64 / p.bytes_total as f64
    } else if p.files_total > 0 {
        p.files_done as f64 / p.files_total as f64
    } else {
        0.0
    };
    let gauge = Gauge::default().gauge_style(Style::new().fg(Color::Cyan).bg(Color::Black)).ratio(ratio.clamp(0.0, 1.0));
    frame.render_widget(gauge, row(2));
    let mut counts = format!("{} / {} items", p.files_done, p.files_total);
    if p.bytes_total > 0 {
        counts += &format!("   {} / {}", util::human_size(p.bytes_done), util::human_size(p.bytes_total));
    }
    frame.render_widget(Paragraph::new(counts).style(style), row(3));
    frame.render_widget(Paragraph::new(Line::from("Esc = cancel").right_aligned()).style(style.fg(Color::DarkGray)), row(5));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Config;
    use crate::platform::Shell;
    use crate::testutil::TempDir;
    use ratatui::Terminal;
    use std::path::PathBuf;
    use ratatui::backend::TestBackend;

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_for(tmp: &TempDir) -> App {
        let config = Config {
            editor: vec!["nvim".into()],
            viewer: vec!["nvim".into()],
            pager: None,
            shell: Shell::detect(),
            hotlist_file: None,
        };
        App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), config)
    }

    #[test]
    fn renders_panels_and_key_bar() {
        let tmp = TempDir::new("ui-render");
        tmp.file("Příliš žluťoučký kůň.txt", "kůň");
        let script = tmp.file("run.bat", "@echo hi");
        // Executable by extension on Windows, by its x bit elsewhere.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(windows)]
        let _ = script;
        tmp.dir("Účty");
        let mut app = app_for(&tmp);
        let screen = render(&mut app, 100, 16);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Modify time"));
        assert!(screen.contains("/Účty"));
        assert!(screen.contains(" Příliš žluťoučký kůň.txt"));
        assert!(screen.contains("*run.bat"));
        assert!(screen.contains("UP--DIR"));
        assert!(screen.contains("10Quit"));
    }

    #[test]
    fn renders_hotlist_and_measured_sizes() {
        use crate::panel::DirSize;
        let tmp = TempDir::new("ui-hotlist");
        tmp.dir("photos");
        tmp.dir("music");
        tmp.file("a.txt", "abc");
        let mut app = app_for(&tmp);
        let panel = &mut app.panels[0];
        panel.dir_sizes.insert("photos".into(), DirSize { bytes: 3 * 1024 * 1024 * 1024, done: true });
        panel.dir_sizes.insert("music".into(), DirSize { bytes: 5 * 1024 * 1024, done: false });
        app.hotlist.dirs = vec![PathBuf::from(r"C:\Users\me\Documents"), PathBuf::from(r"E:\devel")];
        app.dialog = Some(Dialog::Hotlist { cursor: 1 });
        let screen = render(&mut app, 100, 16);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Directory hotlist"));
        assert!(screen.contains(r"2 E:\devel"));
        assert!(screen.contains("3072M"), "measured size shown");
        assert!(screen.contains("5.0M…"), "running total shown");
    }

    #[test]
    fn renders_sort_arrows_and_the_sort_menu() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let tmp = TempDir::new("ui-sort");
        tmp.file("a.txt", "a");
        let mut app = app_for(&tmp);
        let screen = render(&mut app, 100, 16);
        assert!(screen.contains("Name↑"), "{screen}");

        app.panels[0].choose_sort(SortBy::Time);
        app.panels[0].choose_sort(SortBy::Time);
        app.on_key(KeyEvent::from(KeyCode::F(9)));
        let screen = render(&mut app, 100, 16);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Modify time↑"), "oldest first goes up:\n{screen}");
        assert!(screen.contains("Sort by"));
        assert!(screen.contains("e  Extension"));
        assert!(screen.contains("t  Modify time oldest first"), "{screen}");
    }

    #[test]
    fn draws_the_shell_full_screen() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use std::time::{Duration, Instant};
        if !matches!(Shell::detect(), Shell::PowerShell(_)) {
            return;
        }
        let tmp = TempDir::new("ui-shell");
        let mut app = app_for(&tmp);
        render(&mut app, 100, 20);
        for c in "Write-Output 'Příliš žluťoučký'".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut screen = String::new();
        while Instant::now() < deadline {
            screen = render(&mut app, 100, 20);
            if screen.lines().any(|l| l.trim() == "Příliš žluťoučký") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.lines().any(|l| l.trim() == "Příliš žluťoučký"), "{screen}");
        assert!(screen.contains("PS "), "the prompt is there");
        assert!(!screen.contains("10Quit"), "the panels are hidden");
    }

    #[test]
    fn drive_picker_fills_in_slow_drives() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::time::{Duration, Instant};
        let tmp = TempDir::new("ui-drives");
        let mut app = app_for(&tmp);
        app.on_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::ALT));
        let deadline = Instant::now() + Duration::from_secs(15);
        while app.is_busy() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            app.tick();
        }
        let screen = render(&mut app, 100, 20);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Left drive"));
        assert!(!screen.contains("checking…") || Instant::now() >= deadline);
    }

    #[test]
    fn renders_dialogs_on_small_and_large_screens() {
        let tmp = TempDir::new("ui-dialogs");
        tmp.file("a.txt", "a");
        let mut app = app_for(&tmp);
        app.on_key(ratatui::crossterm::event::KeyEvent::from(ratatui::crossterm::event::KeyCode::F(1)));
        let help = render(&mut app, 80, 40);
        assert!(help.contains("rmc keys"));
        // The longest help lines fit whole.
        assert!(help.contains("Enter runs it in the shell, in the panel's folder"), "{help}");
        assert!(help.contains("rmc --no-mouse leaves the mouse alone"), "{help}");
        app.on_key(ratatui::crossterm::event::KeyEvent::from(ratatui::crossterm::event::KeyCode::Esc));
        app.on_key(ratatui::crossterm::event::KeyEvent::from(ratatui::crossterm::event::KeyCode::F(7)));
        let mkdir = render(&mut app, 80, 24);
        assert!(mkdir.contains("New directory name:"));
        // Tiny terminals must not panic.
        for (w, h) in [(10, 3), (20, 6), (40, 8)] {
            render(&mut app, w, h);
        }
    }
}
