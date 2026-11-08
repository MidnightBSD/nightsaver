//! xscreensaver-command compatible control client.
//!
//! Argument handling, output text and exit statuses follow xscreensaver 6.17
//! `driver/xscreensaver-command.c` and `driver/remote.c`; see
//! `nightsaver::compat::protocol` for the wire format. Self-contained: talks to
//! the server with x11rb's pure-Rust connection, not `nightsaver::x11`.

use std::ffi::OsString;
use std::io::Write;
use std::process::exit;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{ArgGroup, Parser};
use nightsaver::compat::protocol::{
    self, ATOM_STATUS, AtomTable, Command, ProtocolAtoms, ProtocolError, RawStatus, Response,
    TimeLine, WatchState,
};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::dpms::{ConnectionExt as _, DPMSMode};
use x11rb::protocol::xproto::{ConnectionExt as _, Property, Window};

/// Exit status for xscreensaver-command's `exit(-1)`.
const EXIT_FAIL: i32 = 255;

/// Canonical (post-normalisation) command line. Users never type these directly
/// in xscreensaver style; [`normalise`] maps `-act`, `--dpy` etc. onto them.
#[derive(Parser, Debug)]
#[command(
    name = "nightsaver-command",
    disable_help_flag = true,
    disable_version_flag = true,
    args_override_self = true,
    group(ArgGroup::new("cmd").multiple(false).args([
        "activate", "deactivate", "suspend", "cycle", "next", "prev", "select",
        "exit", "restart", "demo", "lock", "version", "time", "watch", "help",
    ]))
)]
struct Cli {
    #[arg(long)]
    display: Option<String>,
    /// 0 = -quiet, 1 = -verbose (default), 2..4 = -vv..-vvvv.
    #[arg(long, default_value_t = 1)]
    verbosity: u8,
    #[arg(long)]
    activate: bool,
    #[arg(long)]
    deactivate: bool,
    #[arg(long)]
    suspend: bool,
    #[arg(long)]
    cycle: bool,
    #[arg(long)]
    next: bool,
    #[arg(long)]
    prev: bool,
    #[arg(long, allow_negative_numbers = true)]
    select: Option<i64>,
    #[arg(long)]
    exit: bool,
    #[arg(long)]
    restart: bool,
    #[arg(long, allow_negative_numbers = true)]
    demo: Option<i64>,
    #[arg(long)]
    lock: bool,
    #[arg(long)]
    version: bool,
    #[arg(long)]
    time: bool,
    #[arg(long)]
    watch: bool,
    #[arg(long)]
    help: bool,
}

/// Command names in xscreensaver-command's matching order (prefix match).
const COMMANDS: &[&str] = &[
    "activate",
    "deactivate",
    "suspend",
    "cycle",
    "next",
    "prev",
    "select",
    "exit",
    "restart",
    "demo",
    "lock",
    "version",
    "time",
    "watch",
    "help",
];

fn usage_error(prog: &str, what: &str, arg: &str) -> ! {
    eprintln!("{prog}: {what}: {arg}\n{prog}: try --help");
    exit(1);
}

/// `strncmp(s, full, strlen(s)) == 0`: `s` is a prefix of `full`.
fn prefix_of(s: &str, full: &str) -> bool {
    full.starts_with(s)
}

/// `sscanf(s, " %ld %c", ..) == 1`: exactly one integer, surrounding whitespace allowed.
fn parse_long(s: &str) -> Option<i64> {
    s.trim_matches(|c: char| c.is_ascii_whitespace())
        .parse()
        .ok()
}

