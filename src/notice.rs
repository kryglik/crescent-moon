//! Notices in a box in the middle of the screen. What the last action did ("The text of
//! "notes.txt" is on the clipboard…") was easy to miss at the end of the command line, so it
//! shows in a box over the panels for a few seconds (Ctrl+P: "Notices in the middle"), then
//! goes by itself. The notice stays on the command line after that until the next key, as it
//! always did, so one that was missed can still be read.
//!
//! The box holds nothing up: the next key or click does what it always does, and the box
//! goes with it.

use std::time::{Duration, Instant};

/// The notice in the box, since when, and for how long.
struct Shown {
    text: String,
    since: Instant,
    lasts: Duration,
    gone: bool,
}

#[derive(Default)]
pub struct NoticeBox {
    shown: Option<Shown>,
}

impl NoticeBox {
    /// Follows `App::notice`: a new one starts the clock, when the box can be seen (`free`:
    /// no dialog, shell or job over the panels; otherwise it stays on the command line).
    /// `seconds` 0 keeps notices on the command line. Returns whether the screen changes.
    pub fn follow(&mut self, notice: Option<&str>, free: bool, seconds: u64, now: Instant) -> bool {
        let before = self.is_up();
        match notice {
            Some(text) if seconds > 0 => match &mut self.shown {
                Some(shown) if shown.text == text => shown.gone = now >= shown.since + shown.lasts,
                _ if free => self.shown = Some(Shown { text: text.to_string(), since: now, lasts: lasts(text, seconds), gone: false }),
                _ => self.shown = None,
            },
            _ => self.shown = None,
        }
        self.is_up() != before
    }

    /// A key or a click: what is said after it is a new notice, even in the same words.
    pub fn forget(&mut self) {
        self.shown = None;
    }

    /// The text in the box while it is up.
    pub fn showing(&self) -> Option<&str> {
        self.shown.as_ref().filter(|shown| !shown.gone).map(|shown| shown.text.as_str())
    }

    /// The box is up, and will go by itself.
    pub fn is_up(&self) -> bool {
        self.showing().is_some()
    }
}

/// A long notice stays longer: a second more for every 40 characters past the first 40.
fn lasts(text: &str, seconds: u64) -> Duration {
    let extra = text.chars().count().saturating_sub(40) as u64 * 25;
    Duration::from_secs(seconds) + Duration::from_millis(extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_shows_for_a_while_then_goes() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut boxed = NoticeBox::default();
        assert!(!boxed.follow(None, true, 3, at(0)) && !boxed.is_up(), "nothing to say");
        assert!(boxed.follow(Some("Copied"), true, 3, at(0)), "a new notice: the box comes");
        assert_eq!(boxed.showing(), Some("Copied"));
        assert!(!boxed.follow(Some("Copied"), true, 3, at(2900)), "still there");
        assert!(boxed.follow(Some("Copied"), true, 3, at(3000)), "and gone after 3 s");
        assert!(!boxed.is_up());
        assert!(!boxed.follow(Some("Copied"), true, 3, at(9000)), "gone stays gone");
        // Another notice comes in a box of its own.
        assert!(boxed.follow(Some("Moved"), true, 3, at(9100)));
        assert!(boxed.follow(None, true, 3, at(9200)) && !boxed.is_up(), "a key took the notice away");
    }

    #[test]
    fn the_same_words_again_after_a_key_are_a_new_notice() {
        let start = Instant::now();
        let mut boxed = NoticeBox::default();
        boxed.follow(Some("Copied"), true, 3, start);
        boxed.follow(Some("Copied"), true, 3, start + Duration::from_secs(4));
        assert!(!boxed.is_up());
        boxed.forget();
        assert!(boxed.follow(Some("Copied"), true, 3, start + Duration::from_secs(5)), "shown again");
    }

    #[test]
    fn not_under_a_dialog_and_not_when_turned_off() {
        let start = Instant::now();
        let mut boxed = NoticeBox::default();
        assert!(!boxed.follow(Some("Copied"), false, 3, start) && !boxed.is_up(), "a dialog is open");
        assert!(boxed.follow(Some("Copied"), true, 3, start), "it closed: the box comes now");
        assert!(boxed.follow(Some("Copied"), true, 0, start) && !boxed.is_up(), "0 seconds: on the command line only");
        assert!(!boxed.follow(Some("Copied"), true, 0, start));
    }

    #[test]
    fn a_long_notice_stays_longer() {
        assert_eq!(lasts("short", 3), Duration::from_secs(3));
        assert_eq!(lasts(&"x".repeat(120), 3), Duration::from_secs(5));
    }
}
