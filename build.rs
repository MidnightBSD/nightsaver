//! Add the ports/mports LOCALBASE library directory to the link search path so
//! libxcb, libxkbcommon etc. (installed under /usr/local on *BSD) are found
//! without a RUSTFLAGS override. Override with LOCALBASE=/some/prefix.
//!
//! Also bakes in the directory of the `nightsaver-auth` helper: `$PREFIX/libexec`
//! (PREFIX defaults to LOCALBASE, which defaults to /usr/local). The Makefile passes
//! the same PREFIX it installs to.
fn main() {
    println!("cargo:rerun-if-env-changed=LOCALBASE");
    println!("cargo:rerun-if-env-changed=PREFIX");
    let localbase = std::env::var("LOCALBASE").unwrap_or_else(|_| "/usr/local".to_string());
    let prefix = std::env::var("PREFIX").unwrap_or_else(|_| localbase.clone());
    println!("cargo:rustc-link-search=native={localbase}/lib");
    println!("cargo:rustc-env=NIGHTSAVER_LIBEXEC={prefix}/libexec");
}
