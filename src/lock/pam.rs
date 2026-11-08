//! PAM authentication through libpam-sys (OpenPAM-correct FFI). Owner: Opus E.
//!
//! `libpam-sys` 0.2 detects this target as `pam_impl = "OpenPam"`; its struct layouts and
//! constant values were checked against `/usr/include/security/pam_types.h` and
//! `pam_constants.h`, so no hand-rolled FFI is needed.
//!
//! Nothing here depends on running as root: with the default `xscreensaver` service the
//! password is verified by the setuid `unix-selfauth-helper` through `pam_exec.so`.
//! The password is never logged.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::{Mutex, PoisonError};

use libpam_sys as sys;
use zeroize::{Zeroize, Zeroizing};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PamError {
    /// Wrong password / account problem; `String` is pam_strerror text.
    Denied(String),
    /// pam_start or infrastructure failure.
    System(String),
}

impl std::fmt::Display for PamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PamError::Denied(s) => write!(f, "authentication denied: {s}"),
            PamError::System(s) => write!(f, "PAM system error: {s}"),
        }
    }
}
impl std::error::Error for PamError {}

/// Separator between the `pam_strerror` text and a captured PAM conversation message inside
/// `PamError::Denied` (see `PamError::user_message`).
pub const PAM_MESSAGE_SEP: &str = "; PAM: ";

/// Longest PAM conversation message (in characters) kept for display.
const MAX_PAM_MESSAGE_CHARS: usize = 120;

impl PamError {
    /// The last PAM `TEXT_INFO` / `ERROR_MSG` text captured during the conversation (for
    /// example "Password expired"), if any. Safe to show on screen: messages containing the
    /// password are never captured, control characters are stripped and length is bounded.
    pub fn user_message(&self) -> Option<&str> {
        match self {
            PamError::Denied(s) => s.split_once(PAM_MESSAGE_SEP).map(|(_, m)| m).filter(|m| !m.is_empty()),
            PamError::System(_) => None,
        }
    }
}

/// Clean a PAM informational message for display, or `None` if it must not be shown.
/// Rejects anything that contains the password (a misbehaving module echoing it back).
fn sanitize_message(raw: &[u8], password: &[u8]) -> Option<String> {
    if !password.is_empty() && raw.windows(password.len()).any(|w| w == password) {
        return None;
    }
    let text = String::from_utf8_lossy(raw);
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_PAM_MESSAGE_CHARS)
        .collect();
    let clean = clean.trim();
    (!clean.is_empty()).then(|| clean.to_string())
}

/// Data handed to the conversation callback. Both strings are NUL-terminated.
/// Test-only count of PROMPT_ECHO_OFF messages answered (proves the password reached PAM).
#[cfg(test)]
static ECHO_OFF_ANSWERED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct ConvData {
    user: CString,
    /// Password bytes followed by a single NUL; zeroized on drop.
    password: Zeroizing<Vec<u8>>,
    /// Last displayable PAM `TEXT_INFO` / `ERROR_MSG` text (see `sanitize_message`). A Mutex
    /// because the conversation only gets a shared `&ConvData`.
    last_message: Mutex<Option<String>>,
}

impl ConvData {
    fn new(user: CString, password: Zeroizing<Vec<u8>>) -> ConvData {
        ConvData { user, password, last_message: Mutex::new(None) }
    }

    /// Password bytes without the trailing NUL.
    fn password_bytes(&self) -> &[u8] {
        self.password.strip_suffix(&[0]).unwrap_or(&self.password)
    }
}

/// Zero a NUL-terminated C string in place and free it.
///
/// # Safety
/// `p` must be NULL or a `malloc`-family allocation holding a NUL-terminated string.
unsafe fn zero_and_free(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    // SAFETY: caller guarantees `p` is a valid NUL-terminated string.
    let len = unsafe { libc::strlen(p) };
    // SAFETY: `p` points to at least `len` writable bytes we own (malloc'd by strdup).
    unsafe { std::slice::from_raw_parts_mut(p.cast::<u8>(), len) }.zeroize();
    // SAFETY: `p` came from strdup (malloc) and is freed exactly once here.
    unsafe { libc::free(p.cast()) };
}

