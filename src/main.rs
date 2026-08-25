//! Neewer-Bridge CLI entry point.
//!
//! Key subcommands:
//! - `scan`  — discover NEW Neewer lights (not in config), list name / MAC / RSSI.
//! - `test`  — connect to one light by MAC and prove the BLE path (blink + CCT).
//! - `run`   — run the full bridge. Also the DEFAULT: no subcommand ⇒ `run`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{ArgAction, Parser, Subcommand};
use tracing::{debug, error, info, warn};

use neewer_bridge::config::{self, Config};
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
        ///
        /// `requires`: --mac is the switch into non-interactive mode, and that
        /// mode cannot proceed without a universe + address. Enforced by clap so
        /// the command fails at argument parsing — BEFORE the first-run config
        /// seeding below, which would otherwise leave a `config.toml` behind for
        /// an invocation that added nothing (and turn the next `run`'s
        /// actionable "no config file" hint into a bare "no [[lights]]").
        #[arg(long, requires = "universe", requires = "address")]
        mac: Option<String>,
        /// Protocol driver: auto | classic | infinity | home (default: from model).
        #[arg(long)]
        driver: Option<String>,
        /// DMX profile: cct | cct_gm | hsi | rgb | rgbcw | full | advanced | pixel (default: from model).
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
        /// Stream at this rate (0.001-10000 Hz). Omit for a single packet.
        #[arg(long)]
        hz: Option<f64>,
        /// Stream duration in seconds, greater than 0 up to 86400 (with --hz).
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
        ///   raw:<hex>                     send an arbitrary frame verbatim, e.g. raw:78D00048 (protocol spelunking)
        ///   warmdim                       dim warm white (safe end state)
        /// Most specs paint a CCT-white frame first, as a known baseline: a frame the
        /// fixture ignores then leaves it white instead of holding its previous look.
        ///
        /// `conflicts_with_all`: --set short-circuits before the blink/CCT
        /// sequence and the visual probes, so combining it with one of those
        /// silently ran only the --set. Rejecting the combination is the same
        /// rule --driver already follows: fail on a request we cannot honour
        /// rather than quietly doing something else.
        #[arg(long, conflicts_with_all = ["colors", "modes", "pixel"])]
        set: Option<String>,
        /// Read device status (firmware version, battery, temperature, power/mode) and
        /// print the decoded replies. Non-mutating — no blink, no colour change.
        ///
        /// `conflicts_with_all`: --status short-circuits before everything else,
        /// so it silently discarded a --set (having already validated and
        /// rejected a bad one it would never send) and every probe flag.
        #[arg(long, conflicts_with_all = ["set", "colors", "modes", "pixel"])]
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

