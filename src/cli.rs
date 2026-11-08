//! Daemon command line, accepting xscreensaver's flag spellings. Owner: Sonnet C.
//!
//! Must accept: -display/-dpy/-disp/--display <d>, -verbose/-v/-vv/-vvv/-vvvv/--verbose,
//! -quiet, -no-splash/-nosplash/--no-splash, -splash, -log <file>/--log, -version/-ver/-vers/--version,
//! -help/--help, -debug, -sync/-synch/-synchronize/-synchronise, plus nightsaver's own
//! --lock, --config <toml>, --pattern <name>, --demo-blank (debug: map black windows 3 s and exit).

use clap::{ArgAction, CommandFactory, Parser};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Args {
    pub display: Option<String>,
    pub verbose: u8,
    pub quiet: bool,
    pub splash: Option<bool>,
    pub log_file: Option<PathBuf>,
    pub sync: bool,
    pub debug: bool,
    pub lock: Option<bool>,
    pub config_path: Option<PathBuf>,
    pub pattern: Option<String>,
    pub demo_blank: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "nightsaver",
    about = "OLED-friendly Xorg screensaver and screen locker (xscreensaver compatible)",
    disable_version_flag = true,
    override_usage = "nightsaver [-display host:display.screen] [-verbose] [-quiet] [-no-splash] [-log file] [-debug] [-sync] [--lock] [--config file] [--pattern name]"
)]
struct Raw {
    /// X display to manage
    #[arg(long)]
    display: Option<String>,
    /// Increase verbosity (repeatable)
    #[arg(long, action = ArgAction::Count)]
    verbose: u8,
    /// Suppress informational output
    #[arg(long)]
    quiet: bool,
    /// Show a splash screen at startup (ignored)
    #[arg(long)]
    splash: bool,
    /// Do not show a splash screen
    #[arg(long = "no-splash")]
    no_splash: bool,
    /// Append log messages to this file
    #[arg(long)]
    log: Option<PathBuf>,
    /// Print version and exit
    #[arg(long)]
    version: bool,
    /// Enable debugging output
    #[arg(long)]
    debug: bool,
    /// Run X synchronously
    #[arg(long)]
    sync: bool,
    /// Lock the screen when blanking
    #[arg(long)]
    lock: bool,
    /// Do not lock the screen
    #[arg(long = "no-lock")]
    no_lock: bool,
    /// Path to nightsaver TOML config
    #[arg(long)]
    config: Option<PathBuf>,
    /// Force a single pattern by name
    #[arg(long)]
    pattern: Option<String>,
    /// Debug: map black windows for 3 seconds and exit
    #[arg(long = "demo-blank")]
    demo_blank: bool,
}

const VALUE_FLAGS: &[&str] = &["--display", "--log", "--config", "--pattern"];

/// Rewrite xscreensaver spellings (`-dpy`, `-vvv`, `-nosplash`, ...) into the canonical
/// double-dash forms understood by clap. Unknown arguments pass through untouched.
fn normalize(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut expect_value = false;
    for a in args {
        if expect_value {
            expect_value = false;
            out.push(a);
            continue;
        }
        if !a.starts_with('-') || a == "-" || a == "--" {
            out.push(a);
            continue;
        }
        let stripped = a.trim_start_matches('-');
        let (name, val) = match stripped.split_once('=') {
            Some((n, v)) if a.starts_with("--") => (n, Some(v.to_string())),
            _ => (stripped, None),
        };
        let canon: Option<&str> = match name {
            "display" | "dpy" | "disp" => Some("--display"),
            "verbose" | "v" => Some("--verbose"),
            "quiet" => Some("--quiet"),
            "no-splash" | "nosplash" => Some("--no-splash"),
            "splash" => Some("--splash"),
            "log" => Some("--log"),
            "version" | "ver" | "vers" => Some("--version"),
            "help" | "h" => Some("--help"),
            "debug" => Some("--debug"),
            "sync" | "synch" | "synchronize" | "synchronise" => Some("--sync"),
            "lock" => Some("--lock"),
            "no-lock" => Some("--no-lock"),
            "config" => Some("--config"),
            "pattern" => Some("--pattern"),
            "demo-blank" => Some("--demo-blank"),
            _ => None,
        };
        if let Some(c) = canon {
            match val {
                Some(v) => out.push(format!("{c}={v}")),
                None => {
                    out.push(c.to_string());
                    expect_value = VALUE_FLAGS.contains(&c);
                }
            }
        } else if !a.starts_with("--")
            && name.len() >= 2
            && name.len() <= 4
            && name.bytes().all(|b| b == b'v')
        {
            // -vv, -vvv, -vvvv
            for _ in 0..name.len() {
                out.push("--verbose".to_string());
            }
        } else {
            out.push(a);
        }
    }
    out
}

enum Outcome {
    Run(Args),
    Exit { code: i32, text: String, stderr: bool },
}

