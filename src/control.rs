//! Control socket: the display-independent way to send the daemon commands.
//!
//! The xscreensaver control protocol needs an X server. This is the same conversation over
//! a Unix socket, `$XDG_RUNTIME_DIR/nightsaver-<display>.sock`, so `nightsaver-command`
//! also works where there is no X server to carry it. It reuses the command names, the
//! `+message` / `-message` replies and the status items of `compat::protocol`.
//!
//! Wire format, one request per connection, lines end in `\n`:
//!
//! ```text
//! ACTIVATE | DEACTIVATE | CYCLE | NEXT | PREV | SELECT n | LOCK | SUSPEND | EXIT | RESTART
//!     | DEMO n | PREFS          -> "+message" or "-message"
//! VERSION                       -> "+XScreenSaver 6.15"
//! STATUS                        -> "+BLANK 1794000000 2 2"   (state, since, hack per screen)
//! WATCH                         -> one status line (no "+") per change, until closed
//! ```
//!
//! Security: the socket is only created in a runtime directory that is ours and closed to
//! everyone else, it is mode 0600, and every connection's peer uid is checked. Commands go
//! through the same `Daemon::command` as X ones, so a locked or lock-pending daemon
//! refuses EXIT, RESTART and SUSPEND here exactly as it does there.

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::compat::protocol::{
    self, ATOM_AUTH, ATOM_BLANK, ATOM_LOCK, Command, ProtocolAtoms, Status,
};

/// Longest request line accepted, including the newline.
const MAX_REQUEST: usize = 64;
/// Connections (waiting to send a request, or watching) served at once.
const MAX_CLIENTS: usize = 16;
/// How long a client may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Name the client prints before the version, as `xscreensaver-command` prints the
/// daemon window's class.
pub const DAEMON_CLASS: &str = protocol::DAEMON_RES_CLASS;

/// What a client may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Command(Command),
    Version,
    Status,
    Watch,
}

/// Where the daemon for `display` listens. `display` is the X display name or the
/// Wayland socket name; an X screen suffix (`:0.0`) is dropped so `:0` and `:0.0` agree.
pub fn socket_path(runtime_dir: &Path, display: &str) -> PathBuf {
    let display = match display.rsplit_once(':') {
        Some((host, rest)) => match rest.split_once('.') {
            Some((number, _screen)) => format!("{host}:{number}"),
            None => display.to_owned(),
        },
        None => display.to_owned(),
    };
    let safe: String = display
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') { c } else { '_' })
        .collect();
    runtime_dir.join(format!("nightsaver-{safe}.sock"))
}

/// `$XDG_RUNTIME_DIR`, when set to an absolute path.
pub fn runtime_dir() -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    d.is_absolute().then_some(d)
}

/// True when a daemon for `display` answers on its control socket.
pub fn daemon_listening(display: &str) -> bool {
    runtime_dir().is_some_and(|d| UnixStream::connect(socket_path(&d, display)).is_ok())
}

/// Why a runtime directory must not hold our socket, or `None` when it is fine: it has to
/// be a directory owned by `uid` that nobody else can enter.
pub fn untrusted_runtime_dir(md: &std::fs::Metadata, uid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    if !md.is_dir() {
        Some("not a directory".into())
    } else if md.uid() != uid {
        Some(format!("owned by uid {}, not by us", md.uid()))
    } else if md.mode() & 0o077 != 0 {
        Some("accessible by group or others".into())
    } else {
        None
    }
}

/// The request line for `req`, without the newline.
pub fn encode_request(req: Request) -> String {
    match req {
        Request::Command(Command::Select(n)) => format!("SELECT {n}"),
        Request::Command(c) => c.atom_name().to_owned(),
        Request::Version => "VERSION".to_owned(),
        Request::Status => "STATUS".to_owned(),
        Request::Watch => "WATCH".to_owned(),
    }
}

