# nightsaver — agent guide

OLED-friendly Xorg screensaver and PAM screen locker written in Rust. Drop-in replacement for
the `xscreensaver` daemon and the `xscreensaver-command` control protocol. License: BSD-2-Clause.
Full design: `docs/PLAN.md`. Read it before touching code.

## Target platform

- MidnightBSD 4.x amd64 (FreeBSD-derived). Rust target `x86_64-unknown-freebsd`.
- Build with the **system toolchain from mports**: `/usr/local/bin/cargo`, rustc 1.96.x.
  Do not install rustup toolchains. `edition = "2024"`, `rust-version = "1.96"`.
- Xorg only (no Wayland). Required X extensions: MIT-SCREEN-SAVER, XInputExtension 2.x, XKEYBOARD,
  RANDR, DPMS. Optional: MIT-SHM, XFIXES, XINERAMA.
- Auth is **OpenPAM** (`/usr/include/security/pam_*.h`, `-lpam`), not Linux-PAM. Default PAM service
  name is `xscreensaver` so the existing `/usr/local/etc/pam.d/xscreensaver` policy
  (`pam_exec.so expose_authtok /usr/local/libexec/unix-selfauth-helper`) works unprivileged.

## Layout

```
src/bin/nightsaver.rs          daemon            src/bin/nightsaver-command.rs  control client
src/bin/nightsaver-auth.rs     PAM helper process (one per unlock attempt; see src/lock/helper.rs)
src/cli.rs                     xscreensaver-compatible flag parsing
src/config/                    merged Config: CLI > ~/.config/nightsaver/config.toml > ~/.xscreensaver > defaults
src/compat/protocol.rs         SCREENSAVER ClientMessage + _SCREENSAVER_* root/window properties
src/x11/                       x11rb XCBConnection, monitors, saver windows, idle, grabs, present, keyboard
src/render/                    Framebuffer (0x00RRGGBB), bitmap font, OLED post-pass
src/patterns/                  trait Pattern + blank/clock/starfield/matrix/midnight/cat/juggler (+ assets/)
src/lock/                      Locker state machine, helper-process auth, PAM FFI, unlock dialog
src/daemon.rs                  event loop: Idle -> Blanked -> Locked
etc/ man/ tests/ Makefile      packaging, man pages, Xvfb smoke test
```

## Rules

1. **Correctness, then evidence, then safety, then minimal change.** Never fabricate APIs, config
   keys, or test results. If something is unverified, say so.
2. **Stay inside the files you own.** Shared contracts live in the module `mod.rs` files and
   `src/config/mod.rs`, `src/patterns/mod.rs`, `src/render/mod.rs`, `src/compat/protocol.rs`,
   `src/lock/mod.rs`, `src/lock/pam.rs`, `src/x11/mod.rs`, `src/x11/keyboard.rs`. Changing a
   shared contract requires updating every user in the same change and saying so in your report.
3. **Lock-screen security invariants** (non-negotiable):
   - Never unmap, lower, or exit while `SaverState::Locked` except after a successful PAM result.
   - Never show the unlock dialog without a successful keyboard **and** pointer grab.
   - Password buffers are `zeroize::Zeroizing`; never log them or any key input.
   - PAM/system errors are treated as "denied", never as "unlocked". PAM runs only in the
     `nightsaver-auth` helper process; the daemon never calls `pam_*` itself, and only the
     helper's clean `exit(0)` unlocks (a signal, timeout or any other code is a denial).
   - `EXIT`/`RESTART`/`DEACTIVATE` commands and SIGTERM are ignored while locked **or while a lock
     is pending** (grab not yet acquired); a pending lock shows a visible "NOT LOCKED" notice.
   - Fail closed: an unknown user name, a PAM failure of any kind, or a hung authenticator all
     mean "denied", never "unlocked". Synthetic (`XSendEvent`) events other than ClientMessage
     are dropped. Config and log files are checked for ownership and mode before use.
4. **xscreensaver compatibility is defined by behaviour**, verified with the real
   `/usr/local/bin/xscreensaver-command` and `xprop -root`. When in doubt read the xscreensaver
   source (`driver/remote.c`, `driver/windows.c`, `driver/xscreensaver.c`, `driver/prefs.c`) rather
   than guessing.
5. **OLED first**: every frame passes through `render::oled::OledGuard`. Patterns must respect the
   lit-pixel budget hint, avoid static pixels, and draw against pure black (0x000000).
6. **Dependencies are fixed** in `Cargo.toml`. Do not add crates without asking; prefer std.
   No bindgen/libclang at build time.
7. **Unsafe** is allowed only in `src/lock/pam.rs`, for thin libc calls (e.g. `localtime_r`,
   `setrlimit`) and where x11rb/xkbcommon require it; every
   `unsafe` block gets a `// SAFETY:` comment.
8. `cargo build`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` must pass before
   you report done. Run the narrowest check first, then the full set.
9. Never run the daemon on `DISPLAY=:0` without being told; use `Xvfb :99` (`tests/xvfb_smoke.sh`).
   Never `kill -9` a running locker.
10. Commits: only when asked. Messages are imperative, one line summary, body explains why.
11. **Changes go through pull requests.** Never commit to or push `master` directly. For each
    piece of work create a branch from the current `origin/master` (short kebab-case name, e.g.
    `starfield-streaks`), commit there, push it, and open a PR against `master` on
    https://github.com/MidnightBSD/nightsaver with `gh pr create` (summary, why, how it was
    tested). The user merges. Rebasing or force-pushing `master` is never allowed.

## Testing cheatsheet

```
cargo test                                   # unit tests (parsers, patterns, oled, protocol encode/decode)
Xvfb :99 -screen 0 1280x800x24 &             # isolated X server for integration tests
DISPLAY=:99 target/debug/nightsaver --verbose --no-splash &
DISPLAY=:99 /usr/local/bin/xscreensaver-command -version    # must talk to OUR daemon
DISPLAY=:99 xprop -root _SCREENSAVER_STATUS
DISPLAY=:99 xdotool mousemove 500 500        # should unblank when not locked
```
