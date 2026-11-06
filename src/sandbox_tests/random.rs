//! Random trees through the real jobs. Each round grows a source and a destination from a small
//! set of names, so that they collide: odd names, read-only files, long paths, links out of the
//! trees and links from the destination back to its twin in the source; on Windows some files are
//! held open by "another program". It runs a copy or a move with a way of answering, then a pack
//! and an unpack, then a delete, and holds the whole disk to what must be true whatever the job
//! did: nothing lost, nothing outside touched, nothing left behind, nothing silently skipped.
//!
//! A failure names its seed, which replays it: `RMC_SEED=<seed> RMC_ROUNDS=1 cargo test random_`.
//! For a long run, `RMC_ROUNDS=2000`.

use std::path::MAIN_SEPARATOR;
use std::sync::atomic::Ordering;

use super::*;
use crate::ops::Question;
use crate::ops::remote::{self, Place};
use crate::ssh::{self, Connection};
use std::sync::Arc;

type Tree = BTreeMap<String, String>;

/// Dice that can be thrown again.
struct Dice(u64);

impl Dice {
    fn new(seed: u64) -> Self {
        let mut dice = Dice(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        dice.roll(2);
        dice
    }

    fn roll(&mut self, sides: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 20) as usize % sides
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.roll(100) < percent
    }

    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[self.roll(from.len())]
    }
}

const NAMES: &[&str] = &[
    "a", "b", "c", "data.txt", "Readme.md", "README.md", ".hidden", "with space", " leading", "🙂", "école", "e\u{301}cole",
    "100%", "[x]", "a&b", "x,y;z", "'q'", "~tmp", "$HOME", "%PATH%", "dots.tar.gz", "..foo", "-rf",
];

/// Picked often, so that the two trees meet.
const COLLIDING: &[&str] = &["a", "b", "data.txt", "Readme.md"];

#[cfg(windows)]
const ODD_NAMES: &[&str] = &["foo.", "foo", "bar ", "bar", "nul", "aux.c", "con.txt", "com1", "...", "trailing. "];

#[cfg(unix)]
const ODD_NAMES: &[&str] = &["foo.", "bar ", "nul", "...", "back\\slash", "new\nline", "tab\tname", "*", "?", "\"quoted\"", "a:b"];

/// The way a job's questions are answered.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Way {
    Yes,
    No,
    All,
    None,
    Mixed,
    /// Yes, until Cancel at this question.
    CancelAt(usize),
    /// Yes, and the job is stopped at this progress report.
    StopAfter(usize),
}

impl Way {
    fn random(dice: &mut Dice) -> Way {
        match dice.roll(8) {
            0 => Way::Yes,
            1 => Way::No,
            2 | 3 => Way::All,
            4 => Way::None,
            5 => Way::Mixed,
            6 => Way::CancelAt(1 + dice.roll(3)),
            _ => Way::StopAfter(1 + dice.roll(3)),
        }
    }

    fn may_replace(self) -> bool {
        !matches!(self, Way::No | Way::None)
    }
}

/// Runs a job to the end, answering its way: what went wrong (in errors or in questions), whether
/// it was cancelled, and how many overwrites it asked about.
fn drive(job: Job, way: Way, dice: &mut Dice) -> (Vec<String>, bool, usize) {
    let running = ops::start(job);
    let (mut asked, mut reports, mut failures) = (0, 0, Vec::new());
    loop {
        match running.rx.recv_timeout(Duration::from_secs(60)).expect("job hung") {
            Msg::Progress(_) => {
                reports += 1;
                if way == Way::StopAfter(reports) {
                    running.cancel.store(true, Ordering::Relaxed);
                }
            }
            Msg::Ask(Question::Failed { path, error, .. }, reply) => {
                failures.push(format!("{}: {error}", path.display()));
                let answer = match way {
                    Way::Mixed => [Answer::Skip, Answer::SkipAll, Answer::Retry][dice.roll(3)],
                    _ => Answer::Skip,
                };
                reply.send(answer).unwrap();
            }
            Msg::Ask(Question::Overwrite { .. }, reply) => {
                asked += 1;
                let answer = match way {
                    Way::Yes | Way::StopAfter(_) => Answer::Yes,
                    Way::No => Answer::No,
                    Way::All => Answer::All,
                    Way::None => Answer::None,
                    Way::Mixed => [Answer::Yes, Answer::No, Answer::All, Answer::None][dice.roll(4)],
                    Way::CancelAt(at) if asked >= at => Answer::Cancel,
                    Way::CancelAt(_) => Answer::Yes,
                };
                reply.send(answer).unwrap();
            }
            Msg::Done { mut errors, cancelled, .. } => {
                errors.append(&mut failures);
                return (errors, cancelled, asked);
            }
        }
    }
}

