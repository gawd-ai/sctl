//! The release signing public keys the agent trusts.
//!
//! The private key is a GitHub Actions secret (`SCTL_RELEASE_SIGNING_KEY`);
//! `scripts/release-sign.sh` signs `release.json` with it. A key rotates over
//! two releases: release N ships the current and the next key here, signed
//! by the current; release N+1 is signed by the next. `[upgrade] trust_keys`
//! in `sctl.toml` adds keys for a bench or a lab, never for the fleet.

use sha2::{Digest, Sha256};

/// Raw ed25519 public keys, current first.
pub const EMBEDDED: &[[u8; 32]] = &[
    // Netage release key, generated 2026-09-30 (key id 7e744b7b).
    [
        0x3f, 0xe8, 0x5c, 0x47, 0x02, 0x98, 0x02, 0x44, 0x75, 0x7d, 0x60, 0xf8, 0xe2, 0xf3, 0x07,
        0x89, 0x67, 0x21, 0x63, 0x11, 0x95, 0x0e, 0xe7, 0x3b, 0x49, 0x9a, 0x2f, 0x7f, 0xac, 0x62,
        0x77, 0x30,
    ],
];

/// A key's id: the first 8 hex characters of the SHA-256 of its raw bytes.
pub fn key_id(key: &[u8; 32]) -> String {
    let digest = Sha256::digest(key);
    super::hex_lower(&digest[..4])
}

/// Parse a 64-hex-character public key.
pub fn parse_hex(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in text.as_bytes().chunks_exact(2).enumerate() {
        let hex = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(hex, 16).ok()?;
    }
    Some(out)
}

/// The embedded keys plus `extra` (hex, as configured); an unparsable extra
/// key is reported, not silently dropped.
pub fn trusted(extra: &[String]) -> Result<Vec<[u8; 32]>, String> {
    let mut keys: Vec<[u8; 32]> = EMBEDDED.to_vec();
    for (i, text) in extra.iter().enumerate() {
        let key = parse_hex(text)
            .ok_or_else(|| format!("upgrade.trust_keys[{i}] is not a 64-hex-character key"))?;
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_key_has_the_id_the_docs_name() {
        assert_eq!(key_id(&EMBEDDED[0]), "7e744b7b");
    }

    #[test]
    fn hex_keys_parse_and_bad_ones_do_not() {
        let hex = super::super::hex_lower(&EMBEDDED[0]);
        assert_eq!(parse_hex(&hex), Some(EMBEDDED[0]));
        assert_eq!(parse_hex(&hex[..62]), None);
        assert_eq!(parse_hex(&format!("zz{}", &hex[2..])), None);
    }

    #[test]
    fn trusted_adds_extra_keys_once_and_reports_a_bad_one() {
        let hex = super::super::hex_lower(&EMBEDDED[0]);
        assert_eq!(
            trusted(std::slice::from_ref(&hex)).unwrap().len(),
            EMBEDDED.len()
        );
        let other = "11".repeat(32);
        assert_eq!(trusted(&[other]).unwrap().len(), EMBEDDED.len() + 1);
        assert!(trusted(&["nope".into()]).is_err());
    }
}
