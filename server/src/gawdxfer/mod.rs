//! gawdxfer — chunked resumable file transfer protocol.
//!
//! A self-contained module with shared types, streaming SHA-256, and a
//! `TransferManager` that owns transfer lifecycle, temp files, and chunk I/O.
//! Integration layers (HTTP routes, tunnel relay, tunnel client) adapt gawdxfer
//! to their transport.

pub mod hasher;
pub mod manager;
pub mod types;