/// Free a partially built response array (first `filled` entries) and the array itself.
///
/// # Safety
/// `arr` must be a `calloc`ed array of at least `filled` `pam_response`s.
unsafe fn free_responses(arr: *mut sys::pam_response, filled: usize) {
    for i in 0..filled {
        // SAFETY: i < filled <= array length; the entry was initialised (calloc zeroes).
        let r = unsafe { &mut *arr.add(i) };
        // SAFETY: `resp` is NULL or a strdup'd string we allocated.
        unsafe { zero_and_free(r.resp) };
        r.resp = ptr::null_mut();
    }
    // SAFETY: `arr` was allocated with calloc and is freed exactly once.
    unsafe { libc::free(arr.cast()) };
}

/// OpenPAM conversation function. `msg` is an array of `num_msg` pointers to messages
/// (OpenPAM/Solaris layout). Must never panic: an unwind here would abort the locker.
unsafe extern "C" fn conversation(
    num_msg: c_int,
    msg: *const *const sys::pam_message,
    resp: *mut *mut sys::pam_response,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || num_msg > sys::PAM_MAX_NUM_MSG || msg.is_null() || resp.is_null() || appdata.is_null() {
        return sys::PAM_CONV_ERR;
    }
    let n = num_msg as usize;
    // SAFETY: appdata is the `&ConvData` we passed to pam_start; it outlives the transaction.
    let data = unsafe { &*(appdata as *const ConvData) };
    // SAFETY: plain libc allocation; result checked for NULL below.
    let arr = unsafe { libc::calloc(n, std::mem::size_of::<sys::pam_response>()) }
        .cast::<sys::pam_response>();
    if arr.is_null() {
        return sys::PAM_BUF_ERR;
    }
    for i in 0..n {
        // SAFETY: OpenPAM passes an array of `num_msg` message pointers; i < num_msg.
        let m = unsafe { *msg.add(i) };
        if m.is_null() {
            // SAFETY: arr holds n calloc'd entries, the first i of which we filled.
            unsafe { free_responses(arr, i) };
            return sys::PAM_CONV_ERR;
        }
        // SAFETY: m is a non-NULL pointer to a pam_message supplied by libpam.
        let style = unsafe { (*m).msg_style };
        let src: *const c_char = match style {
            sys::PAM_PROMPT_ECHO_OFF => {
                #[cfg(test)]
                ECHO_OFF_ANSWERED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                data.password.as_ptr().cast()
            }
            sys::PAM_PROMPT_ECHO_ON => data.user.as_ptr(),
            sys::PAM_ERROR_MSG | sys::PAM_TEXT_INFO => {
                // SAFETY: m is valid; msg is NULL or a NUL-terminated string owned by libpam.
                let text = unsafe { (*m).msg };
                if !text.is_null() {
                    // SAFETY: non-NULL NUL-terminated string from libpam.
                    let bytes = unsafe { CStr::from_ptr(text) }.to_bytes();
                    if let Some(t) = sanitize_message(bytes, data.password_bytes()) {
                        log::debug!("PAM message: {t}");
                        // Never panic across the FFI boundary: recover a poisoned lock.
                        *data.last_message.lock().unwrap_or_else(PoisonError::into_inner) = Some(t);
                    } else {
                        log::debug!("PAM message suppressed (empty or contains the password)");
                    }
                }
                ptr::null()
            }
            _ => {
                // SAFETY: as above.
                unsafe { free_responses(arr, i) };
                return sys::PAM_CONV_ERR;
            }
        };
        if !src.is_null() {
            // SAFETY: `src` points to a NUL-terminated buffer owned by `data`.
            let dup = unsafe { libc::strdup(src) };
            if dup.is_null() {
                // SAFETY: as above.
                unsafe { free_responses(arr, i) };
                return sys::PAM_BUF_ERR;
            }
            // SAFETY: i < n, arr has n zero-initialised entries.
            unsafe { (*arr.add(i)).resp = dup };
        }
    }
    // SAFETY: resp is a valid out-pointer supplied by libpam; libpam frees the array.
    unsafe { *resp = arr };
    sys::PAM_SUCCESS
}

