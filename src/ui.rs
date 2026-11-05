//! Drawing: two panels, the command line, the function-key bar, dialogs and job progress.
//! Every frame also records where things are (`HitAreas`) so mouse clicks can find them.

use std::path::Path;
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
use crate::dialog::LineEdit;
use crate::finder::{Field as FindField, Finder, Row};
use crate::git::{Head, Mark, Repo, Status, Tracker, Worktree};
use unicode_width::UnicodeWidthChar;
use crate::options::{self, OptionsAutomatic};
use crate::panel::{Entry, Panel, SortBy};
use crate::preview::{Content, Preview};
use crate::settings::Settings;
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
        if area.width > 0
            && area.height > 0
            && let Some(cursor) = draw_shell(frame.buffer_mut(), area, shell)
        {
            frame.set_cursor_position(cursor);
        }
        return;
    }
    // Too small for the panels (a narrow split pane): say so rather than draw out of bounds.
    if area.height < 6 || area.width < 20 {
        app.hit = HitAreas::default();
        let text = util::take_width("rmc: the window is too small", area.width as usize);
        if area.height > 0 {
            frame.buffer_mut().set_string(area.x, area.y, text, Style::new().fg(Color::Yellow));
        }
        return;
    }

    let [panels_area, cmd_area, keys_area] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let mut hit = HitAreas { keybar: keys_area, ..HitAreas::default() };
    // A row of tabs above the panels once either side has more than one.
    let panels_area = if app.tabs.iter().any(|t| !t.is_empty()) {
        let [tabs_area, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(panels_area);
        let halves: [Rect; 2] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(tabs_area);
        for (side, rect) in halves.into_iter().enumerate() {
            hit.tabs[side] = draw_tabs(frame.buffer_mut(), rect, &app.tab_names(side), app.tab_index[side], side == app.active);
        }
        rest
    } else {
        panels_area
    };
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(panels_area);
    let active = app.active;
    let search = app.quicksearch.clone();
    let mut search_cursor = None;
    for (side, rect) in [left, right].into_iter().enumerate() {
        if app.quick_view && side != active {
            let preview = app.previewer.as_ref().and_then(|p| p.current.as_ref());
            let inner = draw_quick_view(frame.buffer_mut(), rect, preview, app.preview_focus);
            app.preview_rows = inner.height as usize;
            hit.preview = Some(rect);
            continue;
        }
        let query = if side == active { search.as_deref() } else { None };
        let (cursor, panel_hit) = draw_panel(frame.buffer_mut(), rect, &mut app.panels[side], side == active, query, &app.git);
        hit.panels[side] = panel_hit;
        search_cursor = search_cursor.or(cursor);
    }
    let cmd_cursor = draw_cmdline(frame, cmd_area, app);
    draw_keybar(frame.buffer_mut(), keys_area);

    let mut find_cursor = None;
    if app.find_shown
        && let Some(finder) = app.finder.as_mut()
    {
        let (cursor, rows) = draw_find(frame.buffer_mut(), panels_area, finder);
        find_cursor = Some(cursor);
        hit.find = Some(rows);
    }
    if let Some(job) = &app.job {
        draw_progress(frame, area, job);
    }
    // Help can't scroll past its end (or keys up would first have to undo the excess).
    if let Some(Dialog::Help { scroll }) = &mut app.dialog {
        let visible = help_height(area).saturating_sub(2) as usize;
        *scroll = (*scroll).min(dialog::HELP.len().saturating_sub(visible));
    }
    if let Some(Dialog::History { filter, cursor }) = &app.dialog {
        let rows: Vec<String> = app.history_rows(filter.text()).iter().map(|d| d.display().to_string()).collect();
        let (position, dialog_hit) = draw_filtered_list(frame, area, "Folder history", &rows, filter, *cursor, HISTORY_HINT);
        frame.set_cursor_position(position);
        hit.dialog = Some(dialog_hit);
    } else if let Some(Dialog::Commands { filter, cursor }) = &app.dialog {
        let commands = dialog::matching_commands(filter.text());
        let label_w = commands.iter().map(|c| util::width(c.label)).max().unwrap_or(0);
        let rows: Vec<String> = commands.iter().map(|c| format!("{}  {}", util::pad(c.label, label_w), c.keys)).collect();
        let (position, dialog_hit) = draw_filtered_list(frame, area, "Commands", &rows, filter, *cursor, COMMANDS_HINT);
        frame.set_cursor_position(position);
        hit.dialog = Some(dialog_hit);
    } else if let Some(Dialog::Worktrees { filter, cursor, worktrees, here }) = &app.dialog {
        let rows = worktree_rows(&dialog::worktree_rows(worktrees, filter.text()), here);
        let (position, dialog_hit) = draw_filtered_list(frame, area, "Git worktrees", &rows, filter, *cursor, WORKTREES_HINT);
        frame.set_cursor_position(position);
        hit.dialog = Some(dialog_hit);
    } else if let Some(dialog) = &app.dialog {
        let (cursor, dialog_hit) = draw_dialog(frame, area, dialog, &app.hotlist, app.config.settings_file.as_deref());
        if let Some(position) = cursor {
            frame.set_cursor_position(position);
        }
        hit.dialog = Some(dialog_hit);
    } else if let Some(cursor) = find_cursor {
        frame.set_cursor_position(cursor);
    } else if app.job.is_none() {
        frame.set_cursor_position(search_cursor.unwrap_or(cmd_cursor));
    }
    app.hit = hit;
}

