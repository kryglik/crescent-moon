//! A client for SFTP version 3, the file protocol of OpenSSH, over any pair of pipes (`ssh`
//! gets them from the `ssh` program). Requests go out with a number and each answer goes to
//! whoever waits for that number, so a job and the panels can share one connection, and a
//! transfer can have many requests under way, which is what makes it fast over a distance.

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const INIT: u8 = 1;
const VERSION: u8 = 2;
const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const LSTAT: u8 = 7;
const FSTAT: u8 = 8;
const SETSTAT: u8 = 9;
const FSETSTAT: u8 = 10;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const REMOVE: u8 = 13;
const MKDIR: u8 = 14;
const RMDIR: u8 = 15;
const REALPATH: u8 = 16;
const STAT: u8 = 17;
const RENAME: u8 = 18;
const READLINK: u8 = 19;
const SYMLINK: u8 = 20;
const STATUS: u8 = 101;
const HANDLE: u8 = 102;
const DATA: u8 = 103;
const NAME: u8 = 104;
const ATTRS: u8 = 105;
const EXTENDED: u8 = 200;
const EXTENDED_REPLY: u8 = 201;

const STATUS_OK: u32 = 0;
const STATUS_EOF: u32 = 1;
const STATUS_NO_SUCH_FILE: u32 = 2;
const STATUS_PERMISSION_DENIED: u32 = 3;
const STATUS_FAILURE: u32 = 4;
const STATUS_UNSUPPORTED: u32 = 8;

const HAS_SIZE: u32 = 0x1;
const HAS_OWNER: u32 = 0x2;
const HAS_MODE: u32 = 0x4;
const HAS_TIMES: u32 = 0x8;
const HAS_EXTENDED: u32 = 0x8000_0000;

const OPEN_READ: u32 = 0x1;
const OPEN_WRITE: u32 = 0x2;
const OPEN_CREATE: u32 = 0x8;
const OPEN_EXCLUSIVE: u32 = 0x20;

const FILE_TYPE: u32 = 0o170000;
const REGULAR: u32 = 0o100000;
const FOLDER: u32 = 0o040000;
const LINK: u32 = 0o120000;

/// The size of a piece of a file, and how many may be under way: as OpenSSH's own client.
const PIECE: usize = 32 * 1024;
const WINDOW: usize = 64;
/// The most names of a folder cm takes: a server may go on listing for ever.
const MOST_NAMES: usize = 2_000_000;
/// What the first answer tells a server from a program printing text: OpenSSH's own limit is a
/// quarter of this, and text reads as a length far beyond it.
const LONGEST_PACKET: usize = 1 << 20;

thread_local! {
    /// How long a request of this thread may take; for ever if not said.
    static PATIENCE: Cell<Option<Duration>> = const { Cell::new(None) };
}

/// This thread gives up on a request after `limit`: the screen shouldn't wait for a network
/// that has gone quiet. Jobs have all the time they need, and `ssh` ends a dead connection.
pub fn be_impatient(limit: Duration) {
    PATIENCE.set(Some(limit));
}

/// How long a request of this thread may take.
pub fn patience() -> Option<Duration> {
    PATIENCE.get()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a file or folder is like; a server may leave any of it out.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Attrs {
    pub size: Option<u64>,
    /// User and group, as numbers.
    pub owner: Option<(u32, u32)>,
    /// Permissions and, in what a server reports, the type of file.
    pub mode: Option<u32>,
    /// Last access and last change, in seconds.
    pub times: Option<(u32, u32)>,
}

impl Attrs {
    fn is(&self, kind: u32) -> bool {
        self.mode.is_some_and(|mode| mode & FILE_TYPE == kind)
    }

    pub fn is_dir(&self) -> bool {
        self.is(FOLDER)
    }

    pub fn is_link(&self) -> bool {
        self.is(LINK)
    }

    pub fn is_file(&self) -> bool {
        self.is(REGULAR)
    }

    pub fn is_executable(&self) -> bool {
        self.is_file() && self.mode.is_some_and(|mode| mode & 0o111 != 0)
    }

    pub fn modified(&self) -> Option<SystemTime> {
        self.times.map(|(_, modified)| UNIX_EPOCH + Duration::from_secs(modified.into()))
    }

    /// What setting `time` as a file's modified time takes.
    pub fn with_modified(mut self, time: SystemTime) -> Attrs {
        let seconds = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs().min(u32::MAX.into()) as u32);
        self.times = Some((seconds, seconds));
        self
    }
}

/// An entry of a folder. A name that isn't UTF-8 comes lossily, and the server won't know it
/// by that name, so working on it fails and says so.
#[derive(Clone, Debug)]
pub struct Name {
    pub name: String,
    pub attrs: Attrs,
}

/// Free and total bytes of a disk, and a number that is the same all over that disk.
#[derive(Clone, Copy, Debug)]
pub struct Space {
    pub free: u64,
    pub total: u64,
    pub id: u64,
}

/// An open file or folder on the server.
#[derive(Clone, Debug)]
pub struct Handle(Vec<u8>);

#[derive(Default)]
struct Body(Vec<u8>);