/// Parse one request line (without the newline). `None` for anything malformed.
pub fn parse_request(line: &str) -> Option<Request> {
    let mut words = line.split(' ');
    let name = words.next()?;
    let arg = words.next();
    if words.next().is_some() {
        return None;
    }
    let number = || arg.and_then(|a| a.parse::<u32>().ok());
    let bare = |r: Request| arg.is_none().then_some(r);
    match name {
        "ACTIVATE" => bare(Request::Command(Command::Activate)),
        "DEACTIVATE" => bare(Request::Command(Command::Deactivate)),
        "CYCLE" => bare(Request::Command(Command::Cycle)),
        "NEXT" => bare(Request::Command(Command::Next)),
        "PREV" => bare(Request::Command(Command::Prev)),
        "SELECT" => number().map(|n| Request::Command(Command::Select(n))),
        "EXIT" => bare(Request::Command(Command::Exit)),
        "RESTART" => bare(Request::Command(Command::Restart)),
        "LOCK" => bare(Request::Command(Command::Lock)),
        "SUSPEND" => bare(Request::Command(Command::Suspend)),
        // The demo number is accepted and ignored: DEMO is always refused.
        "DEMO" => (arg.is_none() || number().is_some()).then_some(Request::Command(Command::Demo)),
        "PREFS" => bare(Request::Command(Command::Prefs)),
        "VERSION" => bare(Request::Version),
        "STATUS" => bare(Request::Status),
        "WATCH" => bare(Request::Watch),
        _ => None,
    }
}

/// A `+message` / `-message` reply line, without the newline. Line breaks in `msg` would
/// end the reply early, so they are replaced.
pub fn encode_reply(ok: bool, msg: &str) -> String {
    let msg: String = msg.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    String::from_utf8_lossy(&protocol::encode_response(ok, &msg)).into_owned()
}

/// Split a reply line into success and message. `None` without the `+`/`-` prefix.
pub fn parse_reply(line: &str) -> Option<(bool, &str)> {
    match line.as_bytes().first()? {
        b'+' => Some((true, &line[1..])),
        b'-' => Some((false, &line[1..])),
        _ => None,
    }
}

fn status_word(s: Status) -> &'static str {
    match s {
        Status::Off => "OFF",
        Status::Blank => ATOM_BLANK,
        Status::Lock => ATOM_LOCK,
        Status::Auth => ATOM_AUTH,
    }
}

/// The status as one line: state, seconds since the epoch, then one hack number per screen.
pub fn encode_status_line(status: Status, since: SystemTime, hacks: &[u32]) -> String {
    let secs = since.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut s = format!("{} {secs}", status_word(status));
    for h in hacks {
        s.push_str(&format!(" {h}"));
    }
    s
}

/// The atom table that goes with [`parse_status_line`]: the socket has no X atoms, so the
/// state words get fixed numbers and the `compat::protocol` formatters work unchanged.
pub fn status_atoms() -> ProtocolAtoms {
    ProtocolAtoms::from_pairs([(ATOM_BLANK, 1), (ATOM_LOCK, 2), (ATOM_AUTH, 3)])
}

/// Decode a status line into `_SCREENSAVER_STATUS` items over [`status_atoms`].
pub fn parse_status_line(line: &str) -> Option<Vec<u32>> {
    let mut words = line.split(' ');
    let status = match words.next()? {
        "OFF" => Status::Off,
        ATOM_BLANK => Status::Blank,
        ATOM_LOCK => Status::Lock,
        ATOM_AUTH => Status::Auth,
        _ => return None,
    };
    let secs: u64 = words.next()?.parse().ok()?;
    let hacks = words.map(|w| w.parse::<u32>().ok()).collect::<Option<Vec<u32>>>()?;
    let since = UNIX_EPOCH.checked_add(Duration::from_secs(secs))?;
    protocol::encode_status(&status_atoms(), status, since, &hacks).ok()
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// A client that has sent its request and is owed one reply.
#[derive(Debug)]
pub struct Client(UnixStream);

impl Client {
    /// Send the reply and close the connection. Best effort: a client that went away, or
    /// that does not read, loses its answer and nothing else.
    pub fn reply(mut self, ok: bool, msg: &str) {
        let line = format!("{}\n", encode_reply(ok, msg));
        if let Err(e) = self.0.write_all(line.as_bytes()) {
            log::debug!("control client did not take its reply: {e}");
        }
    }
}

struct Pending {
    stream: UnixStream,
    buf: Vec<u8>,
    deadline: Instant,
}

pub struct ControlServer {
    listener: UnixListener,
    path: PathBuf,
    uid: u32,
    pending: Vec<Pending>,
    watchers: Vec<UnixStream>,
    /// The latest status, for STATUS and for new watchers' change detection.
    status_line: String,
}

impl std::fmt::Debug for ControlServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlServer").field("path", &self.path).finish_non_exhaustive()
    }
}

