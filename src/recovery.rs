//! Recovery from a STALE platform device record — a light the OS Bluetooth
//! stack reports as connected although its link is gone.
//!
//! Found on the test rig on 2026-09-02 after the whole fleet had been dark for
//! four days. What happens, measured on BlueZ 5.82 + Linux 6.12 (see the
//! project notes for the full trace):
//!
//! - A marginal fixture's LE link drops in a way that leaves bluetoothd's
//!   device record saying `Connected: yes` while the kernel holds no connection
//!   for it (a bluetoothd state desync; the flapping −90 dBm TL21C produced it).
//! - From then on `Device1.Connect()` on that record is a **silent no-op**
//!   ("Connection successful" in 38 ms, no link created), so the actor's
//!   connect succeeds instantly and its first write fails.
//! - `Device1.Disconnect()` **never returns**: bluetoothd asks the kernel to
//!   disconnect, kernels ≥ 6.11 answer `-ENOTCONN`, which the management API
//!   reports as status `0x0e` "Disconnected", and bluetoothd 5.82 treats that
//!   as a hard failure (`Failed to disconnect device: Disconnected (0x0e)`) and
//!   skips the cleanup that would have replied. Older kernels reported
//!   "Not Connected" (0x02) here, which bluetoothd handles as "already
//!   disconnected" — so the record used to heal itself on the first attempt.
//! - `Adapter1.RemoveDevice()` on such a record hangs the same way (it routes
//!   a "connected" device through the same disconnect path).
//! - Every un-answered D-Bus method call occupies one of the **128 pending
//!   replies** dbus-daemon allows per connection, and the system bus ships with
//!   `reply_timeout = -1` ("never"), so those slots are never returned. After
//!   128 hung disconnects (one per reconnect cycle, ~35 s apart — 75 minutes)
//!   the bus refused EVERY call from the bridge's connection, including the
//!   peripheral listing every other light depends on, and the whole fleet went
//!   dark while the process stayed up.
//!
//! The actor now detects the hang ([`crate::ble::disconnect_outcome`]) after
//! [`crate::ble::DISCONNECT_TIMEOUT`] instead of the library's 30 s, never
//! issues a second disconnect to a record it knows to be stale (one leaked
//! slot per episode, not one per cycle), and asks this module to clear the
//! record. Nothing that goes through the device object can clear it, so the
//! ladder works on the adapter and the daemon instead:
//!
//! 1. **Power-cycle the adapter** (`Adapter1.Powered` off → on). bluetoothd
//!    walks its connection list on power-off and marks those devices
//!    disconnected. Drops every live link for a few seconds; the actors
//!    reconnect.
//! 2. **Restart bluetoothd** (`systemctl restart bluetooth`, asked over D-Bus).
//!    Rebuilds the daemon's state from scratch; the bridge already survives
//!    this (hardware-tested 2026-08-25: every light back within 16 s).
//!
//! Each step is fleet-wide, so a shared gate serialises episodes and
//! rate-limits the steps ([`ADAPTER_CYCLE_MIN_GAP`], [`DAEMON_RESTART_MIN_GAP`]).
//! `[ble] stale_link_recovery` selects how far the ladder may go; with it off,
//! the actor only parks the light (no leak, loud log) until the record clears
//! by other means. Both platform steps exist only on Linux/BlueZ — the only
//! backend with this failure — and report `Unsupported` elsewhere.

use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// How far the recovery ladder may go — `[ble] stale_link_recovery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryMode {
    /// Power-cycle the adapter; if the record is still stale, restart bluetoothd.
    Auto,
    /// Power-cycle the adapter only.
    Adapter,
    /// Detect and park only — never touch the adapter or the daemon.
    Off,
}

impl RecoveryMode {
    pub const NAMES: [&'static str; 3] = ["auto", "adapter", "off"];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "adapter" => Some(Self::Adapter),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Adapter => "adapter",
            Self::Off => "off",
        }
    }
}

impl std::fmt::Display for RecoveryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One rung of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    CycleAdapter,
    RestartDaemon,
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Step::CycleAdapter => "adapter power-cycle",
            Step::RestartDaemon => "bluetoothd restart",
        })
    }
}

