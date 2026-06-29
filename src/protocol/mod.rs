//! Neewer BLE command encoding.
//!
//! Every Neewer light (classic `0x78`, Infinity `0x78`+MAC, and Home `0x7A`)
//! shares one GATT service and the SAME checksum: the final byte is the sum of
//! all preceding bytes, masked to 8 bits.
//!
//! This module exposes **verified, pure** encoder primitives — one function per
//! known frame form, each unit-tested byte-for-byte against the captures in
//! `NeewerLite/Docs/*-Protocol.md`. Selecting *which* form a given physical
//! light wants (it varies by model) is the job of the higher-level, capability-
//! aware driver layer, not this module.
//!
//! Ranges used throughout (caller pre-clamps):
//! - `brr` brightness 0..=100  (Home protocol re-encodes this as BCD 0..=1000)
//! - `cct` raw colour-temp value, model-dependent (e.g. 32..=56 = 3200K..5600K)
//! - `gm`  green/magenta -50..=50 (sent on the wire as `gm + 50`, i.e. 0..=100)
//! - `hue` 0..=360, `sat` 0..=100
//!
//! GATT (identical for all Neewer lights):
//! - service     `69400001-B5A3-F393-E0A9-E50E24DCCA99`
//! - write char  `69400002-B5A3-F393-E0A9-E50E24DCCA99`
//! - notify char `69400003-B5A3-F393-E0A9-E50E24DCCA99`

pub mod classic;
pub mod home;
pub mod infinity;

/// BLE service / characteristic UUIDs, shared by every Neewer light.
pub mod uuids {
    pub const SERVICE: &str = "69400001-b5a3-f393-e0a9-e50e24dcca99";
    pub const WRITE_CHAR: &str = "69400002-b5a3-f393-e0a9-e50e24dcca99";
    pub const NOTIFY_CHAR: &str = "69400003-b5a3-f393-e0a9-e50e24dcca99";
}

/// Which control mode a light is currently being driven in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Bi-colour / white: brightness + colour temperature (+ optional GM tint).
    Cct,
    /// Colour: hue + saturation + intensity.
    Hsi,
}

/// The desired output of a single light, in native parameter ranges.
///
/// This is the value the ArtNet→light mapper produces and the per-light BLE
/// actor flushes. Brightness/sat are 0..=100, hue 0..=360, gm -50..=50, cct is a
/// raw model-dependent value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LightState {
    pub power: bool,
    pub mode: Mode,
    pub brightness: u8,
    pub cct: u8,
    pub gm: i8,
    pub hue: u16,
    pub sat: u8,
}

impl Default for LightState {
    fn default() -> Self {
        LightState {
            power: false,
            mode: Mode::Cct,
            brightness: 50,
            cct: 32, // 3200K
            gm: 0,
            hue: 0,
            sat: 0,
        }
    }
}

/// Neewer checksum: low 8 bits of the sum of every byte given.
///
/// `as u8` truncates to the low byte, which is exactly `sum & 0xFF`.
#[inline]
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u32, |acc, &b| acc + b as u32) as u8
}

/// Append the trailing checksum byte to a frame and return it.
#[inline]
pub fn with_checksum(mut bytes: Vec<u8>) -> Vec<u8> {
    let ck = checksum(&bytes);
    bytes.push(ck);
    bytes
}

/// Encode `gm` (-50..=50) as the on-the-wire byte (0..=100).
#[inline]
pub(crate) fn gm_byte(gm: i8) -> u8 {
    (gm.clamp(-50, 50) as i16 + 50) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_matches_documented_power_frames() {
        // 78 81 01 01 -> FB ; 78 81 01 02 -> FC
        assert_eq!(checksum(&[0x78, 0x81, 0x01, 0x01]), 0xFB);
        assert_eq!(checksum(&[0x78, 0x81, 0x01, 0x02]), 0xFC);
        // 7A 0A 01 01 -> 86 ; 7A 0A 01 02 -> 87
        assert_eq!(checksum(&[0x7A, 0x0A, 0x01, 0x01]), 0x86);
        assert_eq!(checksum(&[0x7A, 0x0A, 0x01, 0x02]), 0x87);
    }

    #[test]
    fn checksum_wraps_past_256() {
        // sum = 0x78+0x87+0x05+0x36+0x3E+0x15 = 397 -> 397 & 0xFF = 0x8D
        assert_eq!(checksum(&[0x78, 0x87, 0x05, 0x36, 0x3E, 0x15, 0x00, 0x00]), 0x8D);
    }

    #[test]
    fn gm_byte_offsets_by_fifty() {
        assert_eq!(gm_byte(0), 50);
        assert_eq!(gm_byte(-50), 0);
        assert_eq!(gm_byte(50), 100);
        assert_eq!(gm_byte(-100), 0); // clamps
    }
}
