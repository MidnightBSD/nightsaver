//! Parser for `~/.xscreensaver` (X resource file syntax). Owner: Sonnet C.
//!
//! Lenient like xscreensaver itself: unknown keys are ignored and values that fail to
//! parse are skipped with a `log::warn!`. Key matching is case-insensitive.

use super::ConfigError;
use std::path::PathBuf;
use std::time::Duration;

/// xscreensaver `mode:` resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XMode {
    One,
    Random,
    RandomSame,
    Blank,
    Off,
}

/// One entry of the `programs:` list, reduced to the hack name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    /// First token of the command line (e.g. `maze`).
    pub name: String,
    /// Entry was prefixed with the `-` disabled marker.
    pub disabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct XResources {
    pub timeout: Option<Duration>,
    pub cycle: Option<Duration>,
    pub lock: Option<bool>,
    pub lock_timeout: Option<Duration>,
    pub passwd_timeout: Option<Duration>,
    pub dpms_enabled: Option<bool>,
    pub dpms_standby: Option<Duration>,
    pub dpms_suspend: Option<Duration>,
    pub dpms_off: Option<Duration>,
    /// `dpmsQuickOff` (also accepts `dpmsQuickoffEnabled`, the key xscreensaver 6 ships).
    pub dpms_quick_off: Option<bool>,
    pub mode: Option<XMode>,
    /// 0-based index into the full `programs` list (including disabled entries).
    pub selected: Option<i64>,
    pub programs: Option<Vec<Program>>,
    pub verbose: Option<bool>,
    pub splash: Option<bool>,
    pub fade: Option<bool>,
    pub unfade: Option<bool>,
}

/// Parse `H:MM:SS`, `MM:SS` or plain seconds. Values are clamped to `config::MAX_DURATION`
/// (L4: huge durations overflow `Instant + Duration`).
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut secs: u64 = 0;
    let mut n = 0;
    for part in s.split(':') {
        let v: u64 = part.trim().parse().ok()?;
        secs = secs.saturating_mul(60).saturating_add(v);
        n += 1;
    }
    if n > 3 {
        return None;
    }
    Some(Duration::from_secs(secs).min(super::MAX_DURATION))
}

/// Parse an X resource boolean.
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn parse_mode(s: &str) -> Option<XMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "one" => Some(XMode::One),
        "random" => Some(XMode::Random),
        "random-same" => Some(XMode::RandomSame),
        "blank" => Some(XMode::Blank),
        "off" => Some(XMode::Off),
        _ => None,
    }
}

/// Parse one `programs:` entry such as `-  GL:  "Pretty Name"  hack -root`.
fn parse_program(entry: &str) -> Option<Program> {
    let mut s = entry.trim();
    let mut disabled = false;
    if let Some(r) = s.strip_prefix('-') {
        disabled = true;
        s = r.trim_start();
    }
    // Visual prefix such as `GL:` or `mono:` (a first token ending in ':').
    if let Some(first) = s.split_whitespace().next()
        && first.ends_with(':')
        && first.len() > 1
    {
        s = s[first.len()..].trim_start();
    }
    // Optional quoted display name.
    if let Some(r) = s.strip_prefix('"') {
        s = r.split_once('"')?.1.trim_start();
    }
    let name = s.split_whitespace().next()?;
    Some(Program { name: name.to_string(), disabled })
}

/// Split the joined `programs:` value into entries. xscreensaver separates entries with a
/// literal `\n`; if none appear, each continuation line is one entry.
fn parse_programs(segments: &[String]) -> Vec<Program> {
    let joined = segments.join(" ");
    let entries: Vec<String> = if joined.contains("\\n") {
        joined.split("\\n").map(str::to_string).collect()
    } else {
        segments.to_vec()
    };
    entries.iter().filter_map(|e| parse_program(e)).collect()
}

/// Reduce a resource name (`*timeout`, `xscreensaver.timeout`, `XScreenSaver*timeout`)
/// to the bare lower-case key, or `None` if it belongs to another application/widget.
fn bare_key(name: &str) -> Option<String> {
    let comps: Vec<&str> = name.split(['.', '*']).collect();
    let (last, prefix) = comps.split_last()?;
    if last.is_empty() {
        return None;
    }
    for c in prefix {
        if !c.is_empty() && !c.eq_ignore_ascii_case("xscreensaver") {
            return None;
        }
    }
    Some(last.to_ascii_lowercase())
}

