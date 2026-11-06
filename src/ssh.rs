//! Connections to other computers, through the `ssh` program. It does the login, with the
//! user's keys, agent, `~/.ssh/config` and known hosts, so rmc never sees a password or a key;
//! over its pipes rmc speaks SFTP (see `sftp`).

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use crate::sftp::Sftp;
use crate::util;

/// How many of `ssh`'s last lines of text are kept, to say what went wrong.
const STDERR_LINES: usize = 20;

/// Who to connect to, as `ssh` would be told. An alias from `~/.ssh/config` is a host too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl fmt::Display for Target {
    /// `nas`, or `me@nas`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.user {
            Some(user) => write!(f, "{user}@{}", self.host),
            None => f.write_str(&self.host),
        }
    }
}

impl Target {
    /// The whole address of `path` there: `nas:/home/me`, and with a port the long form.
    pub fn place(&self, path: &str) -> String {
        match self.port {
            Some(port) => format!("ssh://{self}:{port}{path}"),
            None => format!("{self}:{path}"),
        }
    }

    /// `[user@]host`, or None if that isn't one. A name can't start with `-`: `ssh` would take
    /// it for an option, and some options run commands.
    fn from_text(text: &str) -> Option<Target> {
        let (user, host) = match text.rsplit_once('@') {
            Some((user, host)) => (Some(user), host),
            None => (None, text),
        };
        let name = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
        if user.is_some_and(|user| user.is_empty() || user.starts_with('-') || !user.chars().all(name)) {
            return None;
        }
        let address = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
        let fine = match address {
            // An IPv6 address.
            Some(address) => !address.is_empty() && address.chars().all(|c| c.is_ascii_hexdigit() || ":.%".contains(c)),
            None => !host.is_empty() && !host.starts_with('-') && host.chars().all(name),
        };
        fine.then(|| Target { user: user.map(String::from), host: host.to_string(), port: None })
    }
}

/// The address forms with a scheme (mc's `sh://` too).
fn strip_scheme(text: &str) -> Option<&str> {
    ["ssh://", "sftp://", "sh://"]
        .iter()
        .find_map(|scheme| text.get(..scheme.len()).filter(|start| start.eq_ignore_ascii_case(scheme)).map(|_| &text[scheme.len()..]))
}

/// An address that means another computer: `ssh://[user@]host[:port][/path]`, or scp's
/// `[user@]host:[path]`. Returns who, and the path as written ("" is home). As with scp, one
/// letter before the colon is a drive, and a slash before it makes it a path. Anything else is
/// None, which is a place on this computer.
pub fn parse(text: &str) -> Option<(Target, String)> {
    parse_exact(util::strip_quotes(text.trim()))
}

/// `parse` for an address rmc wrote itself (`Target::place`), to the letter: a name there may
/// end in a space, and read as typed it would be another file.
pub fn parse_exact(text: &str) -> Option<(Target, String)> {
    if let Some(rest) = strip_scheme(text) {
        let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
        let (who, port) = match authority.rsplit_once(':') {
            Some((who, port)) if !port.contains(']') => (who, Some(port.parse().ok()?)),
            _ => (authority, None),
        };
        let target = Target { port, ..Target::from_text(who)? };
        return Some((target, path.to_string()));
    }
    // After a closing bracket, or the first one: an address in brackets holds colons itself.
    let colon = text.find(']').map_or(Some(0), |at| Some(at + 1)).and_then(|from| text[from..].find(':').map(|at| from + at))?;
    let (who, path) = (&text[..colon], &text[colon + 1..]);
    let host_length = who.rsplit('@').next().map_or(0, str::len);
    if host_length < 2 {
        return None;
    }
    Some((Target::from_text(who)?, path.to_string()))
}

/// `parse` for what is typed into a field, where the mistakes are said: rmc takes no password
/// in an address (and keeps no field that holds one).
pub fn parse_typed(text: &str) -> Result<Option<(Target, String)>, String> {
    if has_password(text) {
        return Err("rmc takes no password in an address: ssh asks for it itself.".into());
    }
    match parse(text) {
        None if strip_scheme(util::strip_quotes(text.trim())).is_some() => Err(format!("\"{}\" is not an address rmc can read.", text.trim())),
        parsed => Ok(parsed),
    }
}

