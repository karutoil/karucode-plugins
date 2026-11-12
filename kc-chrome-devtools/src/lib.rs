//! Library half of the `kc-chrome-devtools` karucode plugin.
//!
//! The binary (`src/main.rs`) wires these modules into the karucode plugin
//! protocol; the library half exists so the pure logic (snapshot rendering,
//! heap-snapshot analysis, value formatting) can be unit-tested directly.

#![allow(dead_code)]

pub mod browser;
pub mod cdp;
pub mod heap;
pub mod snapshot;