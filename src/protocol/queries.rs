//! MAC-addressed status **query** frames (battery / state / version / temperature).
//!
//! These ask the light for state; the answer arrives asynchronously on the notify
//! characteristic and is decoded by [`super::replies`]. They are the "Infinity"
//! MAC-addressed reads — the TL120C firmware **drops** the direct `0x80`/`0x85`
//! query opcodes but handles these MAC variants (bengt/verygeeky firmware disasm;
//! NOTES.md §2.1). For a directly-connected light the target MAC is the light's
//! own MAC — it acts as its own group master and answers for itself.
//!
//! Byte-exact from the decompiled `cn.java`:
//! - battery      `78 95 06 <MAC6> <ck>`        (`getBatteryByMac`, cn.java:2794) → reply `0x05`
//! - version      `78 9E 06 <MAC6> <ck>`        (`getDeviceInfoByMac`, cn.java:2847) → reply `0x08`
//! - temperature  `78 B3 06 <MAC6> <ck>`        (`getDeviceTemperatureAndFanMode`, cn.java:2858) → reply `0x12`
//! - state/power  `78 8E 07 <MAC6> 85 <ck>`     (`getInfinityPowerStatus`, cn.java:2877) → reply `0x04`

use super::with_checksum;

const PREFIX: u8 = 0x78;

/// Battery-level query (`0x95`) → reply `0x05`.
pub fn battery(mac: [u8; 6]) -> Vec<u8> {
    let mut f = vec![PREFIX, 0x95, 0x06];
    f.extend_from_slice(&mac);
    with_checksum(f)
}

/// Firmware-version query (`0x9E`) → reply `0x08`.
pub fn version(mac: [u8; 6]) -> Vec<u8> {
    let mut f = vec![PREFIX, 0x9E, 0x06];
    f.extend_from_slice(&mac);
    with_checksum(f)
}

/// Temperature + fan-mode query (`0xB3`) → reply `0x12`.
pub fn temperature(mac: [u8; 6]) -> Vec<u8> {
    let mut f = vec![PREFIX, 0xB3, 0x06];
    f.extend_from_slice(&mac);
    with_checksum(f)
}

/// State / power-status read (`0x8E`, inner sub-op `0x85`) → reply `0x04`.
pub fn state(mac: [u8; 6]) -> Vec<u8> {
    let mut f = vec![PREFIX, 0x8E, 0x07];
    f.extend_from_slice(&mac);
    f.push(0x85);
    with_checksum(f)
}

/// Streamer-support query (`0xC4`) → reply `0x17` (`78 17 07 <MAC6> <supported>`).
///
/// **TL60-specific**: the TL60 answers this live (`streamer_support_query`, bengt/
/// verygeeky `frames.py`; reply decoded there as `R_STREAMER_SUPPORT`), while the
/// TL120C ignores it. The bridge doesn't act on the *value* — it's here purely so a
/// TL60, which does **not** answer the battery/version/state reads the other models
/// do, still produces a notify. That notify is what the per-light actor's liveness
/// probe needs to prove the command path is alive (see `light.rs`).
pub fn streamer_support(mac: [u8; 6]) -> Vec<u8> {
    let mut f = vec![PREFIX, 0xC4, 0x06];
    f.extend_from_slice(&mac);
    with_checksum(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::checksum;

    const MAC: [u8; 6] = [0xCC, 0x8D, 0xBE, 0xBB, 0x25, 0xB0];

    fn assert_mac_frame(f: &[u8], op: u8, len: u8, extra: &[u8]) {
        assert_eq!(f[0], PREFIX);
        assert_eq!(f[1], op);
        assert_eq!(f[2], len);
        assert_eq!(&f[3..9], &MAC);
        assert_eq!(&f[9..f.len() - 1], extra);
        assert_eq!(*f.last().unwrap(), checksum(&f[..f.len() - 1]));
    }

    #[test]
    fn battery_frame() {
        assert_mac_frame(&battery(MAC), 0x95, 0x06, &[]);
        assert_eq!(battery(MAC).len(), 10); // 3 hdr + 6 mac + 1 ck
    }

    #[test]
    fn version_frame() {
        assert_mac_frame(&version(MAC), 0x9E, 0x06, &[]);
    }

    #[test]
    fn temperature_frame() {
        assert_mac_frame(&temperature(MAC), 0xB3, 0x06, &[]);
    }

    #[test]
    fn state_frame() {
        // 0x8E carries an inner 0x85 sub-op, so LEN is 7 (6 MAC + 1) and there's a
        // trailing 0x85 before the checksum.
        assert_mac_frame(&state(MAC), 0x8E, 0x07, &[0x85]);
        assert_eq!(state(MAC).len(), 11);
    }

    #[test]
    fn streamer_support_frame() {
        // 78 C4 06 <MAC6> ck — same shape as the other MAC reads (bengt/verygeeky).
        assert_mac_frame(&streamer_support(MAC), 0xC4, 0x06, &[]);
        assert_eq!(streamer_support(MAC).len(), 10);
    }
}
