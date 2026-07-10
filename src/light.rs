//! Per-light BLE actor. One of these runs as a tokio task for each configured
//! light, keyed by MAC. It owns that light's whole lifecycle:
//!
//! - discover (from the shared scan) → connect → verify Neewer GATT,
//! - power on (if configured),
//! - **coalescing flush:** at `flush_hz`, send the latest desired `LightState`
//!   only if it changed (handles the ArtNet-44Hz → BLE rate mismatch),
//! - **liveness — a LAYERED design (NOTES.md §5), tuned after a reconnect-storm.**
//!   These are two-chip lights — a BLE-UART radio in front of the LED-MCU that runs
//!   the `0x78` parser — so a generic GATT read only proves the *radio* is up; the
//!   command path can wedge while the light sits at its last colour ignoring
//!   everything (the TL60, hours in). Two independent checks:
//!   1. **Connection health = a cheap GATT read every `probe_secs`, for EVERY
//!      fixture.** The radio answers it, so it's stable and keeps a healthy light
//!      connected with zero churn; 3 consecutive misses = a genuinely dead link.
//!   2. **Hard-wedge detector = notify silence, but only for a PROVEN reply-capable
//!      fixture and only after `wedge_secs` (minutes).** We send a status canary each
//!      probe; a fixture that has answered `MIN_CANARY_REPLIES` of them is judged by
//!      its notify stream — if it then goes silent for `wedge_secs`, recycle. A
//!      *missed* canary is NOT itself a trigger. This tolerance is deliberate: an
//!      earlier "3 missed canaries = 60 s ⇒ recycle" gate turned transient notify
//!      loss into a full recycle, and on a shared adapter one flapping light's
//!      reconnects starved the others' canaries until the whole fleet "wedged" — a
//!      thundering-herd storm. Minutes-long silence is the real wedge; seconds is noise.
//!   3. A fixture that never answers a canary is genuinely deaf (e.g. the TL97C): it
//!      never arms (2), and relies on (1) plus a periodic forced reconnect
//!      (`[ble] refresh_secs`) to clear any wedged-but-connected state.
//! - reconnect with jittered backoff (de-syncs the fleet), indefinitely.
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
/// Consecutive GATT-read failures before we declare the *link* dead and recycle.
/// This is the cheap connection-health check (the radio answers it), so it can't
/// false-wedge — a real dead link fails it, a wedged-but-connected one does not.
const MAX_PROBE_FAILURES: u32 = 3;
/// Canary replies a fixture must produce before the notify-based wedge detector arms
/// for it. Below this it's treated as deaf (GATT-read health + `refresh_secs`), so a
/// mostly-silent fixture that emits the odd unsolicited notify (e.g. the TL97C) never
/// gets trapped on a reply signal it can't sustain.
const MIN_CANARY_REPLIES: u32 = 3;
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
    wedge_secs: u64,
    refresh_secs: u64,
}

