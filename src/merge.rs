//! Multi-source DMX input + per-channel merge.
//!
//! The bridge can listen for ArtNet on several sockets ("inputs" — extra UDP
//! ports and/or different bind IPs, `[[artnet.inputs]]` in the config) and
//! merge the streams **per channel** before mapping them onto lights, the way
//! a DMX merger/node does. Modes (`[artnet] merge`):
//!
//! - `htp`    — highest takes precedence: per channel, the max across sources.
//! - `lowest` — per channel, the min across sources.
//! - `ltp`    — latest takes precedence: per channel, the source that most
//!   recently **changed** its value owns the channel. A source re-streaming
//!   the same data does NOT steal a channel back (last *changed*, not last
//!   received) — so a console holding an override keeps it even while another
//!   source refreshes its own unchanged state at full rate.
//!
//! A source that goes quiet for `merge_timeout_secs` is dropped from the merge:
//! its contribution is removed (HTP/lowest), and LTP channels it owned fall
//! back to the most recently active remaining source. A channel with **no**
//! live source holds its last merged value — total signal loss is the
//! `[failsafe]` section's job, not the merger's.
//!
//! With a single input the merger is an identity pass-through and none of the
//! merge settings have any effect, so the feature costs nothing when unused.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, trace, warn};

use crate::artnet::{self, ArtDmx, SeqTracker};

/// Channels in a DMX universe (the merger's per-universe buffer size).
const DMX_CHANNELS: usize = artnet::DMX_UNIVERSE_SIZE as usize;

/// Owner sentinel for "no input owns this channel" (LTP). Config validation
/// caps the input count far below this.
const NO_OWNER: u8 = u8::MAX;

/// Cap on tracked universes — purely defensive (a scanner spraying random
/// port-addresses must not grow the map unbounded). Clearing simply re-derives
/// the merge from the next packets of each live source.
const MAX_UNIVERSES: usize = 1024;

/// How a channel is combined across sources. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMode {
    Htp,
    Lowest,
    Ltp,
}

/// Merge mode names accepted in config (aliases: `highest` = htp, `latest` = ltp).
pub const KNOWN_MERGE_MODES: &[&str] = &["htp", "lowest", "ltp"];

impl MergeMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "htp" | "highest" => Some(Self::Htp),
            "lowest" => Some(Self::Lowest),
            "ltp" | "latest" => Some(Self::Ltp),
            _ => None,
        }
    }
}

impl std::fmt::Display for MergeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Htp => "htp",
            Self::Lowest => "lowest",
            Self::Ltp => "ltp",
        })
    }
}

/// One source's latest DMX data for a universe.
struct Lane {
    data: [u8; DMX_CHANNELS],
    len: usize,
    last_seen: Instant,
}

impl Lane {
    /// The one expiry rule (used by ingest, recompute, and prune alike):
    /// silent past the timeout ⇒ out of the merge. Zero timeout = never.
    fn expired(&self, timeout: Duration, now: Instant) -> bool {
        !timeout.is_zero() && now.duration_since(self.last_seen) > timeout
    }
}

/// Per-universe merge state: one lane per input + the merged output.
struct UniverseMerge {
    lanes: Vec<Option<Lane>>,
    /// Per-channel owning input (LTP only; `NO_OWNER` = unowned/held).
    owner: [u8; DMX_CHANNELS],
    merged: [u8; DMX_CHANNELS],
    /// Monotonic max data length seen — channels past every live lane's length
    /// hold their last merged value rather than disappearing.
    len: usize,
}

impl UniverseMerge {
    fn new(n_inputs: usize) -> Self {
        Self {
            lanes: (0..n_inputs).map(|_| None).collect(),
            owner: [NO_OWNER; DMX_CHANNELS],
            merged: [0; DMX_CHANNELS],
            len: 0,
        }
    }
}

/// Per-channel DMX merger across a fixed set of inputs. Pure state machine —
/// `now` is always passed in, so behaviour is fully deterministic and testable.
pub struct Merger {
    mode: MergeMode,
    /// `ZERO` = sources never expire.
    timeout: Duration,
    n_inputs: usize,
    universes: HashMap<u16, UniverseMerge>,
}

impl Merger {
    pub fn new(mode: MergeMode, timeout: Duration, n_inputs: usize) -> Self {
        assert!(n_inputs >= 1 && n_inputs < NO_OWNER as usize, "input count out of range");
        Self { mode, timeout, n_inputs, universes: HashMap::new() }
    }

    pub fn timeout_enabled(&self) -> bool {
        !self.timeout.is_zero()
    }