/// Port of xscreensaver-command's argument loop. Resolves abbreviations and single
/// or double dashes to canonical `--long` flags for clap, and reports usage errors
/// with xscreensaver's exact wording. Returns the canonical argv and `cmdstr`
/// (the argument used in "bad option"/"missing hack number" messages).
fn normalise(prog: &str, args: &[String]) -> (Vec<OsString>, String) {
    let mut out: Vec<OsString> = vec![prog.into()];
    let mut cmd: Option<&str> = None;
    let mut cmdstr = String::new();
    let mut i = 0;
    while i < args.len() {
        let orig = &args[i];
        let s = if orig.starts_with("--") {
            &orig[1..]
        } else {
            &orig[..]
        };
        if s.len() < 2 {
            usage_error(prog, "unrecognized", orig);
        }
        if prefix_of(s, "-display") || prefix_of(s, "-dpy") {
            i += 1;
            if let Some(d) = args.get(i) {
                out.push("--display".into());
                out.push(d.into());
            }
            i += 1;
            continue;
        }
        let verbosity = if prefix_of(s, "-quiet") {
            Some(0)
        } else {
            match s {
                "-verbose" | "-v" => Some(1),
                "-vv" => Some(2),
                "-vvv" => Some(3),
                "-vvvv" => Some(4),
                _ => None,
            }
        };
        if let Some(v) = verbosity {
            out.push(format!("--verbosity={v}").into());
            i += 1;
            continue;
        }
        if cmd.is_some() {
            usage_error(prog, "extraneous", orig);
        }
        let Some(name) = COMMANDS.iter().find(|c| prefix_of(s, &format!("-{c}"))) else {
            usage_error(prog, "unrecognized", orig);
        };
        if *name == "help" {
            print_usage(prog);
            exit(1);
        }
        cmd = Some(name);
        cmdstr = orig.clone();
        out.push(format!("--{name}").into());
        if *name == "select" || *name == "demo" {
            let mut arg = 0;
            if let Some(n) = args.get(i + 1).and_then(|a| parse_long(a)) {
                cmdstr = args[i + 1].clone();
                arg = n;
                i += 1;
            }
            out.push(format!("{arg}").into());
        }
        i += 1;
    }
    if cmd.is_none() {
        usage_error(prog, "no commands", "");
    }
    (out, cmdstr)
}

fn print_usage(prog: &str) {
    eprint!(
        "\n\
usage: {prog} -<option>\n\
\n\
  Control a running nightsaver (or xscreensaver) daemon. Version {ver}.\n\
  Options may be abbreviated and given with one or two dashes.\n\
\n\
  -display <dpy> X display to talk to (default: $DISPLAY).\n\
  -quiet         Only print output if an error occurs.\n\
  -verbose       Opposite of -quiet. Default.\n\
\n\
  -activate      Blank the screen now, as if the user had been idle.\n\
  -deactivate    Unblank the screen, as if there had been user activity.\n\
  -cycle         If blanked, switch to another (random) display mode.\n\
  -next          Like -activate or -cycle, but run the next mode in the list.\n\
  -prev          Like -next, in the other direction.\n\
  -select <N>    Like -activate, but run mode number N (1-based).\n\
  -lock          Lock the screen now (blanking it if necessary).\n\
  -suspend       Like -activate, but power the monitor off immediately.\n\
  -exit          Make the daemon exit. Does nothing while the screen is locked.\n\
  -restart       Make the daemon re-execute itself.\n\
  -time          Print when the screen was last blanked or unblanked.\n\
  -watch         Print a line each time the saver state changes. Never returns.\n\
  -version       Print the version of the running daemon.\n\
\n",
        ver = nightsaver::VERSION
    );
}

/// `ctime(3)` without the trailing newline, in local time.
fn ctime(t: i64) -> String {
    let tt = t as libc::time_t;
    // SAFETY: an all-zero `tm` is a valid value of this plain C struct (its only
    // pointer field, tm_zone, may be null).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers refer to live, properly aligned locals for the
    // duration of the call; localtime_r is the reentrant variant and only writes `tm`.
    let ok = unsafe { !libc::localtime_r(&tt, &mut tm).is_null() };
    if !ok {
        return "???".into();
    }
    protocol::format_ctime(
        tm.tm_wday as u32,
        tm.tm_mon as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
        i64::from(tm.tm_year) + 1900,
    )
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn flush_all() {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

fn fail(prog: &str, msg: impl std::fmt::Display) -> ! {
    flush_all();
    eprintln!("{prog}: {msg}");
    exit(EXIT_FAIL);
}

enum Action {
    Send(Command),
    Demo(u32),
    Version,
    Time,
    Watch,
}

fn main() {
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let prog = argv
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a).to_owned())
        .unwrap_or_else(|| "nightsaver-command".into());
    let (canonical, cmdstr) = normalise(&prog, argv.get(1..).unwrap_or(&[]));
    let cli = match Cli::try_parse_from(canonical) {
        Ok(c) => c,
        Err(e) => usage_error(
            &prog,
            "bad option",
            e.to_string().lines().next().unwrap_or(""),
        ),
    };

    let arg = cli.select.or(cli.demo).unwrap_or(0);
    let takes_arg = cli.select.is_some() || cli.demo.is_some();
    if arg < 0 || (arg > 0 && !takes_arg) || arg > i64::from(u32::MAX) {
        usage_error(&prog, "bad option", &cmdstr);
    } else if arg == 0 && takes_arg {
        usage_error(&prog, "missing hack number", &cmdstr);
    }
    let n = arg as u32;
    let action = if cli.activate {
        Action::Send(Command::Activate)
    } else if cli.deactivate {
        Action::Send(Command::Deactivate)
    } else if cli.suspend {
        Action::Send(Command::Suspend)
    } else if cli.cycle {
        Action::Send(Command::Cycle)
    } else if cli.next {
        Action::Send(Command::Next)
    } else if cli.prev {
        Action::Send(Command::Prev)
    } else if cli.select.is_some() {
        Action::Send(Command::Select(n))
    } else if cli.exit {
        Action::Send(Command::Exit)
    } else if cli.restart {
        Action::Send(Command::Restart)
    } else if cli.demo.is_some() {
        Action::Demo(n)
    } else if cli.lock {
        Action::Send(Command::Lock)
    } else if cli.version {
        Action::Version
    } else if cli.time {
        Action::Time
    } else if cli.watch {
        Action::Watch
    } else {
        usage_error(&prog, "no commands", "");
    };

    let dpyname = match cli.display.or_else(|| std::env::var("DISPLAY").ok()) {
        Some(d) => d,
        None => {
            let d = ":0.0".to_owned();
            eprintln!("{prog}: warning: $DISPLAY is not set: defaulting to \"{d}\"");
            d
        }
    };
    let (conn, _screen) = match x11rb::connect(Some(&dpyname)) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("{prog}: can't open display {dpyname}");
            exit(1);
        }
    };
    let atoms = match ProtocolAtoms::intern(&conn) {
        Ok(a) => a,
        Err(e) => fail(&prog, e),
    };
    let ctx = Ctx {
        prog: &prog,
        dpyname: &dpyname,
        conn: &conn,
        atoms: &atoms,
        verbosity: cli.verbosity,
    };
    let rc = match action {
        Action::Watch => watch(&ctx),
        Action::Version => ctx.version_or_time(false),
        Action::Time => ctx.version_or_time(true),
        Action::Send(c) => ctx.command(c, None),
        Action::Demo(n) => ctx.command(Command::Demo, Some(n)),
    };
    flush_all();
    exit(match rc {
        Ok(code) => code,
        Err(e) => fail(&prog, e),
    });
}

