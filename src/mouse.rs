//! Mouse support: click to select, double-click to open, right-click to mark, the wheel to
//! scroll, column headings to sort, the F-key bar, and dialog buttons and lists.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::app::App;
use crate::dialog::Dialog;
use crate::panel::SortBy;

/// Windows' default double-click time.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const WHEEL_STEP: isize = 3;

/// Where things were drawn in the last frame, so clicks can be mapped back to them.
#[derive(Default)]
pub struct HitAreas {
    pub panels: [PanelHit; 2],
    pub keybar: Rect,
    pub dialog: Option<DialogHit>,
    /// The Find window's result rows, and the row shown first.
    pub find: Option<(Rect, usize)>,
    /// The quick view, where the other panel was.
    pub preview: Option<Rect>,
    /// Each side's tabs and their numbers (when there are several).
    pub tabs: [Vec<(Rect, usize)>; 2],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PanelHit {
    pub area: Rect,
    /// The file rows; row `i` shows entry `panel.offset + i`.
    pub list: Rect,
    pub header_y: u16,
    pub size_x: u16,
    pub time_x: u16,
    /// Where the long listing's columns start (Alt+T), when they show: their headings don't sort.
    pub long_x: Option<u16>,
    pub details: bool,
}

#[derive(Debug, Default)]
pub struct DialogHit {
    pub area: Rect,
    /// Each button's area and index.
    pub buttons: Vec<(Rect, usize)>,
    /// Clickable list rows and the index shown in the first one.
    pub rows: Option<(Rect, usize)>,
}

/// The last left click in a panel, to recognise double-clicks.
pub struct LastClick {
    at: Instant,
    side: usize,
    index: usize,
}

enum DialogAction {
    Press(usize),
    Row(usize),
    Keys(KeyCode, usize),
    Close,
}

impl App {
    pub fn on_mouse(&mut self, event: MouseEvent) {
        let pos = Position::new(event.column, event.row);
        let wanted = matches!(
            event.kind,
            MouseEventKind::Down(MouseButton::Left | MouseButton::Right)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
        );
        if !wanted {
            return;
        }
        self.notice = None;
        // A click says where to be now, whatever was typed for a folder on its way.
        if matches!(event.kind, MouseEventKind::Down(_)) {
            self.typed_ahead.clear();
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some((at, place)) = self.closing_click.take()
            && place == pos
            && at.elapsed() < DOUBLE_CLICK
            && self.dialog.is_none()
        {
            return;
        }
        if self.shell_shown {
            // The wheel scrolls back through the shell's output.
            if let Some(shell) = &self.subshell {
                match event.kind {
                    MouseEventKind::ScrollUp => shell.scroll(WHEEL_STEP),
                    MouseEventKind::ScrollDown => shell.scroll(-WHEEL_STEP),
                    _ => {}
                }
            }
            return;
        }
        if self.dialog.is_some() {
            self.on_dialog_mouse(event.kind, pos);
            return;
        }
        if self.find_shown {
            self.on_find_mouse(event.kind, pos);
            return;
        }
        if self.job.is_some() || self.connecting.is_some() || self.switching.is_some() {
            return;
        }
        self.quicksearch = None;
        if self.hit.keybar.contains(pos) {
            if event.kind == MouseEventKind::Down(MouseButton::Left)
                && let Some(key) = crate::keybar::key_at(self.hit.keybar, pos.x, self.held)
            {
                self.on_key(key);
            }
            return;
        }
        // A tab: shown, and its side active.
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            let clicked = (0..2).find_map(|side| self.hit.tabs[side].iter().find(|(r, _)| r.contains(pos)).map(|&(_, i)| (side, i)));
            if let Some((side, i)) = clicked {
                self.active = side;
                self.switch_tab(side, i);
                return;
            }
        }
        // The quick view: the wheel scrolls it, a click gives it the focus (for the arrows).
        if let Some(area) = self.hit.preview
            && area.contains(pos)
        {
            let rows = self.preview_rows;
            match (event.kind, self.previewer.as_mut()) {
                (MouseEventKind::ScrollUp, Some(previewer)) => previewer.scroll(-WHEEL_STEP, rows),
                (MouseEventKind::ScrollDown, Some(previewer)) => previewer.scroll(WHEEL_STEP, rows),
                (MouseEventKind::Down(MouseButton::Left), _) => self.preview_focus = true,
                _ => {}
            }
            return;
        }
        for side in 0..2 {
            let hit = self.hit.panels[side];
            if hit.area.contains(pos) {
                self.preview_focus = false;
                self.on_panel_mouse(side, hit, event.kind, pos);
                return;
            }
        }
    }

