//! The capability-aware driver: turns a `LightState` into the BLE command bytes
//! for a specific protocol family. This is the layer that owns the model-
//! dependent choices the pure `protocol` encoders deliberately don't (e.g.
//! which classic CCT frame form to use).

use crate::profile::Profile;
use crate::protocol::{classic, home, infinity, pixel, LightState, Mode};

/// A bound driver for one physical light.
#[derive(Debug, Clone)]
pub enum Driver {
    /// Classic `0x78`. `supports_gm` selects the app's 4-byte CCT (with GM,
    /// `classic::cct4`) vs the universally-accepted 2-byte form. `mac` is carried
    /// for the MAC-embedded frames and status queries. `mac_frames` is the app's
    /// per-model `commandType == 2` split: advanced modes (XY/RGBCW/FX) use the
    /// MAC-embedded frames when true (TL120C — the direct forms are ignored) and
    /// the direct frames when false (TL21C — only direct `0x8B` FX renders).
    Classic { supports_gm: bool, mac: [u8; 6], mac_frames: bool },
    /// Infinity `0x78` with the MAC embedded in every payload.
    Infinity { mac: [u8; 6] },
    /// Neewer Home `0x7A` (`NH-*`).
    Home,
}

impl Driver {
    /// Resolve a driver from the config `driver` field. `auto` infers Home from
    /// an `NH-` BLE name, else falls back to Classic (Infinity can't be reliably
    /// auto-detected, so it must be set explicitly). `cmd_type` is the config's
    /// per-light `commandType` (2 ⇒ MAC-embedded advanced-mode frames).
    pub fn resolve(driver_cfg: &str, profile: Profile, mac: [u8; 6], ble_name: &str, cmd_type: u8) -> Driver {
        // KNOWN SIMPLIFICATION: GM support is inferred from the PROFILE, not the
        // model - any light on a GM-carrying profile (cct_gm/full/advanced) gets
        // the app's 4-byte cct4 frame. Every fixture hardware-tested so far
        // (TL120C/TL21C/TL60/TL97C) accepts cct4, but an old classic panel that
        // only parses the 2-byte form would silently ignore CCT if configured
        // with full/advanced. If such a fixture ever surfaces, the fix is a
        // per-light gm/capability field fed from the models.toml catalog (which
        // already knows supports_gm) rather than widening this guess.
        let supports_gm = matches!(profile, Profile::CctGm | Profile::Full | Profile::Advanced);
        let mac_frames = cmd_type == 2;
        match driver_cfg {
            "classic" => Driver::Classic { supports_gm, mac, mac_frames },
            "infinity" => Driver::Infinity { mac },
            "home" => Driver::Home,
            _ => {
                if ble_name.to_lowercase().starts_with("nh-") {
                    Driver::Home
                } else {
                    Driver::Classic { supports_gm, mac, mac_frames }
                }
            }
        }
    }

    /// Power on/off command bytes.
    pub fn power(&self, on: bool) -> Vec<u8> {
        match self {
            Driver::Classic { .. } => classic::power(on),
            Driver::Infinity { mac } => infinity::power(*mac, on),
            Driver::Home => home::power(on),
        }
    }

