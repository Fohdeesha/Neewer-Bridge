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

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter, Layer, Registry};

use crate::config::Logging;

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

/// Build the level filter for one sink. `RUST_LOG` overrides everything; else the
/// `-v` count raises the level above the configured `level`.
fn make_filter(level: &str, verbosity: u8) -> EnvFilter {
    if let Ok(f) = EnvFilter::try_from_default_env() {
        return f;
    }
    let directive = match verbosity {
        0 => format!("warn,neewer_bridge={}", level.to_lowercase()),
        1 => "warn,neewer_bridge=debug".to_string(),
        _ => "info,neewer_bridge=trace,btleplug=debug".to_string(),
    };
    EnvFilter::new(directive)
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