pub fn parse_str(s: &str) -> Result<XResources, ConfigError> {
    // Gather logical lines (backslash-newline joins), keeping segments for `programs`.
    let mut logical: Vec<(usize, Vec<String>)> = Vec::new();
    let mut cur: Option<(usize, Vec<String>)> = None;
    for (i, line) in s.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if let Some((_, segs)) = cur.as_mut() {
            // Continuation of the previous line.
            match line.trim_end().strip_suffix('\\') {
                Some(body) => segs.push(body.trim().to_string()),
                None => {
                    segs.push(line.trim().to_string());
                    logical.push(cur.take().unwrap());
                }
            }
            continue;
        }
        let t = line.trim();
        if t.is_empty() || t.starts_with('!') || t.starts_with('#') {
            continue;
        }
        match t.strip_suffix('\\') {
            Some(body) => cur = Some((i + 1, vec![body.trim().to_string()])),
            None => logical.push((i + 1, vec![t.to_string()])),
        }
    }
    if let Some(c) = cur {
        logical.push(c);
    }

    let mut x = XResources::default();
    for (lineno, segs) in logical {
        let first = &segs[0];
        let Some((name, rest)) = first.split_once(':') else {
            log::debug!("~/.xscreensaver line {lineno}: no ':' found, ignored");
            continue;
        };
        let Some(key) = bare_key(name.trim()) else { continue };
        if key == "programs" {
            let mut v = vec![rest.trim().to_string()];
            v.extend(segs[1..].iter().cloned());
            v.retain(|s| !s.is_empty());
            x.programs = Some(parse_programs(&v));
            continue;
        }
        let value = segs.iter().enumerate().map(|(i, s)| if i == 0 { rest.trim() } else { s.as_str() })
            .collect::<Vec<_>>().join(" ");
        let value = value.trim();
        macro_rules! set {
            ($field:ident, $parsed:expr) => {
                match $parsed {
                    Some(v) => x.$field = Some(v),
                    None => log::warn!("~/.xscreensaver line {lineno}: bad value {value:?} for {key}"),
                }
            };
        }
        match key.as_str() {
            "timeout" => set!(timeout, parse_duration(value)),
            "cycle" => set!(cycle, parse_duration(value)),
            "lock" => set!(lock, parse_bool(value)),
            "locktimeout" => set!(lock_timeout, parse_duration(value)),
            "passwdtimeout" => set!(passwd_timeout, parse_duration(value)),
            "dpmsenabled" => set!(dpms_enabled, parse_bool(value)),
            "dpmsstandby" => set!(dpms_standby, parse_duration(value)),
            "dpmssuspend" => set!(dpms_suspend, parse_duration(value)),
            "dpmsoff" => set!(dpms_off, parse_duration(value)),
            "dpmsquickoff" | "dpmsquickoffenabled" => set!(dpms_quick_off, parse_bool(value)),
            "mode" => set!(mode, parse_mode(value)),
            "selected" => set!(selected, value.parse::<i64>().ok()),
            "verbose" => set!(verbose, parse_bool(value)),
            "splash" => set!(splash, parse_bool(value)),
            "fade" => set!(fade, parse_bool(value)),
            "unfade" => set!(unfade, parse_bool(value)),
            _ => {}
        }
    }
    Ok(x)
}

/// Path of the per-user resource file: `$HOME/.xscreensaver`.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".xscreensaver"))
}

