//! `release.json` and its detached signature `release.json.sig`.
//!
//! The signature is ed25519 over the exact bytes of `release.json`, one
//! line: `ed25519:<key id>:<base64>`. The key id is a hint; every trusted key
//! is tried. Nothing is canonicalized: the bytes CI wrote are the bytes that
//! are signed, served and verified.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::version::Version;
use super::{Reason, Refusal};

/// The manifest format this code reads.
pub const MANIFEST_VERSION: u32 = 1;
/// A manifest larger than this is not a manifest.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// One file of a target's artifact set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactFile {
    /// `server`, `plugin`, `libc`, `libgcc`: which installed file it replaces.
    pub role: String,
    /// The file name in the bundle (no path).
    pub name: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the file as it sits in the bundle.
    pub sha256: String,
    /// Whether the file is gzipped (the gz layouts store it as is).
    #[serde(default)]
    pub gzip: bool,
}

/// A target's artifact set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFiles {
    pub files: Vec<ArtifactFile>,
}

/// `release.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub v: u32,
    /// The full four-part version every artifact prints.
    pub version: String,
    /// `stable` or `candidate`.
    #[serde(default = "default_channel")]
    pub channel: String,
    /// An agent below this refuses the jump.
    #[serde(default)]
    pub min_from_version: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    pub targets: BTreeMap<String, TargetFiles>,
}

fn default_channel() -> String {
    "stable".to_string()
}

/// Why a manifest was not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// No signature, or one that is not `ed25519:<id>:<base64>`.
    Unsigned(String),
    /// The signature verifies against none of the trusted keys.
    BadSignature,
    /// Not a manifest, or one this code cannot use.
    Invalid(String),
}

impl ManifestError {
    /// The refusal this error is, on the wire.
    pub fn refusal(&self) -> Refusal {
        match self {
            Self::Unsigned(detail) => Refusal::new(Reason::ManifestUnsigned, detail.clone()),
            Self::BadSignature => Refusal::new(
                Reason::ManifestBadSignature,
                "release.json.sig verifies against no trusted key",
            ),
            Self::Invalid(detail) => Refusal::new(Reason::ManifestInvalid, detail.clone()),
        }
    }
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsigned(d) => write!(f, "manifest unsigned: {d}"),
            Self::BadSignature => f.write_str("manifest signature does not verify"),
            Self::Invalid(d) => write!(f, "manifest invalid: {d}"),
        }
    }
}

/// Parse one `release.json.sig` line into (key id, signature bytes).
pub fn parse_signature(line: &str) -> Result<(String, Vec<u8>), ManifestError> {
    let line = line.trim();
    let mut parts = line.splitn(3, ':');
    let algo = parts.next().unwrap_or("");
    let key_id = parts.next().unwrap_or("");
    let b64 = parts.next().unwrap_or("");
    if algo != "ed25519" || key_id.is_empty() || b64.is_empty() {
        return Err(ManifestError::Unsigned(
            "release.json.sig is not `ed25519:<key id>:<base64>`".to_string(),
        ));
    }
    let sig = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| ManifestError::Unsigned(format!("release.json.sig base64: {e}")))?;
    if sig.len() != 64 {
        return Err(ManifestError::Unsigned(format!(
            "release.json.sig is {} bytes, not 64",
            sig.len()
        )));
    }
    Ok((key_id.to_string(), sig))
}

/// Verify `bytes` against `signature_line` with `keys`; returns the id of
/// the key that verified.
pub fn verify(
    bytes: &[u8],
    signature_line: &str,
    keys: &[[u8; 32]],
) -> Result<String, ManifestError> {
    let (hinted, sig) = parse_signature(signature_line)?;
    // The hinted key first, then the rest: the hint is only an order.
    let mut ordered: Vec<&[u8; 32]> = keys
        .iter()
        .filter(|k| super::keys::key_id(k) == hinted)
        .collect();
    ordered.extend(keys.iter().filter(|k| super::keys::key_id(k) != hinted));
    for key in ordered {
        let public = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key);
        if public.verify(bytes, &sig).is_ok() {
            return Ok(super::keys::key_id(key));
        }
    }
    Err(ManifestError::BadSignature)
}

/// A bundle file name: no path, no `..`, printable ASCII.
pub fn is_valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

