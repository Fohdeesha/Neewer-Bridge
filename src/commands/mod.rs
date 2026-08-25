//! CLI command implementations (orchestration). Thin glue over `ble` + `protocol`.
//!
//! The two commands with real machinery behind them live in their own modules:
//! [`probe`] (`test`, its probes and the `--set` spec language) and [`ota`] (the
//! firmware flasher). What stays here reads or writes config, or shovels ArtNet.

use anyhow::{bail, Context, Result};
use btleplug::platform::Peripheral;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader, Stdin};
use tokio::net::UdpSocket;
use tracing::{info, warn};

use crate::artnet::{self, DMX_UNIVERSE_SIZE};
use crate::ble;
use crate::config::{self, parse_mac, LightCfg, KNOWN_DRIVERS, KNOWN_PROFILES};
use crate::driver::Driver;
use crate::models::Catalog;
use crate::profile::Profile;

mod ota;
mod probe;

// The `test` command and its probe machinery live in `probe`; re-exported so
// the CLI keeps calling `commands::test(..)`.
pub use probe::{test, TestProbes};
// `ota` is a module here and a function in the re-export; they occupy different
// namespaces, so `commands::ota(..)` keeps meaning the command it always did.
pub use ota::{ota, parse_version_triplet, version_from_filename};

