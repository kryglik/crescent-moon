//! Drawing: two panels, the command line, the function-key bar, dialogs and job progress.

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
use crate::ops::Running;
use crate::panel::{Entry, Panel};
use crate::util;

const KEYS: [&str; 10] = ["Help", "Rename", "View", "Edit", "Copy", "Move", "Mkdir", "Delete", "Sort", "Quit"];

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
    let [panels_area, cmd_area, keys_area] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(panels_area);

    let active = app.active;
    let search = app.quicksearch.clone();
    let mut search_cursor = None;
    for (side, rect) in [left, right].into_iter().enumerate() {
        let query = if side == active { search.as_deref() } else { None };
        let cursor = draw_panel(frame.buffer_mut(), rect, &mut app.panels[side], side == active, query);
        search_cursor = search_cursor.or(cursor);
    }
    let cmd_cursor = draw_cmdline(frame, cmd_area, app);
    draw_keybar(frame.buffer_mut(), keys_area);

    if let Some(job) = &app.job {
        draw_progress(frame, area, job);
    }
    if let Some(dialog) = &app.dialog {
        if let Some(position) = draw_dialog(frame, area, dialog, &app.hotlist) {
            frame.set_cursor_position(position);
        }
    } else if app.job.is_none() {
        frame.set_cursor_position(search_cursor.unwrap_or(cmd_cursor));
    }
}

/// Draws one panel; returns the cursor position while quick search is typing into it.
fn draw_panel(buf: &mut Buffer, area: Rect, panel: &mut Panel, active: bool, search: Option<&str>) -> Option<Position> {
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
        return None;
    }

    let list_h = (inner.height - 3) as usize;
    panel.ensure_visible(list_h);
    let details = inner.width >= 34;
    let (size_w, time_w) = (7usize, 12usize);
    let name_w = if details { inner_w - size_w - time_w - 2 } else { inner_w };
    let size_x = inner.x + name_w as u16 + 1;
    let time_x = size_x + size_w as u16 + 1;

    // Column headings, with the sort indicator in the corner like mc.
    let heading = base.fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let sort = panel.sort.indicator();
    buf.set_string(inner.x, inner.y, sort, base);
    buf.set_string(inner.x + 2, inner.y, util::center("Name", name_w.saturating_sub(2)), heading);
    if details {
        buf.set_string(size_x - 1, inner.y, "│", base);
        buf.set_string(size_x, inner.y, util::center("Size", size_w), heading);
        buf.set_string(time_x - 1, inner.y, "│", base);
        buf.set_string(time_x, inner.y, util::center("Modify time", time_w), heading);
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
        return Some(Position::new(inner.x + x, status_y));
    }
    let status = match panel.current() {
        Some(e) if e.is_parent => "UP--DIR".to_string(),
        Some(e) => e.name.clone(),
        None => String::new(),
    };
    buf.set_string(inner.x, status_y, util::fit(&status, inner_w), base);
    None
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
    let mut style = Style::new().fg(fg).bg(Color::Blue);
    if marked || entry.is_some_and(|e| e.is_dir) {
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

/// Clears a centred box and draws a dialog frame; returns the area inside it.
fn dialog_frame(frame: &mut Frame, area: Rect, width: u16, height: u16, title: &str, error: bool) -> Rect {
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
    inner
}

fn buttons_line(buttons: &[Button], focus: usize, base: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, button) in buttons.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ", base));
        }
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
    Line::from(spans).centered()
}

/// A message with buttons under it (messages, confirmations and job questions).
fn draw_message(frame: &mut Frame, area: Rect, title: &str, lines: &[String], buttons: &[Button], focus: usize, error: bool) {
    let widest = lines.iter().map(|l| util::width(l)).max().unwrap_or(0).max(util::width(title) + 4);
    let width = (widest as u16 + 4).clamp(34, area.width.saturating_sub(4).max(10));
    let height = lines.len() as u16 + 4;
    let inner = dialog_frame(frame, area, width, height, title, error);
    let style = dialog_style(error);
    let mut text: Vec<Line> = lines.iter().map(|l| Line::from(util::fit_left(l, inner.width as usize))).collect();
    text.push(Line::default());
    text.push(buttons_line(buttons, focus, style));
    frame.render_widget(Paragraph::new(text).style(style), inner);
}

