//! DMX profiles and the DMX→`LightState` mapper.
//!
//! A profile is a fixed channel layout sized to a light's capability. The mapper
//! is a pure function (DMX bytes in, `LightState` out) so it is fully unit-
//! tested without hardware. Per the locked decisions: channels are 8-bit, the
//! master Dimmer sets brightness only (never cuts power, so mapped `power` is
//! always `true`), and multi-mode lights carry a live Mode channel.

use crate::protocol::pixel::Block;
use crate::protocol::{LightState, Mode};

/// Number of independently-addressable segments in the `pixel` profile (the pixel
/// palette's colour-block cap).
pub const PIXEL_SEGMENTS: usize = 8;

/// A DMX personality / channel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// 2ch: Dimmer, CCT.
    Cct,
    /// 3ch: Dimmer, CCT, GM.
    CctGm,
    /// 3ch: Dimmer, Hue, Sat.
    Hsi,
    /// 3ch: Red, Green, Blue. Converted to HSI internally (hue/sat from the RGB
    /// ratio, brightness = the max component), so it works on EVERY colour fixture
    /// — including models with no native RGBCW mode (e.g. the TL21C). Lays out onto
    /// an openHAB DMX `color` thing (3ch) with no white channels and no rules.
    /// White = desaturated HSI rendered through the RGB engine; for dedicated
    /// CW/WW LED banks use `rgbcw` instead.
    Rgb,
    /// 5ch: Red, Green, Blue, Cool-White, Warm-White. Standard 8-bit values (0..=255)
    /// passed **straight through** to the light's native RGBCW mode — one DMX channel
    /// per physical LED bank, no colour-space conversion. Lays out exactly onto an
    /// openHAB DMX `color` thing (3ch RGB) + `tunablewhite` thing (2ch, "cool white,
    /// warm white" order) patched contiguously.
    Rgbcw,
    /// 5ch: Dimmer, Mode, CCT/Hue, GM/Sat, (reserved). Mode <128 = CCT, ≥128 = HSI.
    Full,
    /// 10ch unified mode-channel personality. ch1 Mode-select
    /// (value bands → CCT/HSI/FX/RGBCW/XY), ch2 Dimmer, ch3-10 mode-specific.
    Advanced,
    /// 20ch per-segment PIXEL personality (TL-series pixel fixtures). ch1 Dimmer,
    /// ch2 Effect-select, ch3 Speed, ch4 Direction, then 8×(Hue, Sat) — one HSI
    /// colour per tube segment. The effects animate the palette across the tube
    /// (no static mode exists over BLE on these fixtures).
    Pixel,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Profile> {
        match s {
            "cct" => Some(Profile::Cct),
            "cct_gm" => Some(Profile::CctGm),
            "hsi" => Some(Profile::Hsi),
            "rgb" => Some(Profile::Rgb),
            "rgbcw" => Some(Profile::Rgbcw),
            "full" => Some(Profile::Full),
            "advanced" => Some(Profile::Advanced),
            "pixel" => Some(Profile::Pixel),
            _ => None,
        }
    }

    /// The canonical config string for this profile (inverse of `parse`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Profile::Cct => "cct",
            Profile::CctGm => "cct_gm",
            Profile::Hsi => "hsi",
            Profile::Rgb => "rgb",
            Profile::Rgbcw => "rgbcw",
            Profile::Full => "full",
            Profile::Advanced => "advanced",
            Profile::Pixel => "pixel",
        }
    }

    /// Number of DMX channels this profile occupies.
    pub fn channel_count(&self) -> u16 {
        match self {
            Profile::Cct => 2,
            Profile::Hsi | Profile::CctGm | Profile::Rgb => 3,
            Profile::Full | Profile::Rgbcw => 5,
            Profile::Advanced => 10,
            // 1 Dimmer + 1 Effect + 1 Speed + 1 Direction + PIXEL_SEGMENTS × (Hue, Sat).
            Profile::Pixel => 4 + PIXEL_SEGMENTS as u16 * 2,
        }
    }

    /// Per-channel role labels, in order from the light's start address (index 0
    /// = the start channel). For `full` and `advanced` the later channels are
    /// reinterpreted by the live Mode channel, so their labels list every mode's
    /// meaning (`A / B / …`). Used by the `lights` command to print the mapping.
    pub fn channel_roles(&self) -> &'static [&'static str] {
        match self {
            Profile::Cct => &["Dimmer", "CCT"],
            Profile::CctGm => &["Dimmer", "CCT", "GM"],
            Profile::Hsi => &["Dimmer", "Hue", "Saturation"],
            Profile::Rgb => &["Red", "Green", "Blue"],
            Profile::Rgbcw => &["Red", "Green", "Blue", "Cool White", "Warm White"],
            Profile::Full => &[
                "Dimmer",
                "Mode-select (0-127 CCT / 128-255 HSI)",
                "CCT / Hue",
                "GM / Saturation",
                "(reserved)",
            ],
            Profile::Advanced => &[
                "Mode-select (CCT/HSI/FX/RGBCW/XY — see bands)",
                "Dimmer",
                "CCT / Hue / FX-id / R / X",
                "GM / Saturation / FX-speed / G / Y",
                "— / — / FX-CCT / B / —",
                "— / — / FX-Hue / CW / —",
                "— / — / FX-Sat+GM / WW / —",
                "— / — / FX-extra / — / —",
                "— / — / FX-2nd-value / — / —",
                "(reserved)",
            ],
            Profile::Pixel => &[
                "Dimmer (master)",
                "Effect (ColorReplace/Single/Two/Three-Moving/Fire — see bands)",
                "Speed / motion",
                "Direction",
                "Seg1 Hue (moving/fire: background)", "Seg1 Sat",
                "Seg2 Hue", "Seg2 Sat",
                "Seg3 Hue", "Seg3 Sat",
                "Seg4 Hue", "Seg4 Sat",
                "Seg5 Hue", "Seg5 Sat",
                "Seg6 Hue", "Seg6 Sat",
                "Seg7 Hue", "Seg7 Sat",
                "Seg8 Hue", "Seg8 Sat",
            ],
        }
    }
}