/// Minimal JSON string escaping (quotes + backslashes) for `scan --json`.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Print a prompt and read one trimmed line from stdin.
async fn prompt(reader: &mut BufReader<Stdin>, msg: &str) -> Result<String> {
    use std::io::Write;
    print!("{msg}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    reader.read_line(&mut line).await.context("reading stdin")?;
    Ok(line.trim().to_string())
}

/// Like `prompt` but returns `default` if the user just presses Enter.
async fn prompt_default(reader: &mut BufReader<Stdin>, msg: &str, default: &str) -> Result<String> {
    let s = prompt(reader, msg).await?;
    Ok(if s.is_empty() { default.to_string() } else { s })
}

/// `adapters` — list the BLE adapters the OS exposes, with the index/name to put
/// in `[ble] adapter`. Helps pick a specific dongle when more than one is present.
pub async fn adapters() -> Result<()> {
    let list = ble::list_adapters().await?;
    if list.is_empty() {
        bail!("no Bluetooth adapter found — is Bluetooth enabled?");
    }
    println!("Available BLE adapters ({}):", list.len());
    for (i, info) in &list {
        println!("  [{i}] {info}");
    }
    println!(
        "\nSet `[ble] adapter` in your config to the index (e.g. \"{}\") or a\n\
         substring of the name. \"default\" uses the first one.",
        list.last().map(|(i, _)| *i).unwrap_or(0)
    );
    Ok(())
}

/// `models` — list the known light-model catalog (what `add` matches against).
pub fn models() -> Result<()> {
    let cat = Catalog::builtin();
    println!("Known light models ({}):\n", cat.models.len());
    println!("  {:<14} {:<8} {:<4} {:<3} {:<11} {:<14} extra", "MODEL", "PROFILE", "RGB", "GM", "CCT(K)", "MATCH");
    for m in &cat.models {
        let mut extra = Vec::new();
        if m.supports_rgbcw { extra.push("RGBCW"); }
        if m.supports_xy { extra.push("XY"); }
        if m.supports_dmx { extra.push("DMX"); }
        if m.pixel_classify > 0 { extra.push("Pixel"); }
        let matchstr = if !m.product_ids.is_empty() {
            m.product_ids.join(",")
        } else {
            m.name_matches.first().cloned().unwrap_or_default()
        };
        println!(
            "  {:<14} {:<8} {:<4} {:<3} {:<11} {:<14} {}",
            m.name,
            m.profile().as_str(),
            if m.supports_rgb { "yes" } else { "—" },
            if m.supports_gm { "yes" } else { "—" },
            format!("{}00-{}00", m.cct_min, m.cct_max),
            matchstr,
            extra.join(","),
        );
    }
    println!("\n{} models. Edit models.toml to add/correct; `add` auto-fills these from a light's BLE name.", cat.models.len());
    Ok(())
}

/// `lights` — print every configured light and its DMX channel mapping, showing
/// the absolute ArtNet universe + DMX channel each parameter lands on. This is a
/// static view of the config bindings (the source of truth for DMX→light); live
/// connection state appears in `run`'s logs.
pub fn lights(cfg: &config::Config) -> Result<()> {
    println!(
        "ArtNet {}:{}   BLE adapter: {:?}   failsafe: {} (timeout {}s)",
        cfg.artnet.bind_ip, cfg.artnet.port, cfg.ble.adapter, cfg.failsafe.mode, cfg.failsafe.timeout_secs,
    );
    if cfg.lights.is_empty() {
        println!("\nNo lights configured. Use `neewer-bridge add` to bind one by MAC.");
        return Ok(());
    }
    println!("\nConfigured lights ({}):", cfg.lights.len());

    let mut advanced_used = false;
    for (i, l) in cfg.lights.iter().enumerate() {
        // Profile is validated at config load, so parse can't fail here.
        let profile = Profile::parse(&l.profile).unwrap_or(Profile::Cct);
        if profile == Profile::Advanced {
            advanced_used = true;
        }
        let (net, sub_net, uni) = artnet::split_port_address(l.universe);
        let last = l.address + profile.channel_count() - 1;

        let display_name = l.name.as_deref().filter(|n| !n.is_empty()).unwrap_or("(unnamed)");
        println!("\n  [{}] {}  ({})", i + 1, display_name, l.mac);
        println!(
            "      driver {} · profile {} · CCT {}00-{}00K · power-on-connect {}",
            l.driver, l.profile, l.cct_min, l.cct_max, l.power_on_connect,
        );
        println!(
            "      universe {} (Net {} / Sub-Net {} / Universe {}) · DMX channels {}-{}",
            l.universe, net, sub_net, uni, l.address, last,
        );
        for (off, role) in profile.channel_roles().iter().enumerate() {
            println!("        U{} ch{:<3} → {}", l.universe, l.address + off as u16, role);
        }
    }

    if advanced_used {
        println!(
            "\n  `advanced` Mode-select (ch1) value bands — selects how ch3+ are read:\n   \
             0-31 CCT (ch3 CCT, ch4 GM) · 32-63 HSI (ch3 Hue, ch4 Sat) ·\n   \
             64-95 FX (ch3 FX-id 1-18, ch4 Speed, ch5 CCT, ch6 Hue, ch7 Sat/GM, ch8 Extra, ch9 2nd-val) ·\n   \
             128-159 RGBCW (ch3-7 R,G,B,CW,WW; needs an RGBCW fixture) · 192-231 XY (ch3 X, ch4 Y).\n   \
             Other bands (GEL 96-127, Pixel 160-191, 232-255) → neutral white."
        );
    }
    Ok(())
}

/// `scan` — discover and list lights. Prints a human table; only NEW Neewer
/// lights (not already bound in the config) are shown so you can copy the MAC
/// straight into the config. `--all` lists everything, marking configured ones.
pub async fn scan(cfg: &config::Config, seconds: u64, all: bool, json: bool) -> Result<()> {
    let adapter = ble::acquire_adapter(&cfg.ble.adapter).await?;
    let found = ble::scan(&adapter, seconds).await?;

    let is_configured = |addr: &str| cfg.lights.iter().any(|l| config::mac_eq(&l.mac, addr));
    let mut hidden = 0usize;
    let shown: Vec<_> = found
        .iter()
        .filter(|f| {
            if all {
                return true;
            }
            if !f.is_neewer {
                return false;
            }
            if is_configured(&f.address) {
                hidden += 1;
                return false;
            }
            true
        })
        .collect();

    if json {
        // Machine-readable output (one JSON array) for automated tooling.
        let items: Vec<String> = shown
            .iter()
            .map(|f| {
                let rssi = f.rssi.map(|r| r.to_string()).unwrap_or_else(|| "null".into());
                format!(
                    "{{\"name\":\"{}\",\"mac\":\"{}\",\"rssi\":{},\"neewer\":{},\"configured\":{}}}",
                    json_escape(&f.name),
                    json_escape(&f.address),
                    rssi,
                    f.is_neewer,
                    is_configured(&f.address)
                )
            })
            .collect();
        println!("[{}]", items.join(","));
        return Ok(());
    }

    if shown.is_empty() {
        if hidden > 0 {
            println!("\nNo NEW Neewer lights found ({hidden} already in the config, hidden — `--all` shows them).");
        } else {
            println!("\nNo {}lights found. Is the light powered on and in range?", if all { "" } else { "Neewer " });
            println!("(Try `--all` to list every BLE device, or `--seconds N` for a longer scan.)");
        }
        return Ok(());
    }

    println!("\n  {:<3} {:<22} {:<18} {:>5}  TYPE", "#", "NAME", "MAC", "RSSI");
    println!("  {}", "-".repeat(62));
    for (i, f) in shown.iter().enumerate() {
        let rssi = f.rssi.map(|r| format!("{r}")).unwrap_or_else(|| "  ?".into());
        let kind = match (f.is_neewer, is_configured(&f.address)) {
            (true, true) => "Neewer (configured)",
            (true, false) => "Neewer",
            _ => "other",
        };
        let name = if f.name.is_empty() { "(no name)" } else { &f.name };
        println!("  {:<3} {:<22} {:<18} {:>4}  {}", i + 1, name, f.address, rssi, kind);
    }
    if hidden > 0 {
        println!("\n  ({hidden} already-configured light(s) hidden — `--all` shows them.)");
    }
    println!(
        "\nTo control one: `neewer-bridge test <MAC> --driver <classic|infinity|home>`"
    );
    println!("To bind it for the bridge, add a [[lights]] entry with its MAC to your config.");
    Ok(())
}

/// `add` — interactive pairing: scan, pick a light, blink it to identify, then
/// append a ready-to-edit `[[lights]]` entry to the config file.
pub async fn add(config_path: &Path, adapter_selector: &str) -> Result<()> {
    let adapter = ble::acquire_adapter(adapter_selector).await?;
    let found = ble::scan(&adapter, 6).await?;
    let neewer: Vec<_> = found.into_iter().filter(|f| f.is_neewer).collect();
    if neewer.is_empty() {
        println!("\nNo Neewer lights found. Power one on, bring it close, and retry.");
        return Ok(());
    }

    println!();
    for (i, f) in neewer.iter().enumerate() {
        let rssi = f.rssi.map(|r| r.to_string()).unwrap_or_else(|| "?".into());
        let name = if f.name.is_empty() { "(no name)" } else { &f.name };
        println!("  [{}] {:<22} {:<18} {}dBm", i + 1, name, f.address, rssi);
    }

    let mut reader = BufReader::new(tokio::io::stdin());
    let sel = prompt(&mut reader, "\nSelect a light to add (number, or q to cancel): ").await?;
    if sel.is_empty() || sel.eq_ignore_ascii_case("q") {
        println!("Cancelled.");
        return Ok(());
    }
    let idx = sel
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1 && *n <= neewer.len())
        .ok_or_else(|| anyhow::anyhow!("invalid selection {sel:?}"))?
        - 1;
    let light = &neewer[idx];

    // Identify the model from its advertised BLE name against the catalog, and
    // derive sensible defaults so the user doesn't hand-specify capabilities.
    let model = Catalog::builtin().identify(&light.name);
    let (def_driver, def_profile, def_cct_min, def_cct_max, def_name) = match model {
        Some(m) => {
            println!(
                "\nIdentified model: {} — {}, CCT {}00-{}00K, driver {}",
                m.name,
                if m.supports_rgb { "RGB" } else { "bi-colour" },
                m.cct_min, m.cct_max, m.driver,
            );
            (m.driver.clone(), m.profile().as_str().to_string(), m.cct_min, m.cct_max, m.name.clone())
        }
        None => {
            println!(
                "\nUnknown model (BLE name {:?}) — using safe defaults; verify the\n\
                 driver/profile/CCT range afterwards (add it to models.toml once known).",
                light.name
            );
            let dn = if light.name.is_empty() { light.address.clone() } else { light.name.clone() };
            ("auto".to_string(), "full".to_string(), config::DEFAULT_CCT_MIN, config::DEFAULT_CCT_MAX, dn)
        }
    };

    let driver = prompt_default(
        &mut reader,
        &format!("Driver [auto/classic/infinity/home] ({def_driver}): "),
        &def_driver,
    )
    .await?;
    if !KNOWN_DRIVERS.contains(&driver.as_str()) {
        bail!("unknown driver {driver:?}");
    }

    // Blink to identify which physical fixture this is. (cmd_type only affects
    // the advanced-mode frames, not the power frames a blink uses.)
    println!("Connecting to blink the light so you can identify it…");
    let mac_bytes = parse_mac(&light.address)?;
    let drv = Driver::resolve(&driver, Profile::Full, mac_bytes, &light.name, 2);
    match ble::connect_and_verify(&light.peripheral).await {
        Ok(chars) => {
            blink_to_identify(&light.peripheral, &chars, &drv).await;
            let _ = ble::disconnect(&light.peripheral).await;
        }
        Err(e) => warn!(error = %e, "could not connect to blink; you can still add it manually"),
    }

    let confirm = prompt_default(&mut reader, "Did the right light blink? [Y/n]: ", "y").await?;
    if confirm.eq_ignore_ascii_case("n") {
        println!("Aborted. Re-run to try a different light or driver.");
        return Ok(());
    }

    let profile = prompt_default(
        &mut reader,
        &format!("Profile [{}] ({def_profile}): ", KNOWN_PROFILES.join("/")),
        &def_profile,
    )
    .await?;
    if Profile::parse(&profile).is_none() {
        bail!("unknown profile {profile:?}");
    }
    let universe: u16 = prompt_default(&mut reader, "Universe [0]: ", "0")
        .await?
        .parse()
        .context("invalid universe number")?;
    let address: u16 = prompt_default(&mut reader, "DMX start address [1]: ", "1")
        .await?
        .parse()
        .context("invalid DMX address")?;
    let name = prompt_default(&mut reader, &format!("Name [{def_name}]: "), &def_name).await?;

    let entry = LightCfg {
        mac: config::normalize_mac(&light.address),
        name: Some(name),
        driver,
        profile,
        universe,
        address,
        power_on_connect: true,
        cct_min: def_cct_min,
        cct_max: def_cct_max,
        cmd_type: model.map(|m| m.cmd_type).unwrap_or(2),
    };
    config::append_light(config_path, &entry)?;
    println!(
        "\n✓ Added {} to {} (profile {}, CCT {}00-{}00K). Saved to config.",
        entry.mac, config_path.display(), entry.profile, entry.cct_min, entry.cct_max
    );
    Ok(())
}

