//! Tracing/log setup.
//!
//! Logging is driven by the `[logging]` config section: a global `level`, an
//! optional per-destination override, and two independently-toggleable sinks —
//! the **console** (stderr) and a **rotating log file**. The file uses a
//! non-blocking writer so a slow disk can never stall the BLE actors, and is
//! rotated by size (`max_size_mb` × `max_files`).
//!
//! Precedence (highest first):
//!   1. `RUST_LOG` env var — if set, it wins for every sink (targeted debugging,
//!      e.g. `RUST_LOG=neewer_bridge=trace,btleplug=debug`).
//!   2. `-v` / `-vv` CLI flags — raise verbosity (`-v` debug, `-vv` trace + BLE
//!      wire logs) above whatever the config asks for.
//!   3. The config `[logging]` levels.
//!
//! At `info` (the default) the console stays clean; every BLE command sent to a
//! light is logged at `debug`, so a `debug` file level keeps a full record
//! without cluttering stdout.
//!
//! Both sinks stamp lines with the **local** time as `MM-DD HH:MM:SS`
//! (`TIME_FORMAT`) rather than the default RFC3339-in-UTC.

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter, Layer, Registry};

use crate::config::Logging;

/// Log timestamp format: local time, `MM-DD HH:MM:SS` (chrono strftime). Short and
/// human-readable for a long-lived daemon — the default RFC3339-UTC stamp is noise
/// when you're correlating a log line with what the lights just did.
const TIME_FORMAT: &str = "%m-%d %H:%M:%S";

/// Keep-alive guards for the non-blocking file writer(s). Dropping these flushes
/// and stops the background writer thread, so `main` must hold them until exit.
#[must_use = "dropping the guards stops file logging (flushes pending lines)"]
pub struct LogGuards(#[allow(dead_code)] Vec<WorkerGuard>);

/// Initialise the global logger from config + CLI verbosity. Never fails: if the
/// log file can't be opened it logs a warning to the console and carries on.
pub fn init(cfg: &Logging, verbosity: u8) -> LogGuards {
    let mut guards = Vec::new();
    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();

    if cfg.console {
        let level = cfg.console_level.as_deref().unwrap_or(&cfg.level);
        layers.push(
            fmt::layer()
                .with_timer(ChronoLocal::new(TIME_FORMAT.to_string()))
                .with_target(true)
                .with_level(true)
                // Console → stderr so machine-readable stdout (scan --json) stays clean.
                .with_writer(std::io::stderr)
                .with_filter(make_filter(level, verbosity))
                .boxed(),
        );
    }

    if let Some(path) = cfg.file.as_deref().filter(|p| !p.is_empty()) {
        match build_file_writer(path, cfg.max_size_mb, cfg.max_files) {
            Ok((writer, guard)) => {
                guards.push(guard);
                let level = cfg.file_level.as_deref().unwrap_or(&cfg.level);
                layers.push(
                    fmt::layer()
                        .with_timer(ChronoLocal::new(TIME_FORMAT.to_string()))
                        .with_ansi(false) // no colour escapes in the file
                        .with_target(true)
                        .with_level(true)
                        .with_writer(writer)
                        .with_filter(make_filter(level, verbosity))
                        .boxed(),
                );
            }
            // Can't use tracing yet (not initialised) — go straight to stderr.
            Err(e) => eprintln!("warning: file logging disabled — could not open {path}: {e:#}"),
        }
    }

    tracing_subscriber::registry().with(layers).init();
    LogGuards(guards)
}

/// Level names ordered low → high verbosity; the index is the rank used to
/// compare a configured level against the floor `-v` imposes.
const LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];
const RANK_DEBUG: usize = 3;
const RANK_TRACE: usize = 4;

/// Rank a level name. Unrecognised names rank lowest so they can never hold a
/// sink *above* what `-v` asked for (config validation rejects them anyway —
/// see `KNOWN_LOG_LEVELS`).
fn level_rank(level: &str) -> usize {
    let lower = level.to_ascii_lowercase();
    LEVELS.iter().position(|&l| l == lower).unwrap_or(0)
}

/// Build the level filter for one sink. `RUST_LOG` overrides everything; else the
/// `-v` count raises the level above the configured `level`.
fn make_filter(level: &str, verbosity: u8) -> EnvFilter {
    if let Ok(f) = EnvFilter::try_from_default_env() {
        return f;
    }
    EnvFilter::new(filter_directive(level, verbosity))
}

