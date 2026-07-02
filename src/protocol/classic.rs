//! Classic `0x78` protocol — the original Neewer panels (SL-80, RGB660, CB60,
//! RGB62, GL1, etc.). Frame: `[0x78][TAG][LEN][payload...][checksum]`.
//!
//! Brightness is a single byte 0..=100. Hue is 16-bit **little-endian**.
//!
//! CCT has FOUR observed frame forms in the wild (the right one is per-model;
//! that selection lives in the driver layer, not here):
//! - 2-byte `78 87 02 brr cct`            (brightness + temp only)
//! - 3-byte `78 87 03 brr cct gm`         (GL1 family; gm as gm+50)
//! - 4-byte `78 87 04 brr cct gm 00`      (**the exact form NEEWER Studio sends** —
//!   `setRGBLightValue(135,4,…)`, cn.java:996; 4th byte = dimming-curve type; this
//!   is what the GM-CCT driver path uses)
//! - 5-byte `78 87 05 brr cct gm 00 00`   (RGB62 family; gm as gm+50)

use super::{gm_byte, with_checksum};

const PREFIX: u8 = 0x78;
const TAG_POWER: u8 = 0x81;
const TAG_CCT: u8 = 0x87;
const TAG_HSI: u8 = 0x86;
const TAG_XY: u8 = 0xB9;
// MAC-addressed XY variant — what NEEWER Studio sends to `commandType == 2`
// (Infinity) devices like the TL120C, which ignores the direct `0xB9` frame and
// needs this MAC-embedded form (matching how its FX `0x91` and pixel `0xB0`
// frames are also MAC-embedded).
//
// RGBCW (`0xA8` direct / `0xA9` by-MAC) was REMOVED after hardware testing
// (2026-07-01): the TL120C ignores BOTH forms over direct BLE — the app only ever
// relays RGBCW through a 2.4GHz group master, never to the light's own BLE. HSI
// and XY cover colour. See NOTES.md §3.3 / the frames remain in git history.
const TAG_XY_MAC: u8 = 0xB7;
// RGBCW: direct `0xA8` (ignored on the TL120C — probe-only) / by-MAC `0xA9` (works —
// the production frame). See [`rgbcw`] / [`rgbcw_mac`].
const TAG_RGBCW: u8 = 0xA8;
const TAG_RGBCW_MAC: u8 = 0xA9;
// Old 9-scene FX (`0x88`) — the classic simple effect family (squad car, ambulance,
// fire engine, …). Probe-only: dropped by TL120C firmware (§2.1), but non-Infinity
// (`commandType != 2`) fixtures may honour it where the MAC `0x91` engine is absent.
const TAG_SCENE: u8 = 0x88;

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

