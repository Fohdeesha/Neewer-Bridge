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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::artnet::{self, ArtDmx};
use crate::ble;
use crate::config::Config;
use crate::light::LightActor;
use crate::profile::{extract_slice, map_dmx, CctRange, Profile};
use crate::protocol::LightState;
use crate::scan;

/// One DMX consumer: where a light lives in a universe and how to push to it.
struct Sink {
    address: u16,
    profile: Profile,
    cct: CctRange,
    tx: watch::Sender<LightState>,
}

pub async fn run(cfg: Config) -> Result<()> {
    if cfg.lights.is_empty() {
        warn!("no [[lights]] configured — the bridge will receive ArtNet but drive nothing");
    }

    // Bind the ArtNet socket FIRST, before touching BLE or spawning anything: a
    // bind failure (port already in use, bad bind IP) must be a fatal startup
    // error with a non-zero exit — under a supervisor, warn-and-exit-0 is a
    // silent outage.
    let sock = artnet::bind(&cfg.artnet.bind_ip, cfg.artnet.port).await?;

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
        let sink = Arc::new(Sink { address: light.address, profile, cct, tx });
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

    // ArtNet listener — owns the senders; updates them as packets arrive. The
    // socket was bound above, so the only way this task ends is a receive error
    // — which the select! below treats as fatal.
    let last_for_listener = last_artnet.clone();
    let mut seq = artnet::SeqTracker::new();
    let listener = tokio::spawn(async move {
        artnet::serve(sock, move |src, pkt: ArtDmx| {
            // Any ArtDmx (even a stale one) proves the source is alive, so it
            // feeds the failsafe timer before the sequence check.
            last_for_listener.store(base.elapsed().as_millis() as u64, Ordering::Relaxed);
            // Drop out-of-order/duplicate packets (Art-Net Sequence field) so a
            // late datagram can't briefly re-apply an old state.
            if !seq.is_fresh(src.ip(), pkt.port_address, pkt.sequence) {
                debug!(%src, port = pkt.port_address, seq = pkt.sequence, "stale ArtDmx dropped");
                return;
            }
            if let Some(sinks) = universe_map.get(&pkt.port_address) {
                for s in sinks {
                    if let Some(slice) =
                        extract_slice(&pkt.data, s.address, s.profile.channel_count())
                    {
                        let state = map_dmx(s.profile, slice, s.cct);
                        // Ignore send errors: a downed actor has no receiver; it
                        // reads the latest value when it reconnects.
                        let _ = s.tx.send(state);
                    }
                }
            }
        })
        .await
    });

    info!(
        lights = cfg.lights.len(),
        bind = %cfg.artnet.bind_ip,
        port = cfg.artnet.port,
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

/// Spawn the ArtNet-loss failsafe task, unless the mode is `hold` or no timeout
/// is set. While idle past the timeout, it forces blackout (brightness 0) or
/// power-off on every light; normal ArtNet resumes immediately overwrite it.
fn spawn_failsafe(cfg: &Config, base: Instant, last_artnet: Arc<AtomicU64>, sinks: Vec<Arc<Sink>>) {
    let mode = cfg.failsafe.mode.clone();
    let timeout_ms = cfg.failsafe.timeout_secs.saturating_mul(1000);
    if mode == "hold" || timeout_ms == 0 {
        if mode != "hold" {
            warn!(mode, "failsafe.timeout_secs = 0 → behaves like 'hold'");
        }
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
                warn!(mode, idle_secs = idle_ms / 1000, "ArtNet lost — applying failsafe");
                announced = true;
            }
            for s in &sinks {
                match mode.as_str() {
                    "blackout" => {
                        s.tx.send_if_modified(|st| {
                            if st.brightness != 0 {
                                st.brightness = 0;
                                true
                            } else {
                                false
                            }
                        });
                    }
                    "poweroff" => {
                        s.tx.send_if_modified(|st| {
                            if st.power {
                                st.power = false;
                                true
                            } else {
                                false
                            }
                        });
                    }
                    _ => {}
                }
            }
        }
    });
}
