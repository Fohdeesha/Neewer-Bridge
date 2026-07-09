//! Per-light BLE actor. One of these runs as a tokio task for each configured
//! light, keyed by MAC. It owns that light's whole lifecycle:
//!
//! - discover (from the shared scan) → connect → verify Neewer GATT,
//! - power on (if configured),
//! - **coalescing flush:** at `flush_hz`, send the latest desired `LightState`
//!   only if it changed (handles the ArtNet-44Hz → BLE rate mismatch),
//! - **stale-session detection (NOTES.md §5):** these are two-chip lights — a
//!   BLE-UART radio module in front of the LED-MCU that runs the `0x78` parser —
//!   so a successful `write-without-response` (no ACK) or a generic GATT read
//!   only proves the *radio* is up; the command path can be wedged while the
//!   light sits at its last colour ignoring everything (observed on the TL60 a
//!   few hours in). So for a fixture that answers status queries we require a
//!   **notify reply to a periodic canary** (proves the LED-MCU, not just the
//!   radio); after repeated misses, recycle. A healthy TL60 DOES reply (battery/
//!   version/state) and only goes silent once wedged — so the reply-gate is its
//!   detector. The canary asks several ways (battery `0x95` for TL120C/TL21C/TL60,
//!   streamer `0xC4` as a spare) so most models produce *some* reply. A fixture
//!   that answers *nothing even when healthy* is genuinely deaf (e.g. the TL97C) —
//!   it falls back to the GATT read (deaf ≠ dead, don't drop-cycle it) **plus a
//!   periodic forced reconnect** (`[ble] refresh_secs`) as a backstop for a link we
//!   can't verify (also catches a wedged TL60 that hasn't re-replied yet).
//! - reconnect with backoff, indefinitely.
//!
//! Because binding is by MAC and the actor exists for the whole process
//! lifetime, the DMX→light mapping is stable regardless of power-on/discovery
//! order — a light that's currently absent simply keeps retrying.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use btleplug::api::Characteristic;
use btleplug::platform::{Adapter, Peripheral};
use futures::StreamExt;
use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::ble::{self, NeewerChars};
use crate::config::{parse_mac, LightCfg};
use crate::driver::Driver;
use crate::profile::Profile;
use crate::protocol::replies::{self, Reply};
use crate::protocol::{queries, LightState};

/// How long a liveness probe may take before it counts as a failure (§5).
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
/// Consecutive probe failures before we declare the session stale and recycle.
const MAX_PROBE_FAILURES: u32 = 3;
/// How often to poll the shared scan while waiting for the light to appear.
const FIND_POLL: Duration = Duration::from_secs(2);
/// Backoff after a failed/ended session before reconnecting.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(4);

/// Cross-reconnect wedge bookkeeping, so the actor can log wedge-detection and
/// recovery at a glance during a long soak. Owned by [`LightActor::run`] and passed
/// `&mut` into each session (the state has to outlive a single session — a wedge is
/// detected in one session and cleared when replies resume in a *later* one).
#[derive(Default)]
struct WedgeTracker {
    /// How many wedges (healthy→silent-while-connected) we've caught on this light.
    count: u32,
    /// When the current wedge was detected (`None` = the light is not wedged). Set
    /// when the reply-gate gives up; taken (and logged) when replies resume.
    wedged_at: Option<Instant>,
}

pub struct LightActor {
    cfg: LightCfg,
    adapter: Adapter,
    rx: watch::Receiver<LightState>,
    flush_hz: u32,
    probe_secs: u64,
    refresh_secs: u64,
}

impl LightActor {
    pub fn new(
        cfg: LightCfg,
        adapter: Adapter,
        rx: watch::Receiver<LightState>,
        flush_hz: u32,
        probe_secs: u64,
        refresh_secs: u64,
    ) -> Self {
        Self { cfg, adapter, rx, flush_hz, probe_secs, refresh_secs }
    }

    fn label(&self) -> String {
        self.cfg.name.clone().unwrap_or_else(|| self.cfg.mac.clone())
    }

