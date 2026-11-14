#!/usr/bin/env bash
# Wayland smoke test for the nightsaver daemon.
#
# Starts a private headless sway (its own XDG_RUNTIME_DIR, no real outputs or input
# devices), runs target/debug/nightsaver in it with a throw-away HOME, and drives it with
# nightsaver-command over the control socket. Input comes from a virtual pointer and a
# virtual keyboard, and another locker is played by the wl_lock helper. Never touches the
# user's session.
#
# The daemon runs from a private copy with a stand-in nightsaver-auth beside it that accepts
# one fixed test password, so the unlock path is exercised without the system PAM policy
# or a real password. PAM itself is not covered here.
#
# Usage: cargo build --bins --examples && tests/wayland_smoke.sh
# Needs: sway, ImageMagick's convert.
# Exit status: 0 when every step passes, 1 otherwise.

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${NS_BIN:-$ROOT/target/debug}"
DAEMON_SRC="$BIN/nightsaver"
CMD="$BIN/nightsaver-command"
SHOT="$BIN/examples/wl_shot"
POINTER="$BIN/examples/wl_pointer"
LOCKER="$BIN/examples/wl_lock"
KEYS="$BIN/examples/wl_keys"
# Not a real password: only the stand-in helper below knows it.
TEST_PASSWORD='Smoke-pass1'

SWAY_PID=""
DAEMON_PID=""
POINTER_PID=""
LOCKER_PID=""
KEYS_PID=""
# Short on purpose: the Wayland and control sockets live here and a Unix socket path is
# limited to about 100 bytes.
TMP="$(mktemp -d /tmp/ns-wl.XXXXXX)"
FAILS=0

cleanup() {
    exec 9>&- 2>/dev/null
    exec 8>&- 2>/dev/null
    for p in "$LOCKER_PID" "$POINTER_PID" "$KEYS_PID"; do
        [ -n "$p" ] && kill "$p" 2>/dev/null
    done
    # Stopping the compositor ends a locked daemon cleanly (its connection is gone); the
    # daemon itself is never killed with SIGKILL here.
    if [ -n "$SWAY_PID" ] && kill -0 "$SWAY_PID" 2>/dev/null; then
        kill "$SWAY_PID" 2>/dev/null
        wait "$SWAY_PID" 2>/dev/null
    fi
    if [ -n "$DAEMON_PID" ]; then
        for _ in $(seq 1 30); do
            kill -0 "$DAEMON_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill "$DAEMON_PID" 2>/dev/null
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }

wait_for() {
    local secs="$1"; shift
    local tries=$((secs * 10))
    while [ "$tries" -gt 0 ]; do
        if eval "$@" >/dev/null 2>&1; then return 0; fi
        sleep 0.1
        tries=$((tries - 1))
    done
    return 1
}

ctl() { "$CMD" "$@"; }
status_time() { ctl -time 2>&1; }
is_blanked() { status_time | grep -q "screen blanked since"; }
is_unblanked() { status_time | grep -q "screen non-blanked since"; }
is_locked() { status_time | grep -q "screen locked since"; }
ptr() { echo "$*" >&9; sleep 0.3; }
keys() { echo "$*" >&8; sleep 0.3; }
# Mean brightness of the screen, in millionths (0 = pure black).
screen_mean() {
    "$SHOT" "$TMP/shot.ppm" >/dev/null 2>&1 || { echo -1; return; }
    convert "$TMP/shot.ppm" -format "%[fx:int(mean*1e6)]" info: 2>/dev/null || echo -1
}

for tool in sway convert "$DAEMON_SRC" "$CMD" "$SHOT" "$POINTER" "$LOCKER" "$KEYS"; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "FAIL: missing $tool (run: cargo build --bins --examples)"; exit 1
    fi
done

export XDG_RUNTIME_DIR="$TMP/run"
mkdir -m 700 "$XDG_RUNTIME_DIR"
export HOME="$TMP/home"
mkdir -p "$HOME/.config/nightsaver"
unset DISPLAY XAUTHORITY WAYLAND_DISPLAY SWAYSOCK

# The daemon prefers a nightsaver-auth next to its own executable, so a private copy gets
# the stand-in: exit 0 for the test password, 1 (denied) for anything else.
mkdir "$TMP/bin"
cp "$DAEMON_SRC" "$TMP/bin/nightsaver"
DAEMON="$TMP/bin/nightsaver"
cat >"$TMP/bin/nightsaver-auth" <<EOF
#!/bin/sh
pw="\$(tr '\\000' '\\n' | sed -n 3p)"
[ "\$pw" = '$TEST_PASSWORD' ] && exit 0
echo "stand-in helper: wrong password"
exit 1
EOF
chmod 755 "$TMP/bin/nightsaver-auth"

write_config() {
    cat >"$HOME/.config/nightsaver/config.toml" <<EOF
timeout = "$1"
mode = "one"
patterns = ["clock"]
EOF
}

WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
    sway -c /dev/null >"$TMP/sway.log" 2>&1 &
