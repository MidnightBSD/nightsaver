//! Parser for `~/.config/nightsaver/config.toml`. Owner: Sonnet C.

use super::ConfigError;
use serde::{Deserialize, Deserializer};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(untagged)]
enum DurRepr {
    Secs(i64),
    Text(String),
}

/// Accepts integer seconds or `"H:MM:SS"` / `"MM:SS"` / `"SECS"` strings. Values are clamped to
/// `config::MAX_DURATION` (L4: huge durations overflow `Instant + Duration`).
fn de_dur<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    use serde::de::Error;
    match Option::<DurRepr>::deserialize(d)? {
        None => Ok(None),
        Some(DurRepr::Secs(n)) => u64::try_from(n)
            .map(|s| Some(Duration::from_secs(s).min(super::MAX_DURATION)))
            .map_err(|_| D::Error::custom("duration must not be negative")),
        Some(DurRepr::Text(s)) => super::xresources::parse_duration(&s)
            .map(Some)
            .ok_or_else(|| D::Error::custom(format!("invalid duration {s:?} (use seconds or H:MM:SS)"))),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct TomlDpms {
    pub enabled: Option<bool>,
    #[serde(deserialize_with = "de_dur")]
    pub standby: Option<Duration>,
    #[serde(deserialize_with = "de_dur")]
    pub suspend: Option<Duration>,
    #[serde(deserialize_with = "de_dur")]
    pub off: Option<Duration>,
    pub quick_off: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct TomlOled {
    pub max_brightness: Option<f32>,
    #[serde(deserialize_with = "de_dur")]
    pub shift_interval: Option<Duration>,
    pub shift_max_px: Option<u16>,
    #[serde(deserialize_with = "de_dur")]
    pub rest_interval: Option<Duration>,
    #[serde(deserialize_with = "de_dur")]
    pub rest_duration: Option<Duration>,
    pub lit_budget: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct TomlConfig {
    pub display: Option<String>,
    pub verbose: Option<u8>,
    pub log_file: Option<PathBuf>,
    pub splash: Option<bool>,
    #[serde(deserialize_with = "de_dur")]
    pub timeout: Option<Duration>,
    #[serde(deserialize_with = "de_dur")]
    pub cycle: Option<Duration>,
    pub lock: Option<bool>,
    #[serde(deserialize_with = "de_dur")]
    pub lock_timeout: Option<Duration>,
    #[serde(deserialize_with = "de_dur")]
    pub passwd_timeout: Option<Duration>,
    pub pam_service: Option<String>,
    /// One of `one`, `random`, `blank`, `off`.
    pub mode: Option<String>,
    pub patterns: Option<Vec<String>>,
    pub selected: Option<usize>,
    pub fps: Option<u32>,
    pub motion_threshold_px: Option<u32>,
    pub dpms: Option<TomlDpms>,
    pub oled: Option<TomlOled>,
}

/// Parse a TOML document and validate enumerated values.
pub fn parse_str(s: &str) -> Result<TomlConfig, ConfigError> {
    let c: TomlConfig = ::toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;
    if let Some(m) = &c.mode
        && !matches!(m.to_ascii_lowercase().as_str(), "one" | "random" | "random-same" | "blank" | "off")
    {
        return Err(ConfigError::Parse(format!("invalid mode {m:?} (one|random|blank|off)")));
    }
    Ok(c)
}

/// Load a config file; `Ok(None)` if it does not exist.
pub fn load(path: &Path) -> Result<Option<TomlConfig>, ConfigError> {
    if let Some(why) = super::untrusted_file(path) {
        return Err(ConfigError::Parse(format!("{}: {why}", path.display())));
    }
    match std::fs::read_to_string(path) {
        Ok(s) => parse_str(&s)
            .map(Some)
            .map_err(|e| ConfigError::Parse(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError::Io(e)),
    }
}

/// `$XDG_CONFIG_HOME/nightsaver/config.toml`, else `$HOME/.config/nightsaver/config.toml`.
/// `None` when neither gives an absolute directory: a relative path would be resolved against
/// the working directory, where another user may have planted a file.
pub fn default_path() -> Option<PathBuf> {
    let nonempty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let base = match nonempty("XDG_CONFIG_HOME") {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(nonempty("HOME")?).join(".config"),
    };
    if !base.is_absolute() {
        return None;
    }
    Some(base.join("nightsaver").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
display = ":1"
verbose = 2
log_file = "/tmp/ns.log"
splash = false
timeout = 300
cycle = "0:15:00"
lock = true
lock_timeout = "1:00"
passwd_timeout = "45"
pam_service = "login"
mode = "one"
patterns = ["clock", "matrix"]
selected = 1
fps = 24
motion_threshold_px = 20

[dpms]
enabled = true
standby = "0:30:00"
suspend = 3600
off = "2:00:00"
quick_off = true

[oled]
max_brightness = 0.5
shift_interval = 30
shift_max_px = 8
rest_interval = "5:00"
rest_duration = 10
lit_budget = 0.02
"#;

    #[test]
    fn sample_document() {
        let c = parse_str(SAMPLE).unwrap();
        assert_eq!(c.display.as_deref(), Some(":1"));
        assert_eq!(c.verbose, Some(2));
        assert_eq!(c.timeout, Some(Duration::from_secs(300)));
        assert_eq!(c.cycle, Some(Duration::from_secs(900)));
        assert_eq!(c.lock_timeout, Some(Duration::from_secs(60)));
        assert_eq!(c.passwd_timeout, Some(Duration::from_secs(45)));
        assert_eq!(c.patterns.as_deref(), Some(&["clock".to_string(), "matrix".to_string()][..]));
        let d = c.dpms.unwrap();
        assert_eq!(d.standby, Some(Duration::from_secs(1800)));
        assert_eq!(d.suspend, Some(Duration::from_secs(3600)));
        assert_eq!(d.quick_off, Some(true));
        let o = c.oled.unwrap();
        assert_eq!(o.rest_interval, Some(Duration::from_secs(300)));
        assert_eq!(o.shift_max_px, Some(8));
        assert_eq!(o.max_brightness, Some(0.5));
    }

    #[test]
    fn empty_and_errors() {
        assert_eq!(parse_str("").unwrap(), TomlConfig::default());
        assert!(parse_str("timeout = -5").is_err());
        assert!(parse_str("timeout = \"soon\"").is_err());
        assert!(parse_str("mode = \"sideways\"").is_err());
        assert!(parse_str("this is not toml").is_err());
    }

    #[test]
    fn missing_file_is_none() {
        assert_eq!(load(Path::new("/nonexistent/nightsaver.toml")).unwrap(), None);
    }

    #[test]
    fn sample_file_parses() {
        // Everything in the shipped sample is commented out, so it must parse as empty.
        let s = include_str!("../../etc/nightsaver.toml.sample");
        let c = parse_str(s).unwrap();
        assert_eq!(
            c,
            TomlConfig { dpms: Some(TomlDpms::default()), oled: Some(TomlOled::default()), ..Default::default() }
        );
    }
}
