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
//!
//! [`run`] supervises all of it: any task dying, or the BLE session being judged
//! unusable by the stack watchdog, tears the whole bridge down and rebuilds it
//! after a backoff — in-process, so a bare `neewer-bridge` in a screen session
//! gets the same treatment a systemd `Restart=` would give it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use btleplug::platform::{Adapter, Manager};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use crate::ble;
use crate::config::{Config, FailsafeMode, LightCfg};
use crate::light::LightActor;
use crate::merge;
use crate::profile::{extract_slice, map_dmx, CctRange, Profile};
use crate::protocol::LightState;
use crate::recovery::{RecoveryMode, SharedHistory, StaleLinkRecovery};
use crate::scan;

/// Names for the supervised background tasks, used in the error when one of
/// them ends (see the `background` JoinSet in [`start`]).
const SCAN_TASK: &str = "BLE discovery-scan coordinator";
const FAILSAFE_TASK: &str = "ArtNet-loss failsafe";
const STACK_WATCHDOG_TASK: &str = "BLE stack watchdog";

/// The log label for each configured light, in config order.
///
/// A light's label is its `name`, or its MAC when unnamed, and EVERY per-light
/// line carries only that — `connecting`, `session active`, `power on`, the
/// starved-channel warning, a failed connection probe. Two lights may perfectly
/// legitimately share a name: `add` fills it from the model catalog, so adding
/// two TL120Cs names **both** of them `TL120C`, and the log then cannot tell
/// them apart at all. (Rejecting the config instead — the rule duplicate ArtNet
/// *input* labels get — would break that ordinary workflow, so the labels are
/// disambiguated rather than refused.)
///
/// Colliding labels gain a MAC suffix; unique ones are left exactly as written,
/// so nothing changes for the common case. The short two-octet suffix is used
/// when it is enough to separate them, and the full MAC when it is not (two
/// same-named lights can share their last two octets) — MACs are validated
/// unique, so the full form always resolves the collision.
fn light_labels(lights: &[LightCfg]) -> Vec<String> {
    fn tally(labels: &[String]) -> HashMap<&str, usize> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for l in labels {
            *counts.entry(l.as_str()).or_default() += 1;
        }
        counts
    }
    // Two octets, e.g. `EE:FF`. Built from the parsed bytes rather than by
    // slicing the string, so an unparsable MAC can't panic on a char boundary.
    fn short(mac: &str) -> String {
        match crate::config::parse_mac(mac) {
            Ok(b) => format!("{:02X}:{:02X}", b[4], b[5]),
            Err(_) => mac.to_string(),
        }
    }

    let base: Vec<String> = lights
        .iter()
        .map(|l| {
            l.name
                .as_deref()
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| crate::config::normalize_mac(&l.mac))
        })
        .collect();

    let counts = tally(&base);
    let once: Vec<String> = base
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if counts[b.as_str()] > 1 {
                format!("{b} ({})", short(&lights[i].mac))
            } else {
                b.clone()
            }
        })
        .collect();

    // Anything STILL duplicated shared its last two octets too — spell the MAC out.
    let counts = tally(&once);
    once.iter()
        .enumerate()
        .map(|(i, l)| {
            if counts[l.as_str()] > 1 {
                format!("{} ({})", base[i], crate::config::normalize_mac(&lights[i].mac))
            } else {
                l.clone()
            }
        })
        .collect()
}

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

