//! Merged configuration. Precedence: CLI > ~/.config/nightsaver/config.toml > ~/.xscreensaver > defaults.
//! Owner: Sonnet C.
pub mod toml;
pub mod xresources;

use self::toml::TomlConfig;
use self::xresources::{XMode, XResources};

use std::path::PathBuf;
use std::time::Duration;

/// Upper bound for every configured duration (L4): keeps `Instant + Duration` far from
/// overflow, which would panic (and a panic while locked would unlock).
pub const MAX_DURATION: Duration = Duration::from_secs(365 * 24 * 3600);
/// Lower bound for `passwd_timeout` (L2): a tiny value would hide the dialog and clear the
/// password between keystrokes, making unlocking impossible.
pub const MIN_PASSWD_TIMEOUT: Duration = Duration::from_secs(5);

/// How the pattern to display is chosen (xscreensaver `mode:` resource).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Always run the single `selected` pattern.
    One,
    /// Random pattern from the list, cycling.
    #[default]
    Random,
    /// Pure black only.
    Blank,
    /// Never activate.
    Off,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DpmsConfig {
    pub enabled: bool,
    pub standby: Duration,
    pub suspend: Duration,
    pub off: Duration,
    /// Power the monitor off immediately on blank (xscreensaver `dpmsQuickOff`).
    pub quick_off: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OledConfig {
    /// 0.0..=1.0 luma cap applied to every frame.
    pub max_brightness: f32,
    pub shift_interval: Duration,
    pub shift_max_px: u16,
    pub rest_interval: Duration,
    pub rest_duration: Duration,
    /// Target fraction of lit pixels (0.0..=1.0) given to patterns as a hint.
    pub lit_budget: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub display: Option<String>,
    pub verbose: u8,
    pub log_file: Option<PathBuf>,
    pub splash: bool,
    /// Idle time before blanking.
    pub timeout: Duration,
    /// Time between pattern changes.
    pub cycle: Duration,
    pub lock: bool,
    /// Time after blanking before the screen becomes locked.
    pub lock_timeout: Duration,
    /// How long the unlock dialog stays up without input.
    pub passwd_timeout: Duration,
    pub pam_service: String,
    pub mode: Mode,
    /// Pattern names in order; `selected` indexes into this (0-based).
    pub patterns: Vec<String>,
    pub selected: usize,
    pub fps: u32,
    pub motion_threshold_px: u32,
    pub dpms: DpmsConfig,
    pub oled: OledConfig,
    /// Explicit `--config` path, if any (used by `watch_paths`).
    pub config_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            display: None,
            verbose: 0,
            log_file: None,
            splash: false,
            timeout: Duration::from_secs(10 * 60),
            cycle: Duration::from_secs(10 * 60),
            lock: false,
            lock_timeout: Duration::ZERO,
            passwd_timeout: Duration::from_secs(30),
            pam_service: "xscreensaver".into(),
            mode: Mode::Random,
            patterns: vec!["clock".into(), "starfield".into(), "matrix".into()],
            selected: 0,
            fps: 30,
            motion_threshold_px: 10,
            dpms: DpmsConfig {
                enabled: false,
                standby: Duration::from_secs(2 * 3600),
                suspend: Duration::from_secs(2 * 3600),
                off: Duration::from_secs(4 * 3600),
                quick_off: false,
            },
            oled: OledConfig {
                max_brightness: 0.35,
                shift_interval: Duration::from_secs(60),
                shift_max_px: 16,
                rest_interval: Duration::from_secs(10 * 60),
                rest_duration: Duration::from_secs(20),
                lit_budget: 0.05,
            },
            config_path: None,
        }
    }
}

