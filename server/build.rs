//! Build script — expose a monotonically-increasing build number via
//! `env!("SCTL_BUILD_NUMBER")`.
//!
//! Resolution order:
//!   1. `SCTL_BUILD_NUMBER` env var (set explicitly by the caller — required
//!      for cross-compile, where the build runs inside a Docker container
//!      that can't see the host's .git directory).
//!   2. `git rev-list --count HEAD` from the source tree.
//!   3. "0" as a last-resort fallback (tarball builds, broken git repo).
//!
//! For cross builds, the caller should do:
//!   SCTL_BUILD_NUMBER=$(git rev-list --count HEAD) cross build --release ...

use std::process::Command;

fn main() {
    // Re-run when the env var or git state changes.
    println!("cargo:rerun-if-env-changed=SCTL_BUILD_NUMBER");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
    println!("cargo:rerun-if-changed=../.git/index");

    if let Ok(n) = std::env::var("SCTL_BUILD_NUMBER") {
        let n = n.trim();
        if !n.is_empty() {
            println!("cargo:rustc-env=SCTL_BUILD_NUMBER={n}");
            return;
        }
    }

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
}
