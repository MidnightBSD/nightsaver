//! xscreensaver control protocol (atoms, ClientMessage encoding, root properties).
//!
//! Everything below was verified against the xscreensaver 6.17 sources
//! (`driver/remote.c`, `driver/clientmsg.c`, `driver/xscreensaver-command.c`,
//! `driver/xscreensaver.c`, `driver/windows.c`, `driver/atoms.c`) and against the
//! running xscreensaver 6.15 daemon on this machine (`xprop`, real
//! `xscreensaver-command -version` / `-time`).
//!
//! # Rendezvous window (daemon side)
//!
//! The daemon creates **one hidden window**: a direct child of the root window of
//! **screen 0**, 1x1 at (0,0), border 0, `InputOutput`, default depth/visual, never
//! mapped, `override_redirect = True`, `event_mask = PropertyChangeMask`.
//! The event mask is load-bearing: clients send their ClientMessage with
//! `XSendEvent(.., PropertyChangeMask, ..)`, which is only delivered to clients
//! that selected `PropertyChangeMask` on that window.
//! Properties on it (all `STRING`, format 8, `PropModeReplace`, no trailing NUL):
//!
//! | property               | content (real daemon)                                   |
//! |------------------------|---------------------------------------------------------|
//! | `WM_NAME`              | `"XScreenSaver Daemon"`                                 |
//! | `WM_CLASS`             | res_name = progname, res_class = **`"XScreenSaver"`**     |
//! | `WM_COMMAND`           | progname (e.g. `"xscreensaver"`)                        |
//! | `_SCREENSAVER_VERSION` | bare version number, e.g. `"6.15"`                      |
//! | `_SCREENSAVER_ID`      | `"%lu (%s@%s)"` = `"<pid> (<user>@<uname nodename>)"`   |
//!
//! The client prints `res_class + " " + version` for `-version`/`-time`, so
//! `WM_CLASS` is mandatory (without it the client fails with
//! `class hints not set on window 0x..?`). See [`publish_identity`], [`format_id`].
//!
//! # Locating the daemon (client side, `clientmsg.c: find_screensaver_window`)
//!
//! For each screen, `XQueryTree(root)`; for each top-level child call
//! `XGetWindowProperty(kid, _SCREENSAVER_VERSION, 0, 200, False, XA_STRING)`;
//! the first child where the call succeeds and the returned type is not `None`
//! is the daemon window (any existing property counts, even of another type).
//! `BadWindow` errors from children vanishing mid-walk are ignored.
//! The daemon itself uses the same walk at start-up and refuses to run with
//! `"already running on display %s (window 0x%x)\n from process <ID>"` if found.
//! See [`find_saver_window`] / [`find_saver_window_any_screen`].
//!
//! # Command ClientMessage (`remote.c: send_xscreensaver_command`)
//!
//! 1. Client selects `PropertyChangeMask` on the **daemon window** (its previous
//!    mask OR'ed in) so it can see the response.
//! 2. It sends a ClientMessage **to the daemon window, not the root**:
//!    `window = daemon_win`, `message_type = SCREENSAVER`, `format = 32`,
//!    `data.l = [command_atom, arg1, arg2, 0, 0]`, via
//!    `XSendEvent(dpy, daemon_win, propagate=False, PropertyChangeMask, &ev)`.
//!    - `SELECT N`: `arg1 = N` (1-based, must be > 0), `arg2 = 0`.
//!    - `DEMO N` (N != 0): `arg1 = 5000`, `arg2 = N` (legacy encoding).
//!    - `DEACTIVATE`: `arg1 = getppid()` of the client (logged by the daemon).
//!      Before sending DEACTIVATE the client also resets the server DPMS timer:
//!      if DPMS is capable and enabled: `DPMSForceLevel(On); DPMSDisable; DPMSEnable`.
//!    - everything else: `arg1 = arg2 = 0`.
//! 3. The daemon accepts the message only if `message_type == SCREENSAVER`,
//!    `format == 32` and `l[0] != 0`; for SELECT/DEMO it decodes
//!    `n = l[1]; if n == 5000 { n = l[2] }`. See [`encode_command`], [`parse_client_message`].
//!
//! `-version` and `-time` send nothing; they only read properties.
//!
//! # Response (`clientmsg.c: clientmessage_response`, `remote.c: xscreensaver_command_response`)
//!
//! The daemon replies by `XChangeProperty(ev.window /* = daemon window */,
//! _SCREENSAVER_RESPONSE, XA_STRING, 8, PropModeReplace, "+msg" | "-msg")`
//! (no trailing NUL): `'+'` = success, `'-'` = failure.
//! The client waits for a `PropertyNotify` with `state == NewValue` and
//! `atom == _SCREENSAVER_RESPONSE` using up to 10 one-second `select()` waits
//! (~10 s), else prints `"no response to command."`. It then reads the property
//! with `XGetWindowProperty(win, RESPONSE, 0, 1024, delete=True, AnyPropertyType)`:
//! - `BadWindow` while reading: for EXIT this is success (daemon went away),
//!   otherwise `"xscreensaver window unexpectedly deleted."`;
//! - type != STRING or format != 8: `"unrecognized response property."`;
//! - first byte not `+`/`-`: `"unrecognized response message."`.
//!
//! Messages used by xscreensaver (`xscreensaver.c`, `xscreensaver-gfx.c`):
//! ACTIVATE/SELECT/NEXT/PREV/DEMO while unblanked `+blanking`, ACTIVATE while
//! active `-already active` (SELECT/NEXT/PREV while active are answered by the
//! gfx process: `+cycling` for NEXT/PREV/CYCLE, `+selecting` for SELECT);
//! CYCLE while unblanked `-not blanked`; DEACTIVATE `+deactivating` or
//! `+already inactive, resetting activity time`; LOCK `+locking`,
//! `-locking disabled`, `-already locked`; SUSPEND `+suspending`;
//! EXIT `+exiting` or `-screen is locked`; RESTART `+restarting` or
//! `-screen is locked`. See [`send_response`], [`wait_response`].
//!
//! # `_SCREENSAVER_STATUS` (root window of **screen 0**, `windows.c` comment)
//!
//! Type `INTEGER`, format 32, `PropModeReplace`, at least 3 items:
//!
//! | index | meaning                                                         |
//! |-------|-----------------------------------------------------------------|
//! | 0     | state: atom `BLANK`, `LOCK`, `AUTH` (lock + dialog up) or `0`     |
//! | 1     | high 32 bits of the 64-bit `time_t` at which that state began   |
//! | 2     | low 32 bits of that `time_t`                                     |
//! | 3..   | 1-based hack (pattern) ordinal running on each screen            |
//!
//! xscreensaver writes it at start-up (`0, now`), on blank (`BLANK`), lock
//! (`LOCK`), password dialog (`AUTH`) and unblank (`0, now`). It is **never
//! deleted**; when unblanked the old hack ordinals remain (real `:0` shows
//! `0, 0, 1794122598, 198` while idle). Readers request 999 longs of type INTEGER
//! and require `type == INTEGER && nitems >= 3`. See [`set_status`], [`parse_status`].
//!
//! # `xscreensaver-command` output (`remote.c`, `xscreensaver-command.c`)
//!
//! - `-version`: `"<res_class> <version>\n"`, e.g. `XScreenSaver 6.15`.
//! - `-time`: `"<res_class> <version>: screen <blanked|locked|non-blanked> since <ctime>"`
//!   followed by `" (hack #N)\n"` (exactly one hack slot), `" (hacks: #a, #b)\n"`
//!   (several), or `"\n"` (no slots). `LOCK` and `AUTH` both print `locked`.
//!   Quirk: the "any hack running" test scans `data[2..2+nhacks]`, i.e. it
//!   includes the low time word, so it is effectively always true.
//!   On a missing property: stdout `"\n"`, stderr `"no saver status on root window"`;
//!   on a bad one: stderr `"bad status format on root window"` (no progname), exit 255.
//! - `-watch`: selects PropertyChange on screen-0 root, prints nothing initially;
//!   on each status change prints `BLANK <ctime>`, `LOCK <ctime>` or
//!   `UNBLANK <ctime>` when the state (AUTH folded into LOCK, compared against the
//!   previous *raw* state) changes, and `RUN a b ..` (hack ordinals) when any of
//!   items 2.. changed and any is non-zero; never on the first event. Bad status:
//!   `"<prog>: no saver status on root window"`, exit 255. See [`WatchState`].
//! - Responses print `"<prog>: <msg>\n"` followed by another `"\n"`; success to
//!   stdout only when verbose (the default; `-quiet` suppresses), failure always
//!   to stderr with exit status 255.
//! - After a successful ACTIVATE, SUSPEND, LOCK, NEXT, PREV or SELECT the client
//!   polls the status every 100 ms until state != 0 (10 s limit,
//!   `"Timed out waiting for screen to blank"`, exit 255; with default verbosity it
//!   prints `"waiting for status change"` after 3 s).
//! - No daemon: `"<prog>: no screensaver is running on display <dpy>"`, exit 255,
//!   except `-exit`, which is silent and exits 0.
//! - Usage errors `"<prog>: <what>: <arg>\n<prog>: try --help"` exit 1; `-help`
//!   prints usage on stderr, exit 1; unopenable display exit 1.
//! - `-exit` while locked is refused by the daemon (`-screen is locked`), so the
//!   client prints the error and exits 255. `-select N` requires N > 0.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use x11rb::connection::Connection;
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, EventMask,
    PropMode, Property, Window,
};
use x11rb::wrapper::ConnectionExt as _;

