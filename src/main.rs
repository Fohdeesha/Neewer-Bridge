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
    /// blink-to-identify + prompts); with `--mac`, runs non-interactively.
    Add {
        /// Light MAC. If given, runs non-interactively.
        #[arg(long)]
        mac: Option<String>,
        /// Protocol driver: auto | classic | infinity | home.
        #[arg(long, default_value = "auto")]
        driver: String,
        /// DMX profile (required with --mac): cct | cct_gm | hsi | full.
        #[arg(long)]
        profile: Option<String>,
        /// ArtNet universe / Port-Address (required with --mac).
        #[arg(long)]
        universe: Option<u16>,
        /// DMX start address (required with --mac).
        #[arg(long)]
        address: Option<u16>,
        /// Optional light name.
        #[arg(long)]
        name: Option<String>,
        /// Blink the light to identify it (non-interactive mode).
        #[arg(long)]
        blink: bool,
    },
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
    },
    /// Listen for ArtNet and print received ArtDmx packets (no BLE needed).
    Monitor,
    /// Run the full ArtNet→BLE bridge.
    Run,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    logging::init(cli.verbose);

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
        Command::Scan { seconds, all, json } => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            commands::scan(&cfg.ble.adapter, *seconds, *all, *json).await
        }
        Command::Add { mac, driver, profile, universe, name, blink, address } => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            match mac {
                Some(mac) => {
                    let profile = profile.as_deref().context("--profile is required with --mac")?;
                    let universe = universe.context("--universe is required with --mac")?;
                    let address = address.context("--address is required with --mac")?;
                    commands::add_noninteractive(
                        &cli.config, &cfg.ble.adapter, mac, driver, profile, universe, address,
                        name.as_deref(), *blink,
                    )
                    .await
                }
                None => commands::add(&cli.config, &cfg.ble.adapter).await,
            }
        }
        Command::ArtnetSend { target, port, universe, address, channels, hz, seconds } => {
            commands::artnet_send(target, *port, *universe, *address, channels, *hz, *seconds).await
        }
        Command::Test { mac, driver, seconds } => {
            let cfg = Config::load(&cli.config).unwrap_or_default();
            commands::test(&cfg.ble.adapter, mac, driver, *seconds).await
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