/// `inspect` — connect to a device and dump its full GATT (every characteristic,
/// with readable values shown as hex + ASCII). Identifies unknown lights via the
/// standard Device Information characteristics (2a29 manufacturer, 2a24 model,
/// 2a00 name) and reveals non-standard protocols.
pub async fn inspect(adapter_selector: &str, mac: &str, seconds: u64) -> Result<()> {
    let adapter = ble::acquire_adapter(adapter_selector).await?;
    let peripheral = ble::find_by_mac(&adapter, mac, Duration::from_secs(seconds)).await?;
    let infos = ble::inspect(&peripheral).await?;
    println!("\nConnected to {mac} — {} characteristics:\n", infos.len());
    for ci in &infos {
        print!("  {}  {}", ci.uuid, ci.props);
        if let Some(v) = &ci.value {
            let text: String =
                v.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
            print!("  = [{}] \"{}\"", ble::hexstr(v), text);
        }
        println!();
    }
    let _ = ble::disconnect(&peripheral).await;
    Ok(())
}

/// Non-interactive `add` (when `--mac` is given): no prompts, scriptable. Scans
/// briefly to read the light's BLE name and identify its model from the catalog,
/// so driver / profile / CCT range are filled automatically; any explicit flag
/// overrides the model. Blinks if `blink` is set.
#[allow(clippy::too_many_arguments)]
pub async fn add_noninteractive(
    config_path: &Path,
    adapter_selector: &str,
    mac: &str,
    driver: Option<&str>,
    profile: Option<&str>,
    universe: u16,
    address: u16,
    name: Option<&str>,
    cct_min: Option<u8>,
    cct_max: Option<u8>,
    blink: bool,
) -> Result<()> {
    let mac_bytes = parse_mac(mac)?;

    // Find the light (if present) to read its advertised name → identify model.
    let adapter = ble::acquire_adapter(adapter_selector).await?;
    let peripheral = ble::find_by_mac(&adapter, mac, Duration::from_secs(8)).await.ok();
    let ble_name = match &peripheral {
        Some(p) => ble::peripheral_name(p).await,
        None => {
            warn!("light not found in scan; can't auto-identify — relying on flags/defaults");
            String::new()
        }
    };
    let model = if ble_name.is_empty() { None } else { Catalog::builtin().identify(&ble_name) };
    if let Some(m) = model {
        info!(model = %m.name, rgb = m.supports_rgb, cct = format!("{}-{}", m.cct_min, m.cct_max),
              driver = %m.driver, "identified model from catalog");
    }

    // Resolve each field: explicit flag > catalog model > safe default.
    let driver = driver
        .map(str::to_string)
        .or_else(|| model.map(|m| m.driver.clone()))
        .unwrap_or_else(|| "auto".to_string());
    if !KNOWN_DRIVERS.contains(&driver.as_str()) {
        bail!("unknown driver {driver:?}");
    }
    let profile = match profile.map(str::to_string).or_else(|| model.map(|m| m.profile().as_str().to_string())) {
        Some(p) => p,
        None => bail!("could not identify the light's model (not found / unknown) — pass --profile"),
    };
    if Profile::parse(&profile).is_none() {
        bail!("unknown profile {profile:?}");
    }
    let cct_min = cct_min.or_else(|| model.map(|m| m.cct_min)).unwrap_or(config::DEFAULT_CCT_MIN);
    let cct_max = cct_max.or_else(|| model.map(|m| m.cct_max)).unwrap_or(config::DEFAULT_CCT_MAX);
    let resolved_name = name
        .map(str::to_string)
        .or_else(|| model.map(|m| m.name.clone()))
        .or_else(|| (!ble_name.is_empty()).then(|| ble_name.clone()));

    if blink {
        match &peripheral {
            Some(p) => {
                let drv = Driver::resolve(&driver, Profile::Full, mac_bytes, &ble_name, 2);
                match ble::connect_and_verify(p).await {
                    Ok(chars) => {
                        info!("blinking light to identify");
                        blink_to_identify(p, &chars, &drv).await;
                        let _ = ble::disconnect(p).await;
                    }
                    Err(e) => warn!(error = %e, "could not connect to blink; adding anyway"),
                }
            }
            None => warn!("--blink requested but light not found; adding without blinking"),
        }
    }

    let entry = LightCfg {
        mac: config::normalize_mac(mac),
        name: resolved_name,
        driver,
        profile,
        universe,
        address,
        power_on_connect: true,
        cct_min,
        cct_max,
        cmd_type: model.map(|m| m.cmd_type).unwrap_or(2),
    };
    config::append_light(config_path, &entry)?;
    println!(
        "Added {} ({}, CCT {}00-{}00K, u{} a{}) to {}",
        entry.mac, entry.profile, entry.cct_min, entry.cct_max, universe, address, config_path.display()
    );
    Ok(())
}

