//! Lightpanda-backed browser tools for karucode.
//!
//! The binary wires the plugin protocol; the library contains transport,
//! browser lifecycle and pure snapshot/formatting logic so both can be tested.

#![allow(dead_code)]

pub mod browser;
pub mod cdp;
pub mod snapshot;
