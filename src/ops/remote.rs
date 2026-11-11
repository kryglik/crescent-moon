//! Jobs with an end on another computer: copying and moving between this computer and one
//! reached over SSH, or on and between such computers, and deleting there. The promises are
//! the ones of the rest of `ops`: a copy is written under a temporary name and takes its
//! target's place in one step, a source goes only once its copy is complete and checked, what
//! a job put in place it doesn't replace, and nothing goes down into what is mounted.

use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use super::{
    FLUSH_FROM, FileInfo, Flow, MAX_DEPTH, Stop, Worker, fs, is_link, loops_back, name_key, remove_dir, remove_file_or_link,
    replace_with, temp_leaf, writing,
};
use crate::archive;
use crate::sftp::{Attrs, Download, Sftp, Upload};
use crate::ssh::{self, Connection};
use crate::{platform, util};

/// How much of a file on this computer is read at a time. `Upload` splits it up.
const READ_SIZE: usize = 256 * 1024;

/// Where an item of a job is.
#[derive(Clone)]
pub enum Place {
    Here(PathBuf),
    There(Arc<Connection>, String),
}

impl std::fmt::Debug for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.shown().display())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    File,
    Folder,
    Link,
    Other,
}

/// What a job needs to know of what is at a place.
#[derive(Clone, Copy, Debug)]
struct Item {
    kind: Kind,
    size: u64,
    modified: Option<SystemTime>,
    /// Permissions, on a system that has them.
    mode: Option<u32>,
    owner: Option<(u32, u32)>,
}

impl From<Attrs> for Item {
    fn from(attrs: Attrs) -> Item {
        let kind = match () {
            _ if attrs.is_link() => Kind::Link,
            _ if attrs.is_dir() => Kind::Folder,
            _ if attrs.is_file() => Kind::File,
            _ => Kind::Other,
        };
        let size = if kind == Kind::File { attrs.size.unwrap_or(0) } else { 0 };
        Item { kind, size, modified: attrs.modified(), mode: attrs.mode.map(|mode| mode & 0o7777), owner: attrs.owner }
    }
}

impl From<&Metadata> for Item {
    fn from(meta: &Metadata) -> Item {
        let kind = match () {
            _ if meta.file_type().is_symlink() => Kind::Link,
            _ if meta.is_dir() => Kind::Folder,
            _ if meta.is_file() => Kind::File,
            _ => Kind::Other,
        };
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(meta.permissions().mode() & 0o7777)
        };
        #[cfg(not(unix))]
        let mode = None;
        // Numbers of owners mean nothing on another computer.
        let size = if kind == Kind::File { meta.len() } else { 0 };
        Item { kind, size, modified: meta.modified().ok(), mode, owner: None }
    }
}

impl Place {
    /// As it is shown: the path, or the whole address.
    pub fn shown(&self) -> PathBuf {
        match self {
            Place::Here(path) => path.clone(),
            Place::There(connection, path) => PathBuf::from(connection.target.place(path)),
        }
    }

    fn join(&self, name: &str) -> Place {
        match self {
            Place::Here(path) => Place::Here(path.join(name)),
            Place::There(connection, path) => Place::There(connection.clone(), ssh::join(path, name)),
        }
    }

    fn parent(&self) -> Option<Place> {
        match self {
            Place::Here(path) => path.parent().map(|parent| Place::Here(parent.to_path_buf())),
            Place::There(connection, path) => ssh::parent(path).map(|parent| Place::There(connection.clone(), parent)),
        }
    }

    fn name(&self) -> Option<String> {
        match self {
            Place::Here(path) => path.file_name().map(|name| name.to_string_lossy().into_owned()),
            Place::There(_, path) => Some(ssh::name_of(path).to_string()).filter(|name| !name.is_empty()),
        }
    }

    /// Whether both are in one folder.
    pub fn is_beside(&self, other: &Place) -> bool {
        self.parent().zip(other.parent()).is_some_and(|(a, b)| a.same_place(&b))
    }

    /// Whether both are on the one other computer.
    fn same_host(&self, other: &Place) -> bool {
        matches!((self, other), (Place::There(a, _), Place::There(b, _)) if a.target == b.target)
    }

    fn same_place(&self, other: &Place) -> bool {
        match (self, other) {
            (Place::Here(a), Place::Here(b)) => util::same_path(a, b),
            (Place::There(_, a), Place::There(_, b)) => self.same_host(other) && a == b,
            _ => false,
        }
    }

    /// Whether this is `other` or lies below it.
    fn is_inside(&self, other: &Place) -> bool {
        match (self, other) {
            (Place::Here(a), Place::Here(b)) => util::is_inside(a, b),
            (Place::There(_, a), Place::There(_, b)) => self.same_host(other) && (a == b || a.starts_with(&ssh::join(b, ""))),
            _ => false,
        }
    }

    /// What is at this place for copying: a link is a link, but on Windows what it leads to,
    /// as local copies go there.
    fn look(&self) -> io::Result<Item> {
        match self {
            Place::Here(path) => super::followed_metadata(path).map(|meta| Item::from(&meta)),
            Place::There(connection, path) => connection.sftp.lstat(path).map(Item::from),
        }
    }

    /// What is at this place, a link being a link: what replacing it would touch.
    fn look_at_itself(&self) -> io::Result<Item> {
        match self {
            Place::Here(path) => fs::symlink_metadata(path).map(|meta| Item::from(&meta)),
            Place::There(connection, path) => connection.sftp.lstat(path).map(Item::from),
        }
    }

    /// What this place comes down to, through a link.
    fn follow(&self) -> io::Result<Item> {
        match self {
            Place::Here(path) => fs::metadata(path).map(|meta| Item::from(&meta)),
            Place::There(connection, path) => connection.sftp.stat(path).map(Item::from),
        }
    }

    fn children(&self) -> io::Result<Vec<String>> {
        match self {
            Place::Here(path) => {
                fs::read_dir(path)?.map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned())).collect()
            }
            Place::There(connection, path) => {
                Ok(connection.sftp.read_dir(path)?.into_iter().map(|entry| entry.name).collect())
            }
        }
    }

    /// Whether a name from elsewhere can be one here, joined to a path without going anywhere
    /// else: another computer's names are its own to choose (`..`, `a/b`), and on Windows
    /// `a:b` would be a hidden part of the file `a`, and `*` no name at all.
    pub fn can_hold(&self, name: &str) -> bool {
        match self {
            Place::Here(_) => archive::is_plain_name(name) && !(cfg!(windows) && name.contains(['\\', '/', '<', '>', '"', '|', '?', '*'])),
            Place::There(..) => !name.is_empty() && !name.contains(['/', '\0']) && name != "." && name != "..",
        }
    }

    /// Whether a link can be made here. Windows wants rights for that which users don't have.
    fn can_hold_links(&self) -> bool {
        cfg!(unix) || matches!(self, Place::There(..))
    }

    fn read_link(&self) -> io::Result<String> {
        match self {
            #[cfg(unix)]
            Place::Here(path) => Ok(fs::read_link(path)?.to_string_lossy().into_owned()),
            #[cfg(not(unix))]
            Place::Here(_) => Err(io::Error::other("cm reads links on Linux only")),
            Place::There(connection, path) => connection.sftp.read_link(path),
        }
    }

    fn make_link(&self, target: &str) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Place::Here(path) => std::os::unix::fs::symlink(target, path),
            #[cfg(not(unix))]
            Place::Here(_) => Err(io::Error::other("cm makes links on Linux only")),
            Place::There(connection, path) => connection.sftp.symlink(target, path),
        }
    }

    /// Makes this folder, for a copy of `item`: no more open to others than that one, and
    /// writable by its owner until `finish_folder`.
    fn make_folder(&self, item: &Item) -> io::Result<()> {
        let mode = item.mode.map(|mode| mode & 0o777 | 0o700);
        match self {
            Place::Here(path) => {
                fs::create_dir_all(path)?;
                #[cfg(unix)]
                if let Some(mode) = mode {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
                }
                #[cfg(not(unix))]
                let _ = mode;
                Ok(())
            }
            Place::There(connection, path) => connection.sftp.mkdir(path, mode),
        }
    }

    /// A new folder's last touches, once what goes in it is there.
    fn finish_folder(&self, item: &Item) {
        match self {
            Place::Here(path) => {
                if let Some(time) = item.modified {
                    let _ = fs::set_folder_time(path, time);
                }
                #[cfg(unix)]
                if let Some(mode) = item.mode {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777));
                }
            }
            Place::There(connection, path) => {
                let attrs = Attrs { mode: item.mode.map(|mode| mode & 0o777), ..Attrs::default() };
                let attrs = item.modified.map_or(attrs, |time| attrs.with_modified(time));
                let _ = connection.sftp.set_attrs(path, &attrs);
            }
        }
    }

    fn remove_file(&self) -> io::Result<()> {
        match self {
            Place::Here(path) => remove_file_or_link(path),
            Place::There(connection, path) => connection.sftp.remove(path),
        }
    }

    fn remove_folder(&self) -> io::Result<()> {
        match self {
            Place::Here(path) => remove_dir(path),
            Place::There(connection, path) => connection.sftp.rmdir(path),
        }
    }

    /// A name beside this place to write under before taking its place.
    fn temp_beside(&self) -> Place {
        match self {
            Place::Here(path) => Place::Here(path.with_file_name(temp_leaf())),
            Place::There(connection, path) => {
                let folder = ssh::parent(path).unwrap_or_else(|| "/".to_string());
                Place::There(connection.clone(), ssh::join(&folder, &temp_leaf()))
            }
        }
    }

    /// This finished copy takes the place of `target`, replacing what is there if `replacing`,
    /// which was agreed to.
    fn take_place_of(&self, target: &Place, replacing: bool) -> io::Result<()> {
        match (self, target) {
            (Place::Here(temp), Place::Here(target)) => replace_with(temp, target),
            (Place::There(connection, temp), Place::There(_, target)) => {
                let placed = if replacing { replace_there(&connection.sftp, temp, target)? } else { connection.sftp.rename(temp, target)? };
                if placed { Ok(()) } else { Err(io::Error::other("the server refused to put the copy in place")) }
            }
            _ => Err(io::Error::other("a copy and its target are on two computers")),
        }
    }

    /// Whether a file system is mounted at this folder, which isn't the job's to go into. Over
    /// there it shows by the disk differing from that of the folder above.
    fn is_mounted(&self, mounts: &[PathBuf]) -> bool {
        match self {
            Place::Here(path) => fs::symlink_metadata(path).is_ok_and(|meta| platform::is_mount(path, &meta, mounts)),
            Place::There(connection, path) => {
                let Some(parent) = ssh::parent(path) else { return false };
                let disk = |path: &str| connection.sftp.space(path).map(|space| space.id);
                matches!((disk(path), disk(&parent)), (Some(this), Some(above)) if this != above)
            }
        }
    }
}

