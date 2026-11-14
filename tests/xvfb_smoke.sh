#!/usr/bin/env bash
# Xvfb smoke test for the nightsaver daemon.
#
# Starts a private Xvfb (default :99, override with NS_DISPLAY), runs target/debug/nightsaver
# against it with a throw-away HOME, and drives it with both our nightsaver-command and the
# real /usr/local/bin/xscreensaver-command. Never touches DISPLAY=:0.
#
# Usage: cargo build && cargo build --example hold_grab && tests/xvfb_smoke.sh
# Exit status: 0 when every step passes, 1 otherwise.

set -u

DPY="${NS_DISPLAY:-:99}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DAEMON="$ROOT/target/debug/nightsaver"
OURCMD="$ROOT/target/debug/nightsaver-command"
REALCMD="${XSCREENSAVER_COMMAND:-/usr/local/bin/xscreensaver-command}"
HOLD="$ROOT/target/debug/examples/hold_grab"

XVFB_PID=""
DAEMON_PID=""
HOLD_PID=""
TMP="$(mktemp -d "${TMPDIR:-/tmp}/nightsaver-smoke.XXXXXX")"
FAILS=0

cleanup() {
    # Only kill processes this script started (never by name: a real xscreensaver may run).
    if [ -n "$HOLD_PID" ] && kill -0 "$HOLD_PID" 2>/dev/null; then
        kill "$HOLD_PID" 2>/dev/null
    fi
    if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        # SIGKILL is acceptable here ONLY because this is a disposable Xvfb test daemon that
        # may be locked (a locked daemon ignores SIGTERM and cannot be unlocked without PAM).
        kill -KILL "$DAEMON_PID" 2>/dev/null
    fi
    if [ -n "$XVFB_PID" ] && kill -0 "$XVFB_PID" 2>/dev/null; then
        kill "$XVFB_PID" 2>/dev/null
        wait "$XVFB_PID" 2>/dev/null
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; FAILS=$((FAILS + 1)); }

# Poll `cmd` (a shell snippet) every 0.1 s until it succeeds or `secs` elapse.
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

ours() { DISPLAY="$DPY" "$OURCMD" "$@"; }
# The same client over the daemon's control socket instead of the X protocol.
sock() { DISPLAY="$DPY" "$OURCMD" -socket "$@"; }
# The daemon puts its control socket here (a private directory, as it insists on).
export XDG_RUNTIME_DIR="$TMP/run"
mkdir -m 700 "$XDG_RUNTIME_DIR"
real() { "$REALCMD" -display "$DPY" "$@"; }
status_time() { ours -time 2>&1; }
is_blanked() { status_time | grep -q "screen blanked since"; }
is_unblanked() { status_time | grep -q "screen non-blanked since"; }
is_locked() { status_time | grep -q "screen locked since"; }
daemon_gone() { ! real -version >/dev/null 2>&1; }
# First item of _SCREENSAVER_STATUS is the atom `name` (32a makes xprop print atom names;
# the later items are times/indices and print as nonsense atoms, which we ignore).
status_is() {
    DISPLAY="$DPY" xprop -root -f _SCREENSAVER_STATUS 32a _SCREENSAVER_STATUS \
        | grep -q "_SCREENSAVER_STATUS(INTEGER) = $1,"
}

write_config() {
    mkdir -p "$TMP/home"
    printf 'timeout: %s\ncycle: 0:00:05\nlock: False\nmode: random\n' "$1" >"$TMP/home/.xscreensaver"
}

start_daemon() {
    HOME="$TMP/home" DISPLAY="$DPY" "$DAEMON" --verbose --no-splash >>"$TMP/daemon.log" 2>&1 &
    DAEMON_PID=$!
}

dump_log() {
    echo "---- daemon log ----"
    cat "$TMP/daemon.log"
    echo "--------------------"
}

# ---------------------------------------------------------------------------
for f in "$DAEMON" "$OURCMD" "$REALCMD" "$HOLD"; do
    [ -x "$f" ] || { echo "FAIL: $f missing (run cargo build && cargo build --example hold_grab)"; exit 1; }
done
for t in Xvfb xprop xdotool xrandr; do
    command -v "$t" >/dev/null || { echo "FAIL: $t not installed"; exit 1; }
