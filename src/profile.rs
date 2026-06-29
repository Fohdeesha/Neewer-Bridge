//! DMX profiles and the DMX→`LightState` mapper (NOTES.md §8.1).
//!
//! A profile is a fixed channel layout sized to a light's capability. The mapper
//! is a pure function (DMX bytes in, `LightState` out) so it is fully unit-
//! tested without hardware. Per the locked decisions: channels are 8-bit, the
//! master Dimmer sets brightness only (never cuts power, so mapped `power` is
//! always `true`), and multi-mode lights carry a live Mode channel.

use crate::protocol::{LightState, Mode};

/// A DMX personality / channel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// 2ch: Dimmer, CCT.
    Cct,
    /// 3ch: Dimmer, CCT, GM.
    CctGm,
    /// 3ch: Dimmer, Hue, Sat.
    Hsi,
    /// 5ch: Dimmer, Mode, CCT/Hue, GM/Sat, (reserved). Mode <128 = CCT, ≥128 = HSI.
    Full,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Profile> {
        match s {
            "cct" => Some(Profile::Cct),
            "cct_gm" => Some(Profile::CctGm),
            "hsi" => Some(Profile::Hsi),
            "full" => Some(Profile::Full),
            _ => None,
        }
    }

    /// Number of DMX channels this profile occupies.
    pub fn channel_count(&self) -> u16 {
        match self {
            Profile::Cct => 2,
            Profile::Hsi | Profile::CctGm => 3,
            Profile::Full => 5,
        }
    }
}

/// Raw CCT-value range used to scale the CCT channel. Model-dependent; default
/// 32..=56 = 3200K..5600K. Some lights extend to 85 (8500K) — overridable later.
#[derive(Debug, Clone, Copy)]
pub struct CctRange {
    pub min: u8,
    pub max: u8,
}

impl Default for CctRange {
    fn default() -> Self {
        CctRange { min: 32, max: 56 }
    }
}

/// Mode channel threshold for the `full` profile: < this = CCT, ≥ this = HSI.
pub const MODE_HSI_THRESHOLD: u8 = 128;

/// Scale an 8-bit DMX value to 0..=max with rounding.
#[inline]
fn scale_to(dmx: u8, max: u32) -> u32 {
    (dmx as u32 * max + 127) / 255
}

#[inline]
fn brightness_value(dmx: u8) -> u8 {
    scale_to(dmx, 100) as u8
}

#[inline]
fn cct_value(dmx: u8, range: CctRange) -> u8 {
    let span = range.max.saturating_sub(range.min) as u32;
    range.min + scale_to(dmx, span) as u8
}

#[inline]
fn gm_value(dmx: u8) -> i8 {
    (scale_to(dmx, 100) as i32 - 50).clamp(-50, 50) as i8
}

#[inline]
fn hue_value(dmx: u8) -> u16 {
    scale_to(dmx, 360) as u16
}

#[inline]
fn sat_value(dmx: u8) -> u8 {
    scale_to(dmx, 100) as u8
}

/// Map a light's DMX channel slice to a desired `LightState`.
///
/// `slice` should be `profile.channel_count()` bytes; missing channels are read
/// as 0 (defensive — a short ArtDmx packet shouldn't panic the bridge).
pub fn map_dmx(profile: Profile, slice: &[u8], cct: CctRange) -> LightState {
    let ch = |i: usize| -> u8 { slice.get(i).copied().unwrap_or(0) };

    let mut st = LightState {
        power: true, // kept on; Dimmer is brightness-only
        ..LightState::default()
    };

    match profile {
        Profile::Cct => {
            st.mode = Mode::Cct;
            st.brightness = brightness_value(ch(0));
            st.cct = cct_value(ch(1), cct);
        }
        Profile::CctGm => {
            st.mode = Mode::Cct;
            st.brightness = brightness_value(ch(0));
            st.cct = cct_value(ch(1), cct);
            st.gm = gm_value(ch(2));
        }
        Profile::Hsi => {
            st.mode = Mode::Hsi;
            st.brightness = brightness_value(ch(0));
            st.hue = hue_value(ch(1));
            st.sat = sat_value(ch(2));
        }
        Profile::Full => {
            st.brightness = brightness_value(ch(0));
            if ch(1) < MODE_HSI_THRESHOLD {
                st.mode = Mode::Cct;
                st.cct = cct_value(ch(2), cct);
                st.gm = gm_value(ch(3));
            } else {
                st.mode = Mode::Hsi;
                st.hue = hue_value(ch(2));
                st.sat = sat_value(ch(3));
            }
            // ch(4) reserved (future: FX band / hue-fine)
        }
    }
    st
}

