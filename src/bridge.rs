//! `run` orchestration: spawn one BLE actor per configured light, start the
//! shared scan, and feed mapped ArtDmx into each light's `watch` channel.
//!
//! Data flow (NOTES.md §8):
//!   ArtNet UDP → parse → per-universe lookup → map_dmx → watch::Sender
//!                                                          ↓ (coalesced)
//!                                            per-light actor → BLE write
//!
//! The `watch` channel does the coalescing for free: a fast ArtNet stream only
//! ever leaves the *latest* `LightState` for the actor to read at its flush rate.

use std::collections::HashMap;

use anyhow::Result;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::artnet::{self, ArtDmx};
use crate::ble;
use crate::config::Config;
use crate::light::LightActor;
use crate::profile::{extract_slice, map_dmx, CctRange, Profile};
use crate::protocol::LightState;

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

    let adapter = ble::acquire_adapter(&cfg.ble.adapter).await?;
    ble::start_scan(&adapter).await?;
    info!("BLE scan started (shared)");

    // Build the universe → sinks map and spawn one actor per light.
    let mut universe_map: HashMap<u16, Vec<Sink>> = HashMap::new();
    for light in &cfg.lights {
        let profile = Profile::parse(&light.profile).expect("validated profile");
        let (tx, rx) = watch::channel(LightState::default());
        universe_map.entry(light.universe).or_default().push(Sink {
            address: light.address,
            profile,
            cct: CctRange::default(),
            tx,
        });

        let actor = LightActor::new(
            light.clone(),
            adapter.clone(),
            rx,
            cfg.ble.flush_hz,
            cfg.ble.probe_secs,
        );
        tokio::spawn(actor.run());
    }

    // ArtNet listener — owns the senders; updates them as packets arrive.
    let bind_ip = cfg.artnet.bind_ip.clone();
    let port = cfg.artnet.port;
    let listener = tokio::spawn(async move {
        let res = artnet::listen(&bind_ip, port, move |_src, pkt: ArtDmx| {
            if let Some(sinks) = universe_map.get(&pkt.port_address) {
                for s in sinks {
                    if let Some(slice) =
                        extract_slice(&pkt.data, s.address, s.profile.channel_count())
                    {
                        let state = map_dmx(s.profile, slice, s.cct);
                        // Ignore send errors: a downed actor has no receiver, and
                        // it will read the latest value when it reconnects.
                        let _ = s.tx.send(state);
                    }
                }
            }
        })
        .await;
        if let Err(e) = res {
            warn!(error = %e, "ArtNet listener stopped");
        }
    });

    info!(
        lights = cfg.lights.len(),
        bind = %cfg.artnet.bind_ip,
        port = cfg.artnet.port,
        "bridge running — press Ctrl-C to stop"
    );

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl-C received — shutting down");
        }
        _ = listener => {
            warn!("ArtNet listener task ended unexpectedly");
        }
    }

    // Failsafe on shutdown. v1 only implements "hold": Neewer lights keep their
    // last commanded state when BLE drops, so there's nothing to send.
    if cfg.failsafe.mode != "hold" {
        warn!(mode = %cfg.failsafe.mode, "failsafe mode not implemented yet; treating as 'hold'");
    }
    info!("failsafe = hold: lights keep their last state");
    Ok(())
}