impl ControlServer {
    /// Listen for the daemon of `display`. `Err` carries the reason there is no socket;
    /// the daemon then runs without one.
    pub fn bind(display: &str) -> Result<ControlServer, String> {
        let dir = runtime_dir().ok_or("XDG_RUNTIME_DIR is not set")?;
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        Self::bind_in(&dir, display, uid)
    }

    fn bind_in(dir: &Path, display: &str, uid: u32) -> Result<ControlServer, String> {
        use std::os::unix::fs::PermissionsExt;
        let md = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if let Some(why) = untrusted_runtime_dir(&md, uid) {
            return Err(format!("{}: {why}", dir.display()));
        }
        let path = socket_path(dir, display);
        if std::fs::symlink_metadata(&path).is_ok() {
            // Left behind by a daemon that was killed, unless something still answers.
            if UnixStream::connect(&path).is_ok() {
                return Err(format!("{}: another daemon is listening", path.display()));
            }
            std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let ready = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .and_then(|()| listener.set_nonblocking(true));
        if let Err(e) = ready {
            let _ = std::fs::remove_file(&path);
            return Err(format!("{}: {e}", path.display()));
        }
        Ok(ControlServer {
            listener,
            path,
            uid,
            pending: Vec::new(),
            watchers: Vec::new(),
            status_line: encode_status_line(Status::Off, SystemTime::now(), &[]),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Descriptors that become readable when `poll` has work to do.
    pub fn fds(&self) -> Vec<RawFd> {
        let mut v = vec![self.listener.as_raw_fd()];
        v.extend(self.pending.iter().map(|p| p.stream.as_raw_fd()));
        v.extend(self.watchers.iter().map(AsRawFd::as_raw_fd));
        v
    }

    /// Accept new connections and collect the requests that have arrived. Never blocks.
    /// Connections from another user, malformed or over-long requests and clients that
    /// take too long are dropped.
    pub fn poll(&mut self, now: Instant) -> Vec<(Request, Client)> {
        self.accept(now);
        self.drop_closed_watchers();
        let mut ready = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            match read_request(&mut self.pending[i], now) {
                Progress::Waiting => i += 1,
                Progress::Drop => drop(self.pending.swap_remove(i)),
                Progress::Request(r) => {
                    let p = self.pending.swap_remove(i);
                    ready.push((r, Client(p.stream)));
                }
            }
        }
        ready
    }

    fn accept(&mut self, now: Instant) {
        loop {
            let stream = match self.listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    log::warn!("control socket: accept failed: {e}");
                    return;
                }
            };
            match peer_uid(&stream) {
                Ok(peer) if peer == self.uid => {}
                Ok(peer) => {
                    log::warn!("control socket: connection from uid {peer} refused");
                    continue;
                }
                Err(e) => {
                    log::warn!("control socket: cannot identify the peer ({e}): refused");
                    continue;
                }
            }
            if self.pending.len() + self.watchers.len() >= MAX_CLIENTS {
                log::warn!("control socket: too many connections; dropping a new one");
                continue;
            }
            if let Err(e) = stream.set_nonblocking(true) {
                log::warn!("control socket: {e}");
                continue;
            }
            self.pending.push(Pending {
                stream,
                buf: Vec::new(),
                deadline: now.checked_add(REQUEST_TIMEOUT).unwrap_or(now),
            });
        }
    }

    /// The current status line (the STATUS reply).
    pub fn status_line(&self) -> &str {
        &self.status_line
    }

    /// Record the daemon's status and tell the watchers when it changed.
    pub fn set_status(&mut self, status: Status, since: SystemTime, hacks: &[u32]) {
        let line = encode_status_line(status, since, hacks);
        if line == self.status_line {
            return;
        }
        let out = format!("{line}\n");
        // A watcher that is gone or not reading (its buffer is full) is dropped.
        self.watchers.retain_mut(|w| w.write_all(out.as_bytes()).is_ok());
        self.status_line = line;
    }

    /// Keep `client` connected and send it every status change from now on.
    pub fn watch(&mut self, client: Client) {
        self.watchers.push(client.0);
    }

    /// Watchers only listen: anything readable on one is a hang-up (or noise to discard).
    fn drop_closed_watchers(&mut self) {
        self.watchers.retain_mut(|w| {
            let mut scratch = [0u8; 64];
            loop {
                match w.read(&mut scratch) {
                    Ok(0) => return false,
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => return false,
                }
            }
        });
    }
}

/// The effective uid of the process at the other end of `stream`.
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
    // SAFETY: the descriptor is open for the lifetime of `stream`, and both pointers refer
    // to live, properly aligned locals that getpeereid only writes to.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc == 0 { Ok(uid) } else { Err(std::io::Error::last_os_error()) }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

enum Progress {
    Waiting,
    Drop,
    Request(Request),
}

/// Read what `p` has sent so far; a request is complete at the first newline.
fn read_request(p: &mut Pending, now: Instant) -> Progress {
    let mut chunk = [0u8; MAX_REQUEST];
    loop {
        if let Some(end) = p.buf.iter().position(|&b| b == b'\n') {
            let parsed = std::str::from_utf8(&p.buf[..end]).ok().and_then(parse_request);
            return match parsed {
                Some(r) => Progress::Request(r),
                None => {
                    log::debug!("control socket: malformed request dropped");
                    Progress::Drop
                }
            };
        }
        if p.buf.len() >= MAX_REQUEST {
            log::debug!("control socket: over-long request dropped");
            return Progress::Drop;
        }
        let room = MAX_REQUEST - p.buf.len();
        match p.stream.read(&mut chunk[..room]) {
            Ok(0) => return Progress::Drop,
            Ok(n) => p.buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                return if now >= p.deadline { Progress::Drop } else { Progress::Waiting };
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return Progress::Drop,
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Send `req` to the daemon listening at `path` and return the connection to read the
/// answer from. `Err` with `NotFound`/`ConnectionRefused` means no daemon is listening.
pub fn send(path: &Path, req: Request) -> std::io::Result<std::io::BufReader<UnixStream>> {
    let mut s = UnixStream::connect(path)?;
    s.write_all(format!("{}\n", encode_request(req)).as_bytes())?;
    Ok(std::io::BufReader::new(s))
}

/// Read one line (without its newline). `Ok(None)` when the daemon closed the connection.
pub fn read_line(r: &mut std::io::BufReader<UnixStream>) -> std::io::Result<Option<String>> {
    use std::io::BufRead;
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    if line.ends_with('\n') {
        line.pop();
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::protocol::{TimeLine, format_time_line};
    use std::os::unix::fs::PermissionsExt;

    const ALL: [Request; 15] = [
        Request::Command(Command::Activate),
        Request::Command(Command::Deactivate),
        Request::Command(Command::Cycle),
        Request::Command(Command::Next),
        Request::Command(Command::Prev),
        Request::Command(Command::Select(7)),
        Request::Command(Command::Exit),
        Request::Command(Command::Restart),
        Request::Command(Command::Lock),
        Request::Command(Command::Suspend),
        Request::Command(Command::Demo),
        Request::Command(Command::Prefs),
        Request::Version,
        Request::Status,
        Request::Watch,
    ];

    fn private_dir(tag: &str) -> PathBuf {
        // Short on purpose: a Unix socket path is limited to about 100 bytes.
        let d = std::env::temp_dir().join(format!("ns-ctl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    fn uid() -> u32 {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }

    /// Poll until the server has seen a request (the client writes from another fd).
    fn next_request(srv: &mut ControlServer) -> (Request, Client) {
        for _ in 0..200 {
            if let Some(r) = srv.poll(Instant::now()).pop() {
                return r;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no request arrived");
    }

    #[test]
    fn requests_round_trip() {
        for r in ALL {
            assert_eq!(parse_request(&encode_request(r)), Some(r), "{r:?}");
        }
    }

    #[test]
    fn malformed_requests_are_rejected() {
        for bad in [
            "", " ", "activate", "ACTIVATE 1", "SELECT", "SELECT x", "SELECT -1", "SELECT 1 2",
            "LOCK ", " LOCK", "UNLOCK", "EXIT\r", "SELECT 99999999999",
        ] {
            assert_eq!(parse_request(bad), None, "{bad:?}");
        }
        assert_eq!(parse_request("DEMO 3"), Some(Request::Command(Command::Demo)));
    }

    #[test]
    fn replies_round_trip_and_stay_on_one_line() {
        assert_eq!(parse_reply(&encode_reply(true, "locking")), Some((true, "locking")));
        assert_eq!(parse_reply(&encode_reply(false, "screen is locked")), Some((false, "screen is locked")));
        assert_eq!(encode_reply(false, "a\nb"), "-a b");
        assert_eq!(parse_reply("locking"), None);
        assert_eq!(parse_reply(""), None);
    }

    #[test]
    fn status_line_feeds_the_time_formatter() {
        let since = UNIX_EPOCH + Duration::from_secs(1_794_000_000);
        let line = encode_status_line(Status::Lock, since, &[2, 2]);
        assert_eq!(line, "LOCK 1794000000 2 2");
        let items = parse_status_line(&line).unwrap();
        let text = format_time_line(&items, &status_atoms(), |t| format!("T{t}"));
        assert_eq!(text, TimeLine::Ok(": screen locked since T1794000000 (hacks: #2, #2)\n".into()));

        let off = parse_status_line(&encode_status_line(Status::Off, since, &[])).unwrap();
        let text = format_time_line(&off, &status_atoms(), |t| format!("T{t}"));
        assert_eq!(text, TimeLine::Ok(": screen non-blanked since T1794000000\n".into()));

        for bad in ["", "LOCK", "LOCKED 1", "LOCK x", "LOCK 1 x", "BLANK 1 -2"] {
            assert_eq!(parse_status_line(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn socket_path_is_one_file_per_display() {
        let d = Path::new("/run/u");
        assert_eq!(socket_path(d, ":0"), d.join("nightsaver-_0.sock"));
        assert_eq!(socket_path(d, ":0.0"), socket_path(d, ":0"), "screen suffix ignored");
        assert_eq!(socket_path(d, "host:10.1"), d.join("nightsaver-host_10.sock"));
        assert_eq!(socket_path(d, "wayland-1"), d.join("nightsaver-wayland-1.sock"));
        // A display name can never escape the runtime directory.
        assert_eq!(socket_path(d, "../../etc/x"), d.join("nightsaver-.._.._etc_x.sock"));
    }

    #[test]
    fn runtime_dir_must_be_ours_and_private() {
        let d = private_dir("dir");
        let md = |p: &Path| std::fs::symlink_metadata(p).unwrap();
        assert_eq!(untrusted_runtime_dir(&md(&d), uid()), None);
        assert!(untrusted_runtime_dir(&md(&d), uid() + 1).unwrap().contains("owned by"));
        for mode in [0o750, 0o705, 0o701, 0o777] {
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(untrusted_runtime_dir(&md(&d), uid()).is_some(), "{mode:o}");
            assert!(ControlServer::bind_in(&d, ":9", uid()).is_err(), "{mode:o}");
        }
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        let f = d.join("file");
        std::fs::write(&f, "").unwrap();
        assert_eq!(untrusted_runtime_dir(&md(&f), uid()).as_deref(), Some("not a directory"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn serves_a_command_and_removes_the_socket() {
        let d = private_dir("cmd");
        let mut srv = ControlServer::bind_in(&d, ":9", uid()).unwrap();
        let path = srv.path().to_owned();
        let mode = std::fs::symlink_metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(ControlServer::bind_in(&d, ":9", uid()).is_err(), "a live socket is not replaced");

        let mut conn = send(&path, Request::Command(Command::Select(3))).unwrap();
        let (req, client) = next_request(&mut srv);
        assert_eq!(req, Request::Command(Command::Select(3)));
        client.reply(true, "selecting");
        assert_eq!(read_line(&mut conn).unwrap().as_deref(), Some("+selecting"));
        assert_eq!(read_line(&mut conn).unwrap(), None, "closed after the reply");

        drop(srv);
        assert!(std::fs::symlink_metadata(&path).is_err(), "socket removed on drop");
        // A socket file left by a killed daemon is replaced.
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);
        assert!(ControlServer::bind_in(&d, ":9", uid()).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_peer_with_another_uid_is_refused() {
        let d = private_dir("uid");
        // Pretend to be someone else: every real connection then comes from a "foreign" uid.
        let mut srv = ControlServer::bind_in(&d, ":9", uid()).unwrap();
        srv.uid = uid() + 1;
        let mut conn = send(srv.path(), Request::Command(Command::Exit)).unwrap();
        for _ in 0..20 {
            assert!(srv.poll(Instant::now()).is_empty());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(read_line(&mut conn).unwrap(), None, "dropped without a reply");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_slow_and_excess_clients_are_dropped() {
        let d = private_dir("bad");
        let mut srv = ControlServer::bind_in(&d, ":9", uid()).unwrap();
        let path = srv.path().to_owned();

        let mut garbage = UnixStream::connect(&path).unwrap();
        garbage.write_all(b"UNLOCK\n").unwrap();
        let mut long = UnixStream::connect(&path).unwrap();
        long.write_all(&[b'A'; 200]).unwrap();
        let mut slow = UnixStream::connect(&path).unwrap();
        slow.write_all(b"LOC").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let now = Instant::now();
        assert!(srv.poll(now).is_empty());
        assert_eq!(srv.pending.len(), 1, "only the slow client is still waiting");
        assert!(srv.poll(now + REQUEST_TIMEOUT).is_empty());
        assert!(srv.pending.is_empty(), "the slow client timed out");
        let mut byte = [0u8; 1];
        for c in [&mut garbage, &mut long, &mut slow] {
            assert!(matches!(c.read(&mut byte), Ok(0) | Err(_)), "closed without a reply");
        }

        let crowd: Vec<UnixStream> =
            (0..MAX_CLIENTS + 4).map(|_| UnixStream::connect(&path).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(50));
        assert!(srv.poll(Instant::now()).is_empty());
        assert_eq!(srv.pending.len(), MAX_CLIENTS);
        drop(crowd);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn watchers_get_each_change_once_and_are_dropped_when_gone() {
        let d = private_dir("watch");
        let mut srv = ControlServer::bind_in(&d, ":9", uid()).unwrap();
        let mut conn = send(srv.path(), Request::Watch).unwrap();
        let (req, client) = next_request(&mut srv);
        assert_eq!(req, Request::Watch);
        srv.watch(client);

        let t = UNIX_EPOCH + Duration::from_secs(100);
        srv.set_status(Status::Blank, t, &[1]);
        srv.set_status(Status::Blank, t, &[1]);
        srv.set_status(Status::Lock, t, &[1]);
        assert_eq!(srv.status_line(), "LOCK 100 1");
        assert_eq!(read_line(&mut conn).unwrap().as_deref(), Some("BLANK 100 1"));
        assert_eq!(read_line(&mut conn).unwrap().as_deref(), Some("LOCK 100 1"));

        drop(conn);
        std::thread::sleep(Duration::from_millis(20));
        assert!(srv.poll(Instant::now()).is_empty());
        assert!(srv.watchers.is_empty(), "a watcher that hung up is forgotten");
        let _ = std::fs::remove_dir_all(&d);
    }
}
