# nightsaver

An OLED-friendly Xorg screensaver and PAM screen locker for MidnightBSD, written in Rust.
It is a drop-in replacement for the `xscreensaver` daemon and the `xscreensaver-command`
control client.

Status: **v0.6, Xorg only.** There is no Wayland support yet. In a Wayland session
(`WAYLAND_DISPLAY` set) nightsaver refuses to start rather than run on XWayland, where it could
blank the screen but not lock native Wayland clients; `--backend x11` forces it to run there with
locking disabled.

## Features

- Accepts xscreensaver's daemon flags (`-display`, `-dpy`, `-verbose`, `-vvv`, `-no-splash`, `-log`, ...).
- Speaks the xscreensaver control protocol: the real `xscreensaver-command` works against it, and
  `nightsaver-command` also works against a real xscreensaver daemon.
- Reads the relevant subset of `~/.xscreensaver`, plus its own TOML file.
- Four built-in software-rendered patterns (no external hack programs).
- Every frame goes through an OLED guard: brightness cap, slow pixel shift, periodic black rest.
- Optional screen lock with PAM password unlock, using the existing `xscreensaver` PAM service so
  no setuid code is needed in nightsaver itself.
- DPMS management, multi-monitor coverage, config reload on file change or `SIGHUP`.

Not in scope: running xscreensaver hack binaries, the settings GUI, systemd/D-Bus integration.

## Build

Requires the system toolchain from mports (`/usr/local/bin/cargo`, rustc 1.96 or newer; the crate uses
edition 2024) and the X libraries (libxcb, libxkbcommon) under `/usr/local`.

```
make                  # cargo build --release
make test             # cargo test
```

`build.rs` adds `$LOCALBASE/lib` (default `/usr/local`) to the linker search path, so no `RUSTFLAGS`
are needed. Override with `make LOCALBASE=/other/prefix`. Plain `cargo build --release` works too.

## Install

```
make install                    # binaries, man pages, sample config, desktop files
make install-pam                # PAM policy only (install already does this)
make install-compat-links       # optional, xscreensaver -> nightsaver symlinks
```

`PREFIX` (default `/usr/local`), `DESTDIR`, `MANDIR` and `CARGO` can be overridden. Both BSD make and GNU
make are supported. `make uninstall` removes what was installed (compat links only if they are symlinks).

Installed files:

| Path (under `$PREFIX`)                  | Contents                         |
|-----------------------------------------|----------------------------------|
| `bin/nightsaver`, `bin/nightsaver-command` | daemon and client             |
| `libexec/nightsaver-auth`               | PAM helper run per unlock attempt |
| `man/man1`, `man/man5`                  | `nightsaver(1)`, `nightsaver-command(1)`, `nightsaver.toml(5)` |
| `etc/nightsaver.toml.sample`            | annotated sample configuration   |
| `share/applications/nightsaver*.desktop`| "Blank" and "Lock" launchers     |
| `etc/xdg/autostart/nightsaver.desktop`  | starts `nightsaver --no-splash` at login |
| `etc/pam.d/nightsaver`                  | PAM policy, never overwritten    |

## Migrating from xscreensaver

1. Stop the running daemon: `xscreensaver-command -exit`
2. Remove it from your session startup (autostart entry, `.xinitrc`, desktop settings).
3. Start nightsaver: `nightsaver --no-splash &` (or log out and in to use the autostart entry).
4. Your `~/.xscreensaver` timeouts, lock settings, DPMS settings and mode are picked up
   automatically (see below for which keys). Hack names in `programs:` that are not nightsaver
   patterns are skipped.

`nightsaver` will not start while another saver owns the display, and `nightsaver-command` is a
compatible client, so scripts that call `xscreensaver-command -lock` can keep working either via the
real client (which talks to nightsaver) or via the compat links:

```
make install-compat-links
```

This creates `$PREFIX/bin/xscreensaver` and `$PREFIX/bin/xscreensaver-command` as symlinks to the
nightsaver binaries. It refuses to replace a real file, which is what the xscreensaver package installs;
it prints how to proceed (stop the daemon, then remove the package, e.g. `mport delete xscreensaver`,
and run it again). Removing the package also removes `/usr/local/etc/pam.d/xscreensaver`, so
nightsaver falls back to its own policy installed by `make install`. The setuid
`unix-selfauth-helper` comes from a separate package and stays installed.

## Configuration quick start

