//! Vendor profiles for the `http_api` check kind.
//!
//! A profile knows how to log into one vendor's management API, which
//! endpoints to read, and how to reduce their answers to the structured
//! snapshot other modules consume. The reduction runs HERE, on the device,
//! which is what keeps passenger identifiers (MACs, device names) on the
//! vehicle: only counts leave.

pub mod peplink;