/// Minimum spacing between adapter power-cycles, fleet-wide. A cycle drops
/// every live link, so two stale records found seconds apart must share one.
pub const ADAPTER_CYCLE_MIN_GAP: Duration = Duration::from_secs(120);
/// Minimum spacing between daemon restarts, fleet-wide.
pub const DAEMON_RESTART_MIN_GAP: Duration = Duration::from_secs(600);
/// How long a step is given to take effect before the record is re-checked.
const SETTLE: Duration = Duration::from_secs(4);

/// When each step last ran (fleet-wide).
#[derive(Debug, Default, Clone, Copy)]
pub struct History {
    pub last_adapter_cycle: Option<Instant>,
    pub last_daemon_restart: Option<Instant>,
}

/// Whether a rate-limited step is due: never run, or `gap` has elapsed.
/// Saturating, so a clock that appears to run backwards is just "not due".
fn due(last: Option<Instant>, now: Instant, gap: Duration) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= gap)
}

/// The steps one stale-link episode may take, in order, given the configured
/// mode and how recently each step ran. Pure, so the ladder is testable without
/// an adapter: `Off` yields nothing, `Adapter` at most the power-cycle, `Auto`
/// the power-cycle then the daemon restart — each only if its gap has passed.
pub fn plan(mode: RecoveryMode, history: History, now: Instant) -> Vec<Step> {
    let mut steps = Vec::new();
    if mode == RecoveryMode::Off {
        return steps;
    }
    if due(history.last_adapter_cycle, now, ADAPTER_CYCLE_MIN_GAP) {
        steps.push(Step::CycleAdapter);
    }
    if mode == RecoveryMode::Auto && due(history.last_daemon_restart, now, DAEMON_RESTART_MIN_GAP) {
        steps.push(Step::RestartDaemon);
    }
    steps
}

/// What one recovery request achieved.
#[derive(Debug)]
pub enum Outcome {
    /// The record had already cleared (typically by another light's episode)
    /// before any step ran.
    AlreadyClear,
    /// A step cleared it.
    Cleared(Step),
    /// Every permitted step ran and the record is still stale.
    StillStale,
    /// Nothing was due: every permitted step ran too recently.
    RateLimited,
    /// `stale_link_recovery = "off"`.
    Disabled,
}

/// The fleet-wide recovery gate. One per bridge; every light actor holds a
/// clone of the `Arc`.
pub struct StaleLinkRecovery {
    mode: RecoveryMode,
    /// The adapter's platform id (`hci0`), for the power-cycle step.
    adapter_id: String,
    state: Mutex<History>,
}

impl StaleLinkRecovery {
    pub fn new(mode: RecoveryMode, adapter_id: String) -> Self {
        Self { mode, adapter_id, state: Mutex::new(History::default()) }
    }

    pub fn mode(&self) -> RecoveryMode {
        self.mode
    }

    /// Try to clear a stale record for `light`. `still_stale` re-reads the
    /// platform's view of the light (true = still reported connected); it is
    /// consulted before anything runs and after every step, so no step runs
    /// for a record that has already cleared.
    ///
    /// Episodes are serialised: a second light arriving while a step is in
    /// flight waits, then re-checks — the first light's power-cycle usually
    /// cleared it too.
    pub async fn recover<F, Fut>(&self, light: &str, still_stale: F) -> Outcome
    where
        F: Fn() -> Fut,
        Fut: Future<Output = bool>,
    {
        let mut history = self.state.lock().await;
        if !still_stale().await {
            return Outcome::AlreadyClear;
        }
        if self.mode == RecoveryMode::Off {
            return Outcome::Disabled;
        }
        let steps = plan(self.mode, *history, Instant::now());
        if steps.is_empty() {
            return Outcome::RateLimited;
        }
        for step in steps {
            let now = Instant::now();
            let attempted = match step {
                Step::CycleAdapter => {
                    history.last_adapter_cycle = Some(now);
                    info!(
                        light = %light, adapter = %self.adapter_id,
                        "stale link recovery: power-cycling the Bluetooth adapter \
                         (every light will drop and reconnect)"
                    );
                    platform::power_cycle_adapter(&self.adapter_id).await
                }
                Step::RestartDaemon => {
                    history.last_daemon_restart = Some(now);
                    warn!(
                        light = %light,
                        "stale link recovery: restarting bluetoothd (systemd bluetooth.service) \
                         — the adapter power-cycle did not clear the record"
                    );
                    platform::restart_bluetoothd().await
                }
            };
            if let Err(e) = attempted {
                warn!(light = %light, step = %step, error = %format!("{e:#}"), "stale link recovery step failed");
                continue;
            }
            tokio::time::sleep(SETTLE).await;
            if !still_stale().await {
                return Outcome::Cleared(step);
            }
            warn!(light = %light, step = %step, "stale device record survived this step");
        }
        Outcome::StillStale
    }
}