/// Whether an address carries a password: `ssh://me:secret@host`.
pub fn has_password(text: &str) -> bool {
    strip_scheme(util::strip_quotes(text.trim())).is_some_and(|rest| {
        let authority = rest.split('/').next().unwrap_or_default();
        authority.rsplit_once('@').is_some_and(|(who, _)| who.contains(':'))
    })
}

/// What the connect field takes besides `parse`: a bare `host` or `user@host`.
pub fn parse_connect(text: &str) -> Result<(Target, String), String> {
    match parse_typed(text)? {
        Some(parsed) => Ok(parsed),
        None => Target::from_text(text.trim()).map(|target| (target, String::new())).ok_or_else(|| {
            format!("\"{}\" is not an address rmc can read.\nThe forms are host, user@host, host:/path, ssh://user@host:port/path", text.trim())
        }),
    }
}

/// `name` in the folder `dir`.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// The folder `path` is in; None at the top.
pub fn parent(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    match path.rfind('/')? {
        0 => Some("/".into()),
        at => Some(path[..at].into()),
    }
}

/// The last name of `path`.
pub fn name_of(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// `path` without `.`, `..` and doubled slashes, going by the text as `util::normalize` does.
fn tidy(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The path of an address, whole: from the top if it starts with a slash, otherwise from home
/// (`~` is home, and so is nothing at all), as scp reads them. To the letter.
pub fn absolute(home: &str, path: &str) -> String {
    match path {
        "" | "~" => tidy(home),
        _ if path.starts_with('/') => tidy(path),
        _ => tidy(&join(home, path.strip_prefix("~/").unwrap_or(path))),
    }
}

/// What `input`, as typed, means from folder `base` over there.
pub fn resolve(base: &str, home: &str, input: &str) -> String {
    let input = util::strip_quotes(input.trim());
    let from_home = input.is_empty() || input == "~" || input.starts_with("~/");
    absolute(if from_home { home } else { base }, input)
}

/// How a connection is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Login {
    /// Behind the panels: it works without asking anything, or fails.
    Quiet,
    /// With the terminal handed over, for `ssh` to ask its questions there.
    AtTheTerminal,
}

/// Why a connection wasn't made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// `ssh` has something to ask (a password, a passphrase, an unknown host's key).
    NeedsTheUser,
    /// What went wrong, for the user to read.
    Said(String),
}

pub struct Connection {
    pub target: Target,
    pub sftp: Arc<Sftp>,
    /// The folder it starts in.
    pub home: String,
    /// `ssh`, or for a test the server itself; a scripted server in a test has no program.
    child: Mutex<Option<Child>>,
}

