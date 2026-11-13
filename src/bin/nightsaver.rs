//! Daemon entry point. Owner: Opus F.
fn main() {
    disable_core_dumps();
    let args = nightsaver::cli::parse();
    let cfg = match nightsaver::config::Config::load(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nightsaver: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = nightsaver::logging::init(cfg.verbose, cfg.log_file.as_deref()) {
        eprintln!("nightsaver: {e}");
    }
    let result = if args.demo_blank {
        nightsaver::daemon::demo_blank(cfg.display.as_deref())
    } else {
        nightsaver::daemon::run(args, cfg)
    };
    if let Err(e) = result {
        eprintln!("nightsaver: {e}");
        std::process::exit(1);
    }
}

/// Disable core dumps: the daemon holds typed passwords (zeroized on submit/hide/fail, but
/// live in memory while typing), and the event loop is designed to survive panics, so a
/// hard crash must not be able to leave password bytes in a core file. The
/// `nightsaver-auth` helper already does the same before it reads its request.
fn disable_core_dumps() {
    let lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `lim` is a valid, fully initialised rlimit; setrlimit only reads it.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &lim) };
    if rc != 0 {
        eprintln!(
            "nightsaver: cannot disable core dumps: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn core_dumps_disabled() {
        super::disable_core_dumps();
        let mut lim = libc::rlimit { rlim_cur: 1, rlim_max: 1 };
        // SAFETY: `lim` is a valid, fully initialised rlimit; getrlimit only writes it.
        let rc = unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut lim) };
        assert_eq!(rc, 0, "getrlimit(RLIMIT_CORE)");
        assert_eq!(lim.rlim_cur, 0, "soft core-dump limit");
        assert_eq!(lim.rlim_max, 0, "hard core-dump limit");
    }
}
