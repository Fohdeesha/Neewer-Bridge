//! Decode the status notifications a light pushes on its notify characteristic
//! (`69400003-…`, `onCharacteristicChanged`, `cn.java:128`).
//!
//! Replies use the same `[0x78, opcode, len, payload…, checksum]` shape as commands,
//! but `opcode` is a *reply* code. This maps the codes we understand into a small
//! [`Reply`] enum; unknown/short frames return `None` so the caller can still log the
//! raw hex. Field offsets are byte-exact from the decompiled `cn.java` parser and
//! cross-checked against verygeeky/neewer-lights' `replies.py`.
//!
//! The queries that elicit these are built in [`super::queries`]. Some lights also
//! *volunteer* state (e.g. the TL120C pushes a `0x05` battery frame on connect).

/// A decoded status reply. Only the fields the bridge surfaces are carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Firmware version. `by_mac` distinguishes the direct `0x00` reply (version at
    /// bytes 5–7) from the MAC-addressed `0x08` reply (version at bytes 11–13).
    Version { major: u8, minor: u8, patch: u8, by_mac: bool },
    /// Power status (direct `0x02` reply).
    Power { on: bool },
    /// Device state (MAC `0x04` reply): current mode byte + power.
    State { mode: u8, on: bool },
    /// Battery percentage 0..=100 (`0x05` reply).
    Battery { percent: u8 },
    /// `0x05` reply carrying a non-percentage flag (the TL120C reports `0xF0` here =
    /// running on mains/external power, not a charge level — see PROTOCOL journal).
    ExternalPower { raw: u8 },
    /// Temperature in °C (`0x12` reply; raw byte is `celsius + 50`).
    Temperature { celsius: i16 },
}

impl Reply {
    /// A concise one-line summary for logs, e.g. `"battery 80%"` / `"firmware 2.0.5"`.
    pub fn summary(&self) -> String {
        match self {
            Reply::Version { major, minor, patch, by_mac } => format!(
                "firmware {major}.{minor}.{patch}{}",
                if *by_mac { " (by-MAC)" } else { "" }
            ),
            Reply::Power { on } => format!("power {}", if *on { "on" } else { "off" }),
            Reply::State { mode, on } => {
                format!("state mode={mode} power={}", if *on { "on" } else { "off" })
            }
            Reply::Battery { percent } => format!("battery {percent}%"),
            Reply::ExternalPower { raw } => format!("external/mains power (flag 0x{raw:02x})"),
            Reply::Temperature { celsius } => format!("temperature {celsius}°C"),
        }
    }
}

