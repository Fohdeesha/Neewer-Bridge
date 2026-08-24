//! `run` orchestration: spawn one BLE actor per configured light, start the
//! shared scan, feed mapped ArtDmx into each light's `watch` channel, and run
//! the ArtNet-loss failsafe.
//!
//! Data flow:
//!   ArtNet UDP → parse → per-universe lookup → map_dmx → watch::Sender
//!                                                          ↓ (coalesced)
//!                                            per-light actor → BLE write
//!
//! The `watch` channel coalesces for free: a fast ArtNet stream only ever leaves
//! the *latest* `LightState` for the actor to read at its flush rate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};

use crate::ble;
use crate::config::{Config, FailsafeMode};
use crate::light::LightActor;
use crate::merge;
use crate::profile::{extract_slice, map_dmx, CctRange, Profile};
use crate::protocol::LightState;
use crate::scan;

/// One DMX consumer: where a light lives in a universe and how to push to it.
struct Sink {
    label: String,
    address: u16,
    profile: Profile,
    cct: CctRange,
    tx: watch::Sender<LightState>,
    /// Whether this light's channels currently fall outside the DMX data being
    /// received (see [`Sink::dispatch`]). Latched so the condition is logged on
    /// transition, not once per packet.
    starved: AtomicBool,
}

impl Sink {
    /// Map this light's slice out of a merged universe and push it to the actor.
    ///
    /// A light patched past the end of the incoming DMX data gets no update at
    /// all — it just sits at its last state. That is silent by nature (an
    /// addressing/console mistake looks exactly like a dead fixture), so the
    /// condition is logged on entry and on recovery. Same spirit as the
    /// per-light `configuring light` startup line: a mis-patched light must
    /// never be invisible in the log.
    fn dispatch(&self, data: &[u8]) {
        let count = self.profile.channel_count();
        let Some(slice) = extract_slice(data, self.address, count) else {
            if !self.starved.swap(true, Ordering::Relaxed) {
                warn!(
                    light = %self.label,
                    channels = %format!("{}-{}", self.address, self.address + count - 1),
                    received = data.len(),
                    "light's DMX channels are past the end of the received data — \
                     it will hold its last state (check the light's `address` or the \
                     console's universe size)"
                );
            }
            return;
        };
        if self.starved.swap(false, Ordering::Relaxed) {
            info!(light = %self.label, "DMX data now covers this light's channels again");
        }
        // Ignore send errors: a downed actor has no receiver; it reads the
        // latest value when it reconnects.
        let _ = self.tx.send(map_dmx(self.profile, slice, self.cct));
    }
}

