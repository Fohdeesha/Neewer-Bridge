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
//!
//! The console is coloured only when stderr is a terminal (and `NO_COLOR` is
//! unset). When stderr is the **systemd journal** — the packaged service — the
//! console lines carry no timestamp (journald stamps every line itself) and
//! start with a `<N>` syslog priority, which journald strips and records, so
//! `journalctl -u neewer-bridge -p warning` shows exactly the warnings and
//! errors.

use std::io::IsTerminal;

use tracing::{Event, Level, Subscriber};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
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
        let filter = make_filter(level, verbosity);
        // Console → stderr so machine-readable stdout (scan --json) stays clean.
        let layer = if stderr_is_journal() {
            fmt::layer()
                .with_ansi(false)
                .event_format(JournalFormat(
                    fmt::format().without_time().with_target(true).with_level(true).with_ansi(false),
                ))
                .with_writer(std::io::stderr)
                .with_filter(filter)
                .boxed()
        } else {
            fmt::layer()
                .with_timer(ChronoLocal::new(TIME_FORMAT.to_string()))
                // tracing-subscriber colours whenever NO_COLOR is unset, even
                // into a pipe or a file, so the terminal check is ours to make.
                .with_ansi(colour_console())
                .with_target(true)
                .with_level(true)
                .with_writer(std::io::stderr)
                .with_filter(filter)
                .boxed()
        };
        layers.push(layer);
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

/// Colour escapes only for a person: stderr is a terminal and `NO_COLOR` (the
/// convention tracing-subscriber also honours) is unset or empty.
fn colour_console() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// Whether stderr is connected to the systemd journal.
///
/// systemd sets `JOURNAL_STREAM=<device>:<inode>` for a service whose output
/// goes to the journal; the variable is inherited, so the documented test is to
/// compare it with what stderr actually is (a child whose output was
/// redirected must not take it on trust).
#[cfg(unix)]
fn stderr_is_journal() -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let Some(expected) = std::env::var("JOURNAL_STREAM").ok().as_deref().and_then(parse_journal_stream)
    else {
        return false;
    };
    let Ok(fd) = std::io::stderr().as_fd().try_clone_to_owned() else {
        return false;
    };
    match std::fs::File::from(fd).metadata() {
        Ok(meta) => (meta.dev(), meta.ino()) == expected,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn stderr_is_journal() -> bool {
    false
}

/// `JOURNAL_STREAM`'s value: device and inode in decimal, colon-separated.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_journal_stream(value: &str) -> Option<(u64, u64)> {
    let (dev, ino) = value.split_once(':')?;
    Some((dev.parse().ok()?, ino.parse().ok()?))
}

/// The syslog priority journald should record for a tracing level.
fn syslog_priority(level: Level) -> u8 {
    match level {
        Level::ERROR => 3,                // err
        Level::WARN => 4,                 // warning
        Level::INFO => 6,                 // info
        Level::DEBUG | Level::TRACE => 7, // debug
    }
}

/// The normal line format with the `<N>` priority prefix journald parses off
/// the front of every line a service writes (`SyslogLevelPrefix=`, on by
/// default).
struct JournalFormat<F>(F);

impl<S, N, F> FormatEvent<S, N> for JournalFormat<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let mut text = String::new();
        self.0.format_event(ctx, Writer::new(&mut text), event)?;
        writer.write_str(&with_priority(syslog_priority(*event.metadata().level()), &text))
    }
}

/// Prefix EVERY line of a formatted event with its priority. journald makes
/// each line its own entry, so a multi-line message — a TOML parse error
/// carries the offending line and a caret under it — would otherwise have only
/// its first line at the real priority and the rest at the default (info),
/// and `journalctl -p err` would show the error without what it points at.
fn with_priority(priority: u8, text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 4);
    for line in text.lines() {
        out.push_str(&format!("<{priority}>{line}\n"));
    }
    out
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

    // The journal format's rendering is checked where it matters — in the real
    // journal, under the packaged unit — not by capturing tracing output here,
    // which races other tests over tracing's global callsite cache (see the
    // failsafe-recovery test's history in bridge.rs). These pin the decisions.

    #[test]
    fn journal_priorities_follow_syslog() {
        assert_eq!(syslog_priority(Level::ERROR), 3);
        assert_eq!(syslog_priority(Level::WARN), 4);
        assert_eq!(syslog_priority(Level::INFO), 6);
        assert_eq!(syslog_priority(Level::DEBUG), 7);
        assert_eq!(syslog_priority(Level::TRACE), 7);
    }

    #[test]
    fn every_line_of_a_message_carries_its_priority() {
        assert_eq!(with_priority(6, " INFO x: one line\n"), "<6> INFO x: one line\n");
        // What a config parse error looks like: the snippet lines must stay
        // with the error, at the error's priority.
        assert_eq!(
            with_priority(3, "ERROR x: TOML parse error at line 1\n  |\n1 | bad\n  |  ^\n"),
            "<3>ERROR x: TOML parse error at line 1\n<3>  |\n<3>1 | bad\n<3>  |  ^\n"
        );
        assert_eq!(with_priority(4, ""), "");
    }

    #[test]
    fn journal_stream_is_device_colon_inode() {
        assert_eq!(parse_journal_stream("8:12345"), Some((8, 12345)));
        assert_eq!(parse_journal_stream("0:9"), Some((0, 9)));
        for bad in ["", "8", "8:", ":5", "a:5", "8:5:6", " 8:5"] {
            assert_eq!(parse_journal_stream(bad), None, "{bad:?}");
        }
    }
}
