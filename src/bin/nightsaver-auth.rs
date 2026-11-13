//! `nightsaver-auth`: PAM authentication helper run by the nightsaver daemon for every unlock
//! attempt. Reads `service NUL user NUL password NUL` from stdin (a pipe from the daemon),
//! runs the PAM conversation in this short-lived single-threaded process, and exits 0 only on
//! success. Not setuid; it authenticates the invoking user's own password through the
//! system's PAM policy (e.g. unix-selfauth-helper). See `nightsaver::lock::helper`.
//!
//! PAM attempts are throttled per invoking user (`nightsaver::lock::throttle`): the helper
//! is invocable by anyone, so the daemon-side dialog backoff is not the only protection PAM
//! gets from a hammering loop. The throttle is advisory and fails open; contended slots are
//! a refusal (a denial), never an unlock.
use std::io::{IsTerminal, Read, Write};

use nightsaver::lock::helper::{EXIT_DENIED, EXIT_OK, EXIT_SYSTEM, EXIT_USAGE, decode_request};
use nightsaver::lock::pam::{self, PamError};
use nightsaver::lock::throttle;
use zeroize::Zeroizing;

/// Upper bound on the request size (service + user + a 512-byte password, generously).
const MAX_REQUEST: u64 = 4096;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    if std::env::args_os().count() > 1 {
        eprintln!("usage: nightsaver-auth  (reads its request from stdin; run by nightsaver)");
        return EXIT_USAGE;
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprintln!("nightsaver-auth: stdin must be a pipe from the nightsaver daemon");
        return EXIT_USAGE;
    }
    disable_core_dumps();

    let mut raw = Zeroizing::new(Vec::new());
    if let Err(e) = stdin.lock().take(MAX_REQUEST).read_to_end(&mut raw) {
        eprintln!("nightsaver-auth: reading request: {e}");
        return EXIT_SYSTEM;
    }
    let Some((service, user, password)) = decode_request(&raw) else {
        eprintln!("nightsaver-auth: malformed request");
        return EXIT_USAGE;
    };
    drop(raw);

    let result = throttle::with_slot(|| pam::authenticate(&service, &user, &password));
    drop(password);
    let mut out = std::io::stdout();
    match result {
        Ok(()) => EXIT_OK,
        Err(PamError::Denied(m)) => {
            let _ = writeln!(out, "{}", printable(&m));
            EXIT_DENIED
        }
        Err(PamError::System(m)) => {
            let _ = writeln!(out, "{}", printable(&m));
            EXIT_SYSTEM
        }
    }
}

/// The password lives in this process for a moment; make sure it cannot land in a core file.
fn disable_core_dumps() {
    let lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `lim` is a valid, fully initialised rlimit; setrlimit only reads it.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &lim) };
    if rc != 0 {
        eprintln!("nightsaver-auth: cannot disable core dumps: {}", std::io::Error::last_os_error());
    }
}

fn printable(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}