/// The platform's object path for a BlueZ adapter id such as `hci0`. Only the
/// characters D-Bus allows in a path element are accepted, so an unexpected
/// id can't be turned into a path that names something else.
pub fn adapter_object_path(adapter_id: &str) -> Result<String> {
    if adapter_id.is_empty() || !adapter_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        anyhow::bail!("unexpected adapter id {adapter_id:?} (expected something like hci0)");
    }
    Ok(format!("/org/bluez/{adapter_id}"))
}

#[cfg(target_os = "linux")]
mod platform {
    //! The two platform steps, spoken over the system bus on a connection of
    //! their own (opened for the call, closed after it) so a hung reply here
    //! can never occupy a slot on the bridge's main BLE connection.

    use std::time::Duration;

    use anyhow::{bail, Context, Result};
    use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
    use dbus::blocking::Connection;

    /// Generous: a reply to either call normally arrives within a second.
    const CALL_TIMEOUT: Duration = Duration::from_secs(15);
    /// Between power-off and power-on. Powering straight back on while the
    /// kernel is still tearing the links down has been seen to time out on the
    /// rig's Realtek dongle (reported as "Authentication Failed" — bluetoothd's
    /// rendering of the kernel's -ETIMEDOUT) during the very first live run of
    /// this ladder; in isolation the same cycle takes under half a second.
    const POWER_OFF_SETTLE: Duration = Duration::from_secs(3);
    /// Before the one retry of power-on.
    const POWER_ON_RETRY: Duration = Duration::from_secs(5);

