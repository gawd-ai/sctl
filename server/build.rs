//! Build script — expose a monotonically-increasing build number via
//! `env!("SCTL_BUILD_NUMBER")`.
//!
//! Uses `git rev-list --count HEAD` so each commit bumps the number.
//! Falls back to "0" outside a git checkout (e.g., from a tarball).

use std::process::Command;

fn main() {
    let build_number = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0".to_string());

    println!("cargo:rustc-env=SCTL_BUILD_NUMBER={build_number}");

    // Re-run when commits change (so incremental builds pick up new commits).
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
    println!("cargo:rerun-if-changed=../.git/index");
}
