//! Infinity variant of the `0x78` protocol — newer lights (some SL90 variants,
//! etc.) that embed the 6-byte hardware MAC in every payload. Same `0x78`
//! prefix, different tags. `LEN` counts the MAC + subtag + value bytes.
//!
//! On Linux/Windows the MAC is exactly the BLE peripheral address (no discovery
//! hack needed — see NOTES.md §4).

use super::{gm_byte, with_checksum};

const PREFIX: u8 = 0x78;
const TAG_POWER: u8 = 0x8D;
const SUB_POWER: u8 = 0x81;
const TAG_HSI: u8 = 0x8F;
const SUB_HSI: u8 = 0x86;
const TAG_CCT: u8 = 0x90;
const SUB_CCT: u8 = 0x87;
const DIMMING_CURVE: u8 = 0x04;

/// Power: `78 8D 08 <MAC6> 81 <01 on | 02 off> <ck>`.
///
/// Captured: on `...81 01 27`, off `...81 02 28`. (NeewerLite-Python uses
/// `81 00` for off, but the real capture + Swift use `81 02`; we trust 02.)
pub fn power(mac: [u8; 6], on: bool) -> Vec<u8> {
    let mut f = vec![PREFIX, TAG_POWER, 0x08];
    f.extend_from_slice(&mac);
    f.push(SUB_POWER);
    f.push(if on { 0x01 } else { 0x02 });
    with_checksum(f)
}

/// CCT: `78 90 0B <MAC6> 87 <brr> <cct> <gm+50> 04 <ck>`. Trailing `04` = the
/// dimming-curve type.
pub fn cct(mac: [u8; 6], brr: u8, cct: u8, gm: i8) -> Vec<u8> {
    let mut f = vec![PREFIX, TAG_CCT, 0x0B];
    f.extend_from_slice(&mac);
    f.push(SUB_CCT);
    f.extend_from_slice(&[brr, cct, gm_byte(gm), DIMMING_CURVE]);
    with_checksum(f)
}

/// HSI: `78 8F 0C <MAC6> 86 <hue_lo> <hue_hi> <sat> <brr> 00 <ck>`. Hue 16-bit LE.
pub fn hsi(mac: [u8; 6], hue: u16, sat: u8, brr: u8) -> Vec<u8> {
    let hue = hue.min(360);
    let lo = (hue & 0xFF) as u8;
    let hi = (hue >> 8) as u8;
    let mut f = vec![PREFIX, TAG_HSI, 0x0C];
    f.extend_from_slice(&mac);
    f.push(SUB_HSI);
    f.extend_from_slice(&[lo, hi, sat, brr, 0x00]);
    with_checksum(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02X}", x)).collect::<Vec<_>>().join("")
    }

    const MAC: [u8; 6] = [0xF7, 0xAC, 0x16, 0xF1, 0x58, 0x96];

    #[test]
    fn power_matches_captures() {
        // "788D 08F7 AC16 F158 9681 0127" / "...9681 0228"
        assert_eq!(hex(&power(MAC, true)), "788D08F7AC16F158968101 27".replace(' ', ""));
        assert_eq!(hex(&power(MAC, false)), "788D08F7AC16F1589681 0228".replace(' ', ""));
    }

    #[test]
    fn hsi_matches_capture() {
        // "788F 0CF7 AC16 F158 9686 4500 5C32 0004"
        // hue=0x0045=69, sat=0x5C=92, brr=0x32=50
        assert_eq!(hex(&hsi(MAC, 0x0045, 0x5C, 0x32)), "788F0CF7AC16F158968645005C320004");
    }

    #[test]
    fn cct_structure_and_checksum() {
        // No exact public capture for a brr/cct/gm combo, so verify structure:
        // 78 90 0B <mac6> 87 <brr> <cct> <gm+50> 04 <ck> = 3+6+1+4+1 = 15 bytes.
        let f = cct(MAC, 0x32, 0x38, 0);
        assert_eq!(f.len(), 15);
        assert_eq!(&f[0..3], &[0x78, 0x90, 0x0B]);
        assert_eq!(&f[3..9], &MAC);
        assert_eq!(f[9], 0x87);
        assert_eq!(&f[10..14], &[0x32, 0x38, 50, 0x04]); // brr, cct, gm+50, dimming-curve
        assert_eq!(*f.last().unwrap(), super::super::checksum(&f[..f.len() - 1]));
    }
}
