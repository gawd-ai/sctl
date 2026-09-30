//! Managed upgrades: the agent replaces itself with a signed release its
//! relay serves, restarts, proves the new agent healthy and rolls back on
//! its own when it is not. `docs/upgrade.md` is the contract.
//!
//! The pieces, in the order an upgrade uses them:
//! - [`install`]: what this box is (`/etc/sctl/install.json`: layout, file
//!   locations, restart command, rollback directory).
//! - [`manifest`]: the signed `release.json` and its verification.
//! - [`fetch`]: HTTP(S) downloads to a file, resumable, over the tunnel's
//!   own TLS trust.
//! - [`stage`]: the running agent's half: refuse early, fetch, verify, prove
//!   the binary runs, write the rollback set, hand off to the helper.
//! - [`apply`]: the detached helper's half: swap, restart, watch health,
//!   roll back.
//! - [`state`]: the one state file both halves write and the tunnel pushes.
//! - [`routes`]: `POST/GET/DELETE /api/upgrade`.

pub mod apply;
pub mod fetch;
pub mod install;
pub mod keys;
pub mod manifest;
pub mod routes;
pub mod stage;
pub mod state;
pub mod version;
pub mod watch;

use serde::{Deserialize, Serialize};

/// The compile target, named the OpenWrt way because every install script
/// and artifact already is. What `release.json` keys `targets` by.
#[cfg(all(target_arch = "mips", target_endian = "big"))]
pub const TARGET: &str = "mips_24kc";
#[cfg(all(target_arch = "mips", target_endian = "little"))]
pub const TARGET: &str = "mipsel_24kc";
#[cfg(target_arch = "x86_64")]
pub const TARGET: &str = "x86_64";
#[cfg(target_arch = "aarch64")]
pub const TARGET: &str = "aarch64";
#[cfg(target_arch = "arm")]
pub const TARGET: &str = "armv7";
#[cfg(target_arch = "riscv64")]
pub const TARGET: &str = "riscv64";
#[cfg(not(any(
    target_arch = "mips",
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "arm",
    target_arch = "riscv64"
)))]
pub const TARGET: &str = "unknown";

/// Why an upgrade was refused or ended as `not_applied`. The wire form is
/// snake_case, in the request's 422 body and in `upgrade.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The request was read outside its `not_before`..`not_after` window.
    WindowPassed,
    /// The device's clock is not set, so a window cannot be judged.
    NoClock,
    /// The requested version is lower than the running one.
    Downgrade,
    /// No `/etc/sctl/install.json`: installed before 0.6.7.
    NoInstallJson,
    /// The manifest has no artifact for this target.
    NoArtifactForTarget,
    /// The running version is below the manifest's `min_from_version`.
    BelowMinFromVersion,
    /// No `release.json.sig` beside the manifest.
    ManifestUnsigned,
    /// The signature does not verify against any trusted key.
    ManifestBadSignature,
    /// The manifest is not a manifest, or not for the requested version.
    ManifestInvalid,
    /// A download failed after its retries.
    DownloadFailed,
    /// A file's SHA-256 is not the manifest's.
    Sha256Mismatch,
    /// Not enough free space to stage or to swap.
    NoSpace,
    /// The new binary did not print the expected version on this box.
    BinaryDoesNotRun,
    /// The helper never wrote an outcome; the next request cleared it.
    HelperLost,
    /// The box holds upgrades (`[upgrade] hold` or `/etc/sctl/upgrade-hold`).
    Held,
    /// A ramboot device was asked without a mirror its boot fetcher can reach.
    NoMirror,
    /// Something this code did not foresee; `reason` text says what.
    Internal,
}

impl Reason {
    /// The wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WindowPassed => "window_passed",
            Self::NoClock => "no_clock",
            Self::Downgrade => "downgrade",
            Self::NoInstallJson => "no_install_json",
            Self::NoArtifactForTarget => "no_artifact_for_target",
            Self::BelowMinFromVersion => "below_min_from_version",
            Self::ManifestUnsigned => "manifest_unsigned",
            Self::ManifestBadSignature => "manifest_bad_signature",
            Self::ManifestInvalid => "manifest_invalid",
            Self::DownloadFailed => "download_failed",
            Self::Sha256Mismatch => "sha256_mismatch",
            Self::NoSpace => "no_space",
            Self::BinaryDoesNotRun => "binary_does_not_run",
            Self::HelperLost => "helper_lost",
            Self::Held => "held",
            Self::NoMirror => "no_mirror",
            Self::Internal => "internal",
        }
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A refusal or a failure: the reason and one line for a person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub reason: Reason,
    pub detail: String,
}

impl Refusal {
    pub fn new(reason: Reason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.detail)
    }
}

/// Lowercase hex of `bytes`.
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// The SHA-256 of a file, lowercase hex, read in chunks.
pub fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_one_of_the_manifest_keys() {
        let known = [
            "x86_64",
            "aarch64",
            "armv7",
            "riscv64",
            "mips_24kc",
            "mipsel_24kc",
        ];
        assert!(known.contains(&TARGET), "unknown target {TARGET}");
    }

    #[test]
    fn reasons_round_trip_through_serde_as_snake_case() {
        for reason in [
            Reason::WindowPassed,
            Reason::NoArtifactForTarget,
            Reason::ManifestBadSignature,
            Reason::Held,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("\"{}\"", reason.as_str()));
            let back: Reason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, reason);
        }
    }

    #[test]
    fn sha256_of_a_file_matches_the_known_digest() {
        let dir = std::env::temp_dir().join(format!("sctl-upgrade-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad" // secret-scan: allow
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
