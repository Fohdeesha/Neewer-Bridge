//! CLI command implementations (orchestration). Thin glue over `ble` + `protocol`.

use anyhow::{bail, Context, Result};
use btleplug::api::Characteristic;
use btleplug::platform::Peripheral;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader, Stdin};
use tokio::net::UdpSocket;
use tracing::{info, warn};

use crate::artnet;
use crate::ble;
use crate::config::{self, parse_mac, LightCfg, KNOWN_DRIVERS};
use crate::driver::Driver;
use crate::models::Catalog;
use crate::profile::Profile;
use crate::protocol::pixel::{self, Block};
use crate::protocol::{classic, home, infinity, queries};

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

        println!("\n  [{}] {}  ({})", i + 1, l.name.as_deref().unwrap_or("(unnamed)"), l.mac);
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
            for _ in 0..3 {
                let _ = ble::write_command(&light.peripheral, &chars.write, &drv.power(false)).await;
                tokio::time::sleep(Duration::from_millis(450)).await;
                let _ = ble::write_command(&light.peripheral, &chars.write, &drv.power(true)).await;
                tokio::time::sleep(Duration::from_millis(450)).await;
            }
            let _ = ble::disconnect(&light.peripheral).await;
        }
        Err(e) => warn!(error = %e, "could not connect to blink; you can still add it manually"),
    }

    let confirm = prompt_default(&mut reader, "Did the right light blink? [Y/n]: ", "y").await?;
    if confirm.eq_ignore_ascii_case("n") {
        println!("Aborted. Re-run to try a different light or driver.");
        return Ok(());
    }

    let profile =
        prompt_default(&mut reader, &format!("Profile [cct/cct_gm/hsi/full] ({def_profile}): "), &def_profile)
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
                        for _ in 0..3 {
                            let _ = ble::write_command(p, &chars.write, &drv.power(false)).await;
                            tokio::time::sleep(Duration::from_millis(450)).await;
                            let _ = ble::write_command(p, &chars.write, &drv.power(true)).await;
                            tokio::time::sleep(Duration::from_millis(450)).await;
                        }
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
    let sock = UdpSocket::bind("0.0.0.0:0").await.context("binding sender socket")?;
    let dest = format!("{target}:{port}");

    let start = (address.max(1) - 1) as usize;
    let mut data = vec![0u8; start + channels.len()];
    data[start..].copy_from_slice(channels);
    if data.len() < 2 {
        data.resize(2, 0); // ArtNet length is 2..=512
    }
    if data.len() % 2 == 1 {
        data.push(0); // even length
    }

    match hz {
        Some(h) if h > 0.0 => {
            let period = Duration::from_secs_f64(1.0 / h);
            let end = Instant::now() + Duration::from_secs_f64(seconds);
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
            info!(target = %dest, universe, address, packets = count, "streamed ArtDmx @ {h}Hz for {seconds}s");
        }
        _ => {
            let pkt = artnet::encode_artdmx(universe, 1, &data);
            sock.send_to(&pkt, &dest).await.context("sending")?;
            info!(target = %dest, universe, address, channels = ?channels, "sent 1 ArtDmx packet");
        }
    }
    Ok(())
}

/// `monitor` — bind the ArtNet listener and print a summary of every ArtDmx
/// packet received. Hardware-free; point a console / QLC+ at this host to verify
/// ArtNet reception, universe addressing, and channel data without any lights.
pub async fn monitor(bind_ip: &str, port: u16) -> Result<()> {
    info!(bind_ip, port, "ArtNet monitor — press Ctrl-C to stop");
    artnet::listen(bind_ip, port, |src, pkt| {
        let (net, sub_net, universe) = artnet::split_port_address(pkt.port_address);
        let preview: Vec<String> = pkt.data.iter().take(12).map(|b| format!("{b:3}")).collect();
        info!(
            %src,
            port = pkt.port_address,
            net,
            sub_net,
            universe,
            seq = pkt.sequence,
            channels = pkt.data.len(),
            "ArtDmx ch1.. = [{}{}]",
            preview.join(" "),
            if pkt.data.len() > 12 { " …" } else { "" },
        );
    })
    .await
}

