//! BLE layer (btleplug). Milestone 1-2 scope: discover Neewer lights, confirm
//! the peripheral address is the real MAC, connect, verify the Neewer GATT
//! profile, and send commands. The per-light actor + health FSM (NOTES.md §5)
//! is built on top of these primitives in a later milestone.
//!
//! Everything here logs verbosely (commands are logged as hex) so that bring-up
//! on real hardware is debuggable.

use anyhow::{bail, Context, Result};
use btleplug::api::{
    CharPropFlags, Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::normalize_mac;
use crate::protocol::uuids;

/// A discovered BLE device, with the cached fields we care about.
pub struct Found {
    pub name: String,
    pub address: String,
    pub rssi: Option<i16>,
    pub is_neewer: bool,
    pub peripheral: Peripheral,
}

/// Port of NeewerLite's `isValidPeripheralName` — a heuristic name filter for
/// Neewer lights. Deliberately broad (mirrors upstream); the authoritative
/// binding is still by MAC.
pub fn is_neewer_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("nwr")
        || n.contains("neewer")
        || n.contains("nee")
        || n.starts_with("nw-")
        || n.starts_with("nh-")
        || n.starts_with("sl")
}

fn write_uuid() -> Uuid {
    Uuid::parse_str(uuids::WRITE_CHAR).expect("valid write char uuid")
}
fn notify_uuid() -> Uuid {
    Uuid::parse_str(uuids::NOTIFY_CHAR).expect("valid notify char uuid")
}

/// Acquire a BLE adapter. `selector` is currently only `"default"` (first
/// adapter); kept as a param so config-driven selection can grow later.
pub async fn acquire_adapter(selector: &str) -> Result<Adapter> {
    let manager = Manager::new().await.context("creating BLE manager")?;
    let adapters = manager.adapters().await.context("listing BLE adapters")?;
    let count = adapters.len();
    debug!(adapters = count, "enumerated BLE adapters");
    if count == 0 {
        bail!("no Bluetooth adapter found — is Bluetooth enabled?");
    }
    if selector != "default" {
        // Best-effort: match adapter by its OS info string.
        for a in &adapters {
            if let Ok(info) = a.adapter_info().await {
                if info.contains(selector) {
                    info!(adapter = %info, "selected BLE adapter");
                    return Ok(a.clone());
                }
            }
        }
        warn!(selector, "no adapter matched selector; falling back to first");
    }
    let adapter = adapters.into_iter().next().unwrap();
    if let Ok(info) = adapter.adapter_info().await {
        info!(adapter = %info, "using BLE adapter");
    }
    Ok(adapter)
}

/// Scan for `secs` seconds and return everything seen (Neewer or not). Sorted by
/// descending RSSI (strongest first).
pub async fn scan(adapter: &Adapter, secs: u64) -> Result<Vec<Found>> {
    info!(seconds = secs, "starting BLE scan");
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("start_scan failed (check Bluetooth permissions / adapter power)")?;
    tokio::time::sleep(Duration::from_secs(secs)).await;
    let peripherals = adapter.peripherals().await.context("listing peripherals")?;
    let _ = adapter.stop_scan().await;

    let mut out = Vec::new();
    for p in peripherals {
        match p.properties().await {
            Ok(Some(props)) => {
                let name = props.local_name.unwrap_or_default();
                let address = props.address.to_string();
                let rssi = props.rssi;
                let is_neewer = is_neewer_name(&name);
                debug!(%address, name = %name, ?rssi, is_neewer, "discovered peripheral");
                out.push(Found { name, address, rssi, is_neewer, peripheral: p });
            }
            Ok(None) => debug!("peripheral with no properties; skipping"),
            Err(e) => warn!(error = %e, "failed reading peripheral properties"),
        }
    }
    out.sort_by_key(|f| std::cmp::Reverse(f.rssi.unwrap_or(i16::MIN)));
    info!(found = out.len(), "scan complete");
    Ok(out)
}

