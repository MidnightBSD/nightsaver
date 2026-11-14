//! Screen locker state machine and unlock dialog. Owner: Opus E.
//!
//! The daemon feeds translated keys with `Locker::key` and calls `Locker::poll` whenever it
//! wakes (at the latest at `next_deadline()`). The only path to `LockState::Unlocked` is an
//! `Ok(())` received from the authentication worker thread *tagged with the current attempt's
//! generation*; late results from abandoned (timed-out) attempts are discarded, and every
//! error (including a dead worker or a timeout) is a denial. At most one authentication
//! runs at a time, process-wide. Authentication itself happens in the `nightsaver-auth`
//! helper process (`helper::authenticate`), so a crashing or hanging PAM module cannot take
//! the locker down; the helper is killed after `helper::HELPER_TIMEOUT`.
pub mod helper;
pub mod pam;
pub mod throttle;
pub mod ui;

use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use zeroize::{Zeroize, Zeroizing};

use crate::input::KeyInput;
use pam::PamError;

/// Maximum password length in bytes.
pub const MAX_PASSWORD_BYTES: usize = 512;
/// How long "Incorrect password" stays on screen.
pub const FAIL_MESSAGE: Duration = Duration::from_secs(2);
/// Upper bound of the retry backoff.
pub const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// Poll interval the daemon should use while PAM runs (no fd to wait on).
pub const CHECK_POLL: Duration = Duration::from_millis(50);
/// How long one authentication attempt may run before it is abandoned and treated as a
/// system failure (review finding L1: a hung PAM module must not lock the user out forever).
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
/// Fallback used if `now + passwd_timeout` would overflow (config clamps make this
/// unreachable in practice; never fall back to `now`, which would hide the dialog at once).
const DEADLINE_FALLBACK: Duration = Duration::from_secs(24 * 3600);

/// Status text shown when an attempt timed out.
pub const TIMEOUT_TEXT: &str = "Auth timed out";
/// Status text shown for PAM system errors (not a wrong password).
pub const SYSTEM_ERROR_TEXT: &str = "Authentication failed";

/// Pluggable authenticator: `(service, user, password)`. Defaults to `helper::authenticate`.
pub type AuthFn =
    Arc<dyn Fn(&str, &str, &Zeroizing<String>) -> Result<(), PamError> + Send + Sync>;

/// Worker result tagged with the attempt generation it belongs to.
type AuthResult = (u64, Result<(), PamError>);

#[derive(Debug)]
pub enum LockState {
    /// Locked, dialog hidden (pure pattern on screen).
    Hidden,
    /// Dialog visible, collecting password; hides at `deadline`.
    Prompting { deadline: Instant },
    /// Attempt `generation` is being authenticated (or, while an abandoned worker is still
    /// running, is queued behind it: see `Locker::is_auth_busy`). Times out at
    /// `started + AUTH_TIMEOUT`.
    Checking { generation: u64, started: Instant },
    /// Wrong password; show message until `until`, ignore input until `retry_after`.
    Failed { until: Instant, retry_after: Instant },
    Unlocked,
}

/// Clears the shared "worker running" flag when the worker closure ends: after it sent its
/// result, on unwind (panic), or when the closure is dropped because the spawn failed.
struct InFlightGuard(Arc<AtomicBool>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct Locker {
    pub state: LockState,
    pub user: String,
    pub service: String,
    pub passwd_timeout: Duration,
    pub buffer: Zeroizing<String>,
    pub failures: u32,
    auth: AuthFn,
    /// One long-lived result channel; stale results are recognised by their generation.
    results_tx: Sender<AuthResult>,
    results_rx: Receiver<AuthResult>,
    /// True while an authentication worker thread exists. At most one runs at a time: some
    /// PAM modules are not thread-safe, so an abandoned (timed-out) worker blocks new ones.
    in_flight: Arc<AtomicBool>,
    /// Generation of the most recent attempt; results with any other tag are discarded.
    generation: u64,
    /// Password of an attempt waiting for an abandoned worker to exit ("PAM busy").
    queued: Option<Zeroizing<String>>,
    /// What the Failed state shows instead of "Incorrect password" (PAM message, timeout...).
    fail_text: Option<String>,
    /// A key was pressed during the backoff: show "Please wait" until `retry_after`.
    backoff_notice: bool,
}

impl std::fmt::Debug for Locker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the buffer (or queued password) contents.
        f.debug_struct("Locker")
            .field("state", &self.state)
            .field("user", &self.user)
            .field("service", &self.service)
            .field("masked_len", &self.masked_len())
            .field("failures", &self.failures)
            .field("busy", &self.queued.is_some())
            .finish_non_exhaustive()
    }
}

