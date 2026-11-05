//! Is the Bluetooth controller still passing traffic?
//!
//! On 2026-11-03 the rig's USB dongle stopped passing LE data and the fleet sat
//! dark for seven hours while the bridge reported "4 of 5 lights connected" and
//! logged nothing. Nothing the bridge could see said otherwise:
//!
//! - the kernel logged `link tx timeout` / `killing stalled connection`, but its
//!   disconnects never completed, so BlueZ kept every light `Connected: yes`;
//! - writes are write-without-response, and bluetoothd answers "success" as soon
//!   as it has queued one, so every write looked fine;
//! - BlueZ exports no readable characteristic for these fixtures, so the
//!   GATT-read probe that catches this on Windows has nothing to read
//!   (`light.rs`), and reply-gating was removed because deaf fixtures and wedged
//!   ones made it recycle healthy links.
//!
//! What cannot be faked is the kernel's own count of ACL packets sent to the
//! controller. It stops the moment the controller stops handing back transmit
//! credits, and it is shared by every light on the adapter, so it needs nothing
//! from any fixture. A stall is therefore: **the platform keeps accepting writes
//! while not one packet leaves the adapter for [`STALL_AFTER`].**
//!
//! That goes to the same fleet-wide gate as every other adapter-level fault
//! (`recovery.rs`), which is what cleared the incident by hand. Whether a step
//! worked is judged by data coming back from a light ([`Counters::acl_rx`]): a
//! reset controller has fresh transmit credits, so a few packets leaving it
//! prove nothing.
//!
//! Linux only. On Windows the GATT-read probe works, and there is no such
//! counter to read.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info, trace, warn};

use crate::ble;
use crate::recovery::{Outcome, Problem, StaleLinkRecovery};
use crate::scan::ScanCoordinator;

/// How long writes may go unsent before the controller counts as stalled. Twice
/// the kernel's own 45 s timer, which it sets just above the longest legal link
/// supervision timeout: by then the kernel's stall handling has already fired
/// and had as long again to work, so a slow or dying link cannot reach this on
/// a controller that is still working.
pub const STALL_AFTER: Duration = Duration::from_secs(90);
/// How often the counters are read. Reading them does not touch the controller.
const CHECK_EVERY: Duration = Duration::from_secs(5);
/// While stalled, how soon to try again after the ladder ran out or is turned
/// off. Retrying costs nothing the stall has not already cost, since no light
/// is being reached, and a transport fault can clear long after the first try.
const RETRY_AFTER: Duration = Duration::from_secs(600);
/// While stalled, how soon to ask again when the gate says every step ran too
/// recently.
const RATE_LIMITED_RETRY: Duration = Duration::from_secs(15);

/// The kernel's packet counters for one adapter. They survive an adapter
/// power-cycle, but only ever compare them for equality: they are 32-bit and
/// the kernel can reset them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// ACL packets handed to the controller.
    pub acl_tx: u32,
    /// ACL packets received from the controller, that is, data from a light.
    pub acl_rx: u32,
}

/// One reading of everything the stall decision depends on.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub counters: Counters,
    /// [`ble::writes_accepted`].
    pub writes: u64,
    /// How many lights have a live session.
    pub sessions: usize,
}

/// What one sample means. See [`StallTracker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// Nothing to act on or report.
    Steady,
    /// Writes have gone unsent for at least the limit.
    Stalled { quiet_for: Duration, writes: u64 },
    /// Data came back from a light after a stall was reported.
    Moving { after: Duration },
}

/// Writes seen with no packet leaving the adapter since.
#[derive(Debug, Clone, Copy)]
struct Waiting {
    since: Instant,
    /// [`Sample::writes`] when the wait began.
    writes: u64,
}