impl Body {
    fn u32(mut self, value: u32) -> Body {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn u64(mut self, value: u64) -> Body {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn bytes(self, value: &[u8]) -> Body {
        let mut body = self.u32(value.len() as u32);
        body.0.extend_from_slice(value);
        body
    }

    fn text(self, value: &str) -> Body {
        self.bytes(value.as_bytes())
    }

    fn attrs(self, attrs: &Attrs) -> Body {
        let flags = [
            (attrs.size.is_some(), HAS_SIZE),
            (attrs.owner.is_some(), HAS_OWNER),
            (attrs.mode.is_some(), HAS_MODE),
            (attrs.times.is_some(), HAS_TIMES),
        ];
        let mut body = self.u32(flags.iter().filter(|(present, _)| *present).map(|(_, flag)| flag).sum());
        if let Some(size) = attrs.size {
            body = body.u64(size);
        }
        if let Some((user, group)) = attrs.owner {
            body = body.u32(user).u32(group);
        }
        if let Some(mode) = attrs.mode {
            body = body.u32(mode);
        }
        if let Some((accessed, modified)) = attrs.times {
            body = body.u32(accessed).u32(modified);
        }
        body
    }
}

/// Reads the fields of an answer.
struct Fields<'a>(&'a [u8]);

fn garbled() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "the server's answer makes no sense")
}

impl<'a> Fields<'a> {
    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < count {
            return Err(garbled());
        }
        let (taken, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(taken)
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| garbled())?))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| garbled())?))
    }

    fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let count = self.u32()? as usize;
        self.take(count)
    }

    fn attrs(&mut self) -> io::Result<Attrs> {
        let flags = self.u32()?;
        let mut attrs = Attrs::default();
        if flags & HAS_SIZE != 0 {
            attrs.size = Some(self.u64()?);
        }
        if flags & HAS_OWNER != 0 {
            attrs.owner = Some((self.u32()?, self.u32()?));
        }
        if flags & HAS_MODE != 0 {
            attrs.mode = Some(self.u32()?);
        }
        if flags & HAS_TIMES != 0 {
            attrs.times = Some((self.u32()?, self.u32()?));
        }
        if flags & HAS_EXTENDED != 0 {
            for _ in 0..self.u32()? {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(attrs)
    }
}

struct Reply {
    id: u32,
    kind: u8,
    body: Vec<u8>,
}

impl Reply {
    fn fields(&self) -> Fields<'_> {
        Fields(&self.body)
    }

    /// The error a status answer stands for; Ok for `STATUS_OK`.
    fn status(&self) -> io::Result<()> {
        if self.kind != STATUS {
            return Err(garbled());
        }
        let mut fields = self.fields();
        let code = fields.u32()?;
        if code == STATUS_OK {
            return Ok(());
        }
        let said = fields.bytes().map(|text| String::from_utf8_lossy(text).into_owned()).unwrap_or_default();
        let (kind, usual) = match code {
            STATUS_EOF => (io::ErrorKind::UnexpectedEof, "end of the file"),
            STATUS_NO_SUCH_FILE => (io::ErrorKind::NotFound, "no such file"),
            STATUS_PERMISSION_DENIED => (io::ErrorKind::PermissionDenied, "permission denied"),
            STATUS_UNSUPPORTED => (io::ErrorKind::Unsupported, "the server can't do that"),
            _ => (io::ErrorKind::Other, "the server says it failed"),
        };
        Err(io::Error::new(kind, if said.is_empty() { usual.to_string() } else { said }))
    }

    fn is_eof(&self) -> bool {
        self.kind == STATUS && self.fields().u32().is_ok_and(|code| code == STATUS_EOF)
    }

    /// Whether this is the status "failure", which is all a server says about most refusals.
    fn is_plain_failure(&self) -> bool {
        self.kind == STATUS && self.fields().u32().is_ok_and(|code| code == STATUS_FAILURE)
    }

    fn handle(&self) -> io::Result<Handle> {
        if self.kind != HANDLE {
            self.status()?;
            return Err(garbled());
        }
        Ok(Handle(self.fields().bytes()?.to_vec()))
    }

    fn attrs(&self) -> io::Result<Attrs> {
        if self.kind != ATTRS {
            self.status()?;
            return Err(garbled());
        }
        self.fields().attrs()
    }

    /// The one name of an answer to REALPATH or READLINK.
    fn one_name(&self) -> io::Result<String> {
        if self.kind != NAME {
            self.status()?;
            return Err(garbled());
        }
        let mut fields = self.fields();
        if fields.u32()? != 1 {
            return Err(garbled());
        }
        Ok(String::from_utf8_lossy(fields.bytes()?).into_owned())
    }
}

#[derive(Default)]
struct Shared {
    waiting: HashMap<u32, Sender<Reply>>,
    /// Why the connection is gone.
    lost: Option<String>,
}

pub struct Sftp {
    writer: Mutex<Box<dyn Write + Send>>,
    shared: Arc<Mutex<Shared>>,
    next_id: AtomicU32,
    extensions: HashSet<String>,
}

fn read_packet(reader: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if !(1..=LONGEST_PACKET).contains(&length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the other side doesn't speak SFTP. Does something print text as a shell starts there (~/.bashrc)?",
        ));
    }
    let mut packet = vec![0; length];
    reader.read_exact(&mut packet)?;
    let body = packet.split_off(1);
    Ok((packet[0], body))
}

/// Hands the answers that come in to whoever waits for them, until the connection ends.
fn read_replies(mut reader: impl Read, shared: Arc<Mutex<Shared>>) {
    let why = loop {
        match read_packet(&mut reader) {
            Ok((kind, body)) if body.len() >= 4 => {
                let id = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                // Nobody waiting: the transfer it was for has ended.
                if let Some(to) = lock(&shared).waiting.remove(&id) {
                    let _ = to.send(Reply { id, kind, body: body[4..].to_vec() });
                }
            }
            Ok(_) => break garbled().to_string(),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break "the connection ended".to_string(),
            Err(e) => break e.to_string(),
        }
    };
    let mut shared = lock(&shared);
    shared.lost = Some(why);
    shared.waiting.clear();
}

