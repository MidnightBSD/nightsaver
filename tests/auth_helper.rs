//! Runs the built `nightsaver-auth` helper end to end with inputs that must be refused.
//! Nothing here can succeed, so it is safe on any machine; a correct-password run is covered
//! by the live test (NIGHTSAVER_PAM_TEST=1) in src/lock/pam.rs.
use std::io::Write;
use std::process::{Command, Stdio};

fn run(stdin: &[u8], args: &[&str]) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nightsaver-auth"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helper");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn malformed_request_is_usage_error() {
    assert_eq!(run(b"", &[]).0, Some(3));
    assert_eq!(run(b"svc\0user\0", &[]).0, Some(3));
    assert_eq!(run(b"svc\0user\0pw\0x\0", &[]).0, Some(3));
}

#[test]
fn arguments_are_refused() {
    assert_eq!(run(b"svc\0user\0pw\0", &["-v"]).0, Some(3));
}

#[test]
fn bogus_service_and_user_never_succeed() {
    let (code, _) = run(b"nightsaver-no-such-service-zz\0no-such-user-zz\0password\0", &[]);
    assert!(matches!(code, Some(1) | Some(2)), "got {code:?}");
}

#[test]
fn wrong_password_for_current_user_is_denied() {
    let user = nightsaver::lock::pam::current_username().unwrap_or_else(|| "nobody".into());
    let req = format!("nightsaver\0{user}\0definitely-not-the-password-{}\0", std::process::id());
    let (code, _) = run(req.as_bytes(), &[]);
    assert_ne!(code, Some(0));
    assert!(matches!(code, Some(1) | Some(2)), "got {code:?}");
}