/// Result of feeding one key to the locker.
#[derive(Debug, PartialEq, Eq)]
pub enum LockEvent {
    None,
    Redraw,
    Unlocked,
}

fn empty_buffer() -> Zeroizing<String> {
    // Pre-size so appends never reallocate (a reallocation would leave an unzeroized copy).
    Zeroizing::new(String::with_capacity(MAX_PASSWORD_BYTES))
}

/// Backoff after the `failures`-th consecutive failure: 1s, 2s, 4s, 8s, 10s, 10s, ...
pub fn backoff(failures: u32) -> Duration {
    let exp = failures.saturating_sub(1).min(16);
    (Duration::from_secs(1) * (1u32 << exp)).min(MAX_BACKOFF)
}

/// `now + d` without panicking on overflow (review finding L4). Falls back to a shorter but
/// still long duration, and only as a last resort to `now`.
fn after(now: Instant, d: Duration) -> Instant {
    now.checked_add(d)
        .or_else(|| now.checked_add(d.min(DEADLINE_FALLBACK)))
        .unwrap_or(now)
}

impl Locker {
    /// New locker in `LockState::Hidden`, authenticating through the `nightsaver-auth` helper.
    /// All such lockers share one process-wide "PAM worker running" flag, so a hung worker
    /// from a replaced Locker still blocks new PAM transactions.
    pub fn new(user: String, service: String, passwd_timeout: Duration) -> Locker {
        static PAM_IN_FLIGHT: OnceLock<Arc<AtomicBool>> = OnceLock::new();
        let mut l = Locker::with_auth(user, service, passwd_timeout, Arc::new(helper::authenticate));
        l.in_flight = Arc::clone(PAM_IN_FLIGHT.get_or_init(|| Arc::new(AtomicBool::new(false))));
        l
    }

    /// Like `new` with a custom authenticator (tests).
    pub fn with_auth(user: String, service: String, passwd_timeout: Duration, auth: AuthFn) -> Locker {
        let (results_tx, results_rx) = mpsc::channel();
        Locker {
            state: LockState::Hidden,
            user,
            service,
            passwd_timeout,
            buffer: empty_buffer(),
            failures: 0,
            auth,
            results_tx,
            results_rx,
            in_flight: Arc::new(AtomicBool::new(false)),
            generation: 0,
            queued: None,
            fail_text: None,
            backoff_notice: false,
        }
    }

    fn prompt(&mut self, now: Instant) {
        self.backoff_notice = false;
        self.state = LockState::Prompting { deadline: after(now, self.passwd_timeout) };
    }

    fn hide(&mut self) {
        self.buffer.zeroize();
        self.backoff_notice = false;
        self.state = LockState::Hidden;
    }

    /// Feed one translated key.
    pub fn key(&mut self, input: KeyInput, now: Instant) -> LockEvent {
        let mut input = input;
        let ev = self.key_inner(&mut input, now);
        if let KeyInput::Text(t) = &mut input {
            t.zeroize();
        }
        ev
    }

    fn key_inner(&mut self, input: &mut KeyInput, now: Instant) -> LockEvent {
        match self.state {
            LockState::Hidden => {
                // Any key reveals the dialog; the key itself is swallowed (review finding L5).
                // This matches xscreensaver 6.17, checked in its source: the daemon's KeyPress
                // handler only records activity (driver/xscreensaver.c 2027-2036), then forks
                // `xscreensaver-auth` with no key in its argv (2362-2369), and the dialog only
                // sees keys through XInput2 events it selects after it starts
                // (driver/dialog.c 2478-2488). Inferred from source, not observed live.
                self.buffer.zeroize();
                self.prompt(now);
                LockEvent::Redraw
            }
            LockState::Prompting { .. } => self.prompting_key(input, now),
            LockState::Failed { retry_after, .. } if now >= retry_after => {
                self.buffer.zeroize();
                self.prompt(now);
                self.prompting_key(input, now);
                // The status line changes from "Incorrect" to the prompt either way.
                LockEvent::Redraw
            }
            LockState::Failed { .. } => {
                // Still backing off: the key is dropped, but tell the user to wait.
                if self.backoff_notice {
                    LockEvent::None
                } else {
                    self.backoff_notice = true;
                    LockEvent::Redraw
                }
            }
            LockState::Checking { .. } | LockState::Unlocked => LockEvent::None,
        }
    }

