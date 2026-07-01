//! Neewer-Bridge CLI entry point.
//!
//! Subcommands (milestone 1-2):
//! - `scan`  — discover Neewer lights, list name / MAC / RSSI.
//! - `test`  — connect to one light by MAC and prove the BLE path (blink + CCT).
//! - `run`   — run the bridge (not yet implemented; later milestones).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{ArgAction, Parser, Subcommand};
use tracing::error;

use neewer_bridge::config::Config;
use neewer_bridge::{bridge, commands, logging};

#[derive(Parser)]
#[command(name = "neewer-bridge", version, about = "ArtNet → Neewer Bluetooth light bridge")]
struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, global = true, default_value = "neewer-bridge.toml")]
    config: PathBuf,

    /// Increase log verbosity (-v debug, -vv trace + BLE wire logs).
    #[arg(short = 'v', long, global = true, action = ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List BLE adapters (index + name) for the `[ble] adapter` config setting.
    Adapters,
    /// Scan for Neewer lights and list name / MAC / RSSI.
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
        /// DMX profile: cct | cct_gm | hsi | full (default: from model).
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
    },
    /// Listen for ArtNet and print received ArtDmx packets (no BLE needed).
    Monitor,
    /// Run the full ArtNet→BLE bridge.
    Run,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // Initialise logging from the config's [logging] section (falling back to
    // defaults if the config is missing/invalid — logging must never fail to come
    // up). The returned guards must live for the whole run to keep the
    // non-blocking file writer flushing.
    let log_cfg = Config::load(&cli.config).map(|c| c.logging).unwrap_or_default();
    let _log_guards = logging::init(&log_cfg, cli.verbose);

    if let Err(e) = dispatch(&cli).await {
        // Print the full error chain for debuggability.
        error!("{:#}", e);
        std::process::exit(1);
    }
}

async fn dispatch(cli: &Cli) -> Result<()> {
    match &cli.command {
        // scan/test/monitor don't require a config file — fall back to defaults
        // so the tools work out of the box.
        Command::Adapters => commands::adapters().await,
        Command::Scan { seconds, all, json } => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            commands::scan(&cfg.ble.adapter, *seconds, *all, *json).await
        }
        Command::Add { mac, driver, profile, universe, name, blink, address, cct_min, cct_max } => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            match mac {
                Some(mac) => {
                    let universe = universe.context("--universe is required with --mac")?;
                    let address = address.context("--address is required with --mac")?;
                    commands::add_noninteractive(
                        &cli.config, &cfg.ble.adapter, mac, driver.as_deref(), profile.as_deref(),
                        universe, address, name.as_deref(), *cct_min, *cct_max, *blink,
                    )
                    .await
                }
                None => commands::add(&cli.config, &cfg.ble.adapter).await,
            }
        }
        Command::Models => commands::models(),
        Command::Lights => {
            let cfg = Config::load(&cli.config)
                .with_context(|| format!("loading config {} (required for `lights`)", cli.config.display()))?;
            commands::lights(&cfg)
        }
        Command::Inspect { mac, seconds } => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            commands::inspect(&cfg.ble.adapter, mac, *seconds).await
        }
        Command::ArtnetSend { target, port, universe, address, channels, hz, seconds } => {
            commands::artnet_send(target, *port, *universe, *address, channels, *hz, *seconds).await
        }
        Command::Test { mac, driver, seconds, colors, modes } => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            commands::test(&cfg.ble.adapter, mac, driver, *seconds, *colors, *modes).await
        }
        Command::Monitor => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            commands::monitor(&cfg.artnet.bind_ip, cfg.artnet.port).await
        }
        // `run` requires a valid config (it defines the light bindings).
        Command::Run => {
            let cfg = Config::load(&cli.config)
                .with_context(|| format!("loading config {} (required for `run`)", cli.config.display()))?;
            bridge::run(cfg).await
        }
    }
}