Create `~/.config/nightsaver/config.toml` (or pass `--config file`). Every key is optional.

```toml
timeout = "0:05:00"        # idle time before blanking
lock = true                # require a password
lock_timeout = "0:00:30"   # lock this long after blanking
mode = "random"            # one | random | blank | off
patterns = ["clock", "starfield", "matrix", "midnight", "cat", "juggler", "puppy", "penguin"]

[dpms]
enabled = true
quick_off = true           # power the monitor off as soon as it blanks

[oled]
max_brightness = 0.35
```

Precedence: command line > TOML file > `~/.xscreensaver` > built-in defaults.
See `etc/nightsaver.toml.sample` for every key and `nightsaver.toml(5)` for details. The files are
re-read within about five seconds of a change, or immediately on `kill -HUP`.

`~/.xscreensaver` keys that are honoured: `timeout`, `cycle`, `lock`, `lockTimeout`, `passwdTimeout`,
`dpmsEnabled`, `dpmsStandby`, `dpmsSuspend`, `dpmsOff`, `dpmsQuickOff` (and `dpmsQuickoffEnabled`),
`mode`, `selected`, `programs` (hack names matched against the patterns below), `verbose`, `splash`.
`fade`/`unfade` are parsed and ignored; everything else is ignored.

### Daemon flags

```
nightsaver [-display host:display.screen] [-verbose] [-quiet] [-no-splash] [-log file]
           [-debug] [-sync] [--lock | --no-lock] [--config file] [--pattern name]
           [--backend auto|x11|wayland]
```

Also: `-dpy`, `-disp`, `-v`/`-vv`/`-vvv`/`-vvvv`, `-nosplash`, `-splash` (ignored), `-version`,
`-help`, and `--demo-blank` (debug: black windows for 3 seconds, then exit). One or two dashes are accepted.

### Control

```
nightsaver-command -activate | -deactivate | -cycle | -next | -prev | -select N
                   | -lock | -suspend | -exit | -restart | -time | -watch | -version
```

Same meaning as `xscreensaver-command`. Exit status 0 on success, 255 if the daemon refused or is not
reachable, 1 for usage errors. `-demo` is parsed but answered with "not supported".

The daemon also listens on a Unix socket, `$XDG_RUNTIME_DIR/nightsaver-<display>.sock`, which carries
the same commands and replies without the X protocol: `nightsaver-command -socket -lock`. It is created
only when `XDG_RUNTIME_DIR` is a directory owned by you and closed to everyone else; the socket is mode
0600, connections from other users are dropped, and the lock rules (no `-exit`, `-restart` or
`-suspend` while locked) are the same as over X. The real `xscreensaver-command` only speaks X.

## Patterns

| Name        | Description                                                  |
|-------------|--------------------------------------------------------------|
| `blank`     | Pure black.                                                  |
| `clock`     | A time display that drifts around the screen.                |
| `starfield` | Stars flying outward (alias `stars`).                        |
| `matrix`    | Falling character rain (aliases `glmatrix`, `xmatrix`).      |
| `midnight`  | The MidnightBSD logo as a drifting outline with twinkling fill, and the boot loader's ASCII brand (aliases `midnightbsd`, `logo`). |
| `cat`       | An ASCII-art cat that blinks, swishes its tail, naps with floating z's and watches the odd mouse go by (aliases `asciicat`, `kitty`). |
| `juggler`   | An ASCII-art stick figure juggling a three-ball cascade on smooth arcs (alias `juggle`). |
| `puppy`     | An ASCII-art puppy that wags its tail, pants, barks, gets excited when a ball bounces past and naps with floating z's (aliases `dog`, `puppydog`). |
| `penguin`   | An ASCII-art penguin that waddles back and forth in light snow, flaps its flippers, perks up when a fish leaps past and naps standing up (alias `penguins`). |

`-select N` and `-next`/`-prev` index the configured `patterns` list. The default list is
`clock`, `starfield`, `matrix`, `midnight`, `cat`, `juggler`, `puppy`, `penguin`; `blank` is available by name or via `mode = "blank"`.

## OLED design notes

- `max_brightness` (default 0.35) caps luma on every frame of every pattern; blank is true black.
- Pixel shift: the whole frame is moved by a slow pseudo-random walk every `shift_interval`
  (default 60 s), at most `shift_max_px` (default 16) pixels from the origin.