/// Build the DMX data slot for `artnet-send`: `channels` placed at the 1-based
/// `address`, with earlier channels zero-padded, sized to a legal ArtDmx length.
///
/// Rejects a patch that runs past the universe. `encode_artdmx` truncates to
/// [`DMX_UNIVERSE_SIZE`], so without this check an out-of-range `--address`
/// would drop the channel values on the floor and still report a successful send.
fn dmx_payload(address: u16, channels: &[u8]) -> Result<Vec<u8>> {
    if !(1..=DMX_UNIVERSE_SIZE).contains(&address) {
        bail!("--address {address} out of range (1..={DMX_UNIVERSE_SIZE})");
    }
    if channels.is_empty() {
        bail!("--channels needs at least one value");
    }
    let last = address as usize + channels.len() - 1;
    if last > DMX_UNIVERSE_SIZE as usize {
        bail!(
            "{} channel value(s) starting at address {address} run to channel {last}, past the \
             {DMX_UNIVERSE_SIZE}-channel universe — they would be silently dropped",
            channels.len()
        );
    }

    let start = (address - 1) as usize;
    let mut data = vec![0u8; start + channels.len()];
    data[start..].copy_from_slice(channels);
    // ArtDmx's length field is 2..=512 and even.
    if data.len() < 2 {
        data.resize(2, 0);
    }
    if data.len() % 2 == 1 {
        data.push(0);
    }
    Ok(data)
}

