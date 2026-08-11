//! Persistent record of TLS certificate fingerprints, keyed by `host:port`.
//!
//! Exists because most LAN management gear presents a self-signed certificate:
//! there is no CA to validate against, so the only meaningful question is
//! "is this the same certificate I was told to expect".
//!
//! Two trust levels, and the distinction is the whole point:
//!
//! * **`configured`** — a fingerprint supplied by the operator, out of band.
//!   Proves authenticity.
//! * **`tofu`** — a fingerprint recorded on first contact. Proves only that
//!   nothing has *changed since* first contact. If an attacker was already in
//!   position when we first looked, this pins the attacker.
//!
//! TOFU is therefore opt-in per request, never a default, and entries record
//! which kind they are so a caller can never mistake one for the other. On a
//! network shared with untrusted hosts — a bus AP that also serves passenger
//! wifi, say — a TOFU pin is close to worthless and should be promoted to a
//! configured one as soon as the fingerprint can be confirmed from elsewhere.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How a pin came to be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinOrigin {
    /// Recorded on first contact. Detects change; does not prove authenticity.
    Tofu,
    /// Supplied by an operator out of band.
    Configured,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinEntry {
    /// Lowercase hex SHA-256 of the DER-encoded end-entity certificate.
    pub sha256: String,
    pub origin: PinOrigin,
    /// Seconds since the Unix epoch when this entry was first written.
    pub first_seen: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PinFile {
    #[serde(default)]
    pins: BTreeMap<String, PinEntry>,
}

/// File-backed pin store. Small enough that it is read and written whole.
#[derive(Debug, Clone)]
pub struct PinStore {
    path: PathBuf,
}

impl PinStore {
    pub fn new(data_dir: &str) -> Self {
        Self {
            path: Path::new(data_dir).join("tls_pins.json"),
        }
    }

    fn load(&self) -> PinFile {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn get(&self, host_port: &str) -> Option<PinEntry> {
        self.load().pins.get(host_port).cloned()
    }

    /// Insert a pin. Returns `false` without writing if one already exists —
    /// silently overwriting is how a TOFU store turns into a rubber stamp for
    /// whatever certificate was presented most recently.
    pub fn insert_if_absent(&self, host_port: &str, sha256: &str, origin: PinOrigin) -> bool {
        let mut file = self.load();
        if file.pins.contains_key(host_port) {
            return false;
        }
        file.pins.insert(
            host_port.to_string(),
            PinEntry {
                sha256: sha256.to_ascii_lowercase(),
                origin,
                first_seen: now_secs(),
            },
        );
        self.write(&file)
    }

    fn write(&self, file: &PinFile) -> bool {
        let Ok(json) = serde_json::to_string_pretty(file) else {
            return false;
        };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write-then-rename: the store is read on every TLS handshake and must
        // never be observed half-written.
        let tmp = self.path.with_extension("json.new");
        if std::fs::write(&tmp, json).is_err() {
            return false;
        }
        std::fs::rename(&tmp, &self.path).is_ok()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Lowercase hex SHA-256 of a DER certificate.
pub fn fingerprint(der: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(der);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Constant-time comparison of two hex fingerprints.
///
/// Timing here is a weak channel, but the comparison is cheap and an attacker
/// who can retry a handshake can sample it repeatedly.
pub fn fingerprints_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= u8::from(!x.eq_ignore_ascii_case(y));
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!("sctl-pin-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn fingerprint_is_lowercase_hex_sha256() {
        // Known vector: SHA-256 of the empty input.
        assert_eq!(
            fingerprint(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn comparison_is_case_insensitive_and_length_checked() {
        assert!(fingerprints_match("AABB", "aabb"));
        assert!(!fingerprints_match("aabb", "aabc"));
        assert!(!fingerprints_match("aabb", "aab"));
    }

    #[test]
    fn round_trips_and_refuses_to_overwrite() {
        let dir = tmp_dir("rt");
        let store = PinStore::new(&dir);
        assert!(store.get("host:443").is_none());

        assert!(store.insert_if_absent("host:443", "AAAA", PinOrigin::Tofu));
        let got = store.get("host:443").expect("pin present");
        assert_eq!(got.sha256, "aaaa", "stored lowercased");
        assert_eq!(got.origin, PinOrigin::Tofu);

        // A second insert must NOT silently replace the first.
        assert!(!store.insert_if_absent("host:443", "BBBB", PinOrigin::Configured));
        assert_eq!(store.get("host:443").unwrap().sha256, "aaaa");
    }

    #[test]
    fn distinct_hosts_and_ports_do_not_collide() {
        let dir = tmp_dir("hosts");
        let store = PinStore::new(&dir);
        store.insert_if_absent("a:443", "1111", PinOrigin::Tofu);
        store.insert_if_absent("a:8443", "2222", PinOrigin::Tofu);
        store.insert_if_absent("b:443", "3333", PinOrigin::Tofu);
        assert_eq!(store.get("a:443").unwrap().sha256, "1111");
        assert_eq!(store.get("a:8443").unwrap().sha256, "2222");
        assert_eq!(store.get("b:443").unwrap().sha256, "3333");
    }

    #[test]
    fn a_corrupt_store_degrades_to_empty_rather_than_failing_closed() {
        let dir = tmp_dir("corrupt");
        std::fs::write(Path::new(&dir).join("tls_pins.json"), "{ not json").unwrap();
        let store = PinStore::new(&dir);
        assert!(store.get("host:443").is_none());
        // ...and remains writable, so a truncated file cannot wedge the feature.
        assert!(store.insert_if_absent("host:443", "cccc", PinOrigin::Tofu));
        assert_eq!(store.get("host:443").unwrap().sha256, "cccc");
    }
}