/// Extract a light's channel slice from a universe DMX buffer.
///
/// `address1` is the 1-based DMX start channel. Returns `None` if the requested
/// channels run past the available data (a config/addressing error).
pub fn extract_slice(buffer: &[u8], address1: u16, count: u16) -> Option<&[u8]> {
    let start = (address1 as usize).checked_sub(1)?;
    let end = start + count as usize;
    buffer.get(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_counts() {
        assert_eq!(Profile::Cct.channel_count(), 2);
        assert_eq!(Profile::CctGm.channel_count(), 3);
        assert_eq!(Profile::Hsi.channel_count(), 3);
        assert_eq!(Profile::Full.channel_count(), 5);
    }

    #[test]
    fn scaling_endpoints_and_midpoint() {
        assert_eq!(brightness_value(0), 0);
        assert_eq!(brightness_value(255), 100);
        assert_eq!(brightness_value(128), 50);
        assert_eq!(hue_value(0), 0);
        assert_eq!(hue_value(255), 360);
        assert_eq!(sat_value(255), 100);
        assert_eq!(gm_value(0), -50);
        assert_eq!(gm_value(255), 50);
        assert_eq!(gm_value(128), 0);
        assert_eq!(cct_value(0, CctRange::default()), 32);
        assert_eq!(cct_value(255, CctRange::default()), 56);
    }

    #[test]
    fn cct_profile_maps() {
        let st = map_dmx(Profile::Cct, &[255, 255], CctRange::default());
        assert!(st.power);
        assert_eq!(st.mode, Mode::Cct);
        assert_eq!(st.brightness, 100);
        assert_eq!(st.cct, 56);
    }

    #[test]
    fn full_profile_mode_channel_switches() {
        // mode byte 0 -> CCT mode: ch3=CCT, ch4=GM
        let cct_mode = map_dmx(Profile::Full, &[255, 0, 0, 255, 0], CctRange::default());
        assert_eq!(cct_mode.mode, Mode::Cct);
        assert_eq!(cct_mode.cct, 32); // ch3=0 -> min
        assert_eq!(cct_mode.gm, 50); // ch4=255 -> +50

        // mode byte 200 (>=128) -> HSI mode: ch3=Hue, ch4=Sat
        let hsi_mode = map_dmx(Profile::Full, &[128, 200, 255, 255, 0], CctRange::default());
        assert_eq!(hsi_mode.mode, Mode::Hsi);
        assert_eq!(hsi_mode.brightness, 50);
        assert_eq!(hsi_mode.hue, 360);
        assert_eq!(hsi_mode.sat, 100);
    }

    #[test]
    fn short_slice_is_defensive_not_panicking() {
        // Only 1 byte supplied for a 3-channel profile — missing read as 0.
        let st = map_dmx(Profile::Hsi, &[255], CctRange::default());
        assert_eq!(st.brightness, 100);
        assert_eq!(st.hue, 0);
        assert_eq!(st.sat, 0);
    }

    #[test]
    fn extract_slice_bounds() {
        let buf = [10u8, 20, 30, 40, 50];
        assert_eq!(extract_slice(&buf, 1, 2), Some(&[10u8, 20][..]));
        assert_eq!(extract_slice(&buf, 4, 2), Some(&[40u8, 50][..]));
        assert_eq!(extract_slice(&buf, 5, 2), None); // runs past end
        assert_eq!(extract_slice(&buf, 0, 1), None); // 0 is invalid (1-based)
    }
}