impl Drop for Connection {
    /// `ssh` would end by itself with its pipes closing, unless the network is what hangs.
    fn drop(&mut self) {
        if let Some(child) = self.child.get_mut().unwrap_or_else(|e| e.into_inner()) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The `ssh` program: the first on the PATH, or on Windows where it is usually installed.
fn ssh_program() -> PathBuf {
    if !cfg!(windows) {
        return PathBuf::from("ssh");
    }
    let installed = || {
        let system = std::env::var_os("SystemRoot").map(|root| Path::new(&root).join(r"System32\OpenSSH\ssh.exe"));
        let git = std::env::var_os("ProgramFiles").map(|dir| Path::new(&dir).join(r"Git\usr\bin\ssh.exe"));
        [system, git].into_iter().flatten().find(|path| path.is_file())
    };
    util::find_in_path("ssh.exe").or_else(installed).unwrap_or_else(|| PathBuf::from("ssh.exe"))
}

/// The arguments `sftp` itself gives `ssh`: a file connection, whatever `~/.ssh/config` wants
/// for shells. A dead network ends it within a minute (the connection is otherwise silent).
fn ssh_arguments(target: &Target, login: Login) -> Vec<String> {
    let mut args: Vec<String> = [
        "ForwardX11=no",
        "ForwardAgent=no",
        "PermitLocalCommand=no",
        "ClearAllForwardings=yes",
        "RemoteCommand=none",
        "RequestTTY=no",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=4",
        "ConnectTimeout=15",
    ]
    .iter()
    .flat_map(|option| ["-o".to_string(), option.to_string()])
    .collect();
    if login == Login::Quiet {
        args.extend(["-o".to_string(), "BatchMode=yes".to_string()]);
    }
    if let Some(port) = target.port {
        args.extend(["-p".to_string(), port.to_string()]);
    }
    if let Some(user) = &target.user {
        args.extend(["-l".to_string(), user.clone()]);
    }
    let host = target.host.trim_start_matches('[').trim_end_matches(']').to_string();
    args.extend(["-s".to_string(), "--".to_string(), host, "sftp".to_string()]);
    args
}

/// How `sftp.exe` tells Windows OpenSSH's `ssh.exe` what its stdin, stdout and stderr are: base64
/// of a count of other handles (0) and their kinds, here 2, a pipe to read asynchronously, and
/// twice 3, the usual. See `fast_pipe`.
#[cfg(windows)]
const FD_STATE: (&str, &str) = ("c28fc6f98a2c44abbbd89d6a3037d0d9_POSIX_FD_STATE", "AAAAAAIDAwA=");

/// Whether this is Windows OpenSSH, not the `ssh` of Git or Cygwin.
#[cfg(windows)]
fn takes_fast_pipe(program: &Path) -> bool {
    program.parent().and_then(Path::file_name).is_some_and(|folder| folder.eq_ignore_ascii_case("OpenSSH"))
}

/// A pipe for stdin that `ssh.exe` reads asynchronously. From a plain pipe it takes a piece at
/// a time with a thread in between, which holds an upload to 25 MB/s whatever the network can
/// do; this one does 90. Ours is the writing end. It is `sftp.exe`'s way, undocumented: if a
/// later `ssh.exe` doesn't take it, `Connection::open` falls back on a plain pipe.
#[cfg(windows)]
fn fast_pipe() -> Option<(std::fs::File, std::os::windows::io::OwnedHandle)> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_OUTBOUND,
    };
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT};
    const BUFFER: u32 = 1 << 20;
    let name = format!(r"\\.\pipe\rmc-{}-{:016x}{:016x}", std::process::id(), util::random(), util::random());
    let name: Vec<u16> = std::ffi::OsStr::new(&name).encode_wide().chain([0]).collect();
    let ends = PIPE_ACCESS_OUTBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE;
    let kind = PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    // SAFETY: the name ends in a zero and outlives the calls; each handle gets one owner, here
    // and now.
    unsafe {
        let ours = CreateNamedPipeW(name.as_ptr(), ends, kind, 1, BUFFER, BUFFER, 0, std::ptr::null());
        if ours == INVALID_HANDLE_VALUE {
            return None;
        }
        let ours = std::fs::File::from_raw_handle(ours);
        let theirs = CreateFileW(name.as_ptr(), GENERIC_READ, 0, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, std::ptr::null_mut());
        if theirs == INVALID_HANDLE_VALUE {
            return None;
        }
        Some((ours, OwnedHandle::from_raw_handle(theirs)))
    }
}

/// Starts `ssh`, and gives the writing end of its stdin.
fn start_ssh(target: &Target, login: Login, fast: bool) -> io::Result<(Child, Box<dyn Write + Send>)> {
    let mut command = ssh_command(target, login);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    if fast && let Some((ours, theirs)) = fast_pipe() {
        let child = command.stdin(theirs).env(FD_STATE.0, FD_STATE.1).spawn()?;
        return Ok((child, Box::new(ours)));
    }
    #[cfg(not(windows))]
    let _ = fast;
    let mut child = command.stdin(Stdio::piped()).spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| io::Error::other("ssh started without its stdin"))?;
    Ok((child, Box::new(stdin)))
}

