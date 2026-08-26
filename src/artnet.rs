//! ArtNet receive path. We only need **ArtDmx** (OpCode `0x5000`); everything
//! else (ArtPoll etc.) is ignored for now.
//!
//! Port-Address is the 15-bit `Net(7) | Sub-Net(4) | Universe(4)` value that a
//! light's config binds to; the wire splits it across the `Net` byte and the
//! `SubUni` byte (Sub-Net high nibble + Universe low nibble).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tracing::{info, trace};

/// Standard ArtNet UDP port.
pub const ARTNET_PORT: u16 = 6454;

/// Channels in one DMX universe — the hard ceiling on an ArtDmx data slot, on a
/// light's addressing, and on the merger's per-universe buffers.
pub const DMX_UNIVERSE_SIZE: u16 = 512;

/// Largest valid Port-Address: the field is 15 bits (`Net(7) | Sub-Net(4) |
/// Universe(4)`), so 0..=32767.
///
/// Anything above this cannot be transmitted — [`encode_artdmx`] masks `Net` to
/// its 7 bits — so every user-supplied universe must be checked against this
/// rather than silently truncated. Shared by config validation and the
/// `artnet-send` helper so the two can't disagree about the limit.
pub const MAX_PORT_ADDRESS: u16 = 0x7FFF;

const ARTNET_ID: &[u8; 8] = b"Art-Net\0";
const OP_DMX: u16 = 0x5000;

/// A decoded ArtDmx packet (only the fields we use).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtDmx {
    /// 0x01..=0xff packet sequence (0 = sequencing disabled).
    pub sequence: u8,
    /// Source physical port, straight off the wire. Informational only — the
    /// merger discriminates sources by which input socket they arrived on, not
    /// by this field.
    pub physical: u8,
    /// 15-bit Port-Address (Net/Sub-Net/Universe combined).
    pub port_address: u16,
    /// DMX channel data, 1..=[`DMX_UNIVERSE_SIZE`] bytes.
    pub data: Vec<u8>,
}

/// Parse a UDP datagram as ArtDmx. Returns `None` for anything that isn't a
/// well-formed ArtDmx packet (wrong ID, wrong opcode, truncated, bad length).
pub fn parse_artdmx(buf: &[u8]) -> Option<ArtDmx> {
    // 8 id + 2 op + 2 ver + 1 seq + 1 phys + 1 subuni + 1 net + 2 len = 18 header
    const HEADER: usize = 18;
    if buf.len() < HEADER {
        return None;
    }
    if &buf[0..8] != ARTNET_ID {
        return None;
    }
    // OpCode is transmitted low byte first (little-endian).
    let opcode = u16::from_le_bytes([buf[8], buf[9]]);
    if opcode != OP_DMX {
        return None;
    }
    // buf[10..12] = ProtVer (big-endian, current = 14); we don't enforce it.
    let sequence = buf[12];
    let physical = buf[13];
    let sub_uni = buf[14];
    let net = buf[15];
    // Length is big-endian; the spec says even and 2..=512 (we accept 1..=512).
    let length = u16::from_be_bytes([buf[16], buf[17]]) as usize;
    if length == 0 || length > DMX_UNIVERSE_SIZE as usize {
        return None;
    }
    if buf.len() < HEADER + length {
        return None;
    }
    let data = buf[HEADER..HEADER + length].to_vec();
    let port_address = (((net as u16) & 0x7F) << 8) | (sub_uni as u16);
    Some(ArtDmx { sequence, physical, port_address, data })
}

/// Encode an ArtDmx datagram for a given Port-Address. Used by the `artnet-send`
/// test command (and the parser round-trip test). `data` is truncated to one
/// universe ([`DMX_UNIVERSE_SIZE`]).
pub fn encode_artdmx(port_address: u16, sequence: u8, data: &[u8]) -> Vec<u8> {
    let len = data.len().min(DMX_UNIVERSE_SIZE as usize);
    let mut p = Vec::with_capacity(18 + len);
    p.extend_from_slice(ARTNET_ID);
    p.extend_from_slice(&OP_DMX.to_le_bytes()); // opcode, low byte first
    p.extend_from_slice(&[0x00, 14]); // ProtVer, big-endian (current = 14)
    p.push(sequence);
    p.push(0); // physical
    p.push((port_address & 0xFF) as u8); // SubUni
    p.push(((port_address >> 8) & 0x7F) as u8); // Net
    p.extend_from_slice(&(len as u16).to_be_bytes()); // length, big-endian
    p.extend_from_slice(&data[..len]);
    p
}