/// The path to reach `path` by from here: literal where Windows needs it.
fn at(path: &Path) -> PathBuf {
    crate::lfs::literal(path).into_owned()
}

/// Where cmd's `mklink` can be trusted with a path.
#[cfg(windows)]
fn mklink_takes(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.len() < 200 && !crate::lfs::needs_literal(path) && text.chars().all(|c| c.is_ascii_alphanumeric() || r"\:._- ".contains(c))
}

/// Grows a tree.
struct Garden<'a> {
    dice: &'a mut Dice,
    root: PathBuf,
    /// What a file holds: its tree's name and a number, so that no two files are alike.
    tag: &'static str,
    serial: usize,
    outside: PathBuf,
    /// The tree whose places this one's links may lead to, by the same relative path.
    twin: Option<PathBuf>,
    /// `mklink` takes a while.
    #[cfg_attr(unix, allow(dead_code))]
    junctions_left: usize,
    one_spelling: bool,
    /// Only names every server can reach.
    plain: bool,
}

impl Garden<'_> {
    fn name(&mut self, depth: usize) -> String {
        let name = match self.dice.roll(10) {
            0 | 1 if !self.plain => self.dice.pick(ODD_NAMES).to_string(),
            2 if depth < 3 => format!("{}{depth}", "l".repeat(100)),
            3 | 4 | 5 => self.dice.pick(COLLIDING).to_string(),
            _ => self.dice.pick(NAMES).to_string(),
        };
        if self.one_spelling && name == "README.md" { "Readme.md".into() } else { name }
    }

    fn grow(&mut self, dir: &Path, depth: usize) {
        for _ in 0..3 + self.dice.roll(5) {
            let name = self.name(depth);
            let path = dir.join(&name);
            if fs::symlink_metadata(at(&path)).is_ok() {
                continue;
            }
            match self.dice.roll(12) {
                0..=6 => self.file(&path),
                7..=9 if depth < 4 && fs::create_dir(at(&path)).is_ok() => self.grow(&path, depth + 1),
                7..=9 => self.file(&path),
                10 => self.link_out(dir),
                _ => self.link_to_twin(&path),
            }
        }
    }

    /// Puts something beside much of what `from` holds, under the same names: files over files,
    /// folders to merge, and now and then a file where a folder is and the other way round.
    fn echo(&mut self, from: &Path, to: &Path) {
        for source in top_level(from) {
            let path = to.join(source.file_name().unwrap());
            let Ok(meta) = fs::symlink_metadata(at(&source)) else { continue };
            if fs::symlink_metadata(at(&path)).is_ok() || self.dice.chance(35) {
                continue;
            }
            let other_kind = self.dice.chance(15);
            if meta.is_dir() != other_kind {
                if fs::create_dir(at(&path)).is_ok() && !other_kind {
                    self.echo(&source, &path);
                }
            } else {
                self.file(&path);
            }
        }
    }

    fn file(&mut self, path: &Path) {
        self.serial += 1;
        let mut text = format!("{} {}", self.tag, self.serial);
        if self.dice.chance(4) {
            text.push_str(&"x".repeat(300_000));
        }
        if fs::write(at(path), &text).is_ok() && self.dice.chance(12) {
            set_readonly(&at(path), true);
        }
    }

    /// A link leading out of the trees, under a name no file or folder has here.
    fn link_out(&mut self, dir: &Path) {
        let link = dir.join(format!("lnk{}", self.dice.roll(3)));
        if fs::symlink_metadata(at(&link)).is_ok() {
            return;
        }
        #[cfg(windows)]
        if self.junctions_left > 0 && mklink_takes(&link) {
            self.junctions_left -= 1;
            junction(&link, &self.outside.join("deep"));
        }
        #[cfg(unix)]
        {
            let target = match self.dice.roll(4) {
                0 => self.outside.clone(),
                1 => self.outside.join("one.txt"),
                2 => PathBuf::from("nowhere"),
                _ => PathBuf::from(".."),
            };
            std::os::unix::fs::symlink(target, &link).unwrap();
        }
    }

    /// A link to the same place in the other tree, which makes the two one thing.
    fn link_to_twin(&mut self, path: &Path) {
        let twin = self.twin.as_ref().map(|root| root.join(path.strip_prefix(&self.root).unwrap()));
        let Some((twin, meta)) = twin.and_then(|t| fs::symlink_metadata(at(&t)).ok().map(|m| (t, m))) else {
            return self.file(path);
        };
        #[cfg(windows)]
        let made = {
            let can = meta.is_dir() && self.junctions_left > 0 && mklink_takes(path) && mklink_takes(&twin);
            if can {
                self.junctions_left -= 1;
                junction(path, &twin);
            }
            can
        };
        #[cfg(unix)]
        let made = {
            let _ = meta;
            std::os::unix::fs::symlink(&twin, path).is_ok()
        };
        if !made {
            self.file(path);
        }
    }
}

