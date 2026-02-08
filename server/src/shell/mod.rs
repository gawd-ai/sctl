//! Shell process management.
//!
//! This module provides two modes of shell interaction:
//!
//! - **One-shot** ([`process::exec_command`]) — run a command, capture output, return.
//!   Used by `POST /api/exec` and `POST /api/exec/batch`.
//! - **Interactive** ([`process::spawn_shell`]) — spawn a long-lived shell with piped
//!   stdin/stdout/stderr, used by WebSocket sessions.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub mod process;
pub mod pty;

/// Detect available shells on this system.
///
/// Reads `/etc/shells` first (filtering comments and blank lines), then falls
/// back to probing a hardcoded list of common paths.  Results are deduplicated
/// by canonical path (so `/bin/bash` and `/usr/bin/bash` don't both appear when
/// one is a symlink) and sorted by "elite" rank: zsh > fish > bash > dash > ash > sh.
pub fn detect_shells() -> Vec<String> {
    let candidates = if let Ok(contents) = std::fs::read_to_string("/etc/shells") {
        let from_file: Vec<String> = contents
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter(|l| Path::new(l).exists())
            .map(ToString::to_string)
            .collect();
        if from_file.is_empty() {
            fallback_candidates()
        } else {
            from_file
        }
    } else {
        fallback_candidates()
    };

    // Deduplicate by canonical path (resolves symlinks like /bin/bash ↔ /usr/bin/bash)
    let mut seen = HashSet::new();
    let mut shells: Vec<String> = candidates
        .into_iter()
        .filter(|p| {
            let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
            seen.insert(canonical)
        })
        .collect();

    shells.sort_by_key(|s| shell_rank(s));
    shells
}

fn fallback_candidates() -> Vec<String> {
    [
        "/bin/sh",
        "/bin/bash",
        "/bin/zsh",
        "/bin/ash",
        "/bin/dash",
        "/usr/bin/fish",
        "/usr/bin/zsh",
        "/usr/bin/bash",
    ]
    .iter()
    .filter(|p| Path::new(p).exists())
    .map(|p| (*p).to_string())
    .collect()
}

/// Rank shells from most elite (0) to least (5+).
fn shell_rank(path: &str) -> u8 {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        "zsh" => 0,
        "fish" => 1,
        "bash" => 2,
        "dash" => 3,
        "ash" => 4,
        "sh" => 5,
        _ => 6,
    }
}