/// Decompose a 15-bit Port-Address back into (Net, Sub-Net, Universe) — handy
/// for human-readable logging.
pub fn split_port_address(port_address: u16) -> (u8, u8, u8) {
    let net = ((port_address >> 8) & 0x7F) as u8;
    let sub_net = ((port_address >> 4) & 0x0F) as u8;
    let universe = (port_address & 0x0F) as u8;
    (net, sub_net, universe)
}

/// Bind the ArtNet UDP socket. Split out from [`serve`] so the bridge can bind
/// **before** spawning anything else — a bind failure (port already in use, bad
/// bind IP) must be a hard startup error, not a warning inside a spawned task.
pub async fn bind(bind_ip: &str, port: u16) -> Result<UdpSocket> {
    let addr = format!("{bind_ip}:{port}");
    let sock = UdpSocket::bind(&addr)
        .await
        .with_context(|| format!("binding ArtNet listener to {addr}"))?;
    info!(%addr, "ArtNet listener bound");
    Ok(sock)
}

/// Receive loop over an already-bound socket: call `on_packet` for every valid
/// ArtDmx datagram. Runs until the socket errors or the task is cancelled.
pub async fn serve<F>(sock: UdpSocket, mut on_packet: F) -> Result<()>
where
    F: FnMut(SocketAddr, ArtDmx),
{
    // Max ArtDmx = 18 header + 512 data = 530 bytes — but the buffer must cover
    // the maximum UDP payload (65,507 B) anyway: on Linux an oversized datagram
    // is silently truncated to the buffer, but on Windows `recv_from` FAILS with
    // WSAEMSGSIZE (os error 10040), which with a small buffer let one junk
    // datagram from anywhere on the LAN kill the whole listener (empirically
    // confirmed: a single 2000-byte datagram took the bridge down). Sized to the
    // wire maximum, oversized junk is received whole, fails ArtDmx parsing, and
    // is skipped like any other non-ArtDmx datagram.
    let mut buf = vec![0u8; 65_536];
    loop {
        let (n, src) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            // Windows surfaces async ICMP "port unreachable" notifications as a
            // spurious WSAECONNRESET on a later recv — connectionless noise, not
            // socket death. Skip it; anything else (adapter gone, socket closed)
            // is a real listener failure and must stay fatal so a supervisor
            // sees it (the 1.0.0 fatal-listener rule).
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                trace!(error = %e, "transient recv error ignored (connection reset)");
                continue;
            }
            Err(e) => return Err(e).context("recv_from"),
        };
        match parse_artdmx(&buf[..n]) {
            Some(pkt) => on_packet(src, pkt),
            None => trace!(%src, bytes = n, "ignored non-ArtDmx datagram"),
        }
    }
}

/// How long a (source, port-address) key may go without a FRESH packet before
/// its sequence tracking is reset. A restarted sender (whose sequence numbering
/// starts over) must never be locked out by the stale-window check below — its
/// out-of-window packets are dropped for at most this long, then adopted.
///
/// The clock deliberately runs from the last *accepted* packet, not the last
/// received one: refreshing it on stale packets would lock a restarted console
/// out forever (its every packet would look stale and keep the window "busy").
/// The flip side — a sender stuck replaying an old sequence gets adopted after
/// this window too — is harmless: resyncing to whatever the sender now emits is
/// always safe, and duplicate values are deduplicated by the merge/flush layers.
const SEQ_IDLE_RESET: Duration = Duration::from_secs(2);

/// Cap on tracked (source, port-address) keys — purely defensive (a scanner
/// spraying spoofed sources shouldn't grow the map unbounded). Resetting simply
/// resyncs sequence tracking, which is always safe.
const SEQ_MAX_KEYS: usize = 1024;