/// Scan until a peripheral with the target MAC appears, or `timeout` elapses.
/// More responsive than a fixed-window scan when we already know what we want.
pub async fn find_by_mac(adapter: &Adapter, target_mac: &str, timeout: Duration) -> Result<Peripheral> {
    let target = normalize_mac(target_mac);
    info!(mac = %target, ?timeout, "looking for light by MAC");
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("start_scan failed")?;

    let start = Instant::now();
    loop {
        for p in adapter.peripherals().await.context("listing peripherals")? {
            if let Ok(Some(props)) = p.properties().await {
                if normalize_mac(&props.address.to_string()) == target {
                    let _ = adapter.stop_scan().await;
                    info!(mac = %target, name = %props.local_name.unwrap_or_default(), "found light");
                    return Ok(p);
                }
            }
        }
        if start.elapsed() >= timeout {
            let _ = adapter.stop_scan().await;
            bail!("light {target} not found within {timeout:?} (is it powered on and in range?)");
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Start a continuous scan (idempotent-ish). The bridge runs ONE shared scan and
/// each light actor finds its peripheral from the discovered set — avoids the
/// per-actor `start_scan`/`stop_scan` churn that can fight on one adapter.
pub async fn start_scan(adapter: &Adapter) -> Result<()> {
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("start_scan failed (check Bluetooth permissions / adapter power)")?;
    Ok(())
}

/// Look for a peripheral with `target_mac` among those already discovered by the
/// shared scan. Returns `(peripheral, ble_name)` or `None` if not seen yet.
pub async fn find_scanned(adapter: &Adapter, target_mac: &str) -> Result<Option<(Peripheral, String)>> {
    let target = normalize_mac(target_mac);
    for p in adapter.peripherals().await.context("listing peripherals")? {
        if let Ok(Some(props)) = p.properties().await {
            if normalize_mac(&props.address.to_string()) == target {
                let name = props.local_name.unwrap_or_default();
                return Ok(Some((p, name)));
            }
        }
    }
    Ok(None)
}

/// Find any readable characteristic for use as a non-mutating liveness probe
/// (NOTES.md §5). Standard Generic Access chars (e.g. Device Name) are usually
/// readable even if the Neewer control chars are not.
pub fn find_readable_char(p: &Peripheral) -> Option<Characteristic> {
    p.characteristics()
        .into_iter()
        .find(|c| c.properties.contains(CharPropFlags::READ))
}

/// Read a characteristic with a timeout — the liveness probe round-trip. Returns
/// `true` if the read completed (link is alive), `false` on error/timeout.
pub async fn probe_read(p: &Peripheral, c: &Characteristic, timeout: Duration) -> bool {
    matches!(tokio::time::timeout(timeout, p.read(c)).await, Ok(Ok(_)))
}

/// Whether btleplug currently believes the peripheral is connected.
pub async fn is_connected(p: &Peripheral) -> bool {
    p.is_connected().await.unwrap_or(false)
}

/// The characteristics needed to talk to a Neewer light.
pub struct NeewerChars {
    pub write: Characteristic,
    pub notify: Option<Characteristic>,
}

/// Connect, discover services, and locate the Neewer write/notify
/// characteristics. Fails clearly if this isn't a Neewer light.
pub async fn connect_and_verify(p: &Peripheral) -> Result<NeewerChars> {
    if !p.is_connected().await.unwrap_or(false) {
        debug!("connecting…");
        p.connect().await.context("connect failed")?;
    }
    debug!("connected; discovering services");
    p.discover_services().await.context("service discovery failed")?;

    let chars = p.characteristics();
    debug!(count = chars.len(), "discovered characteristics");
    for c in &chars {
        debug!(uuid = %c.uuid, props = ?c.properties, "  characteristic");
    }

    let write = chars
        .iter()
        .find(|c| c.uuid == write_uuid())
        .cloned()
        .context("Neewer write characteristic (69400002-…) not found — not a Neewer light?")?;
    let notify = chars.iter().find(|c| c.uuid == notify_uuid()).cloned();
    if notify.is_none() {
        warn!("Neewer notify characteristic (69400003-…) not found; continuing without notifications");
    }
    Ok(NeewerChars { write, notify })
}

/// Write a command, choosing write-without-response when the characteristic
/// supports it (low latency, matching the real-time lighting path) and falling
/// back to write-with-response otherwise. Logs the exact bytes.
pub async fn write_command(p: &Peripheral, write: &Characteristic, data: &[u8]) -> Result<()> {
    let wt = if write.properties.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE) {
        WriteType::WithoutResponse
    } else {
        WriteType::WithResponse
    };
    debug!(bytes = %hexstr(data), ?wt, "BLE write");
    p.write(write, data, wt)
        .await
        .with_context(|| format!("writing command {}", hexstr(data)))?;
    Ok(())
}

/// Subscribe to the notify characteristic and spawn a background task that logs
/// every inbound frame as hex (useful for reverse-engineering / liveness debug).
pub async fn spawn_notify_logger(p: &Peripheral, notify: &Characteristic) -> Result<()> {
    p.subscribe(notify).await.context("subscribe to notify char")?;
    let mut stream = p.notifications().await.context("opening notification stream")?;
    tokio::spawn(async move {
        use futures::StreamExt;
        while let Some(n) = stream.next().await {
            info!(uuid = %n.uuid, data = %hexstr(&n.value), "BLE notify");
        }
        debug!("notification stream ended");
    });
    Ok(())
}

/// The peripheral's advertised local name (empty if unavailable).
pub async fn peripheral_name(p: &Peripheral) -> String {
    p.properties()
        .await
        .ok()
        .flatten()
        .and_then(|pr| pr.local_name)
        .unwrap_or_default()
}

/// Cleanly drop a connection (best-effort).
pub async fn disconnect(p: &Peripheral) -> Result<()> {
    p.disconnect().await.context("disconnect failed")?;
    Ok(())
}

/// Lower-case spaced hex for logging, e.g. `78 81 01 01 fb`.
pub fn hexstr(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
}