impl Manifest {
    /// Parse the manifest bytes alone (the caller verified the signature, or
    /// is a relay listing what it holds).
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::Invalid(format!(
                "release.json is {} bytes, over {MAX_MANIFEST_BYTES}",
                bytes.len()
            )));
        }
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|e| ManifestError::Invalid(format!("release.json: {e}")))?;
        manifest.check()?;
        Ok(manifest)
    }

    /// Verify the signature, then parse.
    pub fn parse_signed(
        bytes: &[u8],
        signature_line: &str,
        keys: &[[u8; 32]],
    ) -> Result<Self, ManifestError> {
        verify(bytes, signature_line, keys)?;
        Self::parse(bytes)
    }

    fn check(&self) -> Result<(), ManifestError> {
        if self.v != MANIFEST_VERSION {
            return Err(ManifestError::Invalid(format!(
                "release.json v{} is not v{MANIFEST_VERSION}",
                self.v
            )));
        }
        if Version::parse(&self.version).is_none() {
            return Err(ManifestError::Invalid(format!(
                "release.json version '{}' is not a version",
                self.version
            )));
        }
        if let Some(min) = &self.min_from_version {
            if Version::parse(min).is_none() {
                return Err(ManifestError::Invalid(format!(
                    "release.json min_from_version '{min}' is not a version"
                )));
            }
        }
        if self.channel != "stable" && self.channel != "candidate" {
            return Err(ManifestError::Invalid(format!(
                "release.json channel '{}' is not stable or candidate",
                self.channel
            )));
        }
        for (target, files) in &self.targets {
            if !is_valid_file_name(target) {
                return Err(ManifestError::Invalid(format!(
                    "release.json target '{target}' is not a target name"
                )));
            }
            if files.files.is_empty() {
                return Err(ManifestError::Invalid(format!(
                    "release.json target '{target}' lists no file"
                )));
            }
            for file in &files.files {
                if !is_valid_file_name(&file.name) {
                    return Err(ManifestError::Invalid(format!(
                        "release.json file '{}' is not a file name",
                        file.name
                    )));
                }
                if file.sha256.len() != 64 || !file.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(ManifestError::Invalid(format!(
                        "release.json file '{}' has no sha256",
                        file.name
                    )));
                }
                if file.role.is_empty() || file.role.len() > 32 {
                    return Err(ManifestError::Invalid(format!(
                        "release.json file '{}' has no role",
                        file.name
                    )));
                }
            }
            if !files.files.iter().any(|f| f.role == "server") {
                return Err(ManifestError::Invalid(format!(
                    "release.json target '{target}' has no server file"
                )));
            }
        }
        Ok(())
    }

    /// The parsed version.
    pub fn parsed_version(&self) -> Version {
        Version::parse(&self.version).expect("checked at parse")
    }

    /// The files for `target`, if the release was built for it.
    pub fn files_for(&self, target: &str) -> Option<&[ArtifactFile]> {
        self.targets.get(target).map(|t| t.files.as_slice())
    }

    /// Every file name the bundle should hold, `release.json` and its
    /// signature included.
    pub fn expected_files(&self) -> Vec<String> {
        let mut names = vec!["release.json".to_string(), "release.json.sig".to_string()];
        for files in self.targets.values() {
            for f in &files.files {
                if !names.contains(&f.name) {
                    names.push(f.name.clone());
                }
            }
        }
        names
    }

    /// The manifest entry for a bundle file name, if any.
    pub fn file_named(&self, name: &str) -> Option<&ArtifactFile> {
        self.targets
            .values()
            .flat_map(|t| t.files.iter())
            .find(|f| f.name == name)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A signing key for tests: the agent never signs, CI does with openssl,
    //! so this is the only place a private key exists in Rust.

    use base64::Engine as _;
    use ring::signature::KeyPair;

    pub struct TestSigner {
        pair: ring::signature::Ed25519KeyPair,
    }

    impl TestSigner {
        pub fn new() -> Self {
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
            let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
            Self { pair }
        }

        pub fn public_key(&self) -> [u8; 32] {
            let mut key = [0u8; 32];
            key.copy_from_slice(self.pair.public_key().as_ref());
            key
        }

        /// `ed25519:<id>:<base64>` over `bytes`.
        pub fn sign(&self, bytes: &[u8]) -> String {
            let sig = self.pair.sign(bytes);
            format!(
                "ed25519:{}:{}",
                crate::upgrade::keys::key_id(&self.public_key()),
                base64::engine::general_purpose::STANDARD.encode(sig.as_ref())
            )
        }
    }

    /// A manifest with one target and one server file.
    pub fn manifest_json(
        version: &str,
        target: &str,
        file: &str,
        sha256: &str,
        size: u64,
    ) -> String {
        serde_json::json!({
            "v": 1,
            "version": version,
            "channel": "stable",
            "min_from_version": "0.6.7.0",
            "published_at": "2026-10-01T00:00:00Z",
            "notes": "test",
            "targets": { target: { "files": [
                {"role": "server", "name": file, "size": size, "sha256": sha256, "gzip": false}
            ]}}
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{manifest_json, TestSigner};
    use super::*;

    const SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"; // secret-scan: allow

    #[test]
    fn a_signed_manifest_verifies_with_its_key_and_not_another() {
        let signer = TestSigner::new();
        let bytes = manifest_json("0.6.7.431", "x86_64", "sctl-x86_64", SHA, 3);
        let sig = signer.sign(bytes.as_bytes());
        let manifest =
            Manifest::parse_signed(bytes.as_bytes(), &sig, &[signer.public_key()]).unwrap();
        assert_eq!(manifest.version, "0.6.7.431");
        assert_eq!(manifest.files_for("x86_64").unwrap()[0].name, "sctl-x86_64");
        assert!(manifest.files_for("mips_24kc").is_none());

        let other = TestSigner::new();
        assert_eq!(
            Manifest::parse_signed(bytes.as_bytes(), &sig, &[other.public_key()]).unwrap_err(),
            ManifestError::BadSignature
        );
        // The key id is a hint: the right key verifies wherever it is listed.
        assert!(Manifest::parse_signed(
            bytes.as_bytes(),
            &sig,
            &[other.public_key(), signer.public_key()]
        )
        .is_ok());
    }

    #[test]
    fn a_changed_byte_breaks_the_signature() {
        let signer = TestSigner::new();
        let bytes = manifest_json("0.6.7.431", "x86_64", "sctl-x86_64", SHA, 3);
        let sig = signer.sign(bytes.as_bytes());
        let tampered = bytes.replace("0.6.7.431", "0.6.7.432");
        assert_eq!(
            Manifest::parse_signed(tampered.as_bytes(), &sig, &[signer.public_key()]).unwrap_err(),
            ManifestError::BadSignature
        );
    }

    #[test]
    fn a_missing_or_malformed_signature_is_unsigned() {
        let bytes = manifest_json("0.6.7.431", "x86_64", "sctl-x86_64", SHA, 3);
        for line in ["", "minisign:abc", "ed25519:7e744b7b:not-base64!"] {
            assert!(matches!(
                Manifest::parse_signed(bytes.as_bytes(), line, super::super::keys::EMBEDDED),
                Err(ManifestError::Unsigned(_))
            ));
        }
    }

    #[test]
    fn the_manifest_shape_is_checked() {
        let bad = [
            (r#"{"v":2,"version":"0.6.7.1","targets":{}}"#, "v2"),
            (r#"{"v":1,"version":"x","targets":{}}"#, "version"),
            (
                r#"{"v":1,"version":"0.6.7.1","channel":"beta","targets":{}}"#,
                "channel",
            ),
            (
                r#"{"v":1,"version":"0.6.7.1","targets":{"x86_64":{"files":[]}}}"#,
                "no file",
            ),
            (
                r#"{"v":1,"version":"0.6.7.1","targets":{"x86_64":{"files":[{"role":"plugin","name":"p","size":1,"sha256":"ab"}]}}}"#,
                "sha",
            ),
            (
                r#"{"v":1,"version":"0.6.7.1","targets":{"x86_64":{"files":[{"role":"plugin","name":"../p","size":1,"sha256":"aa"}]}}}"#,
                "name",
            ),
        ];
        for (json, what) in bad {
            assert!(
                matches!(
                    Manifest::parse(json.as_bytes()),
                    Err(ManifestError::Invalid(_))
                ),
                "{what} should be invalid"
            );
        }
        let no_server = format!(
            r#"{{"v":1,"version":"0.6.7.1","targets":{{"x86_64":{{"files":[{{"role":"plugin","name":"p.so","size":1,"sha256":"{SHA}"}}]}}}}}}"#
        );
        assert!(matches!(
            Manifest::parse(no_server.as_bytes()),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn expected_files_and_lookup_by_name() {
        let bytes = manifest_json("0.6.7.431", "x86_64", "sctl-x86_64", SHA, 3);
        let m = Manifest::parse(bytes.as_bytes()).unwrap();
        assert_eq!(
            m.expected_files(),
            vec!["release.json", "release.json.sig", "sctl-x86_64"]
        );
        assert_eq!(m.file_named("sctl-x86_64").unwrap().size, 3);
        assert!(m.file_named("nope").is_none());
    }

    #[test]
    fn file_names_are_plain() {
        assert!(is_valid_file_name("sctl-server-mips_24kc.gz"));
        assert!(is_valid_file_name("libgcc_s-mips_24kc.so.1.gz"));
        assert!(!is_valid_file_name("../x"));
        assert!(!is_valid_file_name("a/b"));
        assert!(!is_valid_file_name(""));
        assert!(!is_valid_file_name(".."));
    }
}