SWAY_PID=$!
if ! wait_for 5 "ls $XDG_RUNTIME_DIR/wayland-1"; then
    echo "FAIL: sway did not start"; cat "$TMP/sway.log"; exit 1
fi
export WAYLAND_DISPLAY=wayland-1

mkfifo "$TMP/ptr.fifo"
"$POINTER" <"$TMP/ptr.fifo" >"$TMP/ptr.out" 2>&1 &
POINTER_PID=$!
exec 9>"$TMP/ptr.fifo"
wait_for 3 "grep -q ready $TMP/ptr.out" || { echo "FAIL: no virtual pointer"; cat "$TMP/ptr.out"; exit 1; }
mkfifo "$TMP/keys.fifo"
"$KEYS" <"$TMP/keys.fifo" >"$TMP/keys.out" 2>&1 &
KEYS_PID=$!
exec 8>"$TMP/keys.fifo"
wait_for 3 "grep -q ready $TMP/keys.out" || { echo "FAIL: no virtual keyboard"; cat "$TMP/keys.out"; exit 1; }

write_config 0:00:03
"$DAEMON" --verbose >"$TMP/daemon.log" 2>&1 &
DAEMON_PID=$!

# (a) startup ----------------------------------------------------------------------------
if wait_for 5 "ctl -version" && [ "$(ctl -version 2>&1)" = "XScreenSaver 6.15" ]; then
    pass "(a) daemon answers on the control socket: $(ctl -version 2>&1)"
else
    fail "(a) no answer on the control socket"; cat "$TMP/daemon.log"; exit 1
fi
if "$DAEMON" >"$TMP/second.log" 2>&1; then
    fail "(a) second daemon started"
elif grep -q "already running on display" "$TMP/second.log"; then
    pass "(a) second daemon refused: $(cat "$TMP/second.log")"
else
    fail "(a) second daemon failed for another reason: $(cat "$TMP/second.log")"
fi

# (b) idle detection and wake-up ------------------------------------------------------------
ptr move 5 5; sleep 2; ptr move 5 5; sleep 1.5
if is_unblanked; then
    pass "(b) pointer activity keeps the screen unblanked past the timeout"
else
    fail "(b) blanked despite activity: $(status_time)"
fi
if wait_for 6 is_blanked; then
    pass "(b) blanked after the idle timeout"
else
    fail "(b) not blanked after idle: $(status_time)"
fi
mean="$(screen_mean)"
if [ "$mean" -gt 0 ]; then
    pass "(b) the pattern is on screen (mean $mean ppm)"
else
    fail "(b) nothing drawn (mean $mean)"
fi
ptr move 2 2
if is_blanked; then
    pass "(b) motion below the threshold does not unblank"
else
    fail "(b) small motion unblanked: $(status_time)"
fi
ptr move 200 100
if wait_for 2 is_unblanked; then
    pass "(b) one large motion unblanks"
else
    fail "(b) large motion did not unblank: $(status_time)"
fi
# No more idle blanking from here on, so the remaining steps cannot race with it.
write_config 0:10:00
kill -HUP "$DAEMON_PID"
wait_for 5 "grep -q 'configuration reloaded: timeout 600s' '$TMP/daemon.log'" \
    || fail "(b) configuration reload not seen"
ctl -deactivate >/dev/null 2>&1
if ctl -activate >/dev/null 2>&1 && is_blanked; then
    # sway gives a newly mapped surface pointer focus only once the pointer moves, so a
    # click with no motion at all since blanking is not delivered to anyone.
    ptr move 1 1
    if is_blanked; then
        ptr click
    fi
    if wait_for 2 is_unblanked; then
        pass "(b) -activate blanks and a click unblanks"
    else
        fail "(b) click did not unblank: $(status_time)"
    fi
else
    fail "(b) -activate: $(status_time)"
fi

# (c) another locker holds the session: our lock is pending, never claimed -------------------
"$LOCKER" 4 >"$TMP/lock.out" 2>&1 &
LOCKER_PID=$!
wait_for 3 "grep -q locked $TMP/lock.out" || fail "(c) wl_lock did not get the session lock"
out="$(ctl -lock 2>&1)"; rc=$?
if [ "$rc" -ne 0 ] && echo "$out" | grep -q "unable to lock the session" && is_blanked; then
    pass "(c) -lock refused while another locker holds the session (rc=$rc); status BLANK"
else
    fail "(c) -lock under another locker: rc=$rc out=$out $(status_time)"
fi
ptr move 300 300; ptr click
ctl -deactivate >/dev/null 2>&1
if is_blanked; then
    pass "(c) activity and -deactivate do not cancel the pending lock"
else
    fail "(c) pending lock cancelled: $(status_time)"
fi
out="$(ctl -exit 2>&1)"; rc=$?
if [ "$rc" -ne 0 ] && echo "$out" | grep -q "screen is locked"; then
    pass "(c) -exit refused while the lock is pending"
else
    fail "(c) -exit while pending: rc=$rc out=$out"