/// The folders of a round under short names, so a tree can name all of them; then other ways
/// to reach them, by a link.
struct Places(Vec<(&'static str, PathBuf)>, Vec<(&'static str, PathBuf)>);

impl Places {
    /// All of it: each file's content, each link's target, each folder, by a path such as
    /// `dst/a/b`, in lower case where case doesn't count.
    fn look(&self) -> Tree {
        let mut all = Tree::new();
        for (label, root) in &self.0 {
            for (path, value) in snapshot(root) {
                all.insert(folded(&format!("{label}/{path}")), value);
            }
        }
        all
    }

    fn key(&self, path: &Path) -> String {
        for (label, root) in self.0.iter().chain(&self.1) {
            if let Ok(rest) = path.strip_prefix(root) {
                // As `snapshot` writes them.
                let rest = rest.to_string_lossy().replace('\\', "/");
                return folded(format!("{label}/{rest}").trim_end_matches('/'));
            }
        }
        panic!("{} is outside the round", path.display());
    }
}

/// A relative path with `/` between its parts. On Linux a backslash is part of a name.
fn slashed(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) { text.replace('\\', "/") } else { text.into_owned() }
}

fn folded(path: &str) -> String {
    if cfg!(windows) { path.to_lowercase() } else { path.to_string() }
}

/// What is below `key` in `tree`, itself too.
fn within<'a>(key: &'a str, tree: &'a Tree) -> impl Iterator<Item = (&'a String, &'a String)> {
    let below = format!("{key}/");
    tree.iter().filter(move |(k, _)| *k == key || k.starts_with(&below))
}

/// Where a job's pairs take `key`.
fn taken_to(key: &str, pairs: &[(String, String)]) -> Option<String> {
    pairs.iter().find_map(|(from, to)| {
        if key == from {
            return Some(to.clone());
        }
        key.strip_prefix(from.as_str()).and_then(|rest| rest.starts_with('/').then(|| format!("{to}{rest}")))
    })
}

/// Whether the place `key`, at a destination, was reached through a link among what the job
/// took: on Windows a copy of a junction is a folder with what the junction leads to.
fn through_a_link(key: &str, pairs: &[(String, String)], before: &Tree) -> bool {
    let is_link = |path: &str| before.get(path).is_some_and(|value| value.starts_with("link"));
    pairs.iter().any(|(from, to)| {
        let rest = if key == to { "" } else { key.strip_prefix(to.as_str()).filter(|rest| rest.starts_with('/')).unwrap_or("?") };
        if rest == "?" {
            return false;
        }
        let mut place = from.clone();
        is_link(&place)
            || rest.split('/').filter(|part| !part.is_empty()).any(|part| {
                place = format!("{place}/{part}");
                is_link(&place)
            })
    })
}

fn short(value: Option<&String>) -> String {
    value.map_or("nothing".into(), |value| value.chars().take(40).collect())
}