impl Sftp {
    /// Starts the protocol on these pipes. This waits for the server's first answer: as long
    /// as `ssh` needs to log in, however long that takes.
    pub fn start(mut reader: impl Read + Send + 'static, mut writer: impl Write + Send + 'static) -> io::Result<Sftp> {
        let hello = Body::default().u32(5).0;
        let mut packet = hello;
        packet.push(INIT);
        packet.extend_from_slice(&3u32.to_be_bytes());
        writer.write_all(&packet)?;
        writer.flush()?;
        let (kind, body) = read_packet(&mut reader)?;
        if kind != VERSION {
            return Err(garbled());
        }
        let mut fields = Fields(&body);
        if fields.u32()? < 3 {
            return Err(io::Error::other("the server speaks an older SFTP than version 3"));
        }
        let mut extensions = HashSet::new();
        while !fields.0.is_empty() {
            extensions.insert(String::from_utf8_lossy(fields.bytes()?).into_owned());
            fields.bytes()?;
        }
        let shared = Arc::new(Mutex::new(Shared::default()));
        let their_shared = shared.clone();
        thread::spawn(move || read_replies(reader, their_shared));
        Ok(Sftp { writer: Mutex::new(Box::new(writer)), shared, next_id: AtomicU32::new(1), extensions })
    }

    /// Why the connection is gone, if it is.
    pub fn lost(&self) -> Option<String> {
        lock(&self.shared).lost.clone()
    }

    fn gone(&self) -> io::Error {
        let why = self.lost().unwrap_or_else(|| "the connection ended".to_string());
        io::Error::new(io::ErrorKind::ConnectionAborted, format!("lost the connection: {why}"))
    }

    /// Sends a request; its answer will go to `to`.
    fn send(&self, kind: u8, body: &[u8], to: Sender<Reply>) -> io::Result<u32> {
        let mut packet = Vec::with_capacity(body.len() + 9);
        let id = self.queue(kind, body, to, &mut packet)?;
        self.send_queued(&packet)?;
        Ok(id)
    }