    /// How many inputs this merger was built for. [`serve_all`] checks its
    /// socket list against this, so a mismatched pair fails at the call instead
    /// of panicking inside a spawned task on the first packet.
    pub fn input_count(&self) -> usize {
        self.n_inputs
    }

    /// Feed one ArtDmx worth of data from `input`. Returns whether the merged
    /// output changed, plus the merged view for that universe — callers can
    /// use the flag for edge-triggered work (skip re-dispatch, log-on-change).
    pub fn ingest(
        &mut self,
        input: usize,
        port_address: u16,
        data: &[u8],
        now: Instant,
    ) -> (bool, &[u8]) {
        assert!(input < self.n_inputs, "input index out of range");
        let len = data.len().min(DMX_CHANNELS);
        if !self.universes.contains_key(&port_address) && self.universes.len() >= MAX_UNIVERSES {
            debug!(cap = MAX_UNIVERSES, "merge universe cap hit — resetting merge state");
            self.universes.clear();
        }
        let mode = self.mode;
        let timeout = self.timeout;
        let n_inputs = self.n_inputs;
        let u = self
            .universes
            .entry(port_address)
            .or_insert_with(|| UniverseMerge::new(n_inputs));
        // A longer merged view is a change even if every byte matches (it also
        // makes the very first packet of a universe always report changed).
        let mut changed = len > u.len;
        u.len = u.len.max(len);

        match mode {
            MergeMode::Ltp => {
                // A channel is (re)taken when THIS input's value differs from
                // its own previous value. A first packet — or one after the
                // source was silent past the timeout — counts as all-changed:
                // a (re)appearing source re-asserts its state.
                let stale = u.lanes[input].as_ref().is_some_and(|l| l.expired(timeout, now));
                for (ch, &v) in data.iter().enumerate().take(len) {
                    let prev = match (&u.lanes[input], stale) {
                        (Some(l), false) if ch < l.len => Some(l.data[ch]),
                        _ => None,
                    };
                    if prev != Some(v) {
                        u.owner[ch] = input as u8;
                        if u.merged[ch] != v {
                            u.merged[ch] = v;
                            changed = true;
                        }
                    }
                }
                store_lane(&mut u.lanes[input], data, len, now);
            }
            MergeMode::Htp | MergeMode::Lowest => {
                store_lane(&mut u.lanes[input], data, len, now);
                changed |= recompute(u, mode, timeout, now);
            }
        }
        (changed, &u.merged[..u.len])
    }

    /// Drop sources that have been silent past the timeout and re-merge.
    /// Returns the port-addresses whose merged output changed (so the caller
    /// can re-dispatch them). No-op when the timeout is disabled.
    pub fn prune(&mut self, now: Instant) -> Vec<u16> {
        if self.timeout.is_zero() {
            return Vec::new();
        }
        let mode = self.mode;
        let timeout = self.timeout;
        let mut changed_universes = Vec::new();
        for (&pa, u) in self.universes.iter_mut() {
            let mut removed = false;
            for lane in u.lanes.iter_mut() {
                if lane.as_ref().is_some_and(|l| l.expired(timeout, now)) {
                    *lane = None;
                    removed = true;
                }
            }
            if !removed {
                continue;
            }
            let changed = match mode {
                MergeMode::Htp | MergeMode::Lowest => recompute(u, mode, timeout, now),
                MergeMode::Ltp => reassign_orphans(u),
            };
            if changed {
                changed_universes.push(pa);
            }
        }
        changed_universes
    }

    /// The current merged view for a universe, if any data has been seen.
    pub fn merged(&self, port_address: u16) -> Option<&[u8]> {
        self.universes.get(&port_address).map(|u| &u.merged[..u.len])
    }
}

fn store_lane(slot: &mut Option<Lane>, data: &[u8], len: usize, now: Instant) {
    let lane = slot.get_or_insert_with(|| Lane {
        data: [0; DMX_CHANNELS],
        len: 0,
        last_seen: now,
    });
    lane.data[..len].copy_from_slice(&data[..len]);
    lane.len = len;
    lane.last_seen = now;
}