impl LightActor {
    pub fn new(
        cfg: LightCfg,
        adapter: Adapter,
        rx: watch::Receiver<LightState>,
        flush_hz: u32,
        probe_secs: u64,
        wedge_secs: u64,
        refresh_secs: u64,
    ) -> Self {
        Self { cfg, adapter, rx, flush_hz, probe_secs, wedge_secs, refresh_secs }
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
        let mut reconnect_count: u64 = 0;

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
            // De-sync the herd: on a shared adapter, several actors reconnecting in
            // lockstep saturate it (scan + connect + disconnect) and starve each
            // other's canaries — which is how one flapping light cascaded into all of
            // them "wedging". Spread reconnects with per-light + per-attempt jitter on
            // top of the base backoff (deterministic, so no rng dependency).
            reconnect_count = reconnect_count.wrapping_add(1);
            let jitter_ms = (mac_bytes[5] as u64)
                .wrapping_mul(97)
                .wrapping_add(reconnect_count.wrapping_mul(211))
                % 3000;
            let backoff = RECONNECT_BACKOFF + Duration::from_millis(jitter_ms);
            info!(light = %label, backoff_secs = backoff.as_secs_f32(), "disconnected; reconnecting after backoff");
            tokio::time::sleep(backoff).await;
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

        // Connection-health probe target: a readable characteristic (usually Generic
        // Access Device Name). The GATT read against it is the base liveness check for
        // EVERY fixture (the radio answers it — stable, no churn). See the probe arm.
        let read_char = ble::find_readable_char(p);
        if read_char.is_none() {
            debug!(light = %label, "no readable characteristic; connection health falls back to is_connected() only");
        }

        let mut last_sent: Option<LightState> = None;
        // Connection-health failures (consecutive GATT-read misses); reset on success.
        let mut failures: u32 = 0;
        // Notify-based hard-wedge signal (see the module docs + the probe arm).
        // `last_reply_at` = time of the most recent notify. `canary_replies` counts
        // canaries the fixture actually ANSWERED (a notify seen while a canary was
        // outstanding); the wedge detector arms only once this reaches
        // `MIN_CANARY_REPLIES` (proven reply-capable) and then fires only after
        // `wedge_secs` of silence — so it can't thrash on transient loss, and a deaf
        // fixture that never answers never arms it.
        let mut last_reply_at: Option<Instant> = None;
        let mut canary_replies: u32 = 0;
        let mut canary_outstanding = false;
        // When this session connected — drives the deaf-fixture periodic refresh.
        let session_start = Instant::now();
        let rssi0 = ble::rssi(p).await;
        info!(
            light = %label,
            flush_hz = self.flush_hz,
            probe_secs = self.probe_secs,
            wedge_secs = self.wedge_secs,
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
            // (landing on the notify arm) starts proving the link is reply-capable.
            if send_status_queries(p, &chars.write, mac, true).await {
                canary_outstanding = true;
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
                    // Advertisement RSSI (diagnostics only — not a liveness signal).
                    let rssi = ble::rssi(p).await;
                    if !ble::is_connected(p).await {
                        bail!("peripheral reports disconnected");
                    }

                    // If the notify stream dropped (a transient backend hiccup on a live
                    // link, or a prior disconnect), try to restore it — here, paced at
                    // the probe interval, so there's no tight re-subscribe loop. Losing
                    // notify only costs the wedge detector below; GATT-read health is
                    // unaffected, so this is best-effort.
                    if notif.is_none() {
                        if let Some(nc) = &chars.notify {
                            if let Ok(stream) = ble::subscribe_notify(p, nc).await {
                                debug!(light = %label, "notify stream re-subscribed");
                                notif = Some(stream);
                            }
                        }
                    }

                    // (1) CONNECTION HEALTH — a cheap GATT read for EVERY fixture. The
                    // radio answers it, so it's stable and can't false-wedge; it's what
                    // keeps a healthy light connected with no churn. Three consecutive
                    // misses = a genuinely dead link. No readable char ⇒ is_connected()
                    // only (checked above).
                    let conn_ok = match &read_char {
                        Some(rc) => ble::probe_read(p, rc, PROBE_TIMEOUT).await,
                        None => true,
                    };
                    if conn_ok {
                        if failures > 0 {
                            debug!(light = %label, "connection restored");
                        }
                        failures = 0;
                    } else {
                        failures += 1;
                        warn!(light = %label, failures, rssi = ?rssi, "connection probe failed (GATT read)");
                        if failures >= MAX_PROBE_FAILURES {
                            bail!("dead link: {failures} consecutive GATT-read failures");
                        }
                    }

                    // (2) HARD-WEDGE DETECTOR — notify-based, tolerant, and ONLY for a
                    // fixture that has proven it answers canaries. It catches the LED-MCU
                    // wedge (the TL60 case) that the radio-answered GATT read above can't
                    // see: the light stays "connected" but stops answering. Firing only
                    // after `wedge_secs` of silence separates that genuine multi-minute
                    // stall from transient notify loss / brief shared-adapter contention
                    // (the old 60 s reply-gate caused a reconnect storm across the fleet).
                    let reply_capable = canary_replies >= MIN_CANARY_REPLIES;
                    if is_wedged(canary_replies, last_reply_at.map(|t| t.elapsed()), self.wedge_secs) {
                        let silent = last_reply_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                        wedge.count += 1;
                        wedge.wedged_at = Some(Instant::now());
                        warn!(light = %label, wedge = wedge.count, silent_secs = silent,
                              "⚠ WEDGE — reply-capable light went silent while still connected; recycling link");
                        bail!("wedge: no notify for {silent}s despite answering earlier");
                    } else if !reply_capable
                        && self.refresh_secs > 0
                        && session_start.elapsed().as_secs() >= self.refresh_secs
                    {
                        // (3) DEAF-FIXTURE BACKSTOP — a fixture that never answers a
                        // canary can't be wedge-detected passively (e.g. the TL97C), so
                        // bound any wedged-but-connected state with a periodic clean
                        // reconnect. Reply-capable fixtures use (2) instead of this.
                        info!(light = %label, refresh_secs = self.refresh_secs,
                              "refreshing unverifiable (deaf) link to clear any wedged-but-connected state");
                        bail!("periodic refresh of a deaf fixture");
                    }

                    // (4) Send this cycle's canary (battery + temperature): status
                    // telemetry, and it elicits the notify the wedge detector watches.
                    // A missed reply is NOT itself a recycle trigger — only (2) is.
                    if let Some(mac) = query_mac {
                        if send_status_queries(p, &chars.write, mac, false).await {
                            canary_outstanding = true;
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
                            // Any notify frame is the LED-MCU (through the radio) proving
                            // the command path is alive. Record the time; if it answers a
                            // canary we issued, count it toward "reply-capable" (a stray
                            // unsolicited notify with no canary outstanding doesn't count,
                            // so a mostly-deaf fixture never arms the wedge detector).
                            last_reply_at = Some(Instant::now());
                            if canary_outstanding {
                                canary_replies = canary_replies.saturating_add(1);
                                canary_outstanding = false;
                            }
                            // First reply after a detected wedge ⇒ the reconnect cleared
                            // it — the headline "did it recover?" signal for the soak.
                            if let Some(since) = wedge.wedged_at.take() {
                                info!(light = %label, wedge = wedge.count,
                                      down_secs = since.elapsed().as_secs(),
                                      "✔ RECOVERED — light is answering again after a wedge");
                            }
                            log_status(label, &n.value, &mut status);
                        }
                        None => {
                            // Stream closed (usually a disconnect, occasionally a backend
                            // hiccup on a live link). Mark it gone; the probe arm re-subscribes
                            // (paced) if the link is still up, and a real disconnect is caught
                            // by is_connected() on the next tick. No hair-trigger reconnect.
                            debug!(light = %label, "notify stream ended; will re-subscribe on next probe tick");
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
    // Battery (0x95) is the canary — every reply-capable model answers it when
    // healthy: TL120C/TL21C AND the TL60 (HW-confirmed: a healthy TL60 replies
    // battery/version/state; it only goes silent once WEDGED, which is what we
    // detect). Temperature is telemetry (these models don't answer it, so it isn't a
    // canary). Keep the canary to plain reads (0x95/0x9E/0xB3/0x8E). A streamer-support
    // read (0xC4 = getIsSupportStreamer) was trialed here as a TL60 spare and removed —
    // NOT because it's harmful (the all-white first blamed on it was really a wrong
    // config, not this frame; see NOTES.md) but because battery already covers every
    // model, so it's redundant.
    let mut frames = vec![queries::battery(mac), queries::temperature(mac)];
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

/// Whether a fixture should be treated as WEDGED (LED-MCU stopped answering while
/// the link is still up), so the session recycles. Pure, so it's unit-tested without
/// a radio. Three gates, all required:
/// - `wedge_secs > 0` — the detector is enabled.
/// - `canary_replies >= MIN_CANARY_REPLIES` — the fixture has PROVEN it answers
///   canaries, so silence is meaningful (a deaf fixture never arms this; it uses the
///   `refresh_secs` backstop instead).
/// - `silent >= wedge_secs` — it's now been quiet that long. Generous by design, so
///   transient notify loss / brief shared-adapter contention never trips it.
fn is_wedged(canary_replies: u32, silent: Option<Duration>, wedge_secs: u64) -> bool {
    wedge_secs > 0
        && canary_replies >= MIN_CANARY_REPLIES
        && silent.is_some_and(|d| d.as_secs() >= wedge_secs)
}

#[cfg(test)]
mod tests {
    use super::{is_wedged, MIN_CANARY_REPLIES};
    use std::time::Duration;

    #[test]
    fn not_wedged_before_arming() {
        // Below MIN_CANARY_REPLIES the detector is not armed, no matter how long the
        // silence — a deaf / barely-replying fixture (e.g. TL97C) never trips it.
        assert!(!is_wedged(MIN_CANARY_REPLIES - 1, Some(Duration::from_secs(9999)), 300));
    }

    #[test]
    fn not_wedged_when_recently_heard() {
        // Armed, but answered within the window ⇒ healthy, no recycle.
        assert!(!is_wedged(MIN_CANARY_REPLIES, Some(Duration::from_secs(30)), 300));
        assert!(!is_wedged(MIN_CANARY_REPLIES, Some(Duration::from_secs(299)), 300));
    }

    #[test]
    fn not_wedged_with_no_reply_timestamp() {
        // Armed count but nothing to time against ⇒ not wedged.
        assert!(!is_wedged(MIN_CANARY_REPLIES, None, 300));
    }

    #[test]
    fn wedged_when_armed_and_silent_past_window() {
        assert!(is_wedged(MIN_CANARY_REPLIES, Some(Duration::from_secs(300)), 300));
        assert!(is_wedged(MIN_CANARY_REPLIES + 5, Some(Duration::from_secs(600)), 300));
    }

    #[test]
    fn disabled_when_wedge_secs_zero() {
        // wedge_secs = 0 turns the detector off entirely.
        assert!(!is_wedged(MIN_CANARY_REPLIES, Some(Duration::from_secs(9999)), 0));
    }
}