/// The Find window over both panels: the Text and Files fields, a hint, the results (each
/// file, then its lines with the matches marked) and a status line. Returns the text cursor
/// and the result rows' area with the first row shown, for the mouse.
fn draw_find(buf: &mut Buffer, area: Rect, finder: &mut Finder) -> (Position, (Rect, usize)) {
    let base = panel_style();
    let inner_w = area.width.saturating_sub(2) as usize;
    let title = format!(" Find in {} ", util::fit_left(&finder.root.display().to_string(), inner_w.saturating_sub(12)));
    let block = Block::bordered().border_style(base).style(base).title(Line::from(Span::styled(title, cursor_style())));
    let inner = block.inner(area);
    block.render(area, buf);
    let fallback = (Position::new(inner.x, inner.y), (Rect::default(), 0));
    if inner.height < 6 || inner.width < 20 {
        return fallback;
    }
    let width = inner.width as usize;
    let grey = base.fg(Color::Gray).add_modifier(Modifier::DIM);
    let field_style = Style::new().fg(Color::Black).bg(Color::Gray);

    // The two fields.
    let mut cursor = Position::new(inner.x, inner.y);
    for (i, (label, edit, field)) in
        [("Text  ", &finder.text, FindField::Text), ("Files ", &finder.names, FindField::Names)].into_iter().enumerate()
    {
        let y = inner.y + i as u16;
        let field_w = width.saturating_sub(label.len());
        let (text, column) = edit.view(field_w.saturating_sub(1));
        let focused = finder.field == field;
        buf.set_string(inner.x, y, label, base.fg(Color::Yellow));
        let style = if focused { cursor_style() } else { field_style };
        buf.set_string(inner.x + label.len() as u16, y, util::pad(&text, field_w), style);
        if focused {
            cursor = Position::new(inner.x + (label.len() + column) as u16, y);
        }
    }
    let hint = if finder.everything {
        "regex, smart case · searching everything, hidden and ignored files too (Alt+U)"
    } else {
        "regex, smart case · skips ignored, hidden and binary files (Alt+U: all) · Tab: Files"
    };
    buf.set_string(inner.x, inner.y + 2, util::fit(hint, width), grey);
    buf.set_string(area.x, inner.y + 3, "├", base);
    buf.set_string(inner.x, inner.y + 3, "─".repeat(width), base);
    buf.set_string(area.right() - 1, inner.y + 3, "┤", base);

    // The results.
    let list = Rect::new(inner.x, inner.y + 4, inner.width, inner.height - 5);
    finder.ensure_visible(list.height as usize);
    let mark = Style::new().fg(Color::Black).bg(Color::Yellow);
    for i in 0..list.height as usize {
        let y = list.y + i as u16;
        let index = finder.offset + i;
        let on_cursor = index == finder.cursor;
        let line = match finder.row(index) {
            Some(Row::File(f)) => {
                let file = &finder.files[f];
                let name = format!("{}{}", finder.shown_path(f), if file.is_dir { "/" } else { "" });
                let count = if file.hits.is_empty() { String::new() } else { format!(" {}", file.hits.len()) };
                let style = if on_cursor { cursor_style() } else { base.fg(Color::LightCyan).add_modifier(Modifier::BOLD) };
                let name_w = width.saturating_sub(util::width(&count));
                Line::from(vec![Span::styled(util::fit_left(&name, name_w), style), Span::styled(count, style)])
            }
            Some(Row::Hit(f, h)) => {
                let hit = &finder.files[f].hits[h];
                let style = if on_cursor { cursor_style() } else { base };
                let number = format!("{:>6}: ", hit.number);
                let mut spans = vec![Span::styled(number.clone(), if on_cursor { style } else { grey })];
                spans.extend(marked_spans(&hit.text, &hit.ranges, width.saturating_sub(number.len()), style, mark));
                Line::from(spans)
            }
            None => Line::default(),
        };
        // The cursor bar spans the whole row; the spans (and their marks) go on top of it.
        let row_style = if on_cursor && line.width() > 0 { cursor_style() } else { base };
        buf.set_string(list.x, y, " ".repeat(width), row_style);
        buf.set_line(list.x, y, &line, list.width);
    }

    // The status line.
    let status = find_status(finder);
    buf.set_string(inner.x, inner.bottom() - 1, util::pad(&util::fit(&status, width), width), base.fg(Color::Yellow));
    (cursor, (list, finder.offset))
}

/// "42 matches in 7 files · 1,203 files searched · 0.21 s", and what is unusual about it.
fn find_status(finder: &Finder) -> String {
    let query = finder.query();
    if query.text.is_empty() && query.names.trim().is_empty() {
        return "Type text to find in files, or names in Files (*.rs, !*test*, or part of a name).".into();
    }
    let files = finder.files.len();
    let mut status = if query.text.is_empty() {
        format!("{files} found · {} names looked at", finder.searched)
    } else {
        let hits = finder.total_hits();
        format!("{hits} match{} in {files} file{} · {} files searched", if hits == 1 { "" } else { "es" }, if files == 1 { "" } else { "s" }, finder.searched)
    };
    match finder.took {
        Some(took) => status.push_str(&format!(" · {:.2} s", took.as_secs_f64())),
        None if finder.working() => status.push_str(" · searching…"),
        None => {}
    }
    if finder.limited {
        status.push_str(&format!(" · stopped at {}", crate::find::MAX_RESULTS));
    }
    if finder.literal {
        status.push_str(" · not a valid regex, searched as typed");
    }
    status
}

/// `text` cut to `width` columns, as spans with the byte `ranges` in the `mark` style.
fn marked_spans(text: &str, ranges: &[(usize, usize)], width: usize, style: Style, mark: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0;
    let mut current = String::new();
    let mut current_marked = false;
    for (i, c) in text.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        let marked = ranges.iter().any(|&(start, end)| i >= start && i < end);
        if marked != current_marked && !current.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut current), if current_marked { mark } else { style }));
        }
        current_marked = marked;
        current.push(c);
    }
    if !current.is_empty() {
        spans.push(Span::styled(current, if current_marked { mark } else { style }));
    }
    spans
}

