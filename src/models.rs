//! Light model capability catalog (`models.toml`).
//!
//! A reliable file store of every known Neewer model and what it can do, so that
//! `add` doesn't need the user to hand-specify CCT range / RGB capability /
//! driver — it identifies the light from its advertised BLE name and fills those
//! in automatically (the same idea as NeewerLite's `lights.json`).
//!
//! The catalog is embedded in the binary (`include_str!`) so it always ships;
//! `Catalog::load` also allows an external override file for field updates
//! without a rebuild.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::profile::Profile;

/// One model's capabilities. Mirrors a `[[model]]` block in `models.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    /// Friendly model name (shown to the user; written as the light's name).
    pub name: String,
    /// Numeric product IDs that appear in `NW-<id>&...` advertised names.
    #[serde(default)]
    pub product_ids: Vec<String>,
    /// Model-name substrings for older lights that advertise their name directly.
    #[serde(default)]
    pub name_matches: Vec<String>,
    /// BLE command family: `classic` | `infinity` | `home`.
    pub driver: String,
    /// Whether the light is colour-capable (drives profile selection).
    pub supports_rgb: bool,
    /// Whether the light supports green/magenta tint in CCT mode.
    #[serde(default)]
    pub supports_gm: bool,
    /// CCT scaling range, raw ×100K (25 = 2500K, 100 = 10000K).
    pub cct_min: u8,
    pub cct_max: u8,
    /// The app's `commandType`: **2** = Infinity — advanced modes (XY/RGBCW/FX)
    /// need the MAC-embedded frames (`0xB7`/`0xA9`/`0x91`), HW-verified on the
    /// TL120C; **0/1** = the direct frames (`0xB9`/`0xA8`/`0x8B`), HW-verified on
    /// the TL21C (FX renders via direct `0x8B` only). Drives driver dispatch.
    #[serde(default = "default_cmd_type")]
    pub cmd_type: u8,
    // --- Extra capability metadata (from the Android DeviceConfigInfo table).
    //     Recorded for future FX/advanced-mode work; not used by v1 profiles yet.
    /// Direct R/G/B/C/W mixing mode (opcode 0xA8).
    #[serde(default)]
    pub supports_rgbcw: bool,
    /// CIE xy colour-coordinate mode (opcode 0xB9).
    #[serde(default)]
    pub supports_xy: bool,
    /// Hardware DMX-512 input + addressing (opcode 0xCA).
    #[serde(default)]
    pub supports_dmx: bool,
    /// Built-in effect engine (the 18-effect "FX" picker, opcode 0x91).
    #[serde(default)]
    pub supports_fx: bool,
    /// Pixel/segment effect class (0 = none; 1/2 = pixel-effect family).
    #[serde(default)]
    pub pixel_classify: u8,
}

/// Absent in a hand-added `[[model]]` block ⇒ 1 (direct frames): every model the
/// extractor emits carries an explicit value, so the default only covers
/// hand-curated legacy entries, which predate the MAC-embedded frame family.
fn default_cmd_type() -> u8 {
    1
}

