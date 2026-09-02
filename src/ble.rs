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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// One-shot: builds a fresh platform session each call, which is right for the
/// tool commands. The bridge holds a [`BleStack`] instead, so a restart does not
/// open a new session unless the old one is known to be unusable.
pub async fn acquire_adapter(selector: &str) -> Result<Adapter> {
    let manager = Manager::new().await.context("creating BLE manager")?;
    select_adapter(&manager, selector).await
}

/// The bridge's handle on the platform BLE session, kept across in-process
/// restarts.
///
/// On BlueZ a `Manager` is a D-Bus connection whose reader task btleplug
/// detaches, so a `Manager` that is dropped still keeps its connection open for
/// the life of the process. Creating one per restart would leak a connection
/// each time — and the system bus caps connections per user (256 by default),
/// so a bridge retrying an absent adapter every few minutes would talk itself
/// out of the bus within days. So the session is created once and reused;
/// [`BleStack::rebuild`] discards it only when it is known to be unusable (a
/// poisoned connection — see [`stack_verdict`]), where a fresh one is the fix.
pub struct BleStack {
    manager: Option<Manager>,
}

impl BleStack {
    pub fn new() -> Self {
        Self { manager: None }
    }

    /// Select the adapter (see [`acquire_adapter`]) on the shared session,
    /// creating the session on first use or after a [`BleStack::rebuild`].
    pub async fn adapter(&mut self, selector: &str) -> Result<Adapter> {
        if self.manager.is_none() {
            self.manager = Some(Manager::new().await.context("creating BLE manager")?);
        }
        let manager = self.manager.as_ref().expect("just created");
        select_adapter(manager, selector).await
    }

    /// Discard the session so the next [`BleStack::adapter`] opens a new one.
    pub fn rebuild(&mut self) {
        self.manager = None;
    }
}

impl Default for BleStack {
    fn default() -> Self {
        Self::new()
    }
}

/// The selector logic behind [`acquire_adapter`], on an existing session.
async fn select_adapter(manager: &Manager, selector: &str) -> Result<Adapter> {
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

/// The platform's own id for an adapter — `hci0` on BlueZ — out of the info
/// string [`acquire_adapter`] logs (`hci0 (usb:v1D6Bp0246d0552)`). Needed by
/// the stale-link recovery (`recovery.rs`) to address the adapter directly.
pub fn adapter_id_from_info(info: &str) -> String {
    info.split_whitespace().next().unwrap_or_default().to_string()
}

/// [`adapter_id_from_info`] for a live adapter; empty if the info can't be read.
pub async fn adapter_id(adapter: &Adapter) -> String {
    adapter
        .adapter_info()
        .await
        .map(|info| adapter_id_from_info(&info))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Stack health — is the platform BLE session usable at all?
//
// Four days of a dark fleet taught the lesson this exists for: every BLE call
// from the bridge's D-Bus connection can be refused, permanently, while the
// process stays up and every actor keeps retrying (see recovery.rs for how the
// connection got that way). Per-light retries can never notice that, because
// each one only sees its own failure. So the shared primitives below report
// every outcome here, and the bridge's watchdog turns "nothing has worked for
// a long time" (or the exact poisoned-connection error) into a restart.
// ---------------------------------------------------------------------------

/// "Not set" sentinel for the millisecond timestamps below.
const UNSET: u64 = u64::MAX;

/// How long the stack may fail continuously — no successful call at all — before
/// the watchdog declares it dead. Longer than the ~95 s an adapter that is
/// unplugged and re-plugged has been seen to take to come back on its own.
pub const STACK_DEAD_AFTER: Duration = Duration::from_secs(120);

struct StackHealth {
    started: Instant,
    /// When a shared BLE call last succeeded (ms since `started`).
    last_ok_ms: AtomicU64,
    /// When the current run of consecutive failures began, if one is running.
    streak_start_ms: AtomicU64,
    /// The exact poisoned-connection error has been seen.
    poisoned: AtomicBool,
}

static STACK: LazyLock<StackHealth> = LazyLock::new(|| StackHealth {
    started: Instant::now(),
    last_ok_ms: AtomicU64::new(UNSET),
    streak_start_ms: AtomicU64::new(UNSET),
    poisoned: AtomicBool::new(false),
});

fn now_ms() -> u64 {
    STACK.started.elapsed().as_millis() as u64
}

/// A shared BLE call succeeded.
pub fn note_ok() {
    STACK.last_ok_ms.store(now_ms(), Ordering::Relaxed);
    STACK.streak_start_ms.store(UNSET, Ordering::Relaxed);
}

/// A shared BLE call failed. Opens a failure streak if none is running, and
/// latches the poisoned flag if this is the D-Bus pending-reply refusal.
pub fn note_error(e: &anyhow::Error) {
    let _ = STACK.streak_start_ms.compare_exchange(UNSET, now_ms(), Ordering::Relaxed, Ordering::Relaxed);
    if is_bus_poisoned(e) {
        STACK.poisoned.store(true, Ordering::Relaxed);
    }
}

/// Forget everything — called when the bridge (re)starts its BLE session.
pub fn reset_stack_health() {
    STACK.last_ok_ms.store(UNSET, Ordering::Relaxed);
    STACK.streak_start_ms.store(UNSET, Ordering::Relaxed);
    STACK.poisoned.store(false, Ordering::Relaxed);
}

/// Whether an error is dbus-daemon refusing a call because this connection has
/// reached its cap of un-answered method calls (`max_replies_per_connection`,
/// 128 on the system bus). Once that happens no call from the connection can
/// ever succeed again — the platform session is poisoned and must be replaced.
pub fn is_bus_poisoned(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        let s = cause.to_string();
        s.contains("maximum number of pending replies") || s.contains("LimitsExceeded")
    })
}

