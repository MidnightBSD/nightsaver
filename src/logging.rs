//! Minimal `log` backend: stderr, optional append-to-file (`--log` implies verbose). Owner: Sonnet C.
//!
//! Line format: `[HH:MM:SS] nightsaver: msg`. The clock is UTC because converting to local
//! time needs `unsafe` libc calls, which AGENTS.md reserves for PAM/X11 code.

use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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
        Some(p) => Some(open_log_file(p)?),
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

/// Open (or create, mode 0600) the log file without following a symlink or blocking on a
/// non-regular file, and refuse a file that is not a regular file owned by us or that
/// others could write: a pre-created link or world-writable file in a shared directory
/// must not redirect or expose the log. `O_NONBLOCK` makes a FIFO fail the open at once
/// (`ENXIO`, no reader) instead of hanging startup forever; the handle's fstat rejects
/// everything that is not a plain file before the logger ever writes to it.
fn open_log_file(p: &Path) -> Result<File, String> {
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(p)
        .map_err(|e| format!("cannot open log file {}: {e}", p.display()))?;
    let md = f.metadata().map_err(|e| format!("cannot stat log file {}: {e}", p.display()))?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    if !md.is_file() {
        return Err(format!("log file {} is not a regular file", p.display()));
    }
    if md.uid() != me {
        return Err(format!("log file {} is owned by uid {}, not us", p.display(), md.uid()));
    }
    if md.mode() & 0o022 != 0 {
        return Err(format!("log file {} is writable by group or others", p.display()));
    }
    Ok(f)
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

    /// A FIFO at the log path must fail the open at once (`ENXIO`: writer-less FIFO with
    /// `O_WRONLY|O_NONBLOCK`), never hang startup. The check runs on a thread with a
    /// bounded wait so a regression fails loudly instead of wedging the test run.
    #[test]
    fn fifo_log_file_refused_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::fs::DirBuilderExt as _;
        use std::sync::mpsc;
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("ns-logging-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let path = dir.join("nightsaver.log");
        let c = CString::new(path.to_string_lossy().into_owned()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path inside our private fixture directory;
        // mkfifo only reads it.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo fixture");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(open_log_file(&path));
        });
        match rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|e| panic!("open_log_file blocked on a FIFO (O_NONBLOCK missing?): {e}"))
        {
            Err(why) => assert!(why.contains("cannot open log file"), "{why}"),
            Ok(_) => panic!("a writer-less FIFO must not open for writing"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