pub async fn run(cfg: Config) -> Result<()> {
    // `Config::load` already validates, but `run` is a public entry point and
    // everything below relies on those invariants (profiles parse, MACs parse,
    // channels fit the universe) — the per-light setup and each actor `expect()`
    // them. Re-checking here is idempotent and cheap, and turns a would-be panic
    // in a spawned task into a plain startup error.
    cfg.validate().context("invalid bridge configuration")?;

    if cfg.lights.is_empty() {
        warn!("no [[lights]] configured — the bridge will receive ArtNet but drive nothing");
    }

    // Bind every ArtNet input FIRST, before touching BLE or spawning anything:
    // a bind failure (port already in use, bad bind IP) must be a fatal startup
    // error with a non-zero exit — under a supervisor, warn-and-exit-0 is a
    // silent outage. Input 0 is the primary [artnet] bind_ip/port; each
    // [[artnet.inputs]] adds another listener, merged per channel (merge.rs —
    // bind_inputs is the same setup path `monitor` uses).
    let (bound, merger) = merge::bind_inputs(&cfg.artnet).await?;
    let inputs_cfg = cfg.artnet.resolved_inputs();

    let adapter = ble::acquire_adapter(&cfg.ble.adapter).await?;
    // Discovery scanning is coordinated (scan.rs), not permanently on: the bridge
    // scans only while a light is disconnected, in duty-cycled bursts, and not at
    // all once every light is connected. A continuous scan starves the active
    // connections on a cheap USB controller and makes the kernel log
    // `LE Set Scan Enable` timeouts (see scan.rs).
    let scan = scan::ScanCoordinator::new();
    tokio::spawn(scan::run(
        adapter.clone(),
        scan.clone(),
        Duration::from_secs(cfg.ble.scan_window_secs),
        Duration::from_secs(cfg.ble.scan_pause_secs),
    ));
    info!(
        scan_window_secs = cfg.ble.scan_window_secs,
        scan_pause_secs = cfg.ble.scan_pause_secs,
        "BLE discovery-scan coordinator started (scans only while a light is disconnected)"
    );

    // Build the universe → sinks map (+ a flat list for the failsafe task) and
    // spawn one actor per light. Sinks are shared via Arc between the ArtNet
    // listener (lookup) and the failsafe task (push to all).
    let mut universe_map: HashMap<u16, Vec<Arc<Sink>>> = HashMap::new();
    let mut all_sinks: Vec<Arc<Sink>> = Vec::new();
    for light in &cfg.lights {
        let profile = Profile::parse(&light.profile).expect("validated profile");
        // Log the resolved personality per light at startup so the run log alone
        // shows what's driving each fixture — a light silently on the wrong profile
        // (e.g. `advanced`/`cct` instead of `rgb`) renders white and ignores colour,
        // and this line makes that obvious without a separate `lights` command.
        let last_ch = light.address + profile.channel_count() - 1;
        let channels = format!("{}-{}", light.address, last_ch);
        // Same label the per-light actor logs under, so a starved-channel warning
        // lines up with that light's connect/session lines.
        let label = light
            .name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or(&light.mac)
            .to_string();
        info!(
            name = light.name.as_deref().filter(|n| !n.is_empty()).unwrap_or("(unnamed)"),
            mac = %light.mac,
            profile = %light.profile,
            universe = light.universe,
            channels = %channels,
            "configuring light"
        );
        // Seed initial power from power_on_connect: the actor sends this as soon
        // as it connects, before any ArtNet arrives. Until the first ArtDmx for
        // this light lands, that seed is the deterministic startup baseline
        // (LightState::default() = CCT 3200K @ 50%) — the locked §5.4 "defined
        // startup state" decision. With a console streaming (openHAB refreshes
        // at ~1.2 Hz) the baseline is visible for well under a second; it only
        // persists if the bridge restarts while the ArtNet source is down.
        let initial = LightState { power: light.power_on_connect, ..LightState::default() };
        let (tx, rx) = watch::channel(initial);
        let cct = CctRange { min: light.cct_min, max: light.cct_max };
        let sink = Arc::new(Sink {
            label,
            address: light.address,
            profile,
            cct,
            tx,
            starved: AtomicBool::new(false),
        });
        universe_map.entry(light.universe).or_default().push(sink.clone());
        all_sinks.push(sink);

        let actor = LightActor::new(
            light.clone(),
            adapter.clone(),
            rx,
            cfg.ble.flush_hz,
            cfg.ble.probe_secs,
            scan.clone(),
        );
        tokio::spawn(actor.run());
    }

    // Shared "last ArtNet packet" timestamp, as millis since `base`.
    let base = Instant::now();
    let last_artnet = Arc::new(AtomicU64::new(0));

    spawn_failsafe(&cfg, base, last_artnet.clone(), all_sinks);

    // ArtNet listeners + merge/dispatch pump (merge.rs) — receives on every
    // input, sequence-filters per input, merges per channel, and pushes the
    // merged universes into the light sinks. The sockets were bound above, so
    // the only way this task ends is a receive error — which the select! below
    // treats as fatal.
    let last_for_listener = last_artnet.clone();
    let listener = tokio::spawn(async move {
        merge::serve_all(
            bound,
            merger,
            // Any ArtDmx on any input (even a stale one) proves a source is
            // alive, so it feeds the failsafe timer before the sequence check.
            move |_idx, _label, _src, _pkt| {
                last_for_listener.store(base.elapsed().as_millis() as u64, Ordering::Relaxed);
            },
            move |port_address, data, _changed| {
                // Dispatch even when the merge is unchanged: the failsafe task
                // mutates sink state out-of-band (blackout/poweroff), and an
                // unconditional re-dispatch of the resumed — possibly
                // identical — stream is what overwrites it back to normal.
                // The per-light actor already skips unchanged BLE writes.
                if let Some(sinks) = universe_map.get(&port_address) {
                    for s in sinks {
                        s.dispatch(data);
                    }
                }
            },
        )
        .await
    });

    let inputs_desc = inputs_cfg
        .iter()
        .map(|i| format!("{}={}:{}", i.label, i.bind_ip, i.port))
        .collect::<Vec<_>>()
        .join(", ");
    info!(
        lights = cfg.lights.len(),
        inputs = %inputs_desc,
        merge = %if inputs_cfg.len() > 1 {
            cfg.artnet.merge.as_str()
        } else {
            "n/a (single input)"
        },
        failsafe = %cfg.failsafe.mode,
        "bridge running — press Ctrl-C to stop"
    );

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl-C received — shutting down");
        }
        res = listener => {
            // The receive loop never returns Ok; any exit here is a failure and
            // must surface as one (non-zero exit) so a supervisor restarts us.
            let err = match res {
                Ok(Ok(())) => anyhow::anyhow!("ArtNet listener ended unexpectedly"),
                Ok(Err(e)) => e.context("ArtNet listener failed"),
                Err(join_err) => anyhow::Error::from(join_err).context("ArtNet listener task panicked"),
            };
            return Err(err);
        }
    }
    info!("shutdown: failsafe = {} (lights keep their last commanded state)", cfg.failsafe.mode);
    Ok(())
}

