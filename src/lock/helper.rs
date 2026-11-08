//! Out-of-process authentication: run `nightsaver-auth` per attempt instead of calling PAM
//! inside the daemon. A PAM module that crashes, aborts or hangs then only takes the helper
//! down, not the locker (audit finding F1/F5): the daemon kills a hung helper on timeout and
//! every outcome other than a clean exit 0 is a denial.
//!
//! Wire protocol (helper stdin): `service NUL user NUL password NUL`. The helper exits 0 on
//! success, 1 when PAM denied (first stdout line: reason), 2 on a system error, 3 on misuse.
//! The password never appears in argv or the environment.
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use super::pam::PamError;

/// Name of the helper binary.
pub const HELPER_NAME: &str = "nightsaver-auth";
/// Directory the helper is installed in (set by build.rs from PREFIX / LOCALBASE).
pub const LIBEXEC_DIR: &str = env!("NIGHTSAVER_LIBEXEC");
/// Kill the helper after this long; shorter than `AUTH_TIMEOUT` so the locker sees the result.
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(25);
/// Longest stdout line we accept from the helper.
const MAX_MESSAGE: usize = 256;

pub const EXIT_OK: i32 = 0;
pub const EXIT_DENIED: i32 = 1;
pub const EXIT_SYSTEM: i32 = 2;
pub const EXIT_USAGE: i32 = 3;

/// Where the helper lives: next to the running executable if present (development builds in
/// `target/`), else the installed libexec directory.
pub fn helper_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(HELPER_NAME);
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(LIBEXEC_DIR).join(HELPER_NAME)
}

/// Encode the request the helper reads from stdin.
pub fn encode_request(service: &str, user: &str, password: &str) -> Zeroizing<Vec<u8>> {
    let mut v = Zeroizing::new(Vec::with_capacity(service.len() + user.len() + password.len() + 3));
    for part in [service, user, password] {
        v.extend_from_slice(part.as_bytes());
        v.push(0);
    }
    v
}

/// Decode a request (helper side). `None` if it is not exactly three NUL-terminated fields.
pub fn decode_request(data: &[u8]) -> Option<(String, String, Zeroizing<String>)> {
    if data.last() != Some(&0) {
        return None;
    }
    let mut parts = data[..data.len() - 1].split(|b| *b == 0);
    let service = std::str::from_utf8(parts.next()?).ok()?.to_owned();
    let user = std::str::from_utf8(parts.next()?).ok()?.to_owned();
    let password = Zeroizing::new(std::str::from_utf8(parts.next()?).ok()?.to_owned());
    if parts.next().is_some() {
        return None;
    }
    Some((service, user, password))
}

/// Map the helper's exit status and first stdout line to an authentication result.
/// Only a clean `exit(0)` unlocks; a signal, an unknown code or no status is a system error.
pub fn outcome(code: Option<i32>, stdout: &[u8]) -> Result<(), PamError> {
    let msg = first_line(stdout);
    match code {
        Some(EXIT_OK) => Ok(()),
        Some(EXIT_DENIED) => Err(PamError::Denied(if msg.is_empty() { "denied".into() } else { msg })),
        Some(c) => Err(PamError::System(format!("{HELPER_NAME} exited with status {c}: {msg}"))),
        None => Err(PamError::System(format!("{HELPER_NAME} killed by a signal"))),
    }
}

/// First line of helper output with control characters removed, truncated.
fn first_line(out: &[u8]) -> String {
    let line = out.split(|b| *b == b'\n').next().unwrap_or(&[]);
    String::from_utf8_lossy(line)
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_MESSAGE)
        .collect()
}

/// Authenticate by running the helper. Same contract as `pam::authenticate`.
pub fn authenticate(service: &str, user: &str, password: &Zeroizing<String>) -> Result<(), PamError> {
    if password.as_bytes().contains(&0) || service.contains('\0') || user.contains('\0') {
        return Err(PamError::Denied("invalid input".into()));
    }
    let path = helper_path();
    let mut child = Command::new(&path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| PamError::System(format!("cannot run {}: {e}", path.display())))?;

    // Write the request and close stdin so the helper sees EOF. A helper that exits early
    // (e.g. refused to start) makes the write fail; that is reported through its status.
    let request = encode_request(service, user, password);
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&request);
        let _ = stdin.flush();
    }
    drop(request);

    // Read stdout on a thread so a chatty helper cannot block; the main wait is bounded.
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = stdout.as_mut() {
            let _ = out.take(4096).read_to_end(&mut buf);
        }
        buf
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) => {
                if started.elapsed() >= HELPER_TIMEOUT {
                    log::error!("{HELPER_NAME} did not finish within {HELPER_TIMEOUT:?}: killing it");
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(PamError::System("authentication helper timed out".into()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => break Err(PamError::System(format!("waiting for {HELPER_NAME}: {e}"))),
        }
    };
    let out = reader.join().unwrap_or_default();
    let st = status?;
    outcome(st.code(), &out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let enc = encode_request("svc", "me", "p\u{e4}ss word");
        let (s, u, p) = decode_request(&enc).unwrap();
        assert_eq!((s.as_str(), u.as_str(), p.as_str()), ("svc", "me", "p\u{e4}ss word"));
        assert!(decode_request(b"svc\0me\0").is_none(), "missing field");
        assert!(decode_request(b"svc\0me\0pw\0extra\0").is_none(), "extra field");
        assert!(decode_request(b"svc\0me\0pw").is_none(), "unterminated");
        assert!(decode_request(b"").is_none());
    }

    #[test]
    fn only_exit_zero_unlocks() {
        assert!(outcome(Some(0), b"").is_ok());
        assert!(matches!(outcome(Some(1), b"wrong\n"), Err(PamError::Denied(m)) if m == "wrong"));
        assert!(matches!(outcome(Some(1), b""), Err(PamError::Denied(_))));
        assert!(matches!(outcome(Some(2), b"boom"), Err(PamError::System(_))));
        assert!(matches!(outcome(Some(3), b""), Err(PamError::System(_))));
        assert!(matches!(outcome(Some(255), b""), Err(PamError::System(_))));
        assert!(matches!(outcome(None, b""), Err(PamError::System(_))));
    }

    #[test]
    fn messages_are_sanitised() {
        // Control characters go (ESC, TAB); printable remainder and only the first line stay.
        let m = first_line(b"a\x1b[31mb\tc\nsecond line");
        assert_eq!(m, "a[31mbc");
        let long = vec![b'x'; 1000];
        assert_eq!(first_line(&long).len(), MAX_MESSAGE);
    }

    #[test]
    fn helper_path_ends_with_name() {
        assert!(helper_path().ends_with(HELPER_NAME));
        assert!(std::path::Path::new(LIBEXEC_DIR).is_absolute());
    }
}