    pub async fn power_cycle_adapter(adapter_id: &str) -> Result<()> {
        let path = super::adapter_object_path(adapter_id)?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = Connection::new_system().context("connecting to the system D-Bus")?;
            let proxy = conn.with_proxy("org.bluez", path.as_str(), CALL_TIMEOUT);
            proxy
                .set("org.bluez.Adapter1", "Powered", false)
                .context("powering the adapter off")?;
            std::thread::sleep(POWER_OFF_SETTLE);
            let mut powered_on = Ok(());
            for attempt in 1..=2 {
                powered_on = proxy
                    .set("org.bluez.Adapter1", "Powered", true)
                    .context("powering the adapter on");
                if powered_on.is_ok() {
                    break;
                }
                if attempt == 1 {
                    std::thread::sleep(POWER_ON_RETRY);
                }
            }
            powered_on?;
            // Never leave the adapter off on a "success": read the state back.
            let powered: bool = proxy
                .get("org.bluez.Adapter1", "Powered")
                .context("reading the adapter's Powered state back")?;
            if !powered {
                bail!("the adapter still reports Powered = false after the cycle");
            }
            Ok(())
        })
        .await
        .context("adapter power-cycle task")?
    }

    pub async fn restart_bluetoothd() -> Result<()> {
        tokio::task::spawn_blocking(|| -> Result<()> {
            let conn = Connection::new_system().context("connecting to the system D-Bus")?;
            let proxy =
                conn.with_proxy("org.freedesktop.systemd1", "/org/freedesktop/systemd1", CALL_TIMEOUT);
            let (_job,): (dbus::Path,) = proxy
                .method_call(
                    "org.freedesktop.systemd1.Manager",
                    "RestartUnit",
                    ("bluetooth.service", "replace"),
                )
                .context("asking systemd to restart bluetooth.service")?;
            Ok(())
        })
        .await
        .context("bluetoothd restart task")?
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use anyhow::{bail, Result};

    pub async fn power_cycle_adapter(_adapter_id: &str) -> Result<()> {
        bail!("adapter power-cycle is only implemented for BlueZ (Linux)")
    }

    pub async fn restart_bluetoothd() -> Result<()> {
        bail!("a Bluetooth daemon restart is only implemented for BlueZ (Linux)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_mode_parses_case_insensitively_and_rejects_the_rest() {
        assert_eq!(RecoveryMode::parse("auto"), Some(RecoveryMode::Auto));
        assert_eq!(RecoveryMode::parse(" Adapter "), Some(RecoveryMode::Adapter));
        assert_eq!(RecoveryMode::parse("OFF"), Some(RecoveryMode::Off));
        assert_eq!(RecoveryMode::parse("none"), None);
        assert_eq!(RecoveryMode::parse(""), None);
        for name in RecoveryMode::NAMES {
            assert_eq!(RecoveryMode::parse(name).unwrap().as_str(), name);
        }
    }

    #[test]
    fn ladder_follows_the_mode_and_the_rate_limits() {
        use Step::*;
        let t0 = Instant::now();
        let fresh = History::default();

        // Off never touches anything, whatever the history.
        assert!(plan(RecoveryMode::Off, fresh, t0).is_empty());
        // Adapter-only stops at the power-cycle.
        assert_eq!(plan(RecoveryMode::Adapter, fresh, t0), vec![CycleAdapter]);
        // Auto escalates to the daemon restart if the cycle doesn't clear it.
        assert_eq!(plan(RecoveryMode::Auto, fresh, t0), vec![CycleAdapter, RestartDaemon]);

        // A cycle that just ran is not repeated — a second stale light found
        // seconds later goes straight to the next rung (Auto) or waits (Adapter).
        let just_cycled = History { last_adapter_cycle: Some(t0), last_daemon_restart: None };
        assert_eq!(plan(RecoveryMode::Auto, just_cycled, t0 + Duration::from_secs(30)), vec![RestartDaemon]);
        assert!(plan(RecoveryMode::Adapter, just_cycled, t0 + Duration::from_secs(30)).is_empty());
        // ...and is allowed again once its gap has passed (inclusive boundary).
        assert_eq!(
            plan(RecoveryMode::Adapter, just_cycled, t0 + ADAPTER_CYCLE_MIN_GAP),
            vec![CycleAdapter]
        );

        // Both rungs used recently: nothing to do until the gaps pass.
        let both = History { last_adapter_cycle: Some(t0), last_daemon_restart: Some(t0) };
        assert!(plan(RecoveryMode::Auto, both, t0 + Duration::from_secs(60)).is_empty());
        assert_eq!(plan(RecoveryMode::Auto, both, t0 + DAEMON_RESTART_MIN_GAP), vec![CycleAdapter, RestartDaemon]);

        // A clock that appears to run backwards saturates to "not due".
        assert!(plan(RecoveryMode::Auto, both, t0 - Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn adapter_object_path_accepts_hci_ids_only() {
        assert_eq!(adapter_object_path("hci0").unwrap(), "/org/bluez/hci0");
        assert_eq!(adapter_object_path("hci12").unwrap(), "/org/bluez/hci12");
        for bad in ["", "hci0/../hci1", "hci 0", "hci0 (usb:v1D6B)", "../"] {
            assert!(adapter_object_path(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[tokio::test]
    async fn recover_checks_the_record_before_and_after_each_step() {
        use std::sync::atomic::{AtomicU32, Ordering};
        // Already clear: no step runs, nothing is recorded.
        let gate = StaleLinkRecovery::new(RecoveryMode::Auto, "hci0".into());
        let checks = AtomicU32::new(0);
        let outcome = gate
            .recover("t", || {
                checks.fetch_add(1, Ordering::SeqCst);
                async { false }
            })
            .await;
        assert!(matches!(outcome, Outcome::AlreadyClear));
        assert_eq!(checks.load(Ordering::SeqCst), 1);
        assert!(gate.state.lock().await.last_adapter_cycle.is_none());

        // Off: parks without touching anything, even though the record is stale.
        let gate = StaleLinkRecovery::new(RecoveryMode::Off, "hci0".into());
        let outcome = gate.recover("t", || async { true }).await;
        assert!(matches!(outcome, Outcome::Disabled));
        assert!(gate.state.lock().await.last_adapter_cycle.is_none());

        // Rate-limited: both rungs ran a moment ago, so nothing is attempted.
        let gate = StaleLinkRecovery::new(RecoveryMode::Auto, "hci0".into());
        {
            let mut h = gate.state.lock().await;
            h.last_adapter_cycle = Some(Instant::now());
            h.last_daemon_restart = Some(Instant::now());
        }
        let outcome = gate.recover("t", || async { true }).await;
        assert!(matches!(outcome, Outcome::RateLimited));
    }
}