    /// Adds a request to `packets`, for `send_queued`: going down the pipe in one write, they
    /// cost `ssh` one wake-up, which on Windows is what limits how fast it takes them.
    fn queue(&self, kind: u8, body: &[u8], to: Sender<Reply>, packets: &mut Vec<u8>) -> io::Result<u32> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut shared = lock(&self.shared);
            if shared.lost.is_some() {
                drop(shared);
                return Err(self.gone());
            }
            shared.waiting.insert(id, to);
        }
        packets.extend_from_slice(&(body.len() as u32 + 5).to_be_bytes());
        packets.push(kind);
        packets.extend_from_slice(&id.to_be_bytes());
        packets.extend_from_slice(body);
        Ok(id)
    }

    fn send_queued(&self, packets: &[u8]) -> io::Result<()> {
        let mut writer = lock(&self.writer);
        if let Err(e) = writer.write_all(packets).and_then(|()| writer.flush()) {
            drop(writer);
            let mut shared = lock(&self.shared);
            shared.lost.get_or_insert_with(|| e.to_string());
            shared.waiting.clear();
            drop(shared);
            return Err(self.gone());
        }
        Ok(())
    }

    /// The next answer on `rx`. The connection ending wakes this within a moment, as a transfer
    /// keeps a sender of its own alive.
    fn wait(&self, rx: &Receiver<Reply>) -> io::Result<Reply> {
        let deadline = PATIENCE.get().map(|limit| Instant::now() + limit);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(reply) => return Ok(reply),
                Err(RecvTimeoutError::Disconnected) => return Err(self.gone()),
                Err(RecvTimeoutError::Timeout) if self.lost().is_some() => return Err(self.gone()),
                Err(RecvTimeoutError::Timeout) => {
                    if deadline.is_some_and(|at| Instant::now() >= at) {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "the server doesn't answer"));
                    }
                }
            }
        }
    }

    fn call(&self, kind: u8, body: Body) -> io::Result<Reply> {
        let (tx, rx) = mpsc::channel();
        self.send(kind, &body.0, tx)?;
        self.wait(&rx)
    }

    fn extended(&self, name: &str, body: impl FnOnce(Body) -> Body) -> io::Result<Reply> {
        self.call(EXTENDED, body(Body::default().text(name)))
    }

    /// `path` as the server reads it, absolute (`.` is the folder it started in: home).
    pub fn realpath(&self, path: &str) -> io::Result<String> {
        self.call(REALPATH, Body::default().text(path))?.one_name()
    }

    /// What `path` is, following a link.
    pub fn stat(&self, path: &str) -> io::Result<Attrs> {
        self.call(STAT, Body::default().text(path))?.attrs()
    }

    /// What `path` is, a link being a link.
    pub fn lstat(&self, path: &str) -> io::Result<Attrs> {
        self.call(LSTAT, Body::default().text(path))?.attrs()
    }

    /// What each of `paths` is, following links, with many requests under way: one at a time,
    /// a folder of links takes as many times there and back.
    pub fn stat_all(&self, paths: &[String]) -> Vec<io::Result<Attrs>> {
        let (tx, rx) = mpsc::channel();
        let mut found: Vec<Option<io::Result<Attrs>>> = paths.iter().map(|_| None).collect();
        let mut asked: HashMap<u32, usize> = HashMap::new();
        let mut next = 0;
        let failure = loop {
            let mut packets = Vec::new();
            while next < paths.len() && asked.len() < WINDOW {
                match self.queue(STAT, &Body::default().text(&paths[next]).0, tx.clone(), &mut packets) {
                    Ok(id) => asked.insert(id, next),
                    // `self.gone` below says why.
                    Err(_) => break,
                };
                next += 1;
            }
            if let Err(e) = self.send_queued(&packets) {
                break e;
            }
            if asked.is_empty() {
                if next >= paths.len() {
                    return found.into_iter().map(|one| one.unwrap_or_else(|| Err(self.gone()))).collect();
                }
                break self.gone();
            }
            match self.wait(&rx) {
                Ok(reply) => {
                    if let Some(at) = asked.remove(&reply.id) {
                        found[at] = Some(reply.attrs());
                    }
                }
                Err(e) => break e,
            }
        };
        found.into_iter().map(|one| one.unwrap_or_else(|| Err(io::Error::new(failure.kind(), failure.to_string())))).collect()
    }

    pub fn fstat(&self, handle: &Handle) -> io::Result<Attrs> {
        self.call(FSTAT, Body::default().bytes(&handle.0))?.attrs()
    }

    pub fn set_attrs(&self, path: &str, attrs: &Attrs) -> io::Result<()> {
        self.call(SETSTAT, Body::default().text(path).attrs(attrs))?.status()
    }

    pub fn set_handle_attrs(&self, handle: &Handle, attrs: &Attrs) -> io::Result<()> {
        self.call(FSETSTAT, Body::default().bytes(&handle.0).attrs(attrs))?.status()
    }

    /// What is in a folder, without `.` and `..`.
    pub fn read_dir(&self, path: &str) -> io::Result<Vec<Name>> {
        let handle = self.call(OPENDIR, Body::default().text(path))?.handle()?;
        let names = self.read_names(&handle);
        // A failure to close says nothing about what was read.
        let _ = self.close(&handle);
        names
    }

    fn read_names(&self, handle: &Handle) -> io::Result<Vec<Name>> {
        // Each answer may come in time, and the list still never end.
        let deadline = PATIENCE.get().map(|limit| Instant::now() + limit * 6);
        let mut names = Vec::new();
        loop {
            if deadline.is_some_and(|at| Instant::now() >= at) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the server doesn't finish listing this folder"));
            }
            if names.len() > MOST_NAMES {
                return Err(io::Error::other("the server lists more names than cm takes in one folder"));
            }
            let reply = self.call(READDIR, Body::default().bytes(&handle.0))?;
            if reply.is_eof() {
                return Ok(names);
            }
            if reply.kind != NAME {
                reply.status()?;
                return Err(garbled());
            }
            let mut fields = reply.fields();
            for _ in 0..fields.u32()? {
                let name = String::from_utf8_lossy(fields.bytes()?).into_owned();
                // As `ls -l` shows it, for people.
                fields.bytes()?;
                let attrs = fields.attrs()?;
                if name != "." && name != ".." {
                    names.push(Name { name, attrs });
                }
            }
        }
    }

    pub fn mkdir(&self, path: &str, mode: Option<u32>) -> io::Result<()> {
        self.call(MKDIR, Body::default().text(path).attrs(&Attrs { mode, ..Attrs::default() }))?.status()
    }

    pub fn rmdir(&self, path: &str) -> io::Result<()> {
        self.call(RMDIR, Body::default().text(path))?.status()
    }

    /// Removes a file or a link (never what the link leads to).
    pub fn remove(&self, path: &str) -> io::Result<()> {
        self.call(REMOVE, Body::default().text(path))?.status()
    }

    /// Renames, refusing to land on something that exists. Ok(false): the server only said it
    /// failed, as it does between two disks.
    pub fn rename(&self, from: &str, to: &str) -> io::Result<bool> {
        let reply = self.call(RENAME, Body::default().text(from).text(to))?;
        if reply.is_plain_failure() {
            return Ok(false);
        }
        reply.status().map(|()| true)
    }

    /// Whether `replace` can be asked for.
    pub fn can_replace(&self) -> bool {
        self.extensions.contains("posix-rename@openssh.com")
    }

    /// Renames in one step, replacing what is there; no moment without either. Ok(false) as
    /// for `rename`.
    pub fn replace(&self, from: &str, to: &str) -> io::Result<bool> {
        let reply = self.extended("posix-rename@openssh.com", |body| body.text(from).text(to))?;
        if reply.is_plain_failure() {
            return Ok(false);
        }
        reply.status().map(|()| true)
    }

    pub fn read_link(&self, path: &str) -> io::Result<String> {
        self.call(READLINK, Body::default().text(path))?.one_name()
    }

    /// Makes the link `link`, leading to `target`. (OpenSSH has these the other way round from
    /// the protocol's text, and being the one that counts, everybody follows it.)
    pub fn symlink(&self, target: &str, link: &str) -> io::Result<()> {
        self.call(SYMLINK, Body::default().text(target).text(link))?.status()
    }

    /// Opens a file to read.
    pub fn open(&self, path: &str) -> io::Result<Handle> {
        self.call(OPEN, Body::default().text(path).u32(OPEN_READ).attrs(&Attrs::default()))?.handle()
    }

    /// Makes a new file to write; one that is there already is an error, never replaced.
    pub fn create(&self, path: &str, mode: Option<u32>) -> io::Result<Handle> {
        let flags = OPEN_WRITE | OPEN_CREATE | OPEN_EXCLUSIVE;
        self.call(OPEN, Body::default().text(path).u32(flags).attrs(&Attrs { mode, ..Attrs::default() }))?.handle()
    }

    /// Closes a file; a full or failing disk may only say so now.
    pub fn close(&self, handle: &Handle) -> io::Result<()> {
        self.call(CLOSE, Body::default().bytes(&handle.0))?.status()
    }

    /// Asks for the file to be on the server's disk. A server that can't do this isn't asked.
    pub fn sync(&self, handle: &Handle) -> io::Result<()> {
        if !self.extensions.contains("fsync@openssh.com") {
            return Ok(());
        }
        self.extended("fsync@openssh.com", |body| body.bytes(&handle.0))?.status()
    }

    /// The disk `path` is on, if the server tells.
    pub fn space(&self, path: &str) -> Option<Space> {
        if !self.extensions.contains("statvfs@openssh.com") {
            return None;
        }
        let reply = self.extended("statvfs@openssh.com", |body| body.text(path)).ok()?;
        if reply.kind != EXTENDED_REPLY {
            return None;
        }
        let mut fields = reply.fields();
        let mut next = || fields.u64().ok();
        let (_block, fragment, blocks, _free, available) = (next()?, next()?, next()?, next()?, next()?);
        let (_files, _files_free, _files_available, id) = (next()?, next()?, next()?, next()?);
        Some(Space { free: available.saturating_mul(fragment), total: blocks.saturating_mul(fragment), id })
    }
}