/// `advanced` profile mode-channel (ch1) value bands. Mirrors the official Neewer
/// DMX personality so a console patched for the fixture feels familiar. Bands not
/// listed (GEL 96-127, Pixel 160-191, 232-255) are unimplemented → neutral white.
/// RGBCW (128-159) is driven via the **by-MAC** frame (`0xA9`); the direct `0xA8` is
/// ignored on the TL120C — hardware-confirmed 2026-07-01.
pub mod mode_band {
    pub const CCT: std::ops::RangeInclusive<u8> = 0..=31;
    pub const HSI: std::ops::RangeInclusive<u8> = 32..=63;
    pub const FX: std::ops::RangeInclusive<u8> = 64..=95;
    pub const RGBCW: std::ops::RangeInclusive<u8> = 128..=159;
    pub const XY: std::ops::RangeInclusive<u8> = 192..=231;
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

/// RGB → HSI: hue 0..=359, sat 0..=100, brightness 0..=100. Standard HSV
/// derivation (value = max component), integer math with rounding.
fn rgb_to_hsi(r: u8, g: u8, b: u8) -> (u16, u8, u8) {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let bri = ((max * 100 + 127) / 255) as u8;
    let sat = if max == 0 { 0 } else { ((delta * 100 + max / 2) / max) as u8 };
    let hue = if delta == 0 {
        0
    } else {
        let h = if max == r {
            60 * (g - b) / delta
        } else if max == g {
            60 * (b - r) / delta + 120
        } else {
            60 * (r - g) / delta + 240
        };
        h.rem_euclid(360) as u16
    };
    (hue, sat, bri)
}

/// FX effect-select: DMX 0..=255 → effect id 1..=18.
#[inline]
fn fx_select(dmx: u8) -> u8 {
    (1 + scale_to(dmx, 17)) as u8
}

/// FX speed/rate: DMX 0..=255 → 1..=10.
#[inline]
fn speed_value(dmx: u8) -> u8 {
    (1 + scale_to(dmx, 9)) as u8
}

/// CIE xy coordinate channel: DMX 0..=255 → ×10000 (0..=8000 = 0.0000..=0.8000).
#[inline]
fn xy_value(dmx: u8) -> u16 {
    scale_to(dmx, 8000) as u16
}

/// Pixel effect-select channel → effect id, in five value bands. Only the effects
/// that work over direct BLE on the TL120C are exposed (hardware-verified): 1
/// ColorReplacement, 3 SingleColorMoving, 4 TwoColorMoving, 5 ThreeColorMoving, 7
/// Fire. (The app's other 5 pixel effects are ignored over direct BLE.)
#[inline]
fn pixel_effect_select(dmx: u8) -> u8 {
    match dmx {
        0..=51 => 1,    // ColorReplacement (8-segment palette)
        52..=102 => 3,  // SingleColorMoving
        103..=153 => 4, // TwoColorMoving
        154..=204 => 5, // ThreeColorMoving
        _ => 7,         // Fire
    }
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
        Profile::Rgb => {
            // Plain 3ch RGB, converted to HSI — drives the light's HSI mode, which
            // every colour fixture honours (incl. non-RGBCW models like the TL21C).
            // Level rides in the channel values (brightness = max component), so an
            // openHAB `color` thing is the only patch needed.
            st.mode = Mode::Hsi;
            let (hue, sat, bri) = rgb_to_hsi(ch(0), ch(1), ch(2));
            st.hue = hue;
            st.sat = sat;
            st.brightness = bri;
        }
        Profile::Rgbcw => {
            // Native RGBCW passthrough: five raw channels straight into the by-MAC
            // 0xA9 frame — R, G, B, cool-white, warm-white, one DMX channel per LED
            // bank. Lays out onto openHAB's DMX `color` thing (3ch RGB) + `tunablewhite`
            // thing (2ch, "cool white, warm white") patched contiguously. Level rides in
            // the channel values themselves, so the frame's master brightness is 100.
            st.mode = Mode::Rgbcw;
            st.brightness = 100;
            st.r = ch(0);
            st.g = ch(1);
            st.b = ch(2);
            st.cw = ch(3);
            st.ww = ch(4);
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
        Profile::Advanced => {
            // ch1 = mode select, ch2 = dimmer, ch3..ch10 = mode-specific.
            let mode_sel = ch(0);
            st.brightness = brightness_value(ch(1));
            use mode_band as mb;
            if mb::CCT.contains(&mode_sel) {
                st.mode = Mode::Cct;
                st.cct = cct_value(ch(2), cct);
                st.gm = gm_value(ch(3));
            } else if mb::HSI.contains(&mode_sel) {
                st.mode = Mode::Hsi;
                st.hue = hue_value(ch(2));
                st.sat = sat_value(ch(3));
            } else if mb::FX.contains(&mode_sel) {
                st.mode = Mode::Fx;
                st.fx_id = fx_select(ch(2));
                st.fx_speed = speed_value(ch(3));
                st.cct = cct_value(ch(4), cct);
                st.hue = hue_value(ch(5));
                // ch7 doubles as Sat (HUE effects) / GM (CCT effects); store both.
                st.sat = sat_value(ch(6));
                st.gm = gm_value(ch(6));
                // ch8 = effect-specific extra (ember/colour/mode); builder clamps.
                st.fx_extra = scale_to(ch(7), 10) as u8;
                // ch9 = effect-specific 2nd value: CCT-loop CCT2 (raw) else Hue2.
                st.fx_val2 = if st.fx_id == 13 {
                    cct_value(ch(8), cct) as u16
                } else {
                    hue_value(ch(8))
                };
            } else if mb::RGBCW.contains(&mode_sel) {
                // RGBCW (128-159): raw R/G/B + cool-white/warm-white channels (per
                // the official DMX personality). Direct 8-bit values, no scaling —
                // ch2 Dimmer is the master brightness. Sent via the by-MAC 0xA9 frame.
                st.mode = Mode::Rgbcw;
                st.r = ch(2);
                st.g = ch(3);
                st.b = ch(4);
                st.cw = ch(5);
                st.ww = ch(6);
            } else if mb::XY.contains(&mode_sel) {
                st.mode = Mode::Xy;
                st.x = xy_value(ch(2));
                st.y = xy_value(ch(3));
            } else {
                // Unimplemented band (GEL 96-127 / Pixel 160-191 / reserved) →
                // neutral white. Use HSI/XY/RGBCW for colour.
                st.mode = Mode::Cct;
                st.cct = cct_value(128, cct);
            }
        }
        Profile::Pixel => {
            // ch1 Dimmer, ch2 Effect-select, ch3 Speed, ch4 Direction, then
            // PIXEL_SEGMENTS × (Hue, Sat). Segment meaning depends on the effect:
            // ColorReplacement uses all 8 as a spatial palette; the moving/fire
            // effects use segment 1 as the background and the rest as their colours.
            st.mode = Mode::Pixel;
            st.brightness = brightness_value(ch(0));
            st.pixel_effect = pixel_effect_select(ch(1));
            st.pixel_speed = brightness_value(ch(2)); // 0..=100 motion speed
            st.pixel_dir = if ch(3) < 128 { 0 } else { 1 };
            st.seg_count = PIXEL_SEGMENTS as u8;
            for i in 0..PIXEL_SEGMENTS {
                let hue = hue_value(ch(4 + i * 2));
                let sat = sat_value(ch(5 + i * 2));
                st.segments[i] = Block::Hsi { hue, sat };
            }
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
        assert_eq!(Profile::Rgb.channel_count(), 3);
        assert_eq!(Profile::Rgbcw.channel_count(), 5);
        assert_eq!(Profile::Full.channel_count(), 5);
    }

    #[test]
    fn rgbcw_profile_direct_passthrough() {
        // ch1-5 = R, G, B, CW, WW straight through; master brightness fixed at 100.
        let st = map_dmx(Profile::Rgbcw, &[10, 20, 30, 40, 50], CctRange::default());
        assert_eq!(st.mode, Mode::Rgbcw);
        assert!(st.power);
        assert_eq!(st.brightness, 100);
        assert_eq!((st.r, st.g, st.b, st.cw, st.ww), (10, 20, 30, 40, 50));

        // Full white via the dedicated white channels (openHAB tunablewhite), colour off.
        let w = map_dmx(Profile::Rgbcw, &[0, 0, 0, 255, 255], CctRange::default());
        assert_eq!((w.r, w.g, w.b, w.cw, w.ww), (0, 0, 0, 255, 255));

        // Short slice is defensive: missing white channels read as 0.
        let s = map_dmx(Profile::Rgbcw, &[255, 0, 0], CctRange::default());
        assert_eq!((s.r, s.g, s.b, s.cw, s.ww), (255, 0, 0, 0, 0));
    }

    #[test]
    fn rgb_profile_converts_to_hsi() {
        // Pure red → hue 0, full sat, full brightness.
        let red = map_dmx(Profile::Rgb, &[255, 0, 0], CctRange::default());
        assert_eq!(red.mode, Mode::Hsi);
        assert!(red.power);
        assert_eq!((red.hue, red.sat, red.brightness), (0, 100, 100));

        // Pure green / blue hit their HSV sector centres.
        let green = map_dmx(Profile::Rgb, &[0, 255, 0], CctRange::default());
        assert_eq!((green.hue, green.sat, green.brightness), (120, 100, 100));
        let blue = map_dmx(Profile::Rgb, &[0, 0, 255], CctRange::default());
        assert_eq!((blue.hue, blue.sat, blue.brightness), (240, 100, 100));

        // White = desaturated; level rides in the channel values.
        let white = map_dmx(Profile::Rgb, &[255, 255, 255], CctRange::default());
        assert_eq!((white.sat, white.brightness), (0, 100));
        let half = map_dmx(Profile::Rgb, &[128, 128, 128], CctRange::default());
        assert_eq!((half.sat, half.brightness), (0, 50));

        // Black → brightness 0 (dark, but power stays on per the locked decisions).
        let black = map_dmx(Profile::Rgb, &[0, 0, 0], CctRange::default());
        assert_eq!(black.brightness, 0);
        assert!(black.power);

        // Magenta (max = r, g < b) wraps via rem_euclid instead of going negative.
        let magenta = map_dmx(Profile::Rgb, &[255, 0, 255], CctRange::default());
        assert_eq!((magenta.hue, magenta.sat), (300, 100));
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
        // TL120C real range 2500..10000K (raw 25..100): endpoints + midpoint.
        let tl = CctRange { min: 25, max: 100 };
        assert_eq!(cct_value(0, tl), 25); // 2500K
        assert_eq!(cct_value(255, tl), 100); // 10000K
        assert_eq!(cct_value(128, tl), 63); // ~6300K mid
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
    fn advanced_mode_bands() {
        let tl = CctRange { min: 25, max: 100 };
        let m = |sel: u8, rest: &[u8]| {
            let mut s = vec![sel, 255]; // mode-select, full dimmer
            s.extend_from_slice(rest);
            map_dmx(Profile::Advanced, &s, tl)
        };
        // CCT band (0-31)
        let c = m(0, &[255, 128]);
        assert_eq!(c.mode, Mode::Cct);
        assert_eq!(c.cct, 100); // ch3=255 -> max
        // HSI band (32-63)
        let h = m(40, &[255, 255]);
        assert_eq!(h.mode, Mode::Hsi);
        assert_eq!(h.hue, 360);
        assert_eq!(h.sat, 100);
        // FX band (64-95): ch3 effect-select, ch4 speed
        let f = m(80, &[0, 255, 255, 0, 0, 0, 0]);
        assert_eq!(f.mode, Mode::Fx);
        assert_eq!(f.fx_id, 1); // ch3=0 -> effect 1
        assert_eq!(f.fx_speed, 10); // ch4=255 -> 10
        // XY band (192-231): ch3=X, ch4=Y
        let xy = m(200, &[255, 0]);
        assert_eq!(xy.mode, Mode::Xy);
        assert_eq!(xy.x, 8000); // ch3=255 -> 0.8000
        assert_eq!(xy.y, 0);
        // GEL band (96-127) still unimplemented -> neutral CCT
        assert_eq!(m(100, &[0, 0]).mode, Mode::Cct);
        // RGBCW band (128-159): ch3-7 = R,G,B,CW,WW (raw 8-bit), ch2 = dimmer.
        let rgbcw = m(130, &[255, 0, 0, 0, 0]);
        assert_eq!(rgbcw.mode, Mode::Rgbcw);
        assert_eq!(rgbcw.brightness, 100);
        assert_eq!((rgbcw.r, rgbcw.g, rgbcw.b, rgbcw.cw, rgbcw.ww), (255, 0, 0, 0, 0));
    }

    #[test]
    fn pixel_profile_maps_segments() {
        assert_eq!(Profile::Pixel.channel_count(), 20);
        // ch1 Dimmer=255, ch2 Effect=0(→1), ch3 Speed=0, ch4 Dir=255, then 8×(Hue,Sat).
        let mut dmx = vec![255u8, 0, 0, 255];
        for _ in 0..PIXEL_SEGMENTS {
            dmx.push(255); // hue
            dmx.push(255); // sat
        }
        let st = map_dmx(Profile::Pixel, &dmx, CctRange::default());
        assert_eq!(st.mode, Mode::Pixel);
        assert!(st.power);
        assert_eq!(st.brightness, 100);
        assert_eq!(st.pixel_effect, 1);
        assert_eq!(st.pixel_speed, 0);
        assert_eq!(st.pixel_dir, 1); // ch4=255 → dir 1
        assert_eq!(st.seg_count, 8);
        assert_eq!(st.segments[0], Block::Hsi { hue: 360, sat: 100 });
        assert_eq!(st.pixel_blocks().len(), 8);

        // Effect band ch2=128 → effect 4 (TwoColorMoving); dir ch4=0; colours from ch5.
        let mut dmx2 = vec![200u8, 128, 128, 0];
        dmx2.extend_from_slice(&[0, 255]); // seg1 hue=0, sat=100
        dmx2.extend_from_slice(&[170, 255]); // seg2 hue≈240
        dmx2.resize(20, 0);
        let st2 = map_dmx(Profile::Pixel, &dmx2, CctRange::default());
        assert_eq!(st2.pixel_effect, 4);
        assert_eq!(st2.pixel_dir, 0);
        assert_eq!(st2.segments[0], Block::Hsi { hue: 0, sat: 100 });
        assert_eq!(st2.segments[1], Block::Hsi { hue: 240, sat: 100 });
    }

    #[test]
    fn pixel_effect_select_bands() {
        assert_eq!(pixel_effect_select(0), 1);
        assert_eq!(pixel_effect_select(51), 1);
        assert_eq!(pixel_effect_select(52), 3);
        assert_eq!(pixel_effect_select(102), 3);
        assert_eq!(pixel_effect_select(103), 4);
        assert_eq!(pixel_effect_select(204), 5);
        assert_eq!(pixel_effect_select(205), 7);
        assert_eq!(pixel_effect_select(255), 7);
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