    fn on_panel_mouse(&mut self, side: usize, hit: PanelHit, kind: MouseEventKind, pos: Position) {
        let button = match kind {
            MouseEventKind::ScrollUp => return self.panels[side].move_by(-WHEEL_STEP),
            MouseEventKind::ScrollDown => return self.panels[side].move_by(WHEEL_STEP),
            MouseEventKind::Down(button) => button,
            _ => return,
        };
        self.active = side;
        // A column heading sorts by it; clicking it again flips the order (A-Z / Z-A).
        // (Sorting by extension is in the F9 menu.)
        if hit.details && pos.y == hit.header_y && button == MouseButton::Left {
            if hit.long_x.is_some_and(|x| pos.x + 1 >= x) {
                return;
            }
            let by = match pos.x {
                x if x >= hit.time_x => SortBy::Time,
                x if x >= hit.size_x => SortBy::Size,
                _ => SortBy::Name,
            };
            self.panels[side].choose_sort(by);
            return;
        }
        if !hit.list.contains(pos) {
            return;
        }
        let panel = &mut self.panels[side];
        let index = panel.offset + (pos.y - hit.list.y) as usize;
        if index >= panel.entries.len() {
            return;
        }
        panel.cursor = index;
        if button == MouseButton::Right {
            panel.toggle_mark();
            return;
        }
        let double = self
            .last_click
            .as_ref()
            .is_some_and(|c| c.side == side && c.index == index && c.at.elapsed() < DOUBLE_CLICK);
        if double {
            self.last_click = None;
            self.open_current();
        } else {
            self.last_click = Some(LastClick { at: Instant::now(), side, index });
        }
    }

    /// In the Find window: the wheel scrolls the results, a click selects one, a double-click
    /// goes there (like Enter).
    fn on_find_mouse(&mut self, kind: MouseEventKind, pos: Position) {
        let Some(finder) = self.finder.as_mut() else { return };
        match kind {
            MouseEventKind::ScrollUp => finder.move_by(-WHEEL_STEP),
            MouseEventKind::ScrollDown => finder.move_by(WHEEL_STEP),
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((rows, first)) = self.hit.find else { return };
                if !rows.contains(pos) {
                    return;
                }
                let index = first + (pos.y - rows.y) as usize;
                if index >= finder.rows() {
                    return;
                }
                finder.move_to(index);
                // Side 2: the Find window, so a panel click never counts as its second click.
                let double = self.last_click.as_ref().is_some_and(|c| c.side == 2 && c.index == index && c.at.elapsed() < DOUBLE_CLICK);
                if double {
                    self.last_click = None;
                    self.find_go_to();
                } else {
                    self.last_click = Some(LastClick { at: Instant::now(), side: 2, index });
                }
            }
            _ => {}
        }
    }

    fn on_dialog_mouse(&mut self, kind: MouseEventKind, pos: Position) {
        self.dialog_mouse_action(kind, pos);
        if kind == MouseEventKind::Down(MouseButton::Left) && self.dialog.is_none() {
            self.closing_click = Some((Instant::now(), pos));
        }
    }

