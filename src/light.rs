//! Per-light BLE actor. One of these runs as a tokio task for each configured
//! light, keyed by MAC. It owns that light's whole lifecycle:
//!
//! - discover (from the shared scan) → connect → verify Neewer GATT,
//! - power on (if configured),
//! - **coalescing flush:** at `flush_hz`, send the latest desired `LightState`
//!   only if it changed (handles the ArtNet-44Hz → BLE rate mismatch),
//! - **liveness:** a cheap GATT read every `probe_secs`, for every fixture. These
//!   lights give no write ACK (write-without-response), so an unprobed half-dead
//!   link would look healthy forever — NeewerLite's stale-session lesson. The read
//!   is answered by the light's radio, so a healthy light stays connected with zero
//!   churn; three consecutive misses — or a reported disconnect — means a dead link
//!   and we reconnect. A stuck/unresponsive fixture has TWO distinct causes
//!   (both seen live on hardware): a **weak-RF link** (drops or
//!   won't connect at −90 dBm and below; moving it closer + this reconnect loop
//!   recovers it) and a genuine **firmware WEDGE** (the fixture stays dead even
//!   with the adapter touching it; ONLY a physical power-cycle clears it — RF
//!   proximity and reflashing do not). The bridge deliberately does NOT try to
//!   auto-detect the wedge: an earlier notify-silence detector false-positived and
//!   aggressively recycled healthy links, and a real wedge needs human action
//!   (power-cycle) anyway. Distinguish them by whether proximity recovers it.
//! - status reads (battery/temperature/firmware/state) alongside each probe, purely
//!   as logged telemetry — a light without notify is still fully controllable.
//! - reconnect with jittered backoff (de-syncs the fleet), indefinitely.
//!
//! Because binding is by MAC and the actor exists for the whole process
//! lifetime, the DMX→light mapping is stable regardless of power-on/discovery
//! order — a light that's currently absent simply keeps retrying.

use std::sync::Arc;
use std::time::Duration;

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
use crate::scan::ScanCoordinator;

/// How long a liveness probe may take before it counts as a failure (§5).
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
/// Consecutive GATT-read failures before we declare the *link* dead and recycle.
/// The radio answers this read, so it's a stable connection-health signal — a real
/// dead link (weak signal / gone) fails it, a merely-idle one does not.
const MAX_PROBE_FAILURES: u32 = 3;
/// How often to poll the shared scan while waiting for the light to appear.
const FIND_POLL: Duration = Duration::from_secs(2);
/// Backoff after a failed/ended session before reconnecting.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(4);
/// Gap between consecutive status-query writes, so a light's reply queue isn't
/// outrun (see [`send_status_queries`]).
const QUERY_SPACING: Duration = Duration::from_millis(60);
/// Gap between the sub-frames of a multi-frame (pixel) state, matching the app.
const PIXEL_FRAME_SPACING: Duration = Duration::from_millis(80);

pub struct LightActor {
    cfg: LightCfg,
    adapter: Adapter,
    rx: watch::Receiver<LightState>,
    flush_hz: u32,
    probe_secs: u64,
    scan: Arc<ScanCoordinator>,
}

impl LightActor {
    pub fn new(
        cfg: LightCfg,
        adapter: Adapter,
        rx: watch::Receiver<LightState>,
        flush_hz: u32,
        probe_secs: u64,
        scan: Arc<ScanCoordinator>,
    ) -> Self {
        Self { cfg, adapter, rx, flush_hz, probe_secs, scan }
    }