struct Ctx<'a, C: Connection> {
    prog: &'a str,
    dpyname: &'a str,
    conn: &'a C,
    atoms: &'a ProtocolAtoms,
    verbosity: u8,
}

impl<C: Connection> Ctx<'_, C> {
    fn root0(&self) -> Window {
        self.conn.setup().roots[0].root
    }

    /// `-version` / `-time` (`remote.c`, the XA_SCREENSAVER_VERSION/STATUS branch).
    fn version_or_time(&self, time: bool) -> Result<i32, ProtocolError> {
        let (conn, prog) = (self.conn, self.prog);
        let Some(win) = protocol::find_saver_window_any_screen(conn, self.atoms)? else {
            fail(
                prog,
                format_args!("no screensaver is running on display {}", self.dpyname),
            );
        };
        protocol::select_property_change(conn, win)?;
        let v = protocol::read_version(conn, win, self.atoms)?.unwrap_or_default();
        if v.is_empty() {
            fail(
                prog,
                format_args!("version property not set on window 0x{win:x}?"),
            );
        }
        let Some(class) = protocol::read_res_class(conn, win)? else {
            fail(
                prog,
                format_args!("class hints not set on window 0x{win:x}?"),
            );
        };
        print!("{} {}", printable(&class), printable(&v));
        if !time {
            println!();
            return Ok(0);
        }
        match protocol::read_status(conn, self.root0(), self.atoms)? {
            RawStatus::Items(items) => {
                match protocol::format_time_line(&items, self.atoms, ctime) {
                    TimeLine::Ok(s) => {
                        print!("{s}");
                        Ok(0)
                    }
                    TimeLine::BadState => {
                        flush_all();
                        eprintln!("bad status format on root window");
                        Ok(EXIT_FAIL)
                    }
                }
            }
            other => {
                println!();
                flush_all();
                if other == RawStatus::Missing {
                    eprintln!("no saver status on root window");
                } else {
                    eprintln!("bad status format on root window");
                }
                Ok(EXIT_FAIL)
            }
        }
    }

    /// Send a command, read the response and, if appropriate, wait for blanking
    /// (`remote.c: xscreensaver_command`).
    fn command(&self, cmd: Command, demo_arg: Option<u32>) -> Result<i32, ProtocolError> {
        let (conn, prog) = (self.conn, self.prog);
        let Some(win) = protocol::find_saver_window_any_screen(conn, self.atoms)? else {
            if cmd == Command::Exit {
                // Don't print an error if the daemon is already gone.
                return Ok(0);
            }
            fail(
                prog,
                format_args!("no screensaver is running on display {}", self.dpyname),
            );
        };
        if cmd == Command::Deactivate {
            reset_dpms_timer(conn);
        }
        match demo_arg {
            Some(n) => {
                let mut data = protocol::encode_command(cmd, self.atoms, 0)?;
                data[1] = protocol::DEMO_ARG_MARKER;
                data[2] = n;
                protocol::send_command_data(conn, win, self.atoms, data)?;
            }
            None => protocol::send_command(conn, win, self.atoms, cmd)?,
        }

        let resp = protocol::wait_response(conn, win, self.atoms, Duration::from_secs(10))?;
        match resp {
            Some(Response::Reply { ok: true, text }) => {
                if self.verbosity > 0 {
                    println!("{prog}: {}\n", printable(&text));
                }
            }
            Some(Response::Reply { ok: false, text }) => {
                flush_all();
                eprintln!("{prog}: {}\n", printable(&text));
                return Ok(EXIT_FAIL);
            }
            Some(Response::NoResponse) => fail(prog, "no response to command."),
            Some(Response::WindowDeleted) => {
                if cmd == Command::Exit {
                    return Ok(0);
                }
                fail(prog, "xscreensaver window unexpectedly deleted.");
            }
            Some(Response::BadProperty) => fail(prog, "unrecognized response property."),
            Some(Response::BadMessage) => fail(prog, "unrecognized response message."),
            None => return Ok(EXIT_FAIL),
        }

        if cmd.waits_for_blank() {
            return self.wait_for_blank();
        }
        Ok(0)
    }

    /// `remote.c: xscreensaver_command_wait_for_blank`.
    fn wait_for_blank(&self) -> Result<i32, ProtocolError> {
        let prog = self.prog;
        let root = self.root0();
        let mut verbosity = self.verbosity;
        let start = now_secs();
        loop {
            if let RawStatus::Items(items) = protocol::read_status(self.conn, root, self.atoms)? {
                if verbosity > 1 {
                    let state = match self.atoms.name(items[0]) {
                        _ if items[0] == 0 => "0",
                        Some(n @ ("BLANK" | "LOCK" | "AUTH")) => n,
                        _ => "???",
                    };
                    let rest: String = items[1..].iter().map(|v| format!(", {v}")).collect();
                    eprintln!("{prog}: read status property: 0x{root:x}: {state}{rest}");
                }
                if items[0] != 0 {
                    if verbosity > 1 {
                        eprintln!("{prog}: screen blanked");
                    }
                    return Ok(0);
                }
            }
            let now = now_secs();
            if now >= start + 10 {
                fail(prog, "Timed out waiting for screen to blank");
            } else if verbosity == 1 && now > start + 3 {
                eprintln!("{prog}: waiting for status change");
                verbosity += 1;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// `-watch` (`xscreensaver-command.c: watch`). Only returns on error.
fn watch<C: Connection>(ctx: &Ctx<'_, C>) -> Result<i32, ProtocolError> {
    let (conn, prog) = (ctx.conn, ctx.prog);
    let root = ctx.root0();
    let status_atom = ctx.atoms.atom(ATOM_STATUS).unwrap_or(0);
    protocol::select_property_change(conn, root)?;
    conn.flush()?;
    let mut state = WatchState::new();
    loop {
        let ev = conn.wait_for_event()?;
        let Event::PropertyNotify(p) = ev else {
            continue;
        };
        if p.state != Property::NEW_VALUE || p.atom != status_atom {
            continue;
        }
        let lines = match protocol::read_status(conn, root, ctx.atoms)? {
            RawStatus::Items(items) => state.update(&items, ctx.atoms, ctime),
            _ => None,
        };
        let Some(lines) = lines else {
            fail(prog, "no saver status on root window");
        };
        let mut out = std::io::stdout().lock();
        for l in lines {
            let _ = writeln!(out, "{l}");
        }
        let _ = out.flush();
    }
}

/// `remote.c: reset_dpms_timer`: if DPMS is capable and enabled, force the
/// monitor on and restart the server's DPMS countdown. All errors are ignored.
fn reset_dpms_timer<C: Connection>(conn: &C) {
    let run = || -> Option<()> {
        if !conn.dpms_capable().ok()?.reply().ok()?.capable {
            return None;
        }
        if !conn.dpms_info().ok()?.reply().ok()?.state {
            return None;
        }
        let _ = conn.dpms_force_level(DPMSMode::ON).ok()?.check();
        let _ = conn.dpms_disable().ok()?.check();
        let _ = conn.dpms_enable().ok()?.check();
        Some(())
    };
    let _ = run();
    let _ = conn.get_input_focus().map(|c| c.reply());
}

/// Strings read from X properties are written by other clients: strip control characters
/// (terminal escape sequences) before printing them.
fn printable(s: &str) -> String {
    s.chars().map(|c| if c.is_control() && c != '\n' { '?' } else { c }).collect()
}