    fn dialog_mouse_action(&mut self, kind: MouseEventKind, pos: Position) {
        let Some(hit) = &self.hit.dialog else { return };
        let action = match kind {
            MouseEventKind::ScrollUp => DialogAction::Keys(KeyCode::Up, WHEEL_STEP as usize),
            MouseEventKind::ScrollDown => DialogAction::Keys(KeyCode::Down, WHEEL_STEP as usize),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(&(_, i)) = hit.buttons.iter().find(|(rect, _)| rect.contains(pos)) {
                    DialogAction::Press(i)
                } else if let Some((rows, first)) = hit.rows
                    && rows.contains(pos)
                {
                    DialogAction::Row(first + (pos.y - rows.y) as usize)
                } else if !hit.area.contains(pos) {
                    DialogAction::Close
                } else {
                    return;
                }
            }
            _ => return,
        };
        match action {
            DialogAction::Press(i) => {
                if let Some(Dialog::Confirm { focus, .. } | Dialog::Question { focus, .. } | Dialog::Delete { focus, .. }) =
                    &mut self.dialog
                {
                    *focus = i;
                }
                self.on_key(KeyEvent::from(KeyCode::Enter));
            }
            DialogAction::Row(i) => {
                if matches!(self.dialog, Some(Dialog::Options { .. })) {
                    self.click_option(i);
                    return;
                }
                // A click on an attribute toggles it.
                if let Some(Dialog::Attributes { states, cursor, .. }) = &mut self.dialog {
                    if i < states.len() {
                        *cursor = i;
                        crate::app::toggle_state(states, i);
                    }
                    return;
                }
                let list_len = match &self.dialog {
                    Some(Dialog::History { filter, .. }) => self.history_rows(filter.text()).len(),
                    Some(Dialog::Earlier { filter, back, .. }) => crate::dialog::earlier_rows(back, filter.text()).len(),
                    Some(Dialog::Commands { filter, .. }) => crate::dialog::matching_commands(filter.text()).len(),
                    Some(Dialog::Worktrees { filter, worktrees, .. }) => {
                        crate::dialog::worktree_rows(worktrees, filter.text()).len()
                    }
                    Some(Dialog::Branches { filter, branches, .. }) => crate::dialog::branch_rows(branches, filter.text()).len(),
                    _ => 0,
                };
                match &mut self.dialog {
                    Some(Dialog::Drives { cursor, drives, .. }) if i < drives.len() => *cursor = i,
                    Some(Dialog::Hotlist { cursor }) if i < self.hotlist.dirs.len() => *cursor = i,
                    Some(
                        Dialog::History { cursor, .. }
                        | Dialog::Earlier { cursor, .. }
                        | Dialog::Commands { cursor, .. }
                        | Dialog::Worktrees { cursor, .. }
                        | Dialog::Branches { cursor, .. },
                    ) if i < list_len => *cursor = i,
                    Some(Dialog::Sort { cursor, .. }) if i < SortBy::ALL.len() => *cursor = i,
                    Some(Dialog::Places { cursor, places, .. }) if i < places.len() => *cursor = i,
                    Some(Dialog::Schemes { cursor, choices, .. }) if i < choices.len() => *cursor = i,
                    _ => return,
                }
                self.on_key(KeyEvent::from(KeyCode::Enter));
            }
            DialogAction::Keys(code, times) => {
                for _ in 0..times {
                    self.on_key(KeyEvent::from(code));
                }
            }
            // Clicking beside a list or message closes it; never a question or typed input.
            DialogAction::Close => {
                if matches!(
                    self.dialog,
                    Some(
                        Dialog::Help { .. }
                            | Dialog::Drives { .. }
                            | Dialog::Hotlist { .. }
                            | Dialog::History { .. }
                            | Dialog::Earlier { .. }
                            | Dialog::Commands { .. }
                            | Dialog::Worktrees { .. }
                            | Dialog::Branches { .. }
                            | Dialog::Places { .. }
                            | Dialog::Schemes { .. }
                            | Dialog::Attributes { .. }
                            | Dialog::Sort { .. }
                            | Dialog::Options { .. }
                            | Dialog::Message { .. }
                    )
                ) {
                    self.on_key(KeyEvent::from(KeyCode::Esc));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui;
    use crate::testutil::TempDir;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use std::path::Path;

    const W: u16 = 100;
    const H: u16 = 20;

    fn app(dir: &Path) -> App {
        let mut app = App::new(dir.to_path_buf(), dir.to_path_buf(), crate::testutil::config());
        draw(&mut app);
        app
    }

    fn draw(app: &mut App) {
        let mut terminal = Terminal::new(TestBackend::new(W, H)).unwrap();
        terminal.draw(|frame| ui::draw(frame, app)).unwrap();
    }

    fn mouse(app: &mut App, kind: MouseEventKind, x: u16, y: u16) {
        app.on_mouse(MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE });
        draw(app);
    }

    fn click(app: &mut App, x: u16, y: u16) {
        mouse(app, MouseEventKind::Down(MouseButton::Left), x, y);
    }

    /// Screen position of entry `index` in panel `side`.
    fn row_of(app: &App, side: usize, index: usize) -> (u16, u16) {
        let hit = app.hit.panels[side];
        (hit.list.x + 3, hit.list.y + (index - app.panels[side].offset) as u16)
    }

    fn sample() -> TempDir {
        let tmp = TempDir::new("mouse");
        tmp.dir("docs");
        tmp.file("a.txt", "a");
        tmp.file("b.txt", "bb");
        tmp
    }

    #[test]
    fn click_selects_and_double_click_opens() {
        let tmp = sample();
        let mut app = app(tmp.path());
        // Right panel, entry 2 (a.txt): it becomes active with the cursor there.
        let (x, y) = row_of(&app, 1, 2);
        click(&mut app, x, y);
        assert_eq!(app.active, 1);
        assert_eq!(app.active_panel().current().unwrap().name, "a.txt");
        // Double-clicking "docs" enters it.
        let (x, y) = row_of(&app, 1, 1);
        click(&mut app, x, y);
        click(&mut app, x, y);
        assert!(app.active_panel().cwd.ends_with("docs"));
    }

    #[test]
    fn right_click_marks_and_wheel_scrolls() {
        let tmp = sample();
        let mut app = app(tmp.path());
        let (x, y) = row_of(&app, 0, 3);
        mouse(&mut app, MouseEventKind::Down(MouseButton::Right), x, y);
        assert_eq!(app.panels[0].marked_summary(), Some((1, 2)));
        mouse(&mut app, MouseEventKind::ScrollUp, x, y);
        assert_eq!(app.panels[0].cursor, 0);
        mouse(&mut app, MouseEventKind::ScrollDown, x, y);
        assert_eq!(app.panels[0].cursor, 3);
    }

    /// The menu of places' rows are clickable; a click beside it closes it.
    #[test]
    fn places_menu_by_mouse() {
        let tmp = TempDir::new("mouse-places");
        let (checkout, file) = crate::places::tests::fake_workspace(&tmp);
        let mut app = app(&checkout);
        app.config.places_file = Some(file);
        app.on_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        draw(&mut app);
        let (rows, _) = app.hit.dialog.as_ref().unwrap().rows.unwrap();
        // The last row: the top, app, blog, the scripts, the docs.
        click(&mut app, rows.x + 2, rows.y + 4);
        assert!(app.dialog.is_none());
        assert_eq!(app.active_panel().current().unwrap().name, "guide.md");

        app.on_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        draw(&mut app);
        click(&mut app, 0, 0);
        assert!(app.dialog.is_none());
    }

    /// The long listing's headings (Alt+T) don't sort: a click there leaves the order as it is.
    #[test]
    fn the_long_listings_headings_dont_sort() {
        let tmp = sample();
        let mut app = app(tmp.path());
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT));
        draw(&mut app);
        let hit = app.hit.panels[0];
        let long_x = hit.long_x.expect("the long listing has columns here");
        let order = |app: &App| (app.panels[0].sort, app.panels[0].reverse);
        let before = order(&app);
        click(&mut app, long_x + 1, hit.header_y);
        assert_eq!(order(&app), before);
        click(&mut app, hit.time_x + 2, hit.header_y);
        assert_eq!(order(&app).0, SortBy::Time, "the time's heading still does");
    }