/// Bounds on `artnet-send --hz`. Deliberately generous — they exist to turn a
/// typo into an error message, not to restrict real use (ArtNet streams at
/// ~44 Hz; 10 kHz is far past anything a console does).
const MIN_SEND_HZ: f64 = 0.001; // one packet per 1000 s
const MAX_SEND_HZ: f64 = 10_000.0;
/// Bound on `artnet-send --seconds`. A test helper streaming for over a day is
/// a typo, and `Instant + Duration` panics on overflow well before that.
const MAX_SEND_SECONDS: f64 = 86_400.0;

/// Resolve `--hz`/`--seconds` into `(packet period, total duration, rate)`, or
/// `None` for the default one-shot send.
///
/// Both values feed `Duration::from_secs_f64`, which **panics** on a negative,
/// NaN, infinite, or overflowing input — so `--seconds=-5` and `--hz 1e-30` used
/// to take the whole command down with a raw panic message. Every other
/// user-supplied value in this binary is range-checked (`dmx_payload`, the
/// `--set` helpers, config validation); this closes the last hole. Pure, so the
/// panic cases are unit-tested without sending anything.
fn stream_timing(hz: Option<f64>, seconds: f64) -> Result<Option<(Duration, Duration, f64)>> {
    // No --hz is a single packet, and --seconds is ignored there (as before).
    let Some(hz) = hz else { return Ok(None) };
    if !hz.is_finite() || !(MIN_SEND_HZ..=MAX_SEND_HZ).contains(&hz) {
        bail!(
            "--hz {hz} out of range ({MIN_SEND_HZ}..={MAX_SEND_HZ}) — omit --hz \
             entirely to send a single packet"
        );
    }
    if !seconds.is_finite() || seconds <= 0.0 || seconds > MAX_SEND_SECONDS {
        bail!("--seconds {seconds} out of range (greater than 0, up to {MAX_SEND_SECONDS})");
    }
    Ok(Some((
        Duration::from_secs_f64(1.0 / hz),
        Duration::from_secs_f64(seconds),
        hz,
    )))
}

