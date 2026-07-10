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
    let log_cfg = Config::load(&config_path).map(|c| c.logging).unwrap_or_default();
    let _log_guards = logging::init(&log_cfg, cli.verbose);

    // Announce which config file actually won. The resolver prefers `config.toml`
    // beside the executable over the working directory, so a stale copy there can
    // silently shadow the intended one — and a wrong config (e.g. lights on the
    // `advanced` profile instead of `rgb`) then looks like a broken bridge (all
    // white, colour ignored) with nothing in the log to explain it. Print the
    // absolute path so it's unambiguous which file on disk was loaded.
    let shown = std::fs::canonicalize(&config_path).unwrap_or_else(|_| config_path.clone());
    if config_path.is_file() {
        info!(config = %shown.display(), "loaded config");
    } else {
        warn!(config = %config_path.display(),
              "config file not found — using built-in defaults (no lights will be driven)");
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
    match command {
        // scan/test/monitor don't require a config file — fall back to defaults
        // so the tools work out of the box.
        Command::Adapters => commands::adapters().await,
        Command::Scan { seconds, all, json } => {
            let cfg = Config::load(config_path).unwrap_or_default();
            commands::scan(&cfg, *seconds, *all, *json).await
        }
        Command::Add { mac, driver, profile, universe, name, blink, address, cct_min, cct_max } => {
            let cfg = Config::load(config_path).unwrap_or_default();
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
            let cfg = Config::load(config_path).unwrap_or_default();
            commands::inspect(&cfg.ble.adapter, mac, *seconds).await
        }
        Command::ArtnetSend { target, port, universe, address, channels, hz, seconds } => {
            commands::artnet_send(target, *port, *universe, *address, channels, *hz, *seconds).await
        }
        Command::Test { mac, driver, seconds, colors, modes, pixel, set, status } => {
            let cfg = Config::load(config_path).unwrap_or_default();
            commands::test(&cfg.ble.adapter, mac, driver, *seconds, *colors, *modes, *pixel, set.as_deref(), *status).await
        }
        Command::Monitor => {
            let cfg = Config::load(config_path).unwrap_or_default();
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
