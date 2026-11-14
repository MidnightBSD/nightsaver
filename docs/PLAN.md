# nightsaver — Rust screensaver + locker for Xorg and Wayland, xscreensaver drop-in

This document is the original Xorg design, kept as written. Wayland support was added later;
see "Wayland" at the end for how it fits in.

## Context

A Rust screensaver daemon for MidnightBSD/Xorg that can replace xscreensaver: accepts xscreensaver's
CLI flags, speaks the `xscreensaver-command` control protocol, reads `~/.xscreensaver`, renders
OLED-friendly built-in patterns, and optionally locks the screen with PAM password unlock. When locking
is off, any key/mouse activity wakes the display.

Decisions:
- Name: **`nightsaver`** (daemon) and **`nightsaver-command`** (client); optional `xscreensaver` /
  `xscreensaver-command` symlinks via a separate install target. License **BSD-2-Clause**.
- Compat scope: daemon CLI flags, xscreensaver-command protocol, read `~/.xscreensaver`.
  **Not** in scope: running external xscreensaver hack binaries, settings GUI, systemd/D-Bus.
- Lock config: config file + `--lock` CLI + runtime `-lock` command. Default lock **off**.
- Patterns v1: Blank, Drifting clock, Starfield, Matrix rain. **Software framebuffer** rendering.

## Verified environment (do not re-derive)

- MidnightBSD 4.2 amd64, rust target `x86_64-unknown-freebsd`, **rustc/cargo 1.96.1** from mports
  (`/usr/local/bin`). crates.io reachable. `edition = "2024"`, `rust-version = "1.96"`.
- Xorg `:0` has MIT-SCREEN-SAVER, XInputExtension, XKEYBOARD, RANDR, XINERAMA, MIT-SHM, DPMS, XFIXES.
  Tools present: `xdotool`, `xprop`, `xev`, `Xvfb`, `xset`, `xinput`. No Xephyr.
- OpenPAM (`/usr/include/security/pam_appl.h`, link `-lpam`). `/usr/local/etc/pam.d/xscreensaver`
  exists and uses `pam_exec.so expose_authtok /usr/local/libexec/unix-selfauth-helper`
  (setuid root) → unprivileged PAM auth works. Default PAM service name: **`xscreensaver`**.
- xscreensaver 6.15 daemon+command are installed (`/usr/local/bin`), hacks are not. Use the real
  `xscreensaver-command` as the black-box compatibility test. Source tarball:
  `http://www.jwz.org/xscreensaver/xscreensaver-6.17.tar.gz` (download into the scratchpad dir, never into the repo).

### Crate research results (verified against docs.rs / local registry)
- **x11rb 0.14.0**, features: `allow-unsafe-code` (needed for `xcb_ffi::XCBConnection`, which
  xkbcommon needs), `image`, `screensaver`, `xinput`, `xkb`, `randr`, `dpms`, `shm`, `xfixes`.
  All protocol enums are newtype structs with consts (`if x == GrabStatus::SUCCESS`).
  - Idle: `screensaver::query_info(conn, root)` → `ms_since_user_input`. `select_input(.., Event::NOTIFY_MASK)`.
  - Activity: `xinput::xi_query_version(conn,2,0)`, `xi_select_events(conn, root, &[EventMask{deviceid: u16::from(Device::ALL_MASTER), mask: vec![XIEventMask::RAW_MOTION|RAW_KEY_PRESS|RAW_BUTTON_PRESS]}])`;
    events `Event::XinputRawMotion/RawKeyPress/RawButtonPress`.
  - Windows: `create_window(.., WindowClass::INPUT_OUTPUT, visual, &CreateWindowAux::new().override_redirect(1).background_pixel(0).event_mask(..).cursor(..))`.
    Grabs: `grab_keyboard(conn, false, win, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)`,
    `grab_pointer(..)`, retry loop until `GrabStatus::SUCCESS`.
  - Present: `x11rb::image::Image::allocate_native(w,h,depth,&setup)`, `.put(conn, win, gc, 0, 0)` auto-splits
    oversize PutImage. MIT-SHM (`shm::attach_fd` + `shm::put_image`) is a later optimization.
  - Monitors: `randr::get_monitors(conn, root, true)` → `MonitorInfo{x,y,width,height,primary}`;
    `randr::select_input(conn, root, NotifyMask::SCREEN_CHANGE|CRTC_CHANGE)` → `Event::RandrScreenChangeNotify`.
  - DPMS: `dpms::{force_level(DPMSMode::ON/OFF), enable, set_timeouts, get_timeouts, info}`.
