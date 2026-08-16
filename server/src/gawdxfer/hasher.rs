//! Streaming SHA-256 hashing utilities.
//!
//! All functions stream data in 64 KiB blocks — never loads a full file into memory.

use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;
use tokio::io::AsyncReadExt;

const BUF_SIZE: usize = 64 * 1024; // 64 KiB

/// Compute SHA-256 of an entire file by streaming. Returns lowercase hex string.
pub async fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Compute SHA-256 of a byte slice. Returns lowercase hex string.
pub fn hash_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Hex-encode a byte slice (replacement for the `hex` crate, to avoid extra deps).
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().fold(
            String::with_capacity(bytes.as_ref().len() * 2),
            |mut s, b| {
                use std::fmt::Write;
                let _ = write!(s, "{b:02x}");
                s
            },
        )
    }
}
