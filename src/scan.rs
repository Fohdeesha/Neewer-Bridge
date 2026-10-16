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
//!
//! A scan that cannot be started at all is the one thing no light can work
//! around — every missing light waits on discovery — so a run of failed starts
//! lasting [`SCAN_BLOCKED_AFTER`] while a light is waiting is escalated to the
//! fleet-wide recovery gate (`recovery.rs`), which may power-cycle the adapter
//! (and, in `auto`, restart bluetoothd) as `[ble] stale_link_recovery` allows.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use btleplug::platform::Adapter;
use tokio::sync::Notify;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use crate::ble;
use crate::recovery::{Outcome, Problem, StaleLinkRecovery};

/// Safety re-check cadence for the idle wait — a backstop in case a wakeup is
/// ever missed. The `Notify` makes the common case react instantly; this just
/// bounds the worst case.
const IDLE_RECHECK: Duration = Duration::from_secs(5);

/// How long every attempt to start a discovery scan may fail, while a light is
/// waiting to be discovered, before the coordinator asks the recovery gate to
/// reset the adapter. About eight failed bursts at the default pause: long
/// enough that a transient refusal (or a bluetoothd restart) never triggers it,
/// short next to the hours a wedged controller would otherwise sit unnoticed.
pub const SCAN_BLOCKED_AFTER: Duration = Duration::from_secs(120);

/// Two failed scan starts further apart than [`streak_gap`] belong to different
/// runs: nothing needed discovery in between. Deliberately NOT "the coordinator
/// went idle": with an adapter that cannot scan, lights whose device records
/// survive fail to connect at once and sit out their backoff, so the count of
/// searching lights touches zero between attempts — seen on the rig, where
/// resetting on idle kept the run from ever reaching the limit unless some
/// light was missing for good.
const MIN_STREAK_GAP: Duration = Duration::from_secs(60);

/// The gap that splits runs, for a coordinator that retries a failed start
/// every `retry`: three retries' worth, and never under [`MIN_STREAK_GAP`]
/// (`scan_pause_secs` has no upper bound, and a fixed gap shorter than the
/// retry interval would make every failure a run of its own). Pure.
fn streak_gap(retry: Duration) -> Duration {
    retry.saturating_mul(3).max(MIN_STREAK_GAP)
}

/// Bookkeeping for a run of failed scan starts.
#[derive(Debug, Default)]
struct Blocked {
    /// When the current run of failed starts began.
    since: Option<Instant>,
    /// The most recent failed start.
    last: Option<Instant>,
    /// Recovery has done all it may for this run (the ladder is exhausted, or
    /// recovery is off): don't ask again until a scan has started — repeating
    /// it would power-cycle the adapter, dropping every connected light, every
    /// couple of minutes for a fault it has already failed to clear.
    spent: bool,
    /// The escalation has been announced for this run (it is retried quietly
    /// while the gate is rate-limited).
    announced: bool,
}

impl Blocked {
    /// Record a failed start at `now`; one more than `gap` after the previous
    /// failure begins a new run. Returns when the current run began. Pure.
    fn failed(&mut self, now: Instant, gap: Duration) -> Instant {
        if self.last.is_some_and(|l| now.saturating_duration_since(l) > gap) {
            *self = Blocked::default();
        }
        self.last = Some(now);
        *self.since.get_or_insert(now)
    }

    /// A scan started: end the run. Returns how long it lasted if it is one
    /// that was still going (the warnings it logged are owed a release line).
    fn started(&mut self, now: Instant, gap: Duration) -> Option<Duration> {
        let ongoing = self.last.is_some_and(|l| now.saturating_duration_since(l) <= gap);
        let lasted = self.since.filter(|_| ongoing).map(|t| now.saturating_duration_since(t));
        *self = Blocked::default();
        lasted
    }
}

/// Whether the current run of failed starts is due for recovery. Pure.
fn recovery_due(blocked: &Blocked, now: Instant, after: Duration) -> bool {
    !blocked.spent && blocked.since.is_some_and(|t| now.saturating_duration_since(t) >= after)
}

