//! Bridge configuration (TOML). See `NOTES.md` §8.2 for the documented schema
//! and the tracked, ready-to-edit `config.toml` for a worked example.
//!
//! The binding identity for every light is its **MAC address** — stable across
//! reboots and independent of power-on/discovery order (NOTES.md §4). On
//! Linux/Windows the BLE peripheral address *is* this MAC.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub artnet: ArtNet,
    #[serde(default)]
    pub ble: Ble,
    #[serde(default)]
    pub failsafe: Failsafe,
    #[serde(default)]
    pub logging: Logging,
    #[serde(default)]
    pub lights: Vec<LightCfg>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArtNet {
    /// Local IP to bind the ArtNet UDP listener to. `0.0.0.0` = all interfaces.
    #[serde(default = "default_bind_ip")]
    pub bind_ip: String,
    /// UDP port to listen on. Defaults to the standard ArtNet port (6454) but is
    /// configurable so the bridge isn't locked to it.
    #[serde(default = "default_artnet_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Ble {
    /// Adapter selector. `"default"` = first adapter reported by the OS.
    #[serde(default = "default_adapter")]
    pub adapter: String,
    /// Max BLE state updates pushed to each light per second (coalescing cap).
    #[serde(default = "default_flush_hz")]
    pub flush_hz: u32,
    /// Liveness RSSI-probe interval, seconds (stale-session detection).
    #[serde(default = "default_probe_secs")]
    pub probe_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Failsafe {
    /// Behaviour when ArtNet data stops. v1 implements `"hold"` only.
    #[serde(default = "default_failsafe_mode")]
    pub mode: String,
    /// Seconds of no ArtNet before acting. `0` = never (hold forever).
    #[serde(default)]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Logging {
    /// Global default level: `trace` | `debug` | `info` | `warn` | `error`.
    /// Per-destination overrides fall back to this. `debug` includes every BLE
    /// command sent to a light; `info` is the clean operational level.
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Log to the console (stderr). Machine-readable command output stays on
    /// stdout, so this never corrupts `scan --json` etc.
    #[serde(default = "default_true")]
    pub console: bool,
    /// Optional console level override (defaults to `level`).
    #[serde(default)]
    pub console_level: Option<String>,
    /// Log file path. Empty / absent = no file. Rotated by size (see below).
    #[serde(default)]
    pub file: Option<String>,
    /// Optional file level override (defaults to `level`). Handy to keep the
    /// console at `info` while the file captures full `debug` history.
    #[serde(default)]
    pub file_level: Option<String>,
    /// Rotate the log file once it exceeds this many megabytes.
    #[serde(default = "default_log_max_size_mb")]
    pub max_size_mb: u64,
    /// How many rotated files to keep (older ones are deleted).
    #[serde(default = "default_log_max_files")]
    pub max_files: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LightCfg {
    /// Binding identity — the hardware MAC, e.g. `"AA:BB:CC:DD:EE:FF"`.
    pub mac: String,
    /// Optional human label / fallback name match.
    #[serde(default)]
    pub name: Option<String>,
    /// Protocol family: `auto` | `classic` | `infinity` | `home`.
    #[serde(default = "default_driver")]
    pub driver: String,
    /// DMX profile name (see NOTES.md §8.1): `cct` | `cct_gm` | `hsi` | `rgbcw` |
    /// `full` | `advanced` | `pixel`.
    pub profile: String,
    /// ArtNet 15-bit Port-Address (Net/Sub-Net/Universe combined), 0..=32767.
    pub universe: u16,
    /// 1-based DMX start channel within the universe.
    pub address: u16,
    /// Power the light on when it first connects.
    #[serde(default = "default_true")]
    pub power_on_connect: bool,
    /// CCT scaling range, raw ×100K units (25 = 2500K, 100 = 10000K). Per-model —
    /// see the light's manual. Defaults to 32..56 (3200..5600K). The TL120C is
    /// 25..100 (2500..10000K).
    #[serde(default = "default_cct_min")]
    pub cct_min: u8,
    #[serde(default = "default_cct_max")]
    pub cct_max: u8,
}

impl Default for ArtNet {
    fn default() -> Self {
        Self { bind_ip: default_bind_ip(), port: default_artnet_port() }
    }
}
impl Default for Ble {
    fn default() -> Self {
        Self {
            adapter: default_adapter(),
            flush_hz: default_flush_hz(),
            probe_secs: default_probe_secs(),
        }
    }
}
impl Default for Failsafe {
    fn default() -> Self {
        Self { mode: default_failsafe_mode(), timeout_secs: 0 }
    }
}
impl Default for Logging {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            console: true,
            console_level: None,
            file: None,
            file_level: None,
            max_size_mb: default_log_max_size_mb(),
            max_files: default_log_max_files(),
        }
    }
}

