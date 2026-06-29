//! CLI command implementations (orchestration). Thin glue over `ble` + `protocol`.

use anyhow::Result;
use std::time::Duration;
use tracing::info;

use crate::artnet;
use crate::ble;
use crate::config::parse_mac;
use crate::protocol::{classic, home, infinity};

/// `scan` — discover and list lights. Prints a human table; Neewer lights are
/// flagged and shown first so you can copy the MAC straight into the config.
pub async fn scan(adapter_selector: &str, seconds: u64, all: bool) -> Result<()> {
    let adapter = ble::acquire_adapter(adapter_selector).await?;
    let found = ble::scan(&adapter, seconds).await?;

    let shown: Vec<_> = found.iter().filter(|f| all || f.is_neewer).collect();
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
pub async fn test(adapter_selector: &str, mac: &str, driver: &str, seconds: u64) -> Result<()> {
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
