//! Discovery-scan coordination.
//!
//! btleplug only discovers peripherals while a scan is active, so the bridge
//! needs a scan running whenever a configured light is absent — at startup, for
//! a powered-off fixture, or after a link drops. But a *continuous* scan on a
//! cheap USB controller (e.g. the Realtek RTL8761BU on the test rig) competes
//! with the active connections for the radio and makes the kernel log
//! `LE Set Scan Enable` timeouts (`hci0: Opcode 0x2042 … tx timeout /
//! start background scanning failed: -110`).
//!
//! So rather than one always-on scan, a single coordinator task scans **only
//! while at least one light needs discovery**, and even then in **duty-cycled
//! bursts** (`scan_window_secs` on, `scan_pause_secs` off) instead of
//! continuously. When every light is connected the adapter isn't scanning at
//! all. A light that's deliberately left off for days therefore does NOT cause a
//! constant scan — it's polled in brief periodic bursts until it returns.
//!
//! Each [`crate::light::LightActor`] holds a [`SearchGuard`] for exactly as long
//! as it is disconnected (from the top of a (re)connect attempt until it is
//! connected). The coordinator watches the count of outstanding guards.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use btleplug::platform::Adapter;
use tokio::sync::Notify;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::ble;

/// Safety re-check cadence for the idle wait — a backstop in case a wakeup is
/// ever missed. The `Notify` makes the common case react instantly; this just
/// bounds the worst case.
const IDLE_RECHECK: Duration = Duration::from_secs(5);

/// Shared discovery-need accounting between the light actors and the scan task.
pub struct ScanCoordinator {
    /// Number of lights currently needing discovery (outstanding [`SearchGuard`]s).
    searching: AtomicUsize,
    /// Pulsed whenever `searching` changes, so the coordinator reacts promptly.
    changed: Notify,
}

impl ScanCoordinator {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { searching: AtomicUsize::new(0), changed: Notify::new() })
    }

    /// Register that the caller needs the scan running until the returned guard
    /// is dropped (i.e. until it has connected).
    pub fn begin_search(self: &Arc<Self>) -> SearchGuard {
        self.searching.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_one();
        SearchGuard { coord: Arc::clone(self) }
    }

    /// How many lights currently need discovery.
    pub fn searching(&self) -> usize {
        self.searching.load(Ordering::SeqCst)
    }

    fn end_search(&self) {
        self.searching.fetch_sub(1, Ordering::SeqCst);
        self.changed.notify_one();
    }
}

/// Held by an actor while it is disconnected; dropping it (on connect) decrements
/// the outstanding-search count.
pub struct SearchGuard {
    coord: Arc<ScanCoordinator>,
}

impl Drop for SearchGuard {
    fn drop(&mut self) {
        self.coord.end_search();
    }
}

/// Run the scan coordinator forever. Scans only while lights need discovery, in
/// `window`-on / `pause`-off bursts. `pause == 0` ⇒ continuous while searching
/// (but still off once every light is connected).
pub async fn run(adapter: Adapter, coord: Arc<ScanCoordinator>, window: Duration, pause: Duration) {
    loop {
        // Idle whenever every light is connected — no scanning at all. Each burst
        // below stops its own scan, so nothing is left running here.
        if coord.searching() == 0 {
            let _ = timeout(IDLE_RECHECK, coord.changed.notified()).await;
            continue;
        }

        // At least one light needs discovery: one scan burst.
        //
        // If a scan session is somehow already active — ours, left behind by a
        // `stop_scan` that didn't take, or a stale one in the BLE backend —
        // BlueZ answers `Operation already in progress` and this used to just
        // warn and retry, forever: the coordinator could NEVER get back in
        // sync, so the adapter sat scanning continuously (the exact load this
        // whole module exists to prevent on a cheap controller) while the log
        // took ~4 warnings a minute indefinitely, each one saying "will retry"
        // as though it were transient.
        //
        // HARDWARE-PROVEN on the test rig (2026-08-26), where a live bridge had
        // been in that state for 15+ hours with zero successful discoveries: a
        // 4-call probe showed start_scan #1 OK, #2/#3/#4 `Operation already in
        // progress`, then ONE `stop_scan` → the next `start_scan` OK. So clear
        // it and retry once before giving up on this burst.
        let mut started = ble::start_scan(&adapter).await;
        if started.is_err() {
            let _ = ble::stop_scan(&adapter).await;
            started = ble::start_scan(&adapter).await;
            if started.is_ok() {
                info!("discovery scan recovered by clearing a stale scan session");
            }
        }
        if let Err(e) = started {
            // `{e:#}` prints the anyhow CHAIN. Plain `%e` showed only the
            // context — "start_scan failed (check Bluetooth permissions /
            // adapter power)" — and hid the actual cause, which is why the live
            // failure above needed a hardware session to diagnose at all.
            warn!(error = %format!("{e:#}"), "discovery scan failed to start; will retry");
            wait(&coord, pause.max(IDLE_RECHECK)).await;
            continue;
        }
        debug!(searching = coord.searching(), secs = window.as_secs(), "discovery scan burst: on");
        wait(&coord, window).await;
        let _ = ble::stop_scan(&adapter).await;
        debug!("discovery scan burst: off");

        // Duty-cycle pause with the scan OFF before the next burst (skipped when
        // continuous, i.e. pause == 0, or once everything has connected).
        if !pause.is_zero() && coord.searching() > 0 {
            wait(&coord, pause).await;
        }
    }
}

/// Sleep for `dur`, returning early if every light connects (searching → 0) in
/// the meantime so the coordinator can stop scanning promptly.
async fn wait(coord: &Arc<ScanCoordinator>, dur: Duration) {
    let deadline = tokio::time::sleep(dur);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return,
            _ = coord.changed.notified() => {
                if coord.searching() == 0 {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_counts_track_searchers() {
        let c = ScanCoordinator::new();
        assert_eq!(c.searching(), 0);
        let g1 = c.begin_search();
        assert_eq!(c.searching(), 1);
        let g2 = c.begin_search();
        assert_eq!(c.searching(), 2);
        drop(g1);
        assert_eq!(c.searching(), 1);
        drop(g2);
        assert_eq!(c.searching(), 0);
    }

    #[test]
    fn guards_are_independent_and_order_agnostic() {
        let c = ScanCoordinator::new();
        let a = c.begin_search();
        let b = c.begin_search();
        let d = c.begin_search();
        assert_eq!(c.searching(), 3);
        drop(b);
        drop(a);
        assert_eq!(c.searching(), 1);
        drop(d);
        assert_eq!(c.searching(), 0);
    }
}
