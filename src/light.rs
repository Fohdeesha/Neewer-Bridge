//! Per-light BLE actor. One of these runs as a tokio task for each configured
//! light, keyed by MAC. It owns that light's whole lifecycle:
//!
//! - discover (from the shared scan) → connect → verify Neewer GATT,
//! - power on (if configured),
//! - **coalescing flush:** at `flush_hz`, send the latest desired `LightState`
//!   only if it changed (handles the ArtNet-44Hz → BLE rate mismatch),
//! - **stale-session detection (NOTES.md §5):** periodic non-mutating read
//!   probe; after repeated failures, recycle the connection,
//! - reconnect with backoff, indefinitely.
//!
//! Because binding is by MAC and the actor exists for the whole process
//! lifetime, the DMX→light mapping is stable regardless of power-on/discovery
//! order — a light that's currently absent simply keeps retrying.

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

/// How long a liveness probe may take before it counts as a failure (§5).
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
/// Consecutive probe failures before we declare the session stale and recycle.
const MAX_PROBE_FAILURES: u32 = 3;
/// How often to poll the shared scan while waiting for the light to appear.
const FIND_POLL: Duration = Duration::from_secs(2);
/// Backoff after a failed/ended session before reconnecting.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(4);

pub struct LightActor {
    cfg: LightCfg,
    adapter: Adapter,
    rx: watch::Receiver<LightState>,
    flush_hz: u32,
    probe_secs: u64,
}

impl LightActor {
    pub fn new(
        cfg: LightCfg,
        adapter: Adapter,
        rx: watch::Receiver<LightState>,
        flush_hz: u32,
        probe_secs: u64,
    ) -> Self {
        Self { cfg, adapter, rx, flush_hz, probe_secs }
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
                    if let Err(e) = self.session(&label, &peripheral, &chars, &driver).await {
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
    ) -> Result<()> {
        let flush_ms = (1000 / self.flush_hz.max(1)).max(1) as u64;
        let mut flush = interval(Duration::from_millis(flush_ms));
        flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut probe = interval(Duration::from_secs(self.probe_secs.max(1)));
        probe.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The first `interval` tick fires immediately; skip an instant probe so
        // we don't probe before the link has settled.
        probe.tick().await;

        let read_char = ble::find_readable_char(p);
        if read_char.is_none() {
            warn!(light = %label, "no readable characteristic; stale-session detection degraded to is_connected() only");
        }

        let mut last_sent: Option<LightState> = None;
        let mut failures: u32 = 0;
        let rssi0 = ble::rssi(p).await;
        info!(
            light = %label,
            flush_hz = self.flush_hz,
            probe_secs = self.probe_secs,
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
            send_status_queries(p, &chars.write, mac, true).await;
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
                    if let Some(rc) = &read_char {
                        if ble::probe_read(p, rc, PROBE_TIMEOUT).await {
                            if failures > 0 {
                                debug!(light = %label, "liveness restored");
                            }
                            failures = 0;
                            debug!(light = %label, rssi = ?rssi, "liveness ok");
                        } else {
                            failures += 1;
                            warn!(light = %label, failures, rssi = ?rssi, "liveness probe failed");
                            if failures >= MAX_PROBE_FAILURES {
                                bail!("stale session: {failures} consecutive probe failures");
                            }
                        }
                    } else {
                        debug!(light = %label, rssi = ?rssi, "alive (is_connected; no readable char)");
                    }
                    // Refresh battery + temperature alongside the liveness probe; the
                    // replies arrive on the notify stream and are logged there.
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
                            debug!(light = %label, "notify stream ended");
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

/// Best-effort MAC-addressed status queries. Replies land on the notify stream (see
/// [`log_status`]). `full` adds the version + state reads (used once per session);
/// otherwise just battery + temperature (polled). Failures are non-fatal — the
/// liveness probe owns link health, not these reads.
async fn send_status_queries(p: &Peripheral, write: &Characteristic, mac: [u8; 6], full: bool) {
    let mut frames = vec![queries::battery(mac), queries::temperature(mac)];
    if full {
        frames.push(queries::version(mac));
        frames.push(queries::state(mac));
    }
    for f in &frames {
        if ble::write_command(p, write, f).await.is_err() {
            debug!("status query write failed (non-fatal)");
            return;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
}