done
case "$DPY" in
    :0|:0.*) echo "FAIL: refusing to run on $DPY"; exit 1 ;;
esac
num="${DPY#:}"; num="${num%%.*}"
if [ -e "/tmp/.X${num}-lock" ]; then
    echo "FAIL: /tmp/.X${num}-lock exists: display $DPY is in use (set NS_DISPLAY)"
    exit 1
fi

Xvfb "$DPY" -screen 0 1280x800x24 -nolisten tcp >"$TMP/xvfb.log" 2>&1 &
XVFB_PID=$!
if ! wait_for 5 "DISPLAY=$DPY xdotool getmouselocation"; then
    echo "FAIL: Xvfb did not start on $DPY"; cat "$TMP/xvfb.log"; exit 1
fi

write_config 0:00:03
start_daemon

# (a) startup: status property and version over both clients ------------------------------
sleep 1
if DISPLAY="$DPY" xprop -root _SCREENSAVER_STATUS | grep -q "_SCREENSAVER_STATUS(INTEGER) = "; then
    pass "(a) _SCREENSAVER_STATUS present on root"
else
    fail "(a) _SCREENSAVER_STATUS missing"
fi
v_ours="$(ours -version 2>&1)"
v_real="$(real -version 2>&1)"
if [ "$v_ours" = "XScreenSaver 6.15" ] && [ "$v_real" = "XScreenSaver 6.15" ]; then
    pass "(a) -version: ours='$v_ours' real='$v_real'"
else
    fail "(a) -version: ours='$v_ours' real='$v_real'"
fi
v_sock="$(sock -version 2>&1)"
t_sock="$(sock -time 2>&1)"
if [ "$v_sock" = "$v_real" ] && [ "$t_sock" = "$(real -time 2>&1)" ]; then
    pass "(a) control socket: -version and -time match the X protocol ('$t_sock')"
else
    fail "(a) control socket: version='$v_sock' time='$t_sock'"
fi
# A second daemon must refuse to start.
if HOME="$TMP/home" DISPLAY="$DPY" "$DAEMON" --no-splash >"$TMP/second.log" 2>&1; then
    fail "(a) second daemon started"
elif grep -q "already running on display" "$TMP/second.log"; then
    pass "(a) second daemon refused: $(grep "already running" "$TMP/second.log")"
else
    fail "(a) second daemon failed oddly: $(cat "$TMP/second.log")"
fi

# (b) idle timeout blanks -------------------------------------------------------------------
if wait_for 6 is_blanked && status_is BLANK; then
    pass "(b) blanked after idle timeout: $(status_time)"
else
    fail "(b) not blanked: $(status_time); $(DISPLAY="$DPY" xprop -root _SCREENSAVER_STATUS)"
fi

# (c) absolute pointer move unblanks --------------------------------------------------------
DISPLAY="$DPY" xdotool mousemove 600 400
if wait_for 1 is_unblanked; then
    pass "(c) mousemove unblanked: $(status_time)"
else
    fail "(c) still blanked after mousemove: $(status_time)"
fi

# Lengthen the timeout through a config reload so the daemon does not re-blank on its own
# during the command tests (this also exercises the mtime-based reload).
sleep 1
write_config 0:10:00
if wait_for 8 "grep -q 'configuration reloaded: timeout 600s' '$TMP/daemon.log'"; then
    pass "(c) config reload picked up timeout 600s"
else
    fail "(c) config reload not seen"
fi
real -deactivate >/dev/null 2>&1   # undo a re-blank that may have happened before the reload
wait_for 2 is_unblanked || fail "(c) could not return to unblanked state"

# (d) real xscreensaver-command drives the daemon -------------------------------------------
if real -activate >/dev/null 2>&1 && is_blanked; then
    pass "(d) -activate blanked"
else
    fail "(d) -activate: $(status_time)"
fi
for c in -cycle -next -prev "-select 2"; do
    # shellcheck disable=SC2086
    if out="$(real $c 2>&1)"; then
        pass "(d) $c: $(echo "$out" | tr -s '\n' ' ')"
    else
        fail "(d) $c: $out"
    fi
done
if status_time | grep -q "(hack #2)"; then
    pass "(d) -select 2 shows hack #2"
else
    fail "(d) -select 2 hack index: $(status_time)"
fi
if real -deactivate >/dev/null 2>&1 && wait_for 1 is_unblanked; then
    pass "(d) -deactivate unblanked"
