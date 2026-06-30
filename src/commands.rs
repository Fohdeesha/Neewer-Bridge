//! CLI command implementations (orchestration). Thin glue over `ble` + `protocol`.

use anyhow::{bail, Context, Result};
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
use crate::protocol::{classic, home, infinity};

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

/// `scan` — discover and list lights. Prints a human table; Neewer lights are
/// flagged and shown first so you can copy the MAC straight into the config.
pub async fn scan(adapter_selector: &str, seconds: u64, all: bool, json: bool) -> Result<()> {
    let adapter = ble::acquire_adapter(adapter_selector).await?;
    let found = ble::scan(&adapter, seconds).await?;

    let shown: Vec<_> = found.iter().filter(|f| all || f.is_neewer).collect();

    if json {
        // Machine-readable output (one JSON array) for automated tooling.
        let items: Vec<String> = shown
            .iter()
            .map(|f| {
                let rssi = f.rssi.map(|r| r.to_string()).unwrap_or_else(|| "null".into());
                format!(
                    "{{\"name\":\"{}\",\"mac\":\"{}\",\"rssi\":{},\"neewer\":{}}}",
                    json_escape(&f.name),
                    json_escape(&f.address),
                    rssi,
                    f.is_neewer
                )
            })
            .collect();
        println!("[{}]", items.join(","));
        return Ok(());
    }

    if shown.is_empty() {
        println!("\nNo {}lights found. Is the light powered on and in range?", if all { "" } else { "Neewer " });
        println!("(Try `--all` to list every BLE device, or `--seconds N` for a longer scan.)");
        return Ok(());
    }

    println!("\n  {:<3} {:<22} {:<18} {:>5}  TYPE", "#", "NAME", "MAC", "RSSI");
    println!("  {}", "-".repeat(62));
    for (i, f) in shown.iter().enumerate() {
        let rssi = f.rssi.map(|r| format!("{r}")).unwrap_or_else(|| "  ?".into());
        let kind = if f.is_neewer { "Neewer" } else { "other" };
        let name = if f.name.is_empty() { "(no name)" } else { &f.name };
        println!("  {:<3} {:<22} {:<18} {:>4}  {}", i + 1, name, f.address, rssi, kind);
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

    // Blink to identify which physical fixture this is.
    println!("Connecting to blink the light so you can identify it…");
    let mac_bytes = parse_mac(&light.address)?;
    let drv = Driver::resolve(&driver, Profile::Full, mac_bytes, &light.name);
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
                let drv = Driver::resolve(&driver, Profile::Full, mac_bytes, &ble_name);
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
pub async fn test(
    adapter_selector: &str,
    mac: &str,
    driver: &str,
    seconds: u64,
    colors: bool,
    modes: bool,
) -> Result<()> {
    let mac_bytes = parse_mac(mac)?;
    let adapter = ble::acquire_adapter(adapter_selector).await?;
    let peripheral = ble::find_by_mac(&adapter, mac, Duration::from_secs(seconds)).await?;
    let chars = ble::connect_and_verify(&peripheral).await?;

    if let Some(notify) = &chars.notify {
        ble::spawn_notify_logger(&peripheral, notify).await?;
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
        // Return to a neutral white so we don't leave it stuck on a colour.
        info!("restoring 5600K white");
        ble::write_command(&peripheral, &chars.write, &set_cct(50, 56)).await?;
    }

    // Optional advanced-mode probe: exercise RGBCW, XY, and a couple of FX
    // effects. RGBCW/XY are direct classic frames; FX is the MAC-embedded effect
    // frame (the only FX form). Watch the light to confirm each mode engages.
    if modes {
        info!("RGBCW probe — direct R/G/B/CW/WW mixing (0xA8)");
        for (label, c) in [
            ("RED", (255u8, 0u8, 0u8, 0u8, 0u8)),
            ("GREEN", (0, 255, 0, 0, 0)),
            ("BLUE", (0, 0, 255, 0, 0)),
            ("COOL-WHITE", (0, 0, 0, 255, 0)),
            ("WARM-WHITE", (0, 0, 0, 0, 255)),
        ] {
            info!("RGBCW {label}");
            ble::write_command(&peripheral, &chars.write, &classic::rgbcw(100, c.0, c.1, c.2, c.3, c.4)).await?;
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        info!("XY probe — CIE coordinate (0xB9)");
        for (label, x, y) in [("D65 white", 3127u16, 3290u16), ("deep red", 6400, 3300), ("green", 3000, 6000)] {
            info!(x, y, "XY {label}");
            ble::write_command(&peripheral, &chars.write, &classic::xy(100, x, y)).await?;
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
        // control (per protocol-analysis.md), then set a neutral white.
        info!("exiting FX (power-cycle) and restoring 5600K white");
        ble::write_command(&peripheral, &chars.write, &power(false)).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        ble::write_command(&peripheral, &chars.write, &power(true)).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        ble::write_command(&peripheral, &chars.write, &set_cct(50, 56)).await?;
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
