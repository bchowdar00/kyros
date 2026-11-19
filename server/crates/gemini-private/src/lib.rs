//! Client primitives for Gemini's **private** prediction-market API.
//!
//! Three modules:
//! - [`auth`]: API-key handling, HMAC-SHA384 signing, REST and WebSocket
//!   authentication headers.
//! - [`wire`]: the authenticated WebSocket JSON shapes (requests and inbound
//!   events), decimals carried as JSON strings.
//! - [`rest`]: a private REST client with a five-path allowlist.
//!
//! Nothing in this crate contacts Gemini on its own; it only computes values
//! and, in [`rest`], sends requests to a base URL the caller supplies.

pub mod auth;
pub mod rest;
pub mod wire;
