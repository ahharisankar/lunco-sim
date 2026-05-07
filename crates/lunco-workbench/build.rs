//! Embed the current short git hash into the build via the
//! `LUNCO_GIT_HASH` env var so the workbench's Help menu can display
//! it. Falls back to `"unknown"` when not built from a git checkout
//! (e.g. tarball / vendored build).

use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=LUNCO_GIT_HASH={hash}");
    // Re-run on branch/HEAD movement so the embedded hash stays
    // current without a `cargo clean`.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
