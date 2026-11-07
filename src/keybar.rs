//! The bar at the bottom: the keys and what they do. While Shift, Ctrl or Alt is held it shows
//! what they do with that, which is how the keys nobody remembers get found.
//!
//! Only Windows tells a program about such a key held on its own (see
//! `platform::wait_for_input`). A terminal on Linux, or over ssh, says nothing until another key
//! comes, so there the bar stays as it is.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::scheme::Scheme;
use crate::util;

/// How often to look while something is held: letting go of it in another window (Alt+Tab)
/// is seen only on the keyboard.
pub const LOOK_EVERY: Duration = Duration::from_millis(50);
/// How long a key is held before the bar changes: Ctrl+S on its way to the S mustn't make the
/// whole bar blink, nor a capital letter.
const HELD_FOR: Duration = Duration::from_millis(200);
/// The places on the bar, as many as there are function keys it shows.
const SLOTS: usize = 10;
/// From how wide a place on the bar a key gets to look like one.
const ROOMY: u16 = 9;

/// What is held down, on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Held {
    #[default]
    Nothing,
    Shift,
    Ctrl,
    Alt,
}

impl Held {
    /// Going by what is down. Two together are nothing the bar knows, and Ctrl with Alt is
    /// what Windows makes of AltGr, which types.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn of(shift: bool, ctrl: bool, alt: bool) -> Held {
        match (shift, ctrl, alt) {
            (true, false, false) => Held::Shift,
            (false, true, false) => Held::Ctrl,
            (false, false, true) => Held::Alt,
            _ => Held::Nothing,
        }
    }

    fn modifiers(self) -> KeyModifiers {
        match self {
            Held::Nothing => KeyModifiers::NONE,
            Held::Shift => KeyModifiers::SHIFT,
            Held::Ctrl => KeyModifiers::CONTROL,
            Held::Alt => KeyModifiers::ALT,
        }
    }
}

/// Tells what the bar should go by from what is down moment to moment.
#[derive(Default)]
pub struct Watch {
    down: Held,
    since: Option<Instant>,
}

impl Watch {
    /// `down` is down now: what the bar shows then.
    pub fn see(&mut self, down: Held, now: Instant) -> Held {
        if down != self.down || self.since.is_none() {
            (self.down, self.since) = (down, Some(now));
        }
        let long_enough = self.since.is_some_and(|since| now.duration_since(since) >= HELD_FOR);
        if long_enough { down } else { Held::Nothing }
    }

    /// Something is down, so the next look shouldn't be long in coming.
    pub fn is_watching(&self) -> bool {
        self.down != Held::Nothing
    }
}

/// A place on the bar: the key's name, what it does, and the key itself.
#[derive(Clone, Copy)]
struct Item {
    key: &'static str,
    label: &'static str,
    code: KeyCode,
}

const fn f(n: u8, key: &'static str, label: &'static str) -> Option<Item> {
    Some(Item { key, label, code: KeyCode::F(n) })
}

const fn letter(c: char, key: &'static str, label: &'static str) -> Option<Item> {
    Some(Item { key, label, code: KeyCode::Char(c) })
}

const PLAIN: [Option<Item>; SLOTS] = [
    f(1, "F1", "Help"),
    f(2, "F2", "Rename"),
    f(3, "F3", "View"),
    f(4, "F4", "Edit"),
    f(5, "F5", "Copy"),
    f(6, "F6", "Move"),
    f(7, "F7", "Mkdir"),
    f(8, "F8", "Delete"),
    f(9, "F9", "Sort"),
    f(10, "F10", "Quit"),
];

/// A key stays where it is on every layer of the bar.
const SHIFT: [Option<Item>; SLOTS] = [
    Some(Item { key: "↵", label: "Open…", code: KeyCode::Enter }),
    f(2, "F2", "Compare"),
    None,
    f(4, "F4", "New file"),
    f(5, "F5", "Copy as"),
    f(6, "F6", "Rename"),
    None,
    f(8, "F8", "Erase"),
    None,
    None,
];

