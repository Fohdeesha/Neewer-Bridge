//! The capability-aware driver: turns a `LightState` into the BLE command bytes
//! for a specific protocol family. This is the layer that owns the model-
//! dependent choices the pure `protocol` encoders deliberately don't (e.g.
//! which classic CCT frame form to use), per NOTES.md §6.

use crate::profile::Profile;
use crate::protocol::{classic, home, infinity, LightState, Mode};

/// A bound driver for one physical light.
#[derive(Debug, Clone)]
pub enum Driver {
    /// Classic `0x78`. `supports_gm` selects the 5-byte CCT (with GM) vs the
    /// universally-accepted 2-byte form.
    Classic { supports_gm: bool },
    /// Infinity `0x78` with the MAC embedded in every payload.
    Infinity { mac: [u8; 6] },
    /// Neewer Home `0x7A` (`NH-*`).
    Home,
}

impl Driver {
    /// Resolve a driver from the config `driver` field. `auto` infers Home from
    /// an `NH-` BLE name, else falls back to Classic (Infinity can't be reliably
    /// auto-detected, so it must be set explicitly).
    pub fn resolve(driver_cfg: &str, profile: Profile, mac: [u8; 6], ble_name: &str) -> Driver {
        let supports_gm = matches!(profile, Profile::CctGm | Profile::Full);
        match driver_cfg {
            "classic" => Driver::Classic { supports_gm },
            "infinity" => Driver::Infinity { mac },
            "home" => Driver::Home,
            _ => {
                if ble_name.to_lowercase().starts_with("nh-") {
                    Driver::Home
                } else {
                    Driver::Classic { supports_gm }
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

    /// The command that realises `state`'s current mode (CCT or HSI).
    pub fn apply(&self, st: &LightState) -> Vec<u8> {
        match (self, st.mode) {
            (Driver::Classic { supports_gm }, Mode::Cct) => {
                if *supports_gm {
                    classic::cct_gm5(st.brightness, st.cct, st.gm)
                } else {
                    classic::cct2(st.brightness, st.cct)
                }
            }
            (Driver::Classic { .. }, Mode::Hsi) => classic::hsi(st.hue, st.sat, st.brightness),
            (Driver::Infinity { mac }, Mode::Cct) => {
                infinity::cct(*mac, st.brightness, st.cct, st.gm)
            }
            (Driver::Infinity { mac }, Mode::Hsi) => {
                infinity::hsi(*mac, st.hue, st.sat, st.brightness)
            }
            // Home brightness is the native 0..=1000 range; our state is 0..=100.
            (Driver::Home, Mode::Cct) => home::cct(st.brightness as u16 * 10, st.cct),
            (Driver::Home, Mode::Hsi) => home::hsi(st.brightness as u16 * 10, st.hue, st.sat),
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
        let no_gm = Driver::Classic { supports_gm: false };
        assert_eq!(no_gm.apply(&cct_state(50, 56, 0)), classic::cct2(50, 56));
        let gm = Driver::Classic { supports_gm: true };
        assert_eq!(gm.apply(&cct_state(50, 56, -10)), classic::cct_gm5(50, 56, -10));
        assert_eq!(no_gm.apply(&hsi_state(180, 100, 75)), classic::hsi(180, 100, 75));
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

    #[test]
    fn resolve_auto_detects_home_from_nh_name() {
        let mac = [0u8; 6];
        assert!(matches!(
            Driver::resolve("auto", Profile::Full, mac, "NH-PD20250030"),
            Driver::Home
        ));
        assert!(matches!(
            Driver::resolve("auto", Profile::Cct, mac, "NEEWER-RGB660"),
            Driver::Classic { supports_gm: false }
        ));
        assert!(matches!(
            Driver::resolve("auto", Profile::Full, mac, "NEEWER-RGB660"),
            Driver::Classic { supports_gm: true }
        ));
    }
}