/// Run the bridge until Ctrl-C, restarting it in-process after any failure.
///
/// The FIRST start is fatal on error (an invalid config, a port already in use,
/// no adapter — user-facing problems that must exit non-zero). Once the bridge
/// has started it never gives up: a task that dies, a BLE session the watchdog
/// judges unusable, or a failed restart is logged at ERROR and the whole bridge
/// — sockets, adapter, actors, merger, failsafe — is torn down and rebuilt
/// after a backoff ([`RestartPolicy`]). That is what a supervisor would do,
/// done in-process, because the deployment this was written for runs the
/// binary bare in a screen session: before this, a bridge whose BLE session
/// had been refused by the bus sat "up" for four days driving nothing.
pub async fn run(cfg: Config) -> Result<()> {
    // `Config::load` already validates, but `run` is a public entry point and
    // everything below relies on those invariants (profiles parse, MACs parse,
    // channels fit the universe) — the per-light setup and each actor `expect()`
    // them. Re-checking here is idempotent and cheap, and turns a would-be panic
    // in a spawned task into a plain startup error.
    cfg.validate().context("invalid bridge configuration")?;

    let mut stack = ble::BleStack::new();
    let mut policy = RestartPolicy::default();
    let mut attempt: u32 = 0;
    // Each light's last desired state, carried from one run into the next so a
    // restart does not change what the lights show (see `initial_state`).
    let mut carried: HashMap<String, LightState> = HashMap::new();
    // When the recovery ladder last power-cycled the adapter or restarted
    // bluetoothd. The process's, not a run's: a restart must not reset the
    // rate limits (see `StaleLinkRecovery::with_history`).
    let recovery_history = SharedHistory::default();
    loop {
        attempt += 1;
        let started_at = Instant::now();
        let outcome = match start(&cfg, &mut stack, attempt, &carried, &recovery_history).await {
            Ok(mut running) => {
                let res = running.serve().await;
                carried = running.light_states();
                running.shutdown().await;
                res
            }
            // The first start failing is the user's problem to fix, not ours to
            // retry: a bad config, a busy port, no adapter.
            Err(e) if attempt == 1 => return Err(e),
            Err(e) => Err(e.context("bridge restart failed")),
        };
        let err = match outcome {
            Ok(()) => return Ok(()), // Ctrl-C
            Err(e) => e,
        };
        // A poisoned platform session can only be fixed by replacing it; any
        // other failure keeps the session (opening a new one per restart would
        // leak a bus connection each time — see `ble::BleStack`).
        let poisoned =
            ble::is_bus_poisoned(&err) || ble::stack_verdict() == ble::StackVerdict::Poisoned;
        if poisoned {
            stack.rebuild();
        }
        // No adapter to restart on (unplugged, mid re-enumeration, bluetoothd
        // down): backing off as from a crash would leave the bridge up to five
        // minutes late once it is back. Wait for one to appear instead.
        if ble::is_no_adapter(&err) {
            error!(
                error = %format!("{err:#}"),
                attempt,
                rebuild_ble_session = poisoned,
                "bridge failed — no usable Bluetooth adapter; waiting for one to appear"
            );
            if !wait_for_adapter(&mut stack).await {
                return Ok(()); // Ctrl-C
            }
            continue;
        }
        let delay = policy.next_delay(started_at.elapsed());
        error!(
            error = %format!("{err:#}"),
            attempt,
            rebuild_ble_session = poisoned,
            retry_in_secs = delay.as_secs(),
            "bridge failed — restarting"
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received while waiting to restart — exiting");
                return Ok(());
            }
        }
    }
}

/// When to retry after a failure: [`RestartPolicy::MIN_DELAY`], doubling per
/// consecutive failure up to [`RestartPolicy::MAX_DELAY`], and back to the
/// minimum once a run has lasted [`RestartPolicy::HEALTHY_RUN`] (a bridge that
/// ran for an hour and then died is not in a crash loop). Pure, so the schedule
/// is pinned by a test.
#[derive(Debug, Default)]
pub struct RestartPolicy {
    consecutive: u32,
}

impl RestartPolicy {
    pub const MIN_DELAY: Duration = Duration::from_secs(5);
    pub const MAX_DELAY: Duration = Duration::from_secs(300);
    pub const HEALTHY_RUN: Duration = Duration::from_secs(600);