- Rest interlude: every `rest_interval` (10 min) the screen goes fully black for `rest_duration` (20 s).
- Lit-pixel budget: patterns are given a target of at most `lit_budget` (5 %) lit pixels as a hint.
- Nothing static stays on screen: the clock drifts and the unlock dialog hides itself.
- For the strongest protection use `mode = "blank"` with `[dpms] enabled = true, quick_off = true`.
- Without the DPMS extension (framebuffer drivers such as scfb) the monitor cannot be powered off, so
  with `[dpms] enabled = true` the screen turns pure black after the shortest configured timeout
  instead (at once with `quick_off`, and on `nightsaver-command -suspend`).

## Lock and PAM

Locking is off by default. Enable it with `lock = true`, `--lock`, or at any time with
`nightsaver-command -lock` (which works even when `lock` is false).

- Authentication uses OpenPAM. The default PAM service is `xscreensaver`; its policy
  (`/usr/local/etc/pam.d/xscreensaver`) runs the setuid helper `unix-selfauth-helper` through
  `pam_exec`, so nightsaver verifies your own password without any privilege and is not setuid.
- PAM runs in a separate short-lived process, `libexec/nightsaver-auth` (not setuid either), started
  by the daemon for each attempt with the password passed over a pipe. A PAM module that crashes or
  hangs takes only the helper down: the daemon kills it after 25 s and counts anything but a clean
  exit 0 as a denial, so the screen stays locked. The daemon looks for the helper next to its own
  binary first (development builds), then in `$PREFIX/libexec` as compiled in by `make`.
- `make install` (or `make install-pam`) installs `etc/pam.d/nightsaver` (a copy of the same policy) only if the file
  does not exist. It is used automatically when the `xscreensaver` policy is missing (for example
  after `mport delete xscreensaver`), or explicitly with `pam_service = "nightsaver"`.
- Wrong passwords show an error for 2 s and add a delay of 1, 2, 4, 8, then 10 s per further failure.
  Escape or `passwd_timeout` (30 s) closes the dialog.
- While locked, `-exit`, `-restart`, `SIGTERM` and `SIGINT` are refused (logged). The same applies while
  a lock is *pending*: if another program holds the keyboard or mouse grab when a lock is due, the
  screen stays blanked with a visible "NOT LOCKED" notice until the grab succeeds, and `-deactivate`
  does not cancel it. Any PAM error counts as a failed unlock. The dialog is never shown without a
  successful keyboard and pointer grab. Password buffers are zeroed and never logged.
- If the user name cannot be looked up, the screen still locks and every attempt is denied (fail closed).
- Configuration files (`~/.xscreensaver`, the TOML file) are ignored with an error if they are not
  regular files owned by you or root, or if they are writable by group or others, since they choose the
  PAM service. `pam_service` may not contain `/`. A log file is opened without following symlinks and must
  be owned by you and not writable by others.
- Events another client sends with `XSendEvent` are ignored except for the control protocol, and
  RANDR monitors that lie inside another or whose total area exceeds twice the root are collapsed, so
  a hostile client cannot hide the dialog or force huge allocations.

Security caveats shared with xscreensaver: an X11 locker cannot stop the X server being killed
(for example Ctrl-Alt-Backspace where enabled), switching to a text VT with a logged-in session
(Ctrl-Alt-Fn), or other X clients that already hold input grabs. Disable those if they are in your
threat model. See the SECURITY section of `nightsaver(1)`.

## Testing

```
cargo test                         # unit tests: parsers, patterns, OLED guard, protocol, lock logic
cargo clippy --all-targets -- -D warnings
tests/xvfb_smoke.sh                # integration test on a private Xvfb (:99), never touches :0
```

`tests/xvfb_smoke.sh` needs `Xvfb`, `xrandr`, `xprop`, `xdotool` and the real `xscreensaver-command`, and uses the
debug binaries plus the grab-holding helper (`cargo build --all-targets` first). Do not run the daemon on your live display for testing without
stopping the real saver first.

Verified on a live MidnightBSD 4.2 Xorg session: blanking, wake on activity, and locking with a
real password accepted and a wrong one rejected through the `nightsaver` PAM policy.
Not yet tested: DPMS on real hardware (neither Xvfb nor the test machine's Xorg has the DPMS
extension), multi-monitor hotplug, and the MIT-SHM path (rendering uses plain `PutImage`).

## License

BSD 2-Clause. See `LICENSE`.