fi
kill -TERM "$DAEMON_PID"; sleep 0.5
if kill -0 "$DAEMON_PID" 2>/dev/null; then
    pass "(c) SIGTERM ignored while the lock is pending"
else
    fail "(c) daemon died on SIGTERM while pending"
fi
if wait_for 8 is_locked; then
    pass "(c) locked once the other locker left: $(status_time)"
else
    fail "(c) still not locked: $(status_time)"
fi
LOCKER_PID=""

# (d) locked ----------------------------------------------------------------------------------
if grep -q "locked (pattern" "$TMP/daemon.log"; then
    pass "(d) the compositor confirmed the session lock"
else
    fail "(d) no lock in the log"
fi
for c in -exit -restart -suspend; do
    out="$(ctl $c 2>&1)"; rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "screen is locked" && is_locked; then
        pass "(d) $c refused while locked (rc=$rc)"
    else
        fail "(d) $c while locked: rc=$rc out=$out"
    fi
done
before="$(screen_mean)"
ptr move 100 100; ptr click
ctl -deactivate >/dev/null 2>&1
sleep 0.5
after="$(screen_mean)"
if is_locked && [ "$after" -gt "$before" ]; then
    pass "(d) activity shows the unlock dialog and keeps the lock (mean $before -> $after ppm)"
else
    fail "(d) dialog: locked=$(status_time) mean $before -> $after"
fi
kill -TERM "$DAEMON_PID"; sleep 1
if kill -0 "$DAEMON_PID" 2>/dev/null && is_locked; then
    pass "(d) SIGTERM ignored while locked"
else
    fail "(d) daemon died or unlocked after SIGTERM"
fi
before="$(grep -c 'monitor layout changed' "$TMP/daemon.log")"
swaymsg -s "$(ls "$XDG_RUNTIME_DIR"/sway-ipc.*.sock)" create_output >/dev/null 2>&1
if wait_for 3 "[ \"\$(grep -c 'monitor layout changed' '$TMP/daemon.log')\" -gt $before ]" && is_locked; then
    pass "(d) a new output is covered and the lock is kept"
else
    fail "(d) output hotplug: $(status_time)"
fi

# (e) the keyboard: a wrong password is denied, the right one unlocks --------------------------
ptr move 20 20
keys type not-the-password
keys key enter
if wait_for 10 "grep -Eqi 'authentication failed|denied' '$TMP/daemon.log'" && is_locked; then
    pass "(e) a typed wrong password reaches the helper, is denied, and the lock is kept"
else
    fail "(e) wrong password: $(status_time); $(tail -3 "$TMP/daemon.log")"
fi
# Typed, corrected with backspace, then submitted: Shift, digits and punctuation included.
# Retried because a failed attempt is followed by a delay.
unlocked=0
for _ in 1 2 3 4 5 6; do
    sleep 2
    ptr move 20 20
    keys type "${TEST_PASSWORD}x"
    keys key backspace
    keys key enter
    if wait_for 3 is_unblanked; then unlocked=1; break; fi
done
if [ "$unlocked" -eq 1 ] && grep -q "unblanked (Unlocked)" "$TMP/daemon.log"; then
    pass "(e) the right password unlocks: $(status_time)"
else
    fail "(e) right password did not unlock: $(status_time); $(tail -5 "$TMP/daemon.log")"
fi
if "$LOCKER" 1 >"$TMP/lock2.out" 2>&1 && grep -q locked "$TMP/lock2.out"; then
    pass "(e) the compositor released the session (another client can lock it again)"
else
    fail "(e) the session lock was not released: $(cat "$TMP/lock2.out")"
fi
if grep -Eqi "key *(press|input|sym)|$TEST_PASSWORD|not-the-password" "$TMP/daemon.log"; then
    fail "(e) key data or a password in the log"
else
    pass "(e) no key events or passwords in the log"
fi
if ctl -lock >/dev/null 2>&1 && wait_for 5 is_locked; then
    pass "(e) locks again after an unlock"
else
    fail "(e) second lock: $(status_time)"
fi

# (f) the compositor goes away: the daemon ends, the lock was never released -----------------
kill "$SWAY_PID"; wait "$SWAY_PID" 2>/dev/null; SWAY_PID=""
gone=0
for _ in $(seq 1 50); do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then gone=1; break; fi
    sleep 0.1
done
if [ "$gone" -eq 1 ] && ! grep -q "unblanked" <(awk '/locked \(pattern/ { buf = "" } { buf = buf $0 "\n" } END { printf "%s", buf }' "$TMP/daemon.log"); then
    pass "(f) daemon exited with the compositor, without unlocking first"
else
    fail "(f) gone=$gone; log tail: $(tail -3 "$TMP/daemon.log")"
fi
DAEMON_PID=""

echo
if [ "$FAILS" -eq 0 ]; then
    echo "WAYLAND SMOKE TEST PASSED"
    exit 0
fi
echo "WAYLAND SMOKE TEST FAILED ($FAILS)"
cp "$TMP/daemon.log" "${TMPDIR:-/tmp}/nightsaver-wayland-smoke-daemon.log" 2>/dev/null
exit 1