/// `artnet-send` — send ArtDmx to drive the bridge (or any node) without a
/// physical console. One-shot by default; `--hz` streams for `--seconds`.
/// Channels are placed starting at `address`; earlier channels are zero-padded.
pub async fn artnet_send(
    target: &str,
    port: u16,
    universe: u16,
    address: u16,
    channels: &[u8],
    hz: Option<f64>,
    seconds: f64,
) -> Result<()> {
    let data = dmx_payload(address, channels)?;
    // Validated before the socket is bound, so a bad flag fails immediately.
    let timing = stream_timing(hz, seconds)?;
    let sock = UdpSocket::bind("0.0.0.0:0").await.context("binding sender socket")?;
    let dest = format!("{target}:{port}");

    match timing {
        Some((period, total, rate)) => {
            let end = Instant::now() + total;
            let mut seq: u8 = 1;
            let mut count = 0u64;
            while Instant::now() < end {
                let pkt = artnet::encode_artdmx(universe, seq, &data);
                sock.send_to(&pkt, &dest).await.context("sending")?;
                seq = seq.wrapping_add(1);
                if seq == 0 {
                    seq = 1;
                }
                count += 1;
                tokio::time::sleep(period).await;
            }
            info!(target = %dest, universe, address, packets = count, "streamed ArtDmx @ {rate}Hz for {seconds}s");
        }
        None => {
            let pkt = artnet::encode_artdmx(universe, 1, &data);
            sock.send_to(&pkt, &dest).await.context("sending")?;
            info!(target = %dest, universe, address, channels = ?channels, "sent 1 ArtDmx packet");
        }
    }
    Ok(())
}