const ALT: [Option<Item>; SLOTS] = [
    f(1, "F1", "Drive L"),
    f(2, "F2", "Drive R"),
    letter('c', "C", "Colours"),
    letter('w', "W", "Worktree"),
    f(5, "F5", "Pack"),
    letter('a', "A", "Attribs"),
    f(7, "F7", "Find"),
    letter('h', "H", "History"),
    f(9, "F9", "Unpack"),
    letter('v', "V", "Paste"),
];

const CTRL: [Option<Item>; SLOTS] = [
    letter('o', "O", "Shell"),
    letter('p', "P", "Options"),
    letter('r', "R", "Reload"),
    letter('t', "T", "New tab"),
    letter('w', "W", "Close"),
    letter('f', "F", "Filter"),
    letter('s', "S", "Search"),
    letter('q', "Q", "Preview"),
    letter('d', "D", "Hotlist"),
    letter('n', "N", "Connect"),
];

fn layer(held: Held) -> &'static [Option<Item>; SLOTS] {
    match held {
        Held::Nothing => &PLAIN,
        Held::Shift => &SHIFT,
        Held::Ctrl => &CTRL,
        Held::Alt => &ALT,
    }
}

/// The key a click at column `x` of the bar presses.
pub fn key_at(bar: Rect, x: u16, held: Held) -> Option<KeyEvent> {
    let slot = bar.width / SLOTS as u16;
    if slot == 0 {
        return None;
    }
    let at = ((x - bar.x) / slot).min(SLOTS as u16 - 1) as usize;
    layer(held)[at].map(|item| KeyEvent::new(item.code, held.modifiers()))
}