/// Read `$HOME/.xscreensaver`; `Ok(None)` if `$HOME` is unset or the file does not exist.
pub fn load_default() -> Result<Option<XResources>, ConfigError> {
    let Some(p) = default_path() else { return Ok(None) };
    let s = match super::read_trusted(&p) {
        Ok(None) => return Ok(None),
        Ok(Some(s)) => s,
        Err(e) => return Err(ConfigError::Parse(format!("{}: {e}", p.display()))),
    };
    parse_str(&s).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("0:10:00"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("2:00:00"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("10:30"), Some(Duration::from_secs(630)));
        assert_eq!(parse_duration(" 45 "), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("99999999999999999999"), None, "not a u64");
        assert_eq!(parse_duration("18446744073709551615"), Some(crate::config::MAX_DURATION));
        assert_eq!(parse_duration("9999999:59:59"), Some(crate::config::MAX_DURATION));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("a:b"), None);
        assert_eq!(parse_duration("1:2:3:4"), None);
    }

    #[test]
    fn booleans() {
        for t in ["True", "yes", "ON", "1"] {
            assert_eq!(parse_bool(t), Some(true));
        }
        for f in ["False", "No", "off", "0"] {
            assert_eq!(parse_bool(f), Some(false));
        }
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn basic_keys_and_prefixes() {
        let x = parse_str(
            "! comment\n\
             xscreensaver.timeout:\t0:05:00\n\
             *cycle: 7:00\n\
             XScreenSaver.lock: Yes\n\
             *mode:  random-same\n\
             *selected: 3\n\
             *dpmsQuickoffEnabled: True\n\
             *someUnknown: whatever\n\
             *Other.timeout: 0:00:01\n\
             *lockTimeout: bogus\n",
        )
        .unwrap();
        assert_eq!(x.timeout, Some(Duration::from_secs(300)));
        assert_eq!(x.cycle, Some(Duration::from_secs(420)));
        assert_eq!(x.lock, Some(true));
        assert_eq!(x.mode, Some(XMode::RandomSame));
        assert_eq!(x.selected, Some(3));
        assert_eq!(x.dpms_quick_off, Some(true));
        assert_eq!(x.lock_timeout, None);
    }

    #[test]
    fn continuation_on_plain_value() {
        let x = parse_str("*timeout: 0:\\\n10:00\n*lock: True\n").unwrap();
        assert_eq!(x.timeout, Some(Duration::from_secs(600)));
        assert_eq!(x.lock, Some(true));
    }

    #[test]
    fn programs_real_syntax() {
        let src = "*programs:\t\t\t\t\t\t      \\\n\
            \t\t\t\txaos -root -autopilot  \\\n\
            \t\t\t\t  -maxframerate 30    \\n\\\n\
            -\t\t\t\txsnow -bg SkyBlue3  \\n\\\n\
            \t\"XMountains (top)\"\txmountains -b -m  \\n\\\n\
            GL:\t\t\t\tfireflies -root   \\n\\\n\
            -  GL: \t\tsuperquadrics --root \\n\\\n\
            \t\t\t\tmaze --root\t\t\t\t    \\n\\\n\
            \n*timeout: 0:03:00\n";
        let x = parse_str(src).unwrap();
        let names: Vec<(&str, bool)> = x.programs.as_ref().unwrap().iter().map(|p| (p.name.as_str(), p.disabled)).collect();
        assert_eq!(
            names,
            vec![
                ("xaos", false),
                ("xsnow", true),
                ("xmountains", false),
                ("fireflies", false),
                ("superquadrics", true),
                ("maze", false)
            ]
        );
        assert_eq!(x.timeout, Some(Duration::from_secs(180)));
    }

    #[test]
    fn programs_one_per_line_fallback() {
        let x = parse_str("programs: \\\n  clock -root \\\n  - matrix \\\n  starfield\n").unwrap();
        let p = x.programs.unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], Program { name: "clock".into(), disabled: false });
        assert_eq!(p[1], Program { name: "matrix".into(), disabled: true });
        assert_eq!(p[2].name, "starfield");
    }

    #[test]
    fn fixture_parses() {
        let x = parse_str(include_str!("../../tests/fixtures/dot-xscreensaver")).unwrap();
        assert_eq!(x.timeout, Some(Duration::from_secs(300)));
        assert_eq!(x.lock, Some(false));
        assert_eq!(x.mode, Some(XMode::Random));
        assert_eq!(x.dpms_enabled, Some(true));
        assert_eq!(x.dpms_off, Some(Duration::from_secs(4 * 3600)));
        let p = x.programs.unwrap();
        assert_eq!(p.len(), 11);
        assert!(p.iter().any(|p| p.name == "matrix" || p.name == "glmatrix"));
        assert_eq!(p.iter().filter(|p| p.disabled).count(), 2);
    }
}