- **xkbcommon 0.9.0** `default-features=false, features=["x11"]`: `xkb::x11::setup_xkb_extension(&conn, MIN_MAJOR_XKB_VERSION, MIN_MINOR_XKB_VERSION, SetupXkbExtensionFlags::NoFlags, &mut maj,&mut min,&mut ev,&mut err)`,
  `get_core_keyboard_device_id(&conn)`, `keymap_new_from_device(&ctx,&conn,dev,KeymapCompileFlags::NO_FLAGS)`,
  `state_new_from_device(&keymap,&conn,dev)`, `State::key_get_utf8(Keycode::from(ev.detail))`,
  `key_get_one_sym`, `update_mask(..)` from `Event::XkbStateNotify`. Takes `&XCBConnection` directly
  (both use `as-raw-xcb-connection ^1.0`).
- **PAM: `libpam-sys` 0.2.0** (nonstick; hand-written OpenPAM-correct FFI, auto-detects freebsd
  target as OpenPam, no bindgen). Do **not** use `pam-client`/`pam` (reference Linux-only constants
  `PAM_CONV_AGAIN`/`PAM_INCOMPLETE`, expected to fail on OpenPAM). Conv callback: `msg` is an array of
  pointers; `calloc` a `pam_response[n]`, `strdup` password for `PAM_PROMPT_ECHO_OFF`, username for
  `PAM_PROMPT_ECHO_ON`, NULL for info/error; libpam frees. Sequence: `pam_start(service, user, &conv, &h)`
  → `pam_authenticate(h,0)` → `pam_acct_mgmt(h,0)` → `pam_end(h, rc)`. Fallback if the crate misbehaves:
  hand-rolled `extern "C"` block copied from `/usr/include/security/pam_types.h`.
- Username: `nix = {version="0.31", features=["user"]}` → `User::from_uid(getuid())?.name`.
- Others: `clap 4.6` (derive), `toml 1.2` + `serde`, `zeroize`, `log` + tiny stderr/file logger, `libc`.

### xscreensaver facts extracted from installed binaries (verify against source before coding the protocol)
- Daemon flags accepted by `xscreensaver` 6.15: `-display/-dpy/-disp`, `-verbose/-vvv/-vvvv`, `-quiet`,
  `-no-splash/-nosplash/-splash`, `-log <file>`, `-version/-ver/-vers`, `-help`, `-debug`, `-sync/-synch/-synchronize/-synchronise`,
  plus `--` spellings.
- Atoms: `SCREENSAVER` (ClientMessage type on root), `_SCREENSAVER_VERSION`, `_SCREENSAVER_ID`, `_SCREENSAVER_STATUS`,
  `_SCREENSAVER_RESPONSE`; command atoms `ACTIVATE DEACTIVATE CYCLE NEXT PREV SELECT EXIT RESTART LOCK SUSPEND DEMO`
  and status atoms `BLANK LOCK AUTH`.
