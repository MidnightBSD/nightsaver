//! Connection setup, extension checks, atom cache. Owner: Opus A.
//!
//! Contract: `X11::open(display) -> Result<X11, X11Error>`; exposes `conn: XCBConnection`,
//! `screen_num`, `root`, `depth`, `visual`, `atoms` (interned names from compat::protocol
//! plus WM_PROTOCOLS etc.), and `has_screensaver_ext / has_xinput2 / has_randr / has_dpms / has_shm`
//! (plus `has_xfixes`).
//!
//! Required extensions (open fails with `X11Error::MissingExtension`): MIT-SCREEN-SAVER,
//! XInputExtension >= 2.0 (we announce 2.2 so raw events are delivered even during grabs; the
//! negotiation is required before `xi_select_events`), RANDR (version 1.5 negotiated here).
//! Optional: DPMS, MIT-SHM, XFIXES (XFIXES version is negotiated here when present).
//!
//! DPMS is optional (contract change): Xvfb on the target system has no DPMS extension at all,
//! and Xvfb is the mandated test server. Callers must gate every `dpms::` request on `has_dpms`.
//!
//! Event helpers: `poll_event` / `wait_event_timeout` return X protocol errors as
//! `Some(Event::Error(..))` (that is how libxcb delivers errors of unchecked void requests);
//! only a broken connection is an `Err`.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{Atom, ConnectionExt as _, Screen, Visualid, Window};
use x11rb::protocol::{dpms, randr, screensaver, shm, xfixes, xinput};
use x11rb::xcb_ffi::XCBConnection;

use super::X11Error;
use crate::compat::protocol::{
    ATOM_ID, ATOM_RESPONSE, ATOM_SCREENSAVER, ATOM_STATUS, ATOM_VERSION, Command,
};

/// Extra (non-protocol) atoms we always intern.
const EXTRA_ATOMS: &[&str] = &[
    "WM_PROTOCOLS",
    "WM_DELETE_WINDOW",
    "WM_NAME",
    "_NET_WM_NAME",
    "_NET_WM_STATE",
    "_NET_WM_STATE_FULLSCREEN",
    "_NET_WM_STATE_ABOVE",
    "_NET_WM_WINDOW_TYPE",
    "UTF8_STRING",
    "STRING",
];

/// Status names advertised in `_SCREENSAVER_STATUS`.
const STATUS_ATOMS: &[&str] = &["BLANK", "LOCK", "AUTH"];

/// All atom names to intern (duplicates are removed in `Atoms::intern`).
fn all_atom_names() -> Vec<&'static str> {
    let mut v = vec![
        ATOM_SCREENSAVER,
        ATOM_VERSION,
        ATOM_ID,
        ATOM_STATUS,
        ATOM_RESPONSE,
    ];
    for c in [
        Command::Activate,
        Command::Deactivate,
        Command::Cycle,
        Command::Next,
        Command::Prev,
        Command::Select(0),
        Command::Exit,
        Command::Restart,
        Command::Lock,
        Command::Suspend,
        Command::Demo,
        Command::Prefs,
    ] {
        v.push(c.atom_name());
    }
    v.extend_from_slice(STATUS_ATOMS);
    v.extend_from_slice(EXTRA_ATOMS);
    v
}

/// Bidirectional name <-> atom cache, filled in one round trip at connect time.
#[derive(Debug, Clone, Default)]
pub struct Atoms {
    by_name: HashMap<&'static str, Atom>,
    by_atom: HashMap<Atom, &'static str>,
}

impl Atoms {
    fn intern(conn: &XCBConnection) -> Result<Atoms, X11Error> {
        let mut names = all_atom_names();
        names.sort_unstable();
        names.dedup();
        // Send all requests first, then collect replies: a single round trip.
        let cookies = names
            .iter()
            .map(|n| conn.intern_atom(false, n.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut atoms = Atoms::default();
        for (name, cookie) in names.into_iter().zip(cookies) {
            let atom = cookie.reply()?.atom;
            atoms.by_name.insert(name, atom);
            atoms.by_atom.insert(atom, name);
        }
        Ok(atoms)
    }

    /// Atom for an interned name. Every protocol name (ATOM_*, command and status names) is present.
    pub fn by_name(&self, name: &str) -> Option<Atom> {
        self.by_name.get(name).copied()
    }

    /// Like `by_name` but for names known to be interned; returns `x11rb::NONE` (0) if missing.
    pub fn get(&self, name: &str) -> Atom {
        self.by_name(name).unwrap_or(x11rb::NONE)
    }

    /// Name of an interned atom (only atoms interned by us are known).
    pub fn name(&self, atom: Atom) -> Option<&'static str> {
        self.by_atom.get(&atom).copied()
    }
}

/// Lets the `compat::protocol` helpers use the daemon's atom cache directly.
impl crate::compat::protocol::AtomTable for Atoms {
    fn atom(&self, name: &str) -> Option<Atom> {
        self.by_name(name)
    }
    fn name(&self, atom: Atom) -> Option<&str> {
        self.by_atom.get(&atom).copied()
    }
}

/// An open X display plus the bits of setup everyone needs.
pub struct X11 {
    pub conn: XCBConnection,
    pub screen_num: usize,
    pub root: Window,
    /// Root window depth.
    pub depth: u8,
    /// Root visual.
    pub visual: Visualid,
    pub atoms: Atoms,
    pub has_screensaver_ext: bool,
    pub has_xinput2: bool,
    pub has_randr: bool,
    pub has_dpms: bool,
    pub has_shm: bool,
    pub has_xfixes: bool,
}

impl std::fmt::Debug for X11 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11")
            .field("screen_num", &self.screen_num)
            .field("root", &self.root)
            .field("depth", &self.depth)
            .field("visual", &self.visual)
            .finish_non_exhaustive()
    }
}