/// Turns samples into edges. Pure: `now` is an input, so the rule is testable
/// without a clock or an adapter.
///
/// A wait needs a LATER write to count, on top of the time limit. The accepted
/// count and the packet counter are not read atomically with each other, and a
/// write can lose a race with its link going down, so a single unsent write is
/// not evidence; another one, at least a sample later, with still no packet out
/// is.
#[derive(Debug, Default)]
pub struct StallTracker {
    last: Option<Counters>,
    /// [`Sample::writes`] when a packet last left the adapter.
    writes_sent: u64,
    waiting: Option<Waiting>,
    stalled_since: Option<Instant>,
}

impl StallTracker {
    pub fn observe(&mut self, s: Sample, now: Instant, limit: Duration) -> Edge {
        let Some(last) = self.last.replace(s.counters) else {
            self.writes_sent = s.writes;
            return Edge::Steady;
        };
        let tx_moved = s.counters.acl_tx != last.acl_tx;
        if tx_moved {
            self.writes_sent = s.writes;
            self.waiting = None;
        }
        // Only data from a light ends a reported stall. Packets going out prove
        // nothing: a controller reset by the ladder has fresh transmit credits,
        // and lights retrying their connections spend them on a controller that
        // may still pass nothing back.
        if let Some(since) = self.stalled_since {
            if s.counters.acl_rx == last.acl_rx {
                return Edge::Steady;
            }
            self.stalled_since = None;
            return Edge::Moving { after: now.saturating_duration_since(since) };
        }
        // With no live session there is no link to judge, and whatever was
        // waiting went down with the link.
        if s.sessions == 0 {
            self.writes_sent = s.writes;
            self.waiting = None;
            return Edge::Steady;
        }
        if s.writes <= self.writes_sent {
            return Edge::Steady;
        }
        let wait = *self.waiting.get_or_insert(Waiting { since: now, writes: s.writes });
        let quiet_for = now.saturating_duration_since(wait.since);
        if quiet_for < limit || s.writes <= wait.writes {
            return Edge::Steady;
        }
        self.stalled_since = Some(now);
        Edge::Stalled { quiet_for, writes: s.writes - self.writes_sent }
    }

    /// Whether a stall has been reported and traffic has not moved since.
    pub fn stalled(&self) -> bool {
        self.stalled_since.is_some()
    }

    /// Start over: the next sample is a new baseline.
    pub fn forget(&mut self) {
        *self = Self::default();
    }
}

/// The kernel's device index for a BlueZ adapter id such as `hci0`.
pub fn hci_index(adapter_id: &str) -> Option<u16> {
    adapter_id.strip_prefix("hci")?.parse().ok()
}

