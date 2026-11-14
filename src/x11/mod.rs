//! X11 layer built on x11rb's libxcb-backed `XCBConnection`. Owner: Opus A (keyboard.rs: Opus E).
pub mod backend;
pub mod conn;
pub mod grab;
pub mod idle;
pub mod keyboard;
pub mod monitors;
pub mod present;
pub mod saver_window;

#[derive(Debug)]
pub enum X11Error {
    Connect(String),
    Protocol(String),
    MissingExtension(&'static str),
    GrabFailed,
}

impl std::fmt::Display for X11Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            X11Error::Connect(s) => write!(f, "cannot connect to X display: {s}"),
            X11Error::Protocol(s) => write!(f, "X protocol error: {s}"),
            X11Error::MissingExtension(e) => write!(f, "X server lacks extension {e}"),
            X11Error::GrabFailed => write!(f, "could not grab keyboard/pointer"),
        }
    }
}
impl std::error::Error for X11Error {}

impl From<x11rb::errors::ConnectionError> for X11Error {
    fn from(e: x11rb::errors::ConnectionError) -> Self {
        X11Error::Protocol(e.to_string())
    }
}
impl From<x11rb::errors::ReplyError> for X11Error {
    fn from(e: x11rb::errors::ReplyError) -> Self {
        X11Error::Protocol(e.to_string())
    }
}
impl From<x11rb::errors::ReplyOrIdError> for X11Error {
    fn from(e: x11rb::errors::ReplyOrIdError) -> Self {
        X11Error::Protocol(e.to_string())
    }
}
impl From<x11rb::errors::ConnectError> for X11Error {
    fn from(e: x11rb::errors::ConnectError) -> Self {
        X11Error::Connect(e.to_string())
    }
}
