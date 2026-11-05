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
#[cfg(unix)]
use crate::util;

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
fn folder_link(link: &Path, target: &Path) {
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