/// Watch adapter `index` for the life of the bridge run, and hand a stall to the
/// recovery gate. `stalled` mirrors the verdict for the service status line.
pub async fn watch(
    adapter: String,
    index: u16,
    scan: Arc<ScanCoordinator>,
    recovery: Arc<StaleLinkRecovery>,
    stalled: Arc<AtomicBool>,
) {
    let mut tick = interval(CHECK_EVERY);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut tracker = StallTracker::default();
    let mut next_attempt = Instant::now();
    let mut unreadable = false;
    loop {
        tick.tick().await;
        let counters = match platform::counters(index) {
            Ok(Some(counters)) => {
                if unreadable {
                    unreadable = false;
                    info!(adapter = %adapter, "the adapter's packet counters can be read again; the controller stall check is back on");
                }
                counters
            }
            // The adapter has gone. The stack watchdog's presence check handles
            // that; this check has nothing to judge until it is back.
            Ok(None) => {
                debug!(adapter = %adapter, "the adapter is not there to read its packet counters");
                tracker.forget();
                stalled.store(false, Ordering::Relaxed);
                continue;
            }
            Err(e) => {
                if !unreadable {
                    unreadable = true;
                    warn!(
                        adapter = %adapter, error = %e,
                        "cannot read the adapter's packet counters — a stalled Bluetooth \
                         controller will not be noticed until this clears"
                    );
                }
                tracker.forget();
                stalled.store(false, Ordering::Relaxed);
                continue;
            }
        };
        let now = Instant::now();
        let sample = Sample { counters, writes: ble::writes_accepted(), sessions: scan.connected() };
        trace!(
            adapter = %adapter, acl_tx = counters.acl_tx, acl_rx = counters.acl_rx,
            writes = sample.writes, sessions = sample.sessions, "controller counters"
        );
        match tracker.observe(sample, now, STALL_AFTER) {
            Edge::Steady => {}
            Edge::Stalled { quiet_for, writes } => {
                error!(
                    adapter = %adapter, quiet_secs = quiet_for.as_secs(), writes,
                    lights_reported_connected = sample.sessions,
                    "the Bluetooth controller has stopped passing traffic: writes are being \
                     accepted but no packet is leaving the adapter, so no light is being reached \
                     even though the platform still reports them connected"
                );
            }
            Edge::Moving { after } => info!(
                adapter = %adapter, after_secs = after.as_secs(),
                "the Bluetooth controller is passing traffic again (data is coming back from the lights)"
            ),
        }
        stalled.store(tracker.stalled(), Ordering::Relaxed);
        if !tracker.stalled() || now < next_attempt {
            continue;
        }

        // Still bad = no data has come back from any light since this reading.
        let outcome = recovery
            .recover(Problem::ControllerStalled, "controller", || async move {
                !matches!(platform::counters(index), Ok(Some(c)) if c.acl_rx != counters.acl_rx)
            })
            .await;
        let now = Instant::now();
        match outcome {
            Outcome::AlreadyClear => {
                info!(adapter = %adapter, "the Bluetooth controller started passing traffic again before recovery ran");
                tracker.forget();
            }
            Outcome::Cleared(step) => {
                info!(adapter = %adapter, step = %step, "the Bluetooth controller is passing traffic again");
                tracker.forget();
            }
            Outcome::StillStale => {
                next_attempt = now + RETRY_AFTER;
                error!(
                    adapter = %adapter, retry_secs = RETRY_AFTER.as_secs(),
                    "no data has come back from any light after every permitted recovery step \
                     (the controller is still stalled, or no light is in reach) — check `dmesg`; \
                     replugging the adapter may be the only fix"
                );
            }
            Outcome::RateLimited => {
                next_attempt = now + RATE_LIMITED_RETRY;
                debug!(adapter = %adapter, "controller stall recovery is rate-limited; will ask again");
            }
            Outcome::Disabled => {
                next_attempt = now + RETRY_AFTER;
                warn!(
                    adapter = %adapter,
                    "the Bluetooth controller is stalled, and stale_link_recovery = \"off\" — not \
                     touching the adapter; no light is being reached until it is reset by hand \
                     (`btmgmt power off`, `btmgmt power on`, `systemctl restart bluetooth`)"
                );
            }
        }
        stalled.store(tracker.stalled(), Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::Counters;

    /// `BTPROTO_HCI`, from the kernel's `include/net/bluetooth/bluetooth.h`.
    const BTPROTO_HCI: libc::c_int = 1;
    /// `_IOR('H', 211, int)`, from `include/net/bluetooth/hci_sock.h`.
    const HCIGETDEVINFO: libc::c_ulong = 0x8004_48D3;

    /// `struct hci_dev_stats` in `include/net/bluetooth/hci_sock.h`.
    #[repr(C)]
    #[derive(Debug, Default)]
    pub(super) struct DevStats {
        pub err_rx: u32,
        pub err_tx: u32,
        pub cmd_tx: u32,
        pub evt_rx: u32,
        pub acl_tx: u32,
        pub acl_rx: u32,
        pub sco_tx: u32,
        pub sco_rx: u32,
        pub byte_rx: u32,
        pub byte_tx: u32,
    }

    /// `struct hci_dev_info` in `include/net/bluetooth/hci_sock.h`.
    #[repr(C)]
    #[derive(Debug, Default)]
    pub(super) struct DevInfo {
        pub dev_id: u16,
        pub name: [u8; 8],
        pub bdaddr: [u8; 6],
        pub flags: u32,
        pub dev_type: u8,
        pub features: [u8; 8],
        pub pkt_type: u32,
        pub link_policy: u32,
        pub link_mode: u32,
        pub acl_mtu: u16,
        pub acl_pkts: u16,
        pub sco_mtu: u16,
        pub sco_pkts: u16,
        pub stat: DevStats,
    }

    /// Read adapter `index`'s packet counters. `Ok(None)` = no such adapter. The
    /// kernel answers from memory without talking to the controller, so this
    /// returns at once even with the controller stalled, and needs no privilege.
    pub fn counters(index: u16) -> io::Result<Option<Counters>> {
        // SAFETY: plain socket(2) with constant arguments; the result is checked.
        let fd = unsafe {
            libc::socket(libc::AF_BLUETOOTH, libc::SOCK_RAW | libc::SOCK_CLOEXEC, BTPROTO_HCI)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just created, checked, and is owned by nothing else.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut info = DevInfo { dev_id: index, ..DevInfo::default() };
        // SAFETY: `info` is a live, exclusively borrowed `struct hci_dev_info`
        // (its layout is pinned by a test), which is what HCIGETDEVINFO fills.
        let rc = unsafe { libc::ioctl(sock.as_raw_fd(), HCIGETDEVINFO, &mut info as *mut DevInfo) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::ENODEV) { Ok(None) } else { Err(e) };
        }
        Ok(Some(Counters { acl_tx: info.stat.acl_tx, acl_rx: info.stat.acl_rx }))
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::io;

    use super::Counters;

    pub fn counters(_index: u16) -> io::Result<Option<Counters>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "HCI counters exist only on Linux"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = STALL_AFTER;

    fn sample(acl_tx: u32, acl_rx: u32, writes: u64, sessions: usize) -> Sample {
        Sample { counters: Counters { acl_tx, acl_rx }, writes, sessions }
    }

    #[test]
    fn hci_index_is_the_number_in_a_bluez_adapter_id() {
        assert_eq!(hci_index("hci0"), Some(0));
        assert_eq!(hci_index("hci12"), Some(12));
        for bad in ["", "hci", "hci-1", "bt0", "hci0 (usb:v1D6B)", "WinRT"] {
            assert_eq!(hci_index(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_healthy_fleet_never_stalls_however_long_it_runs() {
        // Measured shape: every probe tick the lights get status queries and the
        // packet counter moves with them.
        let t0 = Instant::now();
        let mut t = StallTracker::default();
        let (mut tx, mut writes) = (100u32, 0u64);
        for tick in 0..2_000u64 {
            if tick % 4 == 0 {
                writes += 8;
                tx += 8;
            }
            let edge = t.observe(sample(tx, 50, writes, 4), t0 + Duration::from_secs(tick * 5), LIMIT);
            assert_eq!(edge, Edge::Steady, "tick {tick}");
        }
    }

    #[test]
    fn the_2026_11_03_stall_is_reported_at_the_limit_and_not_before() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut t = StallTracker::default();
        // Baseline, then one last sample where the counter moved.
        assert_eq!(t.observe(sample(12_148, 7_429, 100, 4), at(0), LIMIT), Edge::Steady);
        // From here the platform keeps accepting writes and nothing goes out.
        let mut writes = 100;
        for s in (5..95).step_by(5) {
            writes += 2;
            assert_eq!(t.observe(sample(12_148, 7_429, writes, 4), at(s), LIMIT), Edge::Steady, "at {s} s");
        }
        // The wait began at the first sample that saw an unsent write (5 s).
        writes += 2;
        assert_eq!(
            t.observe(sample(12_148, 7_429, writes, 4), at(95), LIMIT),
            Edge::Stalled { quiet_for: LIMIT, writes: writes - 100 }
        );
        assert!(t.stalled());
        // Reported once; it stays stalled while nothing moves.
        for s in (100..400).step_by(5) {
            writes += 2;
            assert_eq!(t.observe(sample(12_148, 7_429, writes, 4), at(s), LIMIT), Edge::Steady);
            assert!(t.stalled());
        }
    }

    #[test]
    fn an_idle_adapter_is_not_a_stalled_one() {
        // A flat counter with nothing asked of it, e.g. a long probe interval
        // over a static scene: no write, no verdict, ever.
        let t0 = Instant::now();
        let mut t = StallTracker::default();
        for s in (0..100_000u64).step_by(5) {
            assert_eq!(t.observe(sample(7, 7, 42, 4), t0 + Duration::from_secs(s), LIMIT), Edge::Steady);
        }
    }

    #[test]
    fn one_unsent_write_is_not_enough() {
        // The accepted count is read apart from the packet counter, and a write
        // can lose a race with its link going down. With a long probe interval
        // the next write may be minutes away; it decides, the clock alone doesn't.
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut t = StallTracker::default();
        assert_eq!(t.observe(sample(7, 7, 10, 1), at(0), LIMIT), Edge::Steady);
        for s in (5..1_000).step_by(5) {
            assert_eq!(t.observe(sample(7, 7, 11, 1), at(s), LIMIT), Edge::Steady, "at {s} s");
        }
        // Next write, and its packet left: it was only ever the race.
        assert_eq!(t.observe(sample(8, 7, 12, 1), at(1_000), LIMIT), Edge::Steady);
        // The same thing again, except this write's packet never leaves.
        assert_eq!(t.observe(sample(8, 7, 13, 1), at(1_005), LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(8, 7, 14, 1), at(1_100), LIMIT), Edge::Stalled {
            quiet_for: Duration::from_secs(95),
            writes: 2,
        });
    }

    #[test]
    fn a_packet_leaving_restarts_the_wait() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut t = StallTracker::default();
        assert_eq!(t.observe(sample(1, 0, 0, 1), at(0), LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(1, 0, 2, 1), at(5), LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(1, 0, 4, 1), at(90), LIMIT), Edge::Steady);
        // A packet got out at 91 s, so the earlier wait no longer counts.
        assert_eq!(t.observe(sample(2, 0, 4, 1), at(91), LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(2, 0, 6, 1), at(96), LIMIT), Edge::Steady, "the wait starts again");
        assert_eq!(t.observe(sample(2, 0, 8, 1), at(185), LIMIT), Edge::Steady);
        assert!(matches!(t.observe(sample(2, 0, 10, 1), at(186), LIMIT), Edge::Stalled { .. }));
    }

    #[test]
    fn with_no_session_there_is_nothing_to_judge() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut t = StallTracker::default();
        assert_eq!(t.observe(sample(1, 1, 0, 1), at(0), LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(1, 1, 2, 1), at(5), LIMIT), Edge::Steady);
        // The last light went away with its writes unsent, which is what a light
        // being switched off looks like. It must not read as a stall.
        assert_eq!(t.observe(sample(1, 1, 2, 0), at(10), LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(1, 1, 2, 0), at(1_000), LIMIT), Edge::Steady);
        // A light coming back starts from nothing: the old writes don't count.
        assert_eq!(t.observe(sample(1, 1, 2, 1), at(1_005), LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(1, 1, 2, 1), at(2_000), LIMIT), Edge::Steady);
    }

    #[test]
    fn only_data_from_a_light_ends_a_reported_stall() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        for (tx, rx) in [(10, 6), (11, 6), (0, 0)] {
            let mut t = StallTracker::default();
            assert_eq!(t.observe(sample(10, 5, 0, 1), at(0), LIMIT), Edge::Steady);
            assert_eq!(t.observe(sample(10, 5, 2, 1), at(5), LIMIT), Edge::Steady);
            assert!(matches!(t.observe(sample(10, 5, 4, 1), at(100), LIMIT), Edge::Stalled { .. }));
            assert_eq!(
                t.observe(sample(tx, rx, 4, 1), at(160), LIMIT),
                Edge::Moving { after: Duration::from_secs(60) },
                "tx {tx} rx {rx}: any change in received data counts, including a counter that went backwards"
            );
            assert!(!t.stalled());
        }
    }

    #[test]
    fn packets_going_out_do_not_end_a_reported_stall() {
        // Seen on the rig with the controller kept stalled through recovery: the
        // reset gives it fresh transmit credits, lights retrying their connections
        // spend them, and nothing ever comes back.
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut t = StallTracker::default();
        assert_eq!(t.observe(sample(10, 5, 0, 4), at(0), LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(10, 5, 2, 4), at(5), LIMIT), Edge::Steady);
        assert!(matches!(t.observe(sample(10, 5, 4, 4), at(100), LIMIT), Edge::Stalled { .. }));
        for (i, tx) in (11..40).enumerate() {
            assert_eq!(t.observe(sample(tx, 5, 4, 0), at(105 + i as u64 * 5), LIMIT), Edge::Steady);
            assert!(t.stalled());
        }
    }

    #[test]
    fn a_stall_stays_reported_while_the_recovery_has_dropped_every_light() {
        // The ladder's power-cycle takes the session count to zero. That says
        // nothing about the controller, so it must not read as recovery.
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut t = StallTracker::default();
        assert_eq!(t.observe(sample(10, 5, 0, 4), at(0), LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(10, 5, 2, 4), at(5), LIMIT), Edge::Steady);
        assert!(matches!(t.observe(sample(10, 5, 4, 4), at(100), LIMIT), Edge::Stalled { .. }));
        assert_eq!(t.observe(sample(10, 5, 4, 0), at(105), LIMIT), Edge::Steady);
        assert!(t.stalled());
        assert_eq!(t.observe(sample(10, 6, 4, 1), at(120), LIMIT), Edge::Moving { after: Duration::from_secs(20) });
    }

    #[test]
    fn forgetting_starts_over_from_a_new_baseline() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut t = StallTracker::default();
        assert_eq!(t.observe(sample(10, 5, 0, 1), at(0), LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(10, 5, 2, 1), at(5), LIMIT), Edge::Steady);
        assert!(matches!(t.observe(sample(10, 5, 4, 1), at(100), LIMIT), Edge::Stalled { .. }));
        t.forget();
        assert!(!t.stalled());
        // The first sample after forgetting is a baseline, whatever it says.
        assert_eq!(t.observe(sample(99, 99, 4, 1), at(105), LIMIT), Edge::Steady);
    }

    #[test]
    fn a_clock_running_backwards_saturates() {
        let t0 = Instant::now() + Duration::from_secs(1_000);
        let mut t = StallTracker::default();
        assert_eq!(t.observe(sample(1, 1, 0, 1), t0, LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(1, 1, 2, 1), t0, LIMIT), Edge::Steady);
        assert_eq!(t.observe(sample(1, 1, 4, 1), t0 - Duration::from_secs(500), LIMIT), Edge::Steady);
    }

    /// The ioctl fills whatever bytes we hand it, so a layout that drifted from
    /// the kernel's would report some other field as the packet count, silently.
    /// Offsets worked out from the kernel's struct, confirmed against `hciconfig`
    /// on a real adapter.
    #[cfg(target_os = "linux")]
    #[test]
    fn hci_dev_info_has_the_kernels_layout() {
        use platform::{DevInfo, DevStats};
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<DevInfo>(), 92);
        assert_eq!(offset_of!(DevInfo, flags), 16);
        assert_eq!(offset_of!(DevInfo, features), 21);
        assert_eq!(offset_of!(DevInfo, pkt_type), 32);
        assert_eq!(offset_of!(DevInfo, acl_mtu), 44);
        assert_eq!(offset_of!(DevInfo, stat), 52);
        assert_eq!(offset_of!(DevStats, acl_tx), 16);
        assert_eq!(offset_of!(DevStats, acl_rx), 20);
    }

    /// No adapter has this index. Machines with no Bluetooth in the kernel at
    /// all (a CI runner) refuse the socket instead, which is an error, not a panic.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_absent_adapter_reads_as_absent() {
        match platform::counters(u16::MAX) {
            Ok(found) => assert_eq!(found, None),
            Err(e) => assert!(e.raw_os_error().is_some(), "{e}"),
        }
    }
}