/// `test` — connect to one light and prove the BLE path end to end:
/// verify GATT, blink power (also serves as visual identify), then set a known
/// CCT. Uses our real protocol encoders so this validates them on hardware.
#[allow(clippy::too_many_arguments)]
pub async fn test(
    adapter_selector: &str,
    mac: &str,
    driver: &str,
    seconds: u64,
    colors: bool,
    modes: bool,
    pixel: bool,
    set: Option<&str>,
    status: bool,
) -> Result<()> {
    let mac_bytes = parse_mac(mac)?;
    let adapter = ble::acquire_adapter(adapter_selector).await?;

    let peripheral = ble::find_by_mac(&adapter, mac, Duration::from_secs(seconds)).await?;
    let chars = ble::connect_and_verify(&peripheral).await?;

    if let Some(notify) = &chars.notify {
        ble::spawn_notify_logger(&peripheral, notify).await?;
    }

    // Status read (`--status`): query firmware version / battery / temperature / state
    // and print the decoded replies. Non-mutating — no blink, no colour change — so
    // it's safe to run anytime. Short-circuits before the blink/CCT sequence.
    if status {
        return test_status(&peripheral, &chars.write, mac_bytes).await;
    }

    // Single-frame set (`--set SPEC`): send exactly one frame (or pixel palette) and
    // hold it, for guided one-at-a-time testing. The light keeps the state after
    // disconnect. Short-circuits before the blink/CCT sequence.
    if let Some(spec) = set {
        return test_set(&peripheral, &chars.write, mac_bytes, spec).await;
    }

    // Encoders per protocol family. `auto` is treated as classic for this manual
    // test (real auto-detection by model lives in the driver layer later).
    let power = |on: bool| -> Vec<u8> {
        match driver {
            "infinity" => infinity::power(mac_bytes, on),
            "home" => home::power(on),
            _ => classic::power(on),
        }
    };
    let set_cct = |brr: u8, cct: u8| -> Vec<u8> {
        match driver {
            "infinity" => infinity::cct(mac_bytes, brr, cct, 0),
            "home" => home::cct(brr as u16 * 10, cct),
            _ => classic::cct2(brr, cct),
        }
    };

    info!(driver, "blinking light to identify (3×) — watch which fixture flashes");
    for n in 1..=3 {
        info!(blink = n, "power OFF");
        ble::write_command(&peripheral, &chars.write, &power(false)).await?;
        tokio::time::sleep(Duration::from_millis(450)).await;
        info!(blink = n, "power ON");
        ble::write_command(&peripheral, &chars.write, &power(true)).await?;
        tokio::time::sleep(Duration::from_millis(450)).await;
    }

    info!("setting CCT: 5600K @ 50% brightness");
    // cct raw 56 = 5600K for most lights; 50 = 50% brightness.
    ble::write_command(&peripheral, &chars.write, &set_cct(50, 56)).await?;

    // Optional RGB capability probe: cycle saturated red→green→blue via HSI so a
    // human can SEE whether the light is RGB or bi-color. The classic HSI encoder
    // is used for all classic-family drivers; infinity uses its MAC-embedded HSI.
    if colors {
        let set_hsi = |hue: u16| -> Vec<u8> {
            match driver {
                "infinity" => infinity::hsi(mac_bytes, hue, 100, 100),
                "home" => home::hsi(1000, hue, 100),
                _ => classic::hsi(hue, 100, 100),
            }
        };
        info!("RGB capability probe — watch for colour changes (bi-color lights stay white/ignore)");
        for (hue, label) in [(0u16, "RED"), (120, "GREEN"), (240, "BLUE")] {
            info!(hue, "HSI {label} @ 100% sat / 100% brightness");
            ble::write_command(&peripheral, &chars.write, &set_hsi(hue)).await?;
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
        // Return to a comfortable dim warm white so we never leave it on a colour.
        info!("restoring dim warm white (2700K @ 12%)");
        ble::write_command(&peripheral, &chars.write, &set_cct(12, 27)).await?;
    }

    // Optional advanced-mode probe: exercise XY and a couple of FX effects (the
    // modes that work over direct BLE on Infinity fixtures). Both use MAC-addressed
    // frames — the TL120C ignores the direct 0xB9/0x88 forms. (RGBCW is NOT probed:
    // the TL120C ignores it entirely over direct BLE — see NOTES.md §3.3.) Watch
    // the light to confirm each mode engages.
    if modes {
        info!("XY probe — CIE coordinate (MAC-addressed 0xB7, as the bridge sends)");
        for (label, x, y) in [("D65 white", 3127u16, 3290u16), ("deep red", 6400, 3300), ("green", 3000, 6000)] {
            info!(x, y, "XY {label}");
            ble::write_command(&peripheral, &chars.write, &classic::xy_mac(mac_bytes, 100, x, y)).await?;
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }

        info!("FX probe — built-in effect engine (0x91, MAC-embedded)");
        for (label, bytes) in [
            ("Lightning", infinity::fx(mac_bytes, 1, 100, 56, 0, 0, 0, 5, 0, 0)),
            ("HUE-pulse (blue)", infinity::fx(mac_bytes, 9, 100, 0, 0, 240, 100, 6, 0, 0)),
            ("Cop-Car (red/blue)", infinity::fx(mac_bytes, 10, 100, 0, 0, 0, 0, 7, 2, 0)),
        ] {
            info!("FX {label}");
            ble::write_command(&peripheral, &chars.write, &bytes).await?;
            tokio::time::sleep(Duration::from_millis(2500)).await;
        }

        // FX may latch the light into effect mode; power-cycle restores direct
        // control (per protocol-analysis.md), then leave a dim warm white — never
        // leave the light strobing an effect (it holds the last command forever).
        info!("exiting FX (power-cycle) and restoring dim warm white (2700K @ 12%)");
        ble::write_command(&peripheral, &chars.write, &power(false)).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        ble::write_command(&peripheral, &chars.write, &power(true)).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        ble::write_command(&peripheral, &chars.write, &set_cct(12, 27)).await?;
    }

    // Optional per-segment PIXEL probe (0xB0, MAC-embedded). Paints the tube with
    // multi-colour palettes so distinct bands appear along its length — the "set
    // different areas to different values" capability. TL-series pixel fixtures
    // only (verified on TL120C); other lights ignore it. Each palette is sent as
    // its param sub-frame then its colour sub-frame(s), spaced ~80 ms as the app
    // does, and long palettes are chunked to ≤20-byte GATT writes by write_command.
    if pixel {
        info!("PIXEL probe — per-segment colour + effects (0xB0); watch the tube");
        // The 5 pixel effects that work over direct BLE on the TL120C. For the
        // moving/fire effects, segment 0 is the background and the rest are the
        // effect's colours. A CCT frame is sent before each to clear the previous
        // effect's latch (a running pixel effect ignores a new one otherwise).
        let demos: [(&str, u8, Vec<Block>); 4] = [
            (
                "ColorReplacement: red|green|blue|yellow bands",
                1,
                vec![
                    Block::Hsi { hue: 0, sat: 100 },
                    Block::Hsi { hue: 120, sat: 100 },
                    Block::Hsi { hue: 240, sat: 100 },
                    Block::Hsi { hue: 55, sat: 100 },
                ],
            ),
            (
                "TwoColorMoving: red+blue over dark bg",
                4,
                vec![Block::Off, Block::Hsi { hue: 0, sat: 100 }, Block::Hsi { hue: 240, sat: 100 }],
            ),
            (
                "ThreeColorMoving: red/green/blue over dark bg",
                5,
                vec![
                    Block::Off,
                    Block::Hsi { hue: 0, sat: 100 },
                    Block::Hsi { hue: 120, sat: 100 },
                    Block::Hsi { hue: 240, sat: 100 },
                ],
            ),
            ("Fire: orange flicker over dark bg", 7, vec![Block::Off, Block::Hsi { hue: 25, sat: 100 }]),
        ];
        for (label, effect, blocks) in &demos {
            // Clear the previous effect's latch first (see NOTES.md §3.3).
            ble::write_command(&peripheral, &chars.write, &set_cct(50, 56)).await?;
            tokio::time::sleep(Duration::from_millis(700)).await;
            info!("PIXEL {label}");
            for frame in pixel::paint(mac_bytes, blocks, 100, *effect, 40, 1) {
                ble::write_command_chunked(&peripheral, &chars.write, &frame).await?;
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
            tokio::time::sleep(Duration::from_millis(2800)).await;
        }

        // Best-effort: leave the light on a comfortable dim warm white. After a long
        // rapid probe the write-without-response link can jam (frames silently
        // dropped), so this may not always take — if the light is left on an effect,
        // run `neewer-bridge test <MAC> --set warmdim` (a fresh connection is
        // reliable). The production bridge (`run`) doesn't have this issue: its
        // per-light actor paces writes and reconnects on a stale link.
        info!("restoring dim warm white (2700K @ 12%); if it sticks on an effect, run `--set warmdim`");
        for _ in 0..3 {
            ble::write_command(&peripheral, &chars.write, &classic::cct2(12, 27)).await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    // Give notifications a moment to arrive, then leave cleanly.
    tokio::time::sleep(Duration::from_millis(800)).await;
    info!("test complete; disconnecting");
    if let Err(e) = ble::disconnect(&peripheral).await {
        // Non-fatal — log and move on.
        tracing::warn!(error = %e, "disconnect returned an error");
    }
    if driver == "auto" {
        tracing::warn!(
            "--driver was 'auto'; sent CLASSIC commands. If nothing happened, retry with \
             --driver infinity (newer lights) or --driver home (NH-* devices)."
        );
    }
    Ok(())
}

/// A few hand-tuned FX presets (effect id → full parameter set) so `--set fx:<id>`
/// renders a recognisable effect. Ids without a preset fall back to generic params.
/// Signature: `fx(mac, effId, brr, cct, gm, hue, sat, speed, extra, val2)`.
fn fx_preset(mac: [u8; 6], id: u8, bri: u8) -> Vec<u8> {
    match id {
        1 => infinity::fx(mac, 1, bri, 56, 0, 0, 0, 5, 0, 0), // Lightning
        9 => infinity::fx(mac, 9, bri, 0, 0, 240, 100, 6, 0, 0), // HUE-pulse (blue)
        10 => infinity::fx(mac, 10, bri, 0, 0, 0, 0, 7, 2, 0), // Cop-Car
        11 => infinity::fx(mac, 11, bri, 32, 0, 0, 0, 4, 0, 0), // Candlelight
        12 => infinity::fx(mac, 12, bri, 0, 0, 0, 100, 5, 0, 0), // HUE-loop
        _ => infinity::fx(mac, id, bri, 56, 0, 0, 100, 5, 0, 0), // generic
    }
}

/// Same presets as [`fx_preset`] but the DIRECT `0x8B` frame (no MAC wrapper) —
/// what the app's `setRGBLightValue(EFFECT_MODE_OLD, …)` path sends.
fn fx_preset_direct(id: u8, bri: u8) -> Vec<u8> {
    match id {
        1 => infinity::fx_direct(1, bri, 56, 0, 0, 0, 5, 0, 0), // Lightning
        9 => infinity::fx_direct(9, bri, 0, 0, 240, 100, 6, 0, 0), // HUE-pulse (blue)
        10 => infinity::fx_direct(10, bri, 0, 0, 0, 0, 7, 2, 0), // Cop-Car
        11 => infinity::fx_direct(11, bri, 32, 0, 0, 0, 4, 0, 0), // Candlelight
        12 => infinity::fx_direct(12, bri, 0, 0, 0, 100, 5, 0, 0), // HUE-loop
        _ => infinity::fx_direct(id, bri, 56, 0, 0, 100, 5, 0, 0), // generic
    }
}

/// Build the exact frames for one pixel effect id (1..=10, the `PixelEffectType`
/// wire type) using the app's own default parameters from `createPixelEffectData`
/// plus the Pixel-N-Model defaults (decompiled), with `running=PLAY`. This is the
/// exhaustive hardware probe for `test --set pixfx:<id>` — effects 8/9/10 remap
/// their wire id to 10/11/12 (the app does this). Returns (frames, effect name).
fn build_pixel_effect_test(mac: [u8; 6], id: u8) -> (Vec<Vec<u8>>, String) {
    let hsi = |h: u16, s: u8| Block::Hsi { hue: h, sat: s }.bytes();
    let cct = |c: u8, g: i8| Block::Cct { cct: c, gm: g }.bytes();
    let off = Block::Off.bytes();
    let run = pixel::RUN_PLAY; // 1
    // Each arm: (params effectData, palette effectData, effect name). subIndex 0 =
    // params, subIndex 1 = colours. Wire id is the first byte of each sub-frame.
    let pal = |wire: u8, colours: &[[u8; 3]]| {
        let mut c = vec![wire, 1u8];
        for b in colours {
            c.extend_from_slice(b);
        }
        c
    };
    let (params, palette, name): (Vec<u8>, Vec<u8>, &str) = match id {
        1 => (
            vec![1, 0, 50, 2, 40, 1, run], // bri,colorNum,speed,dir,running
            pal(1, &[hsi(0, 100), hsi(240, 100)]),
            "ColorReplacement",
        ),
        2 => (
            vec![2, 0, 50, 40, 1, 0, run], // bri,speed,dir,transition,running
            pal(2, &[hsi(0, 100), hsi(240, 100)]),
            "ColorAlternate",
        ),
        3 => (
            vec![3, 0, 50, 50, 0, 40, 1, 0, run], // colorBri,bgBri,way,speed,dir,movement,running
            pal(3, &[cct(55, 0), hsi(0, 100)]),    // bg + 1 moving colour
            "SingleColorMoving",
        ),
        4 => (
            vec![4, 0, 50, 50, 0, 40, 1, 0, run],
            pal(4, &[cct(55, 0), hsi(0, 100), hsi(60, 100)]), // bg + 2
            "TwoColorMoving",
        ),
        5 => (
            vec![5, 0, 50, 50, 0, 40, 1, 0, run],
            pal(5, &[cct(55, 0), hsi(0, 100), hsi(60, 100), hsi(120, 100)]), // bg + 3
            "ThreeColorMoving",
        ),
        6 => (
            vec![6, 0, 50, 40, 1, run], // bri,speed,dir,running
            pal(6, &[hsi(0, 100), hsi(240, 100)]),
            "Colorful",
        ),
        7 => (
            vec![7, 0, 50, 100, 50, 20, 0, run], // briLo,briHi,bgBri,speed,orientation,running
            pal(7, &[off, hsi(30, 100)]),         // bg + fire colour
            "Fire",
        ),
        8 => (
            vec![10, 0, 50, 2, 40, 1, 0, run], // wire 10; bri,colorNum,speed,dir,sectionType,running
            pal(10, &[hsi(0, 100), hsi(240, 100)]),
            "ColorGradient",
        ),
        9 => (
            vec![11, 0, 50, 3, 40, 1, 0, run], // wire 11; bri,colorNum,speed,dir,satType,running
            pal(11, &[hsi(0, 100), hsi(60, 100), hsi(120, 100)]),
            "Trail",
        ),
        _ => (
            vec![12, 0, 50, 3, 40, 1, run], // wire 12; bri,colorNum,speed,dir,running
            pal(12, &[hsi(0, 100), hsi(60, 100), hsi(120, 100)]),
            "ColorShift",
        ),
    };
    (
        vec![pixel::raw_frame(mac, &params), pixel::raw_frame(mac, &palette)],
        format!("effect {id} ({name})"),
    )
}

/// Read device status: send the MAC-addressed version / battery / temperature /
/// state queries and let the notify logger print the decoded replies. Non-mutating
/// (no blink, no output change), so it's safe to run against a light in use. The
/// replies arrive asynchronously on the notify characteristic — hence the settle
/// wait at the end. The TL120C firmware handles these MAC reads (the direct `0x80`/
/// `0x85` version/state queries are dropped — see NOTES.md §2.1/§3.6).
async fn test_status(p: &Peripheral, write: &Characteristic, mac: [u8; 6]) -> Result<()> {
    info!("reading status (version / battery / temperature / state); decoded replies below");
    for (label, frame) in [
        ("version (0x9E)", queries::version(mac)),
        ("battery (0x95)", queries::battery(mac)),
        ("temperature (0xB3)", queries::temperature(mac)),
        ("state (0x8E)", queries::state(mac)),
    ] {
        info!(query = label, "→ query");
        ble::write_command(p, write, &frame).await?;
        // Space the queries so the light's reply queue isn't outrun.
        tokio::time::sleep(Duration::from_millis(350)).await;
    }
    // Let any late replies land before we disconnect (decoded by spawn_notify_logger).
    info!("waiting for replies…");
    tokio::time::sleep(Duration::from_millis(2000)).await;
    info!("status read complete");
    Ok(())
}

/// Send one frame (or pixel palette) described by `spec` and hold it — the engine
/// behind `test --set`, for guided one-value-at-a-time hardware testing. The light
/// keeps the state after disconnect. Non-CCT/pixel specs first send a CCT-white
/// frame to clear any latched pixel/FX mode (a plain CCT overrides the animation
/// where a power-cycle does not — see NOTES.md §3.3).
async fn test_set(p: &Peripheral, write: &Characteristic, mac: [u8; 6], spec: &str) -> Result<()> {
    let parts: Vec<&str> = spec.split(':').collect();
    let get = |i: usize| parts.get(i).copied();
    let num = |i: usize, what: &str| -> Result<u32> {
        get(i)
            .with_context(|| format!("--set {spec}: missing {what}"))?
            .parse::<u32>()
            .with_context(|| format!("--set {spec}: {what} must be a number"))
    };

    // Build the frame(s) + a human description. `reset` = clear a pixel/FX latch first.
    let (frames, desc, reset): (Vec<Vec<u8>>, String, bool) = match parts[0] {
        "warmdim" => (vec![classic::cct2(12, 27)], "dim warm white 2700K @ 12%".into(), false),
        "cct" => {
            let (k, bri) = (num(1, "kelvin")?, num(2, "bri")? as u8);
            (vec![classic::cct2(bri, (k / 100) as u8)], format!("CCT {k}K @ {bri}%"), false)
        }
        "cctgm" => {
            // GM CCT probe. Optional 4th part = frame form: 4 (default; the app's
            // cct4), 3 (GL1-family cct3) or 5 (RGB62-family cct_gm5). gm -50..=50.
            let (k, bri) = (num(1, "kelvin")?, num(3, "bri")? as u8);
            let gm: i8 = get(2)
                .with_context(|| format!("--set {spec}: missing gm"))?
                .parse()
                .with_context(|| format!("--set {spec}: gm must be -50..=50"))?;
            let cct = (k / 100) as u8;
            let (frame, form) = match get(4) {
                Some("3") => (classic::cct3(bri, cct, gm), 3),
                Some("5") => (classic::cct_gm5(bri, cct, gm), 5),
                _ => (classic::cct4(bri, cct, gm), 4),
            };
            (vec![frame], format!("CCT{form} {k}K gm{gm:+} @ {bri}%"), false)
        }
        "hsi" => {
            let (hue, sat, bri) = (num(1, "hue")? as u16, num(2, "sat")? as u8, num(3, "bri")? as u8);
            (vec![classic::hsi(hue, sat, bri)], format!("HSI hue={hue} sat={sat} @ {bri}%"), true)
        }
        "xy" => {
            let (x, y, bri) = (num(1, "x")? as u16, num(2, "y")? as u16, num(3, "bri")? as u8);
            (vec![classic::xy_mac(mac, bri, x, y)], format!("XY by-MAC 0xB7 x={x} y={y} @ {bri}%"), true)
        }
        "xydirect" => {
            // Direct 0xB9 — ignored on commandType==2 (Infinity) fixtures like the
            // TL120C, but the form the app sends to everything else. Probe both.
            let (x, y, bri) = (num(1, "x")? as u16, num(2, "y")? as u16, num(3, "bri")? as u8);
            (vec![classic::xy(bri, x, y)], format!("XY direct 0xB9 x={x} y={y} @ {bri}%"), true)
        }
        "fxdirect" => {
            // Direct 0x8B — the 18-effect payload without the MAC wrapper
            // (`setRGBLightValue(EFFECT_MODE_OLD,…)`, cn.java:3458). For fixtures
            // that ignore the MAC 0x91 form.
            let (id, bri) = (num(1, "id")? as u8, num(2, "bri")? as u8);
            (vec![fx_preset_direct(id, bri)], format!("FX direct 0x8B #{id} @ {bri}%"), true)
        }
        "scene" => {
            // Old 9-scene 0x88 — dropped by TL120C firmware; non-Infinity fixtures
            // may honour it. reset=true so an ignored frame leaves plain white.
            let (id, bri) = (num(1, "scene id (1-9)")? as u8, num(2, "bri")? as u8);
            (vec![classic::scene(bri, id)], format!("SCENE 0x88 #{id} @ {bri}%"), true)
        }
        "fx" => {
            let (id, bri) = (num(1, "id")? as u8, num(2, "bri")? as u8);
            (vec![fx_preset(mac, id, bri)], format!("FX #{id} @ {bri}%"), true)
        }
        "pixel" => {
            let blocks: Vec<Block> = get(1)
                .context("--set pixel:<hue,hue,...>:<eff>:<speed>: missing hues")?
                .split(',')
                .map(|h| Ok(Block::Hsi { hue: h.trim().parse::<u16>().context("bad hue")?, sat: 100 }))
                .collect::<Result<_>>()?;
            let (eff, speed) = (num(2, "effect")? as u8, num(3, "speed")? as u8);
            let n = blocks.len();
            // reset=true: a running pixel effect ignores a new pixel palette/effect
            // until a CCT frame clears the latch first (verified on TL120C).
            (pixel::paint(mac, &blocks, 100, eff, speed, 1), format!("PIXEL {n} seg eff={eff} speed={speed}"), true)
        }
        "pixfx" => {
            // Exhaustive per-effect probe: build effect `id` (1..=10) with the app's
            // own default params from the decompile.
            let id = num(1, "effect id")? as u8;
            let (frames, name) = build_pixel_effect_test(mac, id);
            (frames, format!("PIXEL {name}"), true)
        }
        "rgbcw" | "rgbcwmac" => {
            // RGBCW probe. `rgbcwmac` (by-MAC 0xA9) is the WORKING production form
            // (hardware-confirmed 2026-07-01); `rgbcw` (direct 0xA8) is IGNORED on the
            // TL120C and kept only to demonstrate that. reset=true paints CCT-white
            // first, so an ignored frame leaves the light white, a working one jumps
            // to the R/G/B/CW/WW mix.
            // Spec: rgbcw:<r>:<g>:<b>[:<cw>:<ww>:<bri>]  (values 0..=255; bri 0..=100).
            let optu8 = |i: usize, what: &str, dflt: u8| -> Result<u8> {
                get(i)
                    .map(|s| s.parse::<u8>().with_context(|| format!("--set {spec}: {what} must be 0..=255")))
                    .transpose()
                    .map(|o| o.unwrap_or(dflt))
            };
            let (r, g, b) = (num(1, "r")? as u8, num(2, "g")? as u8, num(3, "b")? as u8);
            let (cw, ww, bri) = (optu8(4, "cw", 0)?, optu8(5, "ww", 0)?, optu8(6, "bri", 100)?);
            let (frame, form) = if parts[0] == "rgbcwmac" {
                (classic::rgbcw_mac(mac, bri, r, g, b, cw, ww, 0), "by-MAC 0xA9 (production form — should render)")
            } else {
                (classic::rgbcw(bri, r, g, b, cw, ww, 0), "direct 0xA8 (ignored on TL120C — should stay white)")
            };
            (
                vec![frame],
                format!("RGBCW {form}: R={r} G={g} B={b} CW={cw} WW={ww} @ {bri}%"),
                true,
            )
        }
        other => bail!("--set: unknown spec kind '{other}' (cct|hsi|xy|xydirect|scene|fx|fxdirect|pixel|pixfx|warmdim|rgbcw|rgbcwmac)"),
    };

    if reset {
        info!("clearing any latched pixel/FX mode with a CCT-white frame");
        ble::write_command(p, write, &classic::cct2(50, 56)).await?;
        tokio::time::sleep(Duration::from_millis(900)).await;
    }

    info!("SET {desc}");
    // Send the frame(s) a few times over ~4s while connected so it's easy to watch
    // (pixel palettes are multi-frame, spaced ~80ms and MTU-chunked by the BLE layer).
    for round in 0..4 {
        for frame in &frames {
            ble::write_command_chunked(p, write, frame).await?;
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        if round < 3 {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }
    info!("held; light retains this state after disconnect");
    Ok(())
}
