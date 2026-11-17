//! The panels as cm left them: each side's tabs (the folder, its order, the name under the
//! cursor), which tab each side shows, and which side is active. Saved on exit to
//! `session.txt` beside the settings, and opened again when cm starts without folders (the
//! option "Reopen the last folders and tabs").
//!
//! One `key=value` per line. A side's `tab` line starts a tab; the `sort`, `reverse` and
//! `cursor` lines after it are that tab's. Values are taken as they are, spaces and all, so
//! any folder name fits (one with a line break in it isn't kept).

use std::io;
use std::path::{Path, PathBuf};

use crate::panel::SortBy;
use crate::util;

#[derive(Clone, Debug, PartialEq)]
pub struct Tab {
    pub dir: PathBuf,
    pub sort: SortBy,
    pub reverse: bool,
    /// The name under the cursor.
    pub cursor: Option<String>,
}

impl Tab {
    pub fn new(dir: PathBuf) -> Tab {
        Tab { dir, sort: SortBy::Name, reverse: false, cursor: None }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Side {
    pub tabs: Vec<Tab>,
    /// Which of them is shown.
    pub shown: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Session {
    pub sides: [Side; 2],
    /// 0 left, 1 right.
    pub active: usize,
}

const SIDES: [&str; 2] = ["left", "right"];

impl Session {
    pub fn default_file() -> Option<PathBuf> {
        util::config_dir().map(|dir| dir.join("session.txt"))
    }

    /// What `file` holds; None when there is none, it can't be read, or a side has no tab.
    pub fn load(file: &Path) -> Option<Session> {
        let session = Session::parse(&util::read_text(file).ok()?);
        session.sides.iter().all(|side| !side.tabs.is_empty()).then_some(session)
    }

    pub fn save(&self, file: &Path) -> io::Result<()> {
        util::write_atomically(file, &self.to_text(), true)
    }

    /// The tab side `side` shows.
    pub fn shown(&self, side: usize) -> &Tab {
        let side = &self.sides[side];
        &side.tabs[side.shown.min(side.tabs.len() - 1)]
    }

    fn parse(text: &str) -> Session {
        let mut session = Session::default();
        for line in text.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let key = key.trim();
            if key == "active" {
                session.active = usize::from(value.trim() == "right");
                continue;
            }
            let Some((side, what)) = key.split_once('.') else { continue };
            let Some(side) = SIDES.iter().position(|name| *name == side) else { continue };
            let side = &mut session.sides[side];
            match what {
                "tab" if !value.is_empty() => side.tabs.push(Tab::new(PathBuf::from(value))),
                "shown" => side.shown = value.trim().parse().unwrap_or(0),
                _ => {
                    let Some(tab) = side.tabs.last_mut() else { continue };
                    match what {
                        "sort" => {
                            if let Some(by) = SortBy::ALL.into_iter().find(|by| by.label().eq_ignore_ascii_case(value.trim())) {
                                tab.sort = by;
                            }
                        }
                        "reverse" => tab.reverse = value.trim() == "true",
                        "cursor" if !value.is_empty() => tab.cursor = Some(value.to_string()),
                        _ => {}
                    }
                }
            }
        }
        for side in &mut session.sides {
            side.shown = side.shown.min(side.tabs.len().saturating_sub(1));
        }
        session
    }

    fn to_text(&self) -> String {
        let mut out = String::from(
            "# Crescent Moon's panels when it last closed. It opens them again when started without\n\
             # folders (Ctrl+P: \"Reopen the last folders and tabs\"). Each side's tabs in order.\n",
        );
        out += &format!("active={}\n", SIDES[self.active.min(1)]);
        let whole = |text: &str| !text.contains(['\n', '\r']);
        for (name, side) in SIDES.iter().zip(&self.sides) {
            let mut shown = side.shown;
            let mut kept = 0usize;
            for (i, tab) in side.tabs.iter().enumerate() {
                let dir = tab.dir.to_string_lossy();
                if dir.is_empty() || !whole(&dir) {
                    if i < side.shown {
                        shown = shown.saturating_sub(1);
                    }
                    continue;
                }
                kept += 1;
                out += &format!("{name}.tab={dir}\n{name}.sort={}\n", tab.sort.label());
                if tab.reverse {
                    out += &format!("{name}.reverse=true\n");
                }
                if let Some(cursor) = tab.cursor.as_deref().filter(|c| whole(c)) {
                    out += &format!("{name}.cursor={cursor}\n");
                }
            }
            out += &format!("{name}.shown={}\n", shown.min(kept.saturating_sub(1)));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn tab(dir: &str, sort: SortBy, reverse: bool, cursor: Option<&str>) -> Tab {
        Tab { dir: PathBuf::from(dir), sort, reverse, cursor: cursor.map(String::from) }
    }

    #[test]
    fn saved_and_read_back() {
        let tmp = TempDir::new("session");
        let file = tmp.path().join("cm").join("session.txt");
        assert_eq!(Session::load(&file), None, "no file, no session");
        let session = Session {
            sides: [
                Side {
                    tabs: vec![
                        tab("/home/me/src", SortBy::Name, false, Some("Cargo.toml")),
                        tab("/srv/photos 2024 ", SortBy::Time, true, Some(" a b ")),
                    ],
                    shown: 1,
                },
                Side { tabs: vec![tab(r"E:\Fotky\Účty", SortBy::Size, false, None)], shown: 0 },
            ],
            active: 1,
        };
        session.save(&file).unwrap();
        assert_eq!(Session::load(&file), Some(session.clone()), "spaces at the ends kept");
        assert_eq!(session.shown(0).dir, PathBuf::from("/srv/photos 2024 "));
    }

    #[test]
    fn odd_files_are_read_as_far_as_they_go() {
        let session = Session::parse(
            "# comment\r\nactive=sideways\nleft.cursor=before any tab\nleft.tab=/a\nleft.sort=Unknown\nleft.shown=7\n\
             right.tab=\nright.tab=/b\nright.cursor=x\nmiddle.tab=/c\nno equals sign\n",
        );
        assert_eq!(session.active, 0);
        assert_eq!(session.sides[0].tabs, [tab("/a", SortBy::Name, false, None)]);
        assert_eq!(session.sides[0].shown, 0, "a tab that isn't there: the last one");
        assert_eq!(session.sides[1].tabs, [tab("/b", SortBy::Name, false, Some("x"))]);
        // A side without tabs: nothing to reopen.
        assert!(Session::parse("left.tab=/a\n").sides[1].tabs.is_empty());
    }

    /// A folder or name with a line break can't be written as a line: it is left out, and the
    /// shown tab still points at the same one.
    #[test]
    fn names_with_line_breaks_are_left_out() {
        let session = Session {
            sides: [
                Side { tabs: vec![tab("/odd\nname", SortBy::Name, false, None), tab("/b", SortBy::Name, false, Some("x\ny"))], shown: 1 },
                Side { tabs: vec![tab("/c", SortBy::Name, false, None)], shown: 0 },
            ],
            active: 0,
        };
        let read = Session::parse(&session.to_text());
        assert_eq!(read.sides[0].tabs, [tab("/b", SortBy::Name, false, None)]);
        assert_eq!(read.sides[0].shown, 0);
    }
}
