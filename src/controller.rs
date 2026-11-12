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
//! # A controller that stops hearing (2026-11-11)
//!
//! The stall rule above needs a live session to judge. On 2026-11-11 the
//! replacement adapter (a StarTech AV53C1, the same RTL8761B family) stalled the
//! same way, but this time BlueZ reported the kernel's disconnects, every session
//! ended inside [`STALL_AFTER`], and with no session left there was nothing to
//! judge. The controller then sat for six hours answering every command while
//! hearing nothing: a btmon capture showed zero LE advertising reports in 55 s,
//! including an 8 s active scan, in a house full of BLE devices. Scans started
//! fine and every D-Bus call succeeded, so no other check saw it either; every
//! connect timed out, 2,319 of them.
//!
//! So the kernel's advertising reports are counted too (a raw HCI socket,
//! filtered to LE meta events: nothing the controller is asked to do changes).
//! **No report from any device for [`DEAF_AFTER`] while no light has a session**
//! is a deaf controller, and goes to the same gate. Gating on "no session" makes
//! a false alarm free: the only way a working controller hears nothing for three
//! minutes is every light off with no other BLE device in range, and then a
//! power-cycle drops nothing.
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
/// How long the controller may hear no advertisement from any device, while no
/// light has a session, before it counts as deaf. Every disconnected light keeps
/// a scan going (the kernel's passive scan for a pending connect, and the
/// coordinator's active bursts), and a working controller reports the first
/// advertisement within seconds; three minutes rides out an adapter
/// power-cycle, a bluetoothd restart and the bridge's own restart.
pub const DEAF_AFTER: Duration = Duration::from_secs(180);
/// While deaf, how soon to try again after the ladder ran out or is turned off.
/// Longer than a stall's retry: the one way to be wrong here is a site with
/// every light off and nothing else transmitting, which should not earn a
/// power-cycle every ten minutes for days.
const DEAF_RETRY_AFTER: Duration = Duration::from_secs(1800);

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

/// What one reading of the advertising-report count means. See [`DeafTracker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeafEdge {
    /// Nothing to act on or report.
    Steady,
    /// No advertisement heard, and no light connected, for at least the limit.
    Deaf { silent_for: Duration },
    /// An advertisement arrived after deafness was reported.
    Hearing { after: Duration },
}

/// Turns advertising-report counts into edges. Pure, like [`StallTracker`].
///
/// Only a report ends a reported deafness, never a session appearing: a session
/// can be a stale platform record (see `recovery.rs`), and a light that really
/// reconnected was heard advertising first.
#[derive(Debug, Default)]
pub struct DeafTracker {
    last: Option<u64>,
    silent_since: Option<Instant>,
    deaf_since: Option<Instant>,
}

impl DeafTracker {
    /// `reports` is the cumulative count of advertising reports; only compared
    /// for equality. `sessions` is how many lights have a live session.
    pub fn observe(&mut self, reports: u64, sessions: usize, now: Instant, limit: Duration) -> DeafEdge {
        let heard = self.last.replace(reports).is_some_and(|last| last != reports);
        if heard {
            self.silent_since = None;
            return match self.deaf_since.take() {
                Some(since) => DeafEdge::Hearing { after: now.saturating_duration_since(since) },
                None => DeafEdge::Steady,
            };
        }
        if self.deaf_since.is_some() {
            return DeafEdge::Steady;
        }
        // A connected light is not advertising, and the stall check judges its
        // link; and with a light connected, a power-cycle would cost something.
        if sessions > 0 {
            self.silent_since = None;
            return DeafEdge::Steady;
        }
        let since = *self.silent_since.get_or_insert(now);
        let silent_for = now.saturating_duration_since(since);
        if silent_for < limit {
            return DeafEdge::Steady;
        }
        self.deaf_since = Some(now);
        DeafEdge::Deaf { silent_for }
    }

    /// Whether deafness has been reported and nothing has been heard since.
    pub fn deaf(&self) -> bool {
        self.deaf_since.is_some()
    }

    /// Start over: the next reading is a new baseline.
    pub fn forget(&mut self) {
        *self = Self::default();
    }
}