impl Command {
    /// Whether this command reads the config file at all.
    ///
    /// The three that don't are exactly the ones a first-run user reaches for
    /// *before* they have a config, so a "config file not found" warning there
    /// is pure noise. Anything that touches BLE needs `[ble] adapter`, and
    /// anything that listens needs `[artnet]`, so the default is `true` — a new
    /// subcommand only opts out deliberately.
    fn needs_config(&self) -> bool {
        !matches!(self, Command::Adapters | Command::Models | Command::ArtnetSend { .. })
    }
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

/// Do two file paths live in the same directory? Canonicalised, so a relative
/// `config.toml` and an absolute example path in the working directory compare
/// equal. False when either side can't be resolved — the caller then falls back
/// to printing the full path, which is never wrong, just longer.
fn same_dir(a: &Path, b: &Path) -> bool {
    let dir = |p: &Path| {
        let parent = p
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::canonicalize(parent).ok()
    };
    match (dir(a), dir(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Absolute path for display in the startup log.
///
/// `canonicalize` is what makes the "which config actually won?" line
/// unambiguous — the resolver prefers a `config.toml` beside the executable over
/// the working directory, and that line is the README's first troubleshooting
/// step. But on Windows canonicalize returns an *extended-length* path
/// (`\\?\C:\…`), a spelling nothing else prints — including the "exists but
/// FAILED to load" warning about the very same file, and every error the
/// commands raise. One run rendering one path two different ways is exactly the
/// confusion this line exists to remove, so the prefix comes back off.
fn display_path(path: &Path) -> PathBuf {
    strip_verbatim(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

/// Undo Windows' `\\?\` extended-length prefix, where a plain equivalent exists.
/// A no-op on other platforms and on any path without one.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let Some(s) = path.to_str() else { return path };
    // \\?\UNC\server\share\x → \\server\share\x  (dropping the whole
    // prefix here would leave `server\share\x`, a different path entirely).
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        // Only a drive path has a plain form. The same prefix also fronts device
        // namespaces (`\\?\Volume{…}`) that are NOT valid without it — those must
        // be printed exactly as they are.
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return PathBuf::from(rest);
        }
    }
    path
}

/// One-line "here's how to get a config" hint for the not-found paths. Names
/// the shipped example when one is actually on disk (the release zips put it
/// next to the binary) so the instruction can be copy-pasted rather than
/// guessed at.
fn config_hint(config_path: &Path) -> String {
    match config::find_example(config_path) {
        // Name the example by filename alone when it sits in the directory the
        // config belongs in (the usual case — both come out of the release
        // zip), so the instruction is short enough to type. Spell out the full
        // path only when it really is somewhere else.
        Some(example) => {
            let shown = if same_dir(&example, config_path) {
                Path::new(config::EXAMPLE_FILE).to_path_buf()
            } else {
                example
            };
            format!(
                "copy {} to {} and edit it, or run `neewer-bridge add` to create one",
                shown.display(),
                config_path.display()
            )
        }
        None => format!(
            "create {} (see {} in the release zip), or run `neewer-bridge add` to create one",
            config_path.display(),
            config::EXAMPLE_FILE
        ),
    }
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

    // Version banner — first line of every invocation, so any log capture
    // (screen buffer, rotated file, bug report) records which build produced it.
    // The same version string backs `--version` (clap reads it from Cargo.toml).
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "neewer-bridge v{} starting",
        env!("CARGO_PKG_VERSION")
    );

    // Announce which config file actually won — and, critically, whether it
    // actually LOADED. The resolver prefers `config.toml` beside the executable
    // over the working directory, so a stale copy there can silently shadow the
    // intended one — and a wrong config (e.g. lights on the `advanced` profile
    // instead of `rgb`) then looks like a broken bridge (all white, colour
    // ignored) with nothing in the log to explain it. Print the absolute path so
    // it's unambiguous which file on disk was consulted. A file that exists but
    // fails to parse/validate must never be announced as "loaded".
    //
    // Stat the file ONCE here and hand the answer to `dispatch`, so the state
    // announced below and the state the command acts on cannot disagree.
    let exists = config_path.is_file();
    let needs_config = cli.command.as_ref().is_none_or(Command::needs_config);
    let shown = display_path(&config_path);
    match (&loaded, exists) {
        (Ok(_), _) => info!(config = %shown.display(), "loaded config"),
        // Always surfaced, even for commands that don't read the config: a
        // broken file also means `[logging]` silently fell back to defaults.
        (Err(e), true) => warn!(
            config = %shown.display(),
            error = %format!("{e:#}"),
            "config file exists but FAILED to load — commands that need it will refuse to run"
        ),
        (Err(_), false) if needs_config => warn!(
            config = %config_path.display(),
            "config file not found — using built-in defaults (no lights will be driven); {}",
            config_hint(&config_path)
        ),
        // `adapters`/`models`/`artnet-send` never read it, so having no config
        // is simply normal for them — don't tell a new user off for it.
        (Err(_), false) => debug!(
            config = %config_path.display(),
            "no config file (this command doesn't need one)"
        ),
    }

    if let Err(e) = dispatch(&cli, &config_path, exists, loaded).await {
        // Print the full error chain for debuggability.
        error!("{:#}", e);
        // Drop the logging guards explicitly: `process::exit` runs no
        // destructors, so without this the non-blocking file writer could be
        // torn down with this very error still queued — losing the one line
        // that explains the exit.
        drop(_log_guards);
        std::process::exit(1);
    }
}

/// Run the selected command. `loaded` is the single config parse `main` already
/// performed (logging needs the config before anything else runs) and `exists`
/// the single stat that went with it — reusing both keeps one file read per
/// invocation instead of two or three, and keeps `main`'s announcement and the
/// command's view of the config in lockstep.
async fn dispatch(
    cli: &Cli,
    config_path: &Path,
    exists: bool,
    loaded: Result<Config>,
) -> Result<()> {
    // No subcommand ⇒ run the bridge (`neewer-bridge` alone just runs).
    let command = cli.command.as_ref().unwrap_or(&Command::Run);
    // The missing-vs-broken rule lives in `Config::for_command`; this just
    // serves it from the parse `main` already did.
    //
    // `FnOnce`: it consumes `loaded`. Only one match arm ever runs, so calling
    // it from several of them is fine.
    let load = move || Config::for_command(config_path, exists, loaded);
    match command {
        Command::Adapters => commands::adapters().await,
        Command::Scan { seconds, all, json } => {
            let cfg = load()?;
            commands::scan(&cfg, *seconds, *all, *json).await
        }
        Command::Add { mac, driver, profile, universe, name, blink, address, cct_min, cct_max } => {
            // First run: start the live config off as a copy of the shipped
            // example, so it keeps every documented default and comment instead
            // of being a bare file holding only the light just added. Purely a
            // convenience — if no example is on disk, `append_light` creates the
            // file from scratch as before.
            if !exists {
                match config::seed_from_example(config_path) {
                    Ok(Some(example)) => info!(
                        config = %config_path.display(),
                        from = %example.display(),
                        "created config from the shipped example"
                    ),
                    Ok(None) => {}
                    Err(e) => warn!(
                        error = %format!("{e:#}"),
                        "could not copy the example config; writing a fresh one instead"
                    ),
                }
            }
            // A config we just seeded postdates `main`'s parse, so read it back
            // rather than serving the "no config" defaults for it.
            let cfg = if !exists && config_path.is_file() {
                Config::load(config_path).with_context(|| {
                    format!("loading the config just created at {}", config_path.display())
                })?
            } else {
                load()?
            };
            match mac {
                Some(mac) => {
                    // clap's `requires` on --mac already rejected a missing
                    // flag before we got here (and before the seeding above);
                    // these keep the unwrap honest if that ever changes.
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
        // `lights` reports the configured channel map, so — like `run` — a
        // missing config is an error, not an empty table.
        Command::Lights => {
            if !exists {
                anyhow::bail!(
                    "no config file at {} — {}",
                    config_path.display(),
                    config_hint(config_path)
                );
            }
            let cfg = load()?;
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
            let probes = commands::TestProbes {
                colors: *colors,
                modes: *modes,
                pixel: *pixel,
                status: *status,
            };
            commands::test(&cfg.ble.adapter, mac, driver, *seconds, probes, set.as_deref()).await
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
            commands::monitor(&cfg.artnet).await
        }
        // `run` requires a valid config (it defines the light bindings). A
        // missing one is the first-run case, so say how to make one rather than
        // surfacing a bare "No such file or directory".
        Command::Run => {
            if !exists {
                anyhow::bail!(
                    "no config file at {} — {}",
                    config_path.display(),
                    config_hint(config_path)
                );
            }
            let cfg = load()?;
            bridge::run(cfg).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefix_is_stripped_for_display() {
        let s = |p: &str| strip_verbatim(PathBuf::from(p)).to_string_lossy().into_owned();
        // What `canonicalize` actually hands back on Windows, and what used to
        // reach the `loaded config` line while every other mention of the same
        // file printed the plain form.
        assert_eq!(s(r"\\?\C:\Users\me\config.toml"), r"C:\Users\me\config.toml");
        assert_eq!(s(r"\\?\D:\"), r"D:\");
        // A UNC path keeps its `\\server\share` form — stripping the whole
        // prefix would silently name a different path.
        assert_eq!(s(r"\\?\UNC\server\share\config.toml"), r"\\server\share\config.toml");
        // Device namespaces are not valid without the prefix; leave them alone.
        assert_eq!(s(r"\\?\Volume{9f8a}\config.toml"), r"\\?\Volume{9f8a}\config.toml");
        // Everything already plain is returned untouched, on any platform.
        assert_eq!(s("/root/neewer-bridge/config.toml"), "/root/neewer-bridge/config.toml");
        assert_eq!(s(r"C:\already\plain.toml"), r"C:\already\plain.toml");
    }

    #[test]
    fn display_path_is_absolute_and_carries_no_verbatim_prefix() {
        // The table above compares hand-written literals against hand-written
        // literals, so a typo in BOTH cancels out — which is exactly what
        // happened once (every `\\?\` was written with one backslash, the test
        // agreed with itself, and the running binary still logged the prefix).
        // This checks the real thing instead: canonicalize a file that actually
        // exists and assert the displayed form still names it.
        let path = std::env::temp_dir().join("neewer-bridge-display-path.probe");
        std::fs::write(&path, b"x").expect("temp file");
        let shown = display_path(&path);
        let text = shown.to_string_lossy().into_owned();
        assert!(
            !text.starts_with(r"\\?\"),
            "display path still carries the verbatim prefix: {text}"
        );
        // …and it is still the same file, not a mangled one.
        assert_eq!(
            std::fs::canonicalize(&shown).expect("displayed path must still resolve"),
            std::fs::canonicalize(&path).unwrap()
        );
        let _ = std::fs::remove_file(&path);
    }
}