else
    fail "(d) -deactivate: $(status_time)"
fi
out="$(real -deactivate 2>&1)"
if echo "$out" | grep -q "already inactive"; then
    pass "(d) -deactivate while idle: already inactive"
else
    fail "(d) -deactivate while idle: $out"
fi

# (e) lock: EXIT and SIGTERM are refused ----------------------------------------------------
if real -lock >/dev/null 2>&1 && wait_for 2 is_locked \
    && status_is LOCK; then
    pass "(e) -lock: $(status_time)"
else
    fail "(e) -lock: $(status_time)"
fi
out="$(real -exit 2>&1)"; rc=$?
if [ "$rc" -ne 0 ] && echo "$out" | grep -q "screen is locked"; then
    pass "(e) -exit refused while locked (rc=$rc)"
else
    fail "(e) -exit while locked: rc=$rc out=$out"
fi
out="$(real -restart 2>&1)"; rc=$?
if [ "$rc" -ne 0 ] && echo "$out" | grep -q "screen is locked"; then
    pass "(e) -restart refused while locked (rc=$rc)"
else
    fail "(e) -restart while locked: rc=$rc out=$out"
fi
for c in -exit -restart -suspend; do
    out="$(sock $c 2>&1)"; rc=$?
    if [ "$rc" -ne 0 ] && echo "$out" | grep -q "screen is locked" && is_locked; then
        pass "(e) control socket: $c refused while locked (rc=$rc)"
    else
        fail "(e) control socket: $c while locked: rc=$rc out=$out"
    fi
done
if sock -time 2>&1 | grep -q "screen locked since"; then
    pass "(e) control socket: -time reports the lock"
else
    fail "(e) control socket: -time: $(sock -time 2>&1)"
fi
real -deactivate >/dev/null 2>&1
sock -deactivate >/dev/null 2>&1
if is_locked; then
    pass "(e) -deactivate keeps the lock"
else
    fail "(e) -deactivate unlocked the screen: $(status_time)"
fi
kill -TERM "$DAEMON_PID"
sleep 1
if kill -0 "$DAEMON_PID" 2>/dev/null && is_locked; then
    pass "(e) SIGTERM ignored while locked"
else
    fail "(e) daemon died or unlocked after SIGTERM"
fi
# Teardown of the locked daemon: SIGKILL is acceptable ONLY for this Xvfb test instance.
kill -KILL "$DAEMON_PID" 2>/dev/null
wait "$DAEMON_PID" 2>/dev/null
DAEMON_PID=""
wait_for 5 daemon_gone || fail "(e) rendezvous window of killed daemon still present"

# (f) unlocked daemon: RESTART re-execs, EXIT exits cleanly ---------------------------------
start_daemon
if wait_for 5 "real -version"; then
    pass "(f) daemon restarted"
else
    fail "(f) daemon did not come back"
fi
out="$(real -restart 2>&1)"; rc=$?
if [ "$rc" -eq 0 ] && wait_for 5 "real -version" && kill -0 "$DAEMON_PID" 2>/dev/null; then
    pass "(f) -restart re-exec'd (same pid $DAEMON_PID)"
else
    fail "(f) -restart: rc=$rc out=$out"
fi
out="$(sock -activate 2>&1)"; rc=$?
if [ "$rc" -eq 0 ] && is_blanked && sock -deactivate >/dev/null 2>&1 && wait_for 1 is_unblanked; then
    pass "(f) control socket: -activate blanks and -deactivate unblanks"
else
    fail "(f) control socket: -activate: rc=$rc out=$out $(status_time)"
fi
out="$(real -exit 2>&1)"; rc=$?
gone=0
for _ in $(seq 1 30); do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then gone=1; break; fi
    sleep 0.1
done
if [ "$rc" -eq 0 ] && [ "$gone" -eq 1 ]; then
    wait "$DAEMON_PID"; drc=$?
    DAEMON_PID=""
    if [ "$drc" -eq 0 ]; then
        pass "(f) -exit: daemon exited with status 0"
        if ls "$XDG_RUNTIME_DIR" | grep -q .; then
            fail "(f) control socket left behind: $(ls "$XDG_RUNTIME_DIR")"
        else
            pass "(f) control socket removed at exit"
        fi
    else
        fail "(f) -exit: daemon exit status $drc"
    fi