/// Draws one panel. Returns the cursor position while quick search is typing into it,
/// and where the panel's parts are for the mouse.
fn draw_panel(
    buf: &mut Buffer,
    area: Rect,
    panel: &mut Panel,
    active: bool,
    search: Option<&str>,
    git: &Tracker,
) -> (Option<Position>, PanelHit) {
    let base = panel_style();
    let inner_w = area.width.saturating_sub(2) as usize;
    // The filter (Ctrl+F) after the path, so hidden files are never a mystery.
    let filter = panel.filter.as_deref().map(|f| format!("[{f}] ")).unwrap_or_default();
    // In an archive, the path through it (E:\x\photos.zip\2024).
    let path = util::fit_left(&panel.location().display().to_string(), inner_w.saturating_sub(4 + util::width(&filter)));
    let title_style = if active { cursor_style() } else { base };
    let mut title = vec![Span::styled(format!(" {path} "), title_style)];
    if !filter.is_empty() {
        title.push(Span::styled(filter, base.fg(Color::Yellow)));
    }
    let mut block = Block::bordered().border_style(base).style(base).title(Line::from(title));
    // Free space on the bottom line.
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

    // In a repository the row below the path is git's, while the list keeps 3 rows or more.
    let repo = panel.repo.clone();
    let status = repo.as_ref().and_then(|repo| git.status(&repo.root));
    let git_row = repo.as_ref().filter(|_| inner.height >= 7);
    if let Some(repo) = git_row {
        buf.set_line(inner.x, inner.y, &git_line(repo, status, inner_w), inner.width);
    }
    // The column headings' row, the list below it.
    let top = inner.y + u16::from(git_row.is_some());
    let list_h = (inner.height - 3 - u16::from(git_row.is_some())) as usize;
    panel.ensure_visible(list_h);
    let details = inner.width >= 34;
    let (size_w, time_w) = (7usize, 12usize);
    let name_w = if details { inner_w - size_w - time_w - 2 } else { inner_w };
    // In a repository the name column's last place holds each file's git mark.
    let mark_w = usize::from(repo.is_some() && git.enabled() && name_w > 8);
    let size_x = inner.x + name_w as u16 + 1;
    let time_x = size_x + size_w as u16 + 1;
    let hit = PanelHit {
        area,
        list: Rect::new(inner.x, top + 1, inner.width, list_h as u16),
        header_y: top,
        size_x,
        time_x,
        details,
    };

    // Column headings, with the sort indicator in the corner like mc, and an arrow on the
    // sorted column: ↑ for A to Z (small to big, old to new), ↓ the other way.
    let heading = base.fg(Color::Yellow).add_modifier(Modifier::BOLD);
    buf.set_string(inner.x, top, panel.sort.indicator(), base);
    let arrow = if panel.sort.descending(panel.reverse) { "↓" } else { "↑" };
    let title = |text: &str, sorted: bool| if sorted { format!("{text}{arrow}") } else { text.to_string() };
    let by_name = matches!(panel.sort, SortBy::Name | SortBy::Extension);
    buf.set_string(inner.x + 2, top, util::center(&title("Name", by_name), name_w.saturating_sub(2)), heading);
    if details {
        buf.set_string(size_x - 1, top, "│", base);
        buf.set_string(size_x, top, util::center(&title("Size", panel.sort == SortBy::Size), size_w), heading);
        buf.set_string(time_x - 1, top, "│", base);
        let time = title("Modify time", panel.sort == SortBy::Time);
        buf.set_string(time_x, top, util::center(&time, time_w), heading);
    }

    let now = SystemTime::now();
    for row in 0..list_h {
        let y = top + 1 + row as u16;
        let index = panel.offset + row;
        let entry = panel.entries.get(index);
        let marked = entry.is_some_and(|e| panel.is_marked(e));
        let on_cursor = active && index == panel.cursor && entry.is_some();
        let git_mark = match (&repo, status, entry) {
            (Some(repo), Some(status), Some(e)) if !e.is_parent => repo.relative(&e.path).and_then(|path| status.mark(&path)),
            _ => None,
        };
        let ignored = matches!(git_mark, Some((Mark::Ignored, _)));
        let style = entry_style(entry, marked, on_cursor, ignored);
        let (name, size, time) = match entry {
            Some(e) => (
                format!("{}{}", prefix(e), e.name),
                size_text(panel, e),
                e.modified.map(|t| util::format_time(t, now)).unwrap_or_default(),
            ),
            None => Default::default(),
        };
        buf.set_string(inner.x, y, util::fit(&name, name_w - mark_w), style);
        if mark_w == 1 {
            let (symbol, mark_style) = match git_mark {
                Some((mark, inside)) if mark != Mark::Ignored => git_symbol(mark, inside),
                _ => (" ", base),
            };
            let mark_style = if on_cursor { style } else { mark_style };
            buf.set_string(inner.x + (name_w - 1) as u16, y, symbol, mark_style);
        }
        if details {
            let separator = if on_cursor { style } else { base };
            buf.set_string(size_x - 1, y, "│", separator);
            buf.set_string(size_x, y, util::pad_left(&size, size_w), style);
            buf.set_string(time_x - 1, y, "│", separator);
            buf.set_string(time_x, y, util::fit(&time, time_w), style);
        }
    }

    // Separator line joined to the frame, with the marked-files summary on it.
    let sep_y = top + 1 + list_h as u16;
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
        // Where a link or a junction leads, as mc shows it.
        Some(e) if e.is_link => match std::fs::read_link(&e.path) {
            Ok(target) => {
                let target = target.display().to_string();
                format!("{} -> {}", e.name, target.strip_prefix(r"\\?\").unwrap_or(&target))
            }
            Err(_) => e.name.clone(),
        },
        Some(e) => e.name.clone(),
        None => String::new(),
    };
    buf.set_string(inner.x, status_y, util::fit(&status, inner_w), base);
    (None, hit)
}

/// A side's tabs, `1:photos 2:Downloads …`, the shown one lit up. Returns each tab's place.
fn draw_tabs(buf: &mut Buffer, area: Rect, names: &[String], shown: usize, active: bool) -> Vec<(Rect, usize)> {
    let mut places = Vec::new();
    let mut x = area.x;
    buf.set_string(area.x, area.y, " ".repeat(area.width as usize), Style::new().bg(Color::Black));
    for (i, name) in names.iter().enumerate() {
        let label = format!(" {}:{} ", i + 1, util::take_width(name, 18));
        let width = util::width(&label) as u16;
        if x + width > area.right() {
            break;
        }
        let style = match (i == shown, active) {
            (true, true) => cursor_style(),
            (true, false) => panel_style().fg(Color::White),
            (false, _) => Style::new().fg(Color::Gray).bg(Color::DarkGray),
        };
        buf.set_string(x, area.y, &label, style);
        places.push((Rect::new(x, area.y, width, 1), i));
        x += width + 1;
    }
    places
}

/// Ctrl+Q: the other panel shows what is under the cursor. Returns the area of its text.
fn draw_quick_view(buf: &mut Buffer, area: Rect, preview: Option<&Preview>, focused: bool) -> Rect {
    let base = panel_style();
    let grey = base.fg(Color::DarkGray);
    let inner_w = area.width.saturating_sub(2) as usize;
    let name = preview.and_then(|p| p.key.path.file_name()).map(|n| n.to_string_lossy().into_owned());
    let title = format!(" Quick view{} ", name.map(|n| format!(": {n}")).unwrap_or_default());
    let title = util::take_width(&title, inner_w.saturating_sub(2)).to_string();
    let mut block = Block::bordered()
        .border_style(base)
        .style(base)
        .title(Line::from(Span::styled(title, if focused { cursor_style() } else { base })));
    let info = preview.map(|p| preview_info(p)).unwrap_or_default();
    if !info.is_empty() && util::width(&info) + 4 < inner_w {
        block = block.title_bottom(Line::from(format!(" {info} ")).right_aligned());
    }
    let inner = block.inner(area);
    block.render(area, buf);
    let Some(preview) = preview else {
        buf.set_string(inner.x, inner.y, util::take_width("Nothing to show here.", inner.width as usize), grey);
        return inner;
    };
    let width = inner.width as usize;
    let mut row = 0u16;
    let mut put = |text: &str, style: Style| {
        if row < inner.height {
            buf.set_string(inner.x, inner.y + row, util::take_width(text, width), style);
            row += 1;
        }
    };
    let rows = inner.height as usize;
    match &preview.content {
        Content::Loading => put("Reading…", grey),
        Content::Message(text) => put(text, grey),
        Content::Text { lines, .. } | Content::Binary { lines, .. } => {
            for line in lines.iter().skip(preview.scroll).take(rows) {
                put(line, base);
            }
        }
        Content::Picture { format, width: w, height: h } => {
            put(&format!("{format} picture"), base.fg(Color::White));
            let megapixels = f64::from(*w) * f64::from(*h) / 1_000_000.0;
            put(&format!("{w} × {h} pixels ({megapixels:.1} megapixels)"), base);
        }
        Content::Folder { folders, files, bytes, names, partial } => {
            let more = if *partial { "+" } else { "" };
            put(
                &format!(
                    "{}{more} folders, {}{more} files ({} in the files)",
                    util::thousands(*folders as u64),
                    util::thousands(*files as u64),
                    util::human_size(*bytes)
                ),
                base.fg(Color::White),
            );
            for (name, is_dir) in names.iter().skip(preview.scroll).take(rows.saturating_sub(1)) {
                let style = if *is_dir { base.fg(Color::White) } else { base };
                put(&format!("{}{name}", if *is_dir { "/" } else { " " }), style);
            }
        }
    }
    inner
}

/// The quick view's bottom line: what the file is.
fn preview_info(preview: &Preview) -> String {
    let size = util::human_size(preview.key.size);
    match &preview.content {
        Content::Text { encoding, partial, .. } => {
            format!("{encoding} · {size}{}", if *partial { " · the start" } else { "" })
        }
        Content::Binary { partial, .. } => format!("binary · {size}{}", if *partial { " · the start" } else { "" }),
        Content::Picture { .. } => size,
        _ => String::new(),
    }
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

/// The mark beside a name in a repository: what happened to the file (M changed, A added, R
/// renamed, ? untracked, ! conflict), or `•` for a folder with changes inside; in its colour.
fn git_symbol(mark: Mark, inside: bool) -> (&'static str, Style) {
    let base = panel_style();
    let style = match mark {
        Mark::Conflict => base.fg(Color::LightRed).add_modifier(Modifier::BOLD),
        Mark::Modified => base.fg(Color::Yellow),
        Mark::Added | Mark::Renamed | Mark::Untracked => base.fg(Color::LightGreen),
        Mark::Ignored => base.fg(Color::DarkGray),
    };
    let symbol = match mark {
        _ if inside => "•",
        Mark::Conflict => "!",
        Mark::Modified => "M",
        Mark::Added => "A",
        Mark::Renamed => "R",
        Mark::Untracked => "?",
        Mark::Ignored => " ",
    };
    (symbol, style)
}

/// The branch: its name, `(1a2b3c4)` for a commit, or what git status said when HEAD can't be
/// read from the files.
fn branch_name(repo: &Repo, status: Option<&Status>) -> String {
    match &repo.head {
        Head::Unknown => status.and_then(|s| s.branch.clone()).unwrap_or_else(|| "git".into()),
        head => head.label(),
    }
}

/// The git row below a panel's path, in as many words as fit in `room` columns: the branch
/// (cyan in a linked worktree), an operation under way, commits ahead/behind, "worktree", and
/// how many files are in conflict, changed or untracked. ` main ↑1 · 3 changed · 1 untracked`,
/// then the counts as signs (` main ↑1 ~3 ?1`), without them, and the branch cut short last.
fn git_line(repo: &Repo, status: Option<&Status>, room: usize) -> Line<'static> {
    let base = panel_style();
    let colour = if repo.linked { Color::LightCyan } else { Color::White };
    let branch = branch_name(repo, status);
    let name = |text: String| vec![Span::styled(" ", base), Span::styled(text, base.fg(colour).add_modifier(Modifier::BOLD))];
    let mut bare = name(branch.clone());
    if let Some(state) = &repo.state {
        bare.push(Span::styled(format!(" {state}"), base.fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    }
    let mut head = bare.clone();
    if let Some((ahead, behind)) = status.and_then(|s| s.ahead_behind) {
        let arrows: String =
            [(ahead, "↑"), (behind, "↓")].into_iter().filter(|(n, _)| *n > 0).map(|(n, arrow)| format!("{arrow}{n}")).collect();
        if !arrows.is_empty() {
            head.push(Span::styled(format!(" {arrows}"), base.fg(Color::White)));
        }
    }
    let mut worktree = head.clone();
    if repo.linked {
        worktree.extend([Span::styled(" ·", base), Span::styled(" worktree", base.fg(Color::LightCyan))]);
    }
    // How many, in words, as a sign, and in what colour; conflicts first, as they need you.
    let counts: Vec<(usize, String, &str, Style)> = status.map_or_else(Vec::new, |s| {
        let plural = if s.conflicts == 1 { "" } else { "s" };
        let counts = [
            (s.conflicts, format!("{} conflict{plural}", s.conflicts), "!", base.fg(Color::LightRed).add_modifier(Modifier::BOLD)),
            (s.changed, format!("{} changed", s.changed), "~", base.fg(Color::Yellow)),
            (s.untracked, format!("{} untracked", s.untracked), "?", base.fg(Color::LightGreen)),
        ];
        counts.into_iter().filter(|(n, ..)| *n > 0).collect()
    });
    let mut long = worktree.clone();
    for (_, words, _, style) in &counts {
        long.extend([Span::styled(" ·", base), Span::styled(format!(" {words}"), *style)]);
    }
    let signs: Vec<Span<'static>> = counts.iter().map(|(n, _, sign, style)| Span::styled(format!(" {sign}{n}"), *style)).collect();
    let medium = [worktree, signs.clone()].concat();
    let short = [head, signs].concat();
    [long, medium, short, bare, name(branch.clone())]
        .into_iter()
        .map(Line::from)
        .find(|line| line.width() <= room)
        // A space, at least one letter and "…".
        .unwrap_or_else(|| if room >= 3 { Line::from(name(format!("{}…", util::take_width(&branch, room - 2)))) } else { Line::default() })
}

fn entry_style(entry: Option<&Entry>, marked: bool, on_cursor: bool, ignored: bool) -> Style {
    let fg = match entry {
        _ if marked => Color::Yellow,
        // Ignored by git (target, .venv): dimmed, as editors show them.
        _ if ignored => Color::DarkGray,
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
    let room = (area.width as usize).saturating_sub(prompt_w + 1);
    let (text, cursor) = app.cmdline.view(room);
    let mut spans = vec![Span::raw(prompt), Span::raw(" "), Span::raw(text)];
    // A short message from the last action, until the next key.
    if app.cmdline.is_empty()
        && let Some(notice) = &app.notice
    {
        spans.push(Span::styled(util::take_width(notice, room).to_string(), Style::new().fg(Color::Yellow)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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

/// The help box's height, border included.
fn help_height(area: Rect) -> u16 {
    (dialog::HELP.len() as u16 + 2).min(area.height.saturating_sub(2))
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
    // At least 34 wide, but never wider than the screen (clamp would panic on a narrow one).
    let width = (widest as u16 + 4).max(34).min(area.width.saturating_sub(4).max(10));
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
fn draw_dialog(
    frame: &mut Frame,
    area: Rect,
    dialog: &Dialog,
    hotlist: &Hotlist,
    settings_file: Option<&std::path::Path>,
) -> (Option<Position>, DialogHit) {
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
        Dialog::Delete { paths, to_trash, focus, survey } => {
            let tally = survey.tally();
            let (title, lines) = crate::app::delete_question(paths, *to_trash, &tally);
            (None, draw_message(frame, area, title, &lines, dialog::delete_buttons(&tally), *focus, !*to_trash))
        }
        Dialog::Options { cursor, edit, settings, automatic } => {
            draw_options(frame, area, *cursor, edit, settings, automatic, settings_file)
        }
        Dialog::Hotlist { cursor } => (None, draw_hotlist(frame, area, hotlist, *cursor)),
        // Drawn by `draw`, which has the rows.
        Dialog::History { .. } | Dialog::Commands { .. } | Dialog::Worktrees { .. } => (None, DialogHit::default()),
        Dialog::Attributes { paths, states, cursor } => (None, draw_attributes(frame, area, paths, states, *cursor)),
        Dialog::Help { scroll } => {
            let height = help_height(area);
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

/// The options editor: checkboxes and text rows; an empty command shows what it stands for
/// in grey. Returns the cursor position when a text row is being edited.
fn draw_options(
    frame: &mut Frame,
    area: Rect,
    cursor: usize,
    edit: &LineEdit,
    settings: &Settings,
    automatic: &OptionsAutomatic,
    settings_file: Option<&std::path::Path>,
) -> (Option<Position>, DialogHit) {
    const LABEL_W: usize = 22;
    let rows = options::ROWS.len() as u16;
    let width = (area.width * 2 / 3).clamp(60, 90).min(area.width);
    let (outer, inner) = dialog_frame(frame, area, width, rows + 5, "Options", false);
    let style = dialog_style(false);
    let grey = style.fg(Color::DarkGray);
    let field_w = (inner.width as usize).saturating_sub(LABEL_W);
    let mut text_cursor = None;
    let mut lines: Vec<Line> = Vec::new();
    for (i, &row) in options::ROWS.iter().enumerate() {
        let focused = i == cursor;
        let y = inner.y + i as u16;
        if row.is_flag() {
            let mark = if row.flag(settings) { "[x]" } else { "[ ]" };
            let text = util::pad(&format!("{mark} {}", row.label()), inner.width as usize);
            lines.push(Line::from(Span::styled(text, if focused { cursor_style() } else { style })));
            continue;
        }
        let label = Span::styled(util::pad(row.label(), LABEL_W), style);
        let value = if focused {
            let (text, column) = edit.view(field_w.saturating_sub(1));
            text_cursor = Some(Position::new(inner.x + (LABEL_W + column) as u16, y));
            Span::styled(util::pad(&text, field_w), cursor_style())
        } else {
            let text = row.text(settings);
            match automatic.for_row(row) {
                Some(auto) if text.is_empty() => Span::styled(util::fit(&format!("{auto} (automatic)"), field_w), grey),
                _ => Span::styled(util::fit(&text, field_w), style),
            }
        };
        lines.push(Line::from(vec![label, value]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("Space toggles · type to change (empty: automatic) · Esc", grey)));
    if let Some(file) = settings_file {
        let text = format!("Saved in {}", file.display());
        lines.push(Line::from(Span::styled(util::fit_left(&text, inner.width as usize), grey)));
    }
    frame.render_widget(Paragraph::new(lines).style(style), inner);
    let list = Rect::new(inner.x, inner.y, inner.width, rows.min(inner.height));
    (text_cursor, DialogHit { area: outer, buttons: Vec::new(), rows: Some((list, 0)) })
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

/// Alt+A: a checkbox per attribute, [?] where the files differ.
fn draw_attributes(frame: &mut Frame, area: Rect, paths: &[std::path::PathBuf], states: &[Option<bool>], cursor: usize) -> DialogHit {
    let rows = states.len() as u16;
    let (outer, inner) = dialog_frame(frame, area, 52, rows + 6, "Attributes", false);
    let style = dialog_style(false);
    let what = match paths {
        [one] => one.file_name().map_or_else(|| one.display().to_string(), |n| n.to_string_lossy().into_owned()),
        many => format!("{} items", many.len()),
    };
    let mut lines = vec![Line::from(util::fit_left(&what, inner.width as usize)), Line::default()];
    for (i, ((label, _), state)) in crate::platform::ATTRIBUTES.iter().zip(states).enumerate() {
        let mark = match state {
            Some(true) => "[x]",
            Some(false) => "[ ]",
            None => "[?]",
        };
        let row_style = if i == cursor { cursor_style() } else { style };
        lines.push(Line::from(Span::styled(util::pad(&format!("{mark} {label}"), inner.width as usize), row_style)));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("Space toggles · Enter sets them · Esc", style.fg(Color::DarkGray))).right_aligned());
    frame.render_widget(Paragraph::new(lines).style(style), inner);
    let list = Rect::new(inner.x, inner.y + 2, inner.width, rows.min(inner.height.saturating_sub(2)));
    DialogHit { area: outer, buttons: Vec::new(), rows: Some((list, 0)) }
}

const HISTORY_HINT: &str = "type to narrow · Enter goes there · Del forgets · Esc";
const COMMANDS_HINT: &str = "type to narrow · Enter runs it · Esc";
const WORKTREES_HINT: &str = "type to narrow · Enter goes there (same subfolder) · Esc";

/// The Alt+W list's rows: `*` on the one the panel is in, the branch, then the folder (which
/// gives way from the left when the box is narrow) and what is unusual about it.
fn worktree_rows(worktrees: &[&Worktree], here: &Path) -> Vec<String> {
    let label = |w: &Worktree| match &w.state {
        Some(state) => format!("{} {state}", w.head.label()),
        None => w.head.label(),
    };
    let branch_w = worktrees.iter().map(|w| util::width(&label(w))).max().unwrap_or(0).min(40);
    worktrees
        .iter()
        .map(|w| {
            let mark = if util::same_path(&w.path, here) { "*" } else { " " };
            let mut notes = String::new();
            for (yes, note) in [(w.main, "main"), (w.locked, "locked"), (w.missing, "missing")] {
                if yes {
                    notes.push_str(&format!("  ({note})"));
                }
            }
            let branch = util::pad(util::take_width(&label(w), 40), branch_w);
            format!("{mark} {branch}  \t{}{notes}", w.path.display())
        })
        .collect()
}

/// A list narrowed by typing (Alt+H's folders, F12's commands): the filter line, then the
/// rows. Returns the text cursor and where the rows are, for the mouse.
fn draw_filtered_list(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    rows: &[String],
    filter: &LineEdit,
    cursor: usize,
    hint: &str,
) -> (Position, DialogHit) {
    let height = (rows.len() as u16 + 5).clamp(8, area.height.saturating_sub(2).max(8));
    let widest = rows.iter().map(|r| util::width(&r.replace('\t', ""))).max().unwrap_or(0) as u16 + 4;
    let width = (area.width * 2 / 3).clamp(50, 100).max(widest.min(area.width)).min(area.width);
    let (outer, inner) = dialog_frame(frame, area, width, height, title, false);
    let style = dialog_style(false);
    let grey = style.fg(Color::DarkGray);
    let field_w = inner.width as usize;
    const LABEL: &str = "Filter: ";
    let (text, column) = filter.view(field_w.saturating_sub(LABEL.len() + 1));
    let mut lines =
        vec![Line::from(vec![Span::styled(LABEL, style), Span::styled(util::pad(&text, field_w.saturating_sub(LABEL.len())), cursor_style())])];
    let list_h = inner.height.saturating_sub(3) as usize;
    let cursor = cursor.min(rows.len().saturating_sub(1));
    let offset = (cursor + 1).saturating_sub(list_h);
    if rows.is_empty() {
        let empty = if filter.text().is_empty() { "Nothing here yet." } else { "Nothing matches." };
        lines.push(Line::from(Span::styled(empty, grey)));
    }
    for (i, row) in rows.iter().enumerate().skip(offset).take(list_h) {
        // A row "left\tright" keeps its left part whole and shortens the right one from the left.
        let text = match row.split_once('\t') {
            Some((left, right)) => format!("{left}{}", util::fit_left(right, field_w.saturating_sub(util::width(left)))),
            None => util::fit_left(row, field_w),
        };
        let row_style = if i == cursor { cursor_style() } else { style };
        lines.push(Line::from(Span::styled(util::pad(&text, field_w), row_style)));
    }
    lines.resize(list_h + 2, Line::default());
    lines.push(Line::from(Span::styled(util::fit_left(hint, field_w), grey)).right_aligned());
    frame.render_widget(Paragraph::new(lines).style(style), inner);
    let shown = rows.len().saturating_sub(offset).min(list_h) as u16;
    let rows_area = (shown > 0).then(|| (Rect::new(inner.x, inner.y + 1, inner.width, shown), offset));
    let position = Position::new(inner.x + (LABEL.len() + column) as u16, inner.y);
    (position, DialogHit { area: outer, buttons: Vec::new(), rows: rows_area })
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
    if let Some((speed, left)) = job.speed() {
        let secs = left.as_secs();
        let left = if secs >= 3600 {
            format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
        } else {
            format!("{}:{:02}", secs / 60, secs % 60)
        };
        let text = format!("{}/s   {left} left", util::human_size(speed as u64));
        frame.render_widget(Paragraph::new(text).style(style), row(4));
    }
    frame.render_widget(Paragraph::new(Line::from("Esc = cancel").right_aligned()).style(style.fg(Color::DarkGray)), row(5));
}

#[cfg(test)]
mod tests {
    use super::*;
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
        App::new(tmp.path().to_path_buf(), tmp.path().to_path_buf(), crate::testutil::config())
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

    /// In a repository: the branch and the counts on the bottom line (fewer words when
    /// narrow), a mark beside each changed or new name, folders with changes inside marked
    /// `•`, ignored ones dimmed; and the Alt+W list.
    #[test]
    fn renders_the_branch_and_git_marks() {
        use crate::git::{Repo, Status, tests::{fake_repo, fake_worktree}};
        let tmp = TempDir::new("ui-git");
        let root = tmp.dir("app");
        fake_repo(&root, "main");
        tmp.file("app/changed.txt", "x");
        tmp.file("app/new.txt", "x");
        tmp.file("app/src/lib.rs", "x");
        tmp.file("app/target/app.exe", "x");
        // The right panel in a linked worktree.
        let other = tmp.path().join("app-login");
        fake_worktree(&root, "app-login", &other, "login");
        // Git status on, but never run here (no ticks): the status is given.
        let settings = crate::settings::Settings { git_status: true, ..crate::settings::Settings::default() };
        let mut app = App::new(root.clone(), other.clone(), crate::app::Config { settings, ..crate::testutil::config() });
        let output = [
            "# branch.head main",
            "# branch.ab +1 -0",
            "1 .M N... 100644 100644 100644 a a changed.txt",
            "1 .M N... 100644 100644 100644 a a src/lib.rs",
            "? new.txt",
            "! target/",
            "",
        ]
        .join("\0");
        app.git.set_status(&root, Status::parse(output.as_bytes()));

        let mut terminal = Terminal::new(TestBackend::new(140, 16)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let lines: Vec<String> =
            (0..16).map(|y| (0..140).map(|x| buffer[(x, y)].symbol()).collect::<String>()).collect();
        let screen = lines.join("\n");
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        // Git's row below the path: the branch and how the working tree stands; a linked
        // worktree's branch in cyan, and saying so (its status isn't known here). (Columns
        // counted in characters: the lines hold │ and ─.)
        let left = |y: usize| lines[y].chars().take(70).collect::<String>();
        let right = |y: usize| lines[y].chars().skip(70).collect::<String>();
        let column = |y: usize, text: &str| lines[y][..lines[y].find(text).unwrap()].chars().count() as u16;
        assert!(left(1).starts_with("│ main ↑1 · 2 changed · 1 untracked  "), "{screen}");
        assert!(right(1).starts_with("│ login · worktree  "), "{screen}");
        assert_eq!(buffer[(column(1, "main"), 1)].fg, Color::White);
        assert_eq!(buffer[(column(1, "login"), 1)].fg, Color::LightCyan);
        assert_eq!(buffer[(column(1, "worktree"), 1)].fg, Color::LightCyan);
        // The headings below it; the path alone on the top line, the free space alone on the
        // bottom one.
        assert!(left(2).contains("Name↑") && right(2).contains("Name↑"), "{screen}");
        assert!(!lines[0].contains(" main") && !lines[0].contains("worktree"), "{screen}");
        let bottom = lines.iter().position(|l| l.starts_with('└')).unwrap();
        assert!(left(bottom).starts_with("└──") && right(bottom).starts_with("└──"), "{screen}");
        let row = |name: &str| lines.iter().position(|l| l.contains(name)).unwrap_or_else(|| panic!("no {name}: {screen}"));
        assert!(left(row(" changed.txt")).contains("M│"), "{screen}");
        assert!(left(row(" new.txt")).contains("?│"), "{screen}");
        assert!(left(row("/src")).contains("•│"), "{screen}");
        assert!(!left(row("/target")).contains("•") && left(row("/target")).contains(" │"));
        // Ignored: dimmed.
        let target = row("/target");
        assert_eq!(buffer[(column(target, "/target") + 1, target as u16)].fg, Color::DarkGray);
        let new = row(" new.txt");
        assert_eq!(buffer[(column(new, "new.txt"), new as u16)].fg, Color::Gray);

        // Narrow: the counts as signs.
        let screen = render(&mut app, 60, 16);
        assert!(screen.contains("│ main ↑1 ~2 ?1 "), "{screen}");
        // Too short a panel for the row: the list keeps its rows.
        let screen = render(&mut app, 140, 9);
        assert!(!screen.contains("main ↑1") && screen.lines().nth(1).unwrap().contains("Name↑"), "{screen}");

        // Alt+W's list: the main one marked as where the panel is.
        let worktrees = Repo::find(&root).unwrap().worktrees();
        app.dialog = Some(Dialog::Worktrees { filter: LineEdit::default(), cursor: 1, worktrees, here: root.clone() });
        let screen = render(&mut app, 140, 16);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Git worktrees"), "{screen}");
        assert!(screen.contains(&format!("* main   {}  (main)", root.display())), "{screen}");
        assert!(screen.contains(&format!("  login  {}", other.display())), "{screen}");
    }

    /// Git's row gives way a part at a time: the counts turn to signs, then "worktree" goes,
    /// then the signs and ↑↓, then the operation under way, and last the end of the branch.
    #[test]
    fn the_git_row_gives_way_a_part_at_a_time() {
        use crate::git::{Head, Repo, Status};
        let repo = Repo {
            root: PathBuf::from("app"),
            git_dir: PathBuf::from("app/.git"),
            common_dir: PathBuf::from("app/.git"),
            linked: true,
            head: Head::Branch("claude/gentle-fox".into()),
            state: Some("REBASE 2/5".into()),
        };
        let output = ["# branch.ab +1 -0", "1 .M N... 100644 100644 100644 a a changed.txt", "? new.txt", ""].join("\0");
        let status = Status::parse(output.as_bytes());
        let text = |repo: &Repo, room| git_line(repo, Some(&status), room).spans.iter().map(|s| s.content.to_string()).collect::<String>();
        let long = " claude/gentle-fox REBASE 2/5 ↑1 · worktree · 1 changed · 1 untracked";
        assert_eq!(text(&repo, 100), long);
        assert_eq!(text(&repo, util::width(long)), long);
        assert_eq!(text(&repo, 60), " claude/gentle-fox REBASE 2/5 ↑1 · worktree ~1 ?1");
        assert_eq!(text(&repo, 45), " claude/gentle-fox REBASE 2/5 ↑1 ~1 ?1");
        assert_eq!(text(&repo, 35), " claude/gentle-fox REBASE 2/5");
        assert_eq!(text(&repo, 20), " claude/gentle-fox");
        assert_eq!(text(&repo, 8), " claude…");
        assert_eq!(text(&repo, 2), "");
        // A commit checked out; nothing to count.
        let detached = Repo { head: Head::Detached("1a2b3c4".into()), linked: false, state: None, ..repo };
        assert_eq!(git_line(&detached, None, 100).spans.iter().map(|s| s.content.as_ref()).collect::<String>(), " (1a2b3c4)");
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

    /// F8 asks at once and fills in the counts; Windows' own files get "As admin". Only F8
    /// and Esc are pressed: nothing is deleted.
    #[test]
    fn renders_the_delete_question_with_its_survey() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use std::time::{Duration, Instant};
        let wait = |app: &mut App| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while app.working() {
                assert!(Instant::now() < deadline, "survey did not finish");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let tmp = TempDir::new("ui-delete");
        tmp.file("old/a.txt", "12345");
        tmp.file("old/sub/b.txt", "123");
        let mut app = app_for(&tmp);
        assert!(app.panels[0].select_name("old"));
        app.on_key(KeyEvent::from(KeyCode::F(8)));
        wait(&mut app);
        let screen = render(&mut app, 100, 24);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Move \"old\" to"), "{screen}");
        assert!(screen.contains("2 files in 2 folders, 8B."), "{screen}");
        assert!(!screen.contains("As admin"));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.dialog.is_none());
        assert!(tmp.path().join("old").exists());

        #[cfg(windows)]
        {
            let mut app = App::new(PathBuf::from(r"C:\Windows\System32"), tmp.path().to_path_buf(), crate::testutil::config());
            assert!(app.panels[0].select_name("drivers"));
            app.on_key(KeyEvent::from(KeyCode::F(8)));
            wait(&mut app);
            let screen = render(&mut app, 120, 30);
            if std::env::var_os("RMC_PRINT").is_some() {
                println!("{screen}");
            }
            app.on_key(KeyEvent::from(KeyCode::Esc));
            assert!(app.dialog.is_none());
            // Without elevation Windows' own folders need an administrator.
            if screen.contains("Needs administrator rights") {
                assert!(screen.contains("TrustedInstaller"), "{screen}");
                assert!(screen.contains("[ As admin ]"), "{screen}");
            }
        }
    }

    #[test]
    fn renders_the_find_window() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::time::{Duration, Instant};
        let tmp = TempDir::new("ui-find");
        tmp.file("src/main.rs", "fn main() {\n    let total = 40 + 2;\n    println!(\"{total}\");\n}\n");
        tmp.file("README.md", "The total is shown.\n");
        let mut app = app_for(&tmp);
        app.on_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::ALT));
        for c in "total".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.finder.as_ref().unwrap().working() {
            assert!(Instant::now() < deadline, "search did not finish");
            std::thread::sleep(Duration::from_millis(10));
            app.tick();
        }
        app.on_key(KeyEvent::from(KeyCode::Down));
        let screen = render(&mut app, 100, 20);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Find in"));
        assert!(screen.contains("Text  total"));
        assert!(screen.contains("README.md 1"), "{screen}");
        assert!(screen.contains("     2:     let total = 40 + 2;"), "{screen}");
        assert!(screen.contains("3 matches in 2 files"), "{screen}");
    }

    #[test]
    fn renders_the_options_editor() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let tmp = TempDir::new("ui-options");
        let mut app = app_for(&tmp);
        app.config.settings_file = Some(PathBuf::from("settings.ini"));
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        let screen = render(&mut app, 100, 30);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Options"));
        assert!(screen.contains("[ ] Ask before quitting (F10)"), "{screen}");
        assert!(screen.contains("[x] Show hidden files"));
        assert!(screen.contains("Big file limit, MB"));
        assert!(screen.contains("(automatic)"), "empty commands say what they stand for");
        assert!(screen.contains("Saved in settings.ini"));
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
        // Tall enough for all of it.
        let help = render(&mut app, 80, 90);
        assert!(help.contains("rmc keys"));
        // The longest help lines fit whole.
        assert!(help.contains("Enter runs it in the shell, in the panel's folder"), "{help}");
        assert!(help.contains("rmc --no-mouse leaves the mouse alone"), "{help}");
        app.on_key(ratatui::crossterm::event::KeyEvent::from(ratatui::crossterm::event::KeyCode::Esc));
        app.on_key(ratatui::crossterm::event::KeyEvent::from(ratatui::crossterm::event::KeyCode::F(7)));
        let mkdir = render(&mut app, 80, 24);
        assert!(mkdir.contains("New directory name:"));
        // Tiny terminals must not panic, with or without a dialog (a message box wanted 34
        // columns; five rows left no room for the key bar).
        for (w, h) in [(10, 3), (20, 6), (40, 8), (30, 5), (37, 24), (100, 5), (12, 1), (0, 0)] {
            render(&mut app, w, h);
        }
        app.dialog = Some(Dialog::Message { title: "Error".into(), lines: vec!["Cannot open \"x\"".into()], error: true });
        for (w, h) in [(30, 24), (37, 12), (20, 6), (1, 40)] {
            render(&mut app, w, h);
        }
        assert!(render(&mut app, 18, 4).contains("rmc: the window"));
    }

    /// Ctrl+Q shows the file under the cursor where the other panel was, and follows the cursor.
    #[test]
    fn quick_view_shows_the_file_under_the_cursor() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let tmp = TempDir::new("ui-quick");
        tmp.file("a.txt", "první řádek\ndruhý řádek\n");
        tmp.dir("photos");
        tmp.file("photos/x.jpg", "x");
        let mut app = app_for(&tmp);
        let settle = |app: &mut App| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                app.tick();
                if !app.previewer.as_ref().is_some_and(crate::preview::Previewer::loading) {
                    break;
                }
                assert!(std::time::Instant::now() < deadline, "the preview never came");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        app.active_panel_mut_for_tests().select_name("a.txt");
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        settle(&mut app);
        let screen = render(&mut app, 100, 16);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Quick view: a.txt") && screen.contains("druhý řádek"), "{screen}");
        assert!(screen.contains("UTF-8"));
        app.active_panel_mut_for_tests().select_name("photos");
        settle(&mut app);
        let screen = render(&mut app, 100, 16);
        assert!(screen.contains("0 folders, 1 files") && screen.contains(" x.jpg"), "{screen}");
        // Tab goes into it; Esc back; Ctrl+Q again brings the other panel back.
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert!(app.preview_focus && app.active == 0);
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.preview_focus);
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!render(&mut app, 100, 16).contains("Quick view"));
    }

    #[test]
    fn renders_the_command_list() {
        let tmp = TempDir::new("ui-commands");
        let mut app = app_for(&tmp);
        app.dialog = Some(Dialog::Commands { filter: LineEdit::new("clip"), cursor: 1 });
        let screen = render(&mut app, 100, 20);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Commands") && screen.contains("Files to the clipboard (cut)") && screen.contains("Ctrl+X"));
    }

    #[test]
    fn renders_the_folder_history() {
        let tmp = TempDir::new("ui-history");
        let mut app = app_for(&tmp);
        app.history.dirs = vec![PathBuf::from(r"E:\Fotky\2024 Léto"), PathBuf::from(r"C:\Users\me\Downloads")];
        app.dialog = Some(Dialog::History { filter: LineEdit::new("fot"), cursor: 0 });
        let screen = render(&mut app, 100, 20);
        if std::env::var_os("RMC_PRINT").is_some() {
            println!("{screen}");
        }
        assert!(screen.contains("Folder history") && screen.contains("Filter: fot"));
        assert!(screen.contains(r"E:\Fotky\2024 Léto") && !screen.contains("Downloads"));
    }

    /// Scrolling help past its end doesn't build up a dead zone: Up moves at once.
    #[test]
    fn help_scrolls_no_further_than_its_end() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let tmp = TempDir::new("ui-help");
        let mut app = app_for(&tmp);
        app.on_key(KeyEvent::from(KeyCode::F(1)));
        for _ in 0..30 {
            app.on_key(KeyEvent::from(KeyCode::PageDown));
            render(&mut app, 80, 24);
        }
        let bottom = render(&mut app, 80, 24);
        app.on_key(KeyEvent::from(KeyCode::Up));
        assert_ne!(render(&mut app, 80, 24), bottom, "one Up scrolls back");
    }
}
