//! Reverse tunnel for CGNAT devices.
//!
//! Provides two modes:
//!
//! - **Relay** (`tunnel.relay = true`): accepts device registrations over WS,
//!   proxies client REST/WS requests to devices via the tunnel connection.
//! - **Client** (`tunnel.url` is set): connects outbound to a relay, handles
//!   proxied requests by calling local route handlers directly.

pub mod client;
pub mod relay;