fn ssh_command(target: &Target, login: Login) -> Command {
    let mut command = Command::new(ssh_program());
    command.args(ssh_arguments(target, login));
    // Behind the panels it stays off the console rmc draws on and reads keys from.
    #[cfg(windows)]
    if login == Login::Quiet {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// How an attempt to connect ended, for `Connection::open`.
enum Refused {
    /// In `ssh`'s words.
    Said(String),
    /// It ended without any: only this is left to tell.
    Silent(String),
}

/// Whether `ssh`'s last words mean it wanted to ask something, which it may at the terminal.
/// A changed host key isn't one of those: `ssh` refuses that at the terminal too.
fn needs_the_user(said: &str) -> bool {
    let wanted = ["Permission denied", "Host key verification failed", "passphrase", "No more authentication methods", "Too many authentication failures"];
    wanted.iter().any(|word| said.contains(word)) && !said.contains("HOST IDENTIFICATION HAS CHANGED")
}

impl Connection {
    /// Connects to `target`: this waits for the login, however long that takes. `started`
    /// runs once `ssh` does.
    pub fn open(target: &Target, login: Login, started: impl FnOnce()) -> Result<Arc<Connection>, Failure> {
        #[cfg(windows)]
        let fast = takes_fast_pipe(&ssh_program());
        #[cfg(not(windows))]
        let fast = false;
        let mut started = Some(started);
        let mut attempt = |fast: bool| {
            let (child, stdin) = start_ssh(target, login, fast).map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => Refused::Said("The ssh program was not found. rmc connects through it.".into()),
                _ => Refused::Said(format!("Cannot run ssh:\n{e}")),
            })?;
            if let Some(started) = started.take() {
                started();
            }
            Connection::over(child, stdin, target.clone())
        };
        let mut result = attempt(fast);
        // It ended without a word: this `ssh` may not take the fast pipe.
        if fast && matches!(result, Err(Refused::Silent(_))) {
            result = attempt(false);
        }
        result.map_err(|refused| {
            let said = match refused {
                Refused::Said(said) | Refused::Silent(said) => said,
            };
            if login == Login::Quiet && needs_the_user(&said) { Failure::NeedsTheUser } else { Failure::Said(said) }
        })
    }

    /// Starts the protocol with `child` over its stdout and `stdin`.
    fn over(mut child: Child, stdin: Box<dyn Write + Send>, target: Target) -> Result<Arc<Connection>, Refused> {
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(Refused::Said("ssh started without its pipes".to_string()));
        };
        // It mustn't fill up, as that would stop `ssh`.
        let said = Arc::new(Mutex::new(VecDeque::new()));
        let their_said = said.clone();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut said = lock(&their_said);
                said.push_back(line);
                if said.len() > STDERR_LINES {
                    said.pop_front();
                }
            }
        });
        let started = Sftp::start(stdout, stdin).and_then(|sftp| {
            let home = sftp.realpath(".")?;
            Ok((sftp, home))
        });
        match started {
            Ok((sftp, home)) => Ok(Arc::new(Connection { target, sftp: Arc::new(sftp), home, child: Mutex::new(Some(child)) })),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                let said = lock(&said);
                // The banner, and notes that a host was added, aren't what went wrong.
                let lines: Vec<&str> = said.iter().map(String::as_str).filter(|line| !line.starts_with("Warning:") && !line.starts_with("**")).collect();
                let last = &lines[lines.len().saturating_sub(4)..];
                Err(if last.is_empty() { Refused::Silent(e.to_string()) } else { Refused::Said(last.join("\n")) })
            }
        }
    }
}

static POOL: Mutex<Vec<Arc<Connection>>> = Mutex::new(Vec::new());

/// The live connection to `target`, if there is one. They stay open until rmc ends: going back
/// to a host mustn't mean logging in again.
pub fn find(target: &Target) -> Option<Arc<Connection>> {
    let mut pool = lock(&POOL);
    pool.retain(|connection| connection.sftp.lost().is_none());
    pool.iter().find(|connection| connection.target == *target).cloned()
}

/// Keeps a new connection for `find`.
pub fn keep(connection: Arc<Connection>) {
    lock(&POOL).push(connection);
}

/// The hosts `~/.ssh/config` names (not its patterns), for the drive picker.
pub fn configured_hosts() -> Vec<String> {
    let Some(file) = util::home_dir().map(|home| home.join(".ssh").join("config")) else { return Vec::new() };
    std::fs::read_to_string(file).map_or_else(|_| Vec::new(), |text| hosts_in(&text))
}