/// 4-byte CCT: `78 87 04 <brr> <cct> <gm+50> <dimCurve=00>` — the **exact frame
/// NEEWER Studio sends** (`setRGBLightValue(135, 4, {INT, CCT, GM, DimmingCurveType})`,
/// cn.java:996; live-verified on the TL120C by bengt/verygeeky). The 4th byte is the
/// dimming-curve type (0 = standard); the firmware's CCT handler ignores it, but the
/// app always sends it and — critically on the two-chip radio+LED-MCU lights — `LEN`
/// must equal the payload count because the inter-chip UART reframer reassembles by
/// the header, not by ATT boundaries (NOTES.md §2.1). GM is read signed (`sxtb`) but
/// every legal value (0..=100) is <128, so the sign-extension is a no-op (50 = neutral).
/// This is the GM-CCT form the driver uses (was the 5-byte `cct_gm5`).
pub fn cct4(brr: u8, cct: u8, gm: i8) -> Vec<u8> {
    with_checksum(vec![PREFIX, TAG_CCT, 0x04, brr, cct, gm_byte(gm), 0x00])
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

/// CIE xy coordinate (direct): `78 B9 06 <brr> <x_lo> <x_hi> <y_lo> <y_hi> 00`.
/// `x`/`y` are the coordinate ×10000 (0..=8000 = 0.0000..=0.8000), 16-bit
/// little-endian. (`createColorCoordinateBluetoothCommand`, cn.java:1365.)
/// NOTE: `commandType == 2` (Infinity) lights like the TL120C ignore this — use
/// [`xy_mac`].
pub fn xy(brr: u8, x: u16, y: u16) -> Vec<u8> {
    let x = x.min(8000);
    let y = y.min(8000);
    with_checksum(vec![
        PREFIX, TAG_XY, 0x06, brr,
        (x & 0xFF) as u8, (x >> 8) as u8,
        (y & 0xFF) as u8, (y >> 8) as u8,
        0x00,
    ])
}

/// CIE xy coordinate (MAC-addressed, for Infinity lights):
/// `78 B7 0C <MAC6> <brr> <x_lo> <x_hi> <y_lo> <y_hi> <decBrr=0> <ck>`.
/// (`createColorCoordinateCommand`, cn.java:1389 — the form the app sends to
/// `commandType == 2` devices such as the TL120C.)
pub fn xy_mac(mac: [u8; 6], brr: u8, x: u16, y: u16) -> Vec<u8> {
    let x = x.min(8000);
    let y = y.min(8000);
    let mut f = vec![PREFIX, TAG_XY_MAC, 0x0C];
    f.extend_from_slice(&mac);
    f.push(brr);
    f.extend_from_slice(&[
        (x & 0xFF) as u8, (x >> 8) as u8,
        (y & 0xFF) as u8, (y >> 8) as u8,
        0x00,
    ]);
    with_checksum(f)
}

/// Old 9-scene FX (direct): `78 88 02 <brr> <scene 1..=9> <ck>`
/// (NeewerLite `Neewer-Light-Protocol.md`; scene ids 1 squad car, 2 ambulance,
/// 3 fire engine, 4 fireworks, 5 party, 6 candlelight, 7 lightning, 8 paparazzi,
/// 9 TV screen). Probe-only — see [`TAG_SCENE`].
pub fn scene(brr: u8, id: u8) -> Vec<u8> {
    with_checksum(vec![PREFIX, TAG_SCENE, 0x02, brr, id])
}

/// RGBCW direct (`0xA8`) — the app's `createRGBCWCommand` (cn.java:2728), byte-exact:
/// `78 A8 07 <brr> <R> <G> <B> <CW> <WW> <decBrr> <ck>` (7 payload bytes, LEN `0x07`).
/// Args in the app's order: `createRGBCWCommand(brightness, R, G, B, cold, warm,
/// decimalBrightness)` (`RGBCWViewModel:130`).
///
/// ⚠️ **Probe-only — NOT used by the production driver.** The TL120C **ignores** this
/// direct form over BLE (hardware-confirmed 2026-07-01, twice) — same as XY's direct
/// `0xB9`. The driver uses [`rgbcw_mac`] (`0xA9`) instead. This is kept for the
/// `test --set rgbcw:…` probe (to demonstrate the direct form does nothing).
///
/// (The `0` in the decompiled Java array literal is the checksum slot — `cn.checkSum`
/// writes the sum into the final byte — so there is **no** trailing `0x00` payload byte,
/// which is why LEN `0x07` matches the 7 payload bytes exactly.)
pub fn rgbcw(brr: u8, r: u8, g: u8, b: u8, cw: u8, ww: u8, dec_brr: u8) -> Vec<u8> {
    with_checksum(vec![PREFIX, TAG_RGBCW, 0x07, brr, r, g, b, cw, ww, dec_brr])
}

/// RGBCW by-MAC (`0xA9`) — `createRGBCWCommandWithMac` (cn.java:2736):
/// `78 A9 0E <MAC6> A8 <brr> <R> <G> <B> <CW> <WW> <decBrr> <ck>` (LEN `0x0E` = 14 =
/// 6 MAC + inner `A8` + 7 params).
///
/// ✅ **The production RGBCW frame** (driver `Mode::Rgbcw`). Hardware-confirmed on the
/// TL120C (2026-07-01: green→blue tracked) — the MAC-addressed form renders where the
/// direct `0xA8` is ignored, exactly like XY (`0xB7` vs `0xB9`). The earlier "RGBCW
/// ignored, removed" result was a **malformed old frame** (an erroneous trailing `00`
/// made LEN≠payload, which the two-chip UART reframer drops); this byte-exact frame
/// works. `dec_brr` (decimal-brightness fraction) is 0 for whole-percent brightness.
#[allow(clippy::too_many_arguments)] // mirrors the app's RGBCW arg list (matches infinity::fx)
pub fn rgbcw_mac(mac: [u8; 6], brr: u8, r: u8, g: u8, b: u8, cw: u8, ww: u8, dec_brr: u8) -> Vec<u8> {
    let mut f = vec![PREFIX, TAG_RGBCW_MAC, 0x0E];
    f.extend_from_slice(&mac);
    f.extend_from_slice(&[0xA8, brr, r, g, b, cw, ww, dec_brr]);
    with_checksum(f)
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
    fn cct4_matches_app_gm_form() {
        // The app's exact 4-byte GM CCT: 78 87 04 <brr> <cct> <gm+50> 00 <ck>.
        // brr=100(0x64), cct=56(0x38=5600K), gm=0 -> gm byte 0x32(50);
        // sum 0x78+0x87+0x04+0x64+0x38+0x32+0x00 = 0x1D1 -> ck 0xD1.
        assert_eq!(hex(&cct4(100, 56, 0)), "78870464383200D1");
        // Negative GM: gm=-29 -> byte 0x15; sum -> ck 0x8C. LEN stays 0x04.
        assert_eq!(hex(&cct4(0x36, 0x3E, -29)), "788704363E15008C");
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

    #[test]
    fn scene_frame() {
        // 78 88 02 <brr> <scene> <ck>; brr=100(0x64), scene 7 (lightning) ->
        // sum 0x78+0x88+0x02+0x64+0x07 = 0x16D -> ck 0x6D.
        assert_eq!(hex(&scene(100, 7)), "78880264076D");
    }

    #[test]
    fn xy_d65_white_point() {
        // x=0.3127 -> 3127=0x0C37 (lo 37, hi 0C); y=0.3290 -> 3290=0x0CDA (lo DA, hi 0C).
        assert_eq!(hex(&xy(100, 3127, 3290)), "78B90664370CDA0C00C4");
    }

    #[test]
    fn rgbcw_direct_matches_app_builder() {
        // createRGBCWCommand(100,255,0,0,0,0,0) = solid red @ 100%. LEN=0x07 with 7
        // payload bytes (NO trailing 0x00 — the Java literal's 0 is the checksum slot).
        let f = rgbcw(100, 255, 0, 0, 0, 0, 0);
        assert_eq!(&f[..3], &[0x78, 0xA8, 0x07]);
        assert_eq!(&f[3..10], &[100, 255, 0, 0, 0, 0, 0]); // brr,R,G,B,CW,WW,decBrr
        assert_eq!(f.len(), 11);
        assert_eq!(*f.last().unwrap(), super::super::checksum(&f[..f.len() - 1]));
    }

    #[test]
    fn rgbcw_mac_matches_app_builder() {
        // createRGBCWCommandWithMac(...,mac,...) wraps the inner 0xA8 params after the MAC.
        let mac = [0xD6, 0x50, 0xF2, 0xF6, 0xBB, 0x1B];
        let f = rgbcw_mac(mac, 100, 0, 255, 0, 0, 0, 0); // solid green
        assert_eq!(&f[..3], &[0x78, 0xA9, 0x0E]);
        assert_eq!(&f[3..9], &mac);
        assert_eq!(&f[9..17], &[0xA8, 100, 0, 255, 0, 0, 0, 0]); // A8, brr,R,G,B,CW,WW,decBrr
        assert_eq!(f.len(), 18);
        assert_eq!(*f.last().unwrap(), super::super::checksum(&f[..f.len() - 1]));
    }

    #[test]
    fn xy_mac_frame() {
        // 78 B7 0C <MAC6> <brr> <xlo xhi> <ylo yhi> 00 <ck>. len 0x0C = 6+1+2+2+1.
        let mac = [0xD6, 0x50, 0xF2, 0xF6, 0xBB, 0x1B];
        let f = xy_mac(mac, 0x5A, 6400, 3294); // x=0x1900, y=0x0CDE
        assert_eq!(&f[0..3], &[0x78, 0xB7, 0x0C]);
        assert_eq!(&f[3..9], &mac);
        assert_eq!(&f[9..15], &[0x5A, 0x00, 0x19, 0xDE, 0x0C, 0x00]); // brr, xlo,xhi, ylo,yhi, decBrr
        assert_eq!(f.len(), 16);
        assert_eq!(*f.last().unwrap(), super::super::checksum(&f[..f.len() - 1]));
    }
}