/// Atom names used by the protocol.
pub const ATOM_SCREENSAVER: &str = "SCREENSAVER";
pub const ATOM_VERSION: &str = "_SCREENSAVER_VERSION";
pub const ATOM_ID: &str = "_SCREENSAVER_ID";
pub const ATOM_STATUS: &str = "_SCREENSAVER_STATUS";
pub const ATOM_RESPONSE: &str = "_SCREENSAVER_RESPONSE";

/// Status atom names (item 0 of `_SCREENSAVER_STATUS`).
pub const ATOM_BLANK: &str = "BLANK";
pub const ATOM_LOCK: &str = "LOCK";
pub const ATOM_AUTH: &str = "AUTH";

/// Every atom name the protocol needs (as interned by xscreensaver's `atoms.c`,
/// plus `PREFS`, which pre-6.0 clients may still send).
pub const ALL_ATOM_NAMES: &[&str] = &[
    ATOM_SCREENSAVER,
    ATOM_ID,
    ATOM_VERSION,
    ATOM_STATUS,
    ATOM_RESPONSE,
    "ACTIVATE",
    "DEACTIVATE",
    "SUSPEND",
    "RESTART",
    "CYCLE",
    "NEXT",
    "PREV",
    "SELECT",
    "EXIT",
    "DEMO",
    "PREFS",
    ATOM_LOCK,
    ATOM_BLANK,
    ATOM_AUTH,
];

/// `WM_CLASS` res_class the real daemon uses; the client prints it verbatim.
pub const DAEMON_RES_CLASS: &str = "XScreenSaver";
/// `WM_NAME` of the real daemon window.
pub const DAEMON_WM_NAME: &str = "XScreenSaver Daemon";
/// Legacy DEMO argument marker (`remote.c`): `l[1] = 5000, l[2] = N`.
pub const DEMO_ARG_MARKER: u32 = 5000;

/// Commands sent by `xscreensaver-command` as ClientMessage data.l[0] atoms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Activate,
    Deactivate,
    Cycle,
    Next,
    Prev,
    /// 1-based index into the pattern list.
    Select(u32),
    Exit,
    Restart,
    Lock,
    Suspend,
    Demo,
    Prefs,
}

