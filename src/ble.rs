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

/// Acquire a BLE adapter by `[ble] adapter` selector:
///
/// - `"default"` → first adapter,
/// - a number (`"0"`, `"1"`) → that adapter index (see `adapters` command),
/// - any other string → case-insensitive substring match on the OS info string.
///
/// On a miss it logs the available adapters and falls back to the first.
pub async fn acquire_adapter(selector: &str) -> Result<Adapter> {
    let manager = Manager::new().await.context("creating BLE manager")?;
    let adapters = manager.adapters().await.context("listing BLE adapters")?;
    let count = adapters.len();
    debug!(adapters = count, "enumerated BLE adapters");
    if count == 0 {
        bail!("no Bluetooth adapter found — is Bluetooth enabled?");
    }

    // Resolve the per-adapter info strings up front (used for index/substring
    // matching and for the diagnostic listing).
    let mut infos = Vec::with_capacity(count);
    for a in &adapters {
        infos.push(a.adapter_info().await.unwrap_or_else(|_| "<unknown>".into()));
    }

    if selector != "default" {
        // 1) Numeric selector => adapter index (as printed by `adapters` / on a
        //    mismatch). Lets users pick deterministically when info strings clash.
        if let Ok(idx) = selector.parse::<usize>() {
            if let Some(a) = adapters.get(idx) {
                info!(index = idx, adapter = %infos[idx], "selected BLE adapter (by index)");
                return Ok(a.clone());
            }
            warn!(selector, count, "adapter index out of range; falling back to first");
        } else {
            // 2) Otherwise, case-insensitive substring match on the info string.
            let needle = selector.to_lowercase();
            for (i, a) in adapters.iter().enumerate() {
                if infos[i].to_lowercase().contains(&needle) {
                    info!(index = i, adapter = %infos[i], "selected BLE adapter (by name)");
                    return Ok(a.clone());
                }
            }
            warn!(selector, "no adapter matched selector; falling back to first");
        }
        // Help the user fix their config: show what IS available.
        for (i, info) in infos.iter().enumerate() {
            warn!(index = i, adapter = %info, "available adapter");
        }
    }

    let adapter = adapters.into_iter().next().unwrap();
    info!(index = 0usize, adapter = %infos[0], total = count, "using BLE adapter");
    Ok(adapter)
}