/// The watchdog's reading of the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackVerdict {
    Healthy,
    /// The poisoned-connection error was seen: replace the session now.
    Poisoned,
    /// Nothing has succeeded for at least [`STACK_DEAD_AFTER`].
    Dead { failing_for: Duration },
}

/// Decide the verdict. Pure — `now`, the timestamps and the flag are inputs, so
/// the rule is testable without a clock or an adapter. A run of failures counts
/// only when it is at least `dead_after` long AND no call has succeeded inside
/// that window (a success ends the run anyway, so the second condition guards
/// against an ordering race between two callers).
fn verdict(now_ms: u64, last_ok_ms: u64, streak_start_ms: u64, poisoned: bool, dead_after: Duration) -> StackVerdict {
    if poisoned {
        return StackVerdict::Poisoned;
    }
    if streak_start_ms == UNSET {
        return StackVerdict::Healthy;
    }
    let failing_for_ms = now_ms.saturating_sub(streak_start_ms);
    let since_ok_ms = if last_ok_ms == UNSET { u64::MAX } else { now_ms.saturating_sub(last_ok_ms) };
    let limit = dead_after.as_millis() as u64;
    if failing_for_ms >= limit && since_ok_ms >= limit {
        StackVerdict::Dead { failing_for: Duration::from_millis(failing_for_ms) }
    } else {
        StackVerdict::Healthy
    }
}

/// The current verdict on the shared session.
pub fn stack_verdict() -> StackVerdict {
    verdict(
        now_ms(),
        STACK.last_ok_ms.load(Ordering::Relaxed),
        STACK.streak_start_ms.load(Ordering::Relaxed),
        STACK.poisoned.load(Ordering::Relaxed),
        STACK_DEAD_AFTER,
    )
}

/// Report a shared call's outcome to the stack health, passing it through.
fn observed<T>(res: Result<T>) -> Result<T> {
    match &res {
        Ok(_) => note_ok(),
        Err(e) => note_error(e),
    }
    res
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
    observed(
        adapter
            .start_scan(ScanFilter::default())
            .await
            .context("start_scan failed (check Bluetooth permissions / adapter power)"),
    )
}