/// A request for a piece of a file.
struct Asked {
    id: u32,
    offset: u64,
    length: u32,
}

/// Reads a file in order, with many requests under way.
pub struct Download {
    sftp: Arc<Sftp>,
    handle: Handle,
    /// The size it had: past that one request looks for more, or for the end.
    size: u64,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    asked: VecDeque<Asked>,
    arrived: HashMap<u32, Reply>,
    next: u64,
    ended: bool,
}

impl Download {
    pub fn new(sftp: Arc<Sftp>, handle: Handle, size: u64) -> Download {
        let (tx, rx) = mpsc::channel();
        Download { sftp, handle, size, tx, rx, asked: VecDeque::new(), arrived: HashMap::new(), next: 0, ended: false }
    }

    fn ask(&mut self, offset: u64, length: u32) -> io::Result<Asked> {
        let body = Body::default().bytes(&self.handle.0).u64(offset).u32(length);
        let id = self.sftp.send(READ, &body.0, self.tx.clone())?;
        Ok(Asked { id, offset, length })
    }

    /// The next piece of the file; None at its end.
    pub fn next_piece(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.ended {
            return Ok(None);
        }
        while self.asked.len() < WINDOW && (self.next < self.size || self.asked.is_empty()) {
            let asked = self.ask(self.next, PIECE as u32)?;
            self.next += PIECE as u64;
            self.asked.push_back(asked);
        }
        let Some(front) = self.asked.pop_front() else { return Ok(None) };
        let reply = loop {
            if let Some(reply) = self.arrived.remove(&front.id) {
                break reply;
            }
            let reply = self.sftp.wait(&self.rx)?;
            self.arrived.insert(reply.id, reply);
        };
        if reply.is_eof() {
            self.ended = true;
            return Ok(None);
        }
        if reply.kind != DATA {
            reply.status()?;
            return Err(garbled());
        }
        let data = reply.fields().bytes()?.to_vec();
        if data.is_empty() || data.len() > front.length as usize {
            return Err(garbled());
        }
        // Less than was asked for, which isn't the end: the rest comes next, before the pieces
        // already asked for that lie behind it.
        if data.len() < front.length as usize {
            let rest = self.ask(front.offset + data.len() as u64, front.length - data.len() as u32)?;
            self.asked.push_front(rest);
        }
        Ok(Some(data))
    }

    pub fn close(self) {
        let _ = self.sftp.close(&self.handle);
    }
}

/// Writes a new file from its start, with many requests under way.
pub struct Upload {
    sftp: Arc<Sftp>,
    pub handle: Handle,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    under_way: usize,
    offset: u64,
}

impl Upload {
    pub fn new(sftp: Arc<Sftp>, handle: Handle) -> Upload {
        let (tx, rx) = mpsc::channel();
        Upload { sftp, handle, tx, rx, under_way: 0, offset: 0 }
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let mut packets = Vec::with_capacity(data.len() + 1024);
        for piece in data.chunks(PIECE) {
            // Answers already in: a refusal stops the writing as soon as it is known.
            while let Ok(reply) = self.rx.try_recv() {
                self.under_way -= 1;
                reply.status()?;
            }
            // Before waiting for answers, what they are to come from has to be on its way.
            if self.under_way >= WINDOW {
                self.sftp.send_queued(&packets)?;
                packets.clear();
            }
            while self.under_way >= WINDOW {
                self.settle_one()?;
            }
            let body = Body::default().bytes(&self.handle.0).u64(self.offset).bytes(piece);
            self.sftp.queue(WRITE, &body.0, self.tx.clone(), &mut packets)?;
            self.under_way += 1;
            self.offset += piece.len() as u64;
        }
        self.sftp.send_queued(&packets)
    }