impl Command {
    /// Atom name for this command.
    pub fn atom_name(self) -> &'static str {
        match self {
            Command::Activate => "ACTIVATE",
            Command::Deactivate => "DEACTIVATE",
            Command::Cycle => "CYCLE",
            Command::Next => "NEXT",
            Command::Prev => "PREV",
            Command::Select(_) => "SELECT",
            Command::Exit => "EXIT",
            Command::Restart => "RESTART",
            Command::Lock => "LOCK",
            Command::Suspend => "SUSPEND",
            Command::Demo => "DEMO",
            Command::Prefs => "PREFS",
        }
    }

    /// Commands after which `xscreensaver-command` waits for the screen to blank.
    pub fn waits_for_blank(self) -> bool {
        matches!(
            self,
            Command::Activate
                | Command::Suspend
                | Command::Lock
                | Command::Next
                | Command::Prev
                | Command::Select(_)
        )
    }
}

/// Saver state as advertised in `_SCREENSAVER_STATUS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Off,
    Blank,
    Lock,
    Auth,
}

impl Status {
    /// Atom name stored in item 0, or `None` for `Off` (stored as 0).
    pub fn atom_name(self) -> Option<&'static str> {
        match self {
            Status::Off => None,
            Status::Blank => Some(ATOM_BLANK),
            Status::Lock => Some(ATOM_LOCK),
            Status::Auth => Some(ATOM_AUTH),
        }
    }
}

/// Errors from protocol helpers.
#[derive(Debug)]
pub enum ProtocolError {
    Connection(ConnectionError),
    Reply(ReplyError),
    /// An atom the protocol needs is not in the [`AtomTable`].
    MissingAtom(String),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::Connection(e) => write!(f, "X connection error: {e}"),
            ProtocolError::Reply(e) => write!(f, "X protocol error: {e}"),
            ProtocolError::MissingAtom(a) => write!(f, "atom {a} not interned"),
        }
    }
}
impl std::error::Error for ProtocolError {}
impl From<ConnectionError> for ProtocolError {
    fn from(e: ConnectionError) -> Self {
        ProtocolError::Connection(e)
    }
}
impl From<ReplyError> for ProtocolError {
    fn from(e: ReplyError) -> Self {
        ProtocolError::Reply(e)
    }
}

/// Atom lookup used by the protocol helpers; the daemon's atom cache can implement it.
pub trait AtomTable {
    fn atom(&self, name: &str) -> Option<Atom>;
    fn name(&self, atom: Atom) -> Option<&str>;
}

fn need(atoms: &impl AtomTable, name: &str) -> Result<Atom, ProtocolError> {
    atoms
        .atom(name)
        .ok_or_else(|| ProtocolError::MissingAtom(name.to_owned()))
}

/// Simple HashMap-based [`AtomTable`] holding [`ALL_ATOM_NAMES`].
#[derive(Debug, Clone, Default)]
pub struct ProtocolAtoms {
    by_name: HashMap<String, Atom>,
    by_atom: HashMap<Atom, String>,
}

impl ProtocolAtoms {
    /// Intern every name in [`ALL_ATOM_NAMES`] (pipelined, `only_if_exists = false`
    /// as xscreensaver does).
    pub fn intern<C: Connection>(conn: &C) -> Result<Self, ProtocolError> {
        let cookies = ALL_ATOM_NAMES
            .iter()
            .map(|n| conn.intern_atom(false, n.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pairs = Vec::with_capacity(cookies.len());
        for (name, cookie) in ALL_ATOM_NAMES.iter().zip(cookies) {
            pairs.push((*name, cookie.reply()?.atom));
        }
        Ok(Self::from_pairs(pairs))
    }

    /// Build from explicit `(name, atom)` pairs (tests, or a daemon-side cache).
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, Atom)>) -> Self {
        let mut t = Self::default();
        for (n, a) in pairs {
            t.by_name.insert(n.to_owned(), a);
            t.by_atom.insert(a, n.to_owned());
        }
        t
    }
}