/// Stop the shared scan. Used by the [`crate::scan`] coordinator's duty cycle so
/// the adapter isn't scanning while every light is already connected.
pub async fn stop_scan(adapter: &Adapter) -> Result<()> {
    observed(adapter.stop_scan().await.context("stop_scan failed"))
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
    // Every disconnected light runs this every FIND_POLL, so it is the stack
    // health's main heartbeat — an empty listing is still a success.
    for p in observed(adapter.peripherals().await.context("listing peripherals"))? {
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

/// Whether btleplug currently believes the peripheral is connected. An error
/// (platform unreachable, device object gone) counts as "not connected".
pub async fn is_connected(p: &Peripheral) -> bool {
    p.is_connected().await.unwrap_or(false)
}

/// [`is_connected`] with the error kept: callers that must tell "the platform
/// says no" from "the platform cannot answer" (the stale-record check in
/// `light.rs`) need the difference.
pub async fn connected_state(p: &Peripheral) -> Result<bool> {
    p.is_connected().await.context("reading the connection state")
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
        Ok(Ok(())) => {
            // A completed connect proves the session works (a FAILED one proves
            // nothing about the session — that is usually the light or the RF).
            note_ok();
            Ok(())
        }
        Ok(Err(e)) => Err(e).context("connect failed"),
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

/// Upper bound on one `disconnect()` call. A disconnect the platform CAN
/// perform completes in well under three seconds on BlueZ (its own disconnect
/// timer is 2 s); one it never answers is the signature of a stale device
/// record (recovery.rs), and the library's own 30 s timeout would just make the
/// actor wait three times longer to learn the same thing.
pub const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How a bounded disconnect ended.
#[derive(Debug)]
pub enum DisconnectOutcome {
    Done,
    /// The platform answered with an error (already gone, adapter off, …).
    /// Harmless for a best-effort release; logged at debug by callers.
    Failed(anyhow::Error),
    /// No answer within [`DISCONNECT_TIMEOUT`]. On BlueZ this means the daemon's
    /// record of the device is stale — it is still marked connected with no
    /// link behind it — and the call will never be answered. Each such call
    /// permanently occupies one of the connection's 128 D-Bus reply slots, so a
    /// caller must not simply try again.
    Hung,
}

/// Classify a bounded disconnect: `None` = the timeout elapsed. Pure, so the
/// mapping is pinned by a test without a peripheral.
fn classify_disconnect(res: Option<Result<()>>) -> DisconnectOutcome {
    match res {
        Some(Ok(())) => DisconnectOutcome::Done,
        Some(Err(e)) => DisconnectOutcome::Failed(e),
        None => DisconnectOutcome::Hung,
    }
}

/// Drop a connection, bounded by [`DISCONNECT_TIMEOUT`], reporting what happened.
pub async fn disconnect_outcome(p: &Peripheral) -> DisconnectOutcome {
    let res = tokio::time::timeout(DISCONNECT_TIMEOUT, p.disconnect())
        .await
        .ok()
        .map(|r| r.context("disconnect failed"));
    classify_disconnect(res)
}

/// Cleanly drop a connection (best-effort), bounded by [`DISCONNECT_TIMEOUT`].
/// The tool commands use this; the light actor uses [`disconnect_outcome`] so
/// it can tell a hung disconnect from a failed one.
pub async fn disconnect(p: &Peripheral) -> Result<()> {
    match disconnect_outcome(p).await {
        DisconnectOutcome::Done => Ok(()),
        DisconnectOutcome::Failed(e) => Err(e),
        DisconnectOutcome::Hung => bail!(
            "disconnect not answered within {DISCONNECT_TIMEOUT:?} — the platform still \
             reports the device connected with no link behind it (stale device record)"
        ),
    }
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

    #[test]
    fn adapter_id_is_the_first_token_of_the_info_string() {
        // What btleplug's BlueZ backend formats: "<id> (<modalias>)".
        assert_eq!(adapter_id_from_info("hci0 (usb:v1D6Bp0246d0552)"), "hci0");
        assert_eq!(adapter_id_from_info("hci1"), "hci1");
        assert_eq!(adapter_id_from_info("  hci2  (x)"), "hci2");
        assert_eq!(adapter_id_from_info(""), "");
    }

    #[test]
    fn disconnect_outcomes_are_classified_by_completion_not_success() {
        assert!(matches!(classify_disconnect(Some(Ok(()))), DisconnectOutcome::Done));
        assert!(matches!(classify_disconnect(Some(Err(anyhow::anyhow!("x")))), DisconnectOutcome::Failed(_)));
        // The timeout elapsing is the one outcome that must never be retried
        // blindly — it is the stale-record signature.
        assert!(matches!(classify_disconnect(None), DisconnectOutcome::Hung));
        // ...and the plain Result form reports it as an error that names the cause.
        let e = anyhow::anyhow!("d-bus");
        assert!(matches!(classify_disconnect(Some(Err(e))), DisconnectOutcome::Failed(_)));
    }

    #[test]
    fn bus_poisoning_is_recognised_anywhere_in_the_chain() {
        // The exact text dbus-daemon returns (bus/connection.c), as it reached
        // the log on 2026-09-02 wrapped in the listing context.
        let inner = anyhow::anyhow!("The maximum number of pending replies per connection has been reached");
        let wrapped = inner.context("listing peripherals");
        assert!(is_bus_poisoned(&wrapped));
        // The error NAME alone is enough too.
        assert!(is_bus_poisoned(&anyhow::anyhow!("org.freedesktop.DBus.Error.LimitsExceeded")));
        // Ordinary failures are not poisoning: an absent adapter or a dead
        // daemon self-heal and must not force a session rebuild.
        for other in [
            "listing peripherals: Unit dbus-org.bluez.service failed to load properly",
            "start_scan failed: Operation already in progress",
            "connect failed: le-connection-abort-by-local",
        ] {
            assert!(!is_bus_poisoned(&anyhow::anyhow!("{other}")), "{other}");
        }
    }

    #[test]
    fn stack_verdict_needs_a_long_unbroken_failure_run() {
        use StackVerdict::*;
        let dead = STACK_DEAD_AFTER;
        let ms = |d: Duration| d.as_millis() as u64;
        // Nothing observed yet, or a success with no failure since: healthy.
        assert_eq!(verdict(0, UNSET, UNSET, false, dead), Healthy);
        assert_eq!(verdict(10_000, 9_000, UNSET, false, dead), Healthy);
        // A failure run shorter than the limit is healthy (transient adapter loss).
        assert_eq!(verdict(60_000, 1_000, 5_000, false, dead), Healthy);
        // Exactly the limit, with the last success outside the window: dead.
        let t = 5_000 + ms(dead);
        assert_eq!(verdict(t, 1_000, 5_000, false, dead), Dead { failing_for: dead });
        // ...and dead with no success EVER (a bridge that started poisoned).
        assert_eq!(verdict(t, UNSET, 5_000, false, dead), Dead { failing_for: dead });
        // A success inside the window (racing writer) keeps it alive.
        assert_eq!(verdict(t, t - 1_000, 5_000, false, dead), Healthy);
        // The poisoned flag wins outright, streak or not.
        assert_eq!(verdict(0, UNSET, UNSET, true, dead), Poisoned);
        assert_eq!(verdict(t, t, UNSET, true, dead), Poisoned);
        // A clock that appears to run backwards saturates rather than panicking.
        assert_eq!(verdict(0, UNSET, 5_000, false, dead), Healthy);
    }
}