/// Out-of-order ArtDmx suppression, per the Art-Net spec's Sequence field:
/// the sender increments 1..=255 (wrapping back to 1; 0 means "sequencing
/// disabled"), and a receiver should drop packets that arrive behind the newest
/// one seen. Tracked per (source IP, port-address) so multiple controllers
/// don't fight each other's counters.
///
/// A packet is **fresh** when sequencing is disabled (0), the key is new, the
/// key has been idle past the reset window, or the wrapped distance from the
/// last accepted sequence is 1..=127. Duplicates (distance 0) and late packets
/// (distance ≥128) are stale.
pub struct SeqTracker {
    idle_reset: Duration,
    map: HashMap<(IpAddr, u16), (u8, Instant)>,
}

impl SeqTracker {
    pub fn new() -> Self {
        Self::with_idle_reset(SEQ_IDLE_RESET)
    }

    /// Testing hook: a custom idle-reset window (`Duration::ZERO` = every packet
    /// resyncs, i.e. tracking disabled).
    pub fn with_idle_reset(idle_reset: Duration) -> Self {
        Self { idle_reset, map: HashMap::new() }
    }

    /// Record `seq` for (`src`, `port_address`) and report whether the packet
    /// should be processed (`true`) or dropped as stale/duplicate (`false`).
    pub fn is_fresh(&mut self, src: IpAddr, port_address: u16, seq: u8) -> bool {
        if seq == 0 {
            return true; // sender disabled sequencing
        }
        let now = Instant::now();
        // Cap the map exactly as the merger caps its universe map: only a key
        // that would GROW it can trigger the reset. `len() > CAP` let the map
        // reach CAP + 1, and it also cleared on packets from senders already
        // tracked — throwing away live sequence state the cap never needed to
        // reclaim.
        let key = (src, port_address);
        if !self.map.contains_key(&key) && self.map.len() >= SEQ_MAX_KEYS {
            self.map.clear();
        }
        match self.map.get_mut(&key) {
            Some((last, seen)) if now.duration_since(*seen) < self.idle_reset => {
                // Wrapped distance from the last accepted sequence. The sender
                // skips 0 on wrap (…, 0xFF, 0x01, …), which wrapping_sub handles
                // naturally (0x01 − 0xFF = 2).
                let delta = seq.wrapping_sub(*last);
                let fresh = (1..=127).contains(&delta);
                if fresh {
                    *last = seq;
                    *seen = now;
                }
                fresh
            }
            _ => {
                // New key, or idle past the reset window: accept and resync.
                self.map.insert(key, (seq, now));
                true
            }
        }
    }
}