/// What must be true after a copy or a move.
fn check_job(failed: &dyn Fn(String), moving: bool, way: Way, pairs: &[(String, String)], errors: &[String], cancelled: bool, before: &Tree, after: &Tree) {
    let came_from: BTreeMap<String, &String> = before.keys().filter_map(|key| taken_to(key, pairs).map(|to| (to, key))).collect();
    for (key, value) in before {
        let now = after.get(key);
        if now == Some(value) {
            continue;
        }
        let target = taken_to(key, pairs);
        if let (true, Some(target)) = (moving, &target) {
            let there = after.get(target);
            let kept = value == "folder" || there == Some(value) || value.starts_with("link") && there.is_some_and(|v| v == "folder");
            if kept {
                continue;
            }
            if !(way.may_replace() && came_from.contains_key(key)) {
                failed(format!("{key} ({}) is neither where it was nor at {target}: {}", short(Some(value)), short(there)));
            }
        }
        if let (true, Some(from)) = (way.may_replace(), came_from.get(key)) {
            if now == before.get(*from) || through_a_link(key, pairs, before) {
                continue;
            }
        }
        failed(format!("{key} changed from {} to {}", short(Some(value)), short(now)));
    }
    for key in after.keys().filter(|key| !before.contains_key(*key)) {
        if !came_from.contains_key(key) && !through_a_link(key, pairs, before) {
            failed(format!("{key} appeared, and is nothing the job brought: {}", short(after.get(key))));
        }
    }
    if way == Way::All && errors.is_empty() && !cancelled {
        for (key, value) in before {
            let Some(target) = taken_to(key, pairs) else { continue };
            if !through_a_link(&target, pairs, before) && after.get(&target) != Some(value) {
                failed(format!("no error, yet {key} ({}) didn't reach {target}: {}", short(Some(value)), short(after.get(&target))));
            }
            if moving && value != "folder" && after.contains_key(key) {
                failed(format!("no error, yet {key} is still there"));
            }
        }
    }
}

fn top_level(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(at(dir)) else { return Vec::new() };
    let mut names: Vec<_> = entries.flatten().map(|entry| entry.file_name()).collect();
    names.sort();
    names.into_iter().map(|name| dir.join(name)).collect()
}

fn is_real_folder(path: &Path) -> bool {
    fs::symlink_metadata(at(path)).is_ok_and(|meta| meta.is_dir())
}

/// Whether the disk holding `dir` takes two spellings of a name for one (Linux's /mnt/e).
#[cfg(unix)]
fn ignores_case(dir: &Path) -> bool {
    let probe = dir.join("CaseProbe");
    fs::write(&probe, "").unwrap();
    let ignores = dir.join("caseprobe").exists();
    fs::remove_file(&probe).unwrap();
    ignores
}

/// A few of the files below `dir`, not through links.
#[cfg(windows)]
fn some_files(dice: &mut Dice, dir: &Path, out: &mut Vec<PathBuf>) {
    for path in top_level(dir) {
        if is_real_folder(&path) {
            some_files(dice, &path, out);
        } else if dice.chance(8) {
            out.push(path);
        }
    }
}

/// Files held open without sharing, as another program may, for as long as they are kept.
fn held_open(dice: &mut Dice, trees: [&Path; 2]) -> Vec<fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let mut files = Vec::new();
        if dice.chance(35) {
            trees.iter().for_each(|tree| some_files(dice, tree, &mut files));
        }
        files.iter().filter_map(|path| fs::File::options().read(true).share_mode(0).open(at(path)).ok()).collect()
    }
    #[cfg(unix)]
    {
        let _ = (dice, trees);
        Vec::new()
    }
}

/// What a round did, for the counts at the end.
#[derive(Default)]
struct Counts {
    /// What errors said, without the paths.
    errors: BTreeMap<String, usize>,
    jobs: usize,
    questions: usize,
    refused: usize,
    with_errors: usize,
    cancelled: usize,
}

