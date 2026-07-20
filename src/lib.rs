//! Neewer-Bridge: an ArtNet (DMX-over-UDP) to Neewer Bluetooth light bridge.
//!
//! The library crate holds the reusable building blocks; the binary (`main.rs`)
//! wires them into a CLI. The protocol encoders are byte-exact against the
//! official apps' reverse-engineered frames — see the `protocol` module docs.

pub mod artnet;
pub mod ble;
pub mod bridge;
pub mod commands;
pub mod config;
pub mod driver;
pub mod light;
pub mod logging;
pub mod merge;
pub mod models;
pub mod profile;
pub mod protocol;
pub mod scan;