    fn settle_one(&mut self) -> io::Result<()> {
        let reply = self.sftp.wait(&self.rx)?;
        self.under_way -= 1;
        reply.status()
    }

    /// Waits until the server has taken everything.
    pub fn finish(&mut self) -> io::Result<()> {
        while self.under_way > 0 {
            self.settle_one()?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    /// OpenSSH's server program, started in `dir`, where it is installed: tests that need one
    /// pass without it. On Windows that is Git's, which needs Git's DLLs and has its paths as
    /// `/c/Users/…`.
    pub fn server_command(dir: &Path) -> Option<Command> {
        let program = [
            "/usr/lib/openssh/sftp-server",
            "/usr/lib/sftp-server",
            "/usr/libexec/openssh/sftp-server",
            "/usr/libexec/sftp-server",
            "/usr/lib/ssh/sftp-server",
            r"C:\Program Files\Git\usr\lib\ssh\sftp-server.exe",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())?;
        let mut command = Command::new(program);
        command.current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        if cfg!(windows) {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let dirs = std::iter::once(PathBuf::from(r"C:\Program Files\Git\usr\bin")).chain(std::env::split_paths(&path));
            command.env("PATH", std::env::join_paths(dirs).ok()?);
        }
        Some(command)
    }

    /// A real server, started as a program in `dir`, speaking on its pipes as it would for `ssh`.
    fn server(dir: &Path) -> Option<(Child, Arc<Sftp>)> {
        let mut child = server_command(dir)?.spawn().ok()?;
        let (stdout, stdin) = (child.stdout.take()?, child.stdin.take()?);
        let sftp = Sftp::start(stdout, stdin).expect("the server starts");
        Some((child, Arc::new(sftp)))
    }

    /// Everything a job uses, against OpenSSH's own server.
    #[test]
    fn files_and_folders_on_a_real_server() {
        let tmp = TempDir::new("sftp-real");
        let Some((mut child, sftp)) = server(tmp.path()) else { return };
        let home = sftp.realpath(".").unwrap();
        let at = |name: &str| format!("{home}/{name}");

        sftp.mkdir(&at("folder"), Some(0o755)).unwrap();
        assert!(sftp.mkdir(&at("folder"), None).is_err(), "it is there");
        assert!(sftp.stat(&at("folder")).unwrap().is_dir());
        assert_eq!(sftp.stat(&at("missing")).unwrap_err().kind(), io::ErrorKind::NotFound);

        // A file bigger than the window, so many requests are under way at once.
        let content: Vec<u8> = (0..(PIECE * WINDOW * 3 + 1234) as u32).map(|i| (i ^ (i >> 8)) as u8).collect();
        let path = at("folder/Příliš žluťoučký.bin");
        let handle = sftp.create(&path, Some(0o600)).unwrap();
        let mut upload = Upload::new(sftp.clone(), handle.clone());
        upload.write(&content).unwrap();
        upload.finish().unwrap();
        assert_eq!(sftp.fstat(&handle).unwrap().size, Some(content.len() as u64));
        sftp.sync(&handle).unwrap();
        let when = UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        sftp.set_handle_attrs(&handle, &Attrs::default().with_modified(when)).unwrap();
        sftp.close(&handle).unwrap();
        assert!(sftp.create(&path, None).is_err(), "a new file never replaces one");

        let attrs = sftp.stat(&path).unwrap();
        assert!(attrs.is_file() && !attrs.is_link());
        assert_eq!((attrs.size, attrs.modified()), (Some(content.len() as u64), Some(when)));
        let mut download = Download::new(sftp.clone(), sftp.open(&path).unwrap(), content.len() as u64);
        let mut read = Vec::new();
        while let Some(piece) = download.next_piece().unwrap() {
            read.extend_from_slice(&piece);
        }
        download.close();
        assert!(read == content, "it comes back as it went, {} bytes", read.len());

        let names = sftp.read_dir(&at("folder")).unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].name, "Příliš žluťoučký.bin");
        assert!(names[0].attrs.is_file());

        // A file growing or shrinking behind the size it had still reads to its end.
        for size in [0, 10, content.len() as u64 * 2] {
            let mut download = Download::new(sftp.clone(), sftp.open(&path).unwrap(), size);
            let mut total = 0;
            while let Some(piece) = download.next_piece().unwrap() {
                total += piece.len();
            }
            download.close();
            assert_eq!(total, content.len(), "told the size was {size}");
        }

        // Renames: one that lands on something is refused, the other kind replaces it.
        let other = at("other.txt");
        let handle = sftp.create(&other, None).unwrap();
        sftp.close(&handle).unwrap();
        assert!(!sftp.rename(&other, &path).unwrap() || sftp.stat(&other).is_ok(), "it doesn't replace");
        assert!(sftp.can_replace());
        assert!(sftp.replace(&other, &path).unwrap());
        assert_eq!(sftp.stat(&path).unwrap().size, Some(0));
        assert!(sftp.stat(&other).is_err());
        assert!(sftp.rename(&path, &other).unwrap());

        // A link is a link to lstat and remove, and what it leads to stays. (Git's server on
        // Windows makes a copy for a link.)
        if cfg!(unix) {
            sftp.symlink("other.txt", &at("link")).unwrap();
            assert!(sftp.lstat(&at("link")).unwrap().is_link());
            assert!(sftp.stat(&at("link")).unwrap().is_file());
            assert_eq!(sftp.read_link(&at("link")).unwrap(), "other.txt");
            sftp.remove(&at("link")).unwrap();
            assert!(sftp.stat(&other).is_ok());
        }

        assert!(sftp.rmdir(&at("folder")).is_ok());
        sftp.remove(&other).unwrap();
        assert!(sftp.read_dir(&home).unwrap().is_empty());
        let space = sftp.space(&home).expect("OpenSSH tells the free space");
        assert!(space.total >= space.free && space.total > 0);

        let _ = child.kill();
        let _ = child.wait();
    }