- `xscreensaver-command` flags: `-quiet -verbose -activate -deactivate -cycle -next -prev -select N -lock -suspend -exit -restart -time -watch -version` (+ `--` forms, `-display`).
- Protocol work step 0: read `driver/remote.c` (client side: how it finds the saver window via `_SCREENSAVER_VERSION`, exact
  `XSendEvent` mask, `_SCREENSAVER_RESPONSE` handling, `-time`/`-watch` reading `_SCREENSAVER_STATUS`),
  `driver/windows.c` + `driver/xscreensaver.c` (daemon side: property formats, ClientMessage handling),
  `driver/prefs.c` + `driver/XScreenSaver.ad.in` (`~/.xscreensaver` keys and time syntax). Encode findings as
  doc comments in `src/compat/protocol.rs`.

## Architecture

Single cargo package `nightsaver`, two binaries, one lib crate so logic is unit-testable. See `AGENTS.md`
for the file layout; shared contracts are already stubbed in the `mod.rs` files.

### Runtime behaviour (daemon)
1. Start: open display (`--display`), refuse to run if another saver owns `_SCREENSAVER_VERSION`
   (xscreensaver does the same). Publish `_SCREENSAVER_VERSION`/`_SCREENSAVER_ID` on a hidden window
   and keep `_SCREENSAVER_STATUS` on root (format from source). Load config, watch `~/.xscreensaver` +
   toml mtime every few seconds and reload.
2. Idle loop (state `Idle`): poll `screensaver::query_info` once per second; when
   `ms_since_user_input >= timeout` → `activate(lock=false)`. Also react to `ScreensaverNotify` and to
   `SCREENSAVER` ClientMessages (ACTIVATE, LOCK, EXIT, RESTART, CYCLE/NEXT/PREV/SELECT, SUSPEND, DEACTIVATE).
3. `activate`: create/map/raise saver windows on all monitors, hide cursor, grab keyboard+pointer (retry up to
   ~5 s; if grab fails and lock requested, keep retrying — never show the lock UI without a grab),
   set DPMS timeouts from config, start pattern (`mode`: one/random/blank), reset pixel-shift clock, set status
   `BLANK` (+time). Frame timer at `pattern.wants_fps()` (Blank = no timer).
4. Activity while `Blanked` and **not locked**: any `KeyPress`, `ButtonPress`, or `MotionNotify` whose cumulative
   distance exceeds `motion_threshold_px` (default 10) → `deactivate`. Also XI2 raw events as fallback.
5. `lock_timeout` after blanking (or immediate on `LOCK` command / `--lock` / `lock: True` with `lockTimeout: 0`)
   → state `Locked`: status `LOCK`. Swallow the first keypress that opens the dialog;
   Escape/timeout hides dialog; Return submits → `pam::authenticate` on a worker thread (status `AUTH`);
   success → `deactivate`; failure → show "Incorrect" ~2 s, exponential backoff capped at 10 s, clear buffer.
   Password buffer is `zeroize`d. While `Locked`: ignore `EXIT`, refuse SIGTERM (log it), re-raise windows on
   any `MapNotify`, re-grab if an ungrab is detected.
6. `deactivate`: ungrab, unmap windows, DPMS `force_level(ON)`, restore DPMS timeouts, status cleared, back to Idle.
7. `cycle` every `cycle` seconds (default 10 min) → next pattern (random or list order). OLED pass runs always.

### OLED-friendliness rules (render/oled.rs, config `[oled]`)
- `max_brightness` (default 0.35) luma cap applied to all patterns; Blank is true black.
- `shift_interval_s` (default 60): whole frame offset by a slow pseudo-random ±(0..16 px) walk.
- `rest_interval_s`/`rest_duration_s`: periodic full-black interlude (default every 10 min, 20 s).
- No static UI element may persist (clock drifts; dialog auto-hides).
- Pattern authors get a lit-pixel budget hint (target ≤ 5 %).

### Command client (`nightsaver-command`)
Mirror `xscreensaver-command` flags exactly; find saver window via `_SCREENSAVER_VERSION`; send ClientMessage to root;
wait for `_SCREENSAVER_RESPONSE` the same way xscreensaver does; `-time`/`-watch` via `_SCREENSAVER_STATUS` +
`PropertyNotify`; `-version` prints the daemon's version from the property. Exit codes like xscreensaver.