fn strerror(h: *const sys::pam_handle, rc: c_int) -> String {
    // SAFETY: OpenPAM's pam_strerror ignores `h` (NULL is fine) and returns a pointer to a
    // static or per-handle string that stays valid for the duration of this call.
    let p = unsafe { sys::pam_strerror(h, rc) };
    if p.is_null() {
        format!("PAM error {rc}")
    } else {
        // SAFETY: non-NULL NUL-terminated string from libpam, copied immediately.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// Map a non-success PAM return code to a `PamError`. Both variants mean "not unlocked".
fn classify(rc: c_int, msg: String) -> PamError {
    match rc {
        sys::PAM_AUTH_ERR
        | sys::PAM_USER_UNKNOWN
        | sys::PAM_PERM_DENIED
        | sys::PAM_MAXTRIES
        | sys::PAM_ACCT_EXPIRED
        | sys::PAM_NEW_AUTHTOK_REQD
        | sys::PAM_AUTHTOK_EXPIRED
        | sys::PAM_CRED_INSUFFICIENT => PamError::Denied(msg),
        _ => PamError::System(format!("{msg} (code {rc})")),
    }
}

/// Authenticate `user` with `password` against PAM `service`
/// (pam_start -> pam_authenticate -> pam_acct_mgmt -> pam_end). Blocking.
///
/// A `pam_acct_mgmt` failure (expired account, `NEW_AUTHTOK_REQD`, ...) after a successful
/// `pam_authenticate` is a **denial** (review finding L3). This mirrors xscreensaver 6.17,
/// verified in `driver/passwd-pam.c` lines 165-187 and 220: the acct_mgmt status becomes the
/// result and only `PAM_SUCCESS` unlocks. One deliberate divergence: on `PAM_NEW_AUTHTOK_REQD`
/// xscreensaver calls `pam_chauthtok(PAM_CHANGE_EXPIRED_AUTHTOK)`; we do not, because our
/// conversation answers every echo-off prompt with the entered password, so a "new password"
/// prompt would be fed the old one. Instead the last PAM `TEXT_INFO`/`ERROR_MSG` text (e.g.
/// "Password expired") is carried in `PamError::Denied` (after `PAM_MESSAGE_SEP`) so the
/// dialog can tell the user why; see `PamError::user_message`.
pub fn authenticate(service: &str, user: &str, password: &Zeroizing<String>) -> Result<(), PamError> {
    let c_service =
        CString::new(service).map_err(|_| PamError::System("service name contains NUL".into()))?;
    let c_user = CString::new(user).map_err(|_| PamError::Denied("invalid user name".into()))?;
    if password.as_bytes().contains(&0) {
        return Err(PamError::Denied("invalid password".into()));
    }
    let mut pw = Zeroizing::new(Vec::with_capacity(password.len() + 1));
    pw.extend_from_slice(password.as_bytes());
    pw.push(0);
    let data = ConvData::new(c_user.clone(), pw);

    // `conv` and `data` live on this stack frame until after pam_end.
    let mut conv = sys::pam_conv {
        conv: conversation,
        appdata_ptr: (&data as *const ConvData).cast_mut().cast(),
    };
    let mut h: *mut sys::pam_handle = ptr::null_mut();
    // SAFETY: all pointers are valid NUL-terminated strings / live structs; `h` is an out-param.
    let rc = unsafe { sys::pam_start(c_service.as_ptr(), c_user.as_ptr(), &mut conv, &mut h) };
    if rc != sys::PAM_SUCCESS || h.is_null() {
        let msg = strerror(ptr::null(), rc);
        if !h.is_null() {
            // SAFETY: h is a handle returned by pam_start; ended exactly once.
            unsafe { sys::pam_end(h, rc) };
        }
        return Err(PamError::System(format!("pam_start({service}): {msg}")));
    }

    // SAFETY: h is a live handle from pam_start; conv/data are still alive.
    let mut rc = unsafe { sys::pam_authenticate(h, 0) };
    if rc == sys::PAM_SUCCESS {
        // SAFETY: as above.
        rc = unsafe { sys::pam_acct_mgmt(h, 0) };
    }
    let result = if rc == sys::PAM_SUCCESS {
        Ok(())
    } else {
        let mut msg = strerror(h, rc);
        let captured = data.last_message.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(m) = captured {
            msg.push_str(PAM_MESSAGE_SEP);
            msg.push_str(&m);
        }
        Err(classify(rc, msg))
    };
    // SAFETY: h is live and ended exactly once; no further use afterwards.
    unsafe { sys::pam_end(h, rc) };
    drop(data);
    result
}

/// Name of the user running this process (`getpwuid(getuid())`).
pub fn current_username() -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::getuid()).ok().flatten().map(|u| u.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_codes() {
        assert!(matches!(classify(sys::PAM_AUTH_ERR, "x".into()), PamError::Denied(_)));
        assert!(matches!(classify(sys::PAM_MAXTRIES, "x".into()), PamError::Denied(_)));
        assert!(matches!(classify(sys::PAM_SYSTEM_ERR, "x".into()), PamError::System(_)));
        assert!(matches!(classify(sys::PAM_CONV_ERR, "x".into()), PamError::System(_)));
    }

    fn msg(style: c_int, text: &CStr) -> sys::pam_message {
        sys::pam_message { msg_style: style, msg: text.as_ptr() }
    }

    fn data() -> ConvData {
        ConvData::new(CString::new("alice").unwrap(), Zeroizing::new(b"pw\0".to_vec()))
    }

    #[test]
    fn conversation_answers_prompts() {
        let d = data();
        let m = [
            msg(sys::PAM_PROMPT_ECHO_OFF, c"Password:"),
            msg(sys::PAM_PROMPT_ECHO_ON, c"login:"),
            msg(sys::PAM_TEXT_INFO, c"hello"),
            msg(sys::PAM_ERROR_MSG, c"oops"),
        ];
        let ptrs: Vec<*const sys::pam_message> = m.iter().map(|x| x as *const _).collect();
        let mut resp: *mut sys::pam_response = ptr::null_mut();
        let appdata = (&d as *const ConvData).cast_mut().cast();
        // SAFETY: valid message array of 4 pointers, valid out-pointer and appdata.
        let rc = unsafe { conversation(4, ptrs.as_ptr(), &mut resp, appdata) };
        assert_eq!(rc, sys::PAM_SUCCESS);
        assert!(!resp.is_null());
        // SAFETY: on success `resp` is a calloc'd array of 4 responses.
        let r = unsafe { std::slice::from_raw_parts(resp, 4) };
        // SAFETY: non-NULL entries are strdup'd NUL-terminated strings.
        assert_eq!(unsafe { CStr::from_ptr(r[0].resp) }, c"pw");
        // SAFETY: as above.
        assert_eq!(unsafe { CStr::from_ptr(r[1].resp) }, c"alice");
        assert!(r[2].resp.is_null() && r[3].resp.is_null());
        // The last informational message is captured for display.
        assert_eq!(d.last_message.lock().unwrap().as_deref(), Some("oops"));
        // SAFETY: array of 4 entries we own now.
        unsafe { free_responses(resp, 4) };
    }

    #[test]
    fn conversation_rejects_bad_args() {
        let d = data();
        let appdata: *mut c_void = (&d as *const ConvData).cast_mut().cast();
        let good = msg(sys::PAM_PROMPT_ECHO_OFF, c"Password:");
        let bad = msg(99, c"?");
        let cases: Vec<(c_int, Vec<*const sys::pam_message>, bool)> = vec![
            (0, vec![&good], false),
            (-1, vec![&good], false),
            (sys::PAM_MAX_NUM_MSG + 1, vec![&good], false),
            (1, vec![], true), // null msg array
            (2, vec![&good, ptr::null()], false),
            (2, vec![&good, &bad], false),
        ];
        for (n, ptrs, null_msg) in cases {
            let mut resp: *mut sys::pam_response = ptr::null_mut();
            let mp = if null_msg { ptr::null() } else { ptrs.as_ptr() };
            // SAFETY: when non-NULL, `mp` has at least `n` entries for every case that reaches
            // the loop (n <= ptrs.len()); invalid counts are rejected before any deref.
            let rc = unsafe { conversation(n, mp, &mut resp, appdata) };
            assert_eq!(rc, sys::PAM_CONV_ERR, "case n={n}");
            assert!(resp.is_null(), "resp touched on error (n={n})");
        }
        let mut resp: *mut sys::pam_response = ptr::null_mut();
        let ptrs = [&good as *const sys::pam_message];
        // SAFETY: valid message array; NULL appdata must be rejected.
        let rc = unsafe { conversation(1, ptrs.as_ptr(), &mut resp, ptr::null_mut()) };
        assert_eq!(rc, sys::PAM_CONV_ERR);
    }

    #[test]
    fn message_capture_filters_password_and_controls() {
        let d = data(); // password "pw"
        let leak = msg(sys::PAM_TEXT_INFO, c"your pw is wrong");
        let fine = msg(sys::PAM_ERROR_MSG, c"Password\texpired\x1b[31m");
        for (m, want) in [(&fine, Some("Password expired [31m")), (&leak, Some("Password expired [31m"))] {
            let ptrs = [m as *const sys::pam_message];
            let mut resp: *mut sys::pam_response = ptr::null_mut();
            // SAFETY: valid single-message array, out-pointer and appdata.
            let rc = unsafe { conversation(1, ptrs.as_ptr(), &mut resp, (&d as *const ConvData).cast_mut().cast()) };
            assert_eq!(rc, sys::PAM_SUCCESS);
            // SAFETY: calloc'd array of 1 response we now own.
            unsafe { free_responses(resp, 1) };
            assert_eq!(d.last_message.lock().unwrap().as_deref(), want);
        }
        assert_eq!(sanitize_message(b"   ", b"pw"), None);
        assert_eq!(sanitize_message("x".repeat(500).as_bytes(), b"pw").unwrap().len(), MAX_PAM_MESSAGE_CHARS);
        assert_eq!(sanitize_message("é".repeat(500).as_bytes(), b"pw").unwrap().chars().count(), MAX_PAM_MESSAGE_CHARS);
    }

    #[test]
    fn user_message_only_from_captured_text() {
        assert_eq!(PamError::Denied("authentication error".into()).user_message(), None);
        let e = PamError::Denied(format!("authentication error{PAM_MESSAGE_SEP}Password expired"));
        assert_eq!(e.user_message(), Some("Password expired"));
        assert_eq!(PamError::System(format!("x{PAM_MESSAGE_SEP}y")).user_message(), None);
    }

    #[test]
    fn current_user_known() {
        assert!(current_username().is_some_and(|u| !u.is_empty()));
    }

    /// Live PAM round trip; only runs with `NIGHTSAVER_PAM_TEST=1` (talks to the real
    /// `xscreensaver` PAM policy and may be slow).
    #[test]
    fn wrong_password_is_denied() {
        if std::env::var("NIGHTSAVER_PAM_TEST").as_deref() != Ok("1") {
            eprintln!("skipping wrong_password_is_denied: set NIGHTSAVER_PAM_TEST=1 to run");
            return;
        }
        let user = current_username().expect("current user");
        let pw = Zeroizing::new("definitely-wrong-password-nightsaver-test".to_string());
        let before = ECHO_OFF_ANSWERED.load(std::sync::atomic::Ordering::SeqCst);
        let r = authenticate("xscreensaver", &user, &pw);
        let prompts = ECHO_OFF_ANSWERED.load(std::sync::atomic::Ordering::SeqCst) - before;
        eprintln!("live PAM result for {user}: {r:?}; password prompts answered: {prompts}");
        assert!(prompts >= 1, "PAM never asked for the password");
        assert!(matches!(r, Err(PamError::Denied(_))), "got {r:?}");
    }
}