    /// The command that realises `state`'s current mode. CCT/HSI use the driver's
    /// native family; **XY and RGBCW use the MAC-addressed classic frames** (`0xB7`/
    /// `0xA9` — the direct `0xB9`/`0xA8` forms are ignored on Infinity fixtures like
    /// the TL120C, hardware-confirmed); FX uses the MAC-embedded effect frame (the
    /// only FX form). Home lacks the advanced modes, so they degrade to a neutral CCT.
    pub fn apply(&self, st: &LightState) -> Vec<u8> {
        match st.mode {
            Mode::Cct => match self {
                Driver::Classic { supports_gm, .. } => {
                    if *supports_gm {
                        // The app's exact 4-byte GM form (was our 5-byte cct_gm5);
                        // byte-verified on the TL120C by verygeeky/neewer-lights.
                        classic::cct4(st.brightness, st.cct, st.gm)
                    } else {
                        classic::cct2(st.brightness, st.cct)
                    }
                }
                Driver::Infinity { mac } => infinity::cct(*mac, st.brightness, st.cct, st.gm),
                Driver::Home => home::cct(st.brightness as u16 * 10, st.cct),
            },
            Mode::Hsi => match self {
                Driver::Classic { .. } => classic::hsi(st.hue, st.sat, st.brightness),
                Driver::Infinity { mac } => infinity::hsi(*mac, st.hue, st.sat, st.brightness),
                Driver::Home => home::hsi(st.brightness as u16 * 10, st.hue, st.sat),
            },
            // XY: the frame form follows the app's commandType split — MAC-addressed
            // 0xB7 for commandType==2 (TL120C ignores the direct 0xB9), direct 0xB9
            // otherwise (the form the app sends to everything else). Home degrades
            // to neutral CCT.
            Mode::Xy => match self {
                Driver::Classic { mac_frames: false, .. } => classic::xy(st.brightness, st.x, st.y),
                Driver::Classic { mac, .. } | Driver::Infinity { mac } => {
                    classic::xy_mac(*mac, st.brightness, st.x, st.y)
                }
                Driver::Home => home::cct(st.brightness as u16 * 10, st.cct),
            },
            // RGBCW: same commandType split. By-MAC 0xA9 for commandType==2 (HW-
            // confirmed on the TL120C, whose direct 0xA8 is ignored); direct 0xA8
            // otherwise. Home degrades to neutral CCT.
            Mode::Rgbcw => match self {
                Driver::Classic { mac_frames: false, .. } => {
                    classic::rgbcw(st.brightness, st.r, st.g, st.b, st.cw, st.ww, 0)
                }
                Driver::Classic { mac, .. } | Driver::Infinity { mac } => {
                    classic::rgbcw_mac(*mac, st.brightness, st.r, st.g, st.b, st.cw, st.ww, 0)
                }
                Driver::Home => home::cct(st.brightness as u16 * 10, st.cct),
            },
            // FX: same commandType split — MAC-embedded 0x91 for commandType==2,
            // direct 0x8B otherwise (HW-confirmed on the TL21C, which ignores 0x91
            // and renders the identical effect payload via 0x8B).
            Mode::Fx => match self {
                Driver::Classic { mac_frames: false, .. } => infinity::fx_direct(
                    st.fx_id, st.brightness, st.cct, st.gm, st.hue, st.sat, st.fx_speed,
                    st.fx_extra, st.fx_val2,
                ),
                Driver::Classic { mac, .. } | Driver::Infinity { mac } => infinity::fx(
                    *mac, st.fx_id, st.brightness, st.cct, st.gm, st.hue, st.sat, st.fx_speed,
                    st.fx_extra, st.fx_val2,
                ),
                Driver::Home => home::cct(st.brightness as u16 * 10, st.cct),
            },
            // Pixel is inherently multi-frame; `apply` returns the first frame
            // (params). Callers that can render it fully use `apply_frames`.
            Mode::Pixel => self.apply_frames(st).into_iter().next().unwrap_or_default(),
        }
    }

    /// Every BLE frame that realises `state`, in send order. Most modes are a
    /// single frame; **Pixel** emits a params frame plus 1–2 palette frames (each
    /// may itself need MTU chunking at the BLE layer). This is what the per-light
    /// actor flushes.
    pub fn apply_frames(&self, st: &LightState) -> Vec<Vec<u8>> {
        if st.mode == Mode::Pixel {
            return match self {
                // Home lights aren't pixel fixtures; degrade to a neutral CCT.
                Driver::Home => vec![home::cct(st.brightness as u16 * 10, st.cct)],
                Driver::Classic { mac, .. } | Driver::Infinity { mac } => pixel::paint(
                    *mac, st.pixel_blocks(), st.brightness, st.pixel_effect, st.pixel_speed, st.pixel_dir,
                ),
            };
        }
        vec![self.apply(st)]
    }