pub fn draw(buf: &mut Buffer, area: Rect, held: Held, t: &Scheme) {
    buf.set_style(area, t.command_line);
    let slot = area.width / SLOTS as u16;
    let (key_style, label_style) = (t.on_bar(t.key_number), t.on_bar(t.key_label));
    // A half block at each end, in the key's own colour, takes the corners off it and leaves
    // half a cell before the label. A key that has no colour of its own (reversed, in the
    // plainer schemes) gets a space instead.
    let edge = t.key_number.bg.map(|colour| t.command_line.fg(colour));
    for (i, item) in layer(held).iter().enumerate() {
        let Some(item) = item else { continue };
        let x = area.x + slot * i as u16;
        let width = if i == SLOTS - 1 { area.width - slot * i as u16 } else { slot };
        let key_width = util::width(item.key) as u16;
        if slot < ROOMY {
            // Narrow: the number alone, as in mc.
            let number = item.key.strip_prefix('F').unwrap_or(item.key);
            let number_width = util::width(number) as u16;
            if width > number_width {
                buf.set_string(x, area.y, number, key_style);
                buf.set_string(x + number_width, area.y, util::fit(item.label, (width - number_width) as usize), label_style);
            }
            continue;
        }
        let used = match edge {
            Some(edge) => {
                buf.set_string(x, area.y, "▐", edge);
                buf.set_string(x + 1, area.y, item.key, key_style);
                buf.set_string(x + 1 + key_width, area.y, "▌", edge);
                key_width + 2
            }
            None => {
                buf.set_string(x, area.y, format!(" {} ", item.key), key_style);
                key_width + 3
            }
        };
        buf.set_string(x + used, area.y, util::fit(item.label, width.saturating_sub(used) as usize), label_style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog::COMMANDS;
    use crate::scheme;

    fn text(held: Held, width: u16) -> String {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, held, &scheme::classic());
        (0..width).map(|x| buf[(x, 0)].symbol().to_string()).collect()
    }

    #[test]
    fn the_keys_look_like_keys_with_room_for_it() {
        let wide = text(Held::Nothing, 130);
        assert!(wide.contains("▐F5▌Copy"), "{wide}");
        assert!(wide.contains("▐F10▌Quit"), "{wide}");
        let narrow = text(Held::Nothing, 80);
        assert!(narrow.contains("5Copy"), "{narrow}");
        assert!(narrow.contains("10Quit"), "{narrow}");
        assert!(!narrow.contains('▐'), "{narrow}");
    }

    /// A scheme without colours can't take the corners off: the key is a block.
    #[test]
    fn a_key_without_a_colour_of_its_own_is_a_block() {
        let area = Rect::new(0, 0, 130, 1);
        let mut buf = Buffer::empty(area);
        draw(&mut buf, area, Held::Nothing, &scheme::mono());
        let line: String = (0..130).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(line.contains(" F5  Copy"), "{line}");
        assert!(!line.contains('▐'), "{line}");
    }

    #[test]
    fn a_key_stays_where_it_is_when_the_bar_changes() {
        let (plain, alt) = (text(Held::Nothing, 130), text(Held::Alt, 130));
        assert!(alt.contains("▐F7▌Find"), "{alt}");
        assert_eq!(plain.chars().position(|c| c == '7'), alt.chars().position(|c| c == '7'));
        assert!(text(Held::Shift, 130).contains("▐F8▌Erase"));
        assert!(text(Held::Ctrl, 130).contains("▐O▌Shell"));
    }

    /// At 110 columns, a usual window, nothing is cut short but the odd long word.
    #[test]
    fn the_labels_fit_a_usual_window() {
        for held in [Held::Nothing, Held::Shift, Held::Ctrl, Held::Alt] {
            for item in layer(held).iter().flatten() {
                assert!(util::width(item.key) + 2 + util::width(item.label) <= 12, "{}", item.label);
            }
        }
    }

    /// The bar says only what is so: each key on it is one F12's list runs, and tests.
    #[test]
    fn every_key_on_the_bar_does_what_it_says_in_the_list_of_commands() {
        for held in [Held::Nothing, Held::Shift, Held::Ctrl, Held::Alt] {
            let bar = Rect::new(0, 0, 130, 1);
            for at in 0..SLOTS {
                let Some(key) = key_at(bar, 13 * at as u16 + 2, held) else { continue };
                let known = COMMANDS.iter().any(|command| command.code == key.code && command.modifiers == key.modifiers);
                assert!(known, "{held:?}: {key:?}");
            }
        }
    }

    #[test]
    fn a_click_presses_the_key_shown_there() {
        let bar = Rect::new(4, 9, 100, 1);
        assert_eq!(key_at(bar, 4, Held::Nothing), Some(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)));
        assert_eq!(key_at(bar, 103, Held::Nothing), Some(KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE)));
        assert_eq!(key_at(bar, 48, Held::Alt), Some(KeyEvent::new(KeyCode::F(5), KeyModifiers::ALT)));
        assert_eq!(key_at(bar, 28, Held::Shift), None, "nothing there");
    }

    #[test]
    fn altgr_and_two_keys_together_are_nothing() {
        assert_eq!(Held::of(true, false, false), Held::Shift);
        assert_eq!(Held::of(false, true, false), Held::Ctrl);
        assert_eq!(Held::of(false, false, true), Held::Alt);
        assert_eq!(Held::of(false, true, true), Held::Nothing, "AltGr");
        assert_eq!(Held::of(true, true, false), Held::Nothing);
        assert_eq!(Held::of(false, false, false), Held::Nothing);
    }

    /// Only a key that stays down changes the bar; letting go shows at once.
    #[test]
    fn the_bar_doesnt_blink_at_a_key_on_its_way_to_another() {
        let mut watch = Watch::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        assert!(!watch.is_watching());
        assert_eq!(watch.see(Held::Ctrl, at(0)), Held::Nothing);
        assert!(watch.is_watching());
        assert_eq!(watch.see(Held::Ctrl, at(100)), Held::Nothing, "Ctrl+S is over by now");
        assert_eq!(watch.see(Held::Nothing, at(150)), Held::Nothing);
        assert_eq!(watch.see(Held::Ctrl, at(160)), Held::Nothing, "pressed anew");
        assert_eq!(watch.see(Held::Ctrl, at(300)), Held::Nothing);
        assert_eq!(watch.see(Held::Ctrl, at(370)), Held::Ctrl);
        assert_eq!(watch.see(Held::Alt, at(380)), Held::Nothing, "another key counts anew");
        assert_eq!(watch.see(Held::Nothing, at(390)), Held::Nothing);
        assert!(!watch.is_watching());
    }
}
