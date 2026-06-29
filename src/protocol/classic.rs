//! Classic `0x78` protocol — the original Neewer panels (SL-80, RGB660, CB60,
//! RGB62, GL1, etc.). Frame: `[0x78][TAG][LEN][payload...][checksum]`.
//!
//! Brightness is a single byte 0..=100. Hue is 16-bit **little-endian**.
//!
//! CCT has THREE observed frame forms in the wild (the right one is per-model;
//! that selection lives in the driver layer, not here):
//! - 2-byte `78 87 02 brr cct`            (brightness + temp only)
//! - 3-byte `78 87 03 brr cct gm`         (GL1 family; gm as gm+50)
//! - 5-byte `78 87 05 brr cct gm 00 00`   (RGB62 family; gm as gm+50)

use super::{gm_byte, with_checksum};

const PREFIX: u8 = 0x78;
const TAG_POWER: u8 = 0x81;
const TAG_CCT: u8 = 0x87;
const TAG_HSI: u8 = 0x86;

/// `78 81 01 01 FB`
pub fn power_on() -> Vec<u8> {
    with_checksum(vec![PREFIX, TAG_POWER, 0x01, 0x01])
}

/// `78 81 01 02 FC`
pub fn power_off() -> Vec<u8> {
    with_checksum(vec![PREFIX, TAG_POWER, 0x01, 0x02])
}

pub fn power(on: bool) -> Vec<u8> {
    if on {
        power_on()
    } else {
        power_off()
    }
}

/// 2-byte CCT: `78 87 02 <brr> <cct>` (no GM).
pub fn cct2(brr: u8, cct: u8) -> Vec<u8> {
    with_checksum(vec![PREFIX, TAG_CCT, 0x02, brr, cct])
}

/// 3-byte CCT: `78 87 03 <brr> <cct> <gm+50>` (GL1 family).
pub fn cct3(brr: u8, cct: u8, gm: i8) -> Vec<u8> {
    with_checksum(vec![PREFIX, TAG_CCT, 0x03, brr, cct, gm_byte(gm)])
}

/// 5-byte CCT: `78 87 05 <brr> <cct> <gm+50> 00 00` (RGB62 family).
pub fn cct_gm5(brr: u8, cct: u8, gm: i8) -> Vec<u8> {
    with_checksum(vec![PREFIX, TAG_CCT, 0x05, brr, cct, gm_byte(gm), 0x00, 0x00])
}

/// HSI: `78 86 04 <hue_lo> <hue_hi> <sat> <brr>`. Hue is 16-bit little-endian.
pub fn hsi(hue: u16, sat: u8, brr: u8) -> Vec<u8> {
    let hue = hue.min(360);
    let lo = (hue & 0xFF) as u8;
    let hi = (hue >> 8) as u8;
    with_checksum(vec![PREFIX, TAG_HSI, 0x04, lo, hi, sat, brr])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02X}", x)).collect::<Vec<_>>().join("")
    }

    #[test]
    fn power_frames() {
        assert_eq!(hex(&power_on()), "78810101FB");
        assert_eq!(hex(&power_off()), "78810102FC");
    }

    #[test]
    fn cct5_matches_rgb62_capture() {
        // Doc: "7887 0536 3E15 0000 8D"  (brr=0x36, cct=0x3E, gm byte=0x15)
        // gm byte 0x15 = 21 -> gm = 21 - 50 = -29
        assert_eq!(hex(&cct_gm5(0x36, 0x3E, -29)), "788705363E1500008D");
    }

    #[test]
    fn cct3_matches_gl1_tungsten_capture() {
        // Doc: "7887 0346 2032 9A"  (brr=0x46, cct=0x20, gm byte=0x32=50 -> gm 0)
        assert_eq!(hex(&cct3(0x46, 0x20, 0)), "788703462032 9A".replace(' ', ""));
    }

    #[test]
    fn hsi_matches_rgb62_capture() {
        // Doc: "7886 040C 0132 3273" -> hue_lo=0C hue_hi=01 (hue=0x010C=268), sat=0x32, brr=0x32
        assert_eq!(hex(&hsi(0x010C, 0x32, 0x32)), "7886040C01323273");
    }
}