impl Config {
    /// Load defaults, overlay ~/.xscreensaver, overlay nightsaver toml, overlay CLI.
    pub fn load(cli: &crate::cli::Args) -> Result<Config, ConfigError> {
        let xres = xresources::load_default()?;
        let toml_path = cli.config_path.clone().unwrap_or_else(toml::default_path);
        let t = toml::load(&toml_path)?;
        // An explicitly requested config file must exist.
        if t.is_none() && cli.config_path.is_some() {
            return Err(ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("config file {} not found", toml_path.display()),
            )));
        }
        Ok(Config::load_from(cli, xres, t))
    }

    /// Pure merge used by `load` and by tests: defaults < xresources < toml < CLI.
    pub fn load_from(args: &crate::cli::Args, xres: Option<XResources>, toml: Option<TomlConfig>) -> Config {
        let mut c = Config { config_path: args.config_path.clone(), ..Config::default() };
        if let Some(x) = xres {
            c.overlay_xres(x);
        }
        if let Some(t) = toml {
            c.overlay_toml(t);
        }
        c.overlay_cli(args);
        c.normalise();
        c
    }

    fn overlay_xres(&mut self, x: XResources) {
        macro_rules! s { ($dst:expr, $src:expr) => { if let Some(v) = $src { $dst = v; } }; }
        s!(self.timeout, x.timeout);
        s!(self.cycle, x.cycle);
        s!(self.lock, x.lock);
        s!(self.lock_timeout, x.lock_timeout);
        s!(self.passwd_timeout, x.passwd_timeout);
        s!(self.dpms.enabled, x.dpms_enabled);
        s!(self.dpms.standby, x.dpms_standby);
        s!(self.dpms.suspend, x.dpms_suspend);
        s!(self.dpms.off, x.dpms_off);
        s!(self.dpms.quick_off, x.dpms_quick_off);
        s!(self.splash, x.splash);
        if let Some(v) = x.verbose {
            self.verbose = u8::from(v);
        }
        if let Some(m) = x.mode {
            self.mode = match m {
                XMode::One => Mode::One,
                XMode::Random | XMode::RandomSame => Mode::Random,
                XMode::Blank => Mode::Blank,
                XMode::Off => Mode::Off,
            };
        }
        // Map the hack list onto the patterns we know. `selected` indexes the full list.
        let mut selected = x.selected.unwrap_or(0).max(0) as usize;
        let mut mapped_selected = None;
        if let Some(progs) = x.programs {
            let mut names: Vec<String> = Vec::new();
            for (i, p) in progs.iter().enumerate() {
                if p.disabled || crate::patterns::by_name(&p.name).is_none() {
                    continue;
                }
                if i == selected {
                    mapped_selected = Some(names.len());
                }
                names.push(p.name.to_ascii_lowercase());
            }
            if !names.is_empty() {
                self.patterns = names;
                selected = mapped_selected.unwrap_or(0);
            }
        }
        self.selected = selected;
    }

    fn overlay_toml(&mut self, t: TomlConfig) {
        macro_rules! s { ($dst:expr, $src:expr) => { if let Some(v) = $src { $dst = v; } }; }
        if t.display.is_some() {
            self.display = t.display;
        }
        s!(self.verbose, t.verbose);
        if t.log_file.is_some() {
            self.log_file = t.log_file;
        }
        s!(self.splash, t.splash);
        s!(self.timeout, t.timeout);
        s!(self.cycle, t.cycle);
        s!(self.lock, t.lock);
        s!(self.lock_timeout, t.lock_timeout);
        s!(self.passwd_timeout, t.passwd_timeout);
        s!(self.pam_service, t.pam_service);
        if let Some(m) = t.mode {
            self.mode = match m.to_ascii_lowercase().as_str() {
                "one" => Mode::One,
                "blank" => Mode::Blank,
                "off" => Mode::Off,
                _ => Mode::Random,
            };
        }
        if let Some(p) = t.patterns.filter(|p| !p.is_empty()) {
            self.patterns = p;
        }
        s!(self.selected, t.selected);
        s!(self.fps, t.fps);
        s!(self.motion_threshold_px, t.motion_threshold_px);
        if let Some(d) = t.dpms {
            s!(self.dpms.enabled, d.enabled);
            s!(self.dpms.standby, d.standby);
            s!(self.dpms.suspend, d.suspend);
            s!(self.dpms.off, d.off);
            s!(self.dpms.quick_off, d.quick_off);
        }
        if let Some(o) = t.oled {
            s!(self.oled.max_brightness, o.max_brightness);
            s!(self.oled.shift_interval, o.shift_interval);
            s!(self.oled.shift_max_px, o.shift_max_px);
            s!(self.oled.rest_interval, o.rest_interval);
            s!(self.oled.rest_duration, o.rest_duration);
            s!(self.oled.lit_budget, o.lit_budget);
        }
    }

    fn overlay_cli(&mut self, a: &crate::cli::Args) {
        if a.display.is_some() {
            self.display = a.display.clone();
        }
        if a.quiet {
            self.verbose = 0;
        }
        if a.verbose > 0 {
            self.verbose = a.verbose;
        }
        if a.debug {
            self.verbose = self.verbose.max(2);
        }
        if a.log_file.is_some() {
            self.log_file = a.log_file.clone();
        }
        if let Some(s) = a.splash {
            self.splash = s;
        }
        if let Some(l) = a.lock {
            self.lock = l;
        }
        if let Some(p) = &a.pattern {
            self.patterns = vec![p.clone()];
            self.mode = Mode::One;
            self.selected = 0;
        }
    }

    /// Clamp values into their valid ranges.
    fn normalise(&mut self) {
        if self.patterns.is_empty() {
            self.patterns = Config::default().patterns;
        }
        self.selected = self.selected.min(self.patterns.len() - 1);
        self.oled.max_brightness = self.oled.max_brightness.clamp(0.0, 1.0);
        self.oled.lit_budget = self.oled.lit_budget.clamp(0.0, 1.0);
        self.fps = self.fps.clamp(1, 120);
        for d in [
            &mut self.timeout,
            &mut self.cycle,
            &mut self.lock_timeout,
            &mut self.passwd_timeout,
            &mut self.dpms.standby,
            &mut self.dpms.suspend,
            &mut self.dpms.off,
            &mut self.oled.shift_interval,
            &mut self.oled.rest_interval,
            &mut self.oled.rest_duration,
        ] {
            *d = (*d).min(MAX_DURATION);
        }
        self.passwd_timeout = self.passwd_timeout.max(MIN_PASSWD_TIMEOUT);
        self.pam_service = resolve_pam_service(&self.pam_service, &pam_policy_dirs());
    }

    /// Config files the daemon should mtime-watch for reload.
    pub fn watch_paths(&self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(p) = xresources::default_path() {
            v.push(p);
        }
        v.push(self.config_path.clone().unwrap_or_else(toml::default_path));
        v
    }
}