impl AtomTable for ProtocolAtoms {
    fn atom(&self, name: &str) -> Option<Atom> {
        self.by_name.get(name).copied()
    }
    fn name(&self, atom: Atom) -> Option<&str> {
        self.by_atom.get(&atom).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Locating the daemon
// ---------------------------------------------------------------------------

/// Find the daemon window among the top-level children of `root`
/// (`clientmsg.c: find_screensaver_window`, one screen).
pub fn find_saver_window<C: Connection>(
    conn: &C,
    root: Window,
    atoms: &impl AtomTable,
) -> Result<Option<Window>, ProtocolError> {
    let version = need(atoms, ATOM_VERSION)?;
    let tree = conn.query_tree(root)?.reply()?;
    // Pipeline all requests, then check replies in stacking order.
    let cookies = tree
        .children
        .iter()
        .map(|&kid| {
            conn.get_property(false, kid, version, AtomEnum::STRING, 0, 200)
                .map(|c| (kid, c))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut found = None;
    for (kid, cookie) in cookies {
        match cookie.reply() {
            Ok(r) if found.is_none() && r.type_ != u32::from(AtomEnum::NONE) => found = Some(kid),
            Ok(_) => {}
            // BadWindow race: the child vanished; it was not the daemon.
            Err(ReplyError::X11Error(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(found)
}

/// Like [`find_saver_window`] but walks the roots of all screens in order, as
/// xscreensaver-command does.
pub fn find_saver_window_any_screen<C: Connection>(
    conn: &C,
    atoms: &impl AtomTable,
) -> Result<Option<Window>, ProtocolError> {
    for screen in &conn.setup().roots {
        if let Some(w) = find_saver_window(conn, screen.root, atoms)? {
            return Ok(Some(w));
        }
    }
    Ok(None)
}

/// Read a STRING/format-8 property as text (lossy UTF-8). `None` if absent.
pub fn read_string_property<C: Connection>(
    conn: &C,
    window: Window,
    property: Atom,
    long_length: u32,
) -> Result<Option<String>, ProtocolError> {
    let r = conn
        .get_property(false, window, property, AtomEnum::STRING, 0, long_length)?
        .reply()?;
    if r.type_ == u32::from(AtomEnum::NONE) {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&r.value).into_owned()))
}

/// Read `_SCREENSAVER_VERSION` from the daemon window (`""` if set with another type).
pub fn read_version<C: Connection>(
    conn: &C,
    saver_win: Window,
    atoms: &impl AtomTable,
) -> Result<Option<String>, ProtocolError> {
    read_string_property(conn, saver_win, need(atoms, ATOM_VERSION)?, 200)
}

/// Read `_SCREENSAVER_ID` from the daemon window.
pub fn read_id<C: Connection>(
    conn: &C,
    saver_win: Window,
    atoms: &impl AtomTable,
) -> Result<Option<String>, ProtocolError> {
    read_string_property(conn, saver_win, need(atoms, ATOM_ID)?, 512)
}

/// Read the res_class part of `WM_CLASS` (`XGetClassHint`). `None` if unset.
pub fn read_res_class<C: Connection>(
    conn: &C,
    window: Window,
) -> Result<Option<String>, ProtocolError> {
    let r = conn
        .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 2048)?
        .reply()?;
    if r.type_ != u32::from(AtomEnum::STRING) || r.format != 8 {
        return Ok(None);
    }
    Ok(parse_wm_class(&r.value).map(|(_, class)| class))
}

/// Split a `WM_CLASS` value `"name\0class\0"` into `(res_name, res_class)`.
pub fn parse_wm_class(value: &[u8]) -> Option<(String, String)> {
    let mut parts = value.split(|&b| b == 0);
    let name = parts.next()?;
    let class = parts.next()?;
    Some((
        String::from_utf8_lossy(name).into_owned(),
        String::from_utf8_lossy(class).into_owned(),
    ))
}

/// Split `_SCREENSAVER_ID` (`"pid (user@host)"` or legacy `"pid on host xyz"`)
/// into `(user, host)` like `remote.c: server_xscreensaver_version`.
pub fn parse_id(id: &str) -> (Option<String>, Option<String>) {
    let keep = |s: &str| (!s.is_empty() && !s.starts_with('?')).then(|| s.to_owned());
    if let Some(pos) = id.find(" on host ") {
        return (None, keep(&id[pos + " on host ".len()..]));
    }
    if let Some(o) = id.find('(')
        && let Some(p) = id[o..].rfind('@').map(|p| p + o)
        && let Some(c) = id[p..].find(')').map(|c| c + p)
    {
        return (keep(&id[o + 1..p]), keep(&id[p + 1..c]));
    }
    (None, None)
}

/// Format `_SCREENSAVER_ID` exactly like the real daemon: `"%lu (%s@%s)"`.
pub fn format_id(pid: u32, user: &str, host: &str) -> String {
    format!("{pid} ({user}@{host})")
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// The five 32-bit data words of the command ClientMessage.
/// `caller_pid` is only used for DEACTIVATE (the client's parent pid).
pub fn encode_command(
    cmd: Command,
    atoms: &impl AtomTable,
    caller_pid: u32,
) -> Result<[u32; 5], ProtocolError> {
    let a = need(atoms, cmd.atom_name())?;
    let (arg1, arg2) = match cmd {
        Command::Select(n) => (n, 0),
        Command::Deactivate => (caller_pid, 0),
        _ => (0, 0),
    };
    Ok([a, arg1, arg2, 0, 0])
}

/// Build the ClientMessage event addressed to the daemon window.
pub fn build_client_message(
    saver_win: Window,
    atoms: &impl AtomTable,
    data: [u32; 5],
) -> Result<ClientMessageEvent, ProtocolError> {
    Ok(ClientMessageEvent::new(
        32,
        saver_win,
        need(atoms, ATOM_SCREENSAVER)?,
        data,
    ))
}

/// Send `cmd` to the daemon like `remote.c`: select PropertyChange on the daemon
/// window (so [`wait_response`] can see the reply), then
/// `SendEvent(daemon_win, propagate=false, PropertyChangeMask, ClientMessage)`.
/// DEACTIVATE carries the parent pid; the DPMS reset is up to the caller.
pub fn send_command<C: Connection>(
    conn: &C,
    saver_win: Window,
    atoms: &impl AtomTable,
    cmd: Command,
) -> Result<(), ProtocolError> {
    let data = encode_command(cmd, atoms, std::os::unix::process::parent_id())?;
    send_command_data(conn, saver_win, atoms, data)
}

/// Lower-level [`send_command`] taking the raw data words (e.g. the legacy
/// `DEMO` encoding `[DEMO, 5000, N, 0, 0]`).
pub fn send_command_data<C: Connection>(
    conn: &C,
    saver_win: Window,
    atoms: &impl AtomTable,
    data: [u32; 5],
) -> Result<(), ProtocolError> {
    select_property_change(conn, saver_win)?;
    let ev = build_client_message(saver_win, atoms, data)?;
    conn.send_event(false, saver_win, EventMask::PROPERTY_CHANGE, ev)?;
    conn.flush()?;
    Ok(())
}

/// Add `PropertyChangeMask` to this client's event mask on `window`
/// (`XGetWindowAttributes` + `XSelectInput(your_event_mask | PropertyChangeMask)`).
pub fn select_property_change<C: Connection>(
    conn: &C,
    window: Window,
) -> Result<(), ProtocolError> {
    let attrs = conn.get_window_attributes(window)?.reply()?;
    conn.change_window_attributes(
        window,
        &ChangeWindowAttributesAux::new()
            .event_mask(attrs.your_event_mask | EventMask::PROPERTY_CHANGE),
    )?;
    Ok(())
}

/// Decode a command ClientMessage on the daemon side (`xscreensaver.c`):
/// requires type SCREENSAVER, format 32, `l[0] != 0` and a known command atom.
pub fn parse_client_message(ev: &ClientMessageEvent, atoms: &impl AtomTable) -> Option<Command> {
    if ev.format != 32 || Some(ev.type_) != atoms.atom(ATOM_SCREENSAVER) {
        return None;
    }
    decode_command(ev.data.as_data32(), atoms)
}

/// Decode the data words of a command message (see [`parse_client_message`]).
pub fn decode_command(l: [u32; 5], atoms: &impl AtomTable) -> Option<Command> {
    if l[0] == 0 {
        return None;
    }
    let n = if l[1] == DEMO_ARG_MARKER { l[2] } else { l[1] };
    Some(match atoms.name(l[0])? {
        "ACTIVATE" => Command::Activate,
        "DEACTIVATE" => Command::Deactivate,
        "CYCLE" => Command::Cycle,
        "NEXT" => Command::Next,
        "PREV" => Command::Prev,
        "SELECT" => Command::Select(n),
        "EXIT" => Command::Exit,
        "RESTART" => Command::Restart,
        "LOCK" => Command::Lock,
        "SUSPEND" => Command::Suspend,
        "DEMO" => Command::Demo,
        "PREFS" => Command::Prefs,
        _ => return None,
    })
}

/// Pid of the sender of a DEACTIVATE message (`l[1]`), if any; for logging.
pub fn deactivate_caller_pid(ev: &ClientMessageEvent) -> Option<u32> {
    let pid = ev.data.as_data32()[1];
    (pid != 0).then_some(pid)
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// Encode a response property value: `'+'`/`'-'` followed by the message, no NUL.
pub fn encode_response(ok: bool, msg: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(msg.len() + 1);
    v.push(if ok { b'+' } else { b'-' });
    v.extend_from_slice(msg.as_bytes());
    v
}

/// Daemon side: reply to a command by setting `_SCREENSAVER_RESPONSE` (STRING,
/// format 8, Replace) on `to_window`, which must be the window the ClientMessage
/// was addressed to (the daemon window).
pub fn send_response<C: Connection>(
    conn: &C,
    atoms: &impl AtomTable,
    to_window: Window,
    ok: bool,
    msg: &str,
) -> Result<(), ProtocolError> {
    conn.change_property8(
        PropMode::REPLACE,
        to_window,
        need(atoms, ATOM_RESPONSE)?,
        AtomEnum::STRING,
        &encode_response(ok, msg),
    )?;
    conn.flush()?;
    Ok(())
}

/// Result of waiting for and reading a command response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// `+msg` (`ok = true`) or `-msg` (`ok = false`); `text` excludes the prefix.
    Reply { ok: bool, text: String },
    /// No PropertyNotify within the timeout: `"no response to command."`.
    NoResponse,
    /// Reading the property failed with an X error (window gone).
    WindowDeleted,
    /// Wrong type/format: `"unrecognized response property."`.
    BadProperty,
    /// Missing `+`/`-`: `"unrecognized response message."`.
    BadMessage,
}

/// Classify a `_SCREENSAVER_RESPONSE` property read with AnyPropertyType.
/// `type_ == 0` (None) yields `None` (the C client falls through: treated as error).
pub fn parse_response(type_: Atom, format: u8, value: &[u8]) -> Option<Response> {
    if type_ == u32::from(AtomEnum::NONE) {
        return None;
    }
    if type_ != u32::from(AtomEnum::STRING) || format != 8 {
        return Some(Response::BadProperty);
    }
    let ok = match value.first() {
        Some(b'+') => true,
        Some(b'-') => false,
        _ => return Some(Response::BadMessage),
    };
    // The C client prints the message as a C string: stop at the first NUL.
    let body = &value[1..];
    let body = body.split(|&b| b == 0).next().unwrap_or(body);
    Some(Response::Reply {
        ok,
        text: String::from_utf8_lossy(body).into_owned(),
    })
}

/// Client side: wait up to `timeout` (xscreensaver: ~10 s) for a
/// `PropertyNotify(NewValue, _SCREENSAVER_RESPONSE)`, then read the property from
/// `saver_win` with delete=true, AnyPropertyType, length 1024.
/// Returns `Ok(None)` if the property vanished before it could be read (the C
/// client treats that as an error as well).
pub fn wait_response<C: Connection>(
    conn: &C,
    saver_win: Window,
    atoms: &impl AtomTable,
    timeout: Duration,
) -> Result<Option<Response>, ProtocolError> {
    let resp = need(atoms, ATOM_RESPONSE)?;
    let started = Instant::now();
    let deadline = started + timeout;
    loop {
        let mut got = false;
        while let Some(ev) = conn.poll_for_event()? {
            if let Event::PropertyNotify(p) = ev
                && p.atom == resp
                && p.state == Property::NEW_VALUE
            {
                got = true;
                break;
            }
        }
        if got {
            break;
        }
        if Instant::now() >= deadline {
            return Ok(Some(Response::NoResponse));
        }
        // The C client blocks in select() and wakes immediately. For EXIT the
        // daemon destroys its window right after replying, so a coarse sleep
        // loses the race to read the reply. Spin (yielding) briefly, then back off.
        if started.elapsed() < Duration::from_millis(1500) {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let cookie = conn.get_property(true, saver_win, resp, AtomEnum::ANY, 0, 1024)?;
    match cookie.reply() {
        Ok(r) => Ok(parse_response(r.type_, r.format, &r.value)),
        Err(ReplyError::X11Error(_)) => Ok(Some(Response::WindowDeleted)),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Identity (daemon side)
// ---------------------------------------------------------------------------

/// Daemon side: publish identity properties on the (unmapped, override-redirect,
/// PropertyChange-selecting) rendezvous window exactly like
/// `xscreensaver.c: create_daemon_window`: WM_NAME, WM_CLASS
/// (`progname\0XScreenSaver\0`), WM_COMMAND, `_SCREENSAVER_VERSION`, `_SCREENSAVER_ID`.
/// `version` should be the bare compat version (e.g. `"6.15"`), `id` from [`format_id`].
pub fn publish_identity<C: Connection>(
    conn: &C,
    window: Window,
    atoms: &impl AtomTable,
    progname: &str,
    version: &str,
    id: &str,
) -> Result<(), ProtocolError> {
    let version_atom = need(atoms, ATOM_VERSION)?;
    let id_atom = need(atoms, ATOM_ID)?;
    let mut class = Vec::new();
    class.extend_from_slice(progname.as_bytes());
    class.push(0);
    class.extend_from_slice(DAEMON_RES_CLASS.as_bytes());
    class.push(0);
    let s = AtomEnum::STRING;
    let r = PropMode::REPLACE;
    conn.change_property8(r, window, AtomEnum::WM_NAME, s, DAEMON_WM_NAME.as_bytes())?;
    conn.change_property8(r, window, AtomEnum::WM_CLASS, s, &class)?;
    conn.change_property8(r, window, AtomEnum::WM_COMMAND, s, progname.as_bytes())?;
    conn.change_property8(r, window, version_atom, s, version.as_bytes())?;
    conn.change_property8(r, window, id_atom, s, id.as_bytes())?;
    conn.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Status property
// ---------------------------------------------------------------------------

/// Seconds since the epoch for `t` (negative for pre-1970 times).
fn epoch_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Encode the `_SCREENSAVER_STATUS` items: `[state, time_hi, time_lo, hacks..]`.
pub fn encode_status(
    atoms: &impl AtomTable,
    status: Status,
    since: SystemTime,
    hack_indices: &[u32],
) -> Result<Vec<u32>, ProtocolError> {
    let state = match status.atom_name() {
        Some(n) => need(atoms, n)?,
        None => 0,
    };
    let t = epoch_secs(since) as u64;
    let mut v = Vec::with_capacity(3 + hack_indices.len());
    v.push(state);
    v.push((t >> 32) as u32);
    v.push((t & 0xFFFF_FFFF) as u32);
    v.extend_from_slice(hack_indices);
    Ok(v)
}

/// Daemon side: write `_SCREENSAVER_STATUS` on `root` (must be screen 0's root).
/// Use `Status::Off` with the unblank time when unblanking; xscreensaver keeps the
/// last hack ordinals in that case. `hack_indices` are 1-based, one per screen.
pub fn set_status<C: Connection>(
    conn: &C,
    root: Window,
    atoms: &impl AtomTable,
    status: Status,
    since: SystemTime,
    hack_indices: &[u32],
) -> Result<(), ProtocolError> {
    let items = encode_status(atoms, status, since, hack_indices)?;
    conn.change_property32(
        PropMode::REPLACE,
        root,
        need(atoms, ATOM_STATUS)?,
        AtomEnum::INTEGER,
        &items,
    )?;
    conn.flush()?;
    Ok(())
}

/// Delete `_SCREENSAVER_STATUS` from `root`. **Only for daemon exit**: the real
/// daemon never deletes it (unblank is `set_status(Status::Off, now, ..)`), and a
/// missing property makes `-time` report "no saver status on root window".
pub fn clear_status<C: Connection>(
    conn: &C,
    root: Window,
    atoms: &impl AtomTable,
) -> Result<(), ProtocolError> {
    conn.delete_property(root, need(atoms, ATOM_STATUS)?)?;
    conn.flush()?;
    Ok(())
}

/// Status property as read from the server, before interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawStatus {
    /// Property absent.
    Missing,
    /// Present but not INTEGER/32 with >= 3 items.
    Bad,
    /// The 32-bit items.
    Items(Vec<u32>),
}

/// Read `_SCREENSAVER_STATUS` from `root` like the C client
/// (`0, 999, False, XA_INTEGER`).
pub fn read_status<C: Connection>(
    conn: &C,
    root: Window,
    atoms: &impl AtomTable,
) -> Result<RawStatus, ProtocolError> {
    let r = conn
        .get_property(
            false,
            root,
            need(atoms, ATOM_STATUS)?,
            AtomEnum::INTEGER,
            0,
            999,
        )?
        .reply()?;
    if r.type_ == u32::from(AtomEnum::NONE) {
        return Ok(RawStatus::Missing);
    }
    if r.type_ != u32::from(AtomEnum::INTEGER) || r.format != 32 {
        return Ok(RawStatus::Bad);
    }
    let items: Vec<u32> = match r.value32() {
        Some(it) => it.collect(),
        None => return Ok(RawStatus::Bad),
    };
    if items.len() < 3 {
        return Ok(RawStatus::Bad);
    }
    Ok(RawStatus::Items(items))
}

/// The `time_t` stored in items 1 (high) and 2 (low).
pub fn status_time(items: &[u32]) -> Option<i64> {
    if items.len() < 3 {
        return None;
    }
    Some(((u64::from(items[1]) << 32) | u64::from(items[2])) as i64)
}

/// Map the state word (item 0) to a [`Status`]; `None` for an unknown atom.
pub fn status_from_atom(state: Atom, atoms: &impl AtomTable) -> Option<Status> {
    if state == 0 {
        return Some(Status::Off);
    }
    match atoms.name(state)? {
        ATOM_BLANK => Some(Status::Blank),
        ATOM_LOCK => Some(Status::Lock),
        ATOM_AUTH => Some(Status::Auth),
        _ => None,
    }
}

/// Decode the 32-bit items of `_SCREENSAVER_STATUS`.
/// `None` if fewer than 3 items or the state word is not 0/BLANK/LOCK/AUTH.
pub fn parse_status(
    items: &[u32],
    atoms: &impl AtomTable,
) -> Option<(Status, SystemTime, Vec<u32>)> {
    let t = status_time(items)?;
    let status = status_from_atom(items[0], atoms)?;
    let since = if t >= 0 {
        UNIX_EPOCH + Duration::from_secs(t as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(t.unsigned_abs())
    };
    Some((status, since, items[3..].to_vec()))
}

/// Format a `ctime(3)` string (without the trailing newline) from broken-down
/// local time: `"Sun Nov  8 02:23:18 2026"`. `wday` 0 = Sunday, `mon` 0 = January.
pub fn format_ctime(
    wday: u32,
    mon: u32,
    mday: u32,
    hour: u32,
    min: u32,
    sec: u32,
    year: i64,
) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let d = DAYS.get(wday as usize).copied().unwrap_or("???");
    let m = MONS.get(mon as usize).copied().unwrap_or("???");
    format!("{d} {m}{mday:3} {hour:02}:{min:02}:{sec:02} {year}")
}

/// Outcome of formatting the `-time` status suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeLine {
    /// Text to print after `"<class> <version>"`, including the final newline.
    Ok(String),
    /// Unknown state atom: stderr `"bad status format on root window"`.
    BadState,
}

/// Format the `-time` suffix from status items, given a `ctime` formatter for the
/// time word (so tests do not depend on the local time zone). Mirrors `remote.c`,
/// including the quirky "any" test over `data[2..2+nhacks]`.
pub fn format_time_line(
    items: &[u32],
    atoms: &impl AtomTable,
    ctime: impl Fn(i64) -> String,
) -> TimeLine {
    let Some(t) = status_time(items) else {
        return TimeLine::BadState;
    };
    let what = match status_from_atom(items[0], atoms) {
        Some(Status::Blank) => "blanked",
        Some(Status::Lock | Status::Auth) => "locked",
        Some(Status::Off) => "non-blanked",
        None => return TimeLine::BadState,
    };
    let mut s = format!(": screen {what} since {}", ctime(t));
    let off = 3;
    let nhacks = items.len() - off;
    let any = (0..nhacks).any(|i| items[i + 2] > 0);
    if any && nhacks == 1 {
        s.push_str(&format!(" (hack #{})\n", items[off] as i32));
    } else if any {
        let list: Vec<String> = items[off..]
            .iter()
            .map(|h| format!("#{}", *h as i32))
            .collect();
        s.push_str(&format!(" (hacks: {})\n", list.join(", ")));
    } else {
        s.push('\n');
    }
    TimeLine::Ok(s)
}

/// `-watch` state machine (`xscreensaver-command.c: watch`).
#[derive(Debug, Clone, Default)]
pub struct WatchState {
    last: Option<Vec<u32>>,
}

impl WatchState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed newly read status items; returns the lines to print (without
    /// newlines), or `None` for an unknown state ("no saver status on root window").
    pub fn update(
        &mut self,
        items: &[u32],
        atoms: &impl AtomTable,
        ctime: impl Fn(i64) -> String,
    ) -> Option<Vec<String>> {
        let t = status_time(items)?;
        let mut out = Vec::new();
        let mut state = items[0];
        let auth = atoms.atom(ATOM_AUTH);
        let lock = atoms.atom(ATOM_LOCK);
        if Some(state) == auth && state != 0 {
            state = lock.unwrap_or(state);
        }
        let last_state = self.last.as_ref().map(|l| l[0]);
        if last_state != Some(state) {
            let word = match status_from_atom(state, atoms) {
                Some(Status::Blank) => "BLANK",
                Some(Status::Lock | Status::Auth) => "LOCK",
                Some(Status::Off) => "UNBLANK",
                None => return None,
            };
            out.push(format!("{word} {}", ctime(t)));
        }
        let (changed, running) = match &self.last {
            None => (true, false),
            Some(last) => {
                let mut changed = false;
                let mut running = false;
                for (i, &d) in items.iter().enumerate().skip(2) {
                    if last.get(i) != Some(&d) {
                        changed = true;
                    }
                    if d != 0 {
                        running = true;
                    }
                }
                (changed, running)
            }
        };
        if running && changed {
            let mut line = String::from("RUN");
            for h in &items[3..] {
                line.push_str(&format!(" {}", *h as i32));
            }
            out.push(line);
        }
        self.last = Some(items.to_vec());
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atoms() -> ProtocolAtoms {
        ProtocolAtoms::from_pairs(
            ALL_ATOM_NAMES
                .iter()
                .enumerate()
                .map(|(i, n)| (*n, 300 + i as u32)),
        )
    }

    fn ct(t: i64) -> String {
        format!("T{t}")
    }

    #[test]
    fn encode_decode_roundtrip() {
        let a = atoms();
        let cmds = [
            Command::Activate,
            Command::Deactivate,
            Command::Cycle,
            Command::Next,
            Command::Prev,
            Command::Select(7),
            Command::Exit,
            Command::Restart,
            Command::Lock,
            Command::Suspend,
            Command::Demo,
            Command::Prefs,
        ];
        for c in cmds {
            let d = encode_command(c, &a, 4242).unwrap();
            assert_eq!(d[0], a.atom(c.atom_name()).unwrap());
            assert_eq!(decode_command(d, &a), Some(c));
            let ev = build_client_message(0x1200001, &a, d).unwrap();
            assert_eq!(ev.format, 32);
            assert_eq!(ev.window, 0x1200001);
            assert_eq!(Some(ev.type_), a.atom(ATOM_SCREENSAVER));
            assert_eq!(parse_client_message(&ev, &a), Some(c));
        }
    }

    #[test]
    fn encode_args() {
        let a = atoms();
        let sel = encode_command(Command::Select(3), &a, 99).unwrap();
        assert_eq!(&sel[1..], &[3, 0, 0, 0]);
        let de = encode_command(Command::Deactivate, &a, 99).unwrap();
        assert_eq!(&de[1..], &[99, 0, 0, 0]);
        let ac = encode_command(Command::Activate, &a, 99).unwrap();
        assert_eq!(&ac[1..], &[0, 0, 0, 0]);
    }

    #[test]
    fn decode_legacy_demo_select() {
        let a = atoms();
        let sel = a.atom("SELECT").unwrap();
        assert_eq!(
            decode_command([sel, 5000, 12, 0, 0], &a),
            Some(Command::Select(12))
        );
    }

    #[test]
    fn reject_bad_messages() {
        let a = atoms();
        let act = a.atom("ACTIVATE").unwrap();
        // l[0] == 0
        assert_eq!(decode_command([0, 0, 0, 0, 0], &a), None);
        // unknown atom
        assert_eq!(decode_command([9999, 0, 0, 0, 0], &a), None);
        // wrong message type
        let ev = ClientMessageEvent::new(32, 1, 12345u32, [act, 0, 0, 0, 0]);
        assert_eq!(parse_client_message(&ev, &a), None);
        // wrong format
        let mut ev = build_client_message(1, &a, [act, 0, 0, 0, 0]).unwrap();
        ev.format = 8;
        assert_eq!(parse_client_message(&ev, &a), None);
    }

    #[test]
    fn missing_atom_is_error() {
        let a = ProtocolAtoms::default();
        assert!(matches!(
            encode_command(Command::Activate, &a, 0),
            Err(ProtocolError::MissingAtom(_))
        ));
    }

    #[test]
    fn response_encoding() {
        assert_eq!(encode_response(true, "blanking"), b"+blanking");
        assert_eq!(
            encode_response(false, "screen is locked"),
            b"-screen is locked"
        );
        let s = u32::from(AtomEnum::STRING);
        assert_eq!(
            parse_response(s, 8, b"+blanking"),
            Some(Response::Reply {
                ok: true,
                text: "blanking".into()
            })
        );
        assert_eq!(
            parse_response(s, 8, b"-already locked"),
            Some(Response::Reply {
                ok: false,
                text: "already locked".into()
            })
        );
        assert_eq!(parse_response(s, 8, b"xyz"), Some(Response::BadMessage));
        assert_eq!(parse_response(s, 8, b""), Some(Response::BadMessage));
        assert_eq!(parse_response(s, 32, b"+x"), Some(Response::BadProperty));
        assert_eq!(
            parse_response(u32::from(AtomEnum::INTEGER), 8, b"+x"),
            Some(Response::BadProperty)
        );
        assert_eq!(parse_response(0, 0, b""), None);
    }

    #[test]
    fn status_roundtrip() {
        let a = atoms();
        let since = UNIX_EPOCH + Duration::from_secs(1_794_122_598);
        let items = encode_status(&a, Status::Lock, since, &[198, 4]).unwrap();
        assert_eq!(
            items,
            vec![a.atom("LOCK").unwrap(), 0, 1_794_122_598, 198, 4]
        );
        assert_eq!(
            parse_status(&items, &a),
            Some((Status::Lock, since, vec![198, 4]))
        );
        let off = encode_status(&a, Status::Off, since, &[]).unwrap();
        assert_eq!(off, vec![0, 0, 1_794_122_598]);
        assert_eq!(parse_status(&off, &a), Some((Status::Off, since, vec![])));
        // 64-bit time split
        let far = UNIX_EPOCH + Duration::from_secs(0x1_0000_0005);
        let it = encode_status(&a, Status::Blank, far, &[]).unwrap();
        assert_eq!(&it[1..], &[1, 5]);
        assert_eq!(parse_status(&it, &a).unwrap().1, far);
    }

    #[test]
    fn status_parse_rejects() {
        let a = atoms();
        assert_eq!(parse_status(&[0, 0], &a), None);
        assert_eq!(parse_status(&[77777, 0, 1], &a), None);
        let auth = a.atom("AUTH").unwrap();
        assert_eq!(parse_status(&[auth, 0, 1], &a).unwrap().0, Status::Auth);
    }

    #[test]
    fn time_line_matches_real_output() {
        let a = atoms();
        // Real :0: "_SCREENSAVER_STATUS(INTEGER) = 0, 0, 1794122598, 198"
        // -> ": screen non-blanked since <ctime> (hack #198)"
        assert_eq!(
            format_time_line(&[0, 0, 1_794_122_598, 198], &a, ct),
            TimeLine::Ok(": screen non-blanked since T1794122598 (hack #198)\n".into())
        );
        let blank = a.atom("BLANK").unwrap();
        assert_eq!(
            format_time_line(&[blank, 0, 5], &a, ct),
            TimeLine::Ok(": screen blanked since T5\n".into())
        );
        let auth = a.atom("AUTH").unwrap();
        assert_eq!(
            format_time_line(&[auth, 0, 5, 1, 2], &a, ct),
            TimeLine::Ok(": screen locked since T5 (hacks: #1, #2)\n".into())
        );
        // Quirk: the "any" scan includes the low time word, so a 0 hack still prints.
        assert_eq!(
            format_time_line(&[0, 0, 5, 0], &a, ct),
            TimeLine::Ok(": screen non-blanked since T5 (hack #0)\n".into())
        );
        assert_eq!(format_time_line(&[4242, 0, 5], &a, ct), TimeLine::BadState);
    }

    #[test]
    fn watch_sequence() {
        let a = atoms();
        let blank = a.atom("BLANK").unwrap();
        let lock = a.atom("LOCK").unwrap();
        let auth = a.atom("AUTH").unwrap();
        let mut w = WatchState::new();
        // First event: state line, never RUN.
        assert_eq!(
            w.update(&[blank, 0, 10, 3], &a, ct).unwrap(),
            vec!["BLANK T10"]
        );
        // Hack change only.
        assert_eq!(w.update(&[blank, 0, 10, 4], &a, ct).unwrap(), vec!["RUN 4"]);
        // Lock: state + RUN (time word changed).
        assert_eq!(
            w.update(&[lock, 0, 20, 4], &a, ct).unwrap(),
            vec!["LOCK T20", "RUN 4"]
        );
        // AUTH folds into LOCK: nothing printed when nothing else changed.
        assert_eq!(
            w.update(&[auth, 0, 20, 4], &a, ct).unwrap(),
            Vec::<String>::new()
        );
        // AUTH -> LOCK: compared against raw last state (AUTH), so LOCK prints again.
        assert_eq!(
            w.update(&[lock, 0, 20, 4], &a, ct).unwrap(),
            vec!["LOCK T20"]
        );
        assert_eq!(
            w.update(&[0, 0, 30, 4], &a, ct).unwrap(),
            vec!["UNBLANK T30", "RUN 4"]
        );
        assert_eq!(w.update(&[999, 0, 30], &a, ct), None);
    }

    #[test]
    fn ctime_format() {
        assert_eq!(
            format_ctime(0, 10, 8, 2, 23, 18, 2026),
            "Sun Nov  8 02:23:18 2026"
        );
        assert_eq!(
            format_ctime(3, 0, 21, 14, 5, 9, 2026),
            "Wed Jan 21 14:05:09 2026"
        );
    }

    #[test]
    fn wm_class_and_id() {
        assert_eq!(
            parse_wm_class(b"xscreensaver\0XScreenSaver\0"),
            Some(("xscreensaver".into(), "XScreenSaver".into()))
        );
        assert_eq!(parse_wm_class(b"x"), None);
        assert_eq!(format_id(2998, "laffer1", "m5"), "2998 (laffer1@m5)");
        assert_eq!(
            parse_id("2998 (laffer1@m5)"),
            (Some("laffer1".into()), Some("m5".into()))
        );
        assert_eq!(
            parse_id("1 (a@b@host)"),
            (Some("a@b".into()), Some("host".into()))
        );
        assert_eq!(parse_id("1234 on host xyz"), (None, Some("xyz".into())));
        assert_eq!(parse_id("1 (???@???)"), (None, None));
    }
}