    /// The bound light's 6-byte MAC, if this driver family uses one. Used by the
    /// actor to build MAC-addressed status queries (battery/temp/version/state);
    /// `Home` (`0x7A`) has no such reads, so it returns `None`.
    pub fn mac(&self) -> Option<[u8; 6]> {
        match self {
            Driver::Classic { mac, .. } | Driver::Infinity { mac } => Some(*mac),
            Driver::Home => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Driver::Classic { .. } => "classic",
            Driver::Infinity { .. } => "infinity",
            Driver::Home => "home",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cct_state(brr: u8, cct: u8, gm: i8) -> LightState {
        LightState { mode: Mode::Cct, brightness: brr, cct, gm, ..LightState::default() }
    }
    fn hsi_state(hue: u16, sat: u8, brr: u8) -> LightState {
        LightState { mode: Mode::Hsi, hue, sat, brightness: brr, ..LightState::default() }
    }

    #[test]
    fn classic_dispatch() {
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let no_gm = Driver::Classic { supports_gm: false, mac, mac_frames: true };
        assert_eq!(no_gm.apply(&cct_state(50, 56, 0)), classic::cct2(50, 56));
        let gm = Driver::Classic { supports_gm: true, mac, mac_frames: true };
        assert_eq!(gm.apply(&cct_state(50, 56, -10)), classic::cct4(50, 56, -10));
        assert_eq!(no_gm.apply(&hsi_state(180, 100, 75)), classic::hsi(180, 100, 75));
    }

    #[test]
    fn advanced_mode_dispatch() {
        let mac = [1, 2, 3, 4, 5, 6];
        let d = Driver::Classic { supports_gm: true, mac, mac_frames: true };
        // XY uses the MAC-addressed frame (TL120C ignores the direct form).
        let xy = LightState { mode: Mode::Xy, brightness: 80, x: 3127, y: 3290, ..LightState::default() };
        assert_eq!(d.apply(&xy), classic::xy_mac(mac, 80, 3127, 3290));
        // FX uses the MAC-embedded frame even under a classic driver.
        let fx = LightState { mode: Mode::Fx, brightness: 100, cct: 56, fx_id: 1, fx_speed: 5, ..LightState::default() };
        assert_eq!(d.apply(&fx), infinity::fx(mac, 1, 100, 56, 0, 0, 0, 5, 0, 0));
        // RGBCW uses the by-MAC frame (0xA9) — direct 0xA8 is ignored on the TL120C.
        let rgbcw = LightState { mode: Mode::Rgbcw, brightness: 100, r: 255, g: 0, b: 0, ..LightState::default() };
        assert_eq!(d.apply(&rgbcw), classic::rgbcw_mac(mac, 100, 255, 0, 0, 0, 0, 0));
    }

    #[test]
    fn advanced_mode_dispatch_direct_frames() {
        // cmd_type != 2 (e.g. the TL21C): XY/RGBCW/FX use the DIRECT frame forms
        // (0xB9 / 0xA8 / 0x8B) — hardware-verified 2026-07-02: the TL21C renders
        // FX via 0x8B only and ignores every MAC-embedded control frame.
        let mac = [1, 2, 3, 4, 5, 6];
        let d = Driver::Classic { supports_gm: false, mac, mac_frames: false };
        let xy = LightState { mode: Mode::Xy, brightness: 80, x: 3127, y: 3290, ..LightState::default() };
        assert_eq!(d.apply(&xy), classic::xy(80, 3127, 3290));
        let fx = LightState { mode: Mode::Fx, brightness: 100, cct: 56, fx_id: 1, fx_speed: 5, ..LightState::default() };
        assert_eq!(d.apply(&fx), infinity::fx_direct(1, 100, 56, 0, 0, 0, 5, 0, 0));
        let rgbcw = LightState { mode: Mode::Rgbcw, brightness: 100, r: 255, g: 0, b: 0, ..LightState::default() };
        assert_eq!(d.apply(&rgbcw), classic::rgbcw(100, 255, 0, 0, 0, 0, 0));
        // CCT/HSI are unaffected by the split.
        assert_eq!(d.apply(&cct_state(50, 56, 0)), classic::cct2(50, 56));
    }

    #[test]
    fn pixel_apply_frames_emits_params_and_palette() {
        let mac = [0xCC, 0x8D, 0xBE, 0xBB, 0x25, 0xB0];
        let d = Driver::Classic { supports_gm: true, mac, mac_frames: true };
        let mut st = LightState { mode: Mode::Pixel, brightness: 100, seg_count: 2, pixel_speed: 30, pixel_effect: 1, ..LightState::default() };
        st.segments[0] = pixel::Block::Hsi { hue: 0, sat: 100 };
        st.segments[1] = pixel::Block::Hsi { hue: 240, sat: 100 };
        let frames = d.apply_frames(&st);
        // params + one palette (≤6 colours) = 2 frames.
        assert_eq!(frames.len(), 2);
        // Palette frame must match the pixel encoder for the same blocks.
        assert_eq!(frames[1], pixel::palette(mac, 1, 1, st.pixel_blocks()));
        // Home degrades pixel to a single neutral-CCT frame.
        assert_eq!(Driver::Home.apply_frames(&st).len(), 1);
    }

    #[test]
    fn infinity_and_home_dispatch() {
        let mac = [1, 2, 3, 4, 5, 6];
        let inf = Driver::Infinity { mac };
        assert_eq!(inf.apply(&cct_state(50, 40, 5)), infinity::cct(mac, 50, 40, 5));
        assert_eq!(inf.power(true), infinity::power(mac, true));

        let h = Driver::Home;
        assert_eq!(h.apply(&cct_state(50, 40, 0)), home::cct(500, 40));
        assert_eq!(h.apply(&hsi_state(240, 100, 100)), home::hsi(1000, 240, 100));
    }

    /// The exact BLE bytes `HARDWARE-BRINGUP.md` step 4 tells the operator to
    /// expect, produced by running the real DMX → `LightState` → driver
    /// pipeline. If this fails, either the mapper/encoder changed or the doc is
    /// stale — fix whichever is wrong.
    ///
    /// This lived in `tests/bringup_bytes.rs`, which is **git-excluded**, so it
    /// was never in a clone and never ran in CI: the tracked, public doc had no
    /// guard against drifting from the code at all. Living in the lib, it ships
    /// and runs everywhere `cargo test` does.
    #[test]
    fn cct_profile_pipeline_matches_the_bringup_doc_byte_table() {
        use crate::profile::{map_dmx, CctRange};

        let drv = Driver::Classic { supports_gm: false, mac: [0; 6], mac_frames: true };
        let hex = |b: &[u8]| {
            b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
        };
        // (DMX [dimmer, cct], expected write) — the doc's table verbatim.
        let cases: &[([u8; 2], &str)] = &[
            ([255, 255], "78 87 02 64 38 9d"), // brr 100, cct 56
            ([128, 255], "78 87 02 32 38 6b"), // brr 50,  cct 56
            ([255, 0], "78 87 02 64 20 85"),   // brr 100, cct 32
        ];
        for (dmx, want) in cases {
            let st = map_dmx(Profile::Cct, dmx, CctRange::default());
            assert_eq!(hex(&drv.apply(&st)), *want, "dmx {dmx:?}");
        }
    }

    #[test]
    fn resolve_auto_detects_home_from_nh_name() {
        let mac = [0u8; 6];
        assert!(matches!(
            Driver::resolve("auto", Profile::Full, mac, "NH-PD20250030", 2),
            Driver::Home
        ));
        assert!(matches!(
            Driver::resolve("auto", Profile::Cct, mac, "NEEWER-RGB660", 1),
            Driver::Classic { supports_gm: false, mac_frames: false, .. }
        ));
        assert!(matches!(
            Driver::resolve("auto", Profile::Full, mac, "NEEWER-RGB660", 2),
            Driver::Classic { supports_gm: true, mac_frames: true, .. }
        ));
    }
}