/// Can a discovery scan be started right now? Tried the way a burst tries it
/// (and clears a stale scan session before the one retry); a scan that does
/// start is stopped again at once — this is a check, not a burst.
async fn scan_blocked(adapter: &Adapter) -> bool {
    for attempt in 0..2 {
        if ble::start_scan(adapter).await.is_ok() {
            let _ = ble::stop_scan(adapter).await;
            return false;
        }
        if attempt == 0 {
            let _ = ble::stop_scan(adapter).await;
        }
    }
    true
}

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
/// (but still off once every light is connected). A scan that stays impossible
/// to start is handed to `recovery` (see [`SCAN_BLOCKED_AFTER`]).
pub async fn run(
    adapter: Adapter,
    coord: Arc<ScanCoordinator>,
    window: Duration,
    pause: Duration,
    recovery: Arc<StaleLinkRecovery>,
) {
    let mut blocked = Blocked::default();
    // How long to wait before retrying a scan that failed to start.
    let retry = pause.max(IDLE_RECHECK);
    let gap = streak_gap(retry);
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
            let now = Instant::now();
            let since = blocked.failed(now, gap);
            if recovery_due(&blocked, now, SCAN_BLOCKED_AFTER) {
                if !blocked.announced {
                    blocked.announced = true;
                    error!(
                        failing_secs = now.saturating_duration_since(since).as_secs(),
                        searching = coord.searching(),
                        "discovery scans have not been able to start while a light is waiting — \
                         no missing light can be found"
                    );
                }
                let a = &adapter;
                match recovery.recover(Problem::ScanBlocked, "discovery scan", move || scan_blocked(a)).await {
                    Outcome::AlreadyClear => {
                        info!("discovery scan started on the re-check; carrying on");
                        blocked = Blocked::default();
                        continue;
                    }
                    Outcome::Cleared(step) => {
                        info!(step = %step, "discovery scan unblocked by recovery");
                        blocked = Blocked::default();
                        continue;
                    }
                    Outcome::StillStale => {
                        blocked.spent = true;
                        error!(
                            "discovery scans still cannot start after every permitted recovery step — \
                             no missing light can be found until this clears (check the adapter: \
                             `bluetoothctl show`, `dmesg`)"
                        );
                    }
                    // A step ran moments ago (for a stale record, most likely):
                    // ask again on a later failure, once one is due.
                    Outcome::RateLimited => debug!("discovery scan recovery is rate-limited; will ask again"),
                    Outcome::Disabled => {
                        blocked.spent = true;
                        warn!(
                            "discovery scans cannot start, and stale_link_recovery = \"off\" — not \
                             touching the adapter; reset it by hand (`bluetoothctl power off`, then \
                             `power on`) or restart bluetoothd"
                        );
                    }
                }
            }
            // A plain sleep, not `wait`: `wait` returns the moment the count of
            // searching lights touches zero, and lights whose connects fail at
            // once (the adapter cannot scan, but their records survive) take
            // the count through zero every few seconds — which made a scan that
            // cannot start retry, and warn, about once a second instead of once
            // per pause (measured on the rig: 29 attempts in 40 s).
            tokio::time::sleep(retry).await;
            continue;
        }
        // The release line for the warnings above.
        if let Some(lasted) = blocked.started(Instant::now(), gap) {
            info!(after_secs = lasted.as_secs(), "discovery scan started again");
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
    fn a_blocked_scan_is_escalated_once_per_run_and_only_after_the_limit() {
        let t0 = Instant::now();
        let after = SCAN_BLOCKED_AFTER;
        // No run of failures: never due.
        assert!(!recovery_due(&Blocked::default(), t0 + after * 10, after));

        // A run shorter than the limit is not due; at the limit it is (inclusive).
        let run = Blocked { since: Some(t0), last: Some(t0), spent: false, announced: false };
        assert!(!recovery_due(&run, t0 + after - Duration::from_secs(1), after));
        assert!(recovery_due(&run, t0 + after, after));
        assert!(recovery_due(&run, t0 + after * 5, after), "still due while rate-limited");

        // Once recovery has done all it may for this run, never again — however
        // long it lasts. A new run (after a scan started) starts unspent.
        let spent = Blocked { since: Some(t0), last: Some(t0), spent: true, announced: true };
        assert!(!recovery_due(&spent, t0 + after * 100, after));

        // A clock that appears to run backwards saturates to "not due".
        assert!(!recovery_due(&run, t0 - Duration::from_secs(1), after));
    }

    #[test]
    fn a_run_of_failures_survives_idle_moments_but_not_a_long_gap() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        // The default pause (15 s) retries every 15 s; the gap is the floor.
        let gap = streak_gap(Duration::from_secs(15));
        assert_eq!(gap, MIN_STREAK_GAP);
        let mut b = Blocked::default();

        // Failures every 15 s — with the coordinator idle in between, as it is
        // when the lights' records survive and they sit out their backoff —
        // are ONE run, and it reaches the limit.
        for s in (0..=SCAN_BLOCKED_AFTER.as_secs()).step_by(15) {
            assert_eq!(b.failed(at(s), gap), t0, "the run must keep its start at {s} s");
        }
        assert!(recovery_due(&b, at(SCAN_BLOCKED_AFTER.as_secs()), SCAN_BLOCKED_AFTER));

        // Spent stays spent within the run...
        b.spent = true;
        b.failed(at(135), gap);
        assert!(!recovery_due(&b, at(135), SCAN_BLOCKED_AFTER));
        // ...but a failure after a gap longer than the limit (nothing needed
        // discovery in between) is a new run: fresh start, unspent, not due.
        assert_eq!(b.failed(at(135 + 61), gap), at(196));
        assert!(!b.spent);
        assert!(!recovery_due(&b, at(196), SCAN_BLOCKED_AFTER), "an old run must not make a new one due");

        // A start ends the run and reports how long it lasted (the warnings
        // are owed a release line)...
        assert_eq!(b.started(at(210), gap), Some(Duration::from_secs(14)));
        assert!(b.since.is_none() && b.last.is_none());
        // ...unless the run had already lapsed — then there is nothing to release.
        b.failed(at(300), gap);
        assert_eq!(b.started(at(300 + 61), gap), None);
        // And with no failures at all, a start reports nothing.
        assert_eq!(Blocked::default().started(at(0), gap), None);
    }

    #[test]
    fn the_run_gap_follows_a_long_retry_interval() {
        // scan_pause_secs has no upper bound. With a 5-minute pause, failures
        // are 5 minutes apart; a fixed 60 s gap would make each one a run of
        // its own and the escalation could never come due.
        let retry = Duration::from_secs(300);
        let gap = streak_gap(retry);
        assert_eq!(gap, Duration::from_secs(900));
        let t0 = Instant::now();
        let mut b = Blocked::default();
        b.failed(t0, gap);
        assert_eq!(b.failed(t0 + retry, gap), t0, "consecutive retries must stay one run");
        assert!(recovery_due(&b, t0 + retry, SCAN_BLOCKED_AFTER));
        // Absurd values saturate instead of panicking.
        assert_eq!(streak_gap(Duration::MAX), Duration::MAX);
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