fn round(seed: u64, counts: &mut Counts) {
    let dice = &mut Dice::new(seed);
    let near = TempDir::new("random");
    let far = TempDir::in_target("random");
    let src = near.dir("src");
    let outside = near.dir("outside");
    near.file("outside/one.txt", "one");
    near.file("outside/deep/two.txt", "two");
    let across = dice.chance(40);
    let dst = if across { far.dir("dst") } else { near.dir("dst") };
    let spare = near.dir("spare");
    let mut places = Places(vec![("src", src.clone()), ("dst", dst.clone()), ("outside", outside.clone())], Vec::new());
    // Here `README.md` beside `Readme.md` would come out as one name, under either spelling.
    #[cfg(windows)]
    let one_spelling = false;
    #[cfg(unix)]
    let one_spelling = ignores_case(&dst);

    let mut garden = Garden { dice, root: src.clone(), tag: "src", serial: 0, outside: outside.clone(), twin: None, junctions_left: 0, one_spelling, plain: false };
    garden.grow(&src, 0);
    let mut garden = Garden { dice, root: dst.clone(), tag: "dst", serial: 0, outside: outside.clone(), twin: Some(src.clone()), junctions_left: 3, one_spelling, plain: false };
    garden.echo(&src, &dst);
    garden.grow(&dst, 0);

    // A copy or a move.
    let moving = dice.chance(50);
    let way = Way::random(dice);
    let mut chosen: Vec<PathBuf> = top_level(&src).into_iter().filter(|_| dice.chance(60)).collect();
    let shape = dice.roll(14);
    if shape < 4 {
        chosen.truncate(1);
    }
    if let Some(first) = chosen.first().cloned() {
        let unchosen = top_level(&src).into_iter().find(|path| !chosen.contains(path) && is_real_folder(path));
        let input = match shape {
            0 | 1 => format!("{}{MAIN_SEPARATOR}new name", dst.display()),
            2 => format!("{}{MAIN_SEPARATOR}beside it", src.display()),
            3 => format!("{}{MAIN_SEPARATOR}inner", first.display()),
            // Into a folder next to them, into the first of them, and by a link to the destination.
            4 if unchosen.is_some() => format!("{}{MAIN_SEPARATOR}", unchosen.unwrap().display()),
            5 => format!("{}{MAIN_SEPARATOR}", first.display()),
            6 => {
                let via = near.path().join("via");
                folder_link(&via, &dst);
                places.1.push(("dst", via.clone()));
                format!("{}{MAIN_SEPARATOR}", via.display())
            }
            _ => format!("{}{MAIN_SEPARATOR}", dst.display()),
        };
        let before = places.look();
        let locks = held_open(dice, [&src, &dst]);
        let plan = if moving { ops::plan_move(&chosen, &input, &src) } else { ops::plan(&chosen, &input, &src) };
        match plan {
            Err(_) => counts.refused += 1,
            Ok(pairs) => {
                let job = if moving { Job::Move(pairs.clone()) } else { Job::Copy(pairs.clone()) };
                let (errors, cancelled, asked) = drive(job, way, dice);
                drop(locks);
                let pairs: Vec<(String, String)> = pairs.iter().map(|(from, to)| (places.key(from), places.key(to))).collect();
                let failed = |what: String| {
                    panic!(
                        "seed {seed}: {what}\n{} across {across}, {way:?}, cancelled {cancelled}\npairs {pairs:#?}\nerrors {errors:#?}",
                        if moving { "move" } else { "copy" }
                    )
                };
                check_job(&failed, moving, way, &pairs, &errors, cancelled, &before, &places.look());
                counts.jobs += 1;
                for error in &errors {
                    let said = error.rsplit(": ").next().unwrap_or(error);
                    *counts.errors.entry(said.chars().take(70).collect()).or_default() += 1;
                }
                counts.questions += asked;
                counts.with_errors += !errors.is_empty() as usize;
                counts.cancelled += cancelled as usize;
            }
        }
    }

    // Packing changes nothing, and what comes out is what went in.
    let chosen: Vec<PathBuf> = top_level(&src).into_iter().filter(|_| dice.chance(60)).collect();
    if !chosen.is_empty() {
        let before = places.look();
        let archive = spare.join("packed.zip");
        let (mut errors, cancelled, _) = drive(Job::Pack { sources: chosen.clone(), dest: archive.clone() }, Way::All, dice);
        assert!(!cancelled, "seed {seed}");
        assert!(places.look() == before, "seed {seed}: packing changed something");
        let out = spare.join("out");
        if let Ok(zip) = crate::archive::Archive::open(&archive) {
            // As the panel lists them, which is by the zip's names.
            let mut items: Vec<String> = zip.items.iter().map(|item| item.path.split('/').next().unwrap().to_string()).collect();
            items.dedup();
            let (more, ..) = drive(Job::Extract { archive: archive.clone(), items, dest: out.clone() }, Way::All, dice);
            errors.extend(more);
        }
        let came_out: Tree = snapshot(&out).into_iter().map(|(path, value)| (folded(&format!("src/{path}")), value)).collect();
        for path in &chosen {
            let key = places.key(path);
            let pairs = [(key.clone(), key.clone())];
            for (inside, value) in within(&key, &before).filter(|(_, value)| value.starts_with("file")) {
                if !through_a_link(inside, &pairs, &before) && came_out.get(inside) != Some(value) {
                    assert!(!errors.is_empty(), "seed {seed}: {inside} didn't come out of the zip, and nothing said so");
                }
            }
        }
    }

    // A delete takes what it was given and nothing else.
    let mut chosen: Vec<PathBuf> = top_level(&src).into_iter().filter(|_| dice.chance(50)).collect();
    let folders: Vec<PathBuf> = top_level(&dst).into_iter().filter(|path| is_real_folder(path)).collect();
    if !folders.is_empty() {
        let folder = folders[dice.roll(folders.len())].clone();
        chosen.extend(top_level(&folder).into_iter().filter(|_| dice.chance(30)));
    }
    if !chosen.is_empty() {
        let before = places.look();
        let keys: Vec<String> = chosen.iter().map(|path| places.key(path)).collect();
        let expected: Tree = before
            .iter()
            .filter(|(key, _)| !keys.iter().any(|gone| *key == gone || key.starts_with(&format!("{gone}/"))))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let (errors, cancelled, _) = drive(delete(chosen), Way::Yes, dice);
        assert!(errors.is_empty() && !cancelled, "seed {seed}: delete: {errors:#?}");
        let after = places.look();
        if after != expected {
            let gone: Vec<&String> = expected.keys().filter(|key| !after.contains_key(*key)).collect();
            let stayed: Vec<&String> = after.keys().filter(|key| !expected.contains_key(*key)).collect();
            panic!("seed {seed}: delete of {keys:#?}\nalso took {gone:#?}\nleft {stayed:#?}");
        }
    }
}

