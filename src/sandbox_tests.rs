//! Copying, moving and deleting on real files in temporary folders, at the edges: names
//! Windows treats specially, links and junctions inside what is deleted or moved, long paths,
//! files in use or unreadable, odd names, answers of No in a merge. Each test checks what is
//! on disk afterwards; most check one rule for the whole tree: after a move every file is at
//! its destination or still at its source, after a delete nothing else has changed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ops::{self, Answer, Job, Msg};
use crate::testutil::TempDir;
use crate::util;

mod random;

/// Runs a job to the end, answering every question with `answer`: the errors, whether it
/// was cancelled, and how many questions it asked.
fn run(job: Job, answer: Answer) -> (Vec<String>, bool, usize) {
    let running = ops::start(job);
    let mut questions = 0;
    loop {
        match running.rx.recv_timeout(Duration::from_secs(30)).expect("job hung") {
            Msg::Progress(_) => {}
            Msg::Ask(_, reply) => {
                questions += 1;
                reply.send(answer).unwrap();
            }
            Msg::Done { errors, cancelled, .. } => return (errors, cancelled, questions),
        }
    }
}

fn delete(paths: Vec<PathBuf>) -> Job {
    Job::Delete { paths, to_trash: false, admin_when_refused: false }
}

/// What a tree holds: each file's content, each link's target, each folder, by relative
/// path. Links are not followed. (Read by literal paths on Windows, so a name ending in a dot
/// is read as itself.)
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    #[cfg(windows)]
    let root = &literal(root);
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let key = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            let meta = fs::symlink_metadata(&path).unwrap();
            let value = if meta.file_type().is_symlink() {
                format!("link -> {}", fs::read_link(&path).map(|t| t.display().to_string()).unwrap_or_default())
            } else if meta.is_dir() {
                stack.push(path);
                "folder".into()
            } else if meta.is_file() {
                format!("file {}", String::from_utf8_lossy(&fs::read(&path).unwrap_or_default()))
            } else {
                "special".into()
            };
            out.insert(key, value);
        }
    }
    out
}

/// After a move: every file and link of `before` is at the destination or still at the
/// source, as it was. Returns how many were left behind.
fn nothing_lost(before: &BTreeMap<String, String>, src: &Path, dst: &Path) -> usize {
    let (left, arrived) = (snapshot(src), snapshot(dst));
    let mut behind = 0;
    for (path, value) in before.iter().filter(|(_, v)| *v != "folder") {
        match (left.get(path), arrived.get(path)) {
            (Some(v), _) if v == value => behind += 1,
            (_, Some(v)) if v == value => {}
            other => panic!("{path} ({value}) was lost or changed: {other:?}"),
        }
    }
    behind
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

fn set_readonly(path: &Path, readonly: bool) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(readonly);
    fs::set_permissions(path, permissions).unwrap();
}

/// The path as Windows takes it literally (`\\?\C:\…`): names ending in a dot or space, and
/// device names such as `con.txt`, can only be made and reached this way.
#[cfg(windows)]
fn literal(path: &Path) -> PathBuf {
    crate::lfs::to_literal(path).unwrap()
}

#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(link).arg(target).output().unwrap();
    assert!(made.status.success(), "mklink /J: {}", String::from_utf8_lossy(&made.stderr));
}