/// `monitor` — bind every configured ArtNet input and print a summary of each
/// ArtDmx packet received. Hardware-free; point a console / QLC+ at this host
/// to verify ArtNet reception, universe addressing, and channel data without
/// any lights. Runs the exact merge pipeline `run` uses (merge::bind_inputs +
/// serve_all), so with multiple inputs it also logs the merged per-universe
/// output whenever it changes — the way to observe/debug the DMX merge live.
pub async fn monitor(artnet_cfg: &config::ArtNet) -> Result<()> {
    let (bound, merger) = crate::merge::bind_inputs(artnet_cfg).await?;
    let multi = bound.len() > 1;
    if multi {
        info!(
            inputs = bound.len(),
            merge = %artnet_cfg.merge,
            merge_timeout_secs = artnet_cfg.merge_timeout_secs,
            "ArtNet monitor (merging) — press Ctrl-C to stop"
        );
    } else {
        info!("ArtNet monitor — press Ctrl-C to stop");
    }
    crate::merge::serve_all(
        bound,
        merger,
        |_idx, label, src, pkt| {
            let (net, sub_net, universe) = artnet::split_port_address(pkt.port_address);
            info!(
                input = %label,
                %src,
                port = pkt.port_address,
                net,
                sub_net,
                universe,
                seq = pkt.sequence,
                channels = pkt.data.len(),
                "ArtDmx ch1.. = {}",
                artnet::preview(&pkt.data, 12),
            );
        },
        // Merged output only when it actually changed (steady re-sends would
        // double every packet line) and only with >1 input (with one input the
        // merged view IS the packet just logged above).
        move |port_address, data, changed| {
            if multi && changed {
                info!(
                    port = port_address,
                    channels = data.len(),
                    "merged ch1.. = {}",
                    artnet::preview(data, 12),
                );
            }
        },
    )
    .await
}

/// How long each half of a blink lasts (off, then on).
const BLINK_STEP: Duration = Duration::from_millis(450);

