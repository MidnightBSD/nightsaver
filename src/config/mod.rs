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
            patterns: vec!["clock".into(), "starfield".into(), "matrix".into(), "midnight".into(), "cat".into(), "juggler".into()],
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
        let toml_path = cli.config_path.clone().or_else(toml::default_path);
        let t = match &toml_path {
            Some(p) => toml::load(p)?,
            None => None,
        };
        // An explicitly requested config file must exist.
        if t.is_none() && let Some(p) = &cli.config_path {
            return Err(ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("config file {} not found", p.display()),
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
        // `clamp` keeps NaN; a non-finite value falls back to the default.
        let dflt = Config::default().oled;
        if !self.oled.max_brightness.is_finite() {
            self.oled.max_brightness = dflt.max_brightness;
        }
        if !self.oled.lit_budget.is_finite() {
            self.oled.lit_budget = dflt.lit_budget;
        }
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
        if let Some(p) = self.config_path.clone().or_else(toml::default_path) {
            v.push(p);
        }
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
    // OpenPAM treats a service name containing '/' as a path to a policy file, which would
    // let a config file point authentication at an arbitrary (e.g. pam_permit) policy.
    if service.is_empty() || service.contains('/') || service.contains("..") {
        log::error!("invalid pam_service {service:?}: using {DEFAULT:?}");
        return resolve_pam_service(DEFAULT, dirs);
    }
    let exists = |name: &str| dirs.iter().any(|d| d.join(name).exists());
    if service == DEFAULT && !exists(DEFAULT) && exists(FALLBACK) {
        log::info!("PAM policy for '{DEFAULT}' not found; using '{FALLBACK}'");
        FALLBACK.to_string()
    } else {
        service.to_string()
    }
}

/// Reason a file must not be trusted, or `None` if it is fine. Used by the open-handle
/// check in [`read_trusted`]: a configuration file must be a regular file owned by us (or
/// root) and not writable by group or others, because config selects the PAM service and
/// may disable locking. The same rules gate the helper binary (`lock::helper`) and the
/// throttle state file (`lock::throttle`).
pub(crate) fn untrusted_metadata(md: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    if !md.is_file() {
        Some("not a regular file; ignored".into())
    } else if md.uid() != me && md.uid() != 0 {
        Some(format!("owned by uid {}, not by us; ignored", md.uid()))
    } else if md.mode() & 0o022 != 0 {
        Some("writable by group or others; ignored".into())
    } else {
        None
    }
}

/// Path form of [`untrusted_metadata`] for the tests (`None` also when the path is absent).
#[cfg(test)]
pub(crate) fn untrusted_file(path: &std::path::Path) -> Option<String> {
    let md = std::fs::metadata(path).ok()?;
    untrusted_metadata(&md)
}

/// Read a configuration file without trusting the directory it sits in: the path is opened
/// with `O_NONBLOCK` (a FIFO or device must never block the `open` — a plain open on a
/// writer-less FIFO hangs forever, which a synchronous reload cannot afford), then verified
/// with `fstat` **on the opened handle**, so the trust check applies to the same inode the
/// `read` pulls bytes from even under a concurrent rename or swap, and rejects anything
/// that is not a plain file before a byte is read. A symlink is followed on purpose
/// (dotfile managers link `~/.xscreensaver` and the TOML file into place); the rules then
/// apply to the file it resolves to, which is the one that is read. `Ok(None)` if the path does not exist; `Err`
/// with the failure reason as the message for every other problem.
pub(crate) fn read_trusted(path: &std::path::Path) -> Result<Option<String>, std::io::Error> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if let Some(why) = untrusted_metadata(&f.metadata()?) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, why));
    }
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    Ok(Some(s))
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
    fn pam_service_with_path_characters_is_rejected() {
        let dir = std::env::temp_dir().join(format!("nightsaver-pam2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("xscreensaver"), "").unwrap();
        let dirs = vec![dir.clone()];
        assert_eq!(resolve_pam_service("/etc/pam.d/permissive", &dirs), "xscreensaver");
        assert_eq!(resolve_pam_service("../evil", &dirs), "xscreensaver");
        assert_eq!(resolve_pam_service("", &dirs), "xscreensaver");
        assert_eq!(resolve_pam_service("login", &dirs), "login");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_finite_oled_values_use_defaults() {
        let t = toml::parse_str("[oled]\nmax_brightness = nan\nlit_budget = inf\n").unwrap();
        let c = Config::load_from(&Args::default(), None, Some(t));
        let d = Config::default();
        assert_eq!(c.oled.max_brightness, d.oled.max_brightness);
        assert_eq!(c.oled.lit_budget, d.oled.lit_budget);
    }

    #[test]
    fn group_or_world_writable_config_is_untrusted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nightsaver-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "lock = false\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(untrusted_file(&p).is_none());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(untrusted_file(&p).is_some());
        assert!(toml::load(&p).is_err(), "writable config must be refused, not applied");
        assert!(untrusted_file(&dir.join("missing")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Exclusive, private (0700) scratch directory for on-disk fixtures: created with the
    /// mode itself (not created-then-chmodded), after clearing any leftover of an earlier
    /// run, so no other local session can pre-plant files this test then writes into.
    fn private_dir(tag: &str) -> std::path::PathBuf {
        use std::os::unix::fs::DirBuilderExt as _;
        let dir = std::env::temp_dir().join(format!("ns-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir
    }

    #[test]
    fn read_trusted_returns_absent_as_none() {
        let dir = private_dir("absent");
        assert_eq!(read_trusted(&dir.join("absent.toml")).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symlinked config (dotfile managers) is followed, and the trust rules apply to the
    /// file it resolves to: a good target loads, a group/others-writable one is refused.
    #[test]
    fn symlinked_config_is_followed_and_its_target_checked() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_dir("symlink");
        let target = dir.join("real.toml");
        std::fs::write(&target, "lock = true\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_trusted(&link).unwrap().as_deref(), Some("lock = true\n"));
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o666)).unwrap();
        let err = read_trusted(&link).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err:?}");
        // A dangling link is a missing config, not an error.
        std::fs::remove_file(&target).unwrap();
        assert_eq!(read_trusted(&link).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_is_refused_as_not_regular() {
        let dir = private_dir("dir");
        assert_eq!(untrusted_file(&dir).unwrap(), "not a regular file; ignored");
        assert!(read_trusted(&dir).is_err(), "opening a directory must not return its bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A FIFO at the path must be refused without ever blocking: `open` on a writer-less
    /// FIFO hangs forever, and a blocking `read_trusted` would freeze a synchronous config
    /// reload even while the screen is locked. The check runs on a thread with a bounded
    /// wait so a regression fails loudly instead of hanging the test run.
    #[test]
    fn fifo_config_is_refused_without_blocking() {
        use std::ffi::CString;
        use std::sync::mpsc;
        let dir = private_dir("fifo");
        let path = dir.join("config.toml");
        let c = CString::new(path.to_string_lossy().into_owned()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path inside our private fixture directory;
        // mkfifo only reads it.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo fixture");
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_trusted(&p));
        });
        match rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or_else(|e| panic!("read_trusted blocked on a FIFO (O_NONBLOCK missing?): {e}"))
        {
            Err(err) => {
                assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err:?}");
                assert!(err.to_string().contains("not a regular file"), "{err}");
            }
            Ok(Some(bytes)) => panic!("read a FIFO's bytes: {bytes:?}"),
            Ok(None) => panic!("a FIFO reported as absent"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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