fn try_parse<I: IntoIterator<Item = String>>(args: I) -> Outcome {
    let mut it = args.into_iter();
    let argv0 = it.next().unwrap_or_else(|| "nightsaver".into());
    let mut v = vec![argv0];
    v.extend(normalize(it.collect()));
    let raw = match Raw::try_parse_from(v) {
        Ok(r) => r,
        Err(e) => {
            let help = !e.use_stderr();
            return Outcome::Exit {
                code: if help { 0 } else { 1 },
                text: e.render().to_string(),
                stderr: !help,
            };
        }
    };
    if raw.version {
        return Outcome::Exit {
            code: 0,
            text: format!(
                "nightsaver {} (xscreensaver {} compatible)\n",
                crate::VERSION,
                crate::COMPAT_XSCREENSAVER_VERSION
            ),
            stderr: false,
        };
    }
    if raw.lock && raw.no_lock {
        return Outcome::Exit {
            code: 1,
            text: "error: --lock and --no-lock are mutually exclusive\n".into(),
            stderr: true,
        };
    }
    let splash = match (raw.splash, raw.no_splash) {
        (_, true) => Some(false),
        (true, false) => Some(true),
        _ => None,
    };
    let lock = match (raw.lock, raw.no_lock) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    Outcome::Run(Args {
        display: raw.display,
        verbose: raw.verbose,
        quiet: raw.quiet,
        splash,
        log_file: raw.log,
        sync: raw.sync,
        debug: raw.debug,
        lock,
        config_path: raw.config,
        pattern: raw.pattern,
        demo_blank: raw.demo_blank,
    })
}

/// Testable parser. `args[0]` is the program name. `Err` carries the text that
/// `parse()` would print (usage error, help text or version line).
pub fn parse_from<I: IntoIterator<Item = String>>(args: I) -> Result<Args, String> {
    match try_parse(args) {
        Outcome::Run(a) => Ok(a),
        Outcome::Exit { text, .. } => Err(text),
    }
}

/// Parse `std::env::args()`; prints help/version and exits for those flags.
/// Usage errors exit with status 1 like xscreensaver.
pub fn parse() -> Args {
    match try_parse(std::env::args()) {
        Outcome::Run(a) => a,
        Outcome::Exit { code, text, stderr } => {
            if stderr {
                eprint!("{text}");
            } else {
                print!("{text}");
            }
            std::process::exit(code);
        }
    }
}

#[allow(dead_code)]
fn _command() -> clap::Command {
    Raw::command()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Args, String> {
        parse_from(std::iter::once("nightsaver".to_string()).chain(s.split_whitespace().map(String::from)))
    }

    #[test]
    fn defaults() {
        assert_eq!(p("").unwrap(), Args::default());
    }

    #[test]
    fn display_spellings() {
        for f in ["-display", "-dpy", "-disp", "--dpy", "--display"] {
            assert_eq!(p(&format!("{f} :1")).unwrap().display.as_deref(), Some(":1"));
        }
        assert_eq!(p("--display=:2").unwrap().display.as_deref(), Some(":2"));
    }

    #[test]
    fn verbosity() {
        assert_eq!(p("-verbose").unwrap().verbose, 1);
        assert_eq!(p("-v").unwrap().verbose, 1);
        assert_eq!(p("-vv").unwrap().verbose, 2);
        assert_eq!(p("-vvv").unwrap().verbose, 3);
        assert_eq!(p("-vvvv").unwrap().verbose, 4);
        assert_eq!(p("--verbose --verbose").unwrap().verbose, 2);
        assert!(p("-quiet").unwrap().quiet);
    }

    #[test]
    fn splash() {
        assert_eq!(p("-no-splash").unwrap().splash, Some(false));
        assert_eq!(p("-nosplash").unwrap().splash, Some(false));
        assert_eq!(p("--no-splash").unwrap().splash, Some(false));
        assert_eq!(p("-splash").unwrap().splash, Some(true));
    }

    #[test]
    fn misc_flags() {
        let a = p("-log /tmp/x.log -debug -synchronise --lock --config /c.toml --pattern clock --demo-blank").unwrap();
        assert_eq!(a.log_file, Some(PathBuf::from("/tmp/x.log")));
        assert!(a.debug && a.sync && a.demo_blank);
        assert_eq!(a.lock, Some(true));
        assert_eq!(a.config_path, Some(PathBuf::from("/c.toml")));
        assert_eq!(a.pattern.as_deref(), Some("clock"));
        assert_eq!(p("--no-lock").unwrap().lock, Some(false));
    }

    #[test]
    fn version_and_help() {
        for f in ["-version", "-ver", "-vers", "--version"] {
            let t = p(f).unwrap_err();
            assert!(t.starts_with("nightsaver "), "{t}");
            assert!(t.contains("xscreensaver 6.15 compatible"));
        }
        assert!(p("-help").unwrap_err().contains("Usage") || p("-help").unwrap_err().contains("usage"));
    }

    #[test]
    fn unknown_is_error() {
        assert!(p("-bogus").is_err());
        assert!(p("--nope").is_err());
        assert!(p("-lockk").is_err());
    }
}
