//! Bridge configuration (TOML). See the tracked, ready-to-edit `config.toml`
//! for a fully commented worked example of the schema.
//!
//! The binding identity for every light is its **MAC address** — stable across
//! reboots and independent of power-on/discovery order. On Linux/Windows the
//! BLE peripheral address *is* this MAC.

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
    /// How DMX from multiple inputs is combined per channel: `htp` (highest),
    /// `lowest`, or `ltp` (latest — the source that most recently **changed**
    /// a channel owns it; re-streaming unchanged data doesn't steal it back).
    /// Irrelevant with a single input.
    #[serde(default = "default_merge_mode")]
    pub merge: String,
    /// Seconds an input may go silent before it's dropped from the merge (its
    /// channels fall back to the remaining sources). `0` = never drop.
    #[serde(default = "default_merge_timeout_secs")]
    pub merge_timeout_secs: u64,
    /// Additional ArtNet listeners (`[[artnet.inputs]]`), each on its own
    /// bind IP and/or port. The `bind_ip`/`port` above is always input 0.
    #[serde(default)]
    pub inputs: Vec<ArtNetInput>,
}

/// One extra ArtNet listener (see `ArtNet::inputs`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArtNetInput {
    /// Optional label used in logs (defaults to `input1`, `input2`, …).
    #[serde(default)]
    pub name: Option<String>,
    /// Local IP to bind this input to. `0.0.0.0` = all interfaces.
    #[serde(default = "default_bind_ip")]
    pub bind_ip: String,
    /// UDP port for this input.
    #[serde(default = "default_artnet_port")]
    pub port: u16,
}

/// A fully resolved input: what to bind + how to label it in logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInput {
    pub bind_ip: String,
    pub port: u16,
    pub label: String,
}

impl ArtNet {
    /// All listeners in input order: the primary `bind_ip`/`port` first (input
    /// 0, labelled `primary`), then each `[[artnet.inputs]]` entry.
    pub fn resolved_inputs(&self) -> Vec<ResolvedInput> {
        let mut out = vec![ResolvedInput {
            bind_ip: self.bind_ip.clone(),
            port: self.port,
            label: "primary".into(),
        }];
        for (i, inp) in self.inputs.iter().enumerate() {
            let label = match inp.name.as_deref().filter(|n| !n.is_empty()) {
                Some(n) => n.to_string(),
                None => format!("input{}", i + 1),
            };
            out.push(ResolvedInput { bind_ip: inp.bind_ip.clone(), port: inp.port, label });
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Ble {
    /// Adapter selector. `"default"` = first adapter reported by the OS.
    #[serde(default = "default_adapter")]
    pub adapter: String,
    /// Max BLE state updates pushed to each light per second (coalescing cap).
    #[serde(default = "default_flush_hz")]
    pub flush_hz: u32,
    /// Liveness/status-probe interval, seconds. Each tick does a cheap GATT read
    /// (connection health — 3 consecutive misses recycle the link) and sends a
    /// best-effort status query for telemetry.
    #[serde(default = "default_probe_secs")]
    pub probe_secs: u64,
    /// Discovery-scan burst length, seconds. The bridge scans for a missing light
    /// only in bursts of this long (then pauses — see `scan_pause_secs`). While
    /// every configured light is connected it does NOT scan at all, which keeps a
    /// cheap USB BT controller from choking (continuous scanning + active
    /// connections makes the kernel log `LE Set Scan Enable` timeouts).
    #[serde(default = "default_scan_window_secs")]
    pub scan_window_secs: u64,
    /// Pause between discovery bursts, seconds, while a light is still missing.
    /// A returning/flaky fixture is picked up within roughly one burst+pause,
    /// without a continuous scan. `0` = scan continuously *while* something is
    /// missing (still off once all are connected).
    #[serde(default = "default_scan_pause_secs")]
    pub scan_pause_secs: u64,
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
    /// DMX profile name (see the README's "DMX profiles"): `cct` | `cct_gm` |
    /// `hsi` | `rgb` | `rgbcw` | `full` | `advanced` | `pixel`.
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
    /// Advanced-mode frame family — the app's per-model `commandType`. **2** =
    /// Infinity fixtures (TL120C): XY/RGBCW/FX need the MAC-embedded frames
    /// (`0xB7`/`0xA9`/`0x91`). **0/1** = direct frames (`0xB9`/`0xA8`/`0x8B`;
    /// HW-verified on the TL21C, whose FX only renders via direct `0x8B`).
    /// `add` fills this from the model catalog; defaults to 2 (the previous
    /// always-MAC behaviour) for hand-written entries.
    #[serde(default = "default_cmd_type")]
    pub cmd_type: u8,
}

impl Default for ArtNet {
    fn default() -> Self {
        Self {
            bind_ip: default_bind_ip(),
            port: default_artnet_port(),
            merge: default_merge_mode(),
            merge_timeout_secs: default_merge_timeout_secs(),
            inputs: Vec::new(),
        }
    }
}
impl Default for Ble {
    fn default() -> Self {
        Self {
            adapter: default_adapter(),
            flush_hz: default_flush_hz(),
            probe_secs: default_probe_secs(),
            scan_window_secs: default_scan_window_secs(),
            scan_pause_secs: default_scan_pause_secs(),
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
/// LTP (latest-changed takes precedence) is the least surprising default for
/// colour fixtures: whichever source last *changed* a channel wins, instead of
/// HTP's per-channel max mixing two sources' colours together.
fn default_merge_mode() -> String {
    "ltp".into()
}
/// Matches the conventional ArtNet merge data-loss window.
fn default_merge_timeout_secs() -> u64 {
    10
}
/// Sanity cap on extra `[[artnet.inputs]]` (8 listeners total).
pub const MAX_EXTRA_INPUTS: usize = 7;
fn default_adapter() -> String {
    "default".into()
}
fn default_flush_hz() -> u32 {
    15
}
fn default_probe_secs() -> u64 {
    20
}
fn default_scan_window_secs() -> u64 {
    8
}
fn default_scan_pause_secs() -> u64 {
    15
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
/// Hand-written entries without `cmd_type` keep the pre-cmd_type behaviour
/// (MAC-embedded advanced-mode frames, correct for the TL120C).
fn default_cmd_type() -> u8 {
    2
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

/// Known DMX profiles (kept in sync with `profile.rs` and the README).
pub const KNOWN_PROFILES: &[&str] =
    &["cct", "cct_gm", "hsi", "rgb", "rgbcw", "full", "advanced", "pixel"];
/// Known driver selectors.
pub const KNOWN_DRIVERS: &[&str] = &["auto", "classic", "infinity", "home"];
/// What lights do when ArtNet stops arriving (`[failsafe] mode`). Parsed once at
/// startup so the run loop never re-matches a config string, and so adding a mode
/// is a compile error everywhere it must be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailsafeMode {
    /// Keep the last commanded state forever (the lights hold it themselves).
    Hold,
    /// Force brightness to 0 (light stays powered and connected).
    Blackout,
    /// Send a BLE power-off.
    PowerOff,
}

impl FailsafeMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hold" => Some(Self::Hold),
            "blackout" => Some(Self::Blackout),
            "poweroff" => Some(Self::PowerOff),
            _ => None,
        }
    }
}

impl std::fmt::Display for FailsafeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Hold => "hold",
            Self::Blackout => "blackout",
            Self::PowerOff => "poweroff",
        })
    }
}

