//! HTTP route handlers.
//!
//! Each sub-module corresponds to an API endpoint group. All handlers except
//! [`health`] require authentication via the [`crate::auth::require_api_key`]
//! middleware.

pub mod exec;
pub mod files;
pub mod health;
pub mod info;