else
    fail "(f) -exit: rc=$rc out=$out gone=$gone"
fi
if DISPLAY="$DPY" xprop -root _SCREENSAVER_STATUS 2>&1 | grep -Eq "not found|no such atom"; then
    pass "(f) _SCREENSAVER_STATUS removed at exit"
else
    fail "(f) status after exit: $(DISPLAY="$DPY" xprop -root _SCREENSAVER_STATUS 2>&1)"
fi

# (g) LOCK while another client holds the keyboard grab -----------------------------------
start_daemon
wait_for 5 "real -version" || fail "(g) daemon did not start"
DISPLAY="$DPY" "$HOLD" 7 >"$TMP/hold.out" 2>&1 &
HOLD_PID=$!
if ! wait_for 3 "grep -q grabbed '$TMP/hold.out'"; then
    fail "(g) hold_grab helper did not grab: $(cat "$TMP/hold.out")"
fi
out="$(real -lock 2>&1)"; rc=$?
if [ "$rc" -ne 0 ] && echo "$out" | grep -q "unable to grab keyboard" && status_is BLANK; then
    pass "(g) -lock refused while keyboard grabbed elsewhere (rc=$rc: $out); status BLANK"
else
    fail "(g) -lock while grabbed elsewhere: rc=$rc out=$out status=$(status_time)"
fi
# Activity must not cancel a pending lock.
DISPLAY="$DPY" xdotool mousemove 100 100
sleep 0.3
DISPLAY="$DPY" xdotool mousemove 700 500
sleep 0.5
if status_is BLANK && is_blanked; then
    pass "(g) pointer activity did not cancel the pending lock"
else
    fail "(g) pending lock cancelled by activity: $(status_time)"
fi
wait "$HOLD_PID" 2>/dev/null
HOLD_PID=""
if wait_for 2 "status_is LOCK" && is_locked; then
    pass "(g) locked within 2 s after the other grab was released: $(status_time)"
else
    fail "(g) not locked after the other grab was released: $(status_time)"
fi

# (h) RANDR change while locked keeps the lock and the grab --------------------------------
before="$(grep -c 'monitor layout changed' "$TMP/daemon.log")"
DISPLAY="$DPY" xrandr --output screen --off >/dev/null 2>&1
sleep 0.5
DISPLAY="$DPY" xrandr --output screen --auto >/dev/null 2>&1
if wait_for 3 "[ \"\$(grep -c 'monitor layout changed' '$TMP/daemon.log')\" -gt $before ]"; then
    pass "(h) relayout ran ($(grep -c 'monitor layout changed' "$TMP/daemon.log") total)"
else
    fail "(h) no relayout observed after xrandr --output screen --off/--auto"
fi
sleep 0.5
if kill -0 "$DAEMON_PID" 2>/dev/null && status_is LOCK && is_locked; then
    pass "(h) still locked after relayout"
else
    fail "(h) after relayout: $(status_time)"
fi
if grep -q "grab lost" "$TMP/daemon.log"; then
    fail "(h) grab was lost: $(grep 'grab lost' "$TMP/daemon.log")"
else
    pass "(h) grab kept across relayout"
fi
# Garbage text only, never a real password.
DISPLAY="$DPY" xdotool type --delay 30 'notapassword'
DISPLAY="$DPY" xdotool key Return
if wait_for 3 "status_is AUTH"; then
    pass "(h) keys reach the locker after relayout (status AUTH)"
else
    fail "(h) typing did not reach the locker: $(DISPLAY="$DPY" xprop -root -f _SCREENSAVER_STATUS 32a _SCREENSAVER_STATUS)"
fi
if grep -Eq "KeyPressEvent|KeyReleaseEvent" "$TMP/daemon.log"; then
    fail "(h) key events appear in the log"
else
    pass "(h) no key events in the log"
fi
kill -KILL "$DAEMON_PID" 2>/dev/null   # locked Xvfb test daemon only
wait "$DAEMON_PID" 2>/dev/null
DAEMON_PID=""

# ---------------------------------------------------------------------------
if [ "$FAILS" -ne 0 ]; then
    dump_log
    echo "SMOKE TEST FAILED ($FAILS failure(s))"
    exit 1
fi
echo "SMOKE TEST PASSED"
exit 0