/// Known failsafe modes, in [`FailsafeMode`] order (config-facing names).
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

    /// Load the config, falling back to built-in defaults ONLY when the file
    /// does not exist (so `scan`/`test`/`monitor` work out of the box). An
    /// existing-but-broken file is a **hard error**, never silently replaced
    /// with defaults — a config with one bad entry must not make `ota` or
    /// `test` quietly run with the wrong adapter/port (this project has been
    /// bitten by silently-wrong configs before; see the "all lights white"
    /// entry in the README's troubleshooting section).
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.is_file() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    /// Structural validation independent of any hardware being present.
    pub fn validate(&self) -> Result<()> {
        if FailsafeMode::parse(&self.failsafe.mode).is_none() {
            bail!(
                "failsafe.mode {:?} unknown; expected one of {KNOWN_FAILSAFE_MODES:?}",
                self.failsafe.mode
            );
        }
        // [artnet]: merge mode + the input list. Bind IPs aren't parsed here —
        // a bad one fails the bind, which is already a fatal startup error.
        if crate::merge::MergeMode::parse(&self.artnet.merge).is_none() {
            bail!(
                "artnet.merge {:?} unknown; expected one of {:?}",
                self.artnet.merge,
                crate::merge::KNOWN_MERGE_MODES
            );
        }
        if self.artnet.inputs.len() > MAX_EXTRA_INPUTS {
            bail!(
                "artnet.inputs has {} entries; at most {MAX_EXTRA_INPUTS} extra inputs \
                 ({} listeners total) are supported",
                self.artnet.inputs.len(),
                MAX_EXTRA_INPUTS + 1
            );
        }
        let inputs = self.artnet.resolved_inputs();
        for inp in &inputs {
            if inp.port == 0 {
                bail!("artnet input {:?}: port must be 1..=65535", inp.label);
            }
        }
        // Two inputs on the identical (bind_ip, port) would be one stream split
        // arbitrarily between two lanes — always a config mistake. The same
        // port on two *specific* IPs is fine (the multi-IP use case), but a
        // wildcard bind claims the port on every interface, so mixing it with
        // any other input on that port would fail at bind time (EADDRINUSE on
        // Linux) — reject it here with a clearer message.
        let is_wildcard = |ip: &str| matches!(ip.trim(), "0.0.0.0" | "::" | "[::]");
        for i in 0..inputs.len() {
            for j in (i + 1)..inputs.len() {
                if inputs[i].port != inputs[j].port {
                    continue;
                }
                if inputs[i].bind_ip == inputs[j].bind_ip {
                    bail!(
                        "artnet inputs {:?} and {:?} both bind {}:{} — each input needs its own \
                         bind_ip/port combination",
                        inputs[i].label, inputs[j].label, inputs[i].bind_ip, inputs[i].port
                    );
                }
                if is_wildcard(&inputs[i].bind_ip) || is_wildcard(&inputs[j].bind_ip) {
                    bail!(
                        "artnet inputs {:?} ({}) and {:?} ({}) share port {} but one binds the \
                         wildcard address, which claims the port on every interface — use \
                         specific IPs on both, or different ports",
                        inputs[i].label, inputs[i].bind_ip,
                        inputs[j].label, inputs[j].bind_ip,
                        inputs[i].port
                    );
                }
            }
        }
        // [ble] rate/interval fields must be non-zero: flush_hz 0 has no
        // meaning, probe_secs 0 would spin the probe loop, and a
        // scan_window_secs of 0 would hammer the adapter with start/stop-scan
        // pairs — exactly the load the duty-cycled scan exists to avoid.
        // (scan_pause_secs 0 is legal: documented as "scan continuously while
        // something is missing".)
        if self.ble.flush_hz == 0 {
            bail!("[ble] flush_hz must be ≥ 1 (max BLE updates per light per second)");
        }
        if self.ble.probe_secs == 0 {
            bail!("[ble] probe_secs must be ≥ 1 (seconds between connection-health probes)");
        }
        if self.ble.scan_window_secs == 0 {
            bail!(
                "[ble] scan_window_secs must be ≥ 1 (a 0-second scan burst would just \
                 hammer the adapter with start/stop-scan; use scan_pause_secs = 0 for \
                 a continuous scan while a light is missing)"
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
            let universe_size = crate::artnet::DMX_UNIVERSE_SIZE;
            if l.address < 1 || l.address > universe_size {
                bail!("{who}: address {} out of range (1..={universe_size})", l.address);
            }
            // The whole profile must fit within the universe.
            let last = l.address as u32 + profile.channel_count() as u32 - 1;
            if last > universe_size as u32 {
                bail!(
                    "{who}: profile {:?} ({} ch) at address {} runs to channel {} (>{universe_size})",
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
            if l.cmd_type > 2 {
                bail!("{who}: cmd_type {} out of range (0..=2; 2 = MAC-embedded advanced frames)", l.cmd_type);
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

/// Escape a string for embedding in a TOML basic (double-quoted) string:
/// backslashes and quotes are escaped, control characters become spaces. BLE
/// names are usually tame, but a stray quote must not produce an unparsable
/// config (append_light validates before writing, so it would fail safe — this
/// makes it succeed instead).
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Append a `[[lights]]` block to a config file (creating it if absent),
/// preserving any existing content/comments. The result is validated before it
/// is written, so a bad addition can't corrupt the file. An absent/empty name
/// omits the `name =` line entirely (log labels then fall back to the MAC)
/// rather than writing a useless `name = ""`.
pub fn append_light(path: &Path, light: &LightCfg) -> Result<()> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let name_line = match light.name.as_deref() {
        Some(n) if !n.is_empty() => format!("name = \"{}\"\n", toml_escape(n)),
        _ => String::new(),
    };
    text.push_str(&format!(
        "\n[[lights]]\nmac = \"{}\"\n{}driver = \"{}\"\nprofile = \"{}\"\nuniverse = {}\naddress = {}\npower_on_connect = {}\ncct_min = {}\ncct_max = {}\ncmd_type = {}\n",
        normalize_mac(&light.mac),
        name_line,
        light.driver,
        light.profile,
        light.universe,
        light.address,
        light.power_on_connect,
        light.cct_min,
        light.cct_max,
        light.cmd_type,
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
            cmd_type: 1,
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
        assert_eq!(loaded.lights[0].cmd_type, 1); // round-trips through append
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_or_default_missing_vs_broken() {
        // Missing file → defaults (commands work out of the box).
        let missing = std::env::temp_dir().join(format!("nb_missing_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        let cfg = Config::load_or_default(&missing).unwrap();
        assert_eq!(cfg.artnet.port, crate::artnet::ARTNET_PORT);

        // Existing-but-broken file → hard error, NOT silent defaults.
        let broken = std::env::temp_dir().join(format!("nb_broken_{}.toml", std::process::id()));
        std::fs::write(&broken, "[artnet]]").unwrap();
        assert!(Config::load_or_default(&broken).is_err());
        // Valid TOML that fails validation is also a hard error.
        std::fs::write(&broken, "[ble]\nflush_hz = 0\n").unwrap();
        assert!(Config::load_or_default(&broken).is_err());
        let _ = std::fs::remove_file(&broken);
    }

    #[test]
    fn validate_rejects_zero_ble_intervals() {
        let mut c = Config::default();
        assert!(c.validate().is_ok());
        c.ble.flush_hz = 0;
        assert!(c.validate().is_err());
        c.ble.flush_hz = 15;
        c.ble.probe_secs = 0;
        assert!(c.validate().is_err());
        c.ble.probe_secs = 20;
        c.ble.scan_window_secs = 0;
        assert!(c.validate().is_err());
        c.ble.scan_window_secs = 8;
        c.ble.scan_pause_secs = 0; // legal: continuous scan while searching
        assert!(c.validate().is_ok());
    }

    #[test]
    fn append_light_escapes_name_and_omits_empty() {
        let path = std::env::temp_dir().join(format!("nb_escape_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // A name with a quote + backslash must survive the TOML round trip.
        let light = LightCfg {
            mac: "AA:BB:CC:DD:EE:01".into(),
            name: Some("Key \"main\" \\ rig".into()),
            driver: "auto".into(),
            profile: "full".into(),
            universe: 0,
            address: 1,
            power_on_connect: true,
            cct_min: DEFAULT_CCT_MIN,
            cct_max: DEFAULT_CCT_MAX,
            cmd_type: 2,
        };
        append_light(&path, &light).unwrap();
        // No name at all → the name line is omitted (not `name = ""`).
        let unnamed = LightCfg { mac: "AA:BB:CC:DD:EE:02".into(), name: None, address: 10, ..light.clone() };
        append_light(&path, &unnamed).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.lights[0].name.as_deref(), Some("Key \"main\" \\ rig"));
        assert_eq!(loaded.lights[1].name, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn artnet_inputs_parse_and_resolve() {
        let cfg: Config = toml::from_str(
            r#"
            [artnet]
            bind_ip = "192.168.1.4"
            port = 6454
            merge = "htp"
            merge_timeout_secs = 5

            [[artnet.inputs]]
            name = "console"
            port = 6455

            [[artnet.inputs]]
            bind_ip = "192.168.1.5"
            "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let inputs = cfg.artnet.resolved_inputs();
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0], ResolvedInput { bind_ip: "192.168.1.4".into(), port: 6454, label: "primary".into() });
        assert_eq!(inputs[1], ResolvedInput { bind_ip: "0.0.0.0".into(), port: 6455, label: "console".into() });
        // Unnamed entry gets a positional label; port defaults to 6454 —
        // legal because both 6454 binds are on specific (different) IPs.
        assert_eq!(inputs[2], ResolvedInput { bind_ip: "192.168.1.5".into(), port: 6454, label: "input2".into() });
        assert_eq!(cfg.artnet.merge, "htp");
        assert_eq!(cfg.artnet.merge_timeout_secs, 5);
    }

    #[test]
    fn artnet_defaults_are_single_input_ltp() {
        let cfg = Config::default();
        assert_eq!(cfg.artnet.merge, "ltp");
        assert_eq!(cfg.artnet.merge_timeout_secs, 10);
        assert_eq!(cfg.artnet.resolved_inputs().len(), 1);
        cfg.validate().unwrap();
    }

    #[test]
    fn validate_rejects_bad_merge_config() {
        let mut c = Config::default();
        c.artnet.merge = "average".into();
        assert!(c.validate().is_err()); // unknown merge mode

        c.artnet.merge = "ltp".into();
        c.artnet.inputs.push(ArtNetInput { name: None, bind_ip: "0.0.0.0".into(), port: 6454 });
        assert!(c.validate().is_err()); // duplicates the primary bind

        c.artnet.inputs[0].port = 6455;
        assert!(c.validate().is_ok());

        c.artnet.inputs.push(ArtNetInput { name: None, bind_ip: "0.0.0.0".into(), port: 6455 });
        assert!(c.validate().is_err()); // duplicate among extras
        c.artnet.inputs[1].bind_ip = "10.0.0.1".into();
        assert!(c.validate().is_err()); // wildcard + specific IP on one port
        c.artnet.inputs[0].bind_ip = "10.0.0.2".into(); // two specific IPs = fine
        assert!(c.validate().is_ok());

        c.artnet.inputs[1].port = 0;
        assert!(c.validate().is_err()); // port 0

        c.artnet.inputs[1].port = 6455;
        for p in 0..7u16 {
            c.artnet.inputs.push(ArtNetInput { name: None, bind_ip: "0.0.0.0".into(), port: 7000 + p });
        }
        assert!(c.validate().is_err()); // more than 7 extra inputs
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
            cmd_type: 2,
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
