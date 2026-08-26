//! Bridge configuration (TOML). See the tracked `config.example.toml` for a
//! fully commented worked example of the schema — the live file it is copied to
//! is always `config.toml`, which is never tracked or shipped so that unzipping
//! a new release over an install can't clobber someone's settings.
//!
//! The binding identity for every light is its **MAC address** — stable across
//! reboots and independent of power-on/discovery order. On Linux/Windows the
//! BLE peripheral address *is* this MAC.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
// Unknown keys are a hard error, at every level. A mistyped field used to be
// silently ignored and the default used in its place, so `flush_hzz = 40` ran at
// 15 Hz, `adress = 26` left a light on channel 1, and nothing in the log said
// so — the same class of invisible-misconfiguration bug as the "all lights
// white, wrong profile loaded" story that made the startup log announce which
// config it read. Refusing the file is the only outcome the user can act on.
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct Failsafe {
    /// Behaviour when ArtNet data stops. v1 implements `"hold"` only.
    #[serde(default = "default_failsafe_mode")]
    pub mode: String,
    /// Seconds of no ArtNet before acting. `0` = never (hold forever).
    #[serde(default)]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    /// (`0xB7`/`0xA9`/`0x91`). **Anything else** = direct frames
    /// (`0xB9`/`0xA8`/`0x8B`; HW-verified on the TL21C, whose FX only renders
    /// via direct `0x8B`). `add` fills this from the model catalog; defaults to
    /// 2 (the previous always-MAC behaviour) for hand-written entries.
    ///
    /// Valid values are 0..=[`MAX_CMD_TYPE`] — see that constant for why the
    /// bound is what it is.
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
/// Largest `cmd_type` a light may declare — the top of the app's `commandType`
/// space as it appears in the model catalog (`models.toml` currently emits 0, 1,
/// 2 and **3**; the 3 belongs to the `ZRP`).
///
/// The bound exists only to catch a typo such as `cmd_type = 20`, which would
/// silently mean "direct frames" and leave a TL120C ignoring every advanced
/// mode. It must therefore never be tighter than what the catalog itself emits:
/// it used to be 2, so `add`-ing a `ZRP` built a config the validator then
/// refused — after the scan, the blink and every prompt — complaining about a
/// field the user never typed, and the light could not be added at all.
/// `models::tests::every_model_produces_a_config_the_validator_accepts` is what
/// keeps the two in step if the catalog is ever regenerated wider.
pub const MAX_CMD_TYPE: u8 = 3;
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

    /// What a command actually gets from a config load attempt.
    ///
    /// A **missing** file falls back to built-in defaults, so `scan`/`test`/
    /// `monitor` work out of the box. An **existing but broken** one is a hard
    /// error, never silently replaced with defaults — a config with one bad
    /// entry must not make `ota` or `test` quietly run against the wrong
    /// adapter/port (this project has been bitten by silently-wrong configs
    /// before; see the "all lights white" entry in the README's troubleshooting
    /// section).
    ///
    /// Takes the already-performed [`Config::load`] result and whether the file
    /// exists, rather than reading the file itself: `main` needs both facts
    /// anyway (logging is configured from the config before any command runs),
    /// so one invocation reads and parses the config exactly once — and the
    /// state `main` announces is provably the same one the command acts on.
    pub fn for_command(path: &Path, exists: bool, loaded: Result<Self>) -> Result<Self> {
        if exists {
            loaded.with_context(|| format!("config {} exists but is invalid", path.display()))
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
        // Labels are how an input is identified in every log line `monitor` and
        // `run` emit, so two inputs sharing one make those lines undiagnosable.
        // Checked on the RESOLVED labels, which also catches a name colliding
        // with an auto-generated one (`primary`, `input1`, …).
        for i in 0..inputs.len() {
            for j in (i + 1)..inputs.len() {
                if inputs[i].label == inputs[j].label {
                    bail!(
                        "two artnet inputs resolve to the same log label {:?} (inputs \
                         #{} and #{}, where #0 is the primary [artnet] block) — give each \
                         [[artnet.inputs]] a distinct `name`, and don't reuse `primary` \
                         or `inputN`",
                        inputs[i].label, i, j
                    );
                }
            }
        }
        // Two inputs on the identical (bind_ip, port) would be one stream split
        // arbitrarily between two lanes — always a config mistake. The same
        // port on two *specific* IPs is fine (the multi-IP use case), but a
        // wildcard bind claims the port on every interface, so mixing it with
        // any other input on that port would fail at bind time (EADDRINUSE on
        // Linux) — reject it here with a clearer message.
        // Every spelling of the unspecified address, not just the three common
        // ones: `::0` and `0:0:0:0:0:0:0:0` are the same wildcard as `::` and
        // used to slip past this check, turning a clear config error into a bare
        // EADDRINUSE at bind time. Parsing is what makes that exhaustive.
        //
        // Comparing two inputs for "same address" needs the same treatment, and
        // for the same reason: `::1` and `0:0:0:0:0:0:0:1` are one address, and
        // a raw string compare called them different, passed validation, and
        // then died at bind with a bare `EADDRINUSE` / `os error 10048`. So
        // compare PARSED addresses whenever both parse, and fall back to the
        // trimmed strings when one doesn't (an interface name or a hostname —
        // `artnet::bind` resolves those, and the identical text is still an
        // identical bind).
        let parse_ip = |ip: &str| {
            let t = ip.trim();
            let t = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')).unwrap_or(t);
            t.parse::<std::net::IpAddr>()
        };
        let is_wildcard = |ip: &str| parse_ip(ip).is_ok_and(|a| a.is_unspecified());
        let same_addr = |a: &str, b: &str| match (parse_ip(a), parse_ip(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => a.trim() == b.trim(),
        };
        for i in 0..inputs.len() {
            for j in (i + 1)..inputs.len() {
                if inputs[i].port != inputs[j].port {
                    continue;
                }
                if same_addr(&inputs[i].bind_ip, &inputs[j].bind_ip) {
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
        // Turning the console off with no file sink installs NO logging layer at
        // all, so the process goes completely dark — measured: a failing command
        // exits 1 having written **zero bytes**, including the error explaining
        // why. `console = false` reads as "quieter", not "discard every
        // diagnostic this program will ever produce", and there is no way to
        // discover the difference from the outside. Same rule as the zero-valued
        // settings below: refuse a setting that silently does something else.
        // (A config that fails to validate still gets DEFAULT logging — `main`
        // falls back with `unwrap_or_default` — so this message is always
        // visible, even though it is a message about logging being off.)
        let file_sink = self.logging.file.as_deref().is_some_and(|p| !p.is_empty());
        if !self.logging.console && !file_sink {
            bail!(
                "[logging] console = false with no `file` set would disable logging \
                 entirely — even fatal errors would be silent. Set `file = \"…\"` to \
                 log to disk instead, or leave `console = true`."
            );
        }
        // Rotation sizes only matter when a file sink is configured. Zero is a
        // typo, not a setting — it reads as "unlimited"/"no rotations" but the
        // writer clamps it to 1, so the user silently gets a 1 MB / 1-file log.
        // Say so instead of quietly doing something else (the same rule the
        // [ble] zero-value checks above exist for). The condition below is the
        // same "is the file sink on?" test logging::init uses.
        if file_sink {
            if self.logging.max_size_mb == 0 {
                bail!("[logging] max_size_mb must be ≥ 1 (size in MB at which the log file rotates)");
            }
            if self.logging.max_files == 0 {
                bail!("[logging] max_files must be ≥ 1 (how many rotated log files to keep)");
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
            let max_pa = crate::artnet::MAX_PORT_ADDRESS;
            if l.universe > max_pa {
                bail!("{who}: universe {} out of range (0..={max_pa})", l.universe);
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
            if l.cmd_type > MAX_CMD_TYPE {
                bail!(
                    "{who}: cmd_type {} out of range (0..={MAX_CMD_TYPE}; 2 = MAC-embedded \
                     advanced frames, anything else = direct)",
                    l.cmd_type
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

/// Escape a string for embedding in a TOML basic (double-quoted) string:
/// backslashes and quotes are escaped, control characters become spaces. BLE
/// names are usually tame, but a stray quote must not produce an unparsable
/// config (append_light validates before writing, so it would fail safe — this
/// makes it succeed instead). TOML bans U+0000..U+0008, U+000A..U+001F AND
/// U+007F (DEL) in basic strings, so DEL must be spaced out too, or a name
/// carrying it would still fail validation.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Filename of the commented template that ships in the release zips. The
/// bridge never loads it: the live config is always `config.toml`, so dropping
/// a new release on top of an existing install leaves the user's file alone.
pub const EXAMPLE_FILE: &str = "config.example.toml";

/// Locate the shipped example config for `config_path`: beside that path first
/// (an explicit `--config /etc/nb/config.toml` usually has it there), then
/// beside the executable, then the working directory — the same order the
/// config resolver itself searches, so the hint always names a real file.
pub fn find_example(config_path: &Path) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = config_path.parent() {
        // `parent()` of a bare filename is "" — that means the working
        // directory, which the last candidate covers.
        if !dir.as_os_str().is_empty() {
            dirs.push(dir.to_path_buf());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs.push(PathBuf::from("."));
    for dir in dirs {
        let candidate = dir.join(EXAMPLE_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Create `path` by copying the shipped example, so a first-run config keeps
/// every documented default and comment instead of being a bare file holding
/// nothing but the light that was just added. `Ok(None)` = no example found
/// (source builds, or the user moved it); the caller carries on and lets
/// [`append_light`] create the file from scratch.
pub fn seed_from_example(path: &Path) -> Result<Option<PathBuf>> {
    let Some(example) = find_example(path) else {
        return Ok(None);
    };
    // Never overwrite an existing config — callers check too, but this is the
    // function that would destroy someone's settings if it were ever wrong.
    if path.exists() {
        return Ok(None);
    }
    std::fs::copy(&example, path)
        .with_context(|| format!("copying {} to {}", example.display(), path.display()))?;
    Ok(Some(example))
}

/// Append a `[[lights]]` block to a config file (creating it if absent),
/// preserving any existing content/comments. The result is validated before it
/// is written, so a bad addition can't corrupt the file. An absent/empty name
/// omits the `name =` line entirely (log labels then fall back to the MAC)
/// rather than writing a useless `name = ""`.
pub fn append_light(path: &Path, light: &LightCfg) -> Result<()> {
    // Only a genuinely-missing file may be treated as empty. Any other read
    // failure (permissions, I/O error, non-UTF-8 content) MUST abort: falling
    // through with an empty string would make the write below replace the
    // user's whole config with a file holding nothing but this one light.
    let mut text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!(
                "reading {} — refusing to append to a config that exists but cannot be read",
                path.display()
            )))
        }
    };
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
    write_atomic(path, &text)?;
    Ok(())
}

/// Replace `path`'s contents atomically: fill a sibling temp file, flush it to
/// disk, then rename it over the target.
///
/// A plain `fs::write` truncates the existing file *first*, so a crash or power
/// loss mid-write leaves the config empty or half-written. This is the only code
/// path that rewrites the live `config.toml` — the very file the
/// `config.example.toml` split exists to protect from being destroyed — so it
/// gets the matching guarantee: the old contents stay intact until the complete
/// new file is on disk, and the rename is atomic (same directory ⇒ same
/// filesystem) and overwrites on both Unix and Windows.
///
/// NOT covered: an fsync of the *directory entry*, which POSIX additionally
/// requires for the rename itself to survive a power loss. The file can never be
/// torn either way; at worst the very last `add` is lost.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;

    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    // Any failure past this point must not leave the temp file lying next to the
    // user's config, so every error path clears it.
    let filled = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        // Get the bytes to the platter before the rename makes them visible.
        f.sync_all()
    })();
    if let Err(e) = filled {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::from(e)
            .context(format!("writing the temporary file {}", tmp.display())));
    }

    // A fresh temp file carries default (umask) permissions; carry over the ones
    // the config already had so the rename can't silently loosen them.
    // Best-effort: an exotic filesystem must not fail the whole `add`.
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::from(e)
            .context(format!("replacing {} with {}", path.display(), tmp.display())));
    }
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

    /// The shipped template must always be a valid config — it is what users
    /// copy to `config.toml`, and what `add` seeds a first-run config from, so
    /// a typo in it would break the out-of-the-box path for everyone.
    #[test]
    fn example_config_is_valid() {
        let text = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(text).expect("config.example.toml must parse");
        cfg.validate().expect("config.example.toml must validate");
        // Every light block is commented out: shipping real fixtures would give
        // a fresh install someone else's MACs to hunt for.
        assert!(cfg.lights.is_empty(), "the example config must not define lights");
    }

    #[test]
    fn seed_from_example_copies_then_refuses_to_clobber() {
        let dir = std::env::temp_dir().join(format!("nb_seed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let example = dir.join(EXAMPLE_FILE);
        let config = dir.join("config.toml");
        std::fs::write(&example, "[artnet]
port = 6455
").unwrap();

        let used = seed_from_example(&config).unwrap();
        assert_eq!(used.as_deref(), Some(example.as_path()));
        assert_eq!(Config::load(&config).unwrap().artnet.port, 6455);

        // A second run must not touch the file the user has since edited.
        std::fs::write(&config, "[artnet]
port = 6999
").unwrap();
        assert_eq!(seed_from_example(&config).unwrap(), None);
        assert_eq!(Config::load(&config).unwrap().artnet.port, 6999);

        // With no example beside the target, the search falls back to the
        // executable's directory and then the working directory (the crate root
        // under `cargo test`), which is how a release install finds the copy
        // sitting next to the binary.
        std::fs::remove_file(&example).unwrap();
        std::fs::remove_file(&config).unwrap();
        let used = seed_from_example(&config).unwrap().expect("falls back to the CWD example");
        assert_eq!(used.file_name().unwrap(), EXAMPLE_FILE);
        assert!(Config::load(&config).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// The rule every command's config resolution follows. `main` serves it from
    /// the single parse it already performed, so this is the only place the rule
    /// itself is pinned.
    #[test]
    fn for_command_missing_vs_broken() {
        // Missing file → defaults (commands work out of the box).
        let missing = std::env::temp_dir().join(format!("nb_missing_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        let cfg = Config::for_command(&missing, false, Config::load(&missing)).unwrap();
        assert_eq!(cfg.artnet.port, crate::artnet::ARTNET_PORT);

        // Existing-but-broken file → hard error, NOT silent defaults.
        let broken = std::env::temp_dir().join(format!("nb_broken_{}.toml", std::process::id()));
        std::fs::write(&broken, "[artnet]]").unwrap();
        assert!(Config::for_command(&broken, true, Config::load(&broken)).is_err());
        // Valid TOML that fails validation is also a hard error.
        std::fs::write(&broken, "[ble]\nflush_hz = 0\n").unwrap();
        assert!(Config::for_command(&broken, true, Config::load(&broken)).is_err());

        // A good file is passed straight through.
        std::fs::write(&broken, "[artnet]\nport = 6789\n").unwrap();
        let cfg = Config::for_command(&broken, true, Config::load(&broken)).unwrap();
        assert_eq!(cfg.artnet.port, 6789);
        let _ = std::fs::remove_file(&broken);
    }

    /// `add` must never be able to leave the live config truncated or littered
    /// with temp files — the whole point of the example/live split.
    #[test]
    fn append_light_replaces_the_config_atomically() {
        let path = std::env::temp_dir().join(format!("nb_atomic_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
        let _ = std::fs::remove_file(&tmp);

        // Pre-existing settings the user would hate to lose.
        std::fs::write(&path, "[artnet]\nport = 6789\n\n# keep this comment\n").unwrap();
        let light = LightCfg {
            mac: "AA:BB:CC:DD:EE:05".into(),
            name: None,
            driver: "auto".into(),
            profile: "rgb".into(),
            universe: 0,
            address: 1,
            power_on_connect: true,
            cct_min: DEFAULT_CCT_MIN,
            cct_max: DEFAULT_CCT_MAX,
            cmd_type: 2,
        };
        append_light(&path, &light).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("port = 6789"), "existing settings must survive");
        assert!(text.contains("# keep this comment"), "comments must survive");
        assert_eq!(Config::load(&path).unwrap().lights.len(), 1);
        assert!(!tmp.exists(), "the temp file must not be left behind");
        let _ = std::fs::remove_file(&path);
    }

    /// Guards the error path of the atomic write: a failure while producing the
    /// new contents must leave the user's config byte-identical and say clearly
    /// what went wrong. That is the property that makes a torn write impossible
    /// — the target is never opened for writing until a complete new file exists
    /// beside it — and it is the closest a unit test can get, since the failure
    /// the rename actually defends against (a crash or power loss between
    /// truncate and write) can't be produced deterministically here.
    ///
    /// The failure is induced by making the temp path un-creatable: a directory
    /// sits where the temp FILE would go, so `File::create` fails on every
    /// platform.
    #[test]
    fn a_failed_write_leaves_the_existing_config_intact() {
        let path = std::env::temp_dir().join(format!("nb_failwrite_{}.toml", std::process::id()));
        let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&tmp);

        let original = "[artnet]\nport = 6789\n\n# irreplaceable\n";
        std::fs::write(&path, original).unwrap();
        std::fs::create_dir(&tmp).unwrap(); // blocks the temp file

        let light = LightCfg {
            mac: "AA:BB:CC:DD:EE:06".into(),
            name: None,
            driver: "auto".into(),
            profile: "rgb".into(),
            universe: 0,
            address: 1,
            power_on_connect: true,
            cct_min: DEFAULT_CCT_MIN,
            cct_max: DEFAULT_CCT_MAX,
            cmd_type: 2,
        };
        let err = append_light(&path, &light).expect_err("the write must fail here");
        assert!(format!("{err:#}").contains("temporary file"), "got: {err:#}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the existing config must survive a failed write"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_file(&path);
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
    fn append_light_refuses_an_unreadable_config() {
        // A config that EXISTS but cannot be read (here: invalid UTF-8) must be
        // a hard error: the old `read_to_string(..).unwrap_or_default()` treated
        // every read failure as "empty file" and the validated write below then
        // REPLACED the user's whole config with one holding only the new light.
        let path = std::env::temp_dir().join(format!("nb_unreadable_{}.toml", std::process::id()));
        let original: &[u8] = &[0xFF, 0xFE, b'j', b'u', b'n', b'k'];
        std::fs::write(&path, original).unwrap();
        let light = LightCfg {
            mac: "AA:BB:CC:DD:EE:03".into(),
            name: None,
            driver: "auto".into(),
            profile: "full".into(),
            universe: 0,
            address: 1,
            power_on_connect: true,
            cct_min: DEFAULT_CCT_MIN,
            cct_max: DEFAULT_CCT_MAX,
            cmd_type: 2,
        };
        let err = append_light(&path, &light).expect_err("unreadable config must be an error");
        assert!(format!("{err:#}").contains("cannot be read"), "got: {err:#}");
        assert_eq!(std::fs::read(&path).unwrap(), original, "the file must be left untouched");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_light_survives_a_del_in_the_name() {
        // U+007F (DEL) is banned in TOML basic strings just like the C0 controls
        // (it is a valid BLE-name byte); toml_escape must space it out or the
        // validated write fails on a name the escape function exists to save.
        let path = std::env::temp_dir().join(format!("nb_del_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let light = LightCfg {
            mac: "AA:BB:CC:DD:EE:04".into(),
            name: Some(format!("Key{}rig", char::from(0x7F))),
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
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.lights[0].name.as_deref(), Some("Key rig"));
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
    fn unknown_config_keys_are_rejected_at_every_level() {
        // A mistyped key used to be dropped on the floor and the default used
        // instead, silently: `flush_hzz = 40` ran at 15 Hz and `adress = 26` left
        // the light on channel 1, with nothing in the log to say so. Every
        // deserialized struct now denies unknown fields, and serde names the
        // offender plus the valid keys, so the message is actionable.
        let cases = [
            ("[artnetx]\nport = 1\n", "artnetx"),
            ("[artnet]\nprot = 6454\n", "prot"),
            ("[ble]\nflush_hzz = 40\n", "flush_hzz"),
            ("[failsafe]\ntimeout_sec = 5\n", "timeout_sec"),
            ("[logging]\nlvl = \"info\"\n", "lvl"),
            ("[[artnet.inputs]]\nbindip = \"0.0.0.0\"\n", "bindip"),
            ("[[lights]]\nmac = \"AA:BB:CC:DD:EE:FF\"\nadress = 26\n", "adress"),
        ];
        for (text, offender) in cases {
            let err = toml::from_str::<Config>(text)
                .expect_err(&format!("{offender} must be rejected, not ignored"));
            let msg = err.to_string();
            assert!(msg.contains(offender), "message must name the bad key: {msg}");
            assert!(msg.contains("unknown field"), "{msg}");
        }
        // …and a correct file still parses, including every optional block being
        // absent (over-strict denial would break a minimal config).
        toml::from_str::<Config>("").unwrap();
        toml::from_str::<Config>("[ble]\nflush_hz = 40\n").unwrap();
    }

    #[test]
    fn every_spelling_of_the_wildcard_address_is_caught() {
        // The check used to compare against three literal strings, so `::0` and
        // the fully-expanded form walked past it and failed at bind time with a
        // bare EADDRINUSE instead of this message. Each of these binds the port
        // on every interface, so pairing it with ANY other input on that port is
        // a config error.
        for wildcard in ["0.0.0.0", "::", "[::]", "::0", "0:0:0:0:0:0:0:0", " 0.0.0.0 ", "0000:0000:0000:0000:0000:0000:0000:0000"] {
            let mut c = Config::default();
            c.artnet.bind_ip = wildcard.into();
            c.artnet.port = 6454;
            c.artnet.inputs.push(ArtNetInput {
                name: Some("console".into()),
                bind_ip: "10.0.0.1".into(),
                port: 6454,
            });
            let err = c.validate().expect_err(&format!("{wildcard} must be rejected"));
            assert!(
                format!("{err:#}").contains("wildcard address"),
                "{wildcard}: wrong error: {err:#}"
            );
        }
        // A specific address on a shared port stays legal — that IS the multi-IP
        // use case, and over-matching here would break it.
        let mut c = Config::default();
        c.artnet.bind_ip = "10.0.0.2".into();
        c.artnet.inputs.push(ArtNetInput {
            name: Some("console".into()),
            bind_ip: "10.0.0.1".into(),
            port: 6454,
        });
        c.validate().unwrap();
        // …including addresses that merely start with a zero.
        c.artnet.bind_ip = "0.0.0.1".into();
        c.validate().unwrap();
    }

    #[test]
    fn duplicate_bind_addresses_are_caught_however_they_are_spelled() {
        // The duplicate-input check compared bind_ip as raw TEXT, so two
        // spellings of ONE address passed validation and then died at bind with
        // a bare `EADDRINUSE` / `os error 10048` — the exact gap the wildcard
        // check above already closed by parsing.
        for (a, b) in [
            ("::1", "0:0:0:0:0:0:0:1"),
            ("::1", "[::1]"),
            ("10.0.0.1", " 10.0.0.1 "),
            ("::ffff:10.0.0.1", "::ffff:10.0.0.1"),
            ("fe80::1", "FE80::1"), // IPv6 literals are case-insensitive
        ] {
            let mut c = Config::default();
            c.artnet.bind_ip = a.into();
            c.artnet.port = 6454;
            c.artnet.inputs.push(ArtNetInput {
                name: Some("console".into()),
                bind_ip: b.into(),
                port: 6454,
            });
            let err = c.validate().expect_err(&format!("{a} vs {b} must be rejected"));
            assert!(
                format!("{err:#}").contains("each input needs its own"),
                "{a} vs {b}: wrong error: {err:#}"
            );
        }

        // Genuinely different addresses on one port stay legal — that IS the
        // multi-IP use case, and over-matching here would break it.
        for (a, b) in [("10.0.0.1", "10.0.0.2"), ("::1", "::2"), ("10.0.0.1", "::1")] {
            let mut c = Config::default();
            c.artnet.bind_ip = a.into();
            c.artnet.port = 6454;
            c.artnet.inputs.push(ArtNetInput {
                name: Some("console".into()),
                bind_ip: b.into(),
                port: 6454,
            });
            c.validate().unwrap_or_else(|e| panic!("{a} vs {b} must be allowed: {e:#}"));
        }

        // A name that doesn't parse as an IP falls back to a text compare —
        // identical text is still an identical bind.
        let mut c = Config::default();
        c.artnet.bind_ip = "my-host.local".into();
        c.artnet.port = 6454;
        c.artnet.inputs.push(ArtNetInput {
            name: Some("console".into()),
            bind_ip: "my-host.local".into(),
            port: 6454,
        });
        assert!(c.validate().is_err(), "identical hostnames on one port must be rejected");
    }

    #[test]
    fn validate_rejects_duplicate_input_labels() {
        // Every `monitor`/`run` log line is tagged with the input label, so two
        // inputs sharing one make those lines impossible to attribute.
        let mut c = Config::default();
        c.artnet.inputs.push(ArtNetInput {
            name: Some("console".into()),
            bind_ip: "0.0.0.0".into(),
            port: 6455,
        });
        c.artnet.inputs.push(ArtNetInput {
            name: Some("console".into()),
            bind_ip: "0.0.0.0".into(),
            port: 6456,
        });
        assert!(c.validate().is_err(), "two inputs named 'console' must be rejected");

        // A name colliding with an auto-generated label counts too.
        c.artnet.inputs[1].name = Some("primary".into());
        assert!(c.validate().is_err(), "a name may not collide with the primary label");

        c.artnet.inputs[1].name = Some("desk".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_log_rotation_sizes() {
        // Zero reads as "unlimited" but the writer clamps it to 1, so the user
        // would silently get a 1 MB / 1-file log. Only checked when a file sink
        // is actually configured.
        let mut c = Config::default();
        c.logging.max_size_mb = 0;
        assert!(c.validate().is_ok(), "no file sink ⇒ rotation settings are inert");

        c.logging.file = Some("bridge.log".into());
        assert!(c.validate().is_err(), "max_size_mb = 0 with a file sink must be rejected");

        c.logging.max_size_mb = 10;
        c.logging.max_files = 0;
        assert!(c.validate().is_err(), "max_files = 0 must be rejected");

        c.logging.max_files = 5;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_rejects_a_completely_silenced_logger() {
        // `console = false` with no file installs no logging layer at all, so the
        // binary writes nothing whatsoever — measured: a failing command exited 1
        // with zero bytes of output, error included.
        let mut c = Config::default();
        c.logging.console = false;
        let err = c.validate().expect_err("a silent logger must be rejected");
        assert!(format!("{err:#}").contains("disable logging entirely"), "{err:#}");

        // An empty `file` is the same as no file (that's how `logging::init`
        // tests it), so it must not be a way round the check.
        c.logging.file = Some(String::new());
        assert!(c.validate().is_err(), "an empty file path is not a file sink");

        // Console off WITH a file is a perfectly ordinary daemon setup.
        c.logging.file = Some("bridge.log".into());
        c.validate().unwrap();

        // …and console on with no file is the default.
        let c = Config::default();
        assert!(c.logging.console && c.logging.file.is_none());
        c.validate().unwrap();
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