    /// The delay before the next attempt, given how long the last run lasted.
    pub fn next_delay(&mut self, last_run: Duration) -> Duration {
        if last_run >= Self::HEALTHY_RUN {
            self.consecutive = 0;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        let factor = 1u32.checked_shl(self.consecutive - 1).unwrap_or(u32::MAX);
        Self::MIN_DELAY.saturating_mul(factor).min(Self::MAX_DELAY)
    }
}

/// While no adapter is usable: how often the supervisor looks for one.
const ADAPTER_POLL: Duration = Duration::from_secs(5);
/// ...and how often it repeats that it is still waiting.
const ADAPTER_WAIT_REPORT: Duration = Duration::from_secs(60);

/// Wait until the platform lists a Bluetooth adapter again, polling every
/// [`ADAPTER_POLL`]. Returns `false` on Ctrl-C. Waiting is announced by the
/// caller, repeated here every [`ADAPTER_WAIT_REPORT`] (no light is driven
/// meanwhile, and that must never be invisible in the log), and its end is
/// announced too.
async fn wait_for_adapter(stack: &mut ble::BleStack) -> bool {
    let since = Instant::now();
    let mut next_report = ADAPTER_WAIT_REPORT;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(ADAPTER_POLL) => {}
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received while waiting for a Bluetooth adapter — exiting");
                return false;
            }
        }
        let counted = tokio::select! {
            res = stack.adapter_count() => res,
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received while waiting for a Bluetooth adapter — exiting");
                return false;
            }
        };
        match counted {
            Ok(n) if n > 0 => {
                info!(
                    adapters = n,
                    waited_secs = since.elapsed().as_secs(),
                    "a Bluetooth adapter is present — restarting the bridge"
                );
                return true;
            }
            Ok(_) => {}
            // The listing itself is refused: this session can never work again.
            Err(e) if ble::is_bus_poisoned(&e) => stack.rebuild(),
            Err(e) => debug!(error = %format!("{e:#}"), "still no usable Bluetooth adapter"),
        }
        if since.elapsed() >= next_report {
            next_report += ADAPTER_WAIT_REPORT;
            warn!(
                waited_secs = since.elapsed().as_secs(),
                "still waiting for a Bluetooth adapter — no light is being driven"
            );
        }
    }
}

/// Why a supervised background task ended: `reason` is set when it ended on
/// purpose (the stack watchdog giving up on the BLE session), `None` when it
/// simply returned or panicked — which for these forever-loops is a bug.
#[derive(Debug)]
struct BackgroundExit {
    task: &'static str,
    reason: Option<String>,
}

/// A started bridge: every task it owns, so it can be served and then torn down
/// completely before a restart (a leftover listener would keep the port).
struct Running {
    actors: JoinSet<String>,
    background: JoinSet<BackgroundExit>,
    listener: JoinHandle<Result<()>>,
    /// Set once the listener branch has fired, so `shutdown` does not poll a
    /// finished `JoinHandle` again.
    listener_done: bool,
    failsafe_mode: String,
    /// Each light's desired state, by normalised MAC — read when the run ends,
    /// so the next run can start every light where this one left it.
    states: Vec<(String, watch::Receiver<LightState>)>,
}

impl Running {
    /// Every light's current desired state, by normalised MAC.
    fn light_states(&self) -> HashMap<String, LightState> {
        self.states.iter().map(|(mac, rx)| (mac.clone(), *rx.borrow())).collect()
    }

    /// Serve until Ctrl-C (`Ok`) or the first task failure (`Err`).
    async fn serve(&mut self) -> Result<()> {
        tokio::select! {
            // A light actor ending is impossible in normal operation (run() loops
            // forever) - a join here means the task panicked or was killed. That
            // light would be invisibly dead for the rest of the process, so treat
            // it like a dead ArtNet listener: fatal to this run.
            Some(res) = self.actors.join_next() => {
                let err = match res {
                    Ok(light) => anyhow::anyhow!("light actor for {light} exited unexpectedly"),
                    Err(join_err) => anyhow::Error::from(join_err).context("a light actor panicked"),
                };
                Err(err)
            }
            // Same rule for the background tasks (scan coordinator, failsafe):
            // they loop forever, so any join is a panic or an impossible early
            // return. Left unsupervised these die silently — no discovery ever
            // again, or a rig that never goes safe — while everything else keeps
            // running. The stack watchdog is the exception that ends on purpose,
            // carrying its reason.
            Some(res) = self.background.join_next() => {
                let err = match res {
                    Ok(BackgroundExit { task, reason: Some(reason) }) => anyhow::anyhow!("{task}: {reason}"),
                    Ok(BackgroundExit { task, reason: None }) => anyhow::anyhow!("{task} task exited unexpectedly"),
                    Err(join_err) => anyhow::Error::from(join_err).context("a background task panicked"),
                };
                Err(err)
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received — shutting down");
                info!("shutdown: failsafe = {} (lights keep their last commanded state)", self.failsafe_mode);
                Ok(())
            }
            res = &mut self.listener => {
                self.listener_done = true;
                // The receive loop never returns Ok; any exit here is a failure.
                let err = match res {
                    Ok(Ok(())) => anyhow::anyhow!("ArtNet listener ended unexpectedly"),
                    Ok(Err(e)) => e.context("ArtNet listener failed"),
                    Err(join_err) => anyhow::Error::from(join_err).context("ArtNet listener task panicked"),
                };
                Err(err)
            }
        }
    }

