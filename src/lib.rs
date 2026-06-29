//! Neewer-Bridge: an ArtNet (DMX-over-UDP) to Neewer Bluetooth light bridge.
//!
//! The library crate holds the reusable building blocks; the binary (`main.rs`)
//! wires them into a CLI. See `NOTES.md` at the repo root for the full design
//! and the reverse-engineering notes that back the protocol module.

pub mod ble;
pub mod commands;
pub mod config;
pub mod logging;
pub mod protocol;
