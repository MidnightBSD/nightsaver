//! Advisory per-user throttle for `nightsaver-auth` PAM attempts. Owner: Opus E.
//!
//! The unlock dialog backs off after failures, but the helper is a plain executable any
//! local process can run (`printf "xscreensaver\0me\0pw\0" | nightsaver-auth`), so nothing
//! else stops a loop from hammering PAM: every attempt forks the setuid
//! `unix-selfauth-helper` through `pam_exec`, whose crypt work is real CPU. The throttle
//! serializes one invoking user's attempts on a `flock` lock for their whole attempt
//! (including PAM, which also caps concurrency at one transaction per uid) and enforces a
//! minimum gap between attempt starts, recorded in the state file.
//!
//! This is deliberately *advisory*: the state is a `/tmp` file that the invoking user could
//! delete, rewrite or point at something else, and the invoking user could preload or patch
//! their own copy of the helper. Any invocation with the correct password still went
//! through a real PAM verify; a determined same-uid process is out of reach for every
//! non-setuid helper (it already must never trust its environment). What the lock buys is
//! protection from accidental loops and unpatched scripted abuse, and a bounded per-uid
//! PAM load rather than an unbounded one.
//!
//! Failure directions:
//! - Every problem opening, locking or reading the state file is **fail-open** (the attempt
//!   runs unthrottled): the throttle may never stand between the user and unlocking, and a
//!   broken state file must not turn into a lockout.
//! - The state file is verified on the opened handle (`config::untrusted_metadata`): it
//!   must be a regular file owned by us or root and not group/others-writable; anything
//!   else bypasses the throttle and logs a warning. This is what keeps a planted `/tmp`
//!   file (a 0666 seed another uid holds `flock` on, a FIFO, a device) from delaying or
//!   refusing someone else's unlock attempt - without the check, contention would be
//!   forgeable cross-uid. It must also have exactly one link: where the kernel lets a user
//!   hard-link files they do not own (`security.bsd.hardlink_check_uid=0`, the default),
//!   another uid could otherwise make the state name an alias of any file of ours on the
//!   `/tmp` filesystem, which passes the ownership rules, gets our timestamp written over
//!   its first bytes, and forges contention if one of our processes holds a lock on it.
//! - Warnings go through `log`; the helper installs a stderr logger and the daemon copies
//!   the helper's stderr into its own log, so a bypassed throttle is visible.
//! - Only *contention* — another attempt holding the slot longer than `MAX_WAIT` — is a
//!   denial, and with the check above the holder can only be a process running as the same
//!   user (or root, which can do anything anyway).
//!
//! The state file is `/tmp/.nightsaver-auth-<uid>.lock` (0600, `O_NOFOLLOW`, `O_NONBLOCK`):
//! a hard-coded directory, because `TMPDIR` is caller-controlled. Kernel locks are released
//! even when the helper is killed mid-PAM, so a hung attempt cannot wedge the slot.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::pam::PamError;

/// Minimum gap between the starts of two PAM attempts of one user. Invisible for the lock
/// dialog (its own backoff after a failure is at least one second) and tight enough to cap
/// a looping caller's sustained rate at a handful of attempts per second.
pub const MIN_GAP: Duration = Duration::from_millis(250);
/// How long a process may wait for a same-uid attempt in flight to finish before the
/// attempt is refused as throttled.
pub const MAX_WAIT: Duration = Duration::from_secs(1);
/// Poll step while waiting for the slot.
const POLL: Duration = Duration::from_millis(25);
/// `/tmp` is the only directory every unprivileged user can create a state file in.
const STATE_DIR: &str = "/tmp";

/// Refusal because another attempt of the same user holds the slot too long.
#[derive(Debug, PartialEq, Eq)]
pub struct Contended;

impl std::fmt::Display for Contended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "throttled: another authentication for this user is in flight")
    }
}
impl std::error::Error for Contended {}