/// Draws the open dialog; returns where the text cursor goes, if it has one.
fn draw_dialog(frame: &mut Frame, area: Rect, dialog: &Dialog, hotlist: &Hotlist) -> Option<Position> {
    match dialog {
        Dialog::Message { title, lines, error } => draw_message(frame, area, title, lines, dialog::OK, 0, *error),
        Dialog::Confirm { title, lines, focus, danger, .. } => {
            draw_message(frame, area, title, lines, dialog::YES_NO, *focus, *danger)
        }
        Dialog::Question { title, lines, buttons, focus, error, .. } => {
            draw_message(frame, area, title, lines, buttons, *focus, *error)
        }
        Dialog::Input { title, prompt, edit, .. } => {
            let width = (area.width * 2 / 3).clamp(40, 90).min(area.width);
            let inner = dialog_frame(frame, area, width, 6, title, false);
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
            return Some(Position::new(inner.x + cursor as u16, inner.y + 1));
        }
        Dialog::Drives { drives, cursor, side, .. } => {
            let title = if *side == 0 { "Left drive" } else { "Right drive" };
            let inner = dialog_frame(frame, area, 52, drives.len() as u16 + 2, title, false);
            let style = dialog_style(false);
            let lines: Vec<Line> = drives
                .iter()
                .enumerate()
                .map(|(i, drive)| {
                    let space = drive
                        .space
                        .map(|(free, total)| format!("{} free of {}", util::human_size(free), util::human_size(total)))
                        .unwrap_or_default();
                    let text = format!(
                        "{}  {} {} {}",
                        drive.root.display(),
                        util::fit(&drive.label, 12),
                        util::pad(drive.kind, 9),
                        space
                    );
                    let style = if i == *cursor { cursor_style() } else { style };
                    Line::from(Span::styled(util::fit(&text, inner.width as usize), style))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines).style(style), inner);
        }
        Dialog::Hotlist { cursor } => draw_hotlist(frame, area, hotlist, *cursor),
        Dialog::Help { scroll } => {
            let height = (dialog::HELP.len() as u16 + 2).min(area.height.saturating_sub(2));
            let inner = dialog_frame(frame, area, 64, height, "rmc keys (Esc closes)", false);
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
        }
    }
    None
}

fn draw_hotlist(frame: &mut Frame, area: Rect, hotlist: &Hotlist, cursor: usize) {
    let rows = hotlist.dirs.len().max(1) as u16;
    let height = (rows + 4).min(area.height.saturating_sub(2)).max(6);
    let width = (area.width * 2 / 3).clamp(50, 100).min(area.width);
    let inner = dialog_frame(frame, area, width, height, "Directory hotlist", false);
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
}

fn draw_progress(frame: &mut Frame, area: Rect, job: &Running) {
    let width = area.width.saturating_sub(4).clamp(30, 72);
    let inner = dialog_frame(frame, area, width, 8, job.title, false);
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
            shell: Shell::detect(),
            hotlist_file: None,
        };
        App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), config)
    }

    #[test]
    fn renders_panels_and_key_bar() {
        let tmp = TempDir::new("ui-render");
        tmp.file("Příliš žluťoučký kůň.txt", "kůň");
        tmp.file("run.bat", "@echo hi");
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
    fn renders_dialogs_on_small_and_large_screens() {
        let tmp = TempDir::new("ui-dialogs");
        tmp.file("a.txt", "a");
        let mut app = app_for(&tmp);
        app.on_key(ratatui::crossterm::event::KeyEvent::from(ratatui::crossterm::event::KeyCode::F(1)));
        let help = render(&mut app, 80, 40);
        assert!(help.contains("rmc keys"));
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
