//! Add the ports/mports LOCALBASE library directory to the link search path so
//! libxcb, libxkbcommon etc. (installed under /usr/local on *BSD) are found
//! without a RUSTFLAGS override. Override with LOCALBASE=/some/prefix.
fn main() {
    println!("cargo:rerun-if-env-changed=LOCALBASE");
    let localbase = std::env::var("LOCALBASE").unwrap_or_else(|_| "/usr/local".to_string());
    println!("cargo:rustc-link-search=native={localbase}/lib");
}
