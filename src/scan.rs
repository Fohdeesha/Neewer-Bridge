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
use tracing::{debug, warn};

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
        if let Err(e) = ble::start_scan(&adapter).await {
            warn!(error = %e, "discovery scan failed to start; will retry");
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
