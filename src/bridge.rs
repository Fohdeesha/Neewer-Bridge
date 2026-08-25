//! `run` orchestration: spawn one BLE actor per configured light, start the
//! discovery-scan coordinator (which scans only while a light is missing — see
//! `scan.rs`), feed mapped ArtDmx into each light's `watch` channel, and run the
//! ArtNet-loss failsafe.
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
use tokio::task::JoinSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};

use crate::ble;
use crate::config::{Config, FailsafeMode};
use crate::light::LightActor;
use crate::merge;
use crate::profile::{extract_slice, map_dmx, CctRange, Profile};
use crate::protocol::LightState;
use crate::scan;

/// Names for the supervised background tasks, used in the fatal error when one
/// of them ends (see the `background` JoinSet in [`run`]).
const SCAN_TASK: &str = "BLE discovery-scan coordinator";
const FAILSAFE_TASK: &str = "ArtNet-loss failsafe";

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
    // Long-lived background tasks, supervised in the same select! as the light
    // actors. Both of these loop forever, so ANY completion — a return or a
    // panic — is a failure, and an UNSUPERVISED one would be silent: a dead scan
    // coordinator means no light is ever discovered again (every disconnected
    // light waits forever) and a dead failsafe means the rig never goes safe,
    // both while the bridge carries on logging as if healthy. That is exactly the
    // failure mode the actor and listener supervision below exists to prevent, so
    // these get the same treatment. The payload is the task's name, for the error.
    let mut background: JoinSet<&'static str> = JoinSet::new();
    {
        let adapter = adapter.clone();
        let scan = scan.clone();
        let window = Duration::from_secs(cfg.ble.scan_window_secs);
        let pause = Duration::from_secs(cfg.ble.scan_pause_secs);
        background.spawn(async move {
            scan::run(adapter, scan, window, pause).await;
            SCAN_TASK
        });
    }
    info!(
        scan_window_secs = cfg.ble.scan_window_secs,
        scan_pause_secs = cfg.ble.scan_pause_secs,
        "BLE discovery-scan coordinator started (scans only while a light is disconnected)"
    );

    // Build the universe → sinks map and spawn one actor per light. Sinks are
    // shared via Arc between the ArtNet listener (lookup) and the failsafe task
    // (which pushes to the sinks of whichever universe went quiet).
    let mut universe_map: HashMap<u16, Vec<Arc<Sink>>> = HashMap::new();
    // Actor tasks are supervised (join errors surface below): a panicked actor
    // would otherwise leave its light silently dead - holding its last colour,
    // never reconnecting - while the bridge reports healthy. LightActor::run
    // loops forever, so ANY join is a failure.
    let mut actors: JoinSet<String> = JoinSet::new();
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
        // `seed` marks the power_on_connect = false baseline as PASSIVE: the
        // actor then leaves the light's power completely alone at connect
        // (previously it actively sent power-OFF — turning off a light the user
        // had switched on manually, the opposite of "don't touch it"). The
        // first real DMX state replaces the seed and normal power handling
        // takes over. With power_on_connect = true the seed flag stays false so
        // every failsafe/reconnect corner behaves exactly as before.
        let initial = LightState {
            power: light.power_on_connect,
            seed: !light.power_on_connect,
            ..LightState::default()
        };
        let (tx, rx) = watch::channel(initial);
        let cct = CctRange { min: light.cct_min, max: light.cct_max };
        let sink = Arc::new(Sink {
            label: label.clone(),
            address: light.address,
            profile,
            cct,
            tx,
            starved: AtomicBool::new(false),
        });
        universe_map.entry(light.universe).or_default().push(sink);

        let actor = LightActor::new(
            light.clone(),
            adapter.clone(),
            rx,
            cfg.ble.flush_hz,
            cfg.ble.probe_secs,
            scan.clone(),
        );
        actors.spawn(async move {
            actor.run().await;
            label
        });
    }

    // Failsafe bookkeeping: one "last ArtDmx seen" clock per CONFIGURED
    // universe, as millis since `base`. Per universe, not global, so a live
    // source on one universe can't hold the failsafe off for lights patched to
    // another; and keyed by configured universe, so foreign ArtNet on a shared
    // lighting LAN (a console broadcasting universes we drive nothing on) is
    // ignored entirely rather than counting as "signal present".
    let base = Instant::now();
    let clocks: HashMap<u16, Arc<AtomicU64>> =
        universe_map.keys().map(|&u| (u, Arc::new(AtomicU64::new(0)))).collect();
    let failsafe_universes: Vec<UniverseClock> = universe_map
        .iter()
        .map(|(&universe, sinks)| UniverseClock {
            universe,
            last_seen: clocks[&universe].clone(),
            sinks: sinks.clone(),
        })
        .collect();

    spawn_failsafe(&mut background, &cfg, base, failsafe_universes);

    // ArtNet listeners + merge/dispatch pump (merge.rs) — receives on every
    // input, sequence-filters per input, merges per channel, and pushes the
    // merged universes into the light sinks. The sockets were bound above, so
    // the only way this task ends is a receive error — which the select! below
    // treats as fatal.
    let listener = tokio::spawn(async move {
        merge::serve_all(
            bound,
            merger,
            // ArtDmx on any input (even a stale one) proves that universe's
            // source is alive, so it feeds the failsafe clock before the
            // sequence check.
            move |_idx, _label, _src, pkt| {
                note_artnet(&clocks, pkt.port_address, base.elapsed().as_millis() as u64);
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
        // A light actor ending is impossible in normal operation (run() loops
        // forever) - a join here means the task panicked or was killed. That
        // light would be invisibly dead for the rest of the process, so treat
        // it like a dead ArtNet listener: fatal, supervisor-visible.
        Some(res) = actors.join_next() => {
            let err = match res {
                Ok(light) => anyhow::anyhow!("light actor for {light} exited unexpectedly"),
                Err(join_err) => anyhow::Error::from(join_err).context("a light actor panicked"),
            };
            return Err(err);
        }
        // Same rule for the background tasks (scan coordinator, failsafe): both
        // loop forever, so any join is a panic or an impossible early return.
        // Left unsupervised these die silently — no discovery ever again, or a
        // rig that never goes safe — while everything else keeps running.
        Some(res) = background.join_next() => {
            let err = match res {
                Ok(name) => anyhow::anyhow!("{name} task exited unexpectedly"),
                Err(join_err) => anyhow::Error::from(join_err).context("a background task panicked"),
            };
            return Err(err);
        }
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

/// One configured universe's failsafe bookkeeping: when ArtDmx for it was last
/// seen (millis since the bridge's `base` instant) and the lights it drives.
struct UniverseClock {
    universe: u16,
    last_seen: Arc<AtomicU64>,
    sinks: Vec<Arc<Sink>>,
}

/// Record the arrival of ArtDmx for `port_address` against the per-universe
/// failsafe clocks.
///
/// Traffic for a universe no light is patched to is **ignored**: ArtNet is
/// routinely broadcast, so on a shared lighting LAN a console streaming
/// universes this bridge drives nothing on would otherwise hold the failsafe
/// off forever — the failsafe would simply never fire.
fn note_artnet(clocks: &HashMap<u16, Arc<AtomicU64>>, port_address: u16, elapsed_ms: u64) {
    if let Some(clock) = clocks.get(&port_address) {
        clock.store(elapsed_ms, Ordering::Relaxed);
    }
}

/// Spawn the ArtNet-loss failsafe task, unless the mode is `hold`, no timeout is
/// set, or no lights are configured. Each universe is timed independently: when
/// one goes quiet past the timeout, the lights patched to *that* universe are
/// forced to blackout (brightness 0) or power-off, while universes still
/// receiving data carry on untouched. Resumed ArtNet immediately overwrites it.
///
/// Each universe's idle clock starts at PROCESS START, not at its first received
/// packet: a bridge that boots with the console already down applies the failsafe
/// `timeout_secs` after startup, exactly as if the signal had just been lost.
/// Deliberate - "no signal for N seconds" should mean the same thing whether
/// the signal vanished before or after the bridge started (and a deterministic
/// blackout beats indefinitely holding the connect-time baseline). Documented
/// at [failsafe] in config.example.toml.
///
/// The mode is parsed once here rather than re-matched per tick, so the loop
/// can't silently no-op on an unrecognised string.
///
/// Spawned into `tasks` rather than detached, so a panic in the loop is fatal and
/// supervisor-visible instead of silently disarming the failsafe for the rest of
/// the process (see the `background` JoinSet in [`run`]). The three early returns
/// below are the legitimate "nothing to run" cases and spawn no task at all.
fn spawn_failsafe(
    tasks: &mut JoinSet<&'static str>,
    cfg: &Config,
    base: Instant,
    universes: Vec<UniverseClock>,
) {
    // Validated at config load; `hold` is also the right fallback for anything
    // unexpected (do nothing rather than blackout a rig on a typo).
    let mode = FailsafeMode::parse(&cfg.failsafe.mode).unwrap_or(FailsafeMode::Hold);
    let timeout_ms = cfg.failsafe.timeout_secs.saturating_mul(1000);
    let Some(action) = failsafe_action(mode) else { return };
    if timeout_ms == 0 {
        warn!(%mode, "failsafe.timeout_secs = 0 → behaves like 'hold'");
        return;
    }
    if universes.is_empty() {
        return; // no lights ⇒ nothing to fail safe
    }

    tasks.spawn(async move {
        failsafe_loop(mode, timeout_ms, base, universes, action).await;
        FAILSAFE_TASK
    });
}

/// The failsafe tick loop. Never returns — extracted from [`spawn_failsafe`] so
/// the spawned future has a concrete `&'static str` output for the supervisor.
async fn failsafe_loop(
    mode: FailsafeMode,
    timeout_ms: u64,
    base: Instant,
    universes: Vec<UniverseClock>,
    action: fn(&mut LightState) -> bool,
) {
    let mut tick = interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut announced = vec![false; universes.len()];
    loop {
        tick.tick().await;
        let now_ms = base.elapsed().as_millis() as u64;
        for (i, u) in universes.iter().enumerate() {
            let idle_ms = now_ms.saturating_sub(u.last_seen.load(Ordering::Relaxed));
            if idle_ms < timeout_ms {
                announced[i] = false;
                continue;
            }
            if !announced[i] {
                warn!(
                    %mode,
                    universe = u.universe,
                    lights = u.sinks.len(),
                    idle_secs = idle_ms / 1000,
                    "ArtNet lost for this universe — applying failsafe"
                );
                announced[i] = true;
            }
            for s in &u.sinks {
                // send_if_modified only notifies the actor on an actual change,
                // so a held failsafe costs nothing after the first tick.
                s.tx.send_if_modified(action);
            }
        }
    }
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
    fn artnet_for_an_unconfigured_universe_does_not_feed_the_failsafe() {
        // ArtNet is routinely broadcast. Before this, ANY ArtDmx reaching the
        // port reset the (then global) idle clock, so a console streaming
        // universes we drive nothing on kept the failsafe from ever firing.
        let clocks: HashMap<u16, Arc<AtomicU64>> =
            [(0u16, Arc::new(AtomicU64::new(0)))].into_iter().collect();

        note_artnet(&clocks, 0, 5_000);
        assert_eq!(clocks[&0].load(Ordering::Relaxed), 5_000);

        note_artnet(&clocks, 9999, 9_000);
        assert_eq!(clocks[&0].load(Ordering::Relaxed), 5_000, "foreign universe must not count");
        assert_eq!(clocks.len(), 1, "an unconfigured universe must not be tracked");
    }

    #[test]
    fn each_universe_keeps_its_own_idle_clock() {
        // Lights on two universes: traffic on one must not vouch for the other.
        let clocks: HashMap<u16, Arc<AtomicU64>> =
            [(0u16, Arc::new(AtomicU64::new(0))), (1u16, Arc::new(AtomicU64::new(0)))]
                .into_iter()
                .collect();

        note_artnet(&clocks, 0, 7_000);
        assert_eq!(clocks[&0].load(Ordering::Relaxed), 7_000);
        assert_eq!(clocks[&1].load(Ordering::Relaxed), 0, "universe 1 has heard nothing");
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
    async fn failsafe_fires_per_universe_not_globally() {
        // Two universes, one still receiving and one gone quiet. The live one
        // must NOT vouch for the dead one: only the dead universe's lights
        // black out. (Real time — the task reads std::time::Instant, which
        // tokio's clock control does not drive.)
        let mut cfg = Config::default();
        cfg.failsafe.mode = "blackout".into();
        cfg.failsafe.timeout_secs = 1;

        let (live, live_rx) = sink(1, Profile::Rgb);
        let (dead, dead_rx) = sink(1, Profile::Rgb);
        // Start both lit, as a mapped DMX state would.
        live.tx.send(LightState { brightness: 80, power: true, ..LightState::default() }).unwrap();
        dead.tx.send(LightState { brightness: 80, power: true, ..LightState::default() }).unwrap();

        let base = Instant::now();
        let live_clock = Arc::new(AtomicU64::new(0));
        let universes = vec![
            UniverseClock { universe: 0, last_seen: live_clock.clone(), sinks: vec![Arc::new(live)] },
            UniverseClock {
                universe: 1,
                last_seen: Arc::new(AtomicU64::new(0)),
                sinks: vec![Arc::new(dead)],
            },
        ];
        let mut tasks: JoinSet<&'static str> = JoinSet::new();
        spawn_failsafe(&mut tasks, &cfg, base, universes);

        // Keep universe 0's source "alive" for well past the timeout.
        for _ in 0..18 {
            note_artnet(
                &[(0u16, live_clock.clone())].into_iter().collect(),
                0,
                base.elapsed().as_millis() as u64,
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        assert_eq!(live_rx.borrow().brightness, 80, "a universe still receiving must be untouched");
        assert_eq!(dead_rx.borrow().brightness, 0, "the silent universe's lights must black out");
    }

    #[tokio::test]
    async fn spawn_failsafe_registers_a_supervised_task_only_when_armed() {
        // The failsafe must be SUPERVISED, not detached: a panic in a detached
        // task silently disarms the failsafe for the rest of the process while
        // the bridge keeps reporting healthy. Pinning that it lands in the
        // caller's JoinSet is what keeps it inside `run`'s fatal select!.
        // The three "nothing to do" cases must still spawn nothing at all.
        let base = Instant::now();
        let count = |mode: &str, timeout_secs: u64, universes: usize| {
            let mut cfg = Config::default();
            cfg.failsafe.mode = mode.into();
            cfg.failsafe.timeout_secs = timeout_secs;
            let clocks: Vec<UniverseClock> = (0..universes)
                .map(|i| UniverseClock {
                    universe: i as u16,
                    last_seen: Arc::new(AtomicU64::new(0)),
                    sinks: vec![Arc::new(sink(1, Profile::Rgb).0)],
                })
                .collect();
            let mut tasks: JoinSet<&'static str> = JoinSet::new();
            spawn_failsafe(&mut tasks, &cfg, base, clocks);
            tasks.len()
        };
        assert_eq!(count("blackout", 5, 1), 1, "an armed failsafe must be supervised");
        assert_eq!(count("poweroff", 5, 2), 1, "one task covers every universe");
        assert_eq!(count("hold", 5, 1), 0, "hold has nothing to run");
        assert_eq!(count("blackout", 0, 1), 0, "timeout_secs = 0 behaves like hold");
        assert_eq!(count("blackout", 5, 0), 0, "no lights ⇒ nothing to fail safe");
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
