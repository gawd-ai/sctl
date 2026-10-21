//! Streaming SHA-256 hashing utilities.
//!
//! All functions stream data in 64 KiB blocks — never loads a full file into memory.

use sha2::{Digest, Sha256};
use std::io::{self, Read};
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

/// Compute SHA-256 of exactly `size` bytes of `reader`, from where it
/// stands, in 64 KiB blocks. A download hashes the size its stat gave: a
/// file that grows is hashed to that size, and one that ends first is an
/// error, never a hash its chunks cannot match. Blocking.
pub fn hash_exact(reader: &mut impl Read, size: u64) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; BUF_SIZE];
    let mut left = size;
    while left > 0 {
        let want = left.min(BUF_SIZE as u64) as usize;
        let n = match reader.read(&mut buf[..want]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("the file ended {left} bytes before its size"),
                ));
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        left -= n as u64;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_exact_reads_the_size_given_and_no_more() {
        let data = vec![7u8; 200_000];
        let hash = hash_exact(&mut &data[..], 150_000).unwrap();
        assert_eq!(hash, hash_bytes(&data[..150_000]));
        assert_eq!(hash_exact(&mut &data[..], 0).unwrap(), hash_bytes(b""));
    }

    #[test]
    fn hash_exact_refuses_a_source_that_ends_first() {
        let data = vec![7u8; 1000];
        let e = hash_exact(&mut &data[..], 1001).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }
}