    fn prompting_key(&mut self, input: &mut KeyInput, now: Instant) -> LockEvent {
        // Every keystroke keeps the dialog open.
        self.prompt(now);
        match input {
            KeyInput::Text(t) => {
                if self.buffer.len() + t.len() <= MAX_PASSWORD_BYTES {
                    self.buffer.push_str(t);
                    LockEvent::Redraw
                } else {
                    LockEvent::None
                }
            }
            KeyInput::Backspace => {
                if let Some((idx, _)) = self.buffer.char_indices().next_back() {
                    // Rebuild instead of `pop` so the removed bytes are zeroized with the old
                    // allocation rather than left in spare capacity.
                    let mut nb = empty_buffer();
                    nb.push_str(&self.buffer[..idx]);
                    self.buffer = nb;
                    LockEvent::Redraw
                } else {
                    LockEvent::None
                }
            }
            KeyInput::Kill => {
                if self.buffer.is_empty() {
                    LockEvent::None
                } else {
                    self.buffer.zeroize();
                    LockEvent::Redraw
                }
            }
            KeyInput::Escape => {
                self.hide();
                LockEvent::Redraw
            }
            KeyInput::Enter => {
                if self.buffer.is_empty() {
                    return LockEvent::None;
                }
                self.submit(now);
                LockEvent::Redraw
            }
            KeyInput::Other => LockEvent::None,
        }
    }

    fn submit(&mut self, now: Instant) {
        let password = Zeroizing::new(String::from(self.buffer.as_str()));
        self.buffer.zeroize();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.state = LockState::Checking { generation, started: now };
        if self.in_flight.load(Ordering::Acquire) {
            // An abandoned worker is still inside PAM; never run two transactions at once.
            log::warn!("previous authentication attempt still running; new attempt waits for it");
            self.queued = Some(password);
        } else {
            self.spawn(generation, password);
        }
    }

    fn spawn(&mut self, generation: u64, password: Zeroizing<String>) {
        self.in_flight.store(true, Ordering::Release);
        let guard = InFlightGuard(Arc::clone(&self.in_flight));
        let tx = self.results_tx.clone();
        let auth = Arc::clone(&self.auth);
        let (service, user) = (self.service.clone(), self.user.clone());
        let spawned = std::thread::Builder::new().name("nightsaver-auth".into()).spawn(move || {
            // Dropped last (after the send), or during unwind if `auth` panics.
            let _guard = guard;
            let r = auth(&service, &user, &password);
            drop(password);
            let _ = tx.send((generation, r));
        });
        if let Err(e) = spawned {
            // The closure (password copy and guard) was dropped, so the in-flight flag is
            // clear and no result will come: `poll` reports a system failure.
            log::error!("cannot spawn authentication thread: {e}");
        }
    }

    fn fail(&mut self, err: &PamError, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        match err {
            PamError::Denied(m) => log::warn!("unlock attempt for {} denied: {m}", self.user),
            PamError::System(m) => log::warn!("unlock attempt for {} failed (PAM system error, treated as denied): {m}", self.user),
        }
        self.fail_text = match err {
            PamError::Denied(_) => err.user_message().map(str::to_string),
            PamError::System(_) => Some(SYSTEM_ERROR_TEXT.to_string()),
        };
        self.buffer.zeroize();
        self.backoff_notice = false;
        self.state = LockState::Failed {
            until: after(now, FAIL_MESSAGE),
            retry_after: after(now, backoff(self.failures)),
        };
    }

    /// Drain the result channel; returns the result for `generation`, discarding stale ones
    /// (late answers from abandoned attempts can never unlock).
    fn take_result(&mut self, generation: u64) -> Option<Result<(), PamError>> {
        let mut found = None;
        while let Ok((g, r)) = self.results_rx.try_recv() {
            if g == generation && found.is_none() {
                found = Some(r);
            } else {
                log::debug!("discarding result of abandoned authentication attempt {g}");
            }
        }
        found
    }