/// Blink a light's power 3× so the operator can see which physical fixture they
/// are about to add. Best-effort by design — identification is a convenience,
/// never a reason to fail the `add` — so write errors are swallowed. Shared by
/// the interactive and `--mac` paths of `add`; `test`'s blink is deliberately
/// separate (it logs each step and treats a failed write as fatal, because
/// proving the write path IS what that command is for).
async fn blink_to_identify(p: &Peripheral, chars: &ble::NeewerChars, drv: &Driver) {
    for _ in 0..3 {
        let _ = ble::write_command(p, &chars.write, &drv.power(false)).await;
        tokio::time::sleep(BLINK_STEP).await;
        let _ = ble::write_command(p, &chars.write, &drv.power(true)).await;
        tokio::time::sleep(BLINK_STEP).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_timing_rejects_everything_that_would_panic() {
        // `Duration::from_secs_f64` panics on all of these; `--seconds=-5` and
        // `--hz 1e-30` took the whole command down with a raw panic message.
        for (hz, seconds) in [
            (Some(1.0), -5.0),           // negative duration
            (Some(1.0), f64::NAN),
            (Some(1.0), f64::INFINITY),
            (Some(1.0), 1e18),           // Instant + Duration would overflow
            (Some(1.0), 0.0),            // would send nothing at all, silently
            (Some(1e-30), 1.0),          // a 1e30-second packet period
            (Some(f64::NAN), 1.0),
            (Some(f64::INFINITY), 1.0),
            (Some(0.0), 1.0),            // "stream at 0 Hz" is a typo, not one-shot
            (Some(-1.0), 1.0),
            (Some(1e9), 1.0),            // absurd rate
        ] {
            assert!(
                stream_timing(hz, seconds).is_err(),
                "--hz {hz:?} --seconds {seconds} should be rejected"
            );
        }

        // No --hz is a single packet, and --seconds is ignored there (unchanged).
        assert!(stream_timing(None, -5.0).unwrap().is_none());
        assert!(stream_timing(None, f64::NAN).unwrap().is_none());

        // A normal streaming request resolves to the right period/duration.
        let (period, total, rate) = stream_timing(Some(40.0), 2.0).unwrap().unwrap();
        assert!((period.as_secs_f64() - 0.025).abs() < 1e-9, "got {period:?}");
        assert_eq!(total, Duration::from_secs(2));
        assert_eq!(rate, 40.0);
        // Both limits are inclusive.
        assert!(stream_timing(Some(MIN_SEND_HZ), MAX_SEND_SECONDS).unwrap().is_some());
        assert!(stream_timing(Some(MAX_SEND_HZ), 0.001).unwrap().is_some());
    }

    #[test]
    fn dmx_payload_places_channels_at_the_address() {
        // Address 1 → channels sit at the front; length padded to the 2-byte
        // minimum / even length ArtDmx requires.
        assert_eq!(dmx_payload(1, &[10, 20, 30]).unwrap(), vec![10, 20, 30, 0]);
        assert_eq!(dmx_payload(1, &[255]).unwrap(), vec![255, 0]);
        // Address 4 → three zero-padding channels first.
        assert_eq!(dmx_payload(4, &[1, 2, 3]).unwrap(), vec![0, 0, 0, 1, 2, 3]);
        // A patch ending exactly on the last channel is legal.
        assert_eq!(dmx_payload(512, &[7]).unwrap().len(), 512);
        assert_eq!(dmx_payload(510, &[1, 2, 3]).unwrap().len(), 512);
    }

    #[test]
    fn dmx_payload_rejects_patches_past_the_universe() {
        // These used to be encoded, truncated to 512 on the wire, and reported
        // as a successful send — the channel values silently went nowhere.
        assert!(dmx_payload(513, &[1]).is_err());
        assert!(dmx_payload(0, &[1]).is_err());
        assert!(dmx_payload(511, &[1, 2, 3]).is_err()); // runs to 513
        assert!(dmx_payload(1, &[]).is_err());
    }

}
