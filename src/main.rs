//! Neewer-Bridge CLI entry point.
//!
//! Key subcommands:
//! - `scan`  — discover NEW Neewer lights (not in config), list name / MAC / RSSI.
//! - `test`  — connect to one light by MAC and prove the BLE path (blink + CCT).
//! - `run`   — run the full bridge. Also the DEFAULT: no subcommand ⇒ `run`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{ArgAction, Parser, Subcommand};
use tracing::{error, info, warn};

use neewer_bridge::config::Config;
use neewer_bridge::{bridge, commands, logging};

#[derive(Parser)]
#[command(name = "neewer-bridge", version, about = "ArtNet → Neewer Bluetooth light bridge")]
struct Cli {
    /// Path to the TOML config file. Default: `config.toml` next to the
    /// executable if present, else `config.toml` in the working directory.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Increase log verbosity (-v debug, -vv trace + BLE wire logs).
    #[arg(short = 'v', long, global = true, action = ArgAction::Count)]
    verbose: u8,

    /// Subcommand. Omitted ⇒ run the bridge (same as `run`).
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List BLE adapters (index + name) for the `[ble] adapter` config setting.
    Adapters,
    /// Scan for NEW Neewer lights (not already in the config) and list
    /// name / MAC / RSSI. `--all` shows every device, incl. configured ones.
    Scan {
        /// Scan duration in seconds.
        #[arg(long, default_value_t = 6)]
        seconds: u64,
        /// Show all BLE devices, not just Neewer ones.
        #[arg(long)]
        all: bool,
        /// Output machine-readable JSON (for scripting).
        #[arg(long)]
        json: bool,
    },
    /// Add a light to the config. With no `--mac`, runs interactively (scan +
    /// blink-to-identify + prompts); with `--mac`, runs non-interactively. The
    /// light's model is identified from its BLE name against the catalog, so
    /// driver/profile/CCT-range are filled automatically — any flag overrides.
    Add {
        /// Light MAC. If given, runs non-interactively.
        #[arg(long)]
        mac: Option<String>,
        /// Protocol driver: auto | classic | infinity | home (default: from model).
        #[arg(long)]
        driver: Option<String>,
        /// DMX profile: cct | cct_gm | hsi | rgbcw | full | advanced | pixel (default: from model).
        #[arg(long)]
        profile: Option<String>,
        /// ArtNet universe / Port-Address (required with --mac).
        #[arg(long)]
        universe: Option<u16>,
        /// DMX start address (required with --mac).
        #[arg(long)]
        address: Option<u16>,
        /// Optional light name (default: from model).
        #[arg(long)]
        name: Option<String>,
        /// Override CCT range minimum, raw ×100K (default: from model).
        #[arg(long)]
        cct_min: Option<u8>,
        /// Override CCT range maximum, raw ×100K (default: from model).
        #[arg(long)]
        cct_max: Option<u8>,
        /// Blink the light to identify it (non-interactive mode).
        #[arg(long)]
        blink: bool,
    },
    /// List the known light-model catalog (capabilities `add` matches against).
    Models,
    /// Show configured lights and their DMX channel mapping (absolute universe +
    /// channel per parameter). Reads the config; requires a valid one.
    Lights,
    /// Send ArtDmx to drive the bridge/a node (no console needed). Test helper.
    ArtnetSend {
        /// Destination IP.
        #[arg(long, default_value = "127.0.0.1")]
        target: String,
        /// Destination UDP port.
        #[arg(long, default_value_t = 6454)]
        port: u16,
        /// ArtNet universe / Port-Address.
        #[arg(long, default_value_t = 0)]
        universe: u16,
        /// DMX start address for the channel values.
        #[arg(long, default_value_t = 1)]
        address: u16,
        /// Comma-separated channel values, e.g. 255,128,64.
        #[arg(long, value_delimiter = ',', required = true)]
        channels: Vec<u8>,
        /// Stream at this rate (Hz). Omit for a single packet.
        #[arg(long)]
        hz: Option<f64>,
        /// Stream duration in seconds (with --hz).
        #[arg(long, default_value_t = 2.0)]
        seconds: f64,
    },
    /// Connect to a device and dump its full GATT (identify unknown lights).
    Inspect {
        /// Target MAC.
        mac: String,
        /// How long to wait to find the device, seconds.
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Connect to one light by MAC and prove BLE control (blink + set CCT).
    Test {
        /// Target light MAC, e.g. AA:BB:CC:DD:EE:FF.
        mac: String,
        /// Protocol family to speak: classic | infinity | home | auto.
        #[arg(long, default_value = "auto")]
        driver: String,
        /// How long to wait to find the light, seconds.
        #[arg(long, default_value_t = 8)]
        seconds: u64,
        /// After CCT, cycle HSI red→green→blue to probe RGB capability (watch
        /// whether the light changes colour — confirms RGB vs bi-color).
        #[arg(long)]
        colors: bool,
        /// Probe the advanced modes: cycle RGBCW, XY, and a few FX effects (watch
        /// the light to confirm each engages). Implies the light supports them.
        #[arg(long)]
        modes: bool,
        /// Probe per-segment PIXEL control (0xB0): paint the tube with multi-colour
        /// palettes so distinct bands appear along it. TL-series pixel fixtures only
        /// (e.g. TL120C); other lights ignore it. Latches pixel mode — the probe
        /// power-cycles to exit.
        #[arg(long)]
        pixel: bool,
        /// Send ONE specific frame and hold it (for guided one-at-a-time testing).
        /// The light keeps the state after disconnect. SPEC is one of:
        ///   cct:<kelvin>:<bri>            e.g. cct:5600:40 (2-byte form)
        ///   cctgm:<kelvin>:<gm>:<bri>     e.g. cctgm:5600:-50:40 (4-byte GM form, gm -50..50)
        ///   hsi:<hue>:<sat>:<bri>         e.g. hsi:0:100:80   (hue 0-360)
        ///   xy:<x>:<y>:<bri>              e.g. xy:6400:3300:80 (x,y ×10000; by-MAC 0xB7)
        ///   xydirect:<x>:<y>:<bri>        same but the direct 0xB9 form (non-Infinity lights)
        ///   fx:<id>:<bri>                 e.g. fx:12:80        (effect id 1-18, MAC 0x91)
        ///   fxdirect:<id>:<bri>           same 18 effects via the direct 0x8B frame
        ///   scene:<id>:<bri>              old 9-scene 0x88, direct (id 1-9)
        ///   pixel:<hue,hue,...>:<eff>:<speed>  e.g. pixel:0,240:1:40
        ///   pixfx:<id>                    per-effect pixel probe (id 1-10, app defaults)
        ///   rgbcwmac:<r>:<g>:<b>[:cw:ww:bri] RGBCW via by-MAC 0xA9 (production form; rgbcw: = direct 0xA8, ignored)
        ///   warmdim                       dim warm white (safe end state)
        /// Non-CCT/pixel specs first send a CCT-white frame to clear any pixel/FX latch.
        #[arg(long)]
        set: Option<String>,
        /// Read device status (firmware version, battery, temperature, power/mode) and
        /// print the decoded replies. Non-mutating — no blink, no colour change.
        #[arg(long)]
        status: bool,
    },
    /// Flash a firmware image to a light over the custom 0x78 OTA block protocol.
    ///
    /// SAFETY: `--check` only probes (link + OTA type) and never writes firmware;
    /// the real flash requires `--confirm`. A link-stability precheck aborts before
    /// any firmware byte is sent if the connection will not hold, so a marginal link
    /// fails safe. The device drives the transfer via ACKs and validates a check-code
    /// before committing — a dropped block fails cleanly and is retryable, not a brick.
    /// STOP THE MAIN BRIDGE FIRST (it fights this tool for the same adapter/MAC).
    Ota {
        /// Target light MAC, e.g. F9:B8:B6:15:F9:8D.
        mac: String,
        /// Path to the firmware .bin (e.g. the TL60-3_V3.0.5_*.bin from Neewer's OTA server).
        #[arg(long)]
        file: PathBuf,
        /// Firmware version being flashed, "MAJOR.MINOR.PATCH" (display metadata in the
        /// 0x96 header — should match the .bin). Omitted ⇒ derived from the filename's
        /// `V<maj>.<min>.<patch>` marker; an error if the filename has none.
        #[arg(long)]
        version: Option<String>,
        /// Cosmetic device/model name written into the header (device ignores it).
        /// Omitted ⇒ the firmware filename stem.
        #[arg(long)]
        name: Option<String>,
        /// Probe only: connect, run the link-stability check, resolve the OTA block
        /// type — but do NOT write firmware. Use this first to confirm the link holds.
        #[arg(long)]
        check: bool,
        /// Actually flash. Required for the real write (guards against accidents).
        #[arg(long)]
        confirm: bool,
        /// Seconds the link must stay connected in the pre-flash stability check.
        #[arg(long, default_value_t = 20)]
        settle_secs: u64,
        /// How long to wait to find the light, seconds.
        #[arg(long, default_value_t = 20)]
        seconds: u64,
        /// Delay between the 20-byte GATT fragments of each OTA frame, milliseconds.
        /// On a marginal link, raising this (e.g. 20-40) gives the device's UART
        /// reassembler more time and cuts silent chunk drops / device resends.
        #[arg(long, default_value_t = 15)]
        chunk_delay_ms: u64,
    },
    /// Listen for ArtNet and print received ArtDmx packets (no BLE needed).
    Monitor,
    /// Run the full ArtNet→BLE bridge (the default when no command is given).
    Run,
}

/// Resolve the config path. An explicit `--config` is used as-is; otherwise
/// prefer `config.toml` next to the executable, falling back to `config.toml`
/// in the working directory (the historical default).
fn resolve_config_path(explicit: Option<&PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path.clone();
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join("config.toml");
            if beside.is_file() {
                return beside;
            }
        }
    }
    PathBuf::from("config.toml")
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config.as_ref());
    // Initialise logging from the config's [logging] section (falling back to
    // defaults if the config is missing/invalid — logging must never fail to come
    // up). The returned guards must live for the whole run to keep the
    // non-blocking file writer flushing.
    let loaded = Config::load(&config_path);
    let log_cfg = loaded.as_ref().map(|c| c.logging.clone()).unwrap_or_default();
    let _log_guards = logging::init(&log_cfg, cli.verbose);

    // Announce which config file actually won — and, critically, whether it
    // actually LOADED. The resolver prefers `config.toml` beside the executable
    // over the working directory, so a stale copy there can silently shadow the
    // intended one — and a wrong config (e.g. lights on the `advanced` profile
    // instead of `rgb`) then looks like a broken bridge (all white, colour
    // ignored) with nothing in the log to explain it. Print the absolute path so
    // it's unambiguous which file on disk was consulted. A file that exists but
    // fails to parse/validate must never be announced as "loaded".
    let shown = std::fs::canonicalize(&config_path).unwrap_or_else(|_| config_path.clone());
    match (&loaded, config_path.is_file()) {
        (Ok(_), _) => info!(config = %shown.display(), "loaded config"),
        (Err(e), true) => warn!(
            config = %shown.display(),
            error = %format!("{e:#}"),
            "config file exists but FAILED to load — commands that need it will refuse to run"
        ),
        (Err(_), false) => warn!(
            config = %config_path.display(),
            "config file not found — using built-in defaults (no lights will be driven)"
        ),
    }

    if let Err(e) = dispatch(&cli, &config_path).await {
        // Print the full error chain for debuggability.
        error!("{:#}", e);
        std::process::exit(1);
    }
}