/// Default per-uid state file. `attempt` takes an explicit path; this is the shape the
/// helper uses in production (hard-coded `/tmp` so `TMPDIR` cannot move it).
fn lock_path() -> PathBuf {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    Path::new(STATE_DIR).join(format!(".nightsaver-auth-{uid}.lock"))
}

/// Milliseconds since the epoch (0 when the clock is before 1970, e.g. after a reset).
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run one PAM attempt through the default per-uid throttle.
///
/// The helper calls this around `pam::authenticate`; `Ok` and error cases are passed
/// through unchanged. Refusals come back as `Err(PamError::System(_))` with the
/// [`Contended`] text, which every PAM system error already maps to a denial.
pub fn with_slot<T>(
    f: impl FnOnce() -> Result<T, PamError>,
) -> Result<T, PamError> {
    attempt(&lock_path(), MAX_WAIT, f)
}

/// The throttle mechanics for one attempt against the state file at `path`.
fn attempt<T>(
    path: &Path,
    wait: Duration,
    f: impl FnOnce() -> Result<T, PamError>,
) -> Result<T, PamError> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            // Fail open: a broken or planted path may cost the throttle, never the unlock.
            log::warn!("throttle: cannot open {}: {e}; running unthrottled", path.display());
            return f();
        }
    };
    // The name is predictable and /tmp is world-writable, so anyone can pre-create or
    // replace the state file (a 0666 seed they then hold `flock` on, a FIFO, a device).
    // Verify the opened handle like a config file and fail open on anything that is not a
    // plain file owned by us (or root): a planted file must cost the throttle, never the
    // unlock, and must never get to hold up - let alone refuse - an unlock attempt.
    let md = match file.metadata() {
        Ok(md) => md,
        Err(e) => {
            log::warn!("throttle: cannot stat {}: {e}; running unthrottled", path.display());
            return f();
        }
    };
    if let Some(why) = crate::config::untrusted_metadata(&md) {
        log::warn!(
            "throttle: {} not trusted ({why}); running unthrottled",
            path.display()
        );
        return f();
    }
    // A second name for the file means the state path may be a hard link someone else
    // planted to one of our own files (see the module comment): never lock or write it.
    if md.nlink() != 1 {
        log::warn!(
            "throttle: {} has {} links, expected 1; running unthrottled",
            path.display(),
            md.nlink()
        );
        return f();
    }

    // Acquire the slot; the lock rides on `file` and is released when it is dropped, i.e.
    // after `f` returns, and at process exit even when the helper is killed mid-PAM.
    let deadline = Instant::now().checked_add(wait).unwrap_or_else(Instant::now);
    loop {
        // SAFETY: advisory flock on the fd we just opened; only EAGAIN is expected.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            log::warn!("throttle: cannot lock {}: {err}; running unthrottled", path.display());
            return f();
        }
        if Instant::now() >= deadline {
            return Err(PamError::System(format!("{Contended} (waited {wait:?})")));
        }
        std::thread::sleep(POLL);
    }

    // Enforce the gap against the recorded last attempt start. A state file we cannot
    // read or parse (never created, tampered with) counts as "long ago": run unimpeded.
    let mut last = String::new();
    let _ = (&file).take(32).read_to_string(&mut last);
    let elapsed = Duration::from_millis(now_millis().saturating_sub(last.trim().parse().unwrap_or(0)));
    if elapsed < MIN_GAP {
        std::thread::sleep(MIN_GAP - elapsed);
    }
    // Record this attempt's start; a failed write must not block the attempt. Truncate
    // first: a longer leftover (tampered or garbage content) would otherwise keep its tail
    // behind the new timestamp, never parse again, and switch the gap off for good.
    let _ = file.set_len(0);
    let _ = file.write_all_at(format!("{}\n", now_millis()).as_bytes(), 0);

    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Exclusive, private (0700) scratch directory for fixture state files: created with
    /// the mode itself (not created-then-chmodded), after clearing any leftover of an
    /// earlier run, so no other local session can pre-plant the fixture.
    fn private_dir(tag: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt as _;
        let dir = std::env::temp_dir().join(format!("ns-throttle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir
    }

    /// Write a fixture state file with mode 0600 whatever the umask is: a group-writable
    /// file (umask 002) is untrusted and would bypass the throttle instead of exercising it.
    fn seed(path: &Path, content: &[u8]) -> File {
        use std::io::Write as _;
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        f.write_all(content).unwrap();
        f
    }

    #[test]
    fn lock_path_is_per_uid_in_tmp() {
        let p = lock_path().to_string_lossy().into_owned();
        assert!(p.starts_with("/tmp/.nightsaver-auth-"), "{p}");
        assert!(p.ends_with(".lock"), "{p}");
        // Different uids must not share a slot: the name carries the uid.
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        assert_eq!(p, format!("/tmp/.nightsaver-auth-{uid}.lock"));
    }

    #[test]
    fn attempts_keep_the_minimum_gap_and_record_their_start() {
        let dir = private_dir("gap");
        let path = dir.join("state.lock");
        let started = Instant::now();
        attempt(&path, MAX_WAIT, || Ok(())).unwrap();
        assert_eq!(attempt(&path, MAX_WAIT, || Ok(())), Ok(()));
        // The first attempt set the timestamp; the second one waited out MIN_GAP.
        assert!(started.elapsed() >= MIN_GAP, "gap not enforced");
        // The state file now records a fresh attempt time.
        let recorded = std::fs::read_to_string(&path).unwrap().trim().parse::<u64>().unwrap();
        assert!(now_millis().saturating_sub(recorded) < 5_000, "stale record {recorded}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_taken_slot_refuses_the_attempt() {
        let dir = private_dir("contended");
        let path = dir.join("state.lock");
        // Hold the slot the way another in-flight attempt would.
        let held = seed(&path, b"");
        // SAFETY: advisory flock on the fixture fd we just created.
        let rc = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) };
        assert_eq!(rc, 0, "fixture must hold the lock");
        let mut ok = false;
        let r = attempt(&path, Duration::from_millis(60), || {
            ok = true;
            Ok(())
        });
        assert!(!ok, "refused attempt must not run");
        assert!(matches!(&r, Err(PamError::System(m)) if m.contains("throttled")), "{r:?}");
        drop(held);
        // Once released, the same attempt goes through.
        assert_eq!(attempt(&path, MAX_WAIT, || Ok(())), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A broken state path (here: a symlink, which `O_NOFOLLOW` refuses to open) must let
    /// the attempt run unthrottled.
    #[test]
    fn broken_state_file_fails_open() {
        let dir = private_dir("failopen");
        let path = dir.join("state.lock");
        let target = dir.join("target");
        std::fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let ran2 = Arc::clone(&ran);
        assert!(attempt(&path, MAX_WAIT, move || {
            ran2.store(true, Ordering::SeqCst);
            Ok(())
        })
        .is_ok());
        assert!(ran.load(Ordering::SeqCst), "fail-open must run the attempt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A foreign-planted state file (0666, e.g. one an attacker holds `flock` on to forge
    /// contention) must be bypassed: the attempt runs unthrottled and the planted file is
    /// left untouched. On this platform the helper demonstrably waited a second and refused
    /// before this check existed.
    #[test]
    fn planted_world_writable_state_file_fails_open() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_dir("planted");
        let path = dir.join("state.lock");
        std::fs::write(&path, b"planted\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let ran2 = Arc::clone(&ran);
        let r = attempt(&path, MAX_WAIT, move || {
            ran2.store(true, Ordering::SeqCst);
            Ok(())
        });
        assert!(r.is_ok() && ran.load(Ordering::SeqCst), "planted file must not gate the attempt: {r:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"planted\n", "planted file must not be written");
        // A 0600 file owned by us is the trusted case: the same attempt path records it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(attempt(&path, MAX_WAIT, || Ok(()))
            .and_then(|()| {
                std::fs::read_to_string(&path)
                    .map_err(|e| crate::lock::pam::PamError::System(e.to_string()))
            })
            .is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A planted FIFO must be refused (not regular) without ever blocking: `open` is
    /// nonblocking and the fstat check rejects it before any read, so a hostile
    /// /tmp/state-file cannot freeze the helper. Bounded like the config test.
    #[test]
    fn planted_fifo_fails_open_without_blocking() {
        use std::ffi::CString;
        use std::sync::mpsc;
        let dir = private_dir("fifo");
        let path = dir.join("state.lock");
        let c = CString::new(path.to_string_lossy().into_owned()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path inside our private fixture directory;
        // mkfifo only reads it.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo fixture");
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        let ran = Arc::new(AtomicBool::new(false));
        let ran2 = Arc::clone(&ran);
        std::thread::spawn(move || {
            let _ = tx.send(attempt(&p, MAX_WAIT, move || {
                ran2.store(true, Ordering::SeqCst);
                Ok(())
            }));
        });
        match rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or_else(|e| panic!("attempt blocked on a planted FIFO: {e}"))
        {
            Ok(()) => assert!(ran.load(Ordering::SeqCst), "fail-open must run the attempt"),
            other => panic!("planted FIFO must not gate the attempt: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A state file with unparseable content counts as "long ago": run unimpeded.
    #[test]
    fn unreadable_timestamp_fails_open() {
        let dir = private_dir("garbage");
        let path = dir.join("state.lock");
        seed(&path, b"not a timestamp");
        assert!(attempt(&path, MAX_WAIT, || Ok(())).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn permissions_kept_private() {
        let dir = private_dir("perm");
        let path = dir.join("state.lock");
        attempt(&path, MAX_WAIT, || Ok(())).unwrap();
        // SAFETY: getuid has no preconditions and cannot fail.
        let me = unsafe { libc::getuid() };
        let md = std::fs::metadata(&path).unwrap();
        assert_eq!(md.mode() & 0o777, 0o600);
        assert_eq!(MetadataExt::uid(&md), me);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A state path that is a hard link to another of our files (plantable by a different
    /// uid where the kernel does not restrict hard links) passes the ownership rules but
    /// must be bypassed: the linked file is neither written nor used as the lock.
    #[test]
    fn hard_linked_state_file_fails_open_and_is_not_written() {
        let dir = private_dir("hardlink");
        let victim = dir.join("victim-data");
        // Hold a lock on it, the way a pidfile owner would: must not count as contention.
        let held = seed(&victim, b"precious contents\n");
        // SAFETY: advisory flock on the fixture fd we just opened.
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) }, 0);
        let path = dir.join("state.lock");
        std::fs::hard_link(&victim, &path).unwrap();
        let mut ran = false;
        let r = attempt(&path, Duration::from_millis(60), || {
            ran = true;
            Ok(())
        });
        assert!(r.is_ok() && ran, "hard-linked state must not gate the attempt: {r:?}");
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious contents\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Content longer than a timestamp must not survive behind the new record: the next
    /// attempt has to parse it and keep the gap.
    #[test]
    fn long_garbage_is_truncated_and_the_gap_still_applies() {
        let dir = private_dir("tail");
        let path = dir.join("state.lock");
        seed(&path, b"garbage that is longer than any timestamp\n");
        attempt(&path, MAX_WAIT, || Ok(())).unwrap();
        let recorded = std::fs::read_to_string(&path).unwrap();
        assert!(recorded.trim().parse::<u64>().is_ok(), "stale tail left behind: {recorded:?}");
        let started = Instant::now();
        attempt(&path, MAX_WAIT, || Ok(())).unwrap();
        assert!(started.elapsed() >= MIN_GAP / 2, "gap not enforced after garbage");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