    #[test]
    fn headings_sort_and_key_bar_runs_function_keys() {
        let tmp = sample();
        let mut app = app(tmp.path());
        let hit = app.hit.panels[0];
        let order = |app: &App| (app.panels[0].sort, app.panels[0].reverse);
        click(&mut app, hit.size_x + 2, hit.header_y);
        assert_eq!(order(&app), (SortBy::Size, false));
        click(&mut app, hit.size_x + 2, hit.header_y);
        assert_eq!(order(&app), (SortBy::Size, true), "the same heading again flips it");
        click(&mut app, hit.time_x + 2, hit.header_y);
        assert_eq!(order(&app), (SortBy::Time, false));
        click(&mut app, hit.list.x + 10, hit.header_y);
        assert_eq!(order(&app), (SortBy::Name, false));
        click(&mut app, hit.list.x + 10, hit.header_y);
        assert_eq!(order(&app), (SortBy::Name, true));

        // "9Sort" opens the sort menu; its rows are clickable ("Size" is the third).
        let bar = app.hit.keybar;
        click(&mut app, bar.x + bar.width / 10 * 8 + 2, bar.y);
        assert!(matches!(app.dialog, Some(Dialog::Sort { .. })));
        let (rows, _) = app.hit.dialog.as_ref().unwrap().rows.unwrap();
        click(&mut app, rows.x + 2, rows.y + 2);
        assert!(app.dialog.is_none());
        assert_eq!(order(&app), (SortBy::Size, false));

        // "7Mkdir" opens the make-directory dialog; clicking outside an input dialog keeps it.
        let bar = app.hit.keybar;
        click(&mut app, bar.x + bar.width / 10 * 6 + 2, bar.y);
        assert!(matches!(app.dialog, Some(Dialog::Input { .. })));
        click(&mut app, 0, 0);
        assert!(app.dialog.is_some());
    }

