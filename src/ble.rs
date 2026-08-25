//! BLE layer (btleplug). Milestone 1-2 scope: discover Neewer lights, confirm
//! the peripheral address is the real MAC, connect, verify the Neewer GATT
//! profile, and send commands. The per-light actor + connection-health logic
//! (`light.rs`) is built on top of these primitives.
//!
//! Everything here logs verbosely (commands are logged as hex) so that bring-up
//! on real hardware is debuggable.

use anyhow::{bail, Context, Result};
use btleplug::api::{
    BDAddr, CharPropFlags, Central, Characteristic, Manager as _, Peripheral as _, ScanFilter,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::{normalize_mac, parse_mac};
use crate::protocol::uuids;

/// A discovered BLE device, with the cached fields we care about.
pub struct Found {
    pub name: String,
    pub address: String,
    pub rssi: Option<i16>,
    pub is_neewer: bool,
    pub peripheral: Peripheral,
}

/// Whether a discovered peripheral's address is the MAC we're looking for.
///
/// Compares the six raw address bytes, which is both separator/case-proof and —
/// crucially — **free**. `Peripheral::address()` returns a field the adapter's
/// own device listing already populated, whereas `Peripheral::properties()` is a
/// D-Bus round trip *per device* on BlueZ (`session.get_device_info`). The
/// per-MAC lookups below poll repeatedly over every discovered device, so they
/// must filter on this before asking anything for its properties — see
/// [`find_scanned`].
fn addr_matches(address: BDAddr, target: [u8; 6]) -> bool {
    address.into_inner() == target
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

/// The Neewer GATT UUIDs, parsed once. They used to be re-parsed inside the
/// characteristic-lookup closures, i.e. once per characteristic examined.
static WRITE_UUID: LazyLock<Uuid> =
    LazyLock::new(|| Uuid::parse_str(uuids::WRITE_CHAR).expect("valid write char uuid"));
static NOTIFY_UUID: LazyLock<Uuid> =
    LazyLock::new(|| Uuid::parse_str(uuids::NOTIFY_CHAR).expect("valid notify char uuid"));
static SERVICE_UUID: LazyLock<Uuid> =
    LazyLock::new(|| Uuid::parse_str(uuids::SERVICE).expect("valid service uuid"));

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
    // Stop the scan we started before propagating a listing error. A plain `?`
    // here leaked a running scan on this path — the identical bug that was fixed
    // in `find_by_mac` below, for the reason recorded there: a scan left running
    // is exactly the adapter load the bridge's scan coordinator exists to avoid.
    let listed = adapter.peripherals().await.context("listing peripherals");
    let _ = adapter.stop_scan().await;
    let peripherals = listed?;

    let mut out = Vec::new();
    for p in peripherals {
        match p.properties().await {
            Ok(Some(props)) => {
                let name = props.local_name.unwrap_or_default();
                let address = props.address.to_string();
                let rssi = props.rssi;
                // Definitive: advertises the Neewer service UUID. Fallback: name.
                let advertises_service = props.services.contains(&SERVICE_UUID);
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
    let target_bytes = parse_mac(target_mac)?;
    let target = normalize_mac(target_mac);
    info!(mac = %target, ?timeout, "looking for light by MAC");
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("start_scan failed")?;

    let start = Instant::now();
    loop {
        // Stop the scan we started before propagating a listing error - the
        // plain `?` here used to leak a running scan on this path (the other
        // two exits below stop it). Tool commands exit soon after, but a
        // scan left running is exactly the adapter load the bridge's scan
        // coordinator exists to prevent.
        let peripherals = match adapter.peripherals().await.context("listing peripherals") {
            Ok(list) => list,
            Err(e) => {
                let _ = adapter.stop_scan().await;
                return Err(e);
            }
        };
        for p in peripherals {
            // Address first (a free field read); only the match pays for a
            // properties() call, which on BlueZ is a D-Bus round trip.
            if !addr_matches(p.address(), target_bytes) {
                continue;
            }
            // Read the name while the scan is still up, as the old code did —
            // BlueZ reaps discovered device objects some time after discovery
            // stops, and there is no reason to race it for a log field.
            let name = peripheral_name(&p).await;
            let _ = adapter.stop_scan().await;
            info!(mac = %target, %name, "found light");
            return Ok(p);
        }
        if start.elapsed() >= timeout {
            let _ = adapter.stop_scan().await;
            bail!("light {target} not found within {timeout:?} (is it powered on and in range?)");
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Start a discovery scan.
///
/// In the bridge this is driven ONLY by the [`crate::scan`] coordinator, which
/// runs it in duty-cycled bursts while at least one light is disconnected and
/// stops it entirely once the fleet is connected. Do NOT reintroduce an
/// always-on scan: a continuous scan alongside active connections starves the
/// radio on cheap USB controllers (the RTL8761BU test rig logged kernel
/// `LE Set Scan Enable` timeouts until this became on-demand). Light actors find
/// their peripheral among whatever the current burst has discovered
/// ([`find_scanned`]).
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
/// coordinated scan. Returns `(peripheral, ble_name, rssi)` or `None` if not
/// seen yet.
///
/// Every disconnected light polls this every couple of seconds, so it matches on
/// the cached address ([`addr_matches`]) and asks only the ONE matching
/// peripheral for its properties. Asking every device instead cost a D-Bus round
/// trip per device per poll on BlueZ — with a fleet down in a busy RF
/// environment that is hundreds of round trips every two seconds, on the exact
/// adapter the duty-cycled scan exists to keep unloaded.
///
/// The RSSI is captured HERE, at discovery time, because it is advertisement
/// RSSI: BlueZ clears the property the moment the device connects, so reading it
/// after connect (as the actor used to) yields `None` almost every time now that
/// scanning is on-demand. The discovery-time value is the freshest signal
/// measurement we will ever have for this session.
pub async fn find_scanned(
    adapter: &Adapter,
    target_mac: &str,
) -> Result<Option<(Peripheral, String, Option<i16>)>> {
    let target = parse_mac(target_mac)?;
    for p in adapter.peripherals().await.context("listing peripherals")? {
        if !addr_matches(p.address(), target) {
            continue;
        }
        // Unreadable properties are no reason to discard a peripheral already
        // matched by MAC — that used to make the light invisible and the actor
        // wait forever. An empty name is handled by the caller (it falls back to
        // the configured name for `driver = "auto"` resolution) and the RSSI is
        // diagnostics only.
        let (name, rssi) = match p.properties().await {
            Ok(Some(props)) => (props.local_name.unwrap_or_default(), props.rssi),
            _ => (String::new(), None),
        };
        return Ok(Some((p, name, rssi)));
    }
    Ok(None)
}

/// Find any readable characteristic for use as a non-mutating liveness probe
/// (the connection check in `light.rs`). Standard Generic Access chars (e.g.
/// Device Name) are usually
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

/// Upper bound on one `connect()` attempt. Generous: marginal fixtures have been
/// observed taking ~21 s to connect on BlueZ, and BlueZ's own supervision usually
/// errors out around 25–40 s — this only fires if the platform call *hangs*
/// (e.g. a stuck D-Bus operation), which would otherwise stall the light's actor
/// forever while it holds a discovery-scan guard.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(45);
/// Upper bound on service discovery after a successful connect.
pub const DISCOVER_TIMEOUT: Duration = Duration::from_secs(30);

/// `connect()` bounded by [`CONNECT_TIMEOUT`] (skipped if already connected).
async fn connect_bounded(p: &Peripheral) -> Result<()> {
    if p.is_connected().await.unwrap_or(false) {
        return Ok(());
    }
    debug!("connecting…");
    match tokio::time::timeout(CONNECT_TIMEOUT, p.connect()).await {
        Ok(res) => res.context("connect failed"),
        Err(_) => bail!("connect timed out after {CONNECT_TIMEOUT:?} (hung platform BLE call)"),
    }
}

/// `discover_services()` bounded by [`DISCOVER_TIMEOUT`].
async fn discover_bounded(p: &Peripheral) -> Result<()> {
    match tokio::time::timeout(DISCOVER_TIMEOUT, p.discover_services()).await {
        Ok(res) => res.context("service discovery failed"),
        Err(_) => bail!("service discovery timed out after {DISCOVER_TIMEOUT:?}"),
    }
}

/// Connect, discover services, and locate the Neewer write/notify
/// characteristics. Fails clearly if this isn't a Neewer light. Both platform
/// calls are time-bounded so a hung BLE stack can never stall a light's actor
/// indefinitely (it fails, backs off, and retries like any other error).
pub async fn connect_and_verify(p: &Peripheral) -> Result<NeewerChars> {
    connect_bounded(p).await?;
    debug!("connected; discovering services");
    discover_bounded(p).await?;

    let chars = p.characteristics();
    debug!(count = chars.len(), "discovered characteristics");
    for c in &chars {
        debug!(uuid = %c.uuid, props = ?c.properties, "  characteristic");
    }

    let write = chars
        .iter()
        .find(|c| c.uuid == *WRITE_UUID)
        .cloned()
        .context("Neewer write characteristic (69400002-…) not found — not a Neewer light?")?;
    let notify = chars.iter().find(|c| c.uuid == *NOTIFY_UUID).cloned();
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

/// Settle time between the fragments of one chunked command (see
/// [`write_command_chunked`]). The device reassembles by the frame's header
/// length byte, and its radio→LED-MCU UART link needs a moment to keep up.
const CHUNK_SETTLE: Duration = Duration::from_millis(10);

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
    let mut chunks = data.chunks(MAX_ATT_WRITE).peekable();
    while let Some(chunk) = chunks.next() {
        p.write(write, chunk, wt)
            .await
            .with_context(|| format!("writing chunk of command {}", hexstr(data)))?;
        // Small settle BETWEEN fragments so the device's reassembler keeps up.
        // Not after the last one — that delay buys nothing and, at flush rates,
        // just eats into the next tick.
        if chunks.peek().is_some() {
            tokio::time::sleep(CHUNK_SETTLE).await;
        }
    }
    Ok(())
}

/// Write one OTA logical frame (`0x96` header or `0x97`/`0xCF` block), fragmenting
/// it into ≤[`MAX_ATT_WRITE`]-byte GATT writes exactly as the device expects (it
/// reassembles by the frame's header length byte).
///
/// Unlike [`write_command_chunked`] this prefers **write-WITH-response** per chunk
/// when the characteristic supports it, so every fragment is ATT-acknowledged
/// before the next — the reliability that a firmware flash wants. It falls back to
/// write-without-response for chars that only advertise that. The block-level
/// device ACK (`0x06`) is the primary flow-control; this just makes each block's
/// bytes land intact. `chunk_delay` spaces fragments (small, e.g. 4–8 ms).
pub async fn write_ota_frame(
    p: &Peripheral,
    write: &Characteristic,
    data: &[u8],
    chunk_delay: Duration,
) -> Result<()> {
    let wt = if write.properties.contains(CharPropFlags::WRITE) {
        WriteType::WithResponse
    } else {
        WriteType::WithoutResponse
    };
    for chunk in data.chunks(MAX_ATT_WRITE) {
        p.write(write, chunk, wt)
            .await
            .with_context(|| format!("writing OTA fragment of {}", hexstr(data)))?;
        if !chunk_delay.is_zero() {
            tokio::time::sleep(chunk_delay).await;
        }
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
    connect_bounded(p).await?;
    discover_bounded(p).await?;
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
/// RSSI read). It's only refreshed while a discovery scan sees the
/// device advertise — and on BlueZ the property is **cleared the moment the
/// device connects**, so on a connected light this is typically `None` now that
/// scanning is on-demand. Callers that want a per-session signal number should
/// use the value [`find_scanned`] captured at discovery time (the actor does).
/// Good for signal-strength diagnostics, NOT for liveness (we use a GATT read
/// probe for that). Always `None` on macOS/CoreBluetooth.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The UUID statics `expect()` on a parse. They're compile-time constants,
    /// so a test is the right place to prove they're well-formed — otherwise a
    /// typo in `protocol::uuids` would only surface when a light is connected.
    #[test]
    fn gatt_uuids_parse_and_are_distinct() {
        assert_eq!(WRITE_UUID.to_string(), uuids::WRITE_CHAR);
        assert_eq!(NOTIFY_UUID.to_string(), uuids::NOTIFY_CHAR);
        assert_eq!(SERVICE_UUID.to_string(), uuids::SERVICE);
        assert_ne!(*WRITE_UUID, *NOTIFY_UUID);
        assert_ne!(*WRITE_UUID, *SERVICE_UUID);
    }

    #[test]
    fn addr_matching_compares_every_byte() {
        let target = [0xD6, 0x50, 0xF2, 0xF6, 0xBB, 0x1B];
        assert!(addr_matches(BDAddr::from(target), target));
        // A difference in ANY byte must fail — a near-miss MAC binding a light
        // to the wrong fixture is the one thing this must never do.
        for i in 0..6 {
            let mut other = target;
            other[i] ^= 0xFF;
            assert!(!addr_matches(BDAddr::from(other), target), "byte {i} ignored");
        }
        // Separator/case tolerance now lives entirely in `parse_mac` (the config
        // side); the platform hands us bytes. Pin that the two agree.
        for written in ["D6:50:F2:F6:BB:1B", "d6-50-f2-f6-bb-1b", "D650F2F6BB1B"] {
            assert!(addr_matches(BDAddr::from(target), parse_mac(written).unwrap()), "{written}");
        }
    }

    #[test]
    fn neewer_name_heuristic() {
        for n in ["NW-20240047&00000000", "NEEWER-TL21C", "nwr-something", "NH-PD20250030", "SL90 Pro"] {
            assert!(is_neewer_name(n), "{n} should match");
        }
        for n in ["LHB-B35DA7F3", "", "iPhone"] {
            assert!(!is_neewer_name(n), "{n} should not match");
        }
    }

    #[test]
    fn hexstr_formats_frames_for_logs() {
        assert_eq!(hexstr(&[0x78, 0x81, 0x01, 0x01, 0xFB]), "78 81 01 01 fb");
        assert_eq!(hexstr(&[]), "");
    }
}