/// Decode one notification frame. Returns `None` for frames that aren't ours, are
/// too short for their reply code, fail their checksum, or use a code we don't
/// decode (the caller can still surface the raw bytes). All indexing is
/// bounds-checked via `get`, so a MTU-truncated frame degrades to `None` rather
/// than panicking.
///
/// Checksum validation is OPPORTUNISTIC: verified only when the full frame its
/// LEN byte declares (3 header + LEN payload + checksum) is present. Real
/// notifications routinely arrive truncated at the 20-byte default-MTU boundary
/// (the TL120C's 27-byte version reply, for one), and every hardware-proven
/// reference decodes those leniently - the official app's handler and
/// verygeeky's replies.py both read fixed offsets with no checksum check at
/// all. Rejecting truncated frames would break real telemetry; rejecting a
/// COMPLETE frame with a bad sum only drops corrupt data.
pub fn parse(data: &[u8]) -> Option<Reply> {
    if data.len() < 3 || data[0] != 0x78 {
        return None;
    }
    let full = 3 + data[2] as usize + 1;
    if data.len() >= full {
        let sum = data[..full - 1].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        if sum != data[full - 1] {
            return None;
        }
    }
    match data[1] {
        0x00 => {
            let v = data.get(5..8)?;
            Some(Reply::Version { major: v[0], minor: v[1], patch: v[2], by_mac: false })
        }
        0x02 => Some(Reply::Power { on: *data.get(3)? == 1 }),
        0x04 => Some(Reply::State { mode: *data.get(9)?, on: *data.get(10)? == 1 }),
        0x05 => {
            let pct = *data.get(9)?;
            if pct <= 100 {
                Some(Reply::Battery { percent: pct })
            } else {
                Some(Reply::ExternalPower { raw: pct })
            }
        }
        0x08 => {
            let v = data.get(11..14)?;
            Some(Reply::Version { major: v[0], minor: v[1], patch: v[2], by_mac: true })
        }
        0x12 => Some(Reply::Temperature { celsius: (*data.get(9)? as i16) - 50 }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_percentage() {
        // 78 05 08 <MAC6> <pct@9> …  pct=0x50=80.
        let f = [0x78, 0x05, 0x08, 0, 0, 0, 0, 0, 0, 0x50];
        assert_eq!(parse(&f), Some(Reply::Battery { percent: 80 }));
    }

    #[test]
    fn battery_external_flag() {
        // TL120C on mains reports 0xF0 (>100) → not a percentage.
        let f = [0x78, 0x05, 0x08, 0, 0, 0, 0, 0, 0, 0xF0];
        assert_eq!(parse(&f), Some(Reply::ExternalPower { raw: 0xF0 }));
    }

    #[test]
    fn version_by_mac_matches_tl120c_capture() {
        // Real capture: 78 08 17 <MAC6> 01 0a 02 00 05 … → version bytes [11,12,13] = 2.0.5.
        let f = [
            0x78, 0x08, 0x17, 0xCC, 0x8D, 0xBE, 0xBB, 0x25, 0xB0, 0x01, 0x0a, 0x02, 0x00, 0x05,
            0x02, 0x54, 0x4c,
        ];
        assert_eq!(parse(&f), Some(Reply::Version { major: 2, minor: 0, patch: 5, by_mac: true }));
    }

    #[test]
    fn temperature_offset_by_fifty() {
        // raw byte 0x4B (75) at index 9 → 75 - 50 = 25 °C.
        let f = [0x78, 0x12, 0x08, 0, 0, 0, 0, 0, 0, 0x4B];
        assert_eq!(parse(&f), Some(Reply::Temperature { celsius: 25 }));
    }

    #[test]
    fn state_mode_and_power() {
        // 78 04 … mode@9 power@10.
        let f = [0x78, 0x04, 0x0a, 0, 0, 0, 0, 0, 0, 0x02, 0x01];
        assert_eq!(parse(&f), Some(Reply::State { mode: 2, on: true }));
    }

    /// Append the additive checksum to a frame body, completing it per its LEN.
    fn ck(body: &[u8]) -> Vec<u8> {
        let mut f = body.to_vec();
        f.push(body.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
        f
    }

    #[test]
    fn complete_frame_with_valid_checksum_decodes() {
        // 78 05 08 <MAC6> <pct> <spare> <ck> - a full 12-byte battery reply.
        let f = ck(&[0x78, 0x05, 0x08, 1, 2, 3, 4, 5, 6, 0x50, 0x00]);
        assert_eq!(f.len(), 12);
        assert_eq!(parse(&f), Some(Reply::Battery { percent: 80 }));
    }

    #[test]
    fn complete_frame_with_bad_checksum_is_rejected() {
        let mut f = ck(&[0x78, 0x05, 0x08, 1, 2, 3, 4, 5, 6, 0x50, 0x00]);
        let last = f.len() - 1;
        f[last] ^= 0xFF;
        assert_eq!(parse(&f), None, "a corrupt complete frame must not decode");
    }

    #[test]
    fn truncated_frames_still_decode_leniently() {
        // MTU truncation is real on this hardware (default ATT MTU carries 20
        // bytes; the TL120C's version reply is 27) - a frame cut short of its
        // declared LEN cannot be checksum-verified and must still decode, as
        // the app and every reference implementation do.
        let f = [0x78, 0x05, 0x08, 1, 2, 3, 4, 5, 6, 0x50]; // 10 of 12 bytes
        assert_eq!(parse(&f), Some(Reply::Battery { percent: 80 }));
    }

    #[test]
    fn unknown_or_short_is_none() {
        assert_eq!(parse(&[0x78, 0x01, 0x01, 0x03]), None); // channel-update push: not decoded
        assert_eq!(parse(&[0x78, 0x05]), None); // truncated before the battery byte
        assert_eq!(parse(&[0x11, 0x22]), None); // not a Neewer frame
    }
}
