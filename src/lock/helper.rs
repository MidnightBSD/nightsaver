//! Out-of-process authentication: run `nightsaver-auth` per attempt instead of calling PAM
//! inside the daemon. A PAM module that crashes, aborts or hangs then only takes the helper
//! down, not the locker (audit finding F1/F5): the daemon kills a hung helper on timeout and
//! every outcome other than a clean exit 0 is a denial.
//!
//! The helper binary is verified for every attempt (`resolve_helper`): the requested path
//! must reproduce itself verbatim under `canonicalize` (no symlink in **any** component, so
//! the kernel cannot exec a file the check did not see, such as a root-owned `/bin/true`
//! through a planted link), it must be a plain file owned by us or root and not writable by
//! group or others (`trust_check`), and its `(dev, ino)` is pinned and re-checked after
//! spawn and before the result is read (`helper_unchanged`), so a rename swap between a
//! sample and the child's exec is caught at the next one and its result discarded. An
//! in-place rewrite of the file keeps the inode; that is what the ownership and mode rules
//! are for. The residual interleaving — a swap completing between the last sample and the
//! child's `execve` — is detected one sample later (result refused), but a swapped-in
//! process could read the request bytes; closing that fully would need exec-by-fd, which
//! std does not expose.
//!
//! Wire protocol (helper stdin): `service NUL user NUL password NUL`. The helper exits 0 on
//! success, 1 when PAM denied (first stdout line: reason), 2 on a system error, 3 on misuse.
//! The password never appears in argv or the environment.
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
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

/// Refuse to run a helper that is not a plain file owned by us (or root) and not writable
/// by group or others: the helper's clean `exit(0)` is the only thing that unlocks, so a
/// tampered or swapped helper would make the unlock dialog accept any password. A missing
/// or symlinked path is refused by [`resolve_helper`] before this runs.
fn trust_check(path: &std::path::Path) -> Result<(), PamError> {
    if let Some(why) = crate::config::untrusted_file(path) {
        return Err(PamError::System(format!(
            "{HELPER_NAME}: refusing to run {}: {why}",
            path.display()
        )));
    }
    Ok(())
}

/// The helper as resolved, trusted and pinned: the exact path plus the `(dev, ino)` of the
/// file, so a replacement between samples can be detected and its result discarded.
struct TrustedHelper {
    path: PathBuf,
    id: (u64, u64),
}

/// Refuse the path when resolving it differs from the path itself, i.e. when **any**
/// component is a symlink: at exec time the kernel would follow it to a file the trust
/// check never verified (a planted link to a root-owned `/bin/true` passes the ownership
/// and mode rules for the target, because the check follows links). A path that cannot be
/// resolved at all (missing) is a refusal, too: `metadata` below fails anyway.
fn check_no_symlinks(path: &std::path::Path) -> Result<(), String> {
    match std::fs::canonicalize(path) {
        Ok(resolved) if resolved.as_path() == path => Ok(()),
        _ => Err(format!(
            "{HELPER_NAME} at {} does not resolve to itself (a path component is a symlink)",
            path.display()
        )),
    }
}

/// Resolve the helper path and verify the exact file we are about to exec: no symlinks
/// anywhere on the path (`canonicalize` must reproduce it verbatim), the plain-file trust
/// rules, and the `(dev, ino)` of the file for the unchanged checks during the attempt.
fn resolve_helper() -> Result<TrustedHelper, PamError> {
    let path = helper_path();
    let md = std::fs::metadata(&path).map_err(|e| {
        PamError::System(format!("{HELPER_NAME} at {}: cannot stat it: {e}", path.display()))
    })?;
    check_no_symlinks(&path).map_err(PamError::System)?;
    trust_check(&path)?;
    Ok(TrustedHelper { path, id: (md.dev(), md.ino()) })
}

/// True while `path` still holds the exact `(dev, ino)` that was resolved and trusted.
/// Only a rename swap changes the inode; an in-place rewrite is impossible for another
/// user under the trust rules (the file must be owned by us or root and not group or
/// others writable), so the same-uid case, which the daemon cannot defend against at all,
/// is out of scope.
fn helper_unchanged(helper: &TrustedHelper) -> bool {
    match std::fs::metadata(&helper.path) {
        Ok(md) => (md.dev(), md.ino()) == helper.id,
        Err(_) => false,
    }
}

/// Kill a replaced helper (a no-op once it has exited), reap it and build its denial.
fn revoke_child(mut child: std::process::Child, path: &std::path::Path, why: &str) -> PamError {
    let _ = child.kill();
    let _ = child.wait();
    PamError::System(format!(
        "{HELPER_NAME} at {} was replaced while it ran ({why}); result discarded",
        path.display()
    ))
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
    let helper = resolve_helper()?;
    let mut child = Command::new(&helper.path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| PamError::System(format!("cannot run {}: {e}", helper.path.display())))?;

    // The child exec's asynchronously: sample the pin once here, before any password bytes
    // are placed on the pipe, and once after the child exits, before its exit code is read.
    // A swap between samples is refused at the next one (see the module comment); the
    // post-exit sample is decisive because anything after it cannot have affected the
    // already-reaped child.
    if !helper_unchanged(&helper) {
        return Err(revoke_child(child, &helper.path, "right after spawn"));
    }

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
    if !helper_unchanged(&helper) {
        // The exit status may come from a swapped-in binary: refuse it. Killing is a no-op
        // once the child has exited; the wait only clears the process entry.
        return Err(revoke_child(child, &helper.path, "before its result was read"));
    }
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

    /// Exclusive, private (0700) scratch directory for on-disk fixtures: created with the
    /// mode itself (not created-then-chmodded), after clearing any leftover of an earlier
    /// run, so no other local session can pre-plant files this test then writes into.
    fn private_dir(tag: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt as _;
        let dir = std::env::temp_dir().join(format!("ns-helper-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir
    }

    #[test]
    fn untrusted_helper_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_dir("perm");
        let p = dir.join(HELPER_NAME);
        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(trust_check(&p).is_ok(), "ours, 0600: must pass");
        // Group/others writable: a swap by another local session must not be exec'd.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(trust_check(&p).is_err(), "0o666 must be refused");
        // A missing file is not trust_check's call: resolve_helper stats first and refuses.
        assert!(trust_check(&dir.join("missing")).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlinked_helper_path_is_refused() {
        let dir = private_dir("symlink");
        let real = dir.join("real");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        let link = dir.join(HELPER_NAME);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // A link anywhere on the path must be refused: the kernel would exec whatever it
        // points to at exec time, a file the trust check did not verify.
        assert!(check_no_symlinks(&link).is_err());
        assert!(check_no_symlinks(&real).is_ok());
        // An unresolvable path is a refusal, too.
        assert!(check_no_symlinks(&dir.join("absent")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn helper_replacement_is_detected() {
        let dir = private_dir("swap");
        let p = dir.join(HELPER_NAME);
        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        check_no_symlinks(&p).unwrap();
        let md = std::fs::metadata(&p).unwrap();
        let pinned = TrustedHelper { path: p.clone(), id: (md.dev(), md.ino()) };
        assert!(helper_unchanged(&pinned));
        // Replacing via rename creates a different inode: the next sample must see it,
        // even though the path looks identical. That is what resolve_helper's pin is for.
        std::fs::write(dir.join("swap"), b"#!/bin/sh\n").unwrap();
        std::fs::rename(dir.join("swap"), &p).unwrap();
        assert!(!helper_unchanged(&pinned), "a rename swap must invalidate the pin");
        // A deleted helper is also "not unchanged": nothing to trust.
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!helper_unchanged(&pinned));
    }
}