/// `temp` takes the place of `target` in one step; a server that can't do that gets the old
/// file renamed aside first, and back if the new one can't follow. Ok(false): the server only
/// said it failed, as it does between two disks.
fn replace_there(sftp: &Sftp, temp: &str, target: &str) -> io::Result<bool> {
    if sftp.can_replace() {
        return sftp.replace(temp, target);
    }
    let folder = ssh::parent(target).unwrap_or_else(|| "/".to_string());
    let aside = ssh::join(&folder, &temp_leaf());
    if !sftp.rename(target, &aside)? {
        return Ok(false);
    }
    match sftp.rename(temp, target) {
        Ok(true) => sftp.remove(&aside).map(|()| true),
        Ok(false) | Err(_) => {
            let _ = sftp.rename(&aside, target);
            Ok(false)
        }
    }
}

/// Makes folder `dir` over there, with the ones above it.
pub fn make_folders(sftp: &Sftp, dir: &str) -> io::Result<()> {
    match sftp.stat(dir) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = ssh::parent(dir) {
                make_folders(sftp, &parent)?;
            }
            sftp.mkdir(dir, None)
        }
        Err(e) => Err(e),
    }
}

/// Pairs each source with its destination by the rules of F5 (see `ops::plan`): `dest` is a
/// folder to put them in if there are several, if it is one, or if it was typed as one.
pub fn plan(sources: &[Place], dest: &Place, as_folder: bool) -> Result<Vec<(Place, Place)>, String> {
    let at_dest = dest.follow().ok();
    let is_folder = at_dest.is_some_and(|item| item.kind == Kind::Folder);
    let into_folder = sources.len() > 1 || is_folder || as_folder;
    if into_folder && at_dest.is_some() && !is_folder {
        return Err(format!("\"{}\" is a file, not a folder to put them in.", dest.shown().display()));
    }
    let mut pairs = Vec::new();
    for source in sources {
        let target = if into_folder {
            let Some(name) = source.name() else {
                return Err(format!("\"{}\" is the top of a disk and cannot be copied.", source.shown().display()));
            };
            if !dest.can_hold(&name) {
                return Err(format!("\"{name}\" can't be a name in \"{}\".", dest.shown().display()));
            }
            dest.join(&name)
        } else {
            dest.clone()
        };
        if source.same_place(&target) {
            return Err(format!("\"{}\" is the same as its destination.", source.shown().display()));
        }
        // Names over SFTP are UTF-8: another name would arrive, and this one find no way back.
        if let Place::Here(path) = source
            && path.file_name().is_some_and(|name| name.to_str().is_none())
        {
            return Err(format!("\"{}\": this name can't be written over SFTP (it isn't UTF-8).", source.shown().display()));
        }
        // By what it leads to, as `check_pair` goes: a link to a folder isn't put in that folder.
        let folder = source.follow().is_ok_and(|item| item.kind == Kind::Folder);
        if folder && (target.is_inside(source) || really_inside(&target, source)) {
            return Err(format!("\"{}\" cannot be put inside itself.", source.shown().display()));
        }
        pairs.push((source.clone(), target));
    }
    Ok(pairs)
}

/// Whether `target`, through the links on its way, lies inside folder `source` on one computer:
/// the folder would be copied into itself, ever deeper.
fn really_inside(target: &Place, source: &Place) -> bool {
    let (Place::There(connection, to), Place::There(_, from)) = (target, source) else { return false };
    if !source.same_host(target) {
        return false;
    }
    let sftp = &connection.sftp;
    // The nearest folder of the way that is there.
    let mut there = to.clone();
    while sftp.stat(&there).is_err() {
        let Some(parent) = ssh::parent(&there) else { return false };
        there = parent;
    }
    match (sftp.realpath(&there), sftp.realpath(from)) {
        (Ok(there), Ok(from)) => there == from || there.starts_with(&ssh::join(&from, "")),
        _ => false,
    }
}

/// Whether `a` and `b`, by a link, are one file on one computer: a link replacing what it leads
/// to would leave nothing to lead to.
fn one_file(a: &Place, b: &Place) -> bool {
    let (Place::There(connection, first), Place::There(_, second)) = (a, b) else { return false };
    let real = |path: &str| connection.sftp.realpath(path).ok();
    a.same_host(b) && matches!((real(first), real(second)), (Some(a), Some(b)) if a == b)
}

/// A file being read, here or there.
enum Reading {
    Here(File),
    There(Download),
}

impl Reading {
    fn open(place: &Place, item: &Item) -> io::Result<Reading> {
        match place {
            Place::Here(path) => Ok(Reading::Here(fs::open(path)?)),
            Place::There(connection, path) => {
                let handle = connection.sftp.open(path)?;
                Ok(Reading::There(Download::new(connection.sftp.clone(), handle, item.size)))
            }
        }
    }