    fn poll_checking(&mut self, generation: u64, started: Instant, now: Instant) -> LockEvent {
        // Load before draining: the worker sends before clearing the flag (Release), so a
        // `false` here guarantees its result (if any) is already in the channel.
        let running = self.in_flight.load(Ordering::Acquire);
        if let Some(r) = self.take_result(generation) {
            return match r {
                Ok(()) => {
                    self.buffer.zeroize();
                    self.failures = 0;
                    self.fail_text = None;
                    self.state = LockState::Unlocked;
                    LockEvent::Unlocked
                }
                Err(e) => {
                    self.fail(&e, now);
                    LockEvent::Redraw
                }
            };
        }
        if !running {
            if let Some(pw) = self.queued.take() {
                // The abandoned worker has exited: start the queued attempt now, with its
                // own full AUTH_TIMEOUT.
                log::info!("previous authentication attempt finished; starting queued attempt");
                self.state = LockState::Checking { generation, started: now };
                self.spawn(generation, pw);
                return LockEvent::Redraw;
            }
            self.fail(&PamError::System("authentication thread exited without a result".into()), now);
            return LockEvent::Redraw;
        }
        if now >= after(started, AUTH_TIMEOUT) {
            // Abandon the attempt (the worker keeps running; its late result is discarded by
            // generation) and drop any queued password.
            self.queued = None;
            self.fail(&PamError::System("authentication timed out".into()), now);
            self.fail_text = Some(TIMEOUT_TEXT.to_string());
            return LockEvent::Redraw;
        }
        LockEvent::None
    }

    /// Advance timers and collect the PAM result. Call whenever the daemon wakes.
    pub fn poll(&mut self, now: Instant) -> LockEvent {
        match self.state {
            LockState::Prompting { deadline } if now >= deadline => {
                self.hide();
                LockEvent::Redraw
            }
            LockState::Checking { generation, started } => self.poll_checking(generation, started, now),
            LockState::Failed { until, retry_after } if now >= until && now >= retry_after => {
                // Message shown and backoff over: back to the prompt, unless the dialog has
                // already been open (since the failure) for longer than passwd_timeout.
                let failed_at = until.checked_sub(FAIL_MESSAGE).unwrap_or(until);
                let deadline = after(failed_at, self.passwd_timeout);
                self.backoff_notice = false;
                if now >= deadline {
                    self.hide();
                } else {
                    self.state = LockState::Prompting { deadline };
                }
                LockEvent::Redraw
            }
            LockState::Failed { retry_after, .. } if self.backoff_notice && now >= retry_after => {
                // Backoff over: drop the "Please wait" notice.
                self.backoff_notice = false;
                LockEvent::Redraw
            }
            _ => LockEvent::None,
        }
    }

    /// Is the dialog on screen (Prompting, Checking or Failed)?
    pub fn is_prompt_visible(&self) -> bool {
        matches!(
            self.state,
            LockState::Prompting { .. } | LockState::Checking { .. } | LockState::Failed { .. }
        )
    }

    /// PAM is running (daemon may publish `AUTH` status).
    pub fn is_checking(&self) -> bool {
        matches!(self.state, LockState::Checking { .. })
    }

    /// Checking, but the attempt waits for an abandoned (timed-out) PAM worker to exit.
    pub fn is_auth_busy(&self) -> bool {
        self.is_checking() && self.queued.is_some()
    }

    /// Text the Failed state shows instead of "Incorrect password": a PAM message such as
    /// "Password expired", or a timeout / system-error notice. Never contains the password.
    pub fn failure_text(&self) -> Option<&str> {
        self.fail_text.as_deref()
    }

    /// A key was pressed while backing off (dialog shows "Please wait").
    pub fn backoff_notice(&self) -> bool {
        self.backoff_notice && matches!(self.state, LockState::Failed { .. })
    }

    pub fn is_unlocked(&self) -> bool {
        matches!(self.state, LockState::Unlocked)
    }