fn default_bind_ip() -> String {
    "0.0.0.0".into()
}
fn default_artnet_port() -> u16 {
    crate::artnet::ARTNET_PORT
}
fn default_adapter() -> String {
    "default".into()
}
fn default_flush_hz() -> u32 {
    15
}
fn default_probe_secs() -> u64 {
    20
}
/// Default CCT scaling range (raw ×100K): 3200K..5600K, the common bi-color span.
pub const DEFAULT_CCT_MIN: u8 = 32;
pub const DEFAULT_CCT_MAX: u8 = 56;
fn default_cct_min() -> u8 {
    DEFAULT_CCT_MIN
}
fn default_cct_max() -> u8 {
    DEFAULT_CCT_MAX
}
fn default_failsafe_mode() -> String {
    "hold".into()
}
fn default_log_level() -> String {
    "info".into()
}
fn default_log_max_size_mb() -> u64 {
    10
}
fn default_log_max_files() -> usize {
    5
}
/// Valid `tracing` levels, low → high verbosity.
pub const KNOWN_LOG_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];
fn default_driver() -> String {
    "auto".into()
}
fn default_true() -> bool {
    true
}

/// Known DMX profiles (kept in sync with NOTES.md §8.1).
pub const KNOWN_PROFILES: &[&str] = &["cct", "cct_gm", "hsi", "rgbcw", "full", "advanced", "pixel"];
/// Known driver selectors.
pub const KNOWN_DRIVERS: &[&str] = &["auto", "classic", "infinity", "home"];
/// Known failsafe modes (only `hold` is fully implemented in v1).
pub const KNOWN_FAILSAFE_MODES: &[&str] = &["hold", "blackout", "poweroff"];

impl Config {
    /// Load and validate a config from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Structural validation independent of any hardware being present.
    pub fn validate(&self) -> Result<()> {
        if !KNOWN_FAILSAFE_MODES.contains(&self.failsafe.mode.as_str()) {
            bail!(
                "failsafe.mode {:?} unknown; expected one of {KNOWN_FAILSAFE_MODES:?}",
                self.failsafe.mode
            );
        }
        // Logging levels (global + optional per-destination overrides).
        for (field, val) in [
            ("logging.level", Some(&self.logging.level)),
            ("logging.console_level", self.logging.console_level.as_ref()),
            ("logging.file_level", self.logging.file_level.as_ref()),
        ] {
            if let Some(lvl) = val {
                if !KNOWN_LOG_LEVELS.contains(&lvl.to_lowercase().as_str()) {
                    bail!("{field} {lvl:?} unknown; expected one of {KNOWN_LOG_LEVELS:?}");
                }
            }
        }
        for (i, l) in self.lights.iter().enumerate() {
            let who = format!("lights[{i}] (mac={:?})", l.mac);
            parse_mac(&l.mac).with_context(|| format!("{who}: invalid mac"))?;
            if !KNOWN_DRIVERS.contains(&l.driver.as_str()) {
                bail!("{who}: unknown driver {:?}; expected one of {KNOWN_DRIVERS:?}", l.driver);
            }
            let profile = crate::profile::Profile::parse(&l.profile).ok_or_else(|| {
                anyhow::anyhow!("{who}: unknown profile {:?}; expected one of {KNOWN_PROFILES:?}", l.profile)
            })?;
            if l.universe > 32767 {
                bail!("{who}: universe {} out of range (0..=32767)", l.universe);
            }
            if l.address < 1 || l.address > 512 {
                bail!("{who}: address {} out of range (1..=512)", l.address);
            }
            // The whole profile must fit within the 512-channel universe.
            let last = l.address as u32 + profile.channel_count() as u32 - 1;
            if last > 512 {
                bail!(
                    "{who}: profile {:?} ({} ch) at address {} runs to channel {} (>512)",
                    l.profile, profile.channel_count(), l.address, last
                );
            }
            // CCT scaling range must be ordered (raw ×100K). Equal is allowed for
            // fixed-CCT lights (e.g. Apollo 150D @ 5600K → the CCT channel is inert).
            if l.cct_min > l.cct_max {
                bail!(
                    "{who}: cct_min {} must be ≤ cct_max {} (raw ×100K, e.g. 25..100 = 2500..10000K)",
                    l.cct_min, l.cct_max
                );
            }
        }
        // Detect duplicate MAC bindings — a config mistake that would make the
        // DMX→light mapping ambiguous.
        for i in 0..self.lights.len() {
            for j in (i + 1)..self.lights.len() {
                if mac_eq(&self.lights[i].mac, &self.lights[j].mac) {
                    bail!("duplicate light mac {:?} (lights[{i}] and lights[{j}])", self.lights[i].mac);
                }
            }
        }
        Ok(())
    }
}