/// Full HTP/lowest recompute of one universe from its live lanes. A channel
/// with no live contribution holds its last merged value. Returns whether the
/// merged output changed.
fn recompute(u: &mut UniverseMerge, mode: MergeMode, timeout: Duration, now: Instant) -> bool {
    let mut changed = false;
    for ch in 0..u.len {
        let mut acc: Option<u8> = None;
        for lane in u.lanes.iter().flatten() {
            if ch >= lane.len {
                continue;
            }
            if lane.expired(timeout, now) {
                continue; // expired but not yet pruned — already out of the merge
            }
            let v = lane.data[ch];
            acc = Some(match (acc, mode) {
                (None, _) => v,
                (Some(a), MergeMode::Lowest) => a.min(v),
                (Some(a), _) => a.max(v),
            });
        }
        if let Some(v) = acc {
            if u.merged[ch] != v {
                u.merged[ch] = v;
                changed = true;
            }
        }
    }
    changed
}

/// LTP after lane removal: channels owned by a dead input fall back to the
/// most recently active remaining source that covers them; with none left the
/// value holds (owner cleared). Returns whether the merged output changed.
fn reassign_orphans(u: &mut UniverseMerge) -> bool {
    let mut changed = false;
    for ch in 0..u.len {
        let owner = u.owner[ch];
        if owner == NO_OWNER {
            continue;
        }
        let owner_alive = u.lanes.get(owner as usize).is_some_and(|l| l.is_some());
        if owner_alive {
            continue;
        }
        let mut best: Option<(u8, u8, Instant)> = None; // (input, value, last_seen)
        for (i, lane) in u.lanes.iter().enumerate() {
            if let Some(l) = lane {
                if ch < l.len && best.is_none_or(|(_, _, seen)| l.last_seen > seen) {
                    best = Some((i as u8, l.data[ch], l.last_seen));
                }
            }
        }
        match best {
            Some((input, value, _)) => {
                u.owner[ch] = input;
                if u.merged[ch] != value {
                    u.merged[ch] = value;
                    changed = true;
                }
            }
            None => u.owner[ch] = NO_OWNER, // hold the last value
        }
    }
    changed
}

/// Record which sender an input has been hearing for a universe, and report the
/// FIRST one the moment a second, different sender appears.
///
/// The merger keeps one lane per **input**, not per sender, so two sources
/// pointed at the same socket end up sharing a lane and the merge rules can no
/// longer tell them apart (in `ltp` the "did this source change?" test starts
/// comparing one source's value against the other's, and an override stops
/// being sticky). Callers warn once per input; the fix is a separate
/// `[[artnet.inputs]]` entry per source.
///
/// The map is capped like the merger's own universe map — a scanner spraying
/// port-addresses must not grow it without bound.
fn note_sender(
    seen: &mut HashMap<(usize, u16), std::net::IpAddr>,
    input: usize,
    port_address: u16,
    src: std::net::IpAddr,
) -> Option<std::net::IpAddr> {
    if seen.len() >= MAX_UNIVERSES && !seen.contains_key(&(input, port_address)) {
        seen.clear();
    }
    let first = *seen.entry((input, port_address)).or_insert(src);
    (first != src).then_some(first)
}

/// One bound ArtNet input socket + its log label.
pub struct Input {
    pub sock: UdpSocket,
    pub label: String,
}

/// Bind every configured ArtNet input and build the matching [`Merger`] — the
/// one shared setup path for `run` and `monitor`, so the monitor provably runs
/// the identical pipeline the bridge does. Any bind failure is a hard error
/// (callers treat it as fatal at startup).
pub async fn bind_inputs(artnet_cfg: &crate::config::ArtNet) -> Result<(Vec<Input>, Merger)> {
    let mut bound = Vec::new();
    for inp in artnet_cfg.resolved_inputs() {
        bound.push(Input {
            sock: artnet::bind(&inp.bind_ip, inp.port).await?,
            label: inp.label,
        });
    }
    let mode = MergeMode::parse(&artnet_cfg.merge)
        .with_context(|| format!("invalid artnet.merge mode {:?}", artnet_cfg.merge))?;
    let merger = Merger::new(
        mode,
        Duration::from_secs(artnet_cfg.merge_timeout_secs),
        bound.len(),
    );
    Ok((bound, merger))
}

/// Which port-addresses [`serve_all`] should actually merge.
///
/// ArtNet is routinely **broadcast** on a lighting LAN, so a bridge driving one
/// universe can easily see traffic for a dozen it has no light patched to. Every
/// such packet used to allocate a lane set and run a full 512-channel merge for
/// a universe whose result nothing would ever read (up to the 1024-universe cap:
/// ~1 KB of merged buffer plus 536 B per input, each). `run` therefore declares
/// the universes it maps and the rest are dropped straight after `on_raw`.
///
/// This mirrors the rule `bridge::note_artnet` already applies to the failsafe
/// clocks — foreign universes must not count as signal — and applies it to the
/// merge state as well.
#[derive(Debug, Clone)]
pub enum Interest {
    /// Merge every universe that arrives. `monitor` uses this: it exists to
    /// show what is actually on the wire, so it must not hide anything.
    All,
    /// Merge only these port-addresses.
    Only(HashSet<u16>),
}