    /// Run forever: (re)connect and serve until shut down.
    pub async fn run(mut self) {
        let label = self.label();
        // These were validated at config load; unwrap is safe.
        let mac_bytes = parse_mac(&self.cfg.mac).expect("validated mac");
        let profile = Profile::parse(&self.cfg.profile).expect("validated profile");
        // Persists across reconnects: counts wedges and tracks an in-progress one so
        // "⚠ WEDGE" / "✔ RECOVERED" read cleanly even though detection and recovery
        // land in different sessions.
        let mut wedge = WedgeTracker::default();

        loop {
            let (peripheral, name) = self.find().await;
            let driver = Driver::resolve(&self.cfg.driver, profile, mac_bytes, &name, self.cfg.cmd_type);
            info!(light = %label, ble_name = %name, driver = driver.label(), "connecting");

            match ble::connect_and_verify(&peripheral).await {
                Ok(chars) => {
                    info!(light = %label, "connected");
                    // Power is driven entirely by the flushed LightState (the
                    // bridge seeds the initial state's power from
                    // `power_on_connect`), so there's no separate power-on here.
                    if let Err(e) = self.session(&label, &peripheral, &chars, &driver, &mut wedge).await {
                        warn!(light = %label, error = %e, "session ended; will reconnect");
                    }
                }
                Err(e) => warn!(light = %label, error = %e, "connect/verify failed"),
            }

            // Best-effort disconnect so the OS doesn't keep a half-open handle.
            let _ = ble::disconnect(&peripheral).await;
            info!(light = %label, backoff_secs = RECONNECT_BACKOFF.as_secs(), "disconnected; reconnecting after backoff");
            tokio::time::sleep(RECONNECT_BACKOFF).await;
        }
    }

    /// Poll the shared scan until our light appears.
    async fn find(&self) -> (Peripheral, String) {
        let label = self.label();
        loop {
            match ble::find_scanned(&self.adapter, &self.cfg.mac).await {
                Ok(Some(found)) => return found,
                Ok(None) => debug!(light = %label, "not discovered yet; waiting"),
                Err(e) => warn!(light = %label, error = %e, "error listing peripherals"),
            }
            tokio::time::sleep(FIND_POLL).await;
        }
    }

