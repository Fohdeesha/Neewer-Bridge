//! Neewer Home `0x7A` protocol — the `NH-*` smart-home line (NS/NF/NR/NW/BF rope
//! lights and panels). Same service UUID + same checksum as `0x78`, but a
//! different prefix, different tags, and **BCD brightness 0..=1000** (0.1%).
//!
//! Standard frame `[0x7A][dataId][size][data...][ck]`. Colour commands use a
//! "LongSizePacket" with a 2-byte big-endian size. Hue here is 16-bit
//! **big-endian** (note: classic/infinity use little-endian).
//!
//! Brightness is the native Home range 0..=1000; callers pass it directly so we
//! don't silently throw away the 0.1% precision the protocol allows.

use super::with_checksum;

const PREFIX: u8 = 0x7A;
const ID_POWER: u8 = 0x0A;
const ID_CCT: u8 = 0x0C;
const ID_BRIGHTNESS: u8 = 0x0B;
const ID_COLOR: u8 = 0x0D;

/// Split a Home brightness (0..=1000) into its two pseudo-BCD digit bytes:
/// `(value/10, value%10)`. Firmware decodes as `hi*10 + lo`.
#[inline]
fn bcd(brr1000: u16) -> (u8, u8) {
    let v = brr1000.min(1000);
    ((v / 10) as u8, (v % 10) as u8)
}

/// Power: `7A 0A 01 01 86` (on) / `7A 0A 01 02 87` (off).
pub fn power(on: bool) -> Vec<u8> {
    with_checksum(vec![PREFIX, ID_POWER, 0x01, if on { 0x01 } else { 0x02 }])
}

/// CCT: `7A 0C 06 <brr_hi> <brr_lo> <cct> 00 01 00 <ck>`. `brr1000` = 0..=1000.
pub fn cct(brr1000: u16, cct: u8) -> Vec<u8> {
    let (hi, lo) = bcd(brr1000);
    with_checksum(vec![PREFIX, ID_CCT, 0x06, hi, lo, cct, 0x00, 0x01, 0x00])
}

/// Brightness-only: `7A 0B 03 00 <brr_hi> <brr_lo> <ck>`.
pub fn brightness(brr1000: u16) -> Vec<u8> {
    let (hi, lo) = bcd(brr1000);
    with_checksum(vec![PREFIX, ID_BRIGHTNESS, 0x03, 0x00, hi, lo])
}

/// HSI whole-light solid colour (lightColorType `0x01`), LongSizePacket:
/// `7A 0D 00 0A <brr_hi> <brr_lo> 01 64 <hue_hi> <hue_lo> <sat> 00 01 00 <ck>`.
///
/// **UNVERIFIED:** the doc's worked example has a checksum byte that does not
/// match the stated sum&0xFF algorithm (see memory `protocol-encoding-gotchas`).
/// The structure is believed correct; verify on real NH-* hardware before trust.
pub fn hsi(brr1000: u16, hue: u16, sat: u8) -> Vec<u8> {
    let (bhi, blo) = bcd(brr1000);
    let hue = hue.min(360);
    let hhi = (hue >> 8) as u8;
    let hlo = (hue & 0xFF) as u8;
    const LIGHTNESS: u8 = 0x64; // per-colour relative brightness, typically 100
    with_checksum(vec![
        PREFIX, ID_COLOR, 0x00, 0x0A, // size = 10, big-endian
        bhi, blo, 0x01, LIGHTNESS, hhi, hlo, sat, 0x00, 0x01, 0x00,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02X}", x)).collect::<Vec<_>>().join("")
    }

    #[test]
    fn bcd_digits() {
        assert_eq!(bcd(500), (50, 0));
        assert_eq!(bcd(1000), (100, 0));
        assert_eq!(bcd(2000), (100, 0)); // clamps
    }

    #[test]
    fn power_frames() {
        assert_eq!(hex(&power(true)), "7A0A010186");
        assert_eq!(hex(&power(false)), "7A0A010287");
    }

    #[test]
    fn cct_matches_capture() {
        // Doc: brr 500 (50%), cct 0x20 -> "7A 0C 06 32 00 20 00 01 00 DF"
        assert_eq!(hex(&cct(500, 0x20)), "7A0C0632002000010 0DF".replace(' ', ""));
    }

    #[test]
    fn hsi_structure_self_consistent_checksum() {
        // Do NOT assert against the doc's 0x8C — that example's checksum is wrong.
        // Verify structure + that our trailing checksum is internally consistent.
        let f = hsi(1000, 240, 100);
        assert_eq!(&f[0..4], &[0x7A, 0x0D, 0x00, 0x0A]);
        assert_eq!(f.len(), 15);
        // brr 1000 -> (100, 0); hue 240 big-endian -> 00 F0
        assert_eq!(&f[4..6], &[100, 0]);
        assert_eq!(&f[8..10], &[0x00, 0xF0]);
        assert_eq!(*f.last().unwrap(), super::super::checksum(&f[..f.len() - 1]));
    }
}