impl Interest {
    /// Universes drawn from a configured universe→sinks map.
    pub fn only<'a>(universes: impl IntoIterator<Item = &'a u16>) -> Self {
        Interest::Only(universes.into_iter().copied().collect())
    }

    /// Should `port_address` be merged?
    pub fn wants(&self, port_address: u16) -> bool {
        match self {
            Interest::All => true,
            Interest::Only(set) => set.contains(&port_address),
        }
    }
}

/// Receive on every input, sequence-filter per input, merge, and hand the
/// merged per-universe data to `on_merged` (with a changed flag, so callers
/// can skip work when a refresh didn't move the output). `on_raw` fires for
/// **every** valid ArtDmx (before the stale-sequence drop AND before the
/// `interest` filter) — the bridge feeds its failsafe timer from it, `monitor`
/// its per-packet log. Any listener socket dying is fatal (returns `Err`),
/// matching the single-listener behaviour.
///
/// `interest` limits which universes are merged at all; see [`Interest`].
///
/// Used by both `run` and `monitor`, so the monitor exercises and displays the
/// exact merge pipeline the bridge runs.
pub async fn serve_all<R, M>(
    inputs: Vec<Input>,
    mut merger: Merger,
    interest: Interest,
    mut on_raw: R,
    mut on_merged: M,
) -> Result<()>
where
    R: FnMut(usize, &str, SocketAddr, &ArtDmx),
    M: FnMut(u16, &[u8], bool),
{
    let n = inputs.len();
    if n != merger.input_count() {
        anyhow::bail!(
            "internal: {n} ArtNet inputs bound but the merger was built for {} — \
             build both with bind_inputs()",
            merger.input_count()
        );
    }
    let labels: Vec<String> = inputs.iter().map(|i| i.label.clone()).collect();
    // Bounded hand-off from the listener tasks to this merge/dispatch loop. If
    // the queue ever fills (it shouldn't — merging is microseconds), dropping a
    // frame is correct: DMX is a lossy stream and the next refresh supersedes it.
    let (tx, mut rx) = mpsc::channel::<(usize, SocketAddr, ArtDmx)>(512);
    let mut listeners = JoinSet::new();
    for (idx, input) in inputs.into_iter().enumerate() {
        let tx = tx.clone();
        let label = input.label;
        let label_inner = label.clone();
        listeners.spawn(async move {
            artnet::serve(input.sock, move |src, pkt| {
                // A full queue also skips `on_raw` (the bridge's failsafe
                // feed) for the dropped frame. Deliberate: the queue only
                // fills if the merge loop is wedged for seconds — at which
                // point the lights aren't being driven either, and a firing
                // failsafe is telling the truth.
                if tx.try_send((idx, src, pkt)).is_err() {
                    trace!(input = %label_inner, "merge queue full — ArtDmx dropped");
                }
            })
            .await
            .with_context(|| format!("ArtNet listener '{label}' failed"))
        });
    }
    drop(tx);

    let mut seq: Vec<SeqTracker> = (0..n).map(|_| SeqTracker::new()).collect();
    // Source-expiry only matters across inputs; with one input the failsafe
    // already covers total loss, so skip the prune ticks entirely.
    let prune_enabled = merger.timeout_enabled() && n > 1;
    // The merger keeps one lane per INPUT, not per sender — so two consoles
    // pointed at the same socket share a lane and the merge rules can no longer
    // tell them apart (in `ltp` the "last changed" test compares each source
    // against the OTHER's last value, and an override stops being sticky).
    // Detect it and say so ONCE PER INPUT for the whole run (`shared_warned`):
    // the remedy is the same however many universes collide on that input —
    // give it its own [[artnet.inputs]] entry — so one warning carries
    // everything the operator needs, and a badly-wired input can't spam the log.
    // Only meaningful while merging is actually active — with one input the
    // merger is a pass-through.
    let mut senders: HashMap<(usize, u16), std::net::IpAddr> = HashMap::new();
    let mut shared_warned: Vec<bool> = vec![false; n];
    let mut tick = interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some((idx, src, pkt)) = msg else {
                    // Every listener task is gone; surface the first failure.
                    return Err(match listeners.join_next().await {
                        Some(Ok(Err(e))) => e,
                        Some(Err(join_err)) => {
                            anyhow::Error::from(join_err).context("ArtNet listener task panicked")
                        }
                        _ => anyhow::anyhow!("all ArtNet listeners ended unexpectedly"),
                    });
                };
                on_raw(idx, &labels[idx], src, &pkt);
                // Foreign universe (nothing patched to it) — `on_raw` has had
                // it for the failsafe/monitor, and everything past this point
                // is merge state we would build and never read. Dropped before
                // the sequence check too, so foreign senders can't consume
                // SeqTracker capacity that belongs to the real ones.
                if !interest.wants(pkt.port_address) {
                    trace!(input = %labels[idx], port = pkt.port_address,
                           "ArtDmx for an unmapped universe ignored");
                    continue;
                }
                // Drop out-of-order/duplicate packets (Art-Net Sequence field) so
                // a late datagram can't briefly re-apply an old state. Tracked
                // per input: the same console feeding two inputs runs an
                // independent sequence stream on each socket.
                if !seq[idx].is_fresh(src.ip(), pkt.port_address, pkt.sequence) {
                    debug!(input = %labels[idx], %src, port = pkt.port_address,
                           seq = pkt.sequence, "stale ArtDmx dropped");
                    continue;
                }
                if n > 1 && !shared_warned[idx] {
                    if let Some(first) =
                        note_sender(&mut senders, idx, pkt.port_address, src.ip())
                    {
                        shared_warned[idx] = true;
                        warn!(
                            input = %labels[idx], port = pkt.port_address,
                            first = %first, also = %src.ip(),
                            "two ArtNet sources are sending the same universe to ONE input — \
                             they share a merge lane, so the merge cannot tell them apart; \
                             give each source its own [[artnet.inputs]] entry (own port \
                             and/or bind_ip)"
                        );
                    }
                }
                let (changed, merged) =
                    merger.ingest(idx, pkt.port_address, &pkt.data, Instant::now());
                on_merged(pkt.port_address, merged, changed);
            }
            Some(res) = listeners.join_next() => {
                // Any listener socket dying is fatal — a supervisor must see it.
                return Err(match res {
                    Ok(Ok(())) => anyhow::anyhow!("an ArtNet listener ended unexpectedly"),
                    Ok(Err(e)) => e,
                    Err(join_err) => {
                        anyhow::Error::from(join_err).context("ArtNet listener task panicked")
                    }
                });
            }
            _ = tick.tick(), if prune_enabled => {
                // prune() only reports universes whose output changed.
                for pa in merger.prune(Instant::now()) {
                    if let Some(m) = merger.merged(pa) {
                        on_merged(pa, m, true);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    fn merger(mode: MergeMode, timeout_secs: u64, n: usize) -> Merger {
        Merger::new(mode, Duration::from_secs(timeout_secs), n)
    }

    #[test]
    fn parse_modes_and_aliases() {
        assert_eq!(MergeMode::parse("htp"), Some(MergeMode::Htp));
        assert_eq!(MergeMode::parse("HIGHEST"), Some(MergeMode::Htp));
        assert_eq!(MergeMode::parse("lowest"), Some(MergeMode::Lowest));
        assert_eq!(MergeMode::parse("ltp"), Some(MergeMode::Ltp));
        assert_eq!(MergeMode::parse("latest"), Some(MergeMode::Ltp));
        assert_eq!(MergeMode::parse("nope"), None);
    }

    #[test]
    fn single_input_is_identity_in_every_mode() {
        let t0 = Instant::now();
        for mode in [MergeMode::Htp, MergeMode::Lowest, MergeMode::Ltp] {
            let mut m = merger(mode, 10, 1);
            assert_eq!(m.ingest(0, 0, &[7, 8, 9], t0).1, &[7, 8, 9]);
            assert_eq!(m.ingest(0, 0, &[1, 2, 3], at(t0, 1)).1, &[1, 2, 3]);
        }
    }

    #[test]
    fn htp_takes_max_per_channel() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Htp, 10, 2);
        assert_eq!(m.ingest(0, 0, &[10, 200, 5], t0).1, &[10, 200, 5]);
        assert_eq!(m.ingest(1, 0, &[20, 100, 5], t0).1, &[20, 200, 5]);
        // Input 0 lowering its own value only shows where it still wins.
        assert_eq!(m.ingest(0, 0, &[0, 150, 5], at(t0, 1)).1, &[20, 150, 5]);
    }

    #[test]
    fn lowest_takes_min_per_channel() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Lowest, 10, 2);
        assert_eq!(m.ingest(0, 0, &[10, 200], t0).1, &[10, 200]);
        assert_eq!(m.ingest(1, 0, &[20, 100], t0).1, &[10, 100]);
    }

    #[test]
    fn short_lane_does_not_contribute_beyond_its_length() {
        let t0 = Instant::now();
        // In lowest mode a phantom 0 from a short lane would be catastrophic.
        let mut m = merger(MergeMode::Lowest, 10, 2);
        m.ingest(0, 0, &[50, 60], t0);
        assert_eq!(m.ingest(1, 0, &[10], t0).1, &[10, 60]);
    }

    #[test]
    fn ltp_change_takes_ownership_resend_does_not() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Ltp, 10, 2);
        assert_eq!(m.ingest(0, 0, &[100, 100], t0).1, &[100, 100]);
        // Input 1's first packet asserts its state (takes what it carries).
        assert_eq!(m.ingest(1, 0, &[100, 50], t0).1, &[100, 50]);
        // Input 0 re-streaming its unchanged data does NOT steal ch2 back —
        // last CHANGED, not last received.
        assert_eq!(m.ingest(0, 0, &[100, 100], at(t0, 1)).1, &[100, 50]);
        // ...but an actual change does take the channel.
        assert_eq!(m.ingest(0, 0, &[100, 80], at(t0, 2)).1, &[100, 80]);
        // And input 1 re-sending ITS old value doesn't steal it back either.
        assert_eq!(m.ingest(1, 0, &[100, 50], at(t0, 3)).1, &[100, 80]);
    }

    #[test]
    fn ltp_timeout_falls_back_to_live_source() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Ltp, 10, 2);
        m.ingest(1, 0, &[10], t0);
        m.ingest(0, 0, &[201], at(t0, 1)); // input 0 takes the channel
        m.ingest(1, 0, &[10], at(t0, 5)); // unchanged — no steal
        assert_eq!(m.merged(0).unwrap(), &[201]);
        // Input 0 silent past the timeout → its channel falls back to input 1.
        assert_eq!(m.prune(at(t0, 12)), vec![0]);
        assert_eq!(m.merged(0).unwrap(), &[10]);
    }

    #[test]
    fn htp_timeout_drops_contribution() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Htp, 10, 2);
        m.ingest(0, 0, &[255], t0);
        m.ingest(1, 0, &[10], at(t0, 9));
        assert_eq!(m.merged(0).unwrap(), &[255]);
        // At t0+11 input 0 (last seen t0) is expired, input 1 (t0+9) is live.
        assert_eq!(m.prune(at(t0, 11)), vec![0]);
        assert_eq!(m.merged(0).unwrap(), &[10]);
    }

    #[test]
    fn all_sources_dead_holds_last_merge() {
        let t0 = Instant::now();
        for mode in [MergeMode::Htp, MergeMode::Lowest, MergeMode::Ltp] {
            let mut m = merger(mode, 10, 2);
            m.ingest(0, 0, &[42, 7], t0);
            m.ingest(1, 0, &[42, 9], t0);
            // Both expired: the merged output holds (nothing "changed").
            assert_eq!(m.prune(at(t0, 60)), Vec::<u16>::new());
            assert_eq!(m.merged(0).unwrap().len(), 2);
            assert_eq!(m.merged(0).unwrap()[0], 42);
        }
    }

    #[test]
    fn timeout_zero_never_expires() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Htp, 0, 2);
        m.ingest(0, 0, &[255], t0);
        m.ingest(1, 0, &[10], t0);
        assert!(!m.timeout_enabled());
        assert_eq!(m.prune(at(t0, 3600)), Vec::<u16>::new());
        assert_eq!(m.merged(0).unwrap(), &[255]);
    }

    #[test]
    fn ltp_returning_source_reasserts_after_silence() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Ltp, 10, 2);
        m.ingest(0, 0, &[5], t0);
        m.ingest(1, 0, &[9], t0); // input 1 owns (first packet asserts)
        assert_eq!(m.merged(0).unwrap(), &[9]);
        // Input 0 returns after silence past the timeout, re-sending its old
        // value — a returning source re-asserts its state even unchanged.
        assert_eq!(m.ingest(0, 0, &[5], at(t0, 20)).1, &[5]);
    }

    #[test]
    fn universes_are_independent() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Htp, 10, 2);
        m.ingest(0, 1, &[100], t0);
        m.ingest(1, 2, &[50], t0);
        assert_eq!(m.merged(1).unwrap(), &[100]);
        assert_eq!(m.merged(2).unwrap(), &[50]);
        assert!(m.merged(3).is_none());
    }

    #[test]
    fn merged_length_is_monotonic_and_short_updates_hold_the_tail() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Htp, 10, 2);
        assert_eq!(m.ingest(0, 0, &[1, 2, 3], t0).1, &[1, 2, 3]);
        assert_eq!(m.ingest(1, 0, &[9], t0).1, &[9, 2, 3]);
        // A shorter refresh from input 0 leaves ch2/ch3 holding.
        assert_eq!(m.ingest(0, 0, &[1, 2], at(t0, 1)).1, &[9, 2, 3]);
    }

    #[test]
    fn ingest_reports_whether_the_merge_changed() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Ltp, 10, 2);
        assert!(m.ingest(0, 0, &[10, 20], t0).0); // first packet (length grew)
        assert!(!m.ingest(0, 0, &[10, 20], at(t0, 1)).0); // identical refresh
        assert!(m.ingest(0, 0, &[10, 21], at(t0, 2)).0); // value change
        assert!(m.ingest(0, 0, &[10, 21, 5], at(t0, 3)).0); // length growth alone
        assert!(!m.ingest(1, 0, &[10, 21, 5], at(t0, 4)).0); // same values from input 1

        let mut h = merger(MergeMode::Htp, 10, 2);
        assert!(h.ingest(0, 0, &[10], t0).0);
        assert!(!h.ingest(1, 0, &[5], t0).0); // loses the max — output unchanged
        assert!(h.ingest(1, 0, &[50], t0).0); // wins the max
    }

    #[test]
    fn full_512_channel_merge() {
        let t0 = Instant::now();
        let mut m = merger(MergeMode::Htp, 10, 2);
        let a = [100u8; DMX_CHANNELS];
        let mut b = [0u8; DMX_CHANNELS];
        b[511] = 255;
        m.ingest(0, 0, &a, t0);
        let out = m.ingest(1, 0, &b, t0).1;
        assert_eq!(out.len(), DMX_CHANNELS);
        assert_eq!(out[0], 100);
        assert_eq!(out[511], 255);
    }

    // ---- serve_all plumbing (real loopback UDP sockets) ----

    use std::sync::{Arc, Mutex};

    type MergedLog = Arc<Mutex<Vec<(u16, Vec<u8>)>>>;

    async fn bind_local() -> (UdpSocket, std::net::SocketAddr) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        (sock, addr)
    }

    #[tokio::test]
    async fn serve_all_merges_across_two_sockets() {
        let (s1, a1) = bind_local().await;
        let (s2, a2) = bind_local().await;
        let raw_log: Arc<Mutex<Vec<String>>> = Arc::default();
        let merged_log: MergedLog = Arc::default();
        let raw2 = raw_log.clone();
        let merged2 = merged_log.clone();
        let pump = tokio::spawn(serve_all(
            vec![
                Input { sock: s1, label: "primary".into() },
                Input { sock: s2, label: "second".into() },
            ],
            Merger::new(MergeMode::Htp, Duration::from_secs(10), 2),
            Interest::All,
            move |_idx, label, _src, _pkt| raw2.lock().unwrap().push(label.to_string()),
            move |pa, data, _changed| merged2.lock().unwrap().push((pa, data.to_vec())),
        ));

        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // seq 0 = sequencing disabled (keeps the test independent of SeqTracker).
        tx.send_to(&artnet::encode_artdmx(0, 0, &[10, 200]), a1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send_to(&artnet::encode_artdmx(0, 0, &[20, 100]), a2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        pump.abort();

        let raw = raw_log.lock().unwrap();
        assert_eq!(*raw, vec!["primary".to_string(), "second".to_string()]);
        let merged = merged_log.lock().unwrap();
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], (0, vec![10, 200]));
        assert_eq!(merged[1], (0, vec![20, 200])); // HTP across the two inputs
    }

    /// `note_sender` tracks per (input, universe) and flags the first sender the
    /// moment a second one appears on that pair. NOTE: `serve_all` additionally
    /// latches the WARNING per input, so only the first collision an input sees
    /// is ever logged — this covers the tracking, not the log gating.
    #[test]
    fn note_sender_flags_a_second_sender_per_input_and_universe() {
        use std::net::{IpAddr, Ipv4Addr};
        let ip = |d| IpAddr::V4(Ipv4Addr::new(10, 0, 0, d));
        let mut seen = HashMap::new();

        // First sender for (input 0, universe 0): nothing to report.
        assert_eq!(note_sender(&mut seen, 0, 0, ip(1)), None);
        // Same sender again: still nothing.
        assert_eq!(note_sender(&mut seen, 0, 0, ip(1)), None);
        // A SECOND sender on the same input+universe: they share a merge lane.
        assert_eq!(note_sender(&mut seen, 0, 0, ip(2)), Some(ip(1)));

        // A different input is tracked separately (that IS the correct setup).
        assert_eq!(note_sender(&mut seen, 1, 0, ip(2)), None);
        // As is a different universe on the same input (no lane is shared).
        assert_eq!(note_sender(&mut seen, 0, 7, ip(2)), None);
    }

    #[tokio::test]
    async fn serve_all_rejects_a_merger_sized_for_a_different_input_count() {
        // A mismatched (sockets, merger) pair used to panic inside a spawned
        // listener task on the first packet — `ingest`'s bounds assert. Fail at
        // the call instead. Both real call sites go through `bind_inputs`, so
        // this only guards the API.
        let (s1, _) = bind_local().await;
        let (s2, _) = bind_local().await;
        let err = serve_all(
            vec![
                Input { sock: s1, label: "primary".into() },
                Input { sock: s2, label: "second".into() },
            ],
            Merger::new(MergeMode::Htp, Duration::from_secs(10), 1),
            Interest::All,
            |_, _, _, _| {},
            |_, _, _| {},
        )
        .await
        .expect_err("2 inputs with a 1-input merger must be rejected");
        assert!(format!("{err:#}").contains("merger was built for 1"));
    }

    #[tokio::test]
    async fn serve_all_merges_only_the_universes_of_interest() {
        // ArtNet is routinely broadcast, so a bridge driving universe 0 sees
        // traffic for universes nothing is patched to. Those must not build
        // merge state — but they MUST still reach `on_raw`, which is what feeds
        // the failsafe clock and the monitor log.
        let (s1, a1) = bind_local().await;
        let raw_log: Arc<Mutex<Vec<u16>>> = Arc::default();
        let merged_log: MergedLog = Arc::default();
        let raw2 = raw_log.clone();
        let merged2 = merged_log.clone();
        let pump = tokio::spawn(serve_all(
            vec![Input { sock: s1, label: "primary".into() }],
            Merger::new(MergeMode::Ltp, Duration::from_secs(10), 1),
            Interest::only([0u16, 3].iter()),
            move |_idx, _label, _src, pkt| raw2.lock().unwrap().push(pkt.port_address),
            move |pa, data, _changed| merged2.lock().unwrap().push((pa, data.to_vec())),
        ));

        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for pa in [0u16, 7, 3, 9] {
            tx.send_to(&artnet::encode_artdmx(pa, 0, &[pa as u8, 5]), a1).await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        pump.abort();

        // Every packet reached on_raw…
        assert_eq!(*raw_log.lock().unwrap(), vec![0, 7, 3, 9]);
        // …but only the mapped universes were merged.
        let merged = merged_log.lock().unwrap();
        assert_eq!(*merged, vec![(0, vec![0, 5]), (3, vec![3, 5])]);
    }

    #[test]
    fn interest_all_wants_everything_and_only_is_exact() {
        assert!(Interest::All.wants(0));
        assert!(Interest::All.wants(32_767));
        let only = Interest::only([0u16, 4].iter());
        assert!(only.wants(0));
        assert!(only.wants(4));
        assert!(!only.wants(1));
        assert!(!only.wants(32_767));
        // An empty interest set merges nothing (a config with no lights never
        // reaches `run`, but the type must not silently mean "everything").
        assert!(!Interest::only(std::iter::empty()).wants(0));
    }

    #[tokio::test]
    async fn serve_all_drops_stale_sequence_per_input() {
        let (s1, a1) = bind_local().await;
        let merged_log: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let merged2 = merged_log.clone();
        let pump = tokio::spawn(serve_all(
            vec![Input { sock: s1, label: "primary".into() }],
            Merger::new(MergeMode::Ltp, Duration::from_secs(10), 1),
            Interest::All,
            |_, _, _, _| {},
            move |_pa, data, _changed| merged2.lock().unwrap().push(data.to_vec()),
        ));

        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        tx.send_to(&artnet::encode_artdmx(0, 5, &[50]), a1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Stale sequence (4 < 5) — must be dropped, merged output unchanged.
        tx.send_to(&artnet::encode_artdmx(0, 4, &[99]), a1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send_to(&artnet::encode_artdmx(0, 6, &[60]), a1).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        pump.abort();

        let merged = merged_log.lock().unwrap();
        assert_eq!(*merged, vec![vec![50], vec![60]]);
    }
}