/// Directories searched for PAM policies: `/etc/pam.d` and `$LOCALBASE/etc/pam.d`.
fn pam_policy_dirs() -> Vec<PathBuf> {
    let localbase = std::env::var_os("LOCALBASE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local"));
    vec![PathBuf::from("/etc/pam.d"), localbase.join("etc/pam.d")]
}

/// The default service is `xscreensaver` so an installed xscreensaver policy is reused.
/// When that policy is absent (the xscreensaver package was removed) and a `nightsaver`
/// policy exists, fall back to it. Any explicitly configured other name is kept as is.
pub fn resolve_pam_service(service: &str, dirs: &[PathBuf]) -> String {
    const DEFAULT: &str = "xscreensaver";
    const FALLBACK: &str = "nightsaver";
    let exists = |name: &str| dirs.iter().any(|d| d.join(name).exists());
    if service == DEFAULT && !exists(DEFAULT) && exists(FALLBACK) {
        log::info!("PAM policy for '{DEFAULT}' not found; using '{FALLBACK}'");
        FALLBACK.to_string()
    } else {
        service.to_string()
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "io error: {e}"),
            ConfigError::Parse(s) => write!(f, "parse error: {s}"),
        }
    }
}
impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::xresources::{Program, XMode, XResources};

    #[test]
    fn pam_service_falls_back_to_nightsaver_only_when_default_policy_missing() {
        let dir = std::env::temp_dir().join(format!("nightsaver-pam-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = vec![dir.clone()];
        // Neither policy: keep the default name.
        assert_eq!(resolve_pam_service("xscreensaver", &dirs), "xscreensaver");
        std::fs::write(dir.join("nightsaver"), "").unwrap();
        assert_eq!(resolve_pam_service("xscreensaver", &dirs), "nightsaver");
        // Explicit names are never rewritten.
        assert_eq!(resolve_pam_service("login", &dirs), "login");
        std::fs::write(dir.join("xscreensaver"), "").unwrap();
        assert_eq!(resolve_pam_service("xscreensaver", &dirs), "xscreensaver");
        let _ = std::fs::remove_dir_all(&dir);
    }
    use super::*;
    use crate::cli::Args;
    use std::time::Instant;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn defaults_when_nothing_given() {
        let c = Config::load_from(&Args::default(), None, None);
        let mut expected = Config::default();
        // The PAM service depends on which policies exist on the host.
        expected.pam_service = resolve_pam_service(&expected.pam_service, &pam_policy_dirs());
        assert_eq!(c, expected);
    }

    #[test]
    fn xres_overlay_and_programs_mapping() {
        let x = XResources {
            timeout: Some(secs(60)),
            mode: Some(XMode::RandomSame),
            selected: Some(3),
            programs: Some(vec![
                Program { name: "maze".into(), disabled: false },
                Program { name: "matrix".into(), disabled: true },
                Program { name: "clock".into(), disabled: false },
                Program { name: "Starfield".into(), disabled: false },
            ]),
            ..Default::default()
        };
        let c = Config::load_from(&Args::default(), Some(x), None);
        assert_eq!(c.timeout, secs(60));
        assert_eq!(c.mode, Mode::Random);
        assert_eq!(c.patterns, vec!["clock", "starfield"]);
        assert_eq!(c.selected, 1);
    }

    #[test]
    fn unrecognised_programs_keep_defaults_and_clamp() {
        let x = XResources {
            selected: Some(99),
            programs: Some(vec![Program { name: "maze".into(), disabled: false }]),
            ..Default::default()
        };
        let c = Config::load_from(&Args::default(), Some(x), None);
        assert_eq!(c.patterns, Config::default().patterns);
        assert_eq!(c.selected, c.patterns.len() - 1);
        let x = XResources { selected: Some(-1), ..Default::default() };
        assert_eq!(Config::load_from(&Args::default(), Some(x), None).selected, 0);
    }

    #[test]
    fn precedence_cli_over_toml_over_xres() {
        let x = XResources { timeout: Some(secs(60)), lock: Some(true), cycle: Some(secs(5)), ..Default::default() };
        let t = toml::parse_str(r#"timeout = 120
lock = false
verbose = 1
display = ":3"
[oled]
max_brightness = 7.0"#).unwrap();
        let a = Args {
            display: Some(":9".into()),
            lock: Some(true),
            verbose: 3,
            pattern: Some("matrix".into()),
            ..Default::default()
        };
        let c = Config::load_from(&a, Some(x), Some(t));
        assert_eq!(c.timeout, secs(120)); // toml over xres
        assert_eq!(c.cycle, secs(5)); // xres survives
        assert!(c.lock); // cli over toml
        assert_eq!(c.display.as_deref(), Some(":9"));
        assert_eq!(c.verbose, 3);
        assert_eq!(c.patterns, vec!["matrix"]);
        assert_eq!(c.mode, Mode::One);
        assert_eq!(c.oled.max_brightness, 1.0); // clamped
    }

    #[test]
    fn debug_and_quiet() {
        let c = Config::load_from(&Args { debug: true, ..Default::default() }, None, None);
        assert_eq!(c.verbose, 2);
        let t = toml::parse_str("verbose = 3").unwrap();
        let c = Config::load_from(&Args { debug: true, ..Default::default() }, None, Some(t));
        assert_eq!(c.verbose, 3);
        let c = Config::load_from(&Args { quiet: true, ..Default::default() }, None, Some(toml::parse_str("verbose = 3").unwrap()));
        assert_eq!(c.verbose, 0);
    }

    #[test]
    fn passwd_timeout_has_floor() {
        for v in [0, 1, 4] {
            let x = XResources { passwd_timeout: Some(secs(v)), ..Default::default() };
            assert_eq!(Config::load_from(&Args::default(), Some(x), None).passwd_timeout, MIN_PASSWD_TIMEOUT);
        }
        let t = toml::parse_str("passwd_timeout = 0").unwrap();
        assert_eq!(Config::load_from(&Args::default(), None, Some(t)).passwd_timeout, MIN_PASSWD_TIMEOUT);
        let x = XResources { passwd_timeout: Some(secs(6)), ..Default::default() };
        assert_eq!(Config::load_from(&Args::default(), Some(x), None).passwd_timeout, secs(6));
    }

    #[test]
    fn durations_have_ceiling() {
        let huge = Duration::MAX;
        let x = XResources {
            timeout: Some(huge),
            cycle: Some(huge),
            lock_timeout: Some(huge),
            passwd_timeout: Some(huge),
            dpms_standby: Some(huge),
            dpms_suspend: Some(huge),
            dpms_off: Some(huge),
            ..Default::default()
        };
        let c = Config::load_from(&Args::default(), Some(x), None);
        for d in [c.timeout, c.cycle, c.lock_timeout, c.passwd_timeout, c.dpms.standby, c.dpms.suspend, c.dpms.off] {
            assert_eq!(d, MAX_DURATION);
        }
        let t = toml::parse_str(
            "timeout = 9223372036854775807\n[oled]\nshift_interval = 9223372036854775807\n\
             rest_interval = \"99999999:00:00\"\nrest_duration = 9223372036854775807",
        )
        .unwrap();
        let c = Config::load_from(&Args::default(), None, Some(t));
        assert_eq!(c.timeout, MAX_DURATION);
        assert_eq!(c.oled.shift_interval, MAX_DURATION);
        assert_eq!(c.oled.rest_interval, MAX_DURATION);
        assert_eq!(c.oled.rest_duration, MAX_DURATION);
        // Overlay bypassing the parsers (e.g. constructed TomlConfig) is clamped in normalise.
        let t = toml::TomlConfig { cycle: Some(huge), ..Default::default() };
        assert_eq!(Config::load_from(&Args::default(), None, Some(t)).cycle, MAX_DURATION);
        // lock_timeout = 0 stays valid; untouched values keep their defaults.
        let c = Config::load_from(&Args::default(), None, None);
        assert_eq!(c.lock_timeout, Duration::ZERO);
        assert!(Instant::now().checked_add(MAX_DURATION).is_some());
    }

    #[test]
    fn watch_paths_include_explicit_config() {
        let a = Args { config_path: Some("/x/c.toml".into()), ..Default::default() };
        let c = Config::load_from(&a, None, None);
        assert!(c.watch_paths().contains(&PathBuf::from("/x/c.toml")));
    }
}