    /// A server that goes away ends what waits for it, soon and with a reason, and whatever
    /// comes after finds out at once.
    #[test]
    fn a_lost_connection_ends_every_request() {
        let tmp = TempDir::new("sftp-lost");
        tmp.file("big.bin", &"x".repeat(1 << 20));
        let Some((mut child, sftp)) = server(tmp.path()) else { return };
        assert!(sftp.lost().is_none());
        let home = sftp.realpath(".").unwrap();
        let mut download = Download::new(sftp.clone(), sftp.open(&format!("{home}/big.bin")).unwrap(), 1 << 20);
        let _ = download.next_piece().unwrap();
        child.kill().unwrap();
        let _ = child.wait();
        let deadline = Instant::now() + Duration::from_secs(5);
        while sftp.lost().is_none() {
            assert!(Instant::now() < deadline, "nobody noticed");
            thread::sleep(Duration::from_millis(5));
        }
        let error = sftp.stat(&home).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted, "{error}");
        assert!(error.to_string().contains("connection"), "{error}");
        loop {
            match download.next_piece() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("a lost file doesn't just end"),
                Err(e) => break assert_eq!(e.kind(), io::ErrorKind::ConnectionAborted, "{e}"),
            }
        }
    }

    /// A server with one folder, `/box`, listing `names`, each a file of one byte: it says what
    /// it likes, as a server that isn't to be trusted would.
    pub fn server_listing(names: &'static [&'static str]) -> Arc<Sftp> {
        let mut listed = false;
        scripted(move |kind, id, body| {
            let folder = Attrs { mode: Some(0o040755), ..Attrs::default() };
            let file = Attrs { size: Some(1), mode: Some(0o100644), ..Attrs::default() };
            let status = |code| answer(STATUS, id, Body::default().u32(code).text("").text(""));
            let mut fields = Fields(body);
            vec![match kind {
                REALPATH => answer(NAME, id, Body::default().u32(1).text("/box").text("").attrs(&folder)),
                LSTAT | STAT => {
                    let path = fields.bytes().unwrap();
                    answer(ATTRS, id, Body::default().attrs(if path == b"/box" { &folder } else { &file }))
                }
                OPENDIR | OPEN => {
                    listed = false;
                    answer(HANDLE, id, Body::default().bytes(b"handle"))
                }
                READDIR if !listed => {
                    listed = true;
                    let list = Body::default().u32(names.len() as u32);
                    answer(NAME, id, names.iter().fold(list, |list, name| list.text(name).text("").attrs(&file)))
                }
                READ => {
                    fields.bytes().unwrap();
                    if fields.u64().unwrap() == 0 { answer(DATA, id, Body::default().bytes(b"x")) } else { status(STATUS_EOF) }
                }
                READDIR => status(STATUS_EOF),
                CLOSE => status(STATUS_OK),
                _ => status(STATUS_FAILURE),
            }]
        })
    }

    /// `stat_all` gives each path its own answer, in the order asked, for more than a window's
    /// worth of paths.
    #[test]
    fn many_stats_at_once() {
        let tmp = TempDir::new("sftp-stats");
        let count = WINDOW * 2 + 3;
        for i in (0..count).step_by(2) {
            tmp.file(&format!("{i}.txt"), &"x".repeat(i));
        }
        let Some((mut child, sftp)) = server(tmp.path()) else { return };
        let home = sftp.realpath(".").unwrap();
        let paths: Vec<String> = (0..count).map(|i| format!("{home}/{i}.txt")).collect();
        let found = sftp.stat_all(&paths);
        assert_eq!(found.len(), count);
        for (i, one) in found.iter().enumerate() {
            match one {
                Ok(attrs) => assert_eq!((i % 2, attrs.size), (0, Some(i as u64))),
                Err(e) => assert_eq!((i % 2, e.kind()), (1, io::ErrorKind::NotFound)),
            }
        }
        assert!(sftp.stat_all(&[]).is_empty());
        let _ = child.kill();
        let _ = child.wait();
    }

    /// A server that keeps listing doesn't hold the screen, whose every request is answered.
    #[test]
    fn a_listing_that_never_ends_is_given_up() {
        let sftp = scripted(move |kind, id, _| {
            let list = Body::default().u32(1).text("again").text("").attrs(&Attrs::default());
            vec![match kind {
                OPENDIR => answer(HANDLE, id, Body::default().bytes(b"h")),
                READDIR => answer(NAME, id, list),
                _ => answer(STATUS, id, Body::default().u32(STATUS_OK).text("").text("")),
            }]
        });
        let finished = thread::spawn(move || {
            be_impatient(Duration::from_millis(50));
            sftp.read_dir("/").map(|names| names.len())
        });
        let error = finished.join().unwrap().expect_err("it never ends");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    }

    /// Pipes that do what a test says: `script` answers each request by hand, so a server's
    /// odd ways can be tried. A real one doesn't reorder, or give less than was asked for.
    fn scripted(mut script: impl FnMut(u8, u32, &[u8]) -> Vec<Vec<u8>> + Send + 'static) -> Arc<Sftp> {
        let (mut from_client, to_server) = io::pipe().unwrap();
        let (from_server, mut to_client) = io::pipe().unwrap();
        thread::spawn(move || {
            let Ok((INIT, _)) = read_packet(&mut from_client) else { return };
            let mut hello = vec![0, 0, 0, 5, VERSION];
            hello.extend_from_slice(&3u32.to_be_bytes());
            let _ = to_client.write_all(&hello);
            while let Ok((kind, body)) = read_packet(&mut from_client) {
                let id = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                for answer in script(kind, id, &body[4..]) {
                    let _ = to_client.write_all(&answer);
                }
            }
        });
        Arc::new(Sftp::start(from_server, to_server).unwrap())
    }

    fn answer(kind: u8, id: u32, body: Body) -> Vec<u8> {
        let mut packet = ((body.0.len() + 5) as u32).to_be_bytes().to_vec();
        packet.push(kind);
        packet.extend_from_slice(&id.to_be_bytes());
        packet.extend_from_slice(&body.0);
        packet
    }

    /// The answer to a READ of `file`: its end, or the piece asked for, cut to `most` bytes.
    fn read_answer(file: &[u8], id: u32, body: &[u8], most: usize) -> Vec<u8> {
        let mut fields = Fields(body);
        fields.bytes().unwrap();
        let (offset, length) = (fields.u64().unwrap() as usize, fields.u32().unwrap() as usize);
        if offset >= file.len() {
            return answer(STATUS, id, Body::default().u32(STATUS_EOF).text("").text(""));
        }
        let end = (offset + length.min(most)).min(file.len());
        answer(DATA, id, Body::default().bytes(&file[offset..end]))
    }

    fn file_of(pieces: usize) -> Arc<Vec<u8>> {
        Arc::new((0..(PIECE * pieces) as u32).map(|i| (i % 251) as u8).collect())
    }

    /// Answers may overtake each other: the pieces still come in the order of the file.
    #[test]
    fn answers_out_of_order_give_the_file_in_order() {
        let content = file_of(4);
        let file = content.clone();
        let mut held = Vec::new();
        let sftp = scripted(move |_, id, body| {
            held.push(read_answer(&file, id, body, PIECE));
            // Four requests go out at once: each pair is answered the other way round.
            if held.len() == 2 { held.drain(..).rev().collect() } else { Vec::new() }
        });
        let mut download = Download::new(sftp, Handle(vec![1]), content.len() as u64);
        let mut read = Vec::new();
        for _ in 0..4 {
            read.extend_from_slice(&download.next_piece().unwrap().unwrap());
        }
        assert!(read == *content);
    }

    /// Less than was asked for isn't the end of the file: the rest is asked for, and comes
    /// before the pieces behind it.
    #[test]
    fn short_answers_give_the_whole_file() {
        let content = file_of(5);
        let file = content.clone();
        let sftp = scripted(move |_, id, body| vec![read_answer(&file, id, body, 1000)]);
        let mut download = Download::new(sftp, Handle(vec![1]), content.len() as u64);
        let mut read = Vec::new();
        while let Some(piece) = download.next_piece().unwrap() {
            read.extend_from_slice(&piece);
        }
        assert!(read == *content, "{} of {} bytes", read.len(), content.len());
    }

    /// A refused write is seen, and an unknown answer isn't taken for data.
    #[test]
    fn errors_in_the_middle_of_a_transfer() {
        let sftp = scripted(move |kind, id, _| match kind {
            WRITE if id > 3 => vec![answer(STATUS, id, Body::default().u32(STATUS_FAILURE).text("disk full").text(""))],
            WRITE => vec![answer(STATUS, id, Body::default().u32(STATUS_OK).text("").text(""))],
            _ => vec![answer(NAME, id, Body::default().u32(0))],
        });
        let mut upload = Upload::new(sftp.clone(), Handle(vec![1]));
        let data = vec![0; PIECE];
        let failed = (0..50).find_map(|_| upload.write(&data).err()).or_else(|| upload.finish().err());
        assert_eq!(failed.expect("the refusal arrives").to_string(), "disk full");
        let mut download = Download::new(sftp, Handle(vec![1]), 10);
        assert!(download.next_piece().is_err(), "a NAME answer to a read");
    }

    /// Text printed by a shell starting up isn't taken for a length.
    #[test]
    fn text_on_the_pipe_is_not_a_server() {
        let (from_server, mut to_client) = io::pipe().unwrap();
        let (_from_client, to_server) = io::pipe().unwrap();
        to_client.write_all(b"Welcome to my server!\n").unwrap();
        let error = Sftp::start(from_server, to_server).err().expect("no server");
        assert!(error.to_string().contains("doesn't speak SFTP"), "{error}");
    }

    /// The fields of a server's answer, read back from what `Body` writes.
    #[test]
    fn attributes_go_there_and_back() {
        let attrs = Attrs { size: Some(1 << 40), owner: Some((1000, 100)), mode: Some(0o100644), times: Some((5, 6)) };
        let body = Body::default().attrs(&attrs);
        assert_eq!(Fields(&body.0).attrs().unwrap(), attrs);
        assert!(attrs.is_file() && !attrs.is_dir() && !attrs.is_executable());
        assert_eq!(Fields(&Body::default().attrs(&Attrs::default()).0).attrs().unwrap(), Attrs::default());
        // Cut short: an error, not a panic.
        assert!(Fields(&body.0[..6]).attrs().is_err());
    }
}