    /// Double-clicking a menu row picks it with the first click; the second one must not
    /// land on the panel that was under the menu.
    #[test]
    fn a_double_click_on_a_menu_row_stays_in_the_menu() {
        let tmp = sample();
        let mut app = app(tmp.path());
        assert_eq!(app.active, 0);
        app.on_key(KeyEvent::from(KeyCode::F(9)));
        draw(&mut app);
        let (rows, _) = app.hit.dialog.as_ref().unwrap().rows.unwrap();
        // The right end of the "Size" row, over the right panel.
        let (x, y) = (rows.right() - 2, rows.y + 2);
        assert!(app.hit.panels[1].area.contains(Position::new(x, y)));
        click(&mut app, x, y);
        assert!(app.dialog.is_none());
        click(&mut app, x, y);
        assert_eq!(app.active, 0, "the second click was dropped");
        assert_eq!(app.panels[0].sort, SortBy::Size);
    }

    #[test]
    fn dialog_buttons_are_clickable() {
        let tmp = sample();
        let mut app = app(tmp.path());
        let (x, y) = row_of(&app, 0, 3);
        click(&mut app, x, y);
        // Shift+F8 asks; clicking "No" keeps the file.
        app.on_key(KeyEvent::new(KeyCode::F(8), KeyModifiers::SHIFT));
        draw(&mut app);
        let no = app.hit.dialog.as_ref().unwrap().buttons[1].0;
        click(&mut app, no.x + 1, no.y);
        assert!(app.dialog.is_none());
        assert!(tmp.path().join("b.txt").exists());
        // Again, clicking "Yes" deletes it.
        app.on_key(KeyEvent::new(KeyCode::F(8), KeyModifiers::SHIFT));
        draw(&mut app);
        let yes = app.hit.dialog.as_ref().unwrap().buttons[0].0;
        click(&mut app, yes.x + 1, yes.y);
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.job.is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            app.tick();
        }
        assert!(!tmp.path().join("b.txt").exists());
    }

    #[test]
    fn hotlist_rows_are_clickable_and_outside_click_closes() {
        let tmp = sample();
        let mut app = app(tmp.path());
        app.hotlist.dirs = vec![tmp.path().to_path_buf(), tmp.path().join("docs")];
        app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        draw(&mut app);
        let (rows, _) = app.hit.dialog.as_ref().unwrap().rows.unwrap();
        click(&mut app, rows.x + 2, rows.y + 1);
        assert!(app.dialog.is_none());
        assert!(app.active_panel().cwd.ends_with("docs"));

        app.on_key(KeyEvent::from(KeyCode::F(1)));
        draw(&mut app);
        click(&mut app, 0, 0);
        assert!(app.dialog.is_none(), "help closes on an outside click");
    }
}