    /// Tear everything down and wait for it to be gone: the listener first, so
    /// the sockets it owns are closed before a restart binds them again; then
    /// the actors and background tasks (which drop their adapter handles).
    async fn shutdown(mut self) {
        if !self.listener_done {
            self.listener.abort();
            let _ = (&mut self.listener).await;
        }
        self.actors.shutdown().await;
        self.background.shutdown().await;
    }
}

/// The adapter this run is using, for the stack watchdog's presence check.
struct AdapterWatch {
    manager: Manager,
    adapter: Adapter,
    /// The platform's id for it (`hci0`).
    id: String,
}

/// Poll the shared BLE session's health ([`ble::stack_verdict`]) and the
/// presence of the adapter this run is using ([`ble::adapter_presence`]), and
/// return a reason the moment either makes the bridge unusable. Ending this
/// task is deliberate: `serve` treats it like any other background exit, so
/// `run` tears the bridge down and rebuilds it — replacing the platform session
/// if it was poisoned, and choosing the adapter afresh. Per-light retries can
/// never make this call, because each only sees its own failure.
///
/// The presence check exists because the session can be perfectly healthy
/// while the adapter behind it has gone: a USB dongle the kernel resets comes
/// back under a new name (`hci0` → `hci1`), and every call on the old handle
/// either fails (scans) or succeeds with nothing in it (the peripheral
/// listing the stack health counts as its heartbeat) — for ever. See
/// `ble::ADAPTER_GONE_AFTER`.
async fn stack_watchdog(watch: AdapterWatch) -> String {
    let mut tick = interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut gone = ble::GoneTracker::default();
    loop {
        tick.tick().await;
        match ble::stack_verdict() {
            ble::StackVerdict::Healthy => {}
            ble::StackVerdict::Poisoned => {
                return "the BLE platform session is being refused by the D-Bus daemon \
                        (its pending-reply limit was reached) — replacing it"
                    .into();
            }
            ble::StackVerdict::Dead { failing_for } => {
                return format!(
                    "no BLE call has succeeded for {} s — rebuilding the bridge",
                    failing_for.as_secs()
                );
            }
        }
        let presence = ble::adapter_presence(&watch.manager, &watch.adapter, &watch.id).await;
        match gone.observe(&presence, Instant::now(), ble::ADAPTER_GONE_AFTER) {
            ble::GoneEdge::Steady => {
                if let ble::AdapterPresence::Unknown(why) = &presence {
                    debug!(adapter = %watch.id, reason = %why, "could not check that the Bluetooth adapter is still present");
                }
            }
            ble::GoneEdge::Vanished => warn!(
                adapter = %watch.id,
                restart_after_secs = ble::ADAPTER_GONE_AFTER.as_secs(),
                "the Bluetooth adapter has disappeared from the system (unplugged, or reset by \
                 the kernel?) — the bridge restarts on whatever adapter is present if it is not back soon"
            ),
            ble::GoneEdge::Returned { after } => info!(
                adapter = %watch.id,
                gone_secs = after.as_secs(),
                "the Bluetooth adapter is back"
            ),
            ble::GoneEdge::Confirmed { gone_for } => {
                return format!(
                    "the Bluetooth adapter {} has been gone for {} s (unplugged, or reset and \
                     re-numbered by the kernel) — rebuilding the bridge on the adapter present now",
                    watch.id,
                    gone_for.as_secs()
                );
            }
        }
    }
}