/// `path`, which is in the folder `home`-on-this-disk stands for: on this computer, or the same
/// file as the server sees it. Both ends of a job are on this disk, so the whole of it can be
/// checked afterwards as ever.
fn seen(path: &Path, root: &Path, connection: &Arc<Connection>, there: bool) -> Place {
    if !there {
        return Place::Here(path.to_path_buf());
    }
    Place::There(connection.clone(), ssh::join(&connection.home, &slashed(path.strip_prefix(root).unwrap())))
}

/// The disk path of a place of a round.
fn on_disk(place: &Place, root: &Path, connection: &Arc<Connection>) -> PathBuf {
    match place {
        Place::Here(path) => path.clone(),
        Place::There(_, path) => root.join(path.strip_prefix(&ssh::join(&connection.home, "")).unwrap_or(path)),
    }
}

/// A round with an end of the job, or both, on "another computer": a real server program
/// started in the round's folder. Trees, answers and what must hold after are those of `round`.
fn round_over_ssh(seed: u64, counts: &mut Counts) {
    let dice = &mut Dice::new(seed);
    let near = TempDir::new("random-ssh");
    let Some(connection) = ssh::tests::local_connection(near.path()) else { return };
    let src = near.dir("src");
    let dst = near.dir("dst");
    let outside = near.dir("outside");
    near.file("outside/one.txt", "one");
    near.file("outside/deep/two.txt", "two");
    let places = Places(vec![("src", src.clone()), ("dst", dst.clone()), ("outside", outside.clone())], Vec::new());
    // Git's server on Windows can't reach odd names, nor read them back.
    let plain = cfg!(windows);
    #[cfg(windows)]
    let one_spelling = false;
    #[cfg(unix)]
    let one_spelling = ignores_case(&dst);
    let mut garden = Garden { dice, root: src.clone(), tag: "src", serial: 0, outside: outside.clone(), twin: None, junctions_left: 0, one_spelling, plain };
    garden.grow(&src, 0);
    let mut garden = Garden { dice, root: dst.clone(), tag: "dst", serial: 0, outside: outside.clone(), twin: Some(src.clone()), junctions_left: 0, one_spelling, plain };
    garden.echo(&src, &dst);
    garden.grow(&dst, 0);

    // Which ends are over there: here to there, there to here, or there to there.
    let (from_there, to_there) = [(false, true), (true, false), (true, true)][dice.roll(3)];
    let moving = dice.chance(50);
    let way = Way::random(dice);
    let root = near.path();
    let mut chosen: Vec<PathBuf> = top_level(&src).into_iter().filter(|_| dice.chance(60)).collect();
    let shape = dice.roll(10);
    if shape < 3 {
        chosen.truncate(1);
    }
    if let Some(first) = chosen.first().cloned() {
        // Here both ends are one disk. Into itself across computers nobody can see that, and it
        // goes down to the depth limit and says so; in life that takes a folder mounted here
        // and moved into itself from over there.
        let shape = if from_there != to_there && (3..=4).contains(&shape) { 5 } else { shape };
        let (dest, as_folder) = match shape {
            0 | 1 => (dst.join("new name"), false),
            2 => (src.join("beside it"), false),
            3 => (first.join("inner"), false),
            4 => (first.clone(), true),
            _ => (dst.clone(), true),
        };
        let sources: Vec<Place> = chosen.iter().map(|path| seen(path, root, &connection, from_there)).collect();
        let dest = seen(&dest, root, &connection, to_there);
        let before = places.look();
        match remote::plan(&sources, &dest, as_folder) {
            Err(_) => counts.refused += 1,
            Ok(pairs) => {
                let job = Job::Transfer { pairs: pairs.clone(), moving, replace: false };
                let (errors, cancelled, asked) = drive(job, way, dice);
                let pairs: Vec<(String, String)> = pairs
                    .iter()
                    .map(|(from, to)| (places.key(&on_disk(from, root, &connection)), places.key(&on_disk(to, root, &connection))))
                    .collect();
                let failed = |what: String| {
                    panic!(
                        "seed {seed}: {what}\n{} {from_there} to {to_there}, {way:?}, cancelled {cancelled}\npairs {pairs:#?}\nerrors {errors:#?}",
                        if moving { "move" } else { "copy" }
                    )
                };
                check_job(&failed, moving, way, &pairs, &errors, cancelled, &before, &places.look());
                counts.jobs += 1;
                for error in &errors {
                    let said = error.rsplit(": ").next().unwrap_or(error);
                    *counts.errors.entry(said.chars().take(70).collect()).or_default() += 1;
                }
                counts.questions += asked;
                counts.with_errors += !errors.is_empty() as usize;
                counts.cancelled += cancelled as usize;
            }
        }
    }

    // A delete over there takes what it was given and nothing else.
    let chosen: Vec<PathBuf> = top_level(&src).into_iter().filter(|_| dice.chance(50)).collect();
    if !chosen.is_empty() {
        let before = places.look();
        let keys: Vec<String> = chosen.iter().map(|path| places.key(path)).collect();
        let expected: Tree = before
            .iter()
            .filter(|(key, _)| !keys.iter().any(|gone| *key == gone || key.starts_with(&format!("{gone}/"))))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let paths: Vec<String> = chosen
            .iter()
            .map(|path| match seen(path, root, &connection, true) {
                Place::There(_, path) => path,
                Place::Here(_) => unreachable!(),
            })
            .collect();
        let (errors, cancelled, _) = drive(Job::DeleteThere { connection: connection.clone(), paths }, Way::Yes, dice);
        assert!(errors.is_empty() && !cancelled, "seed {seed}: delete: {errors:#?}");
        let after = places.look();
        if after != expected {
            let gone: Vec<&String> = expected.keys().filter(|key| !after.contains_key(*key)).collect();
            let stayed: Vec<&String> = after.keys().filter(|key| !expected.contains_key(*key)).collect();
            panic!("seed {seed}: delete of {keys:#?}\nalso took {gone:#?}\nleft {stayed:#?}");
        }
    }
}

fn number(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|text| text.parse().ok()).unwrap_or(default)
}

#[test]
fn random_trees_lose_nothing() {
    let (first, rounds) = (number("RMC_SEED", 1), number("RMC_ROUNDS", 4));
    let mut counts = Counts::default();
    for seed in first..first + rounds {
        round(seed, &mut counts);
    }
    println!(
        "{rounds} rounds: {} jobs, {} questions, {} with errors, {} cancelled, {} refused by the plan",
        counts.jobs, counts.questions, counts.with_errors, counts.cancelled, counts.refused
    );
    println!("{:#?}", counts.errors);
    assert!(counts.jobs > 0);
}

#[test]
fn random_trees_lose_nothing_over_ssh() {
    let (first, rounds) = (number("RMC_SEED", 1), number("RMC_ROUNDS", 4));
    let mut counts = Counts::default();
    for seed in first..first + rounds {
        round_over_ssh(seed, &mut counts);
    }
    println!(
        "{rounds} rounds over ssh: {} jobs, {} questions, {} with errors, {} cancelled, {} refused by the plan",
        counts.jobs, counts.questions, counts.with_errors, counts.cancelled, counts.refused
    );
    println!("{:#?}", counts.errors);
}
