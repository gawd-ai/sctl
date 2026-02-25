//! Reverse tunnel for CGNAT devices.
//!
//! Provides two modes:
//!
//! - **Relay** (`tunnel.relay = true`): accepts device registrations over WS,
//!   proxies client REST/WS requests to devices via the tunnel connection.
//! - **Client** (`tunnel.url` is set): connects outbound to a relay, handles
//!   proxied requests by calling local route handlers directly.

use serde_json::Value;

pub mod client;
pub mod relay;

/// A message that can be sent to a device over the tunnel WS.
/// Text for JSON, Binary for file transfer frames.
pub enum TunnelMessage {
    Text(Value),
    Binary(Vec<u8>),
}

/// Response from a tunnel request — either JSON or a binary file frame.
pub enum TunnelResponse {
    Json(Value),
    Binary { header: Value, data: Vec<u8> },
}

/// Encode a binary frame: `[header_len: u32 BE][JSON header][payload]`.
pub fn encode_binary_frame(header: &Value, payload: &[u8]) -> Vec<u8> {
    let header_bytes = serde_json::to_vec(header).expect("Value serializes");
    #[allow(clippy::cast_possible_truncation)]
    let header_len = header_bytes.len() as u32;
    let mut frame = Vec::with_capacity(4 + header_bytes.len() + payload.len());
    frame.extend_from_slice(&header_len.to_be_bytes());
    frame.extend_from_slice(&header_bytes);
    frame.extend_from_slice(payload);
    frame
}

/// Maximum header size (1 MiB) to prevent overflow attacks.
const MAX_BINARY_FRAME_HEADER: usize = 1_048_576;

/// Decode a binary frame. Returns `(header, payload)` or `None` on invalid data.
pub fn decode_binary_frame(data: &[u8]) -> Option<(Value, &[u8])> {
    if data.len() < 4 {
        return None;
    }
    let header_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if header_len > MAX_BINARY_FRAME_HEADER {
        return None;
    }
    let total = 4_usize.checked_add(header_len)?;
    if data.len() < total {
        return None;
    }
    let header: Value = serde_json::from_slice(&data[4..total]).ok()?;
    let payload = &data[total..];
    Some((header, payload))
}