    /// Active session: coalescing flush loop + liveness probing. Returns `Err`
    /// when the session should be torn down and reconnected.
    async fn session(
        &mut self,
        label: &str,
        p: &Peripheral,
        chars: &NeewerChars,
        driver: &Driver,
        wedge: &mut WedgeTracker,
    ) -> Result<()> {
        let flush_ms = (1000 / self.flush_hz.max(1)).max(1) as u64;
        let mut flush = interval(Duration::from_millis(flush_ms));
        flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut probe = interval(Duration::from_secs(self.probe_secs.max(1)));
        probe.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The first `interval` tick fires immediately; skip an instant probe so
        // we don't probe before the link has settled.
        probe.tick().await;

        // Fallback liveness signal, used ONLY for a fixture that never notifies
        // (the notify reply is a strictly stronger signal — it proves the LED-MCU,
        // not just the radio that answers this read). See the probe arm.
        let read_char = ble::find_readable_char(p);
        if read_char.is_none() {
            debug!(light = %label, "no readable characteristic; a fixture that also never notifies falls back to is_connected() only");
        }

        let mut last_sent: Option<LightState> = None;
        let mut failures: u32 = 0;
        // Liveness signal (see the module docs + the probe arm below). `ever_replied`
        // latches once the light sends any notify — until then it's treated as
        // "deaf, maybe alive" and probed via the GATT read. `last_reply_at` is the
        // monotonic time of the most recent notify; `probe_query_at` is when we last
        // issued the canary. A reply newer than the last canary proves the link.
        let mut ever_replied = false;
        let mut last_reply_at: Option<Instant> = None;
        let mut probe_query_at: Option<Instant> = None;
        // When this session connected — drives the deaf-fixture periodic refresh.
        let session_start = Instant::now();
        let rssi0 = ble::rssi(p).await;
        info!(
            light = %label,
            flush_hz = self.flush_hz,
            probe_secs = self.probe_secs,
            refresh_secs = self.refresh_secs,
            rssi = ?rssi0,
            "session active"
        );

        // Status reads (NOTES.md §3.6): subscribe to the notify characteristic and
        // fire an initial battery/temp/version/state query. MAC-addressed, so only
        // for MAC-carrying drivers (classic/infinity, not Home). All best-effort — a
        // light without notify is still fully controllable.
        let query_mac = driver.mac();
        let mut notif = match &chars.notify {
            Some(nc) => match ble::subscribe_notify(p, nc).await {
                Ok(stream) => Some(stream),
                Err(e) => {
                    warn!(light = %label, error = %e, "notify subscribe failed; status reads disabled");
                    None
                }
            },
            None => None,
        };
        let mut status = StatusCache::default();
        if let Some(mac) = query_mac {
            // The initial full query doubles as the first canary; a reply to it
            // (landing on the notify arm) is what proves the link on the first probe.
            if send_status_queries(p, &chars.write, mac, true).await {
                probe_query_at = Some(Instant::now());
            }
        }

        loop {
            tokio::select! {
                _ = flush.tick() => {
                    let desired = *self.rx.borrow();
                    if Some(desired) != last_sent {
                        let prev_power = last_sent.map(|l| l.power);
                        if desired.power {
                            // Power-on only on transition (or the first send).
                            if prev_power != Some(true) {
                                info!(light = %label, "power on");
                                ble::write_command(p, &chars.write, &driver.power(true))
                                    .await
                                    .map_err(|e| anyhow::anyhow!("power-on write failed: {e}"))?;
                            }
                            // The state command itself is per-frame at ArtNet rates,
                            // so it's debug (kept off the info console but in the file).
                            debug!(light = %label, state = %desired.summary(), "flush");
                            // Most modes are one frame; Pixel emits several. Each
                            // frame is MTU-chunked as needed (pixel palettes >20 B).
                            // Multi-frame (pixel) sub-frames are spaced ~80 ms apart
                            // as the app does, so the effect registers (and, for a
                            // static render, so PLAY establishes before PAUSE).
                            let frames = driver.apply_frames(&desired);
                            let spaced = frames.len() > 1;
                            let n = frames.len();
                            for (i, frame) in frames.iter().enumerate() {
                                ble::write_command_chunked(p, &chars.write, frame)
                                    .await
                                    .map_err(|e| anyhow::anyhow!("flush write failed: {e}"))?;
                                if spaced && i + 1 < n {
                                    tokio::time::sleep(Duration::from_millis(80)).await;
                                }
                            }
                        } else if prev_power != Some(false) {
                            // Power-off only on transition (failsafe poweroff).
                            info!(light = %label, "power off");
                            ble::write_command(p, &chars.write, &driver.power(false))
                                .await
                                .map_err(|e| anyhow::anyhow!("power-off write failed: {e}"))?;
                        }
                        last_sent = Some(desired);
                    }
                }
                _ = probe.tick() => {
                    // Advertisement RSSI (diagnostics only — not the liveness signal).
                    let rssi = ble::rssi(p).await;
                    if !ble::is_connected(p).await {
                        bail!("peripheral reports disconnected");
                    }

                    // Deaf-fixture safety net (the TL60 case): a fixture that never
                    // sends a notify can't be actively verified, yet it can wedge while
                    // still "connected" — writes succeed into it and the radio keeps
                    // answering the GATT read, so NO passive probe sees the stall. Bound
                    // it by forcing a clean reconnect every `refresh_secs`. Only applies
                    // while `ever_replied` is false; a fixture that answers is verified
                    // by the reply-gate below and never force-refreshed.
                    if !ever_replied
                        && self.refresh_secs > 0
                        && session_start.elapsed().as_secs() >= self.refresh_secs
                    {
                        info!(light = %label, refresh_secs = self.refresh_secs,
                              "refreshing unverifiable (never-replied) link to clear any wedged-but-connected state");
                        bail!("periodic refresh of a deaf fixture");
                    }

                    // Liveness. For a fixture that answers status queries, require a
                    // notify reply newer than our last canary — that proves the LED-MCU
                    // command path, which a `write-without-response` or a radio-answered
                    // GATT read can NOT (the wedged-command-path / half-open-link case).
                    // A fixture that has never notified can't be told apart from one
                    // that's simply deaf, so it falls back to the GATT read.
                    let satisfied = if ever_replied {
                        liveness_by_reply(last_reply_at, probe_query_at)
                    } else {
                        match &read_char {
                            Some(rc) => ble::probe_read(p, rc, PROBE_TIMEOUT).await,
                            None => true, // nothing readable + never notified: is_connected() only
                        }
                    };

                    if satisfied {
                        if failures > 0 {
                            debug!(light = %label, "liveness restored");
                        }
                        failures = 0;
                        debug!(light = %label, rssi = ?rssi, ever_replied, "liveness ok");
                    } else {
                        failures += 1;
                        warn!(light = %label, failures, rssi = ?rssi, ever_replied, "liveness probe failed");
                        if failures >= MAX_PROBE_FAILURES {
                            // A reply-capable light that has gone silent while still
                            // "connected" is wedged. Record it and log a headline line
                            // (cleared by "✔ RECOVERED" when it answers again) so a soak
                            // shows the wedge/recovery cycle at a glance.
                            if ever_replied {
                                wedge.count += 1;
                                wedge.wedged_at = Some(Instant::now());
                                warn!(light = %label, wedge = wedge.count,
                                      healthy_secs = session_start.elapsed().as_secs(),
                                      "⚠ WEDGE — light stopped answering the canary while still connected; recycling link");
                            }
                            bail!("stale session: {failures} consecutive liveness misses (no reply to canary)");
                        }
                    }

                    // Issue this cycle's canary (battery + temperature). Replies land on
                    // the notify arm and advance `last_reply_at`; a reply-capable light
                    // that stops answering these is exactly the half-open case above.
                    if let Some(mac) = query_mac {
                        if send_status_queries(p, &chars.write, mac, false).await {
                            probe_query_at = Some(Instant::now());
                        }
                    }
                }
                // Status replies (battery/temp/version/state) pushed on the notify
                // characteristic. Decoded and logged; when the stream ends (disconnect)
                // we stop polling it. `pending()` parks this arm when there's no notify.
                notification = async {
                    match notif.as_mut() {
                        Some(stream) => stream.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match notification {
                        Some(n) => {
                            // Any frame on the notify char is the LED-MCU (through the
                            // radio) proving the command path is alive — the liveness
                            // signal a reply-capable fixture is judged by.
                            let first_reply = !ever_replied;
                            ever_replied = true;
                            last_reply_at = Some(Instant::now());
                            // If this is the first reply since a wedge, the light is
                            // answering again — the headline "did the reconnect clear
                            // it?" signal for the soak.
                            if first_reply {
                                if let Some(since) = wedge.wedged_at.take() {
                                    info!(light = %label, wedge = wedge.count,
                                          down_secs = since.elapsed().as_secs(),
                                          "✔ RECOVERED — light is answering again after a wedge");
                                }
                            }
                            log_status(label, &n.value, &mut status);
                        }
                        None => {
                            // Stream closed (normally = disconnect). Stop polling it and
                            // let the two existing signals recover us rather than bailing
                            // straight away — a hair-trigger reconnect here risks a tight
                            // loop if the backend ever closes a stream on a live link. On a
                            // reply-capable fixture the replies now stop, so the probe arm's
                            // reply-gate recycles the session within a few probe cycles; a
                            // real disconnect is caught by `is_connected()` on the next tick.
                            debug!(light = %label, ever_replied, "notify stream ended; relying on liveness probe to recover");
                            notif = None;
                        }
                    }
                }
            }
        }
    }
}

/// Last-seen status values, so the actor logs battery/temp/firmware at `info` only
/// when they change (and at `debug` otherwise) instead of on every poll.
#[derive(Default)]
struct StatusCache {
    battery: Option<u8>,
    temp_c: Option<i16>,
    version: Option<(u8, u8, u8)>,
    external: bool,
}

/// Decode one notify frame and log it — known status at `info` on change (`debug`
/// unchanged), unrecognised frames at `debug` with the raw hex.
fn log_status(label: &str, data: &[u8], cache: &mut StatusCache) {
    let Some(reply) = replies::parse(data) else {
        debug!(light = %label, data = %ble::hexstr(data), "notify (undecoded)");
        return;
    };
    let changed = match reply {
        Reply::Battery { percent } => {
            let c = cache.battery != Some(percent);
            cache.battery = Some(percent);
            c
        }
        Reply::Temperature { celsius } => {
            let c = cache.temp_c != Some(celsius);
            cache.temp_c = Some(celsius);
            c
        }
        Reply::Version { major, minor, patch, .. } => {
            let v = (major, minor, patch);
            let c = cache.version != Some(v);
            cache.version = Some(v);
            c
        }
        Reply::ExternalPower { .. } => {
            let c = !cache.external;
            cache.external = true;
            c
        }
        Reply::Power { .. } | Reply::State { .. } => true,
    };
    if changed {
        info!(light = %label, status = %reply.summary(), "status");
    } else {
        debug!(light = %label, status = %reply.summary(), "status (unchanged)");
    }
}

/// Best-effort MAC-addressed status queries, doubling as the liveness canary.
/// Replies land on the notify stream (see [`log_status`]) and advance the actor's
/// `last_reply_at`. `full` adds the version + state reads (used once per session);
/// otherwise just battery + temperature (polled). **Battery (`0x95`) is the
/// universal canary** — the only read every reply-capable model answers (it's the
/// sole reply the TL21C gives; the TL60/TL120C answer it too). Returns `true` if
/// every frame was written (so the caller can time the canary), `false` if a write
/// failed. A write failure is itself non-fatal here — link health is decided by the
/// probe arm, which will see the missing reply.
async fn send_status_queries(p: &Peripheral, write: &Characteristic, mac: [u8; 6], full: bool) -> bool {
    // Battery (0x95) is the canary every reply-capable model answers when healthy —
    // TL120C/TL21C AND the TL60 (HW-confirmed: a healthy TL60 replies battery/version/
    // state; it only goes silent once WEDGED, which is exactly what we detect).
    // Streamer-support (0xC4) is a spare for streamer-capable fixtures (some TL60
    // units answer only it — bengt). Sending both makes the canary model-agnostic: a
    // reply to *either* proves the link. Temperature is telemetry (these models don't
    // answer it, so it isn't a canary).
    let mut frames = vec![
        queries::battery(mac),
        queries::streamer_support(mac),
        queries::temperature(mac),
    ];
    if full {
        frames.push(queries::version(mac));
        frames.push(queries::state(mac));
    }
    for f in &frames {
        if ble::write_command(p, write, f).await.is_err() {
            debug!("status query write failed (non-fatal)");
            return false;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    true
}

/// Whether a reply-capable fixture's link is proven live this probe cycle: it has
/// sent a notify (`last_reply_at`) at least as recent as the canary we last issued
/// (`probe_query_at`). Pure so it can be unit-tested without a radio.
///
/// - No canary issued yet (`probe_query_at == None`) ⇒ satisfied (nothing to answer).
/// - A reply at/after the last canary ⇒ satisfied (link proven).
/// - The canary went unanswered (reply older than it, or none) ⇒ not satisfied.
fn liveness_by_reply(last_reply_at: Option<Instant>, probe_query_at: Option<Instant>) -> bool {
    match probe_query_at {
        Some(queried) => last_reply_at.is_some_and(|reply| reply >= queried),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::liveness_by_reply;
    use std::time::{Duration, Instant};

    #[test]
    fn no_canary_issued_yet_is_satisfied() {
        // Before the first canary there is nothing to answer, so never a miss —
        // regardless of whether a reply has been seen.
        assert!(liveness_by_reply(None, None));
        assert!(liveness_by_reply(Some(Instant::now()), None));
    }

    #[test]
    fn reply_after_canary_is_satisfied() {
        let queried = Instant::now();
        let reply = queried + Duration::from_millis(200); // healthy: answered ~200 ms later
        assert!(liveness_by_reply(Some(reply), Some(queried)));
    }

    #[test]
    fn reply_at_same_instant_is_satisfied() {
        let t = Instant::now(); // `reply >= queried` is inclusive
        assert!(liveness_by_reply(Some(t), Some(t)));
    }

    #[test]
    fn stale_reply_before_canary_is_a_miss() {
        // The wedge case: the light answered earlier but the latest canary got no
        // reply, so the newest reply predates it.
        let reply = Instant::now();
        let queried = reply + Duration::from_secs(20);
        assert!(!liveness_by_reply(Some(reply), Some(queried)));
    }

    #[test]
    fn canary_with_no_reply_is_a_miss() {
        assert!(!liveness_by_reply(None, Some(Instant::now())));
    }
}