    fn next_piece(&mut self) -> io::Result<Option<Vec<u8>>> {
        match self {
            Reading::Here(file) => {
                let mut piece = vec![0; READ_SIZE];
                loop {
                    match file.read(&mut piece) {
                        Ok(0) => return Ok(None),
                        Ok(n) => {
                            piece.truncate(n);
                            return Ok(Some(piece));
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            Reading::There(download) => download.next_piece(),
        }
    }

    fn close(self) {
        if let Reading::There(download) = self {
            download.close();
        }
    }
}

/// The permissions a copy gets. A file from Windows has none of its own, and takes those of
/// what it replaces; one put back after editing stays what it was, as its copy here was only
/// as open as this computer's umask lets it be. A link's aren't anybody's.
fn mode_of_copy(item: &Item, existing: Option<&Item>, putting_back: bool) -> Option<u32> {
    let existing = existing.filter(|existing| existing.kind == Kind::File).and_then(|existing| existing.mode);
    if putting_back { existing.or(item.mode) } else { item.mode.or(existing) }
}

/// A new file being written, here or there.
enum Writing {
    Here(File),
    There(Upload),
}

impl Writing {
    /// Makes the file, from the first byte no more open to others than the original.
    fn create(place: &Place, item: &Item) -> io::Result<Writing> {
        match place {
            Place::Here(path) => Ok(Writing::Here(fs::create_new_with_mode(path, item.mode.map(|mode| mode & 0o777))?)),
            Place::There(connection, path) => {
                let handle = connection.sftp.create(path, item.mode.map(|mode| mode & 0o700))?;
                Ok(Writing::There(Upload::new(connection.sftp.clone(), handle)))
            }
        }
    }

    fn write(&mut self, piece: &[u8]) -> io::Result<()> {
        match self {
            Writing::Here(file) => file.write_all(piece),
            Writing::There(upload) => upload.write(piece),
        }
    }

    /// Finishes the file: the file goes on the strength of this, so a late refusal counts, and
    /// the size has to be what was sent. `item` is the original, `existing` what it replaces.
    fn finish(self, place: &Place, written: u64, item: &Item, existing: Option<&Item>, flush: bool) -> io::Result<()> {
        match (self, place) {
            (Writing::Here(file), Place::Here(path)) => {
                // Keeping the time is worth trying, not failing over (see `copy_contents`).
                if let Some(time) = item.modified {
                    let _ = file.set_modified(time);
                }
                if flush {
                    file.sync_all()?;
                }
                fs::close(file)?;
                if fs::symlink_metadata(path)?.len() != written {
                    return Err(io::Error::other("it came out shorter than the original"));
                }
                Ok(())
            }
            (Writing::There(mut upload), Place::There(connection, _)) => {
                let result = upload.finish().and_then(|()| finish_there(&connection.sftp, &upload, written, item, existing, flush));
                let closed = connection.sftp.close(&upload.handle);
                result.and(closed)
            }
            _ => Err(io::Error::other("a file written elsewhere than it was made")),
        }
    }

    /// Lets go of a file that won't be finished.
    fn abandon(self, place: &Place) {
        if let (Writing::There(upload), Place::There(connection, _)) = (self, place) {
            let _ = connection.sftp.close(&upload.handle);
        }
    }
}

/// What a file over there still needs: the size checked, then the looks. A copy replacing
/// another person's file stays theirs if the server lets it (as root).
fn finish_there(sftp: &Sftp, upload: &Upload, written: u64, item: &Item, existing: Option<&Item>, flush: bool) -> io::Result<()> {
    let attrs = sftp.fstat(&upload.handle)?;
    if attrs.size != Some(written) {
        return Err(io::Error::other("it came out shorter than the original"));
    }
    let replaced = existing.filter(|existing| existing.kind == Kind::File);
    if let Some(owner) = replaced.and_then(|item| item.owner).filter(|owner| attrs.owner != Some(*owner)) {
        let _ = sftp.set_handle_attrs(&upload.handle, &Attrs { owner: Some(owner), ..Attrs::default() });
    }
    let look = Attrs { mode: item.mode.map(|mode| mode & 0o777), ..Attrs::default() };
    let look = item.modified.map_or(look, |time| look.with_modified(time));
    let _ = sftp.set_handle_attrs(&upload.handle, &look);
    if flush { sftp.sync(&upload.handle) } else { Ok(()) }
}

/// Whether two folders are one by two ways: a server's folder, and a share of it mounted
/// here. A name made in one then shows in the other. Copying onto "another" file there
/// replaces the source, and deleting the source then deletes the only copy.
fn one_folder(src: &Place, dst: &Place) -> bool {
    let (Some(from), Some(to)) = (src.parent(), dst.parent()) else { return false };
    let leaf = temp_leaf();
    let mark = to.join(&leaf);
    let Ok(writing) = Writing::create(&mark, &Item { kind: Kind::File, size: 0, modified: None, mode: Some(0o600), owner: None }) else {
        return false;
    };
    writing.abandon(&mark);
    let shown = from.join(&leaf).look_at_itself().is_ok();
    let _ = mark.remove_file();
    shown
}

impl Worker {
    pub(super) fn transfer(&mut self, pairs: &[(Place, Place)], moving: bool) -> Flow<()> {
        self.moving = moving;
        self.mounts = platform::mount_dirs();
        // On one computer a move is a rename, and needs no counting.
        let totals = if moving && pairs.iter().all(|(src, dst)| src.same_host(dst)) {
            self.progress.files_total = pairs.len() as u64;
            vec![(1, 0); pairs.len()]
        } else {
            self.count_all(pairs.iter().map(|(src, _)| src))?
        };
        for ((src, dst), (files, bytes)) in pairs.iter().zip(totals) {
            let before = (self.progress.files_done, self.progress.bytes_done);
            if self.make_parent(dst)? {
                if moving && src.same_host(dst) {
                    self.move_there(src, dst, 0)?;
                } else {
                    self.send(src, dst, &mut Vec::new(), moving)?;
                }
            }
            // However it went (renamed, copied, partly skipped), this item is done.
            (self.progress.files_done, self.progress.bytes_done) = (before.0 + files, before.1 + bytes);
            self.send_progress(false);
        }
        Ok(())
    }

    pub(super) fn delete_there(&mut self, connection: &Arc<Connection>, paths: &[String]) -> Flow<()> {
        let places: Vec<Place> = paths.iter().map(|path| Place::There(connection.clone(), path.clone())).collect();
        self.progress.scanning = true;
        for place in &places {
            self.count_there(place, 0, true)?;
        }
        self.progress.scanning = false;
        for place in &places {
            self.remove_there(place, 0)?;
        }
        Ok(())
    }

    fn count_all<'a>(&mut self, sources: impl Iterator<Item = &'a Place>) -> Flow<Vec<(u64, u64)>> {
        self.progress.scanning = true;
        let mut totals = Vec::new();
        for source in sources {
            let before = (self.progress.files_total, self.progress.bytes_total);
            match source {
                Place::Here(path) => self.scan(path, true, &mut Vec::new())?,
                Place::There(..) => self.count_there(source, 0, false)?,
            }
            totals.push((self.progress.files_total - before.0, self.progress.bytes_total - before.1));
        }
        self.progress.scanning = false;
        Ok(totals)
    }

    /// Counts what is below `place` over there. Links aren't followed. `folders`: they count
    /// as items too, as in deleting.
    fn count_there(&mut self, place: &Place, depth: usize, folders: bool) -> Flow<()> {
        self.check_cancel()?;
        match place.look_at_itself() {
            Ok(item) if item.kind == Kind::Folder => {
                if depth < MAX_DEPTH
                    && let Ok(children) = place.children()
                {
                    for name in children {
                        self.count_there(&place.join(&name), depth + 1, folders)?;
                    }
                }
                self.progress.files_total += u64::from(folders);
            }
            Ok(item) => {
                self.progress.files_total += 1;
                self.progress.bytes_total += item.size;
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => return Err(self.connection_lost(place, &e)),
            Err(_) => self.progress.files_total += 1,
        }
        self.progress.current = place.shown().display().to_string();
        self.send_progress(false);
        Ok(())
    }

    fn connection_lost(&mut self, place: &Place, error: &io::Error) -> Stop {
        self.errors.push(format!("{}: {error}", place.shown().display()));
        Stop::Cancelled
    }

    fn make_parent(&mut self, dst: &Place) -> Flow<bool> {
        match dst {
            Place::Here(path) => self.prepare_parent(path),
            Place::There(connection, path) => {
                let Some(parent) = ssh::parent(path) else { return Ok(true) };
                Ok(self.attempt(&dst.shown(), |_| make_folders(&connection.sftp, &parent))?.is_some())
            }
        }
    }

    fn taken(&self, place: &Place) -> bool {
        match place {
            Place::Here(path) => self.written.contains(&name_key(path)),
            Place::There(_, path) => self.written_there.contains(&path.to_lowercase()),
        }
    }

    fn took(&mut self, place: &Place) {
        match place {
            Place::Here(path) => {
                self.written.insert(name_key(path));
            }
            Place::There(_, path) => {
                self.written_there.insert(path.to_lowercase());
            }
        }
    }

    /// Whether what is at `dst` may go for a copy of `src`, as `item`, which is told if it is a
    /// folder, if it is the source itself by a link, or if this job put it there. Where a server
    /// doesn't tell one file from another, a name that is there and that this job wrote in
    /// another letter case counts as that: on a server that ignores case it is that file, and
    /// the worst this does elsewhere is to leave a source where it was, and say so.
    fn may_replace(&mut self, src: &Place, dst: &Place, item: &Item, existing: &Item) -> Flow<bool> {
        let shown = dst.shown();
        if existing.kind == Kind::Folder {
            self.errors.push(format!("{}: a directory with this name already exists", shown.display()));
            return Ok(false);
        }
        if one_file(src, dst) {
            self.errors.push(format!("{}: a link to its destination (or the other way round), skipped", src.shown().display()));
            return Ok(false);
        }
        if self.taken(dst) {
            self.errors.push(format!("{}: another item of this job already took this name here, skipped", shown.display()));
            return Ok(false);
        }
        let new = FileInfo { size: item.size, modified: item.modified };
        let old = FileInfo { size: existing.size, modified: existing.modified };
        self.overwrite_agreed(&shown, new, old)
    }

    /// Sends `src` to `dst`, with what is below it; `remove`: each source goes when it has
    /// arrived, a folder once it is empty. Returns whether everything went.
    fn send(&mut self, src: &Place, dst: &Place, ancestors: &mut Vec<PathBuf>, remove: bool) -> Flow<bool> {
        self.check_cancel()?;
        let shown = src.shown();
        let Some(item) = self.attempt(&shown, |_| src.look())? else {
            self.skip_file(0);
            return Ok(false);
        };
        // On Windows `look` goes through a link. What is moved is the link: what it leads to
        // arrives, and stays where it is too.
        if remove && item.kind != Kind::Link && matches!(src, Place::Here(path) if is_link(path)) {
            let sent = match item.kind {
                Kind::Folder => self.send_folder(src, dst, item, ancestors, false)?,
                _ => self.send_file(src, dst, item, false)?,
            };
            return Ok(sent && self.attempt(&shown, |_| src.remove_file())?.is_some());
        }
        match item.kind {
            Kind::File => self.send_file(src, dst, item, remove),
            Kind::Folder => self.send_folder(src, dst, item, ancestors, remove),
            Kind::Link => self.send_link(src, dst, item, remove),
            Kind::Other => {
                self.errors.push(format!("{}: not a regular file (a pipe, socket or device), skipped", shown.display()));
                self.skip_file(0);
                Ok(false)
            }
        }
    }

    fn send_folder(&mut self, src: &Place, dst: &Place, item: Item, ancestors: &mut Vec<PathBuf>, remove: bool) -> Flow<bool> {
        let shown = src.shown();
        if ancestors.len() >= MAX_DEPTH {
            self.errors.push(format!("{}: nested too deeply, skipped", shown.display()));
            return Ok(false);
        }
        if let Place::Here(path) = src
            && is_link(path)
            && loops_back(path, ancestors)
        {
            self.errors.push(format!("{}: a link back to a folder above it, skipped", shown.display()));
            return Ok(false);
        }
        if remove && src.is_mounted(&self.mounts) {
            self.errors.push(format!("{}: a file system is mounted here, skipped", shown.display()));
            return Ok(false);
        }
        let existing = dst.look_at_itself().ok();
        if existing.is_some_and(|existing| existing.kind != Kind::Folder) {
            self.errors.push(format!("{}: a file with this name already exists", dst.shown().display()));
            return Ok(false);
        }
        let created = existing.is_none();
        if created && self.attempt(&dst.shown(), |_| dst.make_folder(&item))?.is_none() {
            return Ok(false);
        }
        let Some(children) = self.attempt(&shown, |_| src.children())? else { return Ok(false) };
        ancestors.push(src.shown());
        let mut complete = true;
        for name in children {
            if !dst.can_hold(&name) {
                self.errors.push(format!("{}: {name:?} can't be a name there, skipped", shown.display()));
                self.skip_file(0);
                complete = false;
                continue;
            }
            complete &= self.send(&src.join(&name), &dst.join(&name), ancestors, remove)?;
        }
        ancestors.pop();
        if created {
            dst.finish_folder(&item);
        }
        Ok(complete && (!remove || self.attempt(&shown, |_| src.remove_folder())?.is_some()))
    }

    /// A link is sent as a link, as `cp -a` and mc do, even a broken one. Where no link can
    /// be made (Windows) the file it leads to goes instead; a link to a folder is skipped and
    /// said so: it may lead to the top of the disk.
    fn send_link(&mut self, src: &Place, dst: &Place, item: Item, remove: bool) -> Flow<bool> {
        let shown = src.shown();
        self.progress.current = shown.display().to_string();
        if !dst.can_hold_links() {
            return self.send_through_link(src, dst, remove);
        }
        let Some(target) = self.attempt(&shown, |_| src.read_link())? else {
            self.skip_file(0);
            return Ok(false);
        };
        let existing = dst.look_at_itself().ok();
        if let Some(existing) = &existing
            && !self.may_replace(src, dst, &item, existing)?
        {
            self.skip_file(0);
            return Ok(false);
        }
        let made = self.attempt(&shown, |_| {
            let temp = dst.temp_beside();
            temp.make_link(&target)?;
            temp.take_place_of(dst, existing.is_some()).inspect_err(|_| {
                let _ = temp.remove_file();
            })
        })?;
        if made.is_none() {
            self.skip_file(0);
            return Ok(false);
        }
        self.progress.files_done += 1;
        self.took(dst);
        Ok(!remove || self.attempt(&shown, |_| src.remove_file())?.is_some())
    }

    /// The file a link leads to goes in its place; moving, the link goes and that stays.
    fn send_through_link(&mut self, src: &Place, dst: &Place, remove: bool) -> Flow<bool> {
        let shown = src.shown();
        let Some(item) = self.attempt(&shown, |_| src.follow())? else {
            self.skip_file(0);
            return Ok(false);
        };
        let sent = match item.kind {
            Kind::File => self.send_file(src, dst, item, false)?,
            Kind::Folder => {
                self.errors.push(format!("{}: a link to a folder, which can't be made here, skipped", shown.display()));
                self.skip_file(0);
                false
            }
            _ => {
                self.errors.push(format!("{}: a link to something that can't be copied, skipped", shown.display()));
                self.skip_file(0);
                false
            }
        };
        Ok(sent && (!remove || self.attempt(&shown, |_| src.remove_file())?.is_some()))
    }

    fn send_file(&mut self, src: &Place, dst: &Place, item: Item, remove: bool) -> Flow<bool> {
        let shown = src.shown();
        self.progress.current = shown.display().to_string();
        let existing = dst.look_at_itself().ok();
        if let Some(existing) = &existing {
            if !self.may_replace(src, dst, &item, existing)? {
                self.skip_file(item.size);
                return Ok(false);
            }
            if remove && one_folder(src, dst) {
                self.errors.push(format!("{}: looks like the same file as {}, left in place", shown.display(), dst.shown().display()));
                self.skip_file(item.size);
                return Ok(false);
            }
        }
        if self.attempt(&shown, |worker| worker.copy_across(src, dst, &item, existing.as_ref()))?.is_none() {
            self.skip_file(item.size);
            return Ok(false);
        }
        self.progress.files_done += 1;
        self.took(dst);
        self.send_progress(false);
        Ok(!remove || self.attempt(&shown, |_| src.remove_file())?.is_some())
    }

    /// One file under a temporary name beside the target, which it replaces at the end, so a
    /// failed or stopped copy leaves the target as it was.
    fn copy_across(&mut self, src: &Place, dst: &Place, item: &Item, existing: Option<&Item>) -> io::Result<()> {
        let start = self.progress.bytes_done;
        let temp = dst.temp_beside();
        let flush = item.size >= FLUSH_FROM && (self.moving || existing.is_some());
        let result = self.fill(src, &temp, item, existing, flush).and_then(|()| temp.take_place_of(dst, existing.is_some()).map_err(writing));
        if result.is_err() {
            self.progress.bytes_done = start;
            let _ = temp.remove_file();
        }
        result
    }

    fn fill(&mut self, src: &Place, temp: &Place, item: &Item, existing: Option<&Item>, flush: bool) -> io::Result<()> {
        let item = &Item { mode: mode_of_copy(item, existing, self.putting_back), ..*item };
        let mut input = Reading::open(src, item)?;
        let mut output = match Writing::create(temp, item) {
            Ok(output) => output,
            Err(e) => {
                input.close();
                return Err(writing(e));
            }
        };
        let mut written = 0;
        let pumped = (|| {
            while let Some(piece) = input.next_piece()? {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                output.write(&piece).map_err(writing)?;
                written += piece.len() as u64;
                self.progress.bytes_done += piece.len() as u64;
                self.send_progress(false);
            }
            Ok(())
        })();
        input.close();
        // A move takes the source away on the strength of this: all of it must have come.
        let short = self.moving && written != item.size;
        match pumped {
            Ok(()) if short => {
                output.abandon(temp);
                Err(io::Error::other("the file changed while it was copied, so it stays where it is"))
            }
            Ok(()) => output.finish(temp, written, item, existing, flush).map_err(writing),
            Err(e) => {
                output.abandon(temp);
                Err(e)
            }
        }
    }

    /// A move within one other computer: a rename. Onto a folder that exists it merges; between
    /// two disks there it is a copy and a delete. Returns whether all of it moved.
    fn move_there(&mut self, src: &Place, dst: &Place, depth: usize) -> Flow<bool> {
        self.check_cancel()?;
        let shown = src.shown();
        self.progress.current = shown.display().to_string();
        self.send_progress(false);
        let (Place::There(connection, from), Place::There(_, to)) = (src, dst) else { return Ok(false) };
        let Some(item) = self.attempt(&shown, |_| src.look_at_itself())? else { return Ok(false) };
        let existing = dst.look_at_itself().ok();
        if let Some(existing) = &existing {
            if existing.kind == Kind::Folder && item.kind == Kind::Folder {
                return self.merge_there(src, dst, depth);
            }
            if existing.kind == Kind::Folder || item.kind == Kind::Folder {
                self.errors.push(format!("{}: cannot replace a file with a directory or the other way round", dst.shown().display()));
                return Ok(false);
            }
            if !self.may_replace(src, dst, &item, existing)? {
                return Ok(false);
            }
        }
        let renamed = self.attempt(&shown, |_| match existing {
            Some(_) => replace_there(&connection.sftp, from, to),
            None => connection.sftp.rename(from, to),
        })?;
        match renamed {
            Some(true) => {
                self.took(dst);
                Ok(true)
            }
            // All a server says for two disks is "failure". Copied, then deleted, as to another
            // computer; a replace agreed to stays agreed to.
            Some(false) => {
                self.confirmed = existing.map(|_| dst.shown());
                let sent = self.send(src, dst, &mut Vec::new(), true);
                self.confirmed = None;
                sent
            }
            None => Ok(false),
        }
    }

    /// Moves what folder `src` holds into the existing folder `dst` item by item, then removes
    /// `src` if that emptied it.
    fn merge_there(&mut self, src: &Place, dst: &Place, depth: usize) -> Flow<bool> {
        let shown = src.shown();
        if depth >= MAX_DEPTH {
            self.errors.push(format!("{}: nested too deeply, skipped", shown.display()));
            return Ok(false);
        }
        if src.is_mounted(&self.mounts) {
            self.errors.push(format!("{}: a file system is mounted here, skipped", shown.display()));
            return Ok(false);
        }
        let Some(children) = self.attempt(&shown, |_| src.children())? else { return Ok(false) };
        let mut complete = true;
        for name in children {
            complete &= self.move_there(&src.join(&name), &dst.join(&name), depth + 1)?;
        }
        Ok(complete && self.attempt(&shown, |_| src.remove_folder())?.is_some())
    }

    /// Deletes `place` and everything below it, without following links.
    fn remove_there(&mut self, place: &Place, depth: usize) -> Flow<bool> {
        self.check_cancel()?;
        let shown = place.shown();
        let Some(item) = self.attempt(&shown, |_| place.look_at_itself())? else { return Ok(false) };
        let removed = if item.kind == Kind::Folder {
            if depth >= MAX_DEPTH {
                self.errors.push(format!("{}: nested too deeply, skipped", shown.display()));
                return Ok(false);
            }
            if place.is_mounted(&self.mounts) {
                self.errors.push(format!("{}: a file system is mounted here, skipped", shown.display()));
                return Ok(false);
            }
            let Some(children) = self.attempt(&shown, |_| place.children())? else { return Ok(false) };
            let mut complete = true;
            for name in children {
                complete &= self.remove_there(&place.join(&name), depth + 1)?;
            }
            complete && self.attempt(&shown, |_| place.remove_folder())?.is_some()
        } else {
            self.progress.current = shown.display().to_string();
            self.attempt(&shown, |_| place.remove_file())?.is_some()
        };
        self.advance((1, item.size));
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Answer, Job, Msg, Question, start};
    use super::*;
    use crate::sftp::tests::server_listing;
    use crate::ssh::tests::{kill, local_connection, scripted_connection};
    use crate::testutil::TempDir;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    /// A folder here, a folder "on another computer" (a real server program started in it,
    /// without `ssh`), and the connection to it. None where no server is installed.
    struct Two {
        here: TempDir,
        there: TempDir,
        connection: Arc<Connection>,
    }

    impl Two {
        fn new(tag: &str) -> Option<Two> {
            let there = TempDir::new(&format!("{tag}-there"));
            let connection = local_connection(there.path())?;
            Some(Two { here: TempDir::new(&format!("{tag}-here")), there, connection })
        }

        fn local(&self, path: &str) -> Place {
            Place::Here(self.here.path().join(path))
        }

        fn remote(&self, path: &str) -> Place {
            Place::There(self.connection.clone(), ssh::join(&self.connection.home, path))
        }

        fn on_server(&self, path: &str) -> PathBuf {
            self.there.path().join(path)
        }
    }

    /// Runs a job to its end, answering every question with `answer`.
    fn run(job: Job, answer: Answer) -> (Vec<String>, bool, usize) {
        let running = start(job);
        let mut questions = 0;
        loop {
            match running.rx.recv_timeout(Duration::from_secs(20)).expect("job hung") {
                Msg::Progress(_) => {}
                Msg::Ask(_, reply) => {
                    questions += 1;
                    reply.send(answer).unwrap();
                }
                Msg::Done { errors, cancelled, .. } => return (errors, cancelled, questions),
            }
        }
    }

    fn copy(from: Place, to: Place, answer: Answer) -> (Vec<String>, bool, usize) {
        run(Job::Transfer { pairs: vec![(from, to)], moving: false, replace: false }, answer)
    }

    fn take(from: Place, to: Place, answer: Answer) -> (Vec<String>, bool, usize) {
        run(Job::Transfer { pairs: vec![(from, to)], moving: true, replace: false }, answer)
    }

    /// All names below `dir`, with `/` between folders, sorted: a leftover shows here.
    fn tree(dir: &Path) -> Vec<String> {
        fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).unwrap().flatten() {
                let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
                out.push(name.clone());
                if entry.path().symlink_metadata().unwrap().is_dir() {
                    walk(&entry.path(), &format!("{name}/"), out);
                }
            }
        }
        let mut names = Vec::new();
        walk(dir, "", &mut names);
        names.sort();
        names
    }

    /// A tree goes to the other computer and comes back the same: names, contents and times.
    #[test]
    fn a_tree_goes_there_and_comes_back() {
        let Some(two) = Two::new("remote-tree") else { return };
        let big: String = "0123456789abcdef".repeat(1 << 16);
        two.here.file("tree/a.txt", "a");
        two.here.file("tree/sub/Příliš žluťoučký.txt", "kůň");
        two.here.file("tree/big.bin", &big);
        two.here.dir("tree/empty");
        let old = UNIX_EPOCH + Duration::from_secs(1_400_000_000);
        File::options().write(true).open(two.here.path().join("tree/a.txt")).unwrap().set_modified(old).unwrap();

        let (errors, cancelled, questions) = copy(two.local("tree"), two.remote("up"), Answer::Cancel);
        assert!(errors.is_empty() && !cancelled && questions == 0, "{errors:?}");
        let expected = ["a.txt", "big.bin", "empty", "sub", "sub/Příliš žluťoučký.txt"];
        assert_eq!(tree(&two.on_server("up")), expected, "and no leftovers");
        assert_eq!(fs::read_to_string(two.on_server("up/big.bin")).unwrap(), big);
        assert_eq!(fs::metadata(two.on_server("up/a.txt")).unwrap().modified().unwrap(), old);
        assert!(two.here.path().join("tree/a.txt").exists(), "a copy keeps the source");

        let (errors, ..) = copy(two.remote("up"), two.local("down"), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(tree(&two.here.path().join("down")), expected);
        assert_eq!(fs::read_to_string(two.here.path().join("down/sub/Příliš žluťoučký.txt")).unwrap(), "kůň");
        assert_eq!(fs::metadata(two.here.path().join("down/a.txt")).unwrap().modified().unwrap(), old);
    }

    /// Permissions go along, on systems that have them, and a copy isn't more open than its
    /// original. What it replaces gives its own to a file from a system without them.
    #[cfg(unix)]
    #[test]
    fn permissions_go_along() {
        use std::os::unix::fs::PermissionsExt;
        let Some(two) = Two::new("remote-modes") else { return };
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let script = two.here.file("script.sh", "#!/bin/sh");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o750)).unwrap();
        let secret = two.here.file("folder/secret", "s");
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(two.here.path().join("folder"), fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["script.sh", "folder"] {
            let (errors, ..) = copy(two.local(name), two.remote(name), Answer::Cancel);
            assert!(errors.is_empty(), "{errors:?}");
        }
        assert_eq!(mode(&two.on_server("script.sh")), 0o750);
        assert_eq!(mode(&two.on_server("folder/secret")), 0o600);
        assert_eq!(mode(&two.on_server("folder")), 0o700);
        let (errors, ..) = copy(two.remote("script.sh"), two.local("back.sh"), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(mode(&two.here.path().join("back.sh")), 0o750);
    }

    #[test]
    fn what_is_there_goes_only_if_the_answer_says_so() {
        let Some(two) = Two::new("remote-overwrite") else { return };
        two.here.file("x.txt", "new");
        two.there.file("x.txt", "old");
        let stored = || fs::read_to_string(two.on_server("x.txt")).unwrap();

        let (errors, _, questions) = copy(two.local("x.txt"), two.remote("x.txt"), Answer::No);
        assert_eq!((questions, stored().as_str()), (1, "old"), "{errors:?}");
        let (_, _, questions) = copy(two.local("x.txt"), two.remote("x.txt"), Answer::None);
        assert_eq!((questions, stored().as_str()), (1, "old"));
        let (errors, _, questions) = copy(two.local("x.txt"), two.remote("x.txt"), Answer::Yes);
        assert_eq!((questions, stored().as_str()), (1, "new"), "{errors:?}");
        assert_eq!(tree(two.there.path()), ["x.txt"], "no leftovers");

        // And the other way round.
        two.there.file("y.txt", "from there");
        two.here.file("y.txt", "here");
        let (errors, ..) = copy(two.remote("y.txt"), two.local("y.txt"), Answer::No);
        assert!(errors.is_empty());
        assert_eq!(fs::read_to_string(two.here.path().join("y.txt")).unwrap(), "here");
        copy(two.remote("y.txt"), two.local("y.txt"), Answer::Yes);
        assert_eq!(fs::read_to_string(two.here.path().join("y.txt")).unwrap(), "from there");

        // `replace`: asked already, so no question.
        two.here.file("z.txt", "edited");
        two.there.file("z.txt", "old");
        let job = Job::Transfer { pairs: vec![(two.local("z.txt"), two.remote("z.txt"))], moving: false, replace: true };
        let (errors, _, questions) = run(job, Answer::Cancel);
        assert!(errors.is_empty() && questions == 0, "{errors:?}");
        assert_eq!(fs::read_to_string(two.on_server("z.txt")).unwrap(), "edited");
    }

    #[test]
    fn a_file_and_a_folder_dont_replace_each_other() {
        let Some(two) = Two::new("remote-kinds") else { return };
        two.here.file("name/inside.txt", "here");
        two.there.file("name", "a file");
        let (errors, _, questions) = take(two.local("name"), two.remote("name"), Answer::Yes);
        assert_eq!((errors.len(), questions), (1, 0), "{errors:?}");
        assert!(errors[0].contains("file with this name"), "{errors:?}");
        assert!(two.here.path().join("name/inside.txt").exists());
        assert_eq!(fs::read_to_string(two.on_server("name")).unwrap(), "a file");

        two.here.file("other", "a file");
        two.there.file("other/inside.txt", "there");
        let (errors, ..) = take(two.local("other"), two.remote("other"), Answer::Yes);
        assert!(errors[0].contains("directory with this name"), "{errors:?}");
        assert!(two.here.path().join("other").is_file() && two.on_server("other/inside.txt").exists());
    }

    /// Moves between two computers: each source goes when its copy has arrived, and folders
    /// when they are empty. A file that can't arrive keeps its source.
    #[test]
    fn a_move_takes_the_source_only_when_the_copy_is_there() {
        let Some(two) = Two::new("remote-move") else { return };
        two.here.file("tree/a.txt", "a");
        two.here.file("tree/sub/b.txt", "b");
        let (errors, ..) = take(two.local("tree"), two.remote("up"), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!two.here.path().join("tree").exists());
        assert_eq!(tree(&two.on_server("up")), ["a.txt", "sub", "sub/b.txt"]);

        let (errors, ..) = take(two.remote("up"), two.local("down"), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!two.on_server("up").exists());
        assert_eq!(tree(&two.here.path().join("down")), ["a.txt", "sub", "sub/b.txt"]);

        // One item answered No: it stays, and so does the folder it is in.
        two.there.file("mixed/keep.txt", "old");
        two.here.file("mixed/keep.txt", "new");
        two.here.file("mixed/go.txt", "go");
        let (errors, ..) = take(two.local("mixed"), two.remote("mixed"), Answer::No);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(tree(&two.here.path().join("mixed")), ["keep.txt"]);
        assert_eq!(tree(&two.on_server("mixed")), ["go.txt", "keep.txt"]);
        assert_eq!(fs::read_to_string(two.on_server("mixed/keep.txt")).unwrap(), "old");
    }

    /// A junction moved away from Windows: its folder's files arrive, and stay in that folder;
    /// only the junction goes.
    #[cfg(windows)]
    #[test]
    fn a_junction_moved_keeps_the_folder_it_leads_to() {
        let Some(two) = Two::new("remote-junction") else { return };
        let real = two.here.dir("real");
        two.here.file("real/kept.txt", "kept");
        let link = two.here.path().join("link");
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&link).arg(&real).output().unwrap();
        assert!(made.status.success());
        let (errors, ..) = take(two.local("link"), two.remote("link"), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(fs::read_to_string(two.on_server("link/kept.txt")).unwrap(), "kept");
        assert_eq!(fs::read_to_string(real.join("kept.txt")).unwrap(), "kept", "what it leads to stays");
        assert!(fs::symlink_metadata(&link).is_err(), "the junction went");
    }

    /// A link copied or moved onto what it leads to would leave nothing: refused, unasked, as
    /// local jobs do. The same in a folder merged. And the other way round.
    #[cfg(unix)]
    #[test]
    fn a_link_doesnt_replace_what_it_leads_to() {
        use std::os::unix::fs::symlink;
        let Some(two) = Two::new("remote-link-onto") else { return };
        two.there.file("data.txt", "precious");
        symlink("data.txt", two.on_server("link")).unwrap();
        for (errors, _, questions) in [
            copy(two.remote("link"), two.remote("data.txt"), Answer::Yes),
            take(two.remote("link"), two.remote("data.txt"), Answer::Yes),
            take(two.remote("data.txt"), two.remote("link"), Answer::Yes),
        ] {
            assert_eq!(questions, 0, "{errors:?}");
            assert!(errors[0].contains("a link to its destination"), "{errors:?}");
            assert_eq!(fs::read_to_string(two.on_server("data.txt")).unwrap(), "precious");
            assert!(fs::symlink_metadata(two.on_server("link")).unwrap().file_type().is_symlink());
        }
        two.there.file("to/data.txt", "precious too");
        two.there.dir("from");
        symlink("../to/data.txt", two.on_server("from/data.txt")).unwrap();
        let (errors, ..) = take(two.remote("from"), two.remote("to"), Answer::Yes);
        assert!(errors[0].contains("a link to its destination"), "{errors:?}");
        assert_eq!(fs::read_to_string(two.on_server("to/data.txt")).unwrap(), "precious too");
    }

    /// Into itself through a link: the text of the paths doesn't show it, the server does.
    #[cfg(unix)]
    #[test]
    fn a_folder_isnt_copied_into_itself_through_a_link() {
        use std::os::unix::fs::symlink;
        let Some(two) = Two::new("remote-into-itself") else { return };
        two.there.file("real/tree/a.txt", "a");
        symlink("real/tree", two.on_server("alias")).unwrap();
        let source = two.remote("real/tree");
        let error = plan(std::slice::from_ref(&source), &two.remote("alias/sub"), false).unwrap_err();
        assert!(error.contains("inside itself"), "{error}");
        // A link to it doesn't go in there either, as locally.
        assert!(plan(&[two.remote("alias")], &two.remote("real/tree/copy"), false).unwrap_err().contains("inside itself"));
    }

    /// A name this job wrote comes back in another letter case: on a server ignoring case that
    /// is the same file, and a move's source has gone. Nothing may go unnoticed, on any disk.
    #[cfg(unix)]
    #[test]
    fn two_names_landing_on_one_lose_nothing() {
        let here = TempDir::new("remote-case-here");
        let there = TempDir::in_target("remote-case-there");
        let Some(connection) = local_connection(there.path()) else { return };
        here.file("tree/Makefile", "capital");
        here.file("tree/makefile", "small");
        let to = Place::There(connection.clone(), ssh::join(&connection.home, "tree"));
        let (errors, ..) = take(Place::Here(here.path().join("tree")), to, Answer::All);
        let arrived = tree(&there.path().join("tree"));
        let left = tree(&here.path().join("tree"));
        assert_eq!(arrived.len() + left.len(), 2, "arrived {arrived:?}, left {left:?}, {errors:?}");
        assert_eq!(errors.is_empty(), left.is_empty(), "{errors:?}");
    }

    /// What a copy's permissions are.
    #[test]
    fn modes_of_copies() {
        let file = |mode: Option<u32>| Item { kind: Kind::File, size: 0, modified: None, mode, owner: None };
        let link = Item { kind: Kind::Link, ..file(Some(0o777)) };
        assert_eq!(mode_of_copy(&file(Some(0o640)), Some(&file(Some(0o600))), false), Some(0o640), "the original's");
        assert_eq!(mode_of_copy(&file(None), Some(&file(Some(0o600))), false), Some(0o600), "from Windows");
        assert_eq!(mode_of_copy(&file(None), None, false), None);
        assert_eq!(mode_of_copy(&file(None), Some(&link), false), None, "a link's mean nothing");
        assert_eq!(mode_of_copy(&file(Some(0o644)), Some(&file(Some(0o664))), true), Some(0o664), "put back after editing");
        assert_eq!(mode_of_copy(&file(Some(0o644)), None, true), Some(0o644));
    }

    /// A folder with no bottom: the delete gives up, and says so.
    #[test]
    fn a_folder_without_a_bottom_ends() {
        let connection = scripted_connection(server_listing(&["inner"]));
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut worker = Worker::new(tx, Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let place = Place::There(connection, "/box".to_string());
        let _ = worker.remove_there(&place, MAX_DEPTH);
        assert!(worker.errors[0].contains("nested too deeply"), "{:?}", worker.errors);
    }

    /// A folder that refuses new files: the move keeps its source, and says why.
    #[cfg(unix)]
    #[test]
    fn a_refused_copy_keeps_the_source() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let Some(two) = Two::new("remote-refused") else { return };
        two.here.file("x.txt", "x");
        let closed = two.there.dir("closed");
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o555)).unwrap();
        let (errors, ..) = take(two.local("x.txt"), two.remote("closed/x.txt"), Answer::Skip);
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(two.here.path().join("x.txt").exists(), "the source stays");
    }

    /// On the one other computer a move is a rename; onto a name that is there it asks, and
    /// onto a folder it merges.
    #[test]
    fn a_move_over_there_is_a_rename() {
        let Some(two) = Two::new("remote-rename") else { return };
        two.there.file("a.txt", "a");
        let (errors, _, questions) = take(two.remote("a.txt"), two.remote("b.txt"), Answer::Cancel);
        assert!(errors.is_empty() && questions == 0, "{errors:?}");
        assert_eq!(tree(two.there.path()), ["b.txt"]);

        two.there.file("c.txt", "c");
        let (_, _, questions) = take(two.remote("c.txt"), two.remote("b.txt"), Answer::No);
        assert_eq!(questions, 1);
        assert_eq!(tree(two.there.path()), ["b.txt", "c.txt"]);
        take(two.remote("c.txt"), two.remote("b.txt"), Answer::Yes);
        assert_eq!(tree(two.there.path()), ["b.txt"]);
        assert_eq!(fs::read_to_string(two.on_server("b.txt")).unwrap(), "c");

        two.there.file("from/one.txt", "1");
        two.there.file("from/sub/two.txt", "2");
        two.there.file("to/sub/three.txt", "3");
        let (errors, ..) = take(two.remote("from"), two.remote("to"), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!two.on_server("from").exists());
        assert_eq!(tree(&two.on_server("to")), ["one.txt", "sub", "sub/three.txt", "sub/two.txt"]);
    }

    /// F5 on one computer goes through cm, and arrives.
    #[test]
    fn a_copy_over_there() {
        let Some(two) = Two::new("remote-copy-there") else { return };
        two.there.file("tree/a.txt", "a");
        let (errors, ..) = copy(two.remote("tree"), two.remote("again"), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(tree(two.there.path()), ["again", "again/a.txt", "tree", "tree/a.txt"]);
    }

    /// Between two other computers.
    #[test]
    fn a_move_between_two_other_computers() {
        let Some(two) = Two::new("remote-two") else { return };
        let third = TempDir::new("remote-third");
        let Some(other) = local_connection(third.path()) else { return };
        two.there.file("tree/a.txt", "a");
        let to = Place::There(other.clone(), ssh::join(&other.home, "tree"));
        let (errors, ..) = take(two.remote("tree"), to, Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!two.on_server("tree").exists());
        assert_eq!(fs::read_to_string(third.path().join("tree/a.txt")).unwrap(), "a");
    }

    /// Too big to arrive before a test can step in, and takes no room while it is empty.
    const HUGE: u64 = 1 << 30;

    /// Stopped in the middle: nothing half-written stays, and what was there is as it was.
    #[test]
    fn a_stopped_copy_leaves_no_trace() {
        let Some(two) = Two::new("remote-stop") else { return };
        File::create(two.here.dir(".").join("big.bin")).unwrap().set_len(HUGE).unwrap();
        two.there.file("big.bin", "old");
        let job = Job::Transfer { pairs: vec![(two.local("big.bin"), two.remote("big.bin"))], moving: true, replace: true };
        let mut running = start(job);
        let cancelled = loop {
            match running.rx.recv_timeout(Duration::from_secs(20)).expect("job hung") {
                Msg::Progress(progress) if progress.bytes_done > 0 => running.stop(),
                Msg::Progress(_) => {}
                Msg::Ask(..) => panic!("nothing to ask"),
                Msg::Done { cancelled, .. } => break cancelled,
            }
        };
        assert!(cancelled);
        assert_eq!(fs::read_to_string(two.on_server("big.bin")).unwrap(), "old");
        assert_eq!(tree(two.there.path()), ["big.bin"], "no leftovers");
        assert!(two.here.path().join("big.bin").exists());
    }

    /// The connection goes in the middle of a move: the job ends with a reason, without asking
    /// about every file, and the source is there.
    #[test]
    fn a_lost_connection_ends_the_job_and_keeps_the_source() {
        let Some(two) = Two::new("remote-lost") else { return };
        File::create(two.here.dir("a").join("big.bin")).unwrap().set_len(HUGE).unwrap();
        two.here.file("a/more.txt", "more");
        let job = Job::Transfer { pairs: vec![(two.local("a"), two.remote("a"))], moving: true, replace: false };
        let running = start(job);
        let (errors, cancelled) = loop {
            match running.rx.recv_timeout(Duration::from_secs(20)).expect("job hung") {
                Msg::Progress(progress) if progress.bytes_done > 0 => kill(&two.connection),
                Msg::Progress(_) => {}
                Msg::Ask(question, _) => panic!("nothing to ask: {question:?}"),
                Msg::Done { errors, cancelled, .. } => break (errors, cancelled),
            }
        };
        assert!(cancelled && !errors.is_empty(), "{errors:?}");
        assert!(errors[0].contains("lost the connection"), "{errors:?}");
        assert_eq!(fs::metadata(two.here.path().join("a/big.bin")).unwrap().len(), HUGE);
        // The small one may have gone first: then it arrived whole.
        let here = two.here.path().join("a/more.txt");
        assert!(here.exists() || fs::read_to_string(two.on_server("a/more.txt")).is_ok_and(|text| text == "more"));
    }

    /// A folder here and "over there" that are one (a share mounted here, of the server's
    /// disk). The copy would replace the file it came from, and deleting the source would then
    /// delete the only copy.
    #[test]
    fn one_folder_by_two_ways_is_left_alone() {
        let Some(two) = Two::new("remote-one") else { return };
        two.there.file("x.txt", "the only copy");
        let local = Place::Here(two.on_server("x.txt"));
        let (errors, _, questions) = take(two.remote("x.txt"), local, Answer::Yes);
        assert_eq!(questions, 1);
        assert!(errors.iter().any(|e| e.contains("left in place")), "{errors:?}");
        assert_eq!(fs::read_to_string(two.on_server("x.txt")).unwrap(), "the only copy");
        assert_eq!(tree(two.there.path()), ["x.txt"]);
    }

    /// Links are copied as links, and deleting never goes through one.
    #[cfg(unix)]
    #[test]
    fn links_stay_links() {
        use std::os::unix::fs::symlink;
        let Some(two) = Two::new("remote-links") else { return };
        two.here.file("keep/precious.txt", "p");
        two.here.dir("tree");
        symlink("../keep", two.here.path().join("tree/to-folder")).unwrap();
        symlink("nowhere", two.here.path().join("tree/broken")).unwrap();
        let (errors, ..) = copy(two.local("tree"), two.remote("tree"), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(fs::read_link(two.on_server("tree/to-folder")).unwrap(), Path::new("../keep"));
        assert_eq!(fs::read_link(two.on_server("tree/broken")).unwrap(), Path::new("nowhere"));

        let (errors, ..) = copy(two.remote("tree"), two.local("back"), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(fs::read_link(two.here.path().join("back/to-folder")).unwrap(), Path::new("../keep"));

        // A link to a folder goes by itself; that folder and what is in it stay.
        two.there.file("keep/precious.txt", "p");
        let job = Job::DeleteThere { connection: two.connection.clone(), paths: vec![ssh::join(&two.connection.home, "tree")] };
        let (errors, ..) = run(job, Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!two.on_server("tree").exists());
        assert_eq!(fs::read_to_string(two.on_server("keep/precious.txt")).unwrap(), "p");

        // A move too takes only the link.
        two.there.dir("moved");
        symlink("../keep", two.on_server("moved/link")).unwrap();
        let (errors, ..) = take(two.remote("moved"), two.local("moved"), Answer::Cancel);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!two.on_server("moved").exists());
        assert!(two.on_server("keep/precious.txt").exists());
    }

    #[test]
    fn deleting_a_tree_over_there() {
        let Some(two) = Two::new("remote-delete") else { return };
        two.there.file("gone/a.txt", "a");
        two.there.file("gone/sub/b.txt", "b");
        two.there.file("stays.txt", "s");
        let paths = vec![ssh::join(&two.connection.home, "gone")];
        let running = start(Job::DeleteThere { connection: two.connection.clone(), paths });
        let mut last = None;
        let errors = loop {
            match running.rx.recv_timeout(Duration::from_secs(20)).expect("job hung") {
                Msg::Progress(progress) => last = Some(progress),
                Msg::Ask(question, _) => panic!("{question:?}"),
                Msg::Done { errors, .. } => break errors,
            }
        };
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(tree(two.there.path()), ["stays.txt"]);
        let last = last.unwrap();
        assert_eq!((last.files_done, last.files_total), (4, 4), "two files and two folders");
    }

    /// A server chooses its names. None of them gets a file anywhere but into the folder it
    /// is copied to, and none goes missing without a word.
    #[test]
    fn names_from_a_server_stay_in_their_folder() {
        let tmp = TempDir::new("remote-hostile");
        let names = &["../evil.txt", "..", ".", "a/b.txt", "/abs.txt", r"..\win.txt", "C:evil.txt", "a:b.txt", "what?.txt", "ok.txt"];
        let connection = scripted_connection(server_listing(names));
        let to = Place::Here(tmp.path().join("out"));
        let (errors, ..) = copy(Place::There(connection, "/box".to_string()), to, Answer::Skip);
        let mut landed = tree(tmp.path());
        landed.retain(|name| name != "out");
        let expected: &[&str] = if cfg!(windows) { &["out/ok.txt"] } else { &[r"out/..\win.txt", "out/C:evil.txt", "out/a:b.txt", "out/ok.txt", "out/what?.txt"] };
        assert_eq!(landed, expected);
        assert_eq!(errors.len(), if cfg!(windows) { 7 } else { 3 }, "{errors:?}");
        assert!(errors.iter().all(|e| e.contains("can't be a name")), "{errors:?}");
    }

    #[test]
    fn what_a_folder_can_hold() {
        let here = Place::Here(PathBuf::from("somewhere"));
        for name in ["plain.txt", "Příliš žluťoučký", "a b", ".hidden"] {
            assert!(here.can_hold(name), "{name}");
        }
        for name in ["", ".", "..", "../x", "a/b", "/x", "x\0y"] {
            assert!(!here.can_hold(name), "{name}");
        }
        assert_eq!(here.can_hold("a:b"), !cfg!(windows));
        assert_eq!(here.can_hold("what?"), !cfg!(windows));
        assert_eq!(here.can_hold(r"a\b"), !cfg!(windows));
    }

    /// The rules of F5, with an end on another computer.
    #[test]
    fn plans() {
        let Some(two) = Two::new("remote-plan") else { return };
        two.here.file("a.txt", "a");
        two.here.file("b.txt", "b");
        two.there.dir("folder");
        two.there.file("file.txt", "f");
        two.there.file("tree/inner/x.txt", "x");
        let shown = |pairs: Vec<(Place, Place)>| -> Vec<PathBuf> { pairs.iter().map(|(_, to)| to.shown()).collect() };
        let a = two.local("a.txt");

        assert_eq!(shown(plan(std::slice::from_ref(&a), &two.remote("new.txt"), false).unwrap()), [two.remote("new.txt").shown()]);
        assert_eq!(shown(plan(std::slice::from_ref(&a), &two.remote("folder"), false).unwrap()), [two.remote("folder/a.txt").shown()]);
        assert_eq!(shown(plan(std::slice::from_ref(&a), &two.remote("new"), true).unwrap()), [two.remote("new/a.txt").shown()]);
        let both = [a.clone(), two.local("b.txt")];
        assert_eq!(plan(&both, &two.remote("folder"), false).unwrap().len(), 2);
        assert!(plan(&both, &two.remote("file.txt"), false).unwrap_err().contains("is a file"));
        let file = two.remote("file.txt");
        assert!(plan(std::slice::from_ref(&file), &file, false).unwrap_err().contains("same as its destination"));
        let tree = two.remote("tree");
        assert!(plan(std::slice::from_ref(&tree), &two.remote("tree/inner"), false).unwrap_err().contains("inside itself"));
        assert!(plan(std::slice::from_ref(&tree), &tree, false).unwrap_err().contains("inside itself"));
        // A name that only starts the same isn't inside.
        assert!(plan(std::slice::from_ref(&tree), &two.remote("tree2"), false).is_ok());
    }

    /// A question shows the file by its address, whichever end it is at.
    #[test]
    fn questions_name_the_place() {
        let Some(two) = Two::new("remote-question") else { return };
        two.here.file("x.txt", "new");
        two.there.file("x.txt", "old");
        let job = Job::Transfer { pairs: vec![(two.local("x.txt"), two.remote("x.txt"))], moving: false, replace: false };
        let running = start(job);
        loop {
            match running.rx.recv_timeout(Duration::from_secs(20)).expect("job hung") {
                Msg::Progress(_) => {}
                Msg::Ask(Question::Overwrite { target, source, existing }, reply) => {
                    assert_eq!(target, two.remote("x.txt").shown());
                    assert_eq!((source.size, existing.size), (3, 3));
                    reply.send(Answer::No).unwrap();
                }
                Msg::Ask(other, _) => panic!("{other:?}"),
                Msg::Done { .. } => break,
            }
        }
    }
}