/// HCI packet type of an event, the first byte of everything a raw HCI socket
/// delivers.
const HCI_EVENT_PKT: u8 = 0x04;
/// The LE Meta event, which carries every LE advertising report.
const EVT_LE_META: u8 = 0x3E;
/// LE Advertising Report, LE Directed Advertising Report, LE Extended
/// Advertising Report (Core spec Vol 4 Part E 7.7.65.2 / .11 / .13). The
/// StarTech and the UB500 both use the extended form.
const ADV_SUBEVENTS: [u8; 3] = [0x02, 0x0B, 0x0D];

/// Whether one packet read from a raw HCI socket is an advertising report.
/// Pure: `[type, event, length, subevent, ...]`.
pub fn is_adv_report(packet: &[u8]) -> bool {
    matches!(packet, [HCI_EVENT_PKT, EVT_LE_META, _, sub, ..] if ADV_SUBEVENTS.contains(sub))
}

/// The kernel's device index for a BlueZ adapter id such as `hci0`.
pub fn hci_index(adapter_id: &str) -> Option<u16> {
    adapter_id.strip_prefix("hci")?.parse().ok()
}

/// Watch adapter `index` for the life of the bridge run, and hand a stalled or
/// deaf controller to the recovery gate. `stalled` mirrors the verdict for the
/// service status line.
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
    let mut deaf = DeafTracker::default();
    let mut next_attempt = Instant::now();
    let mut next_deaf_attempt = Instant::now();
    let mut unreadable = false;
    // Counts the controller's advertising reports from here on; stops when this
    // future is dropped (the bridge run ends).
    let reports = platform::AdvReports::listen(adapter.clone(), index);
    loop {
        tick.tick().await;
        let counters = match platform::counters(index) {
            Ok(Some(counters)) => {
                if unreadable {
                    unreadable = false;
                    info!(adapter = %adapter, "the adapter's packet counters can be read again; the controller stall check is back on");
                }
                Some(counters)
            }
            // The adapter has gone. The stack watchdog's presence check handles
            // that; this check has nothing to judge until it is back.
            Ok(None) => {
                debug!(adapter = %adapter, "the adapter is not there to read its packet counters");
                None
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
                None
            }
        };
        let Some(counters) = counters else {
            tracker.forget();
            deaf.forget();
            stalled.store(false, Ordering::Relaxed);
            continue;
        };
        let now = Instant::now();
        let sample = Sample { counters, writes: ble::writes_accepted(), sessions: scan.connected() };
        trace!(
            adapter = %adapter, acl_tx = counters.acl_tx, acl_rx = counters.acl_rx,
            writes = sample.writes, sessions = sample.sessions,
            adv_reports = reports.count(), "controller counters"
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
        // Without a working listener the count never moves, which would read as
        // deafness; `listening` is false until the socket is open and verified.
        if reports.listening() {
            match deaf.observe(reports.count(), sample.sessions, now, DEAF_AFTER) {
                DeafEdge::Steady => {}
                DeafEdge::Deaf { silent_for } => error!(
                    adapter = %adapter, silent_secs = silent_for.as_secs(),
                    "the Bluetooth controller has stopped hearing: no advertisement from any \
                     device has reached it while no light is connected, so no light can be \
                     found or reconnected"
                ),
                DeafEdge::Hearing { after } => info!(
                    adapter = %adapter, after_secs = after.as_secs(),
                    "the Bluetooth controller is hearing advertisements again"
                ),
            }
        } else {
            deaf.forget();
        }
        stalled.store(tracker.stalled() || deaf.deaf(), Ordering::Relaxed);

        if tracker.stalled() && now >= next_attempt {
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
        }

        let now = Instant::now();
        if deaf.deaf() && now >= next_deaf_attempt {
            // Still bad = nothing heard since this decision, and no light back.
            let heard_before = reports.count();
            let (r, s) = (&reports, &scan);
            let outcome = recovery
                .recover(Problem::ControllerDeaf, "controller", move || async move {
                    r.count() == heard_before && s.connected() == 0
                })
                .await;
            let now = Instant::now();
            match outcome {
                Outcome::AlreadyClear => {
                    info!(adapter = %adapter, "the Bluetooth controller heard again before recovery ran");
                    deaf.forget();
                }
                Outcome::Cleared(step) => {
                    info!(adapter = %adapter, step = %step, "the Bluetooth controller is hearing again");
                    deaf.forget();
                }
                Outcome::StillStale => {
                    next_deaf_attempt = now + DEAF_RETRY_AFTER;
                    error!(
                        adapter = %adapter, retry_secs = DEAF_RETRY_AFTER.as_secs(),
                        "the controller still hears nothing after every permitted recovery step \
                         (it is still deaf, or every light is off and nothing else is transmitting) \
                         — check `dmesg`; replugging the adapter may be the only fix"
                    );
                }
                Outcome::RateLimited => {
                    next_deaf_attempt = now + RATE_LIMITED_RETRY;
                    debug!(adapter = %adapter, "deaf controller recovery is rate-limited; will ask again");
                }
                Outcome::Disabled => {
                    next_deaf_attempt = now + DEAF_RETRY_AFTER;
                    warn!(
                        adapter = %adapter,
                        "the Bluetooth controller hears nothing, and stale_link_recovery = \"off\" — \
                         not touching the adapter; no light can reconnect until it is reset by hand \
                         (`btmgmt power off`, `btmgmt power on`, `systemctl restart bluetooth`)"
                    );
                }
            }
        }
        stalled.store(tracker.stalled() || deaf.deaf(), Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io;
    use std::mem::size_of;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::unix::AsyncFd;
    use tokio::task::JoinHandle;
    use tracing::{debug, info, warn};

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

    /// `SOL_HCI` and `HCI_FILTER`, from `include/net/bluetooth/hci_sock.h`.
    const SOL_HCI: libc::c_int = 0;
    const HCI_FILTER: libc::c_int = 2;
    /// `HCI_CHANNEL_RAW`: the channel hcidump reads; it sees every event the
    /// controller sends, and listening on it asks nothing of the controller.
    const HCI_CHANNEL_RAW: u16 = 0;
    /// How soon to try again after the socket could not be opened or closed.
    const REOPEN_AFTER: Duration = Duration::from_secs(5);

    /// `struct sockaddr_hci`.
    #[repr(C)]
    pub(super) struct SockAddrHci {
        pub family: libc::sa_family_t,
        pub dev: u16,
        pub channel: u16,
    }

    /// `struct hci_ufilter`, the user-space form `HCI_FILTER` takes and gives.
    #[repr(C)]
    #[derive(Debug, Default)]
    pub(super) struct HciFilter {
        pub type_mask: u32,
        pub event_mask: [u32; 2],
        pub opcode: u16,
    }

    /// Events only, and of those only LE Meta.
    pub(super) fn le_meta_filter() -> HciFilter {
        let ev = super::EVT_LE_META as u32;
        let mut event_mask = [0u32; 2];
        event_mask[(ev >> 5) as usize] = 1 << (ev & 31);
        HciFilter { type_mask: 1 << super::HCI_EVENT_PKT, event_mask, opcode: 0 }
    }

    /// Whether `f` still lets LE Meta events through. The kernel silently masks
    /// a filter down to a "safe" set for a socket opened without CAP_NET_RAW,
    /// and LE Meta is not in that set: such a socket would read nothing, and a
    /// working controller would look deaf.
    pub(super) fn passes_le_meta(f: &HciFilter) -> bool {
        let want = le_meta_filter();
        f.type_mask & want.type_mask != 0 && f.event_mask[1] & want.event_mask[1] != 0
    }

    /// `HCI_DEV_NONE`: bound to this, a raw socket hears EVERY adapter.
    const HCI_DEV_NONE: u16 = 0xFFFF;

    fn open(index: u16) -> io::Result<OwnedFd> {
        if index == HCI_DEV_NONE {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        // SAFETY: plain socket(2) with constant arguments; the result is checked.
        let fd = unsafe {
            libc::socket(
                libc::AF_BLUETOOTH,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                BTPROTO_HCI,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just created, checked, and is owned by nothing else.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        let addr = SockAddrHci {
            family: libc::AF_BLUETOOTH as libc::sa_family_t,
            dev: index,
            channel: HCI_CHANNEL_RAW,
        };
        // SAFETY: `addr` is a live `struct sockaddr_hci` and the length is its size.
        let rc = unsafe {
            libc::bind(
                sock.as_raw_fd(),
                &addr as *const SockAddrHci as *const libc::sockaddr,
                size_of::<SockAddrHci>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let want = le_meta_filter();
        // SAFETY: `want` is a live `struct hci_ufilter` and the length is its size.
        let rc = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                SOL_HCI,
                HCI_FILTER,
                &want as *const HciFilter as *const libc::c_void,
                size_of::<HciFilter>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut got = HciFilter::default();
        let mut len = size_of::<HciFilter>() as libc::socklen_t;
        // SAFETY: `got` is a live, exclusively borrowed `struct hci_ufilter` and
        // `len` holds its size, which is what HCI_FILTER fills.
        let rc = unsafe {
            libc::getsockopt(
                sock.as_raw_fd(),
                SOL_HCI,
                HCI_FILTER,
                &mut got as *mut HciFilter as *mut libc::c_void,
                &mut len,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if !passes_le_meta(&got) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the kernel filtered LE events out of the raw HCI socket (the process lacks CAP_NET_RAW)",
            ));
        }
        Ok(sock)
    }

    /// Counts the controller's advertising reports in a task of its own, which
    /// ends when this is dropped.
    pub struct AdvReports {
        count: Arc<AtomicU64>,
        listening: Arc<AtomicBool>,
        task: JoinHandle<()>,
    }

    impl AdvReports {
        pub fn listen(adapter: String, index: u16) -> Self {
            let count = Arc::new(AtomicU64::new(0));
            let listening = Arc::new(AtomicBool::new(false));
            let task = tokio::spawn(run(adapter, index, count.clone(), listening.clone()));
            Self { count, listening, task }
        }

        /// Reports seen so far. Only ever compare it for equality.
        pub fn count(&self) -> u64 {
            self.count.load(Ordering::Relaxed)
        }

        /// Whether the socket is open and verified to pass advertising reports.
        pub fn listening(&self) -> bool {
            self.listening.load(Ordering::Relaxed)
        }
    }

    impl Drop for AdvReports {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn run(adapter: String, index: u16, count: Arc<AtomicU64>, listening: Arc<AtomicBool>) {
        let mut complained = false;
        loop {
            match open(index).and_then(AsyncFd::new) {
                Ok(fd) => {
                    if complained {
                        complained = false;
                        info!(adapter = %adapter, "counting the controller's advertising reports again; the deaf controller check is back on");
                    }
                    listening.store(true, Ordering::Relaxed);
                    let e = read_reports(&fd, &count).await;
                    listening.store(false, Ordering::Relaxed);
                    debug!(adapter = %adapter, error = %e, "the advertising report socket closed; reopening");
                }
                // The adapter has gone; the presence check deals with that.
                Err(e) if e.raw_os_error() == Some(libc::ENODEV) => {
                    debug!(adapter = %adapter, "the adapter is not there to count its advertising reports");
                }
                Err(e) => {
                    if !complained {
                        complained = true;
                        warn!(
                            adapter = %adapter, error = %e,
                            "cannot count the controller's advertising reports — a Bluetooth \
                             controller that stops hearing will not be noticed until this clears"
                        );
                    }
                }
            }
            tokio::time::sleep(REOPEN_AFTER).await;
        }
    }

    /// Count reports until the socket fails; returns why it did.
    async fn read_reports(fd: &AsyncFd<OwnedFd>, count: &AtomicU64) -> io::Error {
        // An HCI event is at most 2 + 255 bytes after its type byte.
        let mut buf = [0u8; 260];
        loop {
            let mut guard = match fd.readable().await {
                Ok(guard) => guard,
                Err(e) => return e,
            };
            let read = guard.try_io(|inner| {
                // SAFETY: `buf` is a live, exclusively borrowed buffer of the length given.
                let n = unsafe { libc::read(inner.get_ref().as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match read {
                Ok(Ok(0)) => return io::Error::new(io::ErrorKind::UnexpectedEof, "the kernel closed the socket"),
                Ok(Ok(n)) => {
                    if super::is_adv_report(&buf[..n]) {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return e,
                // Not ready after all; wait for the next readiness.
                Err(_would_block) => {}
            }
        }
    }

    #[cfg(test)]
    pub(super) fn open_for_test(index: u16) -> io::Result<OwnedFd> {
        open(index)
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::io;

    use super::Counters;

    pub fn counters(_index: u16) -> io::Result<Option<Counters>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "HCI counters exist only on Linux"))
    }

    /// There is no raw HCI socket to read: never listening, so never deaf.
    pub struct AdvReports;

    impl AdvReports {
        pub fn listen(_adapter: String, _index: u16) -> Self {
            Self
        }

        pub fn count(&self) -> u64 {
            0
        }

        pub fn listening(&self) -> bool {
            false
        }
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

    const DEAF: Duration = DEAF_AFTER;

    #[test]
    fn the_2026_11_11_deaf_controller_is_reported_at_the_limit_and_not_before() {
        // Every light dropped, the controller answered every command, and not one
        // advertising report arrived for six hours.
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut d = DeafTracker::default();
        for s in (0..DEAF.as_secs()).step_by(5) {
            assert_eq!(d.observe(4_242, 0, at(s), DEAF), DeafEdge::Steady, "at {s} s");
        }
        assert_eq!(d.observe(4_242, 0, at(DEAF.as_secs()), DEAF), DeafEdge::Deaf { silent_for: DEAF });
        assert!(d.deaf());
        // Reported once; it stays deaf while nothing is heard.
        for s in (185..20_000).step_by(5) {
            assert_eq!(d.observe(4_242, 0, at(s), DEAF), DeafEdge::Steady);
            assert!(d.deaf());
        }
        // One report ends it.
        assert_eq!(d.observe(4_243, 0, at(20_000), DEAF), DeafEdge::Hearing { after: Duration::from_secs(20_000 - 180) });
        assert!(!d.deaf());
    }

    #[test]
    fn a_working_controller_with_every_light_missing_is_never_deaf() {
        // Lights off or out of range, but the house is full of other devices:
        // reports keep arriving, however long no light connects.
        let t0 = Instant::now();
        let mut d = DeafTracker::default();
        for tick in 0..20_000u64 {
            let reports = tick / 3; // one now and then is enough
            assert_eq!(d.observe(reports, 0, t0 + Duration::from_secs(tick * 5), DEAF), DeafEdge::Steady, "tick {tick}");
        }
    }

    #[test]
    fn silence_with_a_light_connected_is_not_deafness() {
        // Every light connected = no light advertising and no scan running, so
        // nothing is heard, rightly. And with a light connected a power-cycle
        // would cost something; the stall check judges that link instead.
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut d = DeafTracker::default();
        for s in (0..100_000).step_by(5) {
            assert_eq!(d.observe(7, 3, at(s), DEAF), DeafEdge::Steady);
        }
        // The last session ends: the clock starts THEN, not at the first sample.
        assert_eq!(d.observe(7, 0, at(100_000), DEAF), DeafEdge::Steady);
        assert_eq!(d.observe(7, 0, at(100_000 + DEAF.as_secs() - 1), DEAF), DeafEdge::Steady);
        assert!(matches!(d.observe(7, 0, at(100_000 + DEAF.as_secs()), DEAF), DeafEdge::Deaf { .. }));
    }

    #[test]
    fn a_session_alone_does_not_end_reported_deafness() {
        // A session can be a stale platform record; a light that really came
        // back was heard advertising first.
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut d = DeafTracker::default();
        assert_eq!(d.observe(1, 0, at(0), DEAF), DeafEdge::Steady);
        assert!(matches!(d.observe(1, 0, at(DEAF.as_secs()), DEAF), DeafEdge::Deaf { .. }));
        assert_eq!(d.observe(1, 2, at(400), DEAF), DeafEdge::Steady);
        assert!(d.deaf());
    }

    #[test]
    fn a_report_restarts_the_silence_clock() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut d = DeafTracker::default();
        assert_eq!(d.observe(1, 0, at(0), DEAF), DeafEdge::Steady);
        assert_eq!(d.observe(1, 0, at(170), DEAF), DeafEdge::Steady);
        assert_eq!(d.observe(2, 0, at(175), DEAF), DeafEdge::Steady, "heard just in time");
        assert_eq!(d.observe(2, 0, at(180), DEAF), DeafEdge::Steady, "the clock starts again here");
        assert_eq!(d.observe(2, 0, at(359), DEAF), DeafEdge::Steady);
        assert!(matches!(d.observe(2, 0, at(360), DEAF), DeafEdge::Deaf { .. }));
    }

    #[test]
    fn forgetting_deafness_starts_from_a_new_baseline() {
        // After a recovery step cleared it, or while the listener is down.
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut d = DeafTracker::default();
        assert_eq!(d.observe(1, 0, at(0), DEAF), DeafEdge::Steady);
        assert!(matches!(d.observe(1, 0, at(DEAF.as_secs()), DEAF), DeafEdge::Deaf { .. }));
        d.forget();
        assert!(!d.deaf());
        // A count that changed across the gap is a baseline, not "hearing".
        assert_eq!(d.observe(9, 0, at(500), DEAF), DeafEdge::Steady);
        assert_eq!(d.observe(9, 0, at(500 + DEAF.as_secs() - 5), DEAF), DeafEdge::Steady);
    }

    #[test]
    fn a_deaf_clock_running_backwards_saturates() {
        let t0 = Instant::now() + Duration::from_secs(1_000);
        let mut d = DeafTracker::default();
        assert_eq!(d.observe(1, 0, t0, DEAF), DeafEdge::Steady);
        assert_eq!(d.observe(1, 0, t0 - Duration::from_secs(500), DEAF), DeafEdge::Steady);
    }

    #[test]
    fn only_advertising_reports_count() {
        // Layouts as btmon decodes them on the rig: [type, event, len, subevent, ...].
        let extended_adv = [0x04, 0x3E, 0x2A, 0x0D, 0x01, 0x13, 0x00];
        let legacy_adv = [0x04, 0x3E, 0x1A, 0x02, 0x01, 0x00];
        let direct_adv = [0x04, 0x3E, 0x12, 0x0B, 0x01];
        for p in [&extended_adv[..], &legacy_adv, &direct_adv] {
            assert!(is_adv_report(p), "{p:02x?}");
        }
        let not_reports: [&[u8]; 7] = [
            &[0x04, 0x0E, 0x04, 0x02, 0x12, 0x20, 0x12], // Command Complete (Remove From Accept List)
            &[0x04, 0x3E, 0x1F, 0x0A, 0x00],             // LE Enhanced Connection Complete
            &[0x04, 0x3E, 0x0B, 0x03, 0x00],             // LE Connection Update Complete
            &[0x04, 0x05, 0x04, 0x00],                   // Disconnection Complete
            &[0x02, 0x3E, 0x0D, 0x0D],                   // ACL data that happens to look alike
            &[0x04, 0x3E, 0x0D],                         // truncated before the subevent
            &[],
        ];
        for p in not_reports {
            assert!(!is_adv_report(p), "{p:02x?}");
        }
    }

    /// The setsockopt and bind take whatever bytes they are given, so a layout
    /// that drifted would set some other filter, silently.
    #[cfg(target_os = "linux")]
    #[test]
    fn hci_socket_structs_have_the_kernels_layout() {
        use platform::{HciFilter, SockAddrHci};
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<SockAddrHci>(), 6);
        assert_eq!(size_of::<HciFilter>(), 16);
        assert_eq!(offset_of!(HciFilter, event_mask), 4);
        assert_eq!(offset_of!(HciFilter, opcode), 12);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_filter_asks_for_le_meta_events_and_notices_when_they_are_masked_out() {
        let f = platform::le_meta_filter();
        assert_eq!(f.type_mask, 1 << 4, "events only");
        assert_eq!(f.event_mask, [0, 1 << 30], "0x3E is bit 30 of the second word");
        assert!(platform::passes_le_meta(&f));
        // What a socket without CAP_NET_RAW gets back: LE Meta masked away.
        let masked = platform::HciFilter { event_mask: [0, 0], ..platform::le_meta_filter() };
        assert!(!platform::passes_le_meta(&masked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_advertising_socket_opens_for_an_absent_adapter() {
        assert!(platform::open_for_test(0xFFFE).is_err());
        // 0xFFFF is HCI_DEV_NONE, which the kernel binds to every adapter at
        // once (found by this test's first version): it must never be used.
        let e = platform::open_for_test(0xFFFF).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENODEV));
    }
}
