//! Daemon entry point. Owner: Opus F.
fn main() {
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
