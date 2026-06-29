//! Neewer-Bridge: an ArtNet (DMX-over-UDP) to Neewer Bluetooth light bridge.
//!
//! This library crate holds the protocol-agnostic, hardware-free building blocks
//! (command encoding, light state, ArtNet parsing, mapping) so they can be unit
//! tested without any BLE hardware. The binary (`main.rs`) wires them together
//! with btleplug + tokio.
//!
//! See `NOTES.md` at the repo root for the full design and the reverse-
//! engineering notes that back the protocol module.

pub mod protocol;