fn has_ext(conn: &XCBConnection, name: &'static str) -> Result<bool, X11Error> {
    Ok(conn.extension_information(name)?.is_some())
}

impl X11 {
    /// Connect to `display` (or `$DISPLAY` when `None`), verify extensions, intern atoms.
    pub fn open(display: Option<&str>) -> Result<X11, X11Error> {
        let cname = display
            .map(|d| {
                CString::new(d).map_err(|_| X11Error::Connect(format!("bad display name {d:?}")))
            })
            .transpose()?;
        let (conn, screen_num) = XCBConnection::connect(cname.as_deref())?;

        // Prefetch all extension info so the checks below cost one round trip.
        for ext in [
            screensaver::X11_EXTENSION_NAME,
            xinput::X11_EXTENSION_NAME,
            randr::X11_EXTENSION_NAME,
            dpms::X11_EXTENSION_NAME,
            shm::X11_EXTENSION_NAME,
            xfixes::X11_EXTENSION_NAME,
        ] {
            conn.prefetch_extension_information(ext)?;
        }
        let has_screensaver_ext = has_ext(&conn, screensaver::X11_EXTENSION_NAME)?;
        let mut has_randr = has_ext(&conn, randr::X11_EXTENSION_NAME)?;
        if has_randr {
            // RANDR expects QueryVersion before other requests; GetMonitors needs 1.5.
            let r = randr::query_version(&conn, 1, 5)?.reply()?;
            has_randr = r.major_version >= 1;
        }
        let has_dpms = has_ext(&conn, dpms::X11_EXTENSION_NAME)?;
        let has_shm = has_ext(&conn, shm::X11_EXTENSION_NAME)?;
        let mut has_xinput2 = false;
        if has_ext(&conn, xinput::X11_EXTENSION_NAME)? {
            let r = xinput::xi_query_version(&conn, 2, 2)?.reply()?;
            has_xinput2 = r.major_version >= 2;
        }
        let mut has_xfixes = false;
        if has_ext(&conn, xfixes::X11_EXTENSION_NAME)? {
            // XFIXES requires version negotiation before any other request.
            has_xfixes = xfixes::query_version(&conn, 4, 0)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some();
        }

        if !has_screensaver_ext {
            return Err(X11Error::MissingExtension("MIT-SCREEN-SAVER"));
        }
        if !has_xinput2 {
            return Err(X11Error::MissingExtension("XInputExtension 2.0"));
        }
        if !has_randr {
            return Err(X11Error::MissingExtension("RANDR"));
        }
        if !has_dpms {
            log::warn!("X server lacks DPMS; display power management disabled");
        }

        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| X11Error::Connect(format!("screen {screen_num} not found")))?;
        let root = screen.root;
        let depth = screen.root_depth;
        let visual = screen.root_visual;

        let atoms = Atoms::intern(&conn)?;

        Ok(X11 {
            conn,
            screen_num,
            root,
            depth,
            visual,
            atoms,
            has_screensaver_ext,
            has_xinput2,
            has_randr,
            has_dpms,
            has_shm,
            has_xfixes,
        })
    }

    /// The default screen.
    pub fn screen(&self) -> &Screen {
        &self.conn.setup().roots[self.screen_num]
    }

    /// Largest request the server accepts, in bytes (BIG-REQUESTS aware).
    pub fn max_request_bytes(&self) -> usize {
        self.conn.maximum_request_bytes()
    }

    /// Flush pending requests; errors are ignored (a dead connection surfaces on the next read).
    pub fn flush(&self) {
        let _ = self.conn.flush();
    }

    /// Round trip to the server: every request sent so far has been processed when this returns
    /// (errors for them are then queued as `Event::Error`).
    pub fn sync(&self) -> Result<(), X11Error> {
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    /// Non-blocking: next queued event (flushes first).
    pub fn poll_event(&self) -> Result<Option<Event>, X11Error> {
        self.conn.flush()?;
        Ok(self.conn.poll_for_event()?)
    }

    /// Wait up to `timeout` for an event. Returns `Ok(None)` on timeout.
    pub fn wait_event_timeout(&self, timeout: Duration) -> Result<Option<Event>, X11Error> {
        let deadline = Instant::now() + timeout;
        loop {
            // An event may already be buffered inside libxcb; check before touching the fd.
            if let Some(ev) = self.poll_event()? {
                return Ok(Some(ev));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let remaining = deadline - now;
            // Round up so we never spin with a 0 ms timeout while time remains.
            let ms = remaining
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd: self.conn.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `pfd` is a valid, initialised pollfd that lives for the whole call and we
            // pass nfds = 1 matching the single element; the fd is owned by `self.conn`.
            let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(X11Error::Protocol(format!(
                    "poll on X connection failed: {err}"
                )));
            }
            if pfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(X11Error::Protocol("X connection fd error".into()));
            }
            // Readable (or HUP) or timed out: loop; poll_for_event reads the socket and
            // reports a broken connection as an error.
        }
    }
}