/// Start everything: bind the ArtNet inputs, acquire the adapter, spawn the
/// scan coordinator, the light actors, the failsafe, the stack watchdog and the
/// listener. Returns the running set for [`Running::serve`].
async fn start(
    cfg: &Config,
    stack: &mut ble::BleStack,
    attempt: u32,
    carried: &HashMap<String, LightState>,
    recovery_history: &SharedHistory,
) -> Result<Running> {
    if attempt > 1 {
        info!(attempt, "restarting the bridge");
    }
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

    // A fresh verdict for a fresh start: whatever the last run's session did is
    // over (and if it was poisoned, `run` has already replaced it).
    ble::reset_stack_health();
    let adapter = stack.adapter(&cfg.ble.adapter).await?;
    let manager = stack.manager().context("the BLE session closed while starting")?;
    // The platform's id for the adapter (`hci0`), for the stale-record recovery
    // and the watchdog's presence check.
    let adapter_id = ble::adapter_id(&adapter).await;
    let recovery_mode =
        RecoveryMode::parse(&cfg.ble.stale_link_recovery).expect("validated stale_link_recovery");
    let recovery = Arc::new(StaleLinkRecovery::with_history(
        recovery_mode,
        adapter_id.clone(),
        recovery_history.clone(),
    ));
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
    let mut background: JoinSet<BackgroundExit> = JoinSet::new();
    {
        let adapter = adapter.clone();
        let scan = scan.clone();
        let window = Duration::from_secs(cfg.ble.scan_window_secs);
        let pause = Duration::from_secs(cfg.ble.scan_pause_secs);
        let recovery = recovery.clone();
        background.spawn(async move {
            scan::run(adapter, scan, window, pause, recovery).await;
            BackgroundExit { task: SCAN_TASK, reason: None }
        });
    }
    // The BLE stack watchdog: the one task here that is MEANT to end — when the
    // platform session has stopped working for everyone, or the adapter has
    // gone, ending it is what makes `serve` return and `run` rebuild the bridge.
    {
        let watch = AdapterWatch { manager, adapter: adapter.clone(), id: adapter_id };
        background.spawn(async move {
            let reason = stack_watchdog(watch).await;
            BackgroundExit { task: STACK_WATCHDOG_TASK, reason: Some(reason) }
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
    let mut states = Vec::with_capacity(cfg.lights.len());
    // One label per light, disambiguated where two share a name (see
    // `light_labels`). Computed ONCE so the sink and the actor provably log
    // under the same name.
    let labels = light_labels(&cfg.lights);
    for (i, light) in cfg.lights.iter().enumerate() {
        let profile = Profile::parse(&light.profile).expect("validated profile");
        // Log the resolved personality per light at startup so the run log alone
        // shows what's driving each fixture — a light silently on the wrong profile
        // (e.g. `advanced`/`cct` instead of `rgb`) renders white and ignores colour,
        // and this line makes that obvious without a separate `lights` command.
        let last_ch = light.address + profile.channel_count() - 1;
        let channels = format!("{}-{}", light.address, last_ch);
        // Same label the per-light actor logs under, so a starved-channel warning
        // lines up with that light's connect/session lines.
        let label = labels[i].clone();
        info!(
            name = light.name.as_deref().filter(|n| !n.is_empty()).unwrap_or("(unnamed)"),
            // The label is what every later per-light line is tagged with, and it
            // is not always the configured name (see `light_labels`), so print the
            // mapping here rather than leaving the operator to infer it.
            label = %label,
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
        // On a RESTART the light instead starts where the last run left it —
        // see `initial_state`.
        let (tx, rx) = watch::channel(initial_state(light, carried));
        states.push((crate::config::normalize_mac(&light.mac), tx.subscribe()));
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
            label.clone(),
            adapter.clone(),
            rx,
            cfg.ble.flush_hz,
            cfg.ble.probe_secs,
            scan.clone(),
            recovery.clone(),
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

    spawn_failsafe(&mut background, cfg, base, failsafe_universes);

    // ArtNet listeners + merge/dispatch pump (merge.rs) — receives on every
    // input, sequence-filters per input, merges per channel, and pushes the
    // merged universes into the light sinks. The sockets were bound above, so
    // the only way this task ends is a receive error — which `Running::serve`
    // treats as fatal to this run.
    // Merge only the universes something is actually patched to. Foreign
    // broadcast traffic still reaches `on_raw` below (where `note_artnet`
    // ignores it) but never builds merge state — see `merge::Interest`.
    let interest = merge::Interest::only(universe_map.keys());
    let listener = tokio::spawn(async move {
        merge::serve_all(
            bound,
            merger,
            interest,
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
        stale_link_recovery = %recovery_mode,
        "bridge running — press Ctrl-C to stop"
    );

    Ok(Running {
        actors,
        background,
        listener,
        listener_done: false,
        failsafe_mode: cfg.failsafe.mode.clone(),
        states,
    })
}

/// The state a light starts a run in: where the previous run left it if this
/// is a restart (`carried`, by normalised MAC), else the startup baseline
/// seeded from `power_on_connect`.
///
/// A restart is the bridge's business, not the lights'. Re-seeding every light
/// with the baseline (3200K @ 50%) on every restart — as the bridge did until
/// 1.8.0 — is invisible while a console is streaming, which overwrites it within
/// a second; but with the ArtNet source down the baseline STICKS, so an adapter
/// reset or any other internal restart would change the room against
/// `[failsafe] hold`. The carried state is the last desired state including
/// anything the failsafe applied, which the failsafe then keeps applying.
fn initial_state(light: &LightCfg, carried: &HashMap<String, LightState>) -> LightState {
    carried
        .get(&crate::config::normalize_mac(&light.mac))
        .copied()
        .unwrap_or(LightState {
            power: light.power_on_connect,
            seed: !light.power_on_connect,
            ..LightState::default()
        })
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
/// the process (see the `background` JoinSet in [`start`]). The three early returns
/// below are the legitimate "nothing to run" cases and spawn no task at all.
fn spawn_failsafe(
    tasks: &mut JoinSet<BackgroundExit>,
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
        BackgroundExit { task: FAILSAFE_TASK, reason: None }
    });
}

/// What the failsafe does for one universe on one tick.
///
/// Split out as a value so the announce/release EDGES are unit-testable without
/// a clock, a subscriber, or a three-second sleep — the same convention as
/// `light::report_due`, `commands::stream_timing` and `logging::filter_directive`.
/// (An earlier version of this test captured the tracing output instead; that is
/// unreliable in a parallel suite, because `tracing` caches callsite interest
/// GLOBALLY — a sibling test that reaches the same `warn!` with no subscriber
/// installed poisons the callsite for everyone. It passed on Windows and failed
/// on Linux CI purely on test-scheduling order.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailsafeEdge {
    /// Signal present and nothing latched — do nothing at all.
    Idle,
    /// Signal is back after the failsafe had been announced — log the release.
    Released,
    /// Newly timed out — warn, then apply.
    Announce,
    /// Still timed out and already announced — keep applying, silently.
    Applied,
}

/// Decide one universe's tick. Pure.
fn failsafe_edge(idle_ms: u64, timeout_ms: u64, announced: bool) -> FailsafeEdge {
    match (idle_ms >= timeout_ms, announced) {
        (false, true) => FailsafeEdge::Released,
        (false, false) => FailsafeEdge::Idle,
        (true, false) => FailsafeEdge::Announce,
        (true, true) => FailsafeEdge::Applied,
    }
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
            let edge = failsafe_edge(idle_ms, timeout_ms, announced[i]);
            match edge {
                FailsafeEdge::Idle => continue,
                // Say so when it comes back. A latched warning with no matching
                // recovery line leaves the log claiming a fault that has since
                // cleared, and the failsafe was the only one in the bridge
                // missing its pair (`Sink::dispatch`'s channel-starvation
                // warning and `LightActor::find`'s "still not discovered" both
                // announce recovery). The `run` loop keeps going for weeks, so
                // a stale last-word warning is genuinely misleading.
                FailsafeEdge::Released => {
                    info!(
                        %mode,
                        universe = u.universe,
                        lights = u.sinks.len(),
                        "ArtNet resumed for this universe — failsafe released"
                    );
                    announced[i] = false;
                    continue;
                }
                FailsafeEdge::Announce => {
                    warn!(
                        %mode,
                        universe = u.universe,
                        lights = u.sinks.len(),
                        idle_secs = idle_ms / 1000,
                        "ArtNet lost for this universe — applying failsafe"
                    );
                    announced[i] = true;
                }
                FailsafeEdge::Applied => {}
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

    fn cfg_light(mac: &str, name: Option<&str>) -> LightCfg {
        LightCfg {
            mac: mac.into(),
            name: name.map(str::to_string),
            driver: "auto".into(),
            profile: "rgb".into(),
            universe: 0,
            address: 1,
            power_on_connect: true,
            cct_min: 32,
            cct_max: 56,
            cmd_type: 2,
        }
    }

    #[test]
    fn light_labels_disambiguate_only_when_they_collide() {
        // The common case must be untouched: distinct names stay verbatim.
        let labels = light_labels(&[
            cfg_light("AA:BB:CC:DD:EE:01", Some("Key light")),
            cfg_light("AA:BB:CC:DD:EE:02", Some("Fill")),
        ]);
        assert_eq!(labels, vec!["Key light", "Fill"]);

        // `add` fills the name from the model catalog, so adding two TL120Cs
        // names BOTH of them "TL120C" — and every per-light log line carries
        // only the label, which made them indistinguishable at runtime.
        let labels = light_labels(&[
            cfg_light("D6:50:F2:F6:BB:1B", Some("TL120C")),
            cfg_light("CB:33:5A:EE:5A:7C", Some("TL120C")),
            cfg_light("FA:F8:A9:B9:41:1C", Some("TL21C")),
        ]);
        assert_eq!(labels, vec!["TL120C (BB:1B)", "TL120C (5A:7C)", "TL21C"]);

        // Unnamed / empty-named lights fall back to their normalised MAC, which
        // config validation already guarantees is unique.
        let labels = light_labels(&[
            cfg_light("aa-bb-cc-dd-ee-01", None),
            cfg_light("AA:BB:CC:DD:EE:02", Some("")),
        ]);
        assert_eq!(labels, vec!["AA:BB:CC:DD:EE:01", "AA:BB:CC:DD:EE:02"]);

        // Same name AND same last two octets: the short suffix is not enough, so
        // the full MAC is spelled out. Without this fallback both would still
        // read "Tube (EE:FF)" and the fix would not actually fix anything.
        let labels = light_labels(&[
            cfg_light("AA:BB:CC:DD:EE:FF", Some("Tube")),
            cfg_light("11:22:33:44:EE:FF", Some("Tube")),
        ]);
        assert_eq!(
            labels,
            vec!["Tube (AA:BB:CC:DD:EE:FF)", "Tube (11:22:33:44:EE:FF)"]
        );

        // Whatever the shape, the result must be unique — that is the property
        // the whole function exists for.
        for lights in [
            vec![cfg_light("AA:BB:CC:DD:EE:01", Some("x")), cfg_light("AA:BB:CC:DD:EE:02", Some("x"))],
            vec![
                cfg_light("AA:BB:CC:DD:EE:FF", Some("t")),
                cfg_light("11:22:33:44:EE:FF", Some("t")),
                cfg_light("99:88:77:66:EE:FF", Some("t")),
                cfg_light("00:00:00:00:00:01", None),
            ],
            vec![],
        ] {
            let labels = light_labels(&lights);
            let unique: std::collections::HashSet<&String> = labels.iter().collect();
            assert_eq!(unique.len(), labels.len(), "labels not unique: {labels:?}");
        }
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
        let mut tasks: JoinSet<BackgroundExit> = JoinSet::new();
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

    #[test]
    fn failsafe_announces_recovery_as_well_as_loss() {
        use FailsafeEdge::*;
        // The failsafe warned on entering blackout/poweroff and then said
        // NOTHING when ArtNet came back, so a log left running for weeks kept a
        // "signal lost" warning as its last word on a universe that had long
        // since recovered. It was the only latched warning in the bridge without
        // a recovery line (`Sink::dispatch` and `LightActor::find` both have
        // one). `Released` is that missing edge.

        // Signal present, nothing latched — silent, and no action applied.
        assert_eq!(failsafe_edge(0, 1000, false), Idle);
        assert_eq!(failsafe_edge(999, 1000, false), Idle);
        // Signal present after the failsafe fired — announce the release ONCE.
        assert_eq!(failsafe_edge(0, 1000, true), Released);
        assert_eq!(failsafe_edge(999, 1000, true), Released);
        // Newly timed out — warn, then apply. The boundary is inclusive: at
        // exactly `timeout_ms` the failsafe fires (matches the old `<` test).
        assert_eq!(failsafe_edge(1000, 1000, false), Announce);
        assert_eq!(failsafe_edge(5000, 1000, false), Announce);
        // Still timed out — keep applying, but don't repeat the warning.
        assert_eq!(failsafe_edge(1000, 1000, true), Applied);
        assert_eq!(failsafe_edge(u64::MAX, 1000, true), Applied);

        // Every edge except Idle is one the loop acts on, and only Announce and
        // Released log. Walk a full loss→recovery→loss cycle to prove the latch
        // flips both ways: without the recovery edge the second loss would never
        // re-warn either.
        let mut announced = false;
        let mut seen = Vec::new();
        for idle in [0u64, 500, 1200, 2000, 100, 50, 1500] {
            let e = failsafe_edge(idle, 1000, announced);
            match e {
                Announce => announced = true,
                Released => announced = false,
                _ => {}
            }
            seen.push(e);
        }
        assert_eq!(seen, vec![Idle, Idle, Announce, Applied, Released, Idle, Announce]);
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
            let mut tasks: JoinSet<BackgroundExit> = JoinSet::new();
            spawn_failsafe(&mut tasks, &cfg, base, clocks);
            tasks.len()
        };
        assert_eq!(count("blackout", 5, 1), 1, "an armed failsafe must be supervised");
        assert_eq!(count("poweroff", 5, 2), 1, "one task covers every universe");
        assert_eq!(count("hold", 5, 1), 0, "hold has nothing to run");
        assert_eq!(count("blackout", 0, 1), 0, "timeout_secs = 0 behaves like hold");
        assert_eq!(count("blackout", 5, 0), 0, "no lights ⇒ nothing to fail safe");
    }

    #[test]
    fn a_restart_starts_each_light_where_the_last_run_left_it() {
        let on = cfg_light("D6:50:F2:F6:BB:1B", Some("on"));
        let mut hands_off = cfg_light("CB:33:5A:EE:5A:7C", Some("hands-off"));
        hands_off.power_on_connect = false;

        // First start: nothing carried, so the startup baseline as before —
        // powered seed for power_on_connect = true, passive seed otherwise.
        let none = HashMap::new();
        assert_eq!(initial_state(&on, &none), LightState { power: true, seed: false, ..LightState::default() });
        assert_eq!(
            initial_state(&hands_off, &none),
            LightState { power: false, seed: true, ..LightState::default() }
        );

        // Restart: the carried state wins, matched by MAC whatever its spelling
        // (the config may write it any way `parse_mac` accepts).
        let dark = crate::profile::map_dmx(Profile::Rgb, &[0, 0, 0], CctRange::default());
        let carried: HashMap<String, LightState> =
            [(crate::config::normalize_mac("d6-50-f2-f6-bb-1b"), dark)].into_iter().collect();
        assert_eq!(initial_state(&on, &carried), dark);
        assert_eq!(initial_state(&on, &carried).brightness, 0, "a dark light must come back dark");
        // A light the last run knew nothing about still gets the baseline.
        assert_eq!(
            initial_state(&hands_off, &carried),
            LightState { power: false, seed: true, ..LightState::default() }
        );
    }

    #[tokio::test]
    async fn running_reports_every_lights_latest_state_by_mac() {
        // What `run` carries into the next start is whatever each light's
        // channel holds when the run ends — including the latest DMX.
        let (tx, _rx) = watch::channel(LightState::default());
        let states = vec![("AA:BB:CC:DD:EE:01".to_string(), tx.subscribe())];
        let dark = crate::profile::map_dmx(Profile::Rgb, &[0, 0, 0], CctRange::default());
        tx.send(dark).unwrap();
        let running = Running {
            actors: JoinSet::new(),
            background: JoinSet::new(),
            listener: tokio::spawn(async { Ok(()) }),
            listener_done: false,
            failsafe_mode: "hold".into(),
            states,
        };
        assert_eq!(running.light_states().get("AA:BB:CC:DD:EE:01"), Some(&dark));
        running.shutdown().await;
    }

    #[test]
    fn restart_policy_backs_off_and_resets_after_a_healthy_run() {
        let mut p = RestartPolicy::default();
        let crash = Duration::from_secs(1);
        assert_eq!(p.next_delay(crash), Duration::from_secs(5));
        assert_eq!(p.next_delay(crash), Duration::from_secs(10));
        assert_eq!(p.next_delay(crash), Duration::from_secs(20));
        for _ in 0..10 {
            p.next_delay(crash);
        }
        assert_eq!(p.next_delay(crash), RestartPolicy::MAX_DELAY, "capped");
        // A run that lasted long enough is not a crash loop: back to the start.
        assert_eq!(p.next_delay(RestartPolicy::HEALTHY_RUN), Duration::from_secs(5));
        assert_eq!(p.next_delay(Duration::from_secs(3600)), Duration::from_secs(5));
        // Never overflows, however many consecutive failures pile up.
        for _ in 0..100 {
            assert!(p.next_delay(crash) <= RestartPolicy::MAX_DELAY);
        }
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
