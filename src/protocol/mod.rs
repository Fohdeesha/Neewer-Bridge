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
pub mod pixel;
pub mod queries;
pub mod replies;

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
    /// CIE xy colour-coordinate (+ brightness).
    Xy,
    /// Direct R/G/B + cool-white/warm-white channel mix. Independent LED-channel
    /// control (what HSI/XY can't do). Sent via the **by-MAC** frame (`0xA9`) — the
    /// direct `0xA8` is ignored on the TL120C, same as XY. Hardware-confirmed 2026-07-01.
    Rgbcw,
    /// Built-in effect engine: one of 18 effects with per-effect parameters.
    Fx,
    /// Per-segment PIXEL colour: the tube is split into `seg_count` bands, each
    /// an HSI/CCT/off block. TL-series pixel fixtures only (e.g. TL120C).
    Pixel,
}

/// The desired output of a single light, in native parameter ranges.
///
/// This is the value the ArtNet→light mapper produces and the per-light BLE
/// actor flushes. Brightness/sat are 0..=100, hue 0..=360, gm -50..=50, cct is a
/// raw model-dependent value. The XY/FX fields are only meaningful in their
/// respective `mode`; the CCT/HSI fields (cct/gm/hue/sat) are reused by FX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LightState {
    pub power: bool,
    pub mode: Mode,
    pub brightness: u8,
    pub cct: u8,
    pub gm: i8,
    pub hue: u16,
    pub sat: u8,
    // CIE xy, encoded ×10000 (0..=8000 = 0.0000..=0.8000).
    pub x: u16,
    pub y: u16,
    // RGBCW: raw 0..=255 channel values (R,G,B + cool-white, warm-white). Only
    // meaningful in `Mode::Rgbcw`.
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub cw: u8,
    pub ww: u8,
    // FX: effect id 1..=18, speed 1..=10, a per-effect "extra" byte (ember/sparks
    // 1..=10, cop-car colour 0..=4, fireworks/party mode 0..=2), and a 16-bit
    // second value (loop effects: CCT2 or Hue2).
    pub fx_id: u8,
    pub fx_speed: u8,
    pub fx_extra: u8,
    pub fx_val2: u16,
    // PIXEL: up to 8 per-segment colour blocks (the palette cap) and how many are
    // active. Only meaningful in `Mode::Pixel`. `pixel_speed` (0..=100) drives the
    // effect's motion; 0 = as static as effect 1 allows.
    pub segments: [pixel::Block; 8],
    pub seg_count: u8,
    pub pixel_speed: u8,
    /// Pixel effect id (working over direct BLE on the TL120C): 1 ColorReplacement,
    /// 3 SingleColorMoving, 4 TwoColorMoving, 5 ThreeColorMoving, 7 Fire.
    pub pixel_effect: u8,
    /// Pixel flow direction / fire orientation (0 or 1).
    pub pixel_dir: u8,
}

impl LightState {
    /// The active pixel segments as a `Block` slice (for the pixel encoder).
    pub fn pixel_blocks(&self) -> &[pixel::Block] {
        &self.segments[..(self.seg_count as usize).min(8)]
    }
}

impl LightState {
    /// A concise, human-readable one-line summary for logs, e.g.
    /// `"CCT 5600K @ 50% gm+0"` or `"HSI hue=180 sat=100 @ 75%"`. Only the fields
    /// relevant to the current mode are shown. `cct` is the raw ×100K value.
    pub fn summary(&self) -> String {
        match self.mode {
            Mode::Cct => format!(
                "CCT {}K @ {}% gm{:+}",
                self.cct as u32 * 100,
                self.brightness,
                self.gm
            ),
            Mode::Hsi => {
                format!("HSI hue={} sat={} @ {}%", self.hue, self.sat, self.brightness)
            }
            Mode::Xy => format!("XY x={} y={} @ {}%", self.x, self.y, self.brightness),
            Mode::Rgbcw => format!(
                "RGBCW r={} g={} b={} cw={} ww={} @ {}%",
                self.r, self.g, self.b, self.cw, self.ww, self.brightness
            ),
            Mode::Fx => format!(
                "FX #{} speed={} @ {}%",
                self.fx_id, self.fx_speed, self.brightness
            ),
            Mode::Pixel => format!(
                "PIXEL fx#{} {} seg speed={} @ {}%",
                self.pixel_effect, self.seg_count, self.pixel_speed, self.brightness
            ),
        }
    }
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
            x: 0,
            y: 0,
            r: 0,
            g: 0,
            b: 0,
            cw: 0,
            ww: 0,
            fx_id: 1,
            fx_speed: 5,
            fx_extra: 0,
            fx_val2: 0,
            segments: [pixel::Block::Off; 8],
            seg_count: 0,
            pixel_speed: 0,
            pixel_effect: 1,
            pixel_dir: 1,
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