/// Enumerate BLE adapters with their index + OS info string (for `adapters` CLI
/// and so users know what to put in `[ble] adapter`).
pub async fn list_adapters() -> Result<Vec<(usize, String)>> {
    let manager = Manager::new().await.context("creating BLE manager")?;
    let adapters = manager.adapters().await.context("listing BLE adapters")?;
    let mut out = Vec::with_capacity(adapters.len());
    for (i, a) in adapters.iter().enumerate() {
        out.push((i, a.adapter_info().await.unwrap_or_else(|_| "<unknown>".into())));
    }
    Ok(out)
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

    let service_uuid = Uuid::parse_str(crate::protocol::uuids::SERVICE).ok();
    let mut out = Vec::new();
    for p in peripherals {
        match p.properties().await {
            Ok(Some(props)) => {
                let name = props.local_name.unwrap_or_default();
                let address = props.address.to_string();
                let rssi = props.rssi;
                // Definitive: advertises the Neewer service UUID. Fallback: name.
                let advertises_service =
                    service_uuid.map(|su| props.services.contains(&su)).unwrap_or(false);
                let is_neewer = advertises_service || is_neewer_name(&name);
                debug!(
                    %address, name = %name, ?rssi, is_neewer, advertises_service,
                    services = ?props.services, "discovered peripheral"
                );
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

/// Stop the shared scan. Used by the [`crate::scan`] coordinator's duty cycle so
/// the adapter isn't scanning while every light is already connected.
pub async fn stop_scan(adapter: &Adapter) -> Result<()> {
    adapter.stop_scan().await.context("stop_scan failed")?;
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

/// Max payload for a single ATT write at the default MTU of 23 (23 − 3 ATT
/// header bytes). Neewer pixel/OTA frames longer than this are rejected as a
/// single write and must be split; the device reassembles them by the frame's
/// header length byte (continuation chunks do NOT re-start with `0x78`).
pub const MAX_ATT_WRITE: usize = 20;

/// Write a possibly-oversized command, splitting it into ≤`MAX_ATT_WRITE`-byte
/// GATT writes when needed (for pixel palettes and other long frames). Short
/// frames go out as a single write, identical to [`write_command`].
pub async fn write_command_chunked(p: &Peripheral, write: &Characteristic, data: &[u8]) -> Result<()> {
    if data.len() <= MAX_ATT_WRITE {
        return write_command(p, write, data).await;
    }
    let wt = if write.properties.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE) {
        WriteType::WithoutResponse
    } else {
        WriteType::WithResponse
    };
    debug!(bytes = %hexstr(data), ?wt, "BLE write (chunked)");
    for chunk in data.chunks(MAX_ATT_WRITE) {
        p.write(write, chunk, wt)
            .await
            .with_context(|| format!("writing chunk of command {}", hexstr(data)))?;
        // Small settle between fragments so the device's reassembler keeps up.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    Ok(())
}

/// A stream of inbound notification frames from a peripheral.
pub type NotifyStream = std::pin::Pin<Box<dyn futures::Stream<Item = btleplug::api::ValueNotification> + Send>>;

/// Subscribe to the notify characteristic and return the notification stream, so a
/// caller (the per-light actor) can decode status replies inline in its select loop.
pub async fn subscribe_notify(p: &Peripheral, notify: &Characteristic) -> Result<NotifyStream> {
    p.subscribe(notify).await.context("subscribe to notify char")?;
    let stream = p.notifications().await.context("opening notification stream")?;
    Ok(stream)
}

/// Subscribe to the notify characteristic and spawn a background task that logs
/// every inbound frame as hex, plus a decoded summary when we recognise the reply
/// (battery/temp/version/state) — useful for reverse-engineering / the `test` probes.
pub async fn spawn_notify_logger(p: &Peripheral, notify: &Characteristic) -> Result<()> {
    let mut stream = subscribe_notify(p, notify).await?;
    tokio::spawn(async move {
        use futures::StreamExt;
        while let Some(n) = stream.next().await {
            match crate::protocol::replies::parse(&n.value) {
                Some(reply) => info!(uuid = %n.uuid, data = %hexstr(&n.value), decoded = %reply.summary(), "BLE notify"),
                None => info!(uuid = %n.uuid, data = %hexstr(&n.value), "BLE notify"),
            }
        }
        debug!("notification stream ended");
    });
    Ok(())
}

/// One characteristic's identity + value, for the `inspect` diagnostic.
pub struct CharInfo {
    pub uuid: String,
    pub props: String,
    pub value: Option<Vec<u8>>,
}

/// Connect, discover services, and read every readable characteristic — a
/// generic GATT dump for identifying unknown / non-standard lights.
pub async fn inspect(p: &Peripheral) -> Result<Vec<CharInfo>> {
    if !p.is_connected().await.unwrap_or(false) {
        p.connect().await.context("connect failed")?;
    }
    p.discover_services().await.context("service discovery failed")?;
    let mut out = Vec::new();
    for c in p.characteristics() {
        let value = if c.properties.contains(CharPropFlags::READ) {
            p.read(&c).await.ok()
        } else {
            None
        };
        out.push(CharInfo {
            uuid: c.uuid.to_string(),
            props: format!("{:?}", c.properties),
            value,
        });
    }
    Ok(out)
}

/// The most recent advertisement RSSI for this peripheral (dBm), if known.
///
/// This is **advertisement** RSSI (btleplug exposes no on-demand connected-link
/// RSSI — see NOTES.md). It's refreshed by the shared scan while the device
/// advertises; a device that stops advertising once connected keeps its last
/// value. Good for signal-strength diagnostics, NOT for liveness (we use a GATT
/// read probe for that). Always `None` on macOS/CoreBluetooth.
pub async fn rssi(p: &Peripheral) -> Option<i16> {
    p.properties().await.ok().flatten().and_then(|pr| pr.rssi)
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
