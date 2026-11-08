//! Minimal `log` backend: stderr, optional append-to-file (`--log` implies verbose). Owner: Sonnet C.
//!
//! Line format: `[HH:MM:SS] nightsaver: msg`. The clock is UTC because converting to local
//! time needs `unsafe` libc calls, which AGENTS.md reserves for PAM/X11 code.

use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

struct Logger {
    level: LevelFilter,
    file: Mutex<Option<File>>,
}

/// Map the verbosity count (and whether a log file is in use) to a level filter.
pub fn level_for(verbose: u8, has_log_file: bool) -> LevelFilter {
    let v = if has_log_file { verbose.max(1) } else { verbose };
    match v {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!("[{:02}:{:02}:{:02}]", day / 3600, (day / 60) % 60, day % 60)
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.level
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!("{} nightsaver: {}\n", timestamp(), r.args());
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Ok(mut g) = self.file.lock()
            && let Some(f) = g.as_mut()
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut g) = self.file.lock()
            && let Some(f) = g.as_mut()
        {
            let _ = f.flush();
        }
    }
}

/// Install the global logger. Errors if a logger is already installed or the log file
/// cannot be opened.
pub fn init(verbose: u8, log_file: Option<&Path>) -> Result<(), String> {
    let file = match log_file {
        Some(p) => Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(p)
                .map_err(|e| format!("cannot open log file {}: {e}", p.display()))?,
        ),
        None => None,
    };
    let level = level_for(verbose, file.is_some());
    let logger: &'static Logger = Box::leak(Box::new(Logger {
        level,
        file: Mutex::new(file),
    }));
    log::set_logger(logger).map_err(|e| format!("logger already initialised: {e}"))?;
    log::set_max_level(level);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(level_for(0, false), LevelFilter::Warn);
        assert_eq!(level_for(1, false), LevelFilter::Info);
        assert_eq!(level_for(2, false), LevelFilter::Debug);
        assert_eq!(level_for(4, false), LevelFilter::Trace);
        assert_eq!(level_for(0, true), LevelFilter::Info);
    }

    #[test]
    fn timestamp_shape() {
        let t = timestamp();
        assert_eq!(t.len(), 10);
        assert!(t.starts_with('[') && t.ends_with(']'));
    }
}