## Verification

1. `cargo build --release && cargo test && cargo clippy --all-targets -- -D warnings`.
2. Xvfb smoke (`tests/xvfb_smoke.sh`): daemon on `:99` with tiny timeout; `xprop -root _SCREENSAVER_STATUS`
   shows BLANK; `xdotool mousemove` unblanks (no lock); real `xscreensaver-command -display :99 -activate|-deactivate|-version|-time`
   succeed against our daemon; `-lock` then `-exit` refused while locked.
3. Live on `:0` (stop the real daemon first): patterns via `-select N`/`-next`, mouse wiggle wakes, `-lock` → wrong then
   right password. Multi-monitor coverage. DPMS state via `xset q`.
4. Config reload: edit `~/.xscreensaver` `timeout:` while running → takes effect without restart.
5. Security review: no path unmaps or exits while Locked; password zeroized; grabs verified before prompting.

## Wayland (added in 0.6.0)

One binary, two backends, chosen at run time (`backend = auto | x11 | wayland`; `auto` picks Wayland
when `WAYLAND_DISPLAY` is set). The daemon keeps all policy and reaches the display only through
`display::Backend`; `src/x11/backend.rs` and `src/wayland/mod.rs` implement it.

- **Which compositors.** Only `ext-session-lock-v1` gives a client a real lock, so locking works on
  wlroots compositors (sway, labwc). KWin has layer-shell and idle-notify but no session lock: the
  saver runs blank-only there, `-lock` is refused and status never reports LOCK. Mutter has neither
  and is refused at start with the missing protocols named.
- **XWayland is never trusted to lock.** An X grab does not keep input from native Wayland clients,
  so forcing `--backend x11` while `WAYLAND_DISPLAY` is set disables locking.
- **Blanking** uses `wlr-layer-shell` overlay surfaces with exclusive keyboard interactivity, one per
  output. **Locking** creates a session lock and a lock surface per output.
- **Lock invariants.** Locked is entered only on the compositor's `locked` event. A lock that was
  requested but not confirmed (another locker holds the session) is pending: shown as NOT LOCKED,
  retried, and it refuses EXIT/RESTART/DEACTIVATE/SUSPEND and SIGTERM like a pending X grab.
  `unlock_and_destroy` is called only in `Backend::release_exclusive`, reached only when not locked or
  after the helper's clean `exit(0)`. If the daemon dies, the compositor keeps the session locked.
- **Idle** comes from `ext-idle-notify-v1`, so the compositor's idle inhibitors (video players) are
  honoured. **Power** uses `wlr-output-power-management`, timed by the backend because there are no
  DPMS timeouts to install; without it the black-screen fallback applies.
- **Rendering.** `Framebuffer` pixels (0x00RRGGBB, little-endian) are exactly `wl_shm` XRGB8888.
  Buffers are anonymous shared memory filled with positioned file writes; nothing is mapped.
  Integer output scale only.
- **Control.** `xscreensaver-command` needs X properties, so the daemon also listens on
  `$XDG_RUNTIME_DIR/nightsaver-<display>.sock` (mode 0600, same-uid peers only, runtime directory
  must be ours and 0700). `nightsaver-command` uses it under Wayland or with `-socket`. Socket
  commands go through `Daemon::command`, the same function as X ones.
- **Crates.** `wayland-client` (pure Rust, no libwayland), `wayland-protocols`,
  `wayland-protocols-wlr`, the `wayland` feature of `xkbcommon`; `wayland-protocols-misc` is a
  dev-dependency for the test keyboard only.
- **Testing.** `tests/wayland_smoke.sh` runs a private headless sway with a virtual pointer and
  keyboard and a stand-in auth helper. Not covered: PAM and a real keyboard under Wayland, KWin,
  GNOME, real hardware, fractional scaling, multiple seats.
