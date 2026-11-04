//! Compare directories (Shift+F2, as in Total Commander; mc has it as Ctrl+X D): marks in
//! each panel what the other one lacks or has an older copy of, so F5 then brings the other
//! side up to date. Subfolders count as present or missing; their contents aren't compared.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::panel::Entry;

/// Times closer than this count as the same: FAT and exFAT drives (USB sticks, cards) store
/// times to two seconds, and copies to network shares may round them.
const SAME_TIME: Duration = Duration::from_secs(2);

/// The names to mark in the left and the right panel.
pub fn compare(left: &[Entry], right: &[Entry]) -> (Vec<String>, Vec<String>) {
    let index = |entries: &[Entry]| -> HashMap<String, usize> {
        entries.iter().enumerate().filter(|(_, e)| !e.is_parent).map(|(i, e)| (key(&e.name), i)).collect()
    };
    let (left_index, right_index) = (index(left), index(right));
    let mut marks = (Vec::new(), Vec::new());
    for entry in left.iter().filter(|e| !e.is_parent) {
        match right_index.get(&key(&entry.name)).map(|&i| &right[i]) {
            None => marks.0.push(entry.name.clone()),
            Some(other) => match newer(entry, other) {
                Some(true) => marks.0.push(entry.name.clone()),
                Some(false) => marks.1.push(other.name.clone()),
                None => {}
            },
        }
    }
    for entry in right.iter().filter(|e| !e.is_parent) {
        if !left_index.contains_key(&key(&entry.name)) {
            marks.1.push(entry.name.clone());
        }
    }
    marks
}

/// Names are the same regardless of case on Windows.
fn key(name: &str) -> String {
    if cfg!(windows) { name.to_lowercase() } else { name.to_string() }
}

/// Which of two entries of the same name to copy over the other: Some(true) for `a`,
/// Some(false) for `b`, None when they look the same. A file and a folder of the same
/// name are both marked.
fn newer(a: &Entry, b: &Entry) -> Option<bool> {
    if a.is_dir && b.is_dir {
        return None;
    }
    if a.is_dir != b.is_dir {
        return Some(true);
    }
    let (ta, tb) = (a.modified.unwrap_or(SystemTime::UNIX_EPOCH), b.modified.unwrap_or(SystemTime::UNIX_EPOCH));
    match ta.duration_since(tb) {
        Ok(d) if d > SAME_TIME => Some(true),
        Err(e) if e.duration() > SAME_TIME => Some(false),
        // The same time but another size: something is off; mark the bigger one.
        _ if a.size != b.size => Some(a.size > b.size),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::Panel;
    use crate::testutil::TempDir;
    use std::fs::{self, File};

    fn set_time(path: &std::path::Path, secs: u64) {
        let file = File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
    }

    #[test]
    fn marks_what_is_missing_or_newer_on_each_side() {
        let tmp = TempDir::new("compare");
        let (a, b) = (tmp.dir("a"), tmp.dir("b"));
        for (dir, name, text, time) in [
            (&a, "same.txt", "x", 1_000_000),
            (&b, "same.txt", "x", 1_000_001),
            (&a, "newer-left.txt", "x", 2_000_000),
            (&b, "newer-left.txt", "x", 1_000_000),
            (&a, "newer-right.txt", "x", 1_000_000),
            (&b, "NEWER-RIGHT.txt", "x", 2_000_000),
            (&a, "only-left.txt", "x", 1_000_000),
            (&b, "only-right.txt", "x", 1_000_000),
            (&a, "resized.txt", "xx", 1_000_000),
            (&b, "resized.txt", "x", 1_000_000),
        ] {
            let path = dir.join(name);
            fs::write(&path, text).unwrap();
            set_time(&path, time);
        }
        fs::create_dir(a.join("both")).unwrap();
        fs::create_dir(b.join("both")).unwrap();
        fs::create_dir(a.join("folder-only-left")).unwrap();
        let (left, right) = (Panel::new(a.clone(), true), Panel::new(b.clone(), true));
        let (mut l, mut r) = compare(&left.entries, &right.entries);
        l.sort();
        r.sort();
        if cfg!(windows) {
            assert_eq!(l, ["folder-only-left", "newer-left.txt", "only-left.txt", "resized.txt"]);
            assert_eq!(r, ["NEWER-RIGHT.txt", "only-right.txt"]);
        } else {
            // Names differing in case are different files on Linux.
            assert_eq!(l, ["folder-only-left", "newer-left.txt", "newer-right.txt", "only-left.txt", "resized.txt"]);
            assert_eq!(r, ["NEWER-RIGHT.txt", "only-right.txt"]);
        }
    }
}
