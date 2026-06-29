//! Neewer-Bridge CLI entry point.
//!
//! Subcommands (milestone 1-2):
//! - `scan`  — discover Neewer lights, list name / MAC / RSSI.
//! - `test`  — connect to one light by MAC and prove the BLE path (blink + CCT).
//! - `run`   — run the bridge (not yet implemented; later milestones).

use std::path::PathBuf;

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand};
use tracing::error;

use neewer_bridge::config::Config;
use neewer_bridge::{commands, logging};

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
    /// Run the ArtNet→BLE bridge (not yet implemented).
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
    // BLE adapter selection comes from config when present, else "default".
    // scan/test don't require a config file to exist.
    let adapter_selector = Config::load(&cli.config)
        .map(|c| c.ble.adapter)
        .unwrap_or_else(|_| "default".to_string());

    match &cli.command {
        Command::Scan { seconds, all } => commands::scan(&adapter_selector, *seconds, *all).await,
        Command::Test { mac, driver, seconds } => {
            commands::test(&adapter_selector, mac, driver, *seconds).await
        }
        Command::Run => {
            anyhow::bail!("`run` is not implemented yet (ArtNet listener + mapper come next)")
        }
    }
}