impl Model {
    /// The DMX profile that best fits this model's capability. Colour-capable
    /// lights get the full `advanced` mode-channel personality (CCT/HSI/RGBCW/XY/
    /// FX, all gated live by the mode channel); bi-colour ⇒ `cct_gm` if it has GM,
    /// else `cct`.
    pub fn profile(&self) -> Profile {
        if self.supports_rgb {
            Profile::Advanced
        } else if self.supports_gm {
            Profile::CctGm
        } else {
            Profile::Cct
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    #[serde(default, rename = "model")]
    pub models: Vec<Model>,
}

/// The catalog embedded at build time.
const BUILTIN_TOML: &str = include_str!("../models.toml");

impl Catalog {
    /// Parse a catalog from TOML text.
    pub fn parse(text: &str) -> Result<Catalog> {
        toml::from_str(text).context("parsing model catalog TOML")
    }

    /// The built-in catalog (parsed once, cached). Panics only if the embedded
    /// `models.toml` is malformed — which a unit test guards against.
    pub fn builtin() -> &'static Catalog {
        static CACHE: OnceLock<Catalog> = OnceLock::new();
        CACHE.get_or_init(|| Catalog::parse(BUILTIN_TOML).expect("embedded models.toml is valid"))
    }

    /// Identify a light from its advertised BLE name (e.g. `NW-20240047&00000000`
    /// or `NEEWER-RGB660`). Product-ID matches win over name matches; among name
    /// matches the longest (most specific) substring wins, so `RGB660 PRO` beats
    /// `RGB660` and `SL90 Pro` beats `SL90`.
    pub fn identify(&self, ble_name: &str) -> Option<&Model> {
        let hay = ble_name.to_uppercase();

        // 1) High-confidence: a product ID appears verbatim in the name.
        for m in &self.models {
            if m.product_ids.iter().any(|id| hay.contains(&id.to_uppercase())) {
                return Some(m);
            }
        }

        // 2) Fall back to name-substring match, preferring the longest hit.
        let mut best: Option<(&Model, usize)> = None;
        for m in &self.models {
            for nm in &m.name_matches {
                if hay.contains(&nm.to_uppercase()) {
                    let len = nm.len();
                    if best.map(|(_, b)| len > b).unwrap_or(true) {
                        best = Some((m, len));
                    }
                }
            }
        }
        best.map(|(m, _)| m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_parses_and_is_nonempty() {
        let c = Catalog::builtin();
        assert!(c.models.len() > 30, "expected a populated catalog");
        // Every entry must declare at least one way to be matched.
        for m in &c.models {
            assert!(
                !m.product_ids.is_empty() || !m.name_matches.is_empty(),
                "model {} has no product_ids or name_matches",
                m.name
            );
            // Equal is allowed: fixed-CCT lights (e.g. Apollo 150D @ 5600K).
            assert!(m.cct_min <= m.cct_max, "model {} bad cct range", m.name);
        }
    }

    #[test]
    fn identifies_tl120c_by_product_id() {
        let c = Catalog::builtin();
        // Both the documented batch and the user's newer batch resolve to TL120C.
        for name in ["NW-20240047&00000000", "NW-20230031&FFFFFFFF"] {
            let m = c.identify(name).expect("should match TL120C");
            assert_eq!(m.name, "TL120C");
            assert!(m.supports_rgb);
            assert_eq!((m.cct_min, m.cct_max), (25, 100));
            assert_eq!(m.profile(), Profile::Advanced);
            assert!(m.supports_fx);
            assert_eq!(m.driver, "classic");
        }
    }

    #[test]
    fn name_match_prefers_longest_and_specific() {
        let c = Catalog::builtin();
        // "RGB660 PRO" must not be swallowed by the "RGB660" entry.
        assert_eq!(c.identify("NEEWER-RGB660 PRO").unwrap().name, "RGB660 PRO");
        assert_eq!(c.identify("NEEWER-RGB660").unwrap().name, "RGB660");
        // SL90 Pro vs SL90.
        assert_eq!(c.identify("NEEWER-SL90 Pro").unwrap().name, "SL90 Pro");
    }

    #[test]
    fn bi_color_profile_selection() {
        let c = Catalog::builtin();
        let ms60b = c.identify("NEEWER-MS60B").unwrap();
        assert!(!ms60b.supports_rgb);
        assert_eq!(ms60b.profile(), Profile::Cct); // no GM
    }

    #[test]
    fn extracts_advanced_capabilities_from_android_table() {
        let c = Catalog::builtin();
        // TL120C: full RGB + RGBCW + XY + pixel, CCT 2500-10000K.
        let tl = c.identify("NW-20240047&00000000").unwrap();
        assert!(tl.supports_rgbcw && tl.supports_xy && tl.pixel_classify > 0);
        // HS60C PRO: the DMX-capable model from the protocol analysis (type 208).
        let hs = c.identify("NEEWER-HS60C PRO").unwrap();
        assert!(hs.supports_dmx && hs.supports_xy && hs.supports_rgbcw);
        // A bi-colour studio panel stays bi-colour.
        assert!(!c.identify("NEEWER-SNL660").unwrap().supports_rgb);
        // cmd_type: TL120C is Infinity (MAC frames); TL21C is direct — and its
        // hardware-verified override holds (FX works via 0x8B). GM stays true:
        // the 2026-07-02 "GM no-op" verdict was retracted 2026-07-04 (subtle
        // render; the TL97C's OLED tracked ±50 — nj0's haveGM is right).
        assert_eq!(tl.cmd_type, 2);
        let tl21 = c.identify("NEEWER-TL21C").unwrap();
        assert_eq!(tl21.cmd_type, 1);
        assert!(tl21.supports_fx && tl21.supports_gm && tl21.supports_rgb);
        assert_eq!((tl21.cct_min, tl21.cct_max), (25, 85));
        // Catalog is now broad (extracted from the Android DeviceConfigInfo table).
        assert!(c.models.len() > 100, "expected the full extracted catalog");
    }

    #[test]
    fn unknown_returns_none() {
        let c = Catalog::builtin();
        assert!(c.identify("LHB-B35DA7F3").is_none()); // a Valve base station
        assert!(c.identify("").is_none());
    }
}