impl Default for SeqTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Format the first `n` channels of a DMX buffer for log lines, e.g.
/// `[100  50 …]`. Shared by `monitor`'s per-packet and merged log lines.
pub fn preview(data: &[u8], n: usize) -> String {
    let cells: Vec<String> = data.iter().take(n).map(|b| format!("{b:3}")).collect();
    format!("[{}{}]", cells.join(" "), if data.len() > n { " …" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid ArtDmx datagram for tests.
    fn make(seq: u8, sub_uni: u8, net: u8, data: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(ARTNET_ID);
        p.extend_from_slice(&OP_DMX.to_le_bytes()); // opcode LE
        p.extend_from_slice(&[0x00, 14]); // protver BE
        p.push(seq);
        p.push(0); // physical
        p.push(sub_uni);
        p.push(net);
        p.extend_from_slice(&(data.len() as u16).to_be_bytes()); // length BE
        p.extend_from_slice(data);
        p
    }

    #[test]
    fn parses_valid_packet() {
        let data = [10u8, 20, 30, 40];
        let pkt = parse_artdmx(&make(7, 0x00, 0x00, &data)).unwrap();
        assert_eq!(pkt.sequence, 7);
        assert_eq!(pkt.port_address, 0);
        assert_eq!(pkt.data, data);
    }

    #[test]
    fn port_address_composition() {
        // net=1, sub_uni=0x23 (sub-net 2, universe 3) -> 0x0123 = 291
        let pkt = parse_artdmx(&make(1, 0x23, 0x01, &[1, 2])).unwrap();
        assert_eq!(pkt.port_address, 0x0123);
        assert_eq!(split_port_address(pkt.port_address), (1, 2, 3));
    }

    #[test]
    fn rejects_bad_inputs() {
        // wrong id
        let mut bad = make(1, 0, 0, &[1, 2]);
        bad[0] = b'X';
        assert!(parse_artdmx(&bad).is_none());

        // wrong opcode (flip to 0x2000 ArtPoll)
        let mut poll = make(1, 0, 0, &[1, 2]);
        poll[8..10].copy_from_slice(&0x2000u16.to_le_bytes());
        assert!(parse_artdmx(&poll).is_none());

        // truncated body (claims length 4, only 2 bytes present)
        let mut trunc = make(1, 0, 0, &[1, 2]);
        trunc[16..18].copy_from_slice(&4u16.to_be_bytes());
        assert!(parse_artdmx(&trunc).is_none());

        // too short overall
        assert!(parse_artdmx(&[0u8; 10]).is_none());

        // zero length
        let zero = make(1, 0, 0, &[]);
        assert!(parse_artdmx(&zero).is_none());
    }

    #[test]
    fn accepts_full_512() {
        let data = vec![0xAB; 512];
        let pkt = parse_artdmx(&make(0, 0, 0, &data)).unwrap();
        assert_eq!(pkt.data.len(), 512);
    }

    #[test]
    fn encode_parse_round_trip() {
        let data = [1u8, 2, 3, 4, 5, 6];
        let pkt = parse_artdmx(&encode_artdmx(0x0123, 9, &data)).unwrap();
        assert_eq!(pkt.port_address, 0x0123);
        assert_eq!(pkt.sequence, 9);
        assert_eq!(pkt.data, data);
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([192, 168, 1, last])
    }

    #[test]
    fn seq_in_order_and_gaps_are_fresh() {
        let mut t = SeqTracker::new();
        assert!(t.is_fresh(ip(1), 0, 1));
        assert!(t.is_fresh(ip(1), 0, 2));
        assert!(t.is_fresh(ip(1), 0, 10)); // gaps (lost packets) are fine
    }

    #[test]
    fn seq_duplicates_and_stale_are_dropped() {
        let mut t = SeqTracker::new();
        assert!(t.is_fresh(ip(1), 0, 50));
        assert!(!t.is_fresh(ip(1), 0, 50)); // exact duplicate
        assert!(!t.is_fresh(ip(1), 0, 49)); // one behind
        assert!(!t.is_fresh(ip(1), 0, 50u8.wrapping_sub(127))); // far behind (delta 129... wraps ≥128)
        assert!(t.is_fresh(ip(1), 0, 51)); // stale packets didn't poison the counter
    }

    #[test]
    fn seq_wraps_past_255() {
        let mut t = SeqTracker::new();
        assert!(t.is_fresh(ip(1), 0, 0xFF));
        // Sender wraps 0xFF → 0x01 (0 is skipped): wrapping distance 2 = fresh.
        assert!(t.is_fresh(ip(1), 0, 0x01));
        assert!(!t.is_fresh(ip(1), 0, 0xFF)); // now 0xFF is behind
    }

    #[test]
    fn seq_zero_disables_sequencing() {
        let mut t = SeqTracker::new();
        assert!(t.is_fresh(ip(1), 0, 0));
        assert!(t.is_fresh(ip(1), 0, 0)); // always fresh
        // ...and doesn't disturb a real counter on the same key.
        assert!(t.is_fresh(ip(1), 0, 7));
        assert!(t.is_fresh(ip(1), 0, 0));
        assert!(!t.is_fresh(ip(1), 0, 6));
    }

    #[test]
    fn seq_keys_are_independent() {
        let mut t = SeqTracker::new();
        assert!(t.is_fresh(ip(1), 0, 100));
        assert!(t.is_fresh(ip(2), 0, 5)); // different source
        assert!(t.is_fresh(ip(1), 1, 5)); // different port-address
        assert!(!t.is_fresh(ip(1), 0, 99)); // original key still tracks
    }

    #[test]
    fn seq_idle_reset_resyncs_a_restarted_sender() {
        // Zero idle window = every packet is past the window ⇒ always resync.
        let mut t = SeqTracker::with_idle_reset(Duration::ZERO);
        assert!(t.is_fresh(ip(1), 0, 200));
        assert!(t.is_fresh(ip(1), 0, 1)); // would be stale, but the key had "idled"
    }

    #[test]
    fn seq_key_cap_never_exceeds_the_limit_or_evicts_a_live_sender() {
        // The cap is defensive (a spoofed-source flood must not grow the map
        // without bound), so it must (a) actually hold at SEQ_MAX_KEYS — the old
        // `len() > CAP` check let the map reach CAP + 1 — and (b) only fire for
        // a key that would GROW the map. It used to clear on ANY packet once
        // over the cap, discarding live sequence state for senders that were
        // already tracked and did not need reclaiming.
        let mut t = SeqTracker::new();
        for i in 0..SEQ_MAX_KEYS {
            assert!(t.is_fresh(ip(1), i as u16, 1));
            assert!(t.map.len() <= SEQ_MAX_KEYS, "cap exceeded at key {i}");
        }
        assert_eq!(t.map.len(), SEQ_MAX_KEYS, "the map should be exactly full");

        // Sitting exactly at the cap, an ALREADY-TRACKED key must be served from
        // its existing state — not trigger a reset. Proof: a stale sequence is
        // still rejected, which is only possible if its entry survived.
        assert!(!t.is_fresh(ip(1), 0, 1), "duplicate must still be dropped");
        assert_eq!(t.map.len(), SEQ_MAX_KEYS, "a known key must not reset the map");

        // A genuinely NEW key is what reclaims: the map resets and restarts.
        assert!(t.is_fresh(ip(2), 9999, 1));
        assert_eq!(t.map.len(), 1);
    }

    /// Regression test for the Windows oversized-datagram bug: `recv_from` into
    /// a too-small buffer FAILS on Windows (WSAEMSGSIZE, os error 10040) instead
    /// of truncating like Linux, and any listener error is fatal by design — so
    /// with the old 1024-byte buffer, one junk datagram from anywhere on the LAN
    /// killed the whole bridge. The buffer now covers the maximum UDP payload:
    /// `serve` must survive oversized garbage and keep processing ArtDmx.
    #[tokio::test]
    async fn serve_survives_oversized_datagrams() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(serve(sock, move |_src, pkt| {
            let _ = tx.send(pkt);
        }));

        let send = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // 2000 bytes: the size empirically confirmed to kill the old listener.
        send.send_to(&[0u8; 2000], addr).await.unwrap();
        // The maximum UDP payload (65,507 B): must be received whole, not error.
        // (Guarded send: some loopback stacks cap the send size — the datagram
        // not being sendable at all is fine, we only care that receiving one
        // never kills the listener.)
        let _ = send.send_to(&vec![0xAAu8; 65_507], addr).await;

        // A valid ArtDmx after the garbage proves the listener is still alive.
        // Sent in a retry loop: the 65,507-byte junk can transiently exhaust the
        // OS receive buffer (Windows defaults to 64 KiB) and drop the next
        // datagram — ordinary lossy UDP, not a listener death, and exactly what
        // a real console's continuous refresh rides out.
        let valid = encode_artdmx(0x0001, 1, &[10, 20, 30]);
        let deadline = Instant::now() + Duration::from_secs(10);
        let received = loop {
            assert!(
                Instant::now() < deadline,
                "listener must still be alive after oversized datagrams"
            );
            send.send_to(&valid, addr).await.unwrap();
            if let Ok(Some(pkt)) =
                tokio::time::timeout(Duration::from_millis(250), rx.recv()).await
            {
                break pkt;
            }
        };
        assert_eq!(received.port_address, 0x0001);
        assert_eq!(received.data, vec![10, 20, 30]);
        assert!(!server.is_finished(), "serve() must not have returned");
        server.abort();
    }
}