async fn dispatch(cli: &Cli, config_path: &Path) -> Result<()> {
    // No subcommand ⇒ run the bridge (`neewer-bridge` alone just runs).
    let command = cli.command.as_ref().unwrap_or(&Command::Run);
    // scan/test/monitor/… don't require a config FILE — a missing one falls back
    // to defaults so the tools work out of the box. An existing-but-invalid file
    // is a hard error for every command (load_or_default): silently proceeding on
    // defaults would run `ota`/`test` against the wrong adapter or `monitor` on
    // the wrong port with nothing but a mislabelled log line to show for it.
    let load = || {
        Config::load_or_default(config_path)
            .with_context(|| format!("config {} exists but is invalid", config_path.display()))
    };
    match command {
        Command::Adapters => commands::adapters().await,
        Command::Scan { seconds, all, json } => {
            let cfg = load()?;
            commands::scan(&cfg, *seconds, *all, *json).await
        }
        Command::Add { mac, driver, profile, universe, name, blink, address, cct_min, cct_max } => {
            let cfg = load()?;
            match mac {
                Some(mac) => {
                    let universe = universe.context("--universe is required with --mac")?;
                    let address = address.context("--address is required with --mac")?;
                    commands::add_noninteractive(
                        config_path, &cfg.ble.adapter, mac, driver.as_deref(), profile.as_deref(),
                        universe, address, name.as_deref(), *cct_min, *cct_max, *blink,
                    )
                    .await
                }
                None => commands::add(config_path, &cfg.ble.adapter).await,
            }
        }
        Command::Models => commands::models(),
        Command::Lights => {
            let cfg = Config::load(config_path)
                .with_context(|| format!("loading config {} (required for `lights`)", config_path.display()))?;
            commands::lights(&cfg)
        }
        Command::Inspect { mac, seconds } => {
            let cfg = load()?;
            commands::inspect(&cfg.ble.adapter, mac, *seconds).await
        }
        Command::ArtnetSend { target, port, universe, address, channels, hz, seconds } => {
            commands::artnet_send(target, *port, *universe, *address, channels, *hz, *seconds).await
        }
        Command::Test { mac, driver, seconds, colors, modes, pixel, set, status } => {
            let cfg = load()?;
            commands::test(&cfg.ble.adapter, mac, driver, *seconds, *colors, *modes, *pixel, set.as_deref(), *status).await
        }
        Command::Ota { mac, file, version, name, check, confirm, settle_secs, seconds, chunk_delay_ms } => {
            let cfg = load()?;
            // Version: explicit flag wins; else derive from the filename's
            // V<maj>.<min>.<patch> marker. Refusing to guess beats stamping a
            // wrong version into the 0x96 header.
            let ver = match version {
                Some(s) => commands::parse_version_triplet(s)?,
                None => {
                    let fname = file.file_name().and_then(|s| s.to_str()).unwrap_or_default();
                    commands::version_from_filename(fname).with_context(|| {
                        format!(
                            "could not derive a firmware version from {fname:?} — pass \
                             --version MAJOR.MINOR.PATCH (it should match the .bin)"
                        )
                    })?
                }
            };
            // Header name is cosmetic (the device ignores it); default to the
            // filename stem rather than assuming any particular model.
            let header_name = match name {
                Some(n) => n.clone(),
                None => file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("firmware")
                    .to_string(),
            };
            commands::ota(
                &cfg.ble.adapter, mac, file, ver, &header_name, *confirm, *check, *settle_secs,
                *seconds, *chunk_delay_ms,
            )
            .await
        }
        Command::Monitor => {
            let cfg = load()?;
            commands::monitor(&cfg.artnet.bind_ip, cfg.artnet.port).await
        }
        // `run` requires a valid config (it defines the light bindings).
        Command::Run => {
            let cfg = Config::load(config_path)
                .with_context(|| format!("loading config {} (required for `run`)", config_path.display()))?;
            bridge::run(cfg).await
        }
    }
}