fn hosts_in(config: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        let Some((keyword, rest)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else { continue };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for host in rest.split_whitespace().map(|host| host.trim_matches('"')) {
            let pattern = host.contains(['*', '?', '!']);
            if !pattern && !hosts.iter().any(|known| known == host) && Target::from_text(host).is_some() {
                hosts.push(host.to_string());
            }
        }
    }
    hosts
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::sftp::tests::server_command;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A connection to a real server program started in `dir`, in the pool under a host name of
    /// its own (no `ssh` in between). None where no server is installed.
    pub fn local_connection(dir: &Path) -> Option<Arc<Connection>> {
        static HOSTS: AtomicU32 = AtomicU32::new(0);
        let mut child = server_command(dir)?.stderr(Stdio::piped()).spawn().ok()?;
        let stdin: Box<dyn Write + Send> = Box::new(child.stdin.take()?);
        let host = format!("test-host-{}-{}", std::process::id(), HOSTS.fetch_add(1, Ordering::Relaxed));
        let target = Target { user: None, host, port: None };
        let connection = Connection::over(child, stdin, target).ok().expect("the server starts");
        keep(connection.clone());
        Some(connection)
    }

    /// A connection to a scripted server.
    pub fn scripted_connection(sftp: Arc<Sftp>) -> Arc<Connection> {
        let target = Target { user: None, host: "scripted".to_string(), port: None };
        Arc::new(Connection { target, sftp, home: "/".to_string(), child: Mutex::new(None) })
    }

    /// The server goes away.
    pub fn kill(connection: &Connection) {
        if let Some(child) = lock(&connection.child).as_mut() {
            let _ = child.kill();
        }
    }

    fn target(user: Option<&str>, host: &str, port: Option<u16>) -> Target {
        Target { user: user.map(String::from), host: host.to_string(), port }
    }

    #[test]
    fn addresses() {
        let parsed = |text: &str| parse(text).map(|(target, path)| (target, path));
        assert_eq!(parsed("nas:/home/me"), Some((target(None, "nas", None), "/home/me".into())));
        assert_eq!(parsed("me@nas:"), Some((target(Some("me"), "nas", None), "".into())));
        assert_eq!(parsed("nas:sub/folder"), Some((target(None, "nas", None), "sub/folder".into())));
        assert_eq!(parsed("me@192.0.2.10:/tmp/x y"), Some((target(Some("me"), "192.0.2.10", None), "/tmp/x y".into())));
        assert_eq!(parsed("ssh://me@nas:2222/srv"), Some((target(Some("me"), "nas", Some(2222)), "/srv".into())));
        assert_eq!(parsed("SFTP://nas"), Some((target(None, "nas", None), "".into())));
        assert_eq!(parsed("sh://nas/etc"), Some((target(None, "nas", None), "/etc".into())));
        assert_eq!(parsed("\"nas:/a b\""), Some((target(None, "nas", None), "/a b".into())));
        assert_eq!(parsed("[fe80::1]:/x"), Some((target(None, "[fe80::1]", None), "/x".into())));
        assert_eq!(parsed("ssh://[fe80::1]:2222/x"), Some((target(None, "[fe80::1]", Some(2222)), "/x".into())));
        assert_eq!(parsed("ssh://[fe80::1]/x"), Some((target(None, "[fe80::1]", None), "/x".into())));

        // Places on this computer.
        for local in [r"C:\Users\me", "c:", "e:foo", "/home/me", "./a:b", "sub/a:b", r"\\server\share", "plain", "a b:c", "ssh://", "ssh://me:secret@nas/x"] {
            assert_eq!(parsed(local), None, "{local}");
        }
        // An option in disguise goes nowhere near `ssh`.
        for bad in ["-oProxyCommand=evil:/x", "ssh://-oProxyCommand=evil/x", "-me@host:/x", "me@-host:/x", "ssh://me@-host"] {
            assert_eq!(parsed(bad), None, "{bad}");
        }

        assert_eq!(target(Some("me"), "nas", None).place("/srv"), "me@nas:/srv");
        assert_eq!(target(None, "nas", Some(2222)).place("/srv"), "ssh://nas:2222/srv");
        // What a place says reads back as the same place.
        for place in ["me@nas:/srv", "ssh://nas:2222/srv", "[fe80::1]:/x"] {
            let (target, path) = parse(place).unwrap();
            assert_eq!(target.place(&path), place);
        }
    }

    /// The connect field takes a bare host, and says what it can't read. A password isn't read
    /// at all, nor kept.
    #[test]
    fn typed_addresses() {
        assert_eq!(parse_connect("nas"), Ok((target(None, "nas", None), String::new())));
        assert_eq!(parse_connect(" me@nas "), Ok((target(Some("me"), "nas", None), String::new())));
        assert_eq!(parse_connect("nas:/srv").map(|(_, path)| path), Ok("/srv".to_string()));
        assert!(parse_connect("not a host").is_err());
        assert!(parse_connect("-oProxyCommand=evil").is_err());
        assert!(parse_connect("ssh://me:secret@nas/x").unwrap_err().contains("no password"));
        assert!(parse_typed("sftp://me:secret@nas").is_err());
        assert!(has_password("ssh://me:secret@nas/x") && !has_password("ssh://me@nas:22/x") && !has_password("a:b@c"));
        assert_eq!(parse_typed(r"C:\Users"), Ok(None));
        assert!(parse_typed("ssh://").is_err());
    }

    #[test]
    fn paths_over_there() {
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(parent("/a/b"), Some("/a".into()));
        assert_eq!(parent("/a"), Some("/".into()));
        assert_eq!(parent("/"), None);
        assert_eq!(name_of("/a/b.txt"), "b.txt");
        assert_eq!(name_of("/a/b/"), "b");
        // Names keep their spaces, in an address rmc made.
        assert_eq!(parse_exact("nas:/tmp/name "), Some((target(None, "nas", None), "/tmp/name ".into())));
        assert_eq!(parse("nas:/tmp/name ").map(|(_, path)| path), Some("/tmp/name".into()));
        assert_eq!(absolute("/home/me", "/tmp/ name "), "/tmp/ name ");
        assert_eq!(absolute("/home/me", "sub"), "/home/me/sub");
        assert_eq!(absolute("/home/me", "~/sub"), "/home/me/sub");
        let resolved = |input| resolve("/srv/www", "/home/me", input);
        assert_eq!(resolved("~name"), "/srv/www/~name");
        assert_eq!(resolved("logs"), "/srv/www/logs");
        assert_eq!(resolved(".."), "/srv");
        assert_eq!(resolved("../../.."), "/");
        assert_eq!(resolved("/etc//ssh/"), "/etc/ssh");
        assert_eq!(resolved("~"), "/home/me");
        assert_eq!(resolved("~/bin"), "/home/me/bin");
        assert_eq!(resolved(""), "/home/me");
        assert_eq!(resolved("\"a b\""), "/srv/www/a b");
    }

    /// `ssh` is told who to reach with nothing it could read as an option, and what is safe
    /// for a file connection, whatever `~/.ssh/config` says for shells.
    #[test]
    fn what_ssh_is_told() {
        let args = ssh_arguments(&target(Some("me"), "[fe80::1]", Some(2222)), Login::Quiet);
        let at = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(args[at - 1], "-s");
        assert_eq!(&args[at + 1..], ["fe80::1", "sftp"]);
        assert!(args.windows(2).any(|pair| pair == ["-p", "2222"]) && args.windows(2).any(|pair| pair == ["-l", "me"]));
        assert!(args.contains(&"BatchMode=yes".to_string()) && args.contains(&"ForwardAgent=no".to_string()));
        assert!(!ssh_arguments(&target(None, "nas", None), Login::AtTheTerminal).contains(&"BatchMode=yes".to_string()));
    }

    #[test]
    fn what_ssh_says_tells_what_to_do_next() {
        assert!(needs_the_user("me@nas: Permission denied (publickey,password)."));
        assert!(needs_the_user("Host key verification failed."));
        assert!(!needs_the_user("ssh: Could not resolve hostname nowhere: Name or service not known"));
        assert!(!needs_the_user("connect to host nas port 22: Connection refused"));
        assert!(!needs_the_user("WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\nHost key verification failed."));
    }

    #[test]
    fn hosts_from_the_config_file() {
        let config = "# mine\nHost nas laptop\n  HostName 192.0.2.10\nhost=web\nHost *\n  User me\nHost *.example.com !x\nMatch host y\nHost -bad\n";
        assert_eq!(hosts_in(config), ["nas", "laptop", "web"]);
    }

    /// Windows OpenSSH gets the fast pipe, others the plain one, and the pipe works.
    #[cfg(windows)]
    #[test]
    fn the_fast_pipe_is_for_windows_openssh() {
        assert!(takes_fast_pipe(Path::new(r"C:\Windows\System32\OpenSSH\ssh.exe")));
        assert!(takes_fast_pipe(Path::new(r"C:\Program Files\OpenSSH\ssh.exe")));
        assert!(!takes_fast_pipe(Path::new(r"C:\Program Files\Git\usr\bin\ssh.exe")));
        assert!(!takes_fast_pipe(Path::new("ssh.exe")));
        let (mut ours, _theirs) = fast_pipe().expect("a pipe");
        ours.write_all(b"goes in").unwrap();
    }

    /// The pool gives a live connection and forgets a dead one.
    #[test]
    fn the_pool_keeps_live_connections() {
        let tmp = crate::testutil::TempDir::new("ssh-pool");
        let Some(connection) = local_connection(tmp.path()) else { return };
        assert!(find(&connection.target).is_some());
        assert!(find(&target(None, "nobody-here", None)).is_none());
        kill(&connection);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while find(&connection.target).is_some() {
            assert!(std::time::Instant::now() < deadline, "the pool kept a dead connection");
            thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