/// How a failsafe mode mutates a light's state, or `None` for `hold` (nothing to
/// do). Each mutator returns whether it actually changed anything, which is what
/// `watch::Sender::send_if_modified` uses to decide whether to wake the actor —
/// so a failsafe that is already applied costs nothing on subsequent ticks.
fn failsafe_action(mode: FailsafeMode) -> Option<fn(&mut LightState) -> bool> {
    match mode {
        FailsafeMode::Hold => None,
        FailsafeMode::Blackout => Some(|st: &mut LightState| {
            let changed = st.brightness != 0;
            st.brightness = 0;
            changed
        }),
        FailsafeMode::PowerOff => Some(|st: &mut LightState| {
            let changed = st.power;
            st.power = false;
            changed
        }),
    }
}

/// Spawn the ArtNet-loss failsafe task, unless the mode is `hold` or no timeout
/// is set. While idle past the timeout, it forces blackout (brightness 0) or
/// power-off on every light; normal ArtNet resumes immediately overwrite it.
///
/// The mode is parsed once here rather than re-matched per tick, so the loop
/// can't silently no-op on an unrecognised string.
fn spawn_failsafe(cfg: &Config, base: Instant, last_artnet: Arc<AtomicU64>, sinks: Vec<Arc<Sink>>) {
    // Validated at config load; `hold` is also the right fallback for anything
    // unexpected (do nothing rather than blackout a rig on a typo).
    let mode = FailsafeMode::parse(&cfg.failsafe.mode).unwrap_or(FailsafeMode::Hold);
    let timeout_ms = cfg.failsafe.timeout_secs.saturating_mul(1000);
    let Some(action) = failsafe_action(mode) else { return };
    if timeout_ms == 0 {
        warn!(%mode, "failsafe.timeout_secs = 0 → behaves like 'hold'");
        return;
    }

    tokio::spawn(async move {
        let mut tick = interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut announced = false;
        loop {
            tick.tick().await;
            let idle_ms = (base.elapsed().as_millis() as u64)
                .saturating_sub(last_artnet.load(Ordering::Relaxed));
            if idle_ms < timeout_ms {
                announced = false;
                continue;
            }
            if !announced {
                warn!(%mode, idle_secs = idle_ms / 1000, "ArtNet lost — applying failsafe");
                announced = true;
            }
            for s in &sinks {
                // send_if_modified only notifies the actor on an actual change,
                // so a held failsafe costs nothing after the first tick.
                s.tx.send_if_modified(action);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Mode;

    fn sink(address: u16, profile: Profile) -> (Sink, watch::Receiver<LightState>) {
        let (tx, rx) = watch::channel(LightState::default());
        let s = Sink {
            label: "test".into(),
            address,
            profile,
            cct: CctRange::default(),
            tx,
            starved: AtomicBool::new(false),
        };
        (s, rx)
    }

    #[test]
    fn dispatch_maps_the_light_slice() {
        // `rgb` at address 4 reads channels 4-6 = pure green.
        let (s, rx) = sink(4, Profile::Rgb);
        s.dispatch(&[0, 0, 0, 0, 255, 0]);
        let st = *rx.borrow();
        assert_eq!(st.mode, Mode::Hsi);
        assert_eq!((st.hue, st.sat, st.brightness), (120, 100, 100));
        assert!(!s.starved.load(Ordering::Relaxed));
    }

    #[test]
    fn dispatch_holds_and_flags_when_channels_are_past_the_data() {
        // A light at ch26-28 fed a universe that only carries 12 channels gets
        // no update at all — the condition must latch (so it's logged once).
        let (s, rx) = sink(26, Profile::Rgb);
        s.dispatch(&[255; 12]);
        assert!(s.starved.load(Ordering::Relaxed), "short data should latch the flag");
        assert_eq!(*rx.borrow(), LightState::default(), "state must be untouched");

        // Still short: stays latched (no repeat logging), still no update.
        s.dispatch(&[255; 20]);
        assert!(s.starved.load(Ordering::Relaxed));
        assert_eq!(*rx.borrow(), LightState::default());

        // Data grows to cover the light: flag clears and the state flows again.
        let mut full = vec![0u8; 28];
        full[25] = 255; // ch26 = red
        s.dispatch(&full);
        assert!(!s.starved.load(Ordering::Relaxed), "recovery should clear the flag");
        assert_eq!(rx.borrow().brightness, 100);
    }

    #[test]
    fn failsafe_actions_are_edge_triggered() {
        // The mutators must report "changed" only on the first application, so a
        // failsafe that is already applied stops waking the per-light actor.
        assert!(failsafe_action(FailsafeMode::Hold).is_none());

        let blackout = failsafe_action(FailsafeMode::Blackout).unwrap();
        let mut st = LightState { brightness: 80, power: true, ..LightState::default() };
        assert!(blackout(&mut st));
        assert_eq!(st.brightness, 0);
        assert!(!blackout(&mut st));
        assert!(st.power, "blackout must not cut power — the light stays connected");

        let poweroff = failsafe_action(FailsafeMode::PowerOff).unwrap();
        let mut st = LightState { brightness: 80, power: true, ..LightState::default() };
        assert!(poweroff(&mut st));
        assert!(!st.power);
        assert!(!poweroff(&mut st));
        assert_eq!(st.brightness, 80, "poweroff must not touch brightness");
    }

    #[tokio::test]
    async fn run_rejects_an_invalid_config_instead_of_panicking() {
        // `run` is a public entry point; an unvalidated Config used to reach the
        // per-light `expect()`s and panic inside a spawned task.
        let mut cfg = Config::default();
        cfg.lights.push(crate::config::LightCfg {
            mac: "not-a-mac".into(),
            name: None,
            driver: "auto".into(),
            profile: "rgb".into(),
            universe: 0,
            address: 1,
            power_on_connect: true,
            cct_min: 32,
            cct_max: 56,
            cmd_type: 2,
        });
        let err = run(cfg).await.expect_err("invalid config must be an error");
        assert!(format!("{err:#}").contains("invalid bridge configuration"));
    }
}