/// Append a `[[lights]]` block to a config file (creating it if absent),
/// preserving any existing content/comments. The result is validated before it
/// is written, so a bad addition can't corrupt the file.
pub fn append_light(path: &Path, light: &LightCfg) -> Result<()> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "\n[[lights]]\nmac = \"{}\"\nname = \"{}\"\ndriver = \"{}\"\nprofile = \"{}\"\nuniverse = {}\naddress = {}\npower_on_connect = {}\ncct_min = {}\ncct_max = {}\n",
        normalize_mac(&light.mac),
        light.name.clone().unwrap_or_default(),
        light.driver,
        light.profile,
        light.universe,
        light.address,
        light.power_on_connect,
        light.cct_min,
        light.cct_max,
    ));

    // Parse + validate the whole file before committing it to disk.
    let cfg: Config = toml::from_str(&text).context("the resulting config would not parse")?;
    cfg.validate().context("the resulting config would be invalid")?;
    std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Parse a MAC string (`"AA:BB:CC:DD:EE:FF"`, `-` or `:` separators, any case)
/// into 6 bytes. Used both to validate config and to build Infinity payloads.
pub fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.len() != 12 {
        bail!("expected 12 hex digits, got {} in {:?}", cleaned.len(), s);
    }
    let mut out = [0u8; 6];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("bad hex byte in mac {:?}", s))?;
    }
    Ok(out)
}

/// Canonical upper-case colon form, e.g. `AA:BB:CC:DD:EE:FF`.
pub fn normalize_mac(s: &str) -> String {
    match parse_mac(s) {
        Ok(b) => format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        ),
        Err(_) => s.to_uppercase(),
    }
}

/// Compare two MAC strings for equality regardless of case/separator.
pub fn mac_eq(a: &str, b: &str) -> bool {
    match (parse_mac(a), parse_mac(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.eq_ignore_ascii_case(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mac_forms() {
        let want = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        assert_eq!(parse_mac("AA:BB:CC:DD:EE:FF").unwrap(), want);
        assert_eq!(parse_mac("aa-bb-cc-dd-ee-ff").unwrap(), want);
        assert_eq!(parse_mac("AABBCCDDEEFF").unwrap(), want);
        assert!(parse_mac("AA:BB:CC").is_err());
        assert!(parse_mac("GG:BB:CC:DD:EE:FF").is_err());
    }

    #[test]
    fn normalize_and_eq() {
        assert_eq!(normalize_mac("aa-bb-cc-dd-ee-ff"), "AA:BB:CC:DD:EE:FF");
        assert!(mac_eq("AABBCCDDEEFF", "aa:bb:cc:dd:ee:ff"));
        assert!(!mac_eq("AABBCCDDEEFF", "aa:bb:cc:dd:ee:00"));
    }

    #[test]
    fn validate_rejects_unknown_failsafe_mode() {
        let mut c = Config::default();
        assert!(c.validate().is_ok()); // default "hold"
        c.failsafe.mode = "explode".into();
        assert!(c.validate().is_err());
        c.failsafe.mode = "blackout".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn append_light_round_trips() {
        let path = std::env::temp_dir().join(format!("nb_append_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let light = LightCfg {
            mac: "aa-bb-cc-dd-ee-ff".into(),
            name: Some("Key".into()),
            driver: "auto".into(),
            profile: "full".into(),
            universe: 2,
            address: 5,
            power_on_connect: true,
            cct_min: DEFAULT_CCT_MIN,
            cct_max: DEFAULT_CCT_MAX,
        };
        append_light(&path, &light).unwrap();
        // Append a second one to confirm multiple [[lights]] blocks accumulate.
        let light2 = LightCfg { mac: "11:22:33:44:55:66".into(), ..light.clone() };
        append_light(&path, &light2).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.lights.len(), 2);
        assert_eq!(normalize_mac(&loaded.lights[0].mac), "AA:BB:CC:DD:EE:FF");
        assert_eq!(loaded.lights[0].universe, 2);
        assert_eq!(loaded.lights[1].address, 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_checks_logging_levels() {
        let mut c = Config::default();
        assert!(c.validate().is_ok()); // default level "info"
        c.logging.level = "verbose".into();
        assert!(c.validate().is_err()); // unknown global level
        c.logging.level = "debug".into();
        assert!(c.validate().is_ok());
        c.logging.file_level = Some("nope".into());
        assert!(c.validate().is_err()); // unknown per-sink override
        c.logging.file_level = Some("trace".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_catches_bad_profile_and_dupes() {
        let mut c = Config::default();
        c.lights.push(LightCfg {
            mac: "AA:BB:CC:DD:EE:FF".into(),
            name: None,
            driver: "auto".into(),
            profile: "nope".into(),
            universe: 0,
            address: 1,
            power_on_connect: true,
            cct_min: DEFAULT_CCT_MIN,
            cct_max: DEFAULT_CCT_MAX,
        });
        assert!(c.validate().is_err()); // bad profile

        c.lights[0].profile = "full".into();
        assert!(c.validate().is_ok());

        let mut dupe = c.lights[0].clone();
        dupe.mac = "aa-bb-cc-dd-ee-ff".into();
        c.lights.push(dupe);
        assert!(c.validate().is_err()); // duplicate mac
    }
}