    /// Soonest instant the daemon must wake and call `poll`. While checking this is a short
    /// poll interval from now, since the worker result has no fd to wait on.
    pub fn next_deadline(&self) -> Option<Instant> {
        match &self.state {
            LockState::Prompting { deadline } => Some(*deadline),
            LockState::Checking { .. } => Some(after(Instant::now(), CHECK_POLL)),
            LockState::Failed { until, retry_after } => {
                let now = Instant::now();
                // Wake for whichever pending instant comes first (retry_after enables input,
                // which changes nothing on screen, but until/both expiring does).
                [*until, *retry_after].into_iter().filter(|t| *t > now).min().or(Some(now))
            }
            LockState::Hidden | LockState::Unlocked => None,
        }
    }

    /// Number of characters entered (for the mask row). Never reveals content.
    pub fn masked_len(&self) -> usize {
        self.buffer.chars().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PW: &str = "s3cret";

    fn mock(calls: Arc<AtomicUsize>) -> AuthFn {
        Arc::new(move |svc: &str, user: &str, pw: &Zeroizing<String>| {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(svc, "svc");
            assert_eq!(user, "alice");
            match pw.as_str() {
                PW => Ok(()),
                "boom" => Err(PamError::System("module failure".into())),
                "panic" => panic!("auth thread dies"),
                _ => Err(PamError::Denied("Authentication error".into())),
            }
        })
    }

    fn locker() -> (Locker, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let l = Locker::with_auth("alice".into(), "svc".into(), Duration::from_secs(30), mock(calls.clone()));
        (l, calls)
    }

    fn type_str(l: &mut Locker, s: &str, now: Instant) {
        for c in s.chars() {
            l.key(KeyInput::Text(c.to_string()), now);
        }
    }

    /// Poll until the worker answers (bounded).
    fn wait_result(l: &mut Locker, now: Instant) -> LockEvent {
        for _ in 0..400 {
            let ev = l.poll(now);
            if ev != LockEvent::None {
                return ev;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("auth thread never answered");
    }

    #[test]
    fn first_key_reveals_and_is_swallowed() {
        let (mut l, _) = locker();
        let t = Instant::now();
        assert!(!l.is_prompt_visible());
        assert_eq!(l.next_deadline(), None);
        assert_eq!(l.key(KeyInput::Text("x".into()), t), LockEvent::Redraw);
        assert!(l.is_prompt_visible());
        assert_eq!(l.masked_len(), 0);
        assert_eq!(l.next_deadline(), Some(t + Duration::from_secs(30)));
    }

    #[test]
    fn editing_keys() {
        let (mut l, _) = locker();
        let t = Instant::now();
        l.key(KeyInput::Other, t);
        type_str(&mut l, "abé", t);
        assert_eq!(l.masked_len(), 3);
        assert_eq!(l.key(KeyInput::Backspace, t), LockEvent::Redraw);
        assert_eq!(l.buffer.as_str(), "ab");
        assert!(l.buffer.capacity() >= MAX_PASSWORD_BYTES);
        assert_eq!(l.key(KeyInput::Kill, t), LockEvent::Redraw);
        assert_eq!(l.masked_len(), 0);
        assert_eq!(l.key(KeyInput::Backspace, t), LockEvent::None);
        assert_eq!(l.key(KeyInput::Enter, t), LockEvent::None, "empty enter ignored");
        assert!(matches!(l.state, LockState::Prompting { .. }));
        type_str(&mut l, "zz", t);
        assert_eq!(l.key(KeyInput::Escape, t), LockEvent::Redraw);
        assert!(matches!(l.state, LockState::Hidden));
        assert_eq!(l.masked_len(), 0);
    }

    #[test]
    fn length_cap() {
        let (mut l, _) = locker();
        let t = Instant::now();
        l.key(KeyInput::Other, t);
        type_str(&mut l, &"a".repeat(600), t);
        assert_eq!(l.buffer.len(), MAX_PASSWORD_BYTES);
        assert!(l.buffer.capacity() >= MAX_PASSWORD_BYTES);
    }

    #[test]
    fn prompt_times_out() {
        let (mut l, _) = locker();
        let t = Instant::now();
        l.key(KeyInput::Other, t);
        type_str(&mut l, "ab", t + Duration::from_secs(10));
        // Typing extended the deadline.
        assert_eq!(l.poll(t + Duration::from_secs(31)), LockEvent::None);
        assert_eq!(l.poll(t + Duration::from_secs(40)), LockEvent::Redraw);
        assert!(matches!(l.state, LockState::Hidden));
        assert_eq!(l.masked_len(), 0);
    }

    #[test]
    fn correct_password_unlocks() {
        let (mut l, calls) = locker();
        let t = Instant::now();
        l.key(KeyInput::Other, t);
        type_str(&mut l, PW, t);
        assert_eq!(l.key(KeyInput::Enter, t), LockEvent::Redraw);
        assert!(l.is_checking());
        assert!(l.is_prompt_visible());
        assert_eq!(l.masked_len(), 0, "buffer zeroized on submit");
        assert!(l.next_deadline().is_some());
        // Keys are ignored while checking.
        assert_eq!(l.key(KeyInput::Text("q".into()), t), LockEvent::None);
        assert_eq!(wait_result(&mut l, t), LockEvent::Unlocked);
        assert!(l.is_unlocked());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn wrong_password_backoff_and_retry() {
        let (mut l, _) = locker();
        let t0 = Instant::now();
        l.key(KeyInput::Other, t0);
        type_str(&mut l, "nope", t0);
        l.key(KeyInput::Enter, t0);
        assert_eq!(wait_result(&mut l, t0), LockEvent::Redraw);
        assert_eq!(l.failures, 1);
        let LockState::Failed { until, retry_after } = l.state else { panic!("{:?}", l.state) };
        assert_eq!(until, t0 + FAIL_MESSAGE);
        assert_eq!(retry_after, t0 + Duration::from_secs(1));
        assert!(l.is_prompt_visible());
        // Input ignored before retry_after, but the first such key shows "Please wait".
        assert!(!l.backoff_notice());
        assert_eq!(l.key(KeyInput::Text("a".into()), t0 + Duration::from_millis(500)), LockEvent::Redraw);
        assert!(l.backoff_notice());
        assert_eq!(l.key(KeyInput::Text("b".into()), t0 + Duration::from_millis(600)), LockEvent::None);
        assert_eq!(l.masked_len(), 0);
        assert!(matches!(l.state, LockState::Failed { .. }));
        // The notice is dropped once the backoff is over.
        assert_eq!(l.poll(t0 + Duration::from_secs(1)), LockEvent::Redraw);
        assert!(!l.backoff_notice());
        assert!(matches!(l.state, LockState::Failed { .. }));
        // After retry_after, the key is processed as a prompt key (not swallowed).
        let t1 = t0 + Duration::from_millis(1500);
        assert_eq!(l.key(KeyInput::Text("a".into()), t1), LockEvent::Redraw);
        assert!(matches!(l.state, LockState::Prompting { .. }));
        assert_eq!(l.masked_len(), 1);
    }

    #[test]
    fn backoff_gates_even_after_message() {
        let (mut l, _) = locker();
        let t0 = Instant::now();
        l.failures = 3; // next failure is the 4th: 8 s backoff
        l.key(KeyInput::Other, t0);
        type_str(&mut l, "nope", t0);
        l.key(KeyInput::Enter, t0);
        wait_result(&mut l, t0);
        assert_eq!(l.failures, 4);
        let t3 = t0 + Duration::from_secs(3);
        assert_eq!(l.poll(t3), LockEvent::None, "still backing off after message");
        assert!(matches!(l.state, LockState::Failed { .. }));
        assert_eq!(l.key(KeyInput::Text("a".into()), t3), LockEvent::Redraw, "please-wait notice");
        assert_eq!(l.masked_len(), 0);
        assert_eq!(l.poll(t0 + Duration::from_secs(8)), LockEvent::Redraw);
        assert!(!l.backoff_notice());
        assert!(matches!(l.state, LockState::Prompting { .. }));
    }

    #[test]
    fn failed_expires_to_prompt_or_hidden() {
        let (mut l, _) = locker();
        let t0 = Instant::now();
        l.key(KeyInput::Other, t0);
        type_str(&mut l, "nope", t0);
        l.key(KeyInput::Enter, t0);
        wait_result(&mut l, t0);
        assert_eq!(l.poll(t0 + Duration::from_secs(1)), LockEvent::None);
        assert_eq!(l.poll(t0 + FAIL_MESSAGE), LockEvent::Redraw);
        assert!(matches!(l.state, LockState::Prompting { .. }));

        // A tiny passwd_timeout: Failed expiry goes straight to Hidden.
        let calls = Arc::new(AtomicUsize::new(0));
        let mut l = Locker::with_auth("alice".into(), "svc".into(), Duration::from_secs(1), mock(calls));
        l.key(KeyInput::Other, t0);
        type_str(&mut l, "nope", t0);
        l.key(KeyInput::Enter, t0);
        wait_result(&mut l, t0);
        assert_eq!(l.poll(t0 + FAIL_MESSAGE), LockEvent::Redraw);
        assert!(matches!(l.state, LockState::Hidden));
    }

    #[test]
    fn system_error_and_dead_thread_are_denials() {
        for pw in ["boom", "panic"] {
            let (mut l, _) = locker();
            let t = Instant::now();
            l.key(KeyInput::Other, t);
            type_str(&mut l, pw, t);
            l.key(KeyInput::Enter, t);
            assert_eq!(wait_result(&mut l, t), LockEvent::Redraw);
            assert!(matches!(l.state, LockState::Failed { .. }), "{pw}: {:?}", l.state);
            assert!(!l.is_unlocked());
        }
    }

    /// Authenticator whose "block" password waits for `release` and then answers `Ok` (a
    /// late success that must never unlock); other passwords behave like `mock`.
    fn blocking_mock(calls: Arc<AtomicUsize>) -> (AuthFn, mpsc::Sender<()>) {
        let (release, gate) = mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let inner = mock(Arc::new(AtomicUsize::new(0)));
        let f: AuthFn = Arc::new(move |svc: &str, user: &str, pw: &Zeroizing<String>| {
            calls.fetch_add(1, Ordering::SeqCst);
            if pw.as_str() == "block" {
                let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(20));
                return Ok(());
            }
            inner(svc, user, pw)
        });
        (f, release)
    }

    fn submit(l: &mut Locker, pw: &str, now: Instant) {
        l.key(KeyInput::Other, now);
        type_str(l, pw, now);
        assert_eq!(l.key(KeyInput::Enter, now), LockEvent::Redraw);
        assert!(l.is_checking(), "{:?}", l.state);
    }

    fn wait_idle(l: &Locker) {
        for _ in 0..400 {
            if !l.in_flight.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("worker never exited");
    }

    #[test]
    fn hung_auth_times_out_and_late_ok_is_discarded() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (auth, release) = blocking_mock(calls.clone());
        let mut l = Locker::with_auth("alice".into(), "svc".into(), Duration::from_secs(30), auth);
        let t0 = Instant::now();
        submit(&mut l, "block", t0);
        assert_eq!(l.poll(t0 + AUTH_TIMEOUT - Duration::from_millis(1)), LockEvent::None);
        assert!(l.is_checking());
        let t1 = t0 + AUTH_TIMEOUT;
        assert_eq!(l.poll(t1), LockEvent::Redraw);
        assert!(matches!(l.state, LockState::Failed { .. }), "{:?}", l.state);
        assert_eq!(l.failures, 1);
        assert_eq!(l.failure_text(), Some(TIMEOUT_TEXT));
        // The hung worker now answers Ok: it must be discarded.
        release.send(()).unwrap();
        wait_idle(&l);
        let t2 = t1 + Duration::from_secs(5);
        l.poll(t2);
        assert!(!l.is_unlocked());
        // A new (wrong) attempt drains the stale Ok and is still denied.
        submit(&mut l, "nope", t2);
        assert_eq!(wait_result(&mut l, t2), LockEvent::Redraw);
        assert!(matches!(l.state, LockState::Failed { .. }), "{:?}", l.state);
        assert!(!l.is_unlocked());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn one_worker_at_a_time_busy_then_queued_attempt_runs() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (auth, release) = blocking_mock(calls.clone());
        let mut l = Locker::with_auth("alice".into(), "svc".into(), Duration::from_secs(30), auth);
        let t0 = Instant::now();
        submit(&mut l, "block", t0);
        let t1 = t0 + AUTH_TIMEOUT;
        assert_eq!(l.poll(t1), LockEvent::Redraw);
        // After the backoff, submit again while the abandoned worker is still running.
        let t2 = t1 + Duration::from_secs(3);
        l.poll(t2);
        submit(&mut l, "nope", t2);
        assert!(l.is_auth_busy(), "second attempt must wait, not spawn");
        assert_eq!(l.masked_len(), 0, "buffer zeroized on submit");
        assert!(!format!("{l:?}").contains("nope"));
        for _ in 0..400 {
            if calls.load(Ordering::SeqCst) >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        for i in 0..10 {
            assert_eq!(l.poll(t2 + Duration::from_millis(i)), LockEvent::None);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no second PAM thread while one is in flight");
        assert_eq!(l.key(KeyInput::Text("q".into()), t2), LockEvent::None, "keys ignored while busy");
        // The stale worker finishes with Ok (stale generation): the queued attempt starts and
        // its own (wrong) result decides; the stale Ok never unlocks.
        release.send(()).unwrap();
        wait_idle(&l);
        let t3 = t2 + Duration::from_secs(1);
        assert_eq!(l.poll(t3), LockEvent::Redraw, "queued attempt started");
        assert!(l.is_checking() && !l.is_auth_busy());
        assert_eq!(wait_result(&mut l, t3), LockEvent::Redraw);
        assert!(matches!(l.state, LockState::Failed { .. }), "{:?}", l.state);
        assert!(!l.is_unlocked());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn busy_attempt_also_times_out() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (auth, release) = blocking_mock(calls.clone());
        let mut l = Locker::with_auth("alice".into(), "svc".into(), Duration::from_secs(30), auth);
        let t0 = Instant::now();
        submit(&mut l, "block", t0);
        let t1 = t0 + AUTH_TIMEOUT;
        l.poll(t1);
        let t2 = t1 + Duration::from_secs(3);
        l.poll(t2);
        submit(&mut l, PW, t2);
        assert!(l.is_auth_busy());
        assert_eq!(l.poll(t2 + AUTH_TIMEOUT), LockEvent::Redraw);
        assert!(matches!(l.state, LockState::Failed { .. }));
        assert!(l.queued.is_none(), "queued password dropped on timeout");
        release.send(()).unwrap();
        wait_idle(&l);
        l.poll(t2 + AUTH_TIMEOUT + Duration::from_secs(20));
        assert!(!l.is_unlocked());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pam_message_and_system_error_text() {
        let auth: AuthFn = Arc::new(|_: &str, _: &str, pw: &Zeroizing<String>| match pw.as_str() {
            "exp" => Err(PamError::Denied(format!("authentication error{}Password expired", pam::PAM_MESSAGE_SEP))),
            "sys" => Err(PamError::System("module failure".into())),
            _ => Err(PamError::Denied("authentication error".into())),
        });
        let mut l = Locker::with_auth("alice".into(), "svc".into(), Duration::from_secs(30), auth);
        let t = Instant::now();
        for (pw, want) in [("exp", Some("Password expired")), ("sys", Some(SYSTEM_ERROR_TEXT)), ("bad", None)] {
            l.failures = 0;
            l.state = LockState::Hidden;
            submit(&mut l, pw, t);
            wait_result(&mut l, t);
            assert_eq!(l.failure_text(), want, "{pw}");
        }
    }

    #[test]
    fn huge_passwd_timeout_does_not_panic() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut l = Locker::with_auth("alice".into(), "svc".into(), Duration::MAX, mock(calls));
        let t = Instant::now();
        assert_eq!(l.key(KeyInput::Other, t), LockEvent::Redraw);
        let LockState::Prompting { deadline } = l.state else { panic!("{:?}", l.state) };
        assert!(deadline > t, "overflow must not fall back to an immediate deadline");
        type_str(&mut l, "nope", t);
        l.key(KeyInput::Enter, t);
        wait_result(&mut l, t);
        l.poll(t + Duration::from_secs(60));
        assert!(matches!(l.state, LockState::Prompting { .. }));
    }

    #[test]
    fn backoff_schedule() {
        let s: Vec<u64> = (1..=7).map(|f| backoff(f).as_secs()).collect();
        assert_eq!(s, [1, 2, 4, 8, 10, 10, 10]);
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
    }

    #[test]
    fn debug_never_shows_password() {
        let (mut l, _) = locker();
        let t = Instant::now();
        l.key(KeyInput::Other, t);
        type_str(&mut l, "hunter2", t);
        assert!(!format!("{l:?}").contains("hunter2"));
    }
}