/// The filter directive for one sink — pure, so the `-v` interaction is testable
/// without building a subscriber.
///
/// `-v` **raises** verbosity and must never lower a sink below what the config
/// asked for. It used to replace the level outright, so a sink configured
/// `file_level = "trace"` was silently demoted to `debug` the moment anyone
/// passed `-v` — the flag documented as "increase verbosity" *removed* records
/// from the file, and `-v` on a `trace` console lost every trace line (the
/// dropped-non-ArtDmx-datagram trace, for one). Now the floor `-v` imposes is
/// combined with the configured level by taking the more verbose of the two.
fn filter_directive(level: &str, verbosity: u8) -> String {
    // No `-v`: the config alone decides (unchanged — including handing an
    // unrecognised name straight to EnvFilter, which ignores bad directives).
    if verbosity == 0 {
        return format!("warn,neewer_bridge={}", level.to_lowercase());
    }
    let (floor, external, extra) = if verbosity == 1 {
        (RANK_DEBUG, "warn", "")
    } else {
        (RANK_TRACE, "info", ",btleplug=debug")
    };
    let crate_level = LEVELS[level_rank(level).max(floor)];
    format!("{external},neewer_bridge={crate_level}{extra}")
}

/// A size-rotating, non-blocking file writer. `max_size_mb`/`max_files` cap total
/// on-disk size at roughly `max_size_mb × (max_files + 1)`.
fn build_file_writer(
    path: &str,
    max_size_mb: u64,
    max_files: usize,
) -> anyhow::Result<(NonBlocking, WorkerGuard)> {
    use file_rotate::{compression::Compression, suffix::AppendCount, ContentLimit, FileRotate};

    // Create the parent directory if the path points into one.
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let bytes = (max_size_mb.max(1) * 1024 * 1024) as usize;
    let files = max_files.max(1);
    // `FileRotate::new` takes a trailing Unix-only file-mode arg; gate it by OS.
    #[cfg(unix)]
    let rotate = FileRotate::new(
        path,
        AppendCount::new(files),
        ContentLimit::Bytes(bytes),
        Compression::None,
        None,
    );
    #[cfg(not(unix))]
    let rotate = FileRotate::new(
        path,
        AppendCount::new(files),
        ContentLimit::Bytes(bytes),
        Compression::None,
    );

    Ok(tracing_appender::non_blocking(rotate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_never_lowers_a_configured_level() {
        // Without -v the config alone decides (unchanged behaviour).
        assert_eq!(filter_directive("info", 0), "warn,neewer_bridge=info");
        assert_eq!(filter_directive("trace", 0), "warn,neewer_bridge=trace");
        assert_eq!(filter_directive("WARN", 0), "warn,neewer_bridge=warn");

        // -v raises a quieter sink to debug...
        assert_eq!(filter_directive("info", 1), "warn,neewer_bridge=debug");
        assert_eq!(filter_directive("error", 1), "warn,neewer_bridge=debug");
        // ...but must NOT demote one already above it. This is the bug: a
        // `file_level = "trace"` sink used to drop to debug on -v, so the flag
        // meant to add detail silently removed records.
        assert_eq!(filter_directive("trace", 1), "warn,neewer_bridge=trace");
        assert_eq!(filter_directive("debug", 1), "warn,neewer_bridge=debug");

        // -vv is trace for us plus the BLE wire logs, whatever was configured.
        assert_eq!(filter_directive("error", 2), "info,neewer_bridge=trace,btleplug=debug");
        assert_eq!(filter_directive("trace", 2), "info,neewer_bridge=trace,btleplug=debug");
        assert_eq!(filter_directive("info", 9), "info,neewer_bridge=trace,btleplug=debug");
    }

    #[test]
    fn level_rank_orders_levels_and_floors_unknown_names() {
        assert!(level_rank("error") < level_rank("warn"));
        assert!(level_rank("warn") < level_rank("info"));
        assert!(level_rank("info") < level_rank("debug"));
        assert!(level_rank("debug") < level_rank("trace"));
        assert_eq!(level_rank("TRACE"), RANK_TRACE);
        // Unknown ranks lowest, so it can never hold a sink above -v's floor.
        assert_eq!(level_rank("nonsense"), 0);
        assert_eq!(filter_directive("nonsense", 1), "warn,neewer_bridge=debug");
    }
}
