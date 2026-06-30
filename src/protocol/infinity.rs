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
// FX (built-in effect engine): EFFECT_MODE_NEW = 0x91, EFFECT_MODE_OLD = 0x8B
// (EffectType.java). Frame: `78 91 <N+7> <MAC6> 8B <effId params...> ck`.
const TAG_FX: u8 = 0x91;
const SUB_FX: u8 = 0x8B;

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

/// Lay out the effect-data array `[effId, params...]` for one of the 18 built-in
/// effects, exactly as `getEffectData` (cn.java:702). Params already in native
/// ranges: `int`/`sat` 0..=100, `cct` raw, `gm` -50..=50, `hue` 0..=360,
/// `speed` 1..=10. `extra` is the effect-specific byte (ember/sparks 1..=10,
/// cop-car colour 0..=4, fireworks/party mode 0..=2, INT-loop sub-mode 0/1) and
/// `val2` the 16-bit second value (HUE-loop Hue2 0..=360, CCT-loop CCT2 raw).
#[allow(clippy::too_many_arguments)]
fn fx_data(id: u8, int: u8, cct: u8, gm: i8, hue: u16, sat: u8, speed: u8, extra: u8, val2: u16) -> Vec<u8> {
    let g = gm_byte(gm);
    let spd = speed.clamp(1, 10);
    let hue = hue.min(360);
    let (hlo, hhi) = ((hue & 0xFF) as u8, (hue >> 8) as u8);
    let h2 = val2.min(360);
    let (h2lo, h2hi) = ((h2 & 0xFF) as u8, (h2 >> 8) as u8);
    let cct2 = val2 as u8; // CCT-loop second temperature (raw, single byte)
    match id {
        1 => vec![1, int, cct, spd],                              // Lightning
        2 | 3 | 6 | 8 => vec![id, int, cct, g, spd],              // Paparazzi/Defective/CCTflash/CCTpulse
        4 => vec![4, int, cct, g, spd, extra.clamp(1, 10)],       // Explosion (extra = sparks)
        5 | 15 => vec![id, 0, int, cct, g, spd],                  // Welding/TVscreen (INTmin=0, INTmax=int)
        7 | 9 => vec![id, int, hlo, hhi, sat, spd],               // HUEflash/HUEpulse
        10 => vec![10, int, extra.min(4), spd],                   // Cop Car (extra = colour 0..4)
        11 => vec![11, 0, int, cct, g, spd, extra.clamp(1, 10)],  // Candlelight (INTmin/max, ember)
        12 => vec![12, int, hlo, hhi, h2lo, h2hi, spd],           // HUE loop (hue1, hue2=val2)
        13 => vec![13, int, cct, cct2, spd],                      // CCT loop (cct1, cct2=val2)
        14 => vec![14, extra.min(1), 0, int, hlo, hhi, cct, spd], // INT loop (sub-mode, min/max, hue, cct)
        16 => vec![16, int, extra.min(2), spd, 5],                // Fireworks (mode, ember fixed mid)
        17 => vec![17, int, extra.min(2), spd],                   // Party (mode)
        18 => vec![18, int],                                      // Music
        _ => vec![1, int, cct, spd],                              // default: Lightning
    }
}

/// Built-in effect (FX) frame: `78 91 <N+7> <MAC6> 8B <effId params...> ck`.
/// `id` selects one of the 18 effects (see `fx_data`); the remaining args are the
/// effect parameters in native ranges (unused ones are ignored per effect).
#[allow(clippy::too_many_arguments)]
pub fn fx(mac: [u8; 6], id: u8, int: u8, cct: u8, gm: i8, hue: u16, sat: u8, speed: u8, extra: u8, val2: u16) -> Vec<u8> {
    let data = fx_data(id, int, cct, gm, hue, sat, speed, extra, val2);
    let mut f = vec![PREFIX, TAG_FX, (data.len() + 7) as u8];
    f.extend_from_slice(&mac);
    f.push(SUB_FX);
    f.extend_from_slice(&data);
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
    fn fx_lightning_frame() {
        // Lightning(1), INT=100(0x64), CCT=0x38(5600K), speed=5. data=[01 64 38 05],
        // len=4+7=0x0B. 78 91 0B <MAC6> 8B 01 64 38 05 <ck>.
        assert_eq!(hex(&fx(MAC, 1, 100, 0x38, 0, 0, 0, 5, 0, 0)), "78910BF7AC16F158968B01643805D9");
    }

    #[test]
    fn fx_hue_pulse_layout() {
        // HUE pulse(9): data = [09 INT hueLo hueHi SAT speed], len=6+7=0x0D.
        let f = fx(MAC, 9, 80, 0, 0, 0x0078 /*120*/, 100, 7, 0, 0);
        assert_eq!(&f[0..3], &[0x78, 0x91, 0x0D]);
        assert_eq!(&f[3..9], &MAC);
        assert_eq!(f[9], 0x8B);
        assert_eq!(&f[10..16], &[9, 80, 0x78, 0x00, 100, 7]); // id,int,hueLo,hueHi,sat,speed
        assert_eq!(*f.last().unwrap(), super::super::checksum(&f[..f.len() - 1]));
    }

    #[test]
    fn fx_cct_loop_uses_val2_as_second_cct() {
        // CCT loop(13): [13 INT CCT1 CCT2 speed]; val2 low byte = CCT2.
        let f = fx(MAC, 13, 100, 0x20, 0, 0, 0, 4, 0, 0x0048);
        assert_eq!(&f[10..15], &[13, 100, 0x20, 0x48, 4]);
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
