//! Throwaway manual test: blank `:99` for 3 s. Run against Xvfb only.
fn main() {
    if let Err(e) = nightsaver::daemon::demo_blank(Some(":99")) {
        eprintln!("demo_blank failed: {e}");
        std::process::exit(1);
    }
}