    fn label(&self) -> String {
        // An empty configured name (older `add` runs wrote `name = ""`) falls
        // back to the MAC, same as no name at all.
        self.cfg
            .name
            .as_deref()
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.cfg.mac.clone())
    }

    /// Run forever: (re)connect and serve until shut down.
    pub async fn run(mut self) {
        let label = self.label();
        // These were validated at config load; unwrap is safe.
        let mac_bytes = parse_mac(&self.cfg.mac).expect("validated mac");
        let profile = Profile::parse(&self.cfg.profile).expect("validated profile");
        let mut reconnect_count: u64 = 0;

        loop {
            // Request the discovery scan for exactly as long as this light is
            // disconnected. The coordinator scans only while ≥1 such request is
            // outstanding (and even then in bursts), so a fully-connected fleet
            // does no scanning at all — which is what stops the cheap USB
            // controller choking on a permanent scan (see scan.rs).
            let mut searching = Some(self.scan.begin_search());
            let (peripheral, name, discovery_rssi) = self.find().await;
            // Discovery can race the platform's name cache (BlueZ may not have
            // local_name yet when we find the peripheral by MAC); an empty name
            // would make `driver = "auto"` mis-resolve an NH-* Home light to
            // Classic, so fall back to the configured name for resolution.
            let resolve_name: &str = if name.is_empty() {
                self.cfg.name.as_deref().unwrap_or("")
            } else {
                &name
            };
            let driver =
                Driver::resolve(&self.cfg.driver, profile, mac_bytes, resolve_name, self.cfg.cmd_type);
            info!(light = %label, ble_name = %name, driver = driver.label(), rssi = ?discovery_rssi, "connecting");

            match ble::connect_and_verify(&peripheral).await {
                Ok(chars) => {
                    info!(light = %label, "connected");
                    // Connected: drop the discovery request for the whole session
                    // (re-acquired on the next loop iteration if the link drops).
                    searching = None;
                    // Power is driven entirely by the flushed LightState (the
                    // bridge seeds the initial state's power from
                    // `power_on_connect`), so there's no separate power-on here.
                    if let Err(e) =
                        self.session(&label, &peripheral, &chars, &driver, discovery_rssi).await
                    {
                        warn!(light = %label, error = %e, "session ended; will reconnect");
                    }
                }
                Err(e) => warn!(light = %label, error = %e, "connect/verify failed"),
            }

            // Best-effort disconnect so the OS doesn't keep a half-open handle.
            let _ = ble::disconnect(&peripheral).await;
            // Release the discovery request (if still held after a failed
            // connect) so we don't scan during the backoff sleep.
            drop(searching.take());
            // De-sync the herd: on a shared adapter, several actors reconnecting in
            // lockstep saturate it (scan + connect + disconnect) and starve each
            // other. Spread reconnects with per-light + per-attempt jitter on top of
            // the base backoff (deterministic, so no rng dependency).
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

    /// Poll the shared scan until our light appears. Returns the peripheral, its
    /// advertised name, and the discovery-time RSSI (the freshest signal reading
    /// this session will get — BlueZ clears the property once connected).
    async fn find(&self) -> (Peripheral, String, Option<i16>) {
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
    ///
    /// Note the select arms run their handlers to completion before the loop
    /// re-enters `select!` — so a probe against a *degrading* link can stall the
    /// flush arm for up to `PROBE_TIMEOUT` (12 s). That's a deliberate tradeoff:
    /// a link that slow isn't transferring write frames anyway (HW-proven on the
    /// TL60: 10 s of streamed colour over a −90 dBm "connected" link changed
    /// nothing), it's per-light (each actor is its own task, the fleet is never
    /// blocked), and on a healthy link the read returns in tens of ms.
    async fn session(
        &mut self,
        label: &str,
        p: &Peripheral,
        chars: &NeewerChars,
        driver: &Driver,
        discovery_rssi: Option<i16>,
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
        // Access Device Name). The GATT read against it is the liveness check for
        // every fixture (the radio answers it — stable, no churn). See the probe arm.
        let read_char = ble::find_readable_char(p);
        if read_char.is_none() {
            debug!(light = %label, "no readable characteristic; connection health falls back to is_connected() only");
        }

        let mut last_sent: Option<LightState> = None;
        // Connection-health failures (consecutive GATT-read misses); reset on success.
        let mut failures: u32 = 0;
        // Advertisement RSSI: BlueZ clears the live property once the device
        // connects, so the "most recent advertisement RSSI" for this session is
        // the value captured at discovery — fall back to it whenever the live
        // read is empty (which, with on-demand scanning, is nearly always).
        let rssi0 = ble::rssi(p).await.or(discovery_rssi);
        info!(
            light = %label,
            flush_hz = self.flush_hz,
            probe_secs = self.probe_secs,
            rssi = ?rssi0,
            "session active"
        );

        // Status reads (protocol::queries/replies): subscribe to the notify char and
        // fire an initial battery/temp/version/state query. MAC-addressed, so only
        // for MAC-carrying drivers (classic/infinity, not Home). Pure telemetry, all
        // best-effort — a light without notify is still fully controllable.
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
            send_status_queries(p, &chars.write, mac, true).await;
        }

        loop {
            tokio::select! {
                _ = flush.tick() => {
                    let desired = *self.rx.borrow();
                    if Some(desired) != last_sent {
                        // Power transitions first (pure decision — see
                        // `power_command` for the seed/failsafe corners).
                        match power_command(last_sent.map(|l| l.power), &desired) {
                            Some(true) => {
                                info!(light = %label, "power on");
                                ble::write_command(p, &chars.write, &driver.power(true))
                                    .await
                                    .map_err(|e| anyhow::anyhow!("power-on write failed: {e}"))?;
                            }
                            Some(false) => {
                                info!(light = %label, "power off");
                                ble::write_command(p, &chars.write, &driver.power(false))
                                    .await
                                    .map_err(|e| anyhow::anyhow!("power-off write failed: {e}"))?;
                            }
                            None => {}
                        }
                        if desired.power {
                            // The state command itself is per-frame at ArtNet rates,
                            // so it's debug (kept off the info console but in the file).
                            debug!(light = %label, state = %desired.summary(), "flush");
                            // Most modes are one frame; Pixel emits several. Each
                            // frame is MTU-chunked as needed (pixel palettes >20 B).
                            // Multi-frame (pixel) sub-frames are spaced ~80 ms apart
                            // as the app does, so the effect registers (and, for a
                            // static render, so PLAY establishes before PAUSE).
                            let frames = driver.apply_frames(&desired);
                            let mut it = frames.iter().peekable();
                            while let Some(frame) = it.next() {
                                ble::write_command_chunked(p, &chars.write, frame)
                                    .await
                                    .map_err(|e| anyhow::anyhow!("flush write failed: {e}"))?;
                                if it.peek().is_some() {
                                    tokio::time::sleep(PIXEL_FRAME_SPACING).await;
                                }
                            }
                        }
                        last_sent = Some(desired);
                    }
                }
                _ = probe.tick() => {
                    // Advertisement RSSI (diagnostics only — not a liveness signal,
                    // but the most useful field for spotting a weak/flaky placement).
                    // Falls back to the discovery-time value: BlueZ clears the live
                    // property while connected, and the discovery reading IS the most
                    // recent advertisement RSSI this session has.
                    let rssi = ble::rssi(p).await.or(discovery_rssi);
                    if !ble::is_connected(p).await {
                        bail!("peripheral reports disconnected");
                    }

                    // If the notify stream dropped (a transient backend hiccup on a
                    // live link, or a prior disconnect), try to restore it — paced at
                    // the probe interval, so there's no tight re-subscribe loop. It's
                    // only telemetry; the GATT-read health below is unaffected.
                    if notif.is_none() {
                        if let Some(nc) = &chars.notify {
                            if let Ok(stream) = ble::subscribe_notify(p, nc).await {
                                debug!(light = %label, "notify stream re-subscribed");
                                notif = Some(stream);
                            }
                        }
                    }

                    // CONNECTION HEALTH — a cheap GATT read, answered by the radio, so
                    // it's stable and can't false-trip; it's what keeps a healthy light
                    // connected with no churn. Three consecutive misses = a genuinely
                    // dead link (weak signal / gone) → recycle. No readable char ⇒
                    // is_connected() only (checked above).
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

                    // Status telemetry (battery + temperature): logged, best-effort.
                    if let Some(mac) = query_mac {
                        send_status_queries(p, &chars.write, mac, false).await;
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
                        Some(n) => log_status(label, &n.value, &mut status),
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

/// The power frame (if any) the flush loop should send for a state transition.
/// `prev_power` is the power of the last state flushed THIS session (`None` =
/// nothing flushed yet — every session starts over, because a reconnected light
/// may have been power-cycled and its true state is unknowable over this
/// write-only protocol).
///
/// - Power-ON on any transition into `power = true`, including the first flush.
/// - Power-OFF only for an ACTIVE off (DMX- or failsafe-demanded) not already
///   sent. The passive `seed` off — the pre-ArtNet baseline of a light with
///   `power_on_connect = false` — sends NOTHING: the user said hands-off, and
///   actively powering the light off at connect (as the code once did) is the
///   opposite of that. The poweroff failsafe still lands after a reconnect:
///   its states carry `seed = false` and `prev_power` starts as `None`, so the
///   off frame goes out.
fn power_command(prev_power: Option<bool>, desired: &LightState) -> Option<bool> {
    if desired.power {
        (prev_power != Some(true)).then_some(true)
    } else if desired.seed {
        None
    } else if prev_power != Some(false) {
        Some(false)
    } else {
        None
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

/// Best-effort MAC-addressed status reads (battery/temp/firmware/state) — pure
/// telemetry. Replies land on the notify stream (see [`log_status`]). `full` adds
/// the version + state reads (used once per session); otherwise just battery +
/// temperature (polled each probe). All best-effort — a failed write is non-fatal
/// (link health is decided by the GATT-read probe), and a light that never replies
/// (e.g. the TL97C) is still fully controllable.
async fn send_status_queries(p: &Peripheral, write: &Characteristic, mac: [u8; 6], full: bool) {
    let mut frames = vec![queries::battery(mac), queries::temperature(mac)];
    if full {
        frames.push(queries::version(mac));
        frames.push(queries::state(mac));
    }
    // Spaced so the light's reply queue isn't outrun — but only BETWEEN frames.
    // A trailing sleep would just stall this select arm (and therefore the flush
    // arm alongside it) for no benefit.
    let mut it = frames.iter().peekable();
    while let Some(f) = it.next() {
        if ble::write_command(p, write, f).await.is_err() {
            debug!("status query write failed (non-fatal)");
            return;
        }
        if it.peek().is_some() {
            tokio::time::sleep(QUERY_SPACING).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(power: bool, seed: bool) -> LightState {
        LightState { power, seed, ..LightState::default() }
    }

    #[test]
    fn power_on_is_sent_on_first_flush_and_transitions_only() {
        // First flush of a power_on_connect = true seed (or any on state).
        assert_eq!(power_command(None, &state(true, false)), Some(true));
        // Unchanged on → nothing (the flush loop's skip already handles equal
        // states; this guards the case where only non-power fields changed).
        assert_eq!(power_command(Some(true), &state(true, false)), None);
        // Off → on transition (DMX resuming after a poweroff failsafe).
        assert_eq!(power_command(Some(false), &state(true, false)), Some(true));
    }

    #[test]
    fn passive_seed_off_sends_nothing() {
        // power_on_connect = false: the pre-ArtNet baseline must not touch the
        // light's power — the old code actively sent power-OFF here, switching
        // off a light the user had turned on manually.
        assert_eq!(power_command(None, &state(false, true)), None);
        // ...and stays silent however often the seed state is re-examined
        // (e.g. after a blackout failsafe mutated only its brightness).
        assert_eq!(power_command(Some(false), &state(false, true)), None);
    }

    #[test]
    fn active_off_is_sent_even_on_first_flush() {
        // The poweroff failsafe fired while this light was disconnected; on
        // reconnect the first flushed state is an ACTIVE off (seed = false)
        // and the off frame must go out — this is the corner that forbids a
        // plain "only send off on an observed on→off transition" rule.
        assert_eq!(power_command(None, &state(false, false)), Some(false));
        // On → off transition (failsafe firing mid-session).
        assert_eq!(power_command(Some(true), &state(false, false)), Some(false));
        // Already sent off → don't repeat.
        assert_eq!(power_command(Some(false), &state(false, false)), None);
    }

    #[test]
    fn mapped_dmx_states_are_never_seeds() {
        // map_dmx output must always be an active state — if this ever regressed,
        // a DMX-driven light could be mistaken for a hands-off seed.
        let st = crate::profile::map_dmx(
            Profile::Rgb,
            &[255, 0, 0],
            crate::profile::CctRange::default(),
        );
        assert!(!st.seed);
        assert!(st.power);
    }
}