/// A link to a folder: a junction on Windows (no privilege needed), a symlink elsewhere.
pub fn folder_link(link: &Path, target: &Path) {
    #[cfg(windows)]
    junction(link, target);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// A folder holding `foo` and `foo.`, `bar` and `bar `: four files to Windows, though its
/// usual paths can't tell them apart.
#[cfg(windows)]
fn dotted_folder(tmp: &TempDir) -> PathBuf {
    let dir = tmp.dir("d");
    fs::write(dir.join("foo"), "plain").unwrap();
    fs::write(literal(&dir.join("foo.")), "dot").unwrap();
    fs::write(dir.join("bar"), "plain bar").unwrap();
    fs::write(literal(&dir.join("bar ")), "space").unwrap();
    assert_eq!(names(&dir), ["bar", "bar ", "foo", "foo."]);
    dir
}

/// Deleting `foo.` deletes that file, never `foo`; a folder `folder.` goes with what it holds.
#[cfg(windows)]
#[test]
fn deleting_a_name_ending_in_a_dot_spares_the_one_without() {
    let tmp = TempDir::new("edge-dots-delete");
    let dir = dotted_folder(&tmp);
    let (errors, _, _) = run(delete(vec![dir.join("foo."), dir.join("bar ")]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(names(&dir), ["bar", "foo"], "only foo. and bar  went");
    assert_eq!(fs::read_to_string(dir.join("foo")).unwrap(), "plain");
    assert_eq!(fs::read_to_string(dir.join("bar")).unwrap(), "plain bar");

    let odd = dir.join("folder.");
    fs::create_dir(literal(&odd)).unwrap();
    fs::write(literal(&odd.join("inside.txt")), "x").unwrap();
    let (errors, _, _) = run(delete(vec![odd]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(names(&dir), ["bar", "foo"]);
}

/// The Recycle Bin can only be given the usual path, which means `foo` for `foo.`: such a
/// name is refused there, with Shift+F8 named instead, and nothing is touched.
#[cfg(windows)]
#[test]
fn the_recycle_bin_refuses_a_name_it_would_mistake() {
    let tmp = TempDir::new("edge-dots-trash");
    let dir = dotted_folder(&tmp);
    let job = Job::Delete { paths: vec![dir.join("foo.")], to_trash: true, admin_when_refused: false };
    let running = ops::start(job);
    let mut said = String::new();
    loop {
        match running.rx.recv_timeout(Duration::from_secs(30)).expect("job hung") {
            Msg::Progress(_) => {}
            Msg::Ask(ops::Question::Failed { error, as_admin, .. }, reply) => {
                assert!(as_admin.is_none(), "no As admin for this");
                said = error;
                reply.send(Answer::Skip).unwrap();
            }
            Msg::Ask(other, _) => panic!("unexpected {other:?}"),
            Msg::Done { .. } => break,
        }
    }
    assert!(said.contains("Shift+F8"), "{said}");
    assert_eq!(names(&dir), ["bar", "bar ", "foo", "foo."]);
}

/// Copying them copies each with its own content, by itself or in its folder.
#[cfg(windows)]
#[test]
fn copying_a_name_ending_in_a_dot_copies_that_file() {
    let tmp = TempDir::new("edge-dots-copy");
    let dir = dotted_folder(&tmp);
    // The panel's paths: the folder joined with each name it lists.
    let listed: Vec<PathBuf> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
    let out = tmp.dir("out");
    let pairs = ops::plan(&listed, out.to_str().unwrap(), tmp.path()).unwrap();
    let (errors, _, questions) = run(Job::Copy(pairs), Answer::Skip);
    assert!(errors.is_empty() && questions == 0, "{errors:?}");
    assert_eq!(snapshot(&out), snapshot(&dir), "each copied with its own content");
    let whole = tmp.path().join("whole");
    let (errors, _, _) = run(Job::Copy(vec![(dir.clone(), whole.clone())]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snapshot(&whole), snapshot(&dir));
    // A new name typed with a dot at the end is made as Windows makes it, without the dot.
    let pairs = ops::plan(&[dir.join("foo.")], "typed.", tmp.path()).unwrap();
    assert_eq!(pairs[0].1, tmp.path().join("typed"));
}

/// Moving them, on one drive and to another, moves each file as itself.
#[cfg(windows)]
#[test]
fn moving_a_name_ending_in_a_dot_moves_that_file() {
    let tmp = TempDir::new("edge-dots-move");
    let other_drive = TempDir::in_target("edge-dots-move-dst");
    let dir = dotted_folder(&tmp);
    let before = snapshot(&dir);
    let (errors, _, _) = run(Job::Move(vec![(dir.join("bar "), tmp.path().join("moved"))]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(fs::read_to_string(tmp.path().join("moved")).unwrap(), "space");
    assert_eq!(names(&dir), ["bar", "foo", "foo."]);
    let across = other_drive.path().join("d");
    let (errors, _, _) = run(Job::Move(vec![(dir.clone(), across.clone())]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    let mut expected = before;
    expected.remove("bar ");
    assert_eq!(snapshot(&across), expected);
    assert!(!dir.exists());
}

/// The Linux kernel has an `aux.c`: names that are devices on older Windows only are plain
/// files here, which other programs open as usual, so nothing refuses them.
#[cfg(windows)]
#[test]
fn only_what_windows_really_misreads_counts() {
    let tmp = TempDir::new("edge-misread");
    let file = tmp.file("aux.c", "int main;");
    assert_eq!(fs::read_to_string(&file).unwrap(), "int main;", "this Windows takes it as a file");
    assert!(!crate::lfs::needs_literal(&file));
    assert!(crate::lfs::needs_literal(&tmp.path().join("nul")));
    assert!(crate::lfs::needs_literal(&tmp.path().join("foo.")));
}

/// A settings file that is a link into a dotfiles repository stays a link when it is saved.
#[cfg(unix)]
#[test]
fn saving_keeps_a_link_to_the_settings() {
    let tmp = TempDir::new("edge-save-link");
    let real = tmp.file("dotfiles/settings.ini", "old");
    let link = tmp.path().join("settings.ini");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    crate::util::write_atomically(&link, "new", true).unwrap();
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(&real).unwrap(), "new");
    assert_eq!(names(&tmp.path().join("dotfiles")), ["settings.ini"], "nothing left aside");
}

/// What a cm that was killed left in the temp folder is recognised by its name; other
/// folders aren't touched.
#[test]
fn only_cm_private_folders_are_swept() {
    assert_eq!(crate::util::private_dir_owner("cm-1234-session-0123456789abcdef"), Some(1234));
    assert_eq!(crate::util::private_dir_owner("cm-1234-shell-0123456789abcdef"), Some(1234));
    assert_eq!(crate::util::private_dir_owner("rmc-1234-shell-0123456789abcdef"), Some(1234), "from before the new name");
    for other in ["cm-edge-dots-140564-5", "cm-1234-admin-0123456789abcdef", "cm-1234-session-xyz", "cm-panic.txt", "other"] {
        assert_eq!(crate::util::private_dir_owner(other), None, "{other}");
    }
}

/// `con.txt`, `nul` and the like are device names to Windows, but files of these names exist
/// (made by WSL, Linux, or archives): they are copied and deleted as files, without hanging.
#[cfg(windows)]
#[test]
fn files_named_like_devices_are_files_here() {
    let tmp = TempDir::new("edge-devices");
    let dir = tmp.dir("d");
    for name in ["con.txt", "nul", "aux.log", "com1", "prn.md", "lpt1", "con"] {
        fs::write(literal(&dir.join(name)), name).unwrap();
    }
    let out = tmp.path().join("out");
    let (errors, _, _) = run(Job::Copy(vec![(dir.clone(), out.clone())]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snapshot(&out), snapshot(&dir));
    let (errors, _, _) = run(delete(vec![dir.join("nul"), dir.join("con")]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(names(&dir).len(), 5);
    let (errors, _, _) = run(delete(vec![dir.clone(), out.clone()]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!dir.exists() && !out.exists());
}

/// Deleting a folder that holds a link to another folder removes the link, never what the
/// other folder holds; the same when the link itself is deleted.
#[test]
fn a_delete_never_reaches_through_a_link() {
    let tmp = TempDir::new("edge-link-delete");
    tmp.file("outside/keep.txt", "keep");
    tmp.file("outside/sub/deep.txt", "deep");
    let outside = tmp.path().join("outside");
    let before = snapshot(&outside);
    tmp.file("victim/own.txt", "own");
    folder_link(&tmp.path().join("victim").join("link"), &outside);
    tmp.dir("other");
    folder_link(&tmp.path().join("other").join("link"), &outside);

    let (errors, _, _) = run(delete(vec![tmp.path().join("victim"), tmp.path().join("other").join("link")]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!tmp.path().join("victim").exists());
    assert!(names(&tmp.path().join("other")).is_empty());
    assert_eq!(snapshot(&outside), before);
}

/// Moving a folder that holds a link to another folder, on one drive and to another one:
/// the other folder is never emptied.
#[test]
fn a_move_never_empties_what_a_link_leads_to() {
    let tmp = TempDir::new("edge-link-move");
    let other_drive = TempDir::in_target("edge-link-move-dst");
    tmp.file("outside/keep.txt", "keep");
    let outside = tmp.path().join("outside");
    let before = snapshot(&outside);
    for (n, target_root) in [tmp.path(), other_drive.path()].into_iter().enumerate() {
        let src = tmp.path().join(format!("tree{n}"));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("own.txt"), "own").unwrap();
        folder_link(&src.join("link"), &outside);
        let dst = target_root.join(format!("moved{n}"));
        let (errors, cancelled, _) = run(Job::Move(vec![(src.clone(), dst.clone())]), Answer::Skip);
        assert!(!cancelled && errors.is_empty(), "{errors:?}");
        assert_eq!(fs::read_to_string(dst.join("own.txt")).unwrap(), "own");
        assert_eq!(snapshot(&outside), before, "move {n}");
    }
}

/// Folders and files deeper than Windows' old 260-character limit.
#[test]
fn paths_longer_than_260_characters() {
    let tmp = TempDir::new("edge-long");
    let mut deep = PathBuf::from("src");
    for i in 0..10 {
        deep.push(format!("{i:02}-a-rather-long-folder-name-to-go-past-max-path"));
    }
    let file = tmp.file(&deep.join("file.txt").to_string_lossy(), "deep");
    assert!(file.as_os_str().len() > 500);
    let src = tmp.path().join("src");
    let before = snapshot(&src);

    let copy = tmp.path().join("copy");
    let (errors, _, _) = run(Job::Copy(vec![(src.clone(), copy.clone())]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snapshot(&copy), before);

    let moved = tmp.path().join("moved");
    let (errors, _, _) = run(Job::Move(vec![(copy.clone(), moved.clone())]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snapshot(&moved), before);

    let other_drive = TempDir::in_target("edge-long-dst");
    let across = other_drive.path().join("across");
    let (errors, _, _) = run(Job::Move(vec![(moved.clone(), across.clone())]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snapshot(&across), before);
    assert!(!moved.exists());

    let (errors, _, _) = run(delete(vec![src.clone(), across.clone()]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!src.exists() && !across.exists());
}

/// Names that are valid but unusual: emoji, combining accents, 250 characters, spaces,
/// punctuation a shell would trip over.
#[test]
fn unusual_names_survive_copy_move_and_delete() {
    let tmp = TempDir::new("edge-names");
    let long = format!("{}.txt", "x".repeat(246));
    let names_in = [
        "🙂 smile.txt",
        "e\u{301}cole.txt",
        "école.txt",
        long.as_str(),
        " leading space.txt",
        "#hash%20&amp;.txt",
        "'single' \"double\".txt",
        "[brackets] {braces} (parens).txt",
        "semi;colon=equals,comma.txt",
        "$dollar `tick` ~tilde!.txt",
    ];
    let names_in: Vec<&str> =
        names_in.into_iter().filter(|name| cfg!(unix) || !name.contains('"')).collect();
    for name in &names_in {
        tmp.file(&format!("src/{name}"), name);
    }
    let src = tmp.path().join("src");
    let before = snapshot(&src);
    assert_eq!(before.len(), names_in.len());

    let copy = tmp.path().join("copy");
    let (errors, _, _) = run(Job::Copy(vec![(src.clone(), copy.clone())]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snapshot(&copy), before);

    let other_drive = TempDir::in_target("edge-names-dst");
    let across = other_drive.path().join("across");
    let (errors, _, _) = run(Job::Move(vec![(copy.clone(), across.clone())]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snapshot(&across), before);

    let one = src.join(names_in[0]);
    let (errors, _, _) = run(delete(vec![one]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snapshot(&src).len(), names_in.len() - 1);
}

/// Overwriting a read-only file, by a copy and by a move: it is replaced, and takes the
/// new file's flags.
#[test]
fn read_only_files_are_replaced_when_the_user_says_so() {
    let tmp = TempDir::new("edge-readonly");
    let src = tmp.file("a/x.txt", "new");
    let dst = tmp.file("b/x.txt", "old");
    set_readonly(&dst, true);
    let (errors, _, questions) = run(Job::Copy(vec![(src.clone(), dst.clone())]), Answer::Yes);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(questions, 1);
    assert_eq!(fs::read_to_string(&dst).unwrap(), "new");
    assert!(!fs::metadata(&dst).unwrap().permissions().readonly());

    let src = tmp.file("a/y.txt", "new y");
    let dst = tmp.file("b/y.txt", "old y");
    set_readonly(&dst, true);
    let (errors, _, _) = run(Job::Move(vec![(src.clone(), dst.clone())]), Answer::Yes);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(fs::read_to_string(&dst).unwrap(), "new y");
    assert!(!src.exists());

    // And a read-only file moved to another drive, where it is copied, then deleted.
    let other_drive = TempDir::in_target("edge-readonly-dst");
    let src = tmp.file("a/z.txt", "z");
    set_readonly(&src, true);
    let (errors, _, _) = run(Job::Move(vec![(src.clone(), other_drive.path().join("z.txt"))]), Answer::Yes);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!src.exists());
    assert!(fs::metadata(other_drive.path().join("z.txt")).unwrap().permissions().readonly());
}

/// A move onto a folder that holds files of the same names, answered No: what wasn't moved
/// stays where it was, and so does its folder. On one drive and across drives.
#[test]
fn a_merge_answered_no_loses_nothing() {
    for across in [false, true] {
        let tmp = TempDir::new("edge-merge-no");
        let other_drive = TempDir::in_target("edge-merge-no-dst");
        let target_root = if across { other_drive.path() } else { tmp.path() };
        tmp.file("a/x/same.txt", "from a");
        tmp.file("a/x/only-a.txt", "only a");
        tmp.file("a/x/sub/same-deep.txt", "deep a");
        let dst = target_root.join("b/x");
        fs::create_dir_all(dst.join("sub")).unwrap();
        fs::write(dst.join("same.txt"), "from b").unwrap();
        fs::write(dst.join("sub/same-deep.txt"), "deep b").unwrap();
        let src = tmp.path().join("a/x");
        let before = snapshot(&src);
        let dst_before = snapshot(&dst);

        let (errors, cancelled, questions) = run(Job::Move(vec![(src.clone(), dst.clone())]), Answer::No);
        assert!(!cancelled, "{errors:?}");
        assert_eq!(questions, 2, "across: {across}");
        assert_eq!(nothing_lost(&before, &src, &dst), 2, "the two files answered No stay");
        assert_eq!(fs::read_to_string(dst.join("same.txt")).unwrap(), "from b");
        assert_eq!(fs::read_to_string(dst.join("sub/same-deep.txt")).unwrap(), "deep b");
        assert_eq!(fs::read_to_string(dst.join("only-a.txt")).unwrap(), "only a");
        let after: BTreeMap<_, _> = snapshot(&dst).into_iter().filter(|(k, _)| dst_before.contains_key(k)).collect();
        assert_eq!(after, dst_before, "nothing of b changed");
    }
}

/// A file another program holds open without sharing: a delete skips only that file (and
/// the folders holding it), and deletes the rest.
#[cfg(windows)]
#[test]
fn a_file_in_use_stops_only_its_own_delete() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = TempDir::new("edge-in-use");
    let busy = tmp.file("victim/sub/busy.txt", "busy");
    tmp.file("victim/free.txt", "free");
    tmp.file("victim/other/deep.txt", "deep");
    let open = fs::File::options().read(true).share_mode(0).open(&busy).unwrap();
    let victim = tmp.path().join("victim");
    let (errors, cancelled, questions) = run(delete(vec![victim.clone()]), Answer::Skip);
    assert!(!cancelled);
    assert_eq!((errors.len(), questions), (1, 1), "{errors:?}");
    assert_eq!(names(&victim), ["sub"]);
    assert!(busy.exists());
    drop(open);
}

/// A folder whose contents may not be deleted (Linux: no write permission on it): the rest
/// goes, that folder stays with its contents, and one error says why.
#[cfg(unix)]
#[test]
fn a_folder_that_refuses_keeps_only_itself() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new("edge-refuse");
    tmp.file("victim/free.txt", "free");
    let inside = tmp.file("victim/shut/inside.txt", "inside");
    let shut = tmp.path().join("victim/shut");
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::remove_file(&inside).is_ok() {
        return; // root deletes anything
    }
    let victim = tmp.path().join("victim");
    let (errors, cancelled, _) = run(delete(vec![victim.clone()]), Answer::Skip);
    fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!cancelled);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(names(&victim), ["shut"]);
    assert!(inside.exists());
}

/// Moving a tree with a link and a pipe to another file system: the link arrives as a link,
/// the pipe is skipped and stays, so the folder holding it stays too. Nothing is lost.
#[cfg(unix)]
#[test]
fn a_move_to_another_file_system_keeps_what_it_cannot_take() {
    let tmp = TempDir::new("edge-xmove-special");
    let other = TempDir::in_target("edge-xmove-special-dst");
    if util::same_volume(tmp.path(), other.path()) {
        eprintln!("one file system only; not tested");
        return;
    }
    let src = tmp.dir("tree");
    tmp.file("tree/a.txt", "a");
    tmp.file("tree/sub/b.txt", "b");
    std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();
    let fifo = std::ffi::CString::new(src.join("sub/pipe").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    let before = snapshot(&src);
    let dst = other.path().join("tree");
    let (errors, cancelled, _) = run(Job::Move(vec![(src.clone(), dst.clone())]), Answer::Skip);
    assert!(!cancelled);
    assert!(!errors.is_empty(), "the pipe is reported");
    nothing_lost(&before, &src, &dst);
    assert!(src.join("sub/pipe").exists(), "the pipe stays");
    assert_eq!(fs::read_to_string(dst.join("sub/b.txt")).unwrap(), "b");
}

/// Copying a file onto a link to another file (Linux) or a junction (Windows) replaces the
/// link, never what it leads to.
#[cfg(unix)]
#[test]
fn an_overwrite_replaces_a_link_not_its_target() {
    let tmp = TempDir::new("edge-overwrite-link");
    let precious = tmp.file("elsewhere/precious.txt", "precious");
    let src = tmp.file("a/x.txt", "new");
    tmp.dir("b");
    std::os::unix::fs::symlink(&precious, tmp.path().join("b/x.txt")).unwrap();
    let (errors, _, questions) = run(Job::Copy(vec![(src, tmp.path().join("b/x.txt"))]), Answer::Yes);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(questions, 1);
    assert_eq!(fs::read_to_string(&precious).unwrap(), "precious");
    assert_eq!(fs::read_to_string(tmp.path().join("b/x.txt")).unwrap(), "new");
}

/// A folder that respects letter case (WSL makes them on Windows; any folder on Linux):
/// `readme.md` and `README.md` are two files, and a move of one onto the other asks first.
#[test]
fn two_files_differing_in_case_stay_two() {
    let tmp = TempDir::new("edge-case-two");
    let dir = tmp.dir("d");
    #[cfg(windows)]
    {
        let made = std::process::Command::new("fsutil").args(["file", "setCaseSensitiveInfo"]).arg(&dir).arg("enable").output().unwrap();
        if !made.status.success() {
            eprintln!("no case-sensitive folders here; not tested");
            return;
        }
    }
    let lower = tmp.file("d/readme.md", "lower");
    let upper = tmp.file("d/README.md", "upper");
    assert_eq!(names(&dir), ["README.md", "readme.md"]);
    let pairs = ops::plan_move(std::slice::from_ref(&lower), "README.md", &dir).unwrap();
    let (errors, _, questions) = run(Job::Move(pairs), Answer::No);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(questions, 1, "asked before replacing the other file");
    assert_eq!((fs::read_to_string(&lower).unwrap(), fs::read_to_string(&upper).unwrap()), ("lower".into(), "upper".into()));
}

/// Moving a folder onto one that holds the same files under hard links (one file, two
/// names): the file stays, and the emptied source goes, as with any other merge.
#[test]
fn a_merge_through_hard_links_empties_the_source() {
    let tmp = TempDir::new("edge-merge-hardlink");
    let kept = tmp.file("b/x/f.txt", "data");
    tmp.dir("a/x");
    fs::hard_link(&kept, tmp.path().join("a/x/f.txt")).unwrap();
    tmp.file("a/x/other.txt", "other");
    let src = tmp.path().join("a").join("x");
    let (errors, _, questions) = run(Job::Move(vec![(src.clone(), tmp.path().join("b").join("x"))]), Answer::Cancel);
    assert!(errors.is_empty() && questions == 0, "{errors:?}");
    assert!(!src.exists(), "{:?}", names(&src));
    assert_eq!(fs::read_to_string(&kept).unwrap(), "data");
    assert_eq!(fs::read_to_string(tmp.path().join("b/x/other.txt")).unwrap(), "other");
}

/// In a merge, a link to the very file it would replace is left alone: moved over it, the
/// link would take its place and the file would be gone.
#[test]
fn a_merge_never_puts_a_link_over_what_it_leads_to() {
    let tmp = TempDir::new("edge-merge-link");
    let target = tmp.file("b/x/f.txt", "precious");
    tmp.dir("a/x");
    let link = tmp.path().join("a").join("x").join("f.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
        eprintln!("no symbolic links without Developer Mode; not tested");
        return;
    }
    let src = tmp.path().join("a").join("x");
    let (errors, _, _) = run(Job::Move(vec![(src.clone(), tmp.path().join("b").join("x"))]), Answer::Yes);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "precious");
    assert!(fs::symlink_metadata(&target).unwrap().is_file(), "still the file itself");
    assert!(fs::symlink_metadata(&link).is_ok(), "the link stays where it was");
}

/// Alt+F5 into a folder that isn't there yet makes it, as copies do.
#[test]
fn packing_into_a_folder_that_isnt_there_yet() {
    let tmp = TempDir::new("edge-pack-new");
    let src = tmp.file("src/a.txt", "a");
    let dest = tmp.path().join("new").join("deeper").join("out.zip");
    let (errors, _, _) = run(Job::Pack { sources: vec![src], dest: dest.clone() }, Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(fs::metadata(&dest).unwrap().len() > 0);
}

/// A zip made to reach outside: `..` hidden behind a NUL, a drive-relative `C:x`, a stream
/// `a:b`. None of it is listed or written; the honest names around them are.
#[test]
fn a_crafted_zip_stays_inside_the_folder() {
    use std::io::Write;
    let tmp = TempDir::new("edge-zip-slip");
    let zip_path = tmp.path().join("evil.zip");
    let entries = ["fine.txt", "..\0/..\0/escaped.txt", "x/..\0/..\0/escaped2.txt", "../plain.txt", "sub/ok.txt"];
    let mut windows_names = vec!["C:evil.txt", "docs/E:run.bat", "a/G:/photos/x.jpg", "note.txt:hidden"];
    if cfg!(unix) {
        windows_names.clear();
    }
    let mut zip = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
    let options = zip::write::SimpleFileOptions::default();
    for name in entries.iter().chain(&windows_names) {
        zip.start_file(*name, options).unwrap();
        zip.write_all(b"x").unwrap();
    }
    zip.finish().unwrap();

    let archive = crate::archive::Archive::open(&zip_path).unwrap();
    assert!(archive.items.iter().all(|i| !i.path.split('/').any(|p| p == ".." || p.contains(':') || p.contains('\0'))), "{:?}", archive.items);
    assert_eq!(archive.left_out.len(), entries.len() - 2 + windows_names.len(), "{:?}", archive.left_out);
    assert!(archive.left_out_note().is_some_and(|note| note.starts_with("Left out")));

    let dest = tmp.path().join("a").join("b").join("c");
    let items = archive.children("").map(|i| i.path.clone()).collect();
    let (errors, _, _) = run(Job::Extract { archive: zip_path.clone(), items, dest: dest.clone() }, Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(snapshot(&dest).get("fine.txt").map(String::as_str), Some("file x"));
    assert_eq!(snapshot(&dest).get("sub/ok.txt").map(String::as_str), Some("file x"));
    assert_eq!(names(tmp.path()), ["a", "evil.zip"], "nothing beside the folder");
    assert_eq!(names(&tmp.path().join("a")), ["b"]);
    assert_eq!(names(&tmp.path().join("a").join("b")), ["c"]);
    #[cfg(windows)]
    assert!(!Path::new(r"C:evil.txt").exists());
}

/// Packing with a file that can't be read: it alone is missing. The file packed just before
/// it stays in the zip (the zip crate's abort removes the last finished entry if none is
/// under way), however often it is retried.
#[cfg(windows)]
#[test]
fn a_locked_file_costs_a_zip_only_itself() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = TempDir::new("edge-pack-locked");
    let first = tmp.file("src/1-first.txt", "first");
    let locked = tmp.file("src/2-locked.txt", "locked");
    let third = tmp.file("src/3-third.txt", "third");
    let open = fs::File::options().read(true).share_mode(0).open(&locked).unwrap();
    let dest = tmp.path().join("out.zip");
    let running = ops::start(Job::Pack { sources: vec![first, locked, third], dest: dest.clone() });
    let mut retries = 3;
    loop {
        match running.rx.recv_timeout(Duration::from_secs(30)).expect("job hung") {
            Msg::Progress(_) => {}
            Msg::Ask(_, reply) => {
                reply.send(if retries > 0 { Answer::Retry } else { Answer::Skip }).unwrap();
                retries -= 1;
            }
            Msg::Done { .. } => break,
        }
    }
    drop(open);
    let archive = crate::archive::Archive::open(&dest).unwrap();
    let packed: Vec<&str> = archive.items.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(packed, ["1-first.txt", "3-third.txt"]);
}

/// A zip that is there is only replaced by a complete one: with something left out the new
/// one goes beside it, and with nothing packed there is no new zip at all.
#[cfg(windows)]
#[test]
fn an_incomplete_pack_never_replaces_a_zip() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = TempDir::new("edge-pack-keep");
    let good = tmp.file("good.txt", "good");
    let locked = tmp.file("locked.txt", "locked");
    let dest = tmp.path().join("data.zip");
    fs::write(&dest, "the old zip").unwrap();
    let open = fs::File::options().read(true).share_mode(0).open(&locked).unwrap();

    let (errors, _, _) = run(Job::Pack { sources: vec![locked.clone()], dest: dest.clone() }, Answer::Skip);
    assert!(errors.iter().any(|e| e.contains("nothing could be packed")), "{errors:?}");
    assert_eq!(fs::read_to_string(&dest).unwrap(), "the old zip");

    let (errors, _, _) = run(Job::Pack { sources: vec![good, locked], dest: dest.clone() }, Answer::Skip);
    assert!(errors.iter().any(|e| e.contains("stays as it was")), "{errors:?}");
    assert_eq!(fs::read_to_string(&dest).unwrap(), "the old zip");
    let beside = tmp.path().join("data (incomplete).zip");
    assert_eq!(crate::archive::Archive::open(&beside).unwrap().items.len(), 1);
    drop(open);
    assert_eq!(names(tmp.path()).iter().filter(|n| n.ends_with(".part")).count(), 0);
}

/// Links stay links in a zip, and a script stays runnable through a pack and an unpack.
#[cfg(unix)]
#[test]
fn packing_keeps_links_and_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new("edge-pack-unix");
    let script = tmp.file("src/run.sh", "#!/bin/sh\n");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o750)).unwrap();
    std::os::unix::fs::symlink("run.sh", tmp.path().join("src/link")).unwrap();
    let dest = tmp.path().join("out.zip");
    let (errors, _, _) = run(Job::Pack { sources: vec![tmp.path().join("src")], dest: dest.clone() }, Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    let archive = crate::archive::Archive::open(&dest).unwrap();
    assert!(archive.items.iter().any(|i| i.path == "src/link"), "{:?}", archive.items);
    let out = tmp.path().join("out");
    let items = vec!["src".to_string()];
    let (errors, _, _) = run(Job::Extract { archive: dest, items, dest: out.clone() }, Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(fs::metadata(out.join("src/run.sh")).unwrap().permissions().mode() & 0o777, 0o750);
}

/// Where case counts, `README.md` beside the `Readme.md` a job wrote is another file: the job
/// asks about it as about any other.
#[cfg(unix)]
#[test]
fn names_differing_in_case_are_two_files_where_case_counts() {
    let tmp = TempDir::new("edge-case-counts");
    let first = tmp.file("src/Readme.md", "first");
    let second = tmp.file("src/README.md", "second");
    tmp.file("dst/README.md", "old");
    let dst = tmp.path().join("dst");
    let job = Job::Copy(vec![(first, dst.join("Readme.md")), (second, dst.join("README.md"))]);
    let (errors, _, questions) = run(job, Answer::Yes);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(questions, 1);
    assert_eq!(fs::read_to_string(dst.join("Readme.md")).unwrap(), "first");
    assert_eq!(fs::read_to_string(dst.join("README.md")).unwrap(), "second");
}

/// Merged into a folder already there, a move goes item by item: never into what is mounted.
#[cfg(unix)]
#[test]
fn a_merge_keeps_out_of_a_mounted_file_system() {
    let tmp = TempDir::new("edge-merge-mount");
    let inside = tmp.file("chroot/home/user/precious.txt", "precious");
    tmp.file("backup/chroot/home/other.txt", "other");
    let mount = fs::canonicalize(tmp.path().join("chroot/home")).unwrap();
    assert!(ops::merged_with_mounts(&tmp.path().join("chroot"), &tmp.path().join("backup/chroot"), vec![mount]));
    assert_eq!(fs::read_to_string(&inside).unwrap(), "precious");
    assert!(!tmp.path().join("backup/chroot/home/user").exists());
}

/// One file by two ways, which look like two disks: found out before anything is copied, as a
/// copy would replace the file and then "the source" could be deleted. (A hard link stands in
/// for the second mount of a share here: the same inode.)
#[cfg(unix)]
#[test]
fn one_file_by_two_ways_is_left_alone() {
    use std::os::unix::fs::MetadataExt;
    let tmp = TempDir::new("edge-two-mounts");
    let src = tmp.file("a/file.txt", "the only copy");
    let dst = tmp.path().join("file.txt");
    fs::hard_link(&src, &dst).unwrap();
    let inode = fs::metadata(&dst).unwrap().ino();
    let errors = ops::moved_across(&src, &dst);
    assert!(errors.iter().any(|error| error.contains("same file")), "{errors:?}");
    assert_eq!(fs::read_to_string(&src).unwrap(), "the only copy");
    assert_eq!(fs::metadata(&dst).unwrap().ino(), inode, "no copy was made");
}

/// F8 of a link to a mount (or above one) only takes the link: it is no folder holding one.
#[cfg(unix)]
#[test]
fn a_link_to_a_mount_is_only_a_link() {
    let tmp = TempDir::new("edge-link-mount");
    let holder = tmp.dir("holder");
    let mount = fs::canonicalize(tmp.dir("holder/mounted")).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&holder, &link).unwrap();
    let mounts = [mount];
    assert!(ops::mount_in_the_way(&holder, &mounts));
    assert!(!ops::mount_in_the_way(&link, &mounts));
}

/// Where a disk keeps no file ids, the folder's list tells two spellings of one name from two
/// files.
#[test]
fn two_spellings_of_one_name() {
    let tmp = TempDir::new("edge-spellings");
    let file = tmp.file("Readme.md", "x");
    let other_spelling = tmp.path().join("README.md");
    if other_spelling.exists() {
        assert!(util::same_entry(&file, &other_spelling));
        assert!(util::one_spelling(&file, &other_spelling));
    } else {
        fs::write(&other_spelling, "y").unwrap();
        assert!(!util::one_spelling(&file, &other_spelling), "two files");
    }
    assert!(!util::one_spelling(&file, &file), "not two spellings");
}

/// `Makefile` and `makefile` going to a disk that ignores case, or files of one name from two
/// folders pasted into one: the second never replaces the first, whose source is gone.
#[test]
fn two_sources_landing_on_one_name_lose_neither() {
    let tmp = TempDir::new("edge-clash");
    let first = tmp.file("one/same.txt", "first");
    let second = tmp.file("two/same.txt", "second");
    tmp.dir("to");
    let target = tmp.path().join("to").join("same.txt");
    let job = Job::Move(vec![(first.clone(), target.clone()), (second.clone(), target.clone())]);
    let (errors, _, questions) = run(job, Answer::All);
    assert_eq!((errors.len(), questions), (1, 0), "{errors:?}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "first");
    assert_eq!(fs::read_to_string(&second).unwrap(), "second", "still at its source");
}

/// F5 of a link onto the very file it leads to: that file would be gone, with the question
/// showing 0 bytes. Skipped instead.
#[cfg(unix)]
#[test]
fn a_copy_never_puts_a_link_over_what_it_leads_to() {
    let tmp = TempDir::new("edge-copy-link");
    let target = tmp.file("b/f.txt", "precious");
    tmp.dir("a");
    let link = tmp.path().join("a/f.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let (errors, _, questions) = run(Job::Copy(vec![(link, target.clone())]), Answer::All);
    assert_eq!((errors.len(), questions), (1, 0), "{errors:?}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "precious");
    assert!(fs::symlink_metadata(&target).unwrap().is_file());
}

/// On a case-sensitive disk `Notes.txt`, a link to `notes.txt`, isn't that file in other
/// letter case: F6 mustn't take it for a rename and put the link over the file.
#[cfg(unix)]
#[test]
fn a_link_isnt_a_case_rename_of_its_target() {
    let tmp = TempDir::new("edge-case-link");
    let target = tmp.file("notes.txt", "precious");
    std::os::unix::fs::symlink("notes.txt", tmp.path().join("Notes.txt")).unwrap();
    let link = tmp.path().join("Notes.txt");
    assert!(ops::plan_move(std::slice::from_ref(&link), "notes.txt", tmp.path()).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "precious");
    // And a slash at the end means a new folder, not a rename.
    let folder = tmp.dir("Photos");
    let pairs = ops::plan_move(std::slice::from_ref(&folder), "photos/", tmp.path()).unwrap();
    assert_eq!(pairs[0].1, tmp.path().join("photos").join("Photos"));
}

/// Mode and contents on Linux: a private key stays private in its copy, from the start; a
/// private folder too, also when it arrives by a move to another file system.
#[cfg(unix)]
#[test]
fn private_files_and_folders_stay_private() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let tmp = TempDir::new("edge-modes");
    let other = TempDir::in_target("edge-modes-dst");
    let key = tmp.file("secret/id_rsa", "key");
    let folder = tmp.path().join("secret");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
    let copy = tmp.path().join("copy");
    let (errors, _, _) = run(Job::Copy(vec![(folder.clone(), copy.clone())]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!((mode(&copy), mode(&copy.join("id_rsa"))), (0o700, 0o600));
    // Read-only folders are copied too, their files written first.
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o500)).unwrap();
    let again = tmp.path().join("again");
    let (errors, _, _) = run(Job::Copy(vec![(folder.clone(), again.clone())]), Answer::Skip);
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(mode(&again), 0o500);
    fs::set_permissions(&again, fs::Permissions::from_mode(0o700)).unwrap();
    if crate::util::same_file(tmp.path(), other.path()) || util::same_volume(tmp.path(), other.path()) {
        return;
    }
    let moved = other.path().join("moved");
    let (errors, _, _) = run(Job::Move(vec![(folder.clone(), moved.clone())]), Answer::Skip);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!folder.exists());
    assert_eq!(fs::read_to_string(moved.join("id_rsa")).unwrap(), "key");
}

/// On Windows the hidden and read-only flags come along, on files and on folders; folders keep
/// their time on both systems.
#[test]
fn flags_and_folder_times_come_along() {
    let tmp = TempDir::new("edge-looks");
    let inner = tmp.file("src/inner/file.txt", "x");
    let folder = tmp.path().join("src").join("inner");
    let old = std::time::SystemTime::now() - Duration::from_secs(86_400 * 30);
    crate::lfs::set_folder_time(&folder, old).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        for path in [&inner, &folder] {
            let hidden = std::process::Command::new("attrib").arg("+h").arg(path).status().unwrap();
            assert!(hidden.success());
        }
        let copy = tmp.path().join("copy");
        let (errors, _, _) = run(Job::Copy(vec![(tmp.path().join("src"), copy.clone())]), Answer::Skip);
        assert!(errors.is_empty(), "{errors:?}");
        for path in [copy.join("inner"), copy.join("inner").join("file.txt")] {
            assert_eq!(fs::metadata(&path).unwrap().file_attributes() & 2, 2, "{}", path.display());
        }
        assert_eq!(fs::metadata(copy.join("inner")).unwrap().modified().unwrap(), old);
    }
    #[cfg(unix)]
    {
        let _ = &inner;
        let copy = tmp.path().join("copy");
        let (errors, _, _) = run(Job::Copy(vec![(tmp.path().join("src"), copy.clone())]), Answer::Skip);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(fs::metadata(copy.join("inner")).unwrap().modified().unwrap(), old);
    }
}

/// Several items onto one file: said at once, not as a string of system errors.
#[test]
fn several_items_cant_go_onto_a_file() {
    let tmp = TempDir::new("edge-onto-file");
    let (a, b) = (tmp.file("a.txt", "a"), tmp.file("b.txt", "b"));
    let error = ops::plan(&[a, b], "c.txt", tmp.path()).err();
    assert_eq!(error, None, "c.txt isn't there: a folder of that name is made");
    tmp.file("c.txt", "c");
    let (a, b) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
    let error = ops::plan(&[a, b], "c.txt", tmp.path()).unwrap_err();
    assert!(error.contains("is a file"), "{error}");
}

/// `"D:\New folder\"` in quotes means a folder as it does without them.
#[test]
fn a_quoted_folder_with_a_slash_at_the_end() {
    let tmp = TempDir::new("edge-quoted");
    let a = tmp.file("a.txt", "a");
    let typed = format!("\"{}\"", tmp.path().join("New folder").join("").display());
    let pairs = ops::plan(std::slice::from_ref(&a), &typed, tmp.path()).unwrap();
    assert_eq!(pairs[0].1, tmp.path().join("New folder").join("a.txt"));
}

/// F5 of a folder holding a junction to the place it is copied to: not copied into itself.
#[cfg(windows)]
#[test]
fn a_junction_to_the_destination_isnt_followed_round() {
    let tmp = TempDir::new("edge-junction-dest");
    tmp.file("src/own.txt", "own");
    let backup = tmp.dir("backup");
    junction(&tmp.path().join("src").join("to-backup"), &backup);
    let dst = backup.join("src");
    let (errors, cancelled, _) = run(Job::Copy(vec![(tmp.path().join("src"), dst.clone())]), Answer::Skip);
    assert!(!cancelled);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(names(&dst), ["own.txt"]);
    fs::remove_dir(tmp.path().join("src").join("to-backup")).unwrap();
}

/// A permanent delete keeps out of what is mounted in a folder (here told by the mount list,
/// as a bind mount is), and goes on with the rest.
#[cfg(unix)]
#[test]
fn deleting_keeps_out_of_a_mounted_file_system() {
    let tmp = TempDir::new("edge-mount");
    tmp.file("victim/own.txt", "own");
    let inside = tmp.file("victim/mnt/server-file.txt", "on the server");
    let mount = fs::canonicalize(tmp.path().join("victim/mnt")).unwrap();
    assert!(ops::deleted_with_mounts(&tmp.path().join("victim"), vec![mount]));
    assert_eq!(fs::read_to_string(&inside).unwrap(), "on the server");
    assert_eq!(names(&tmp.path().join("victim")), ["mnt"]);
}

/// F6 that only changes the letter case of a name renames it (Windows ignores case, so the
/// two names are one file there).
#[test]
fn a_rename_that_only_changes_case() {
    let tmp = TempDir::new("edge-case-rename");
    let file = tmp.file("readme.md", "text");
    let folder = tmp.dir("Folder");
    tmp.file("Folder/inside.txt", "inside");
    for (source, new_name) in [(file, "README.md"), (folder, "folder")] {
        let pairs = ops::plan_move(std::slice::from_ref(&source), new_name, tmp.path()).unwrap();
        let (errors, _, questions) = run(Job::Move(pairs), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(questions, 0, "nothing to ask: it's the same file");
        assert!(names(tmp.path()).contains(&new_name.to_string()), "{:?}", names(tmp.path()));
    }
    assert_eq!(fs::read_to_string(tmp.path().join("folder/inside.txt")).unwrap(), "inside");
    // A copy onto itself in another case is still refused.
    let again = tmp.path().join("README.md");
    assert!(ops::plan(std::slice::from_ref(&again), "readme.md", tmp.path()).is_err() || cfg!(unix));
}
