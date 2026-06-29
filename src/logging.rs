//! Tracing/log setup. Verbosity is driven by `-v` flags (repeatable) or the
//! `RUST_LOG` env var (which, if set, wins — handy for targeted debugging like
//! `RUST_LOG=neewer_bridge=trace,btleplug=debug`).

use tracing_subscriber::EnvFilter;

/// Initialise the global logger.
///
/// - `0` → info (our crate + warnings elsewhere)
/// - `1` (`-v`) → debug for our crate
/// - `2+` (`-vv`) → trace for our crate + debug for btleplug (BLE wire-level)
pub fn init(verbosity: u8) {
    let default = match verbosity {
        0 => "info,neewer_bridge=info",
        1 => "info,neewer_bridge=debug",
        _ => "info,neewer_bridge=trace,btleplug=debug",
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .init();
}
