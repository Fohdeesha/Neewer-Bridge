//! ArtNet receive path. We only need **ArtDmx** (OpCode `0x5000`) — see
//! NOTES.md §7. Everything else (ArtPoll etc.) is ignored for now.
//!
//! Port-Address is the 15-bit `Net(7) | Sub-Net(4) | Universe(4)` value that a
//! light's config binds to; the wire splits it across the `Net` byte and the
//! `SubUni` byte (Sub-Net high nibble + Universe low nibble).

use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tracing::{debug, info, trace};

/// Standard ArtNet UDP port.
pub const ARTNET_PORT: u16 = 6454;

const ARTNET_ID: &[u8; 8] = b"Art-Net\0";
const OP_DMX: u16 = 0x5000;

/// A decoded ArtDmx packet (only the fields we use).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtDmx {
    /// 0x01..=0xff packet sequence (0 = sequencing disabled).
    pub sequence: u8,
    /// Source physical port (informational; used for merge discrimination).
    pub physical: u8,
    /// 15-bit Port-Address (Net/Sub-Net/Universe combined).
    pub port_address: u16,
    /// DMX channel data, 1..=512 bytes.
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
    // Length is big-endian, must be even and in 2..=512 (we accept 1..=512).
    let length = u16::from_be_bytes([buf[16], buf[17]]) as usize;
    if length == 0 || length > 512 {
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
/// test command (and the parser round-trip test). `data` is truncated to 512.
pub fn encode_artdmx(port_address: u16, sequence: u8, data: &[u8]) -> Vec<u8> {
    let len = data.len().min(512);
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

/// Bind the ArtNet UDP listener and call `on_packet` for every valid ArtDmx
/// datagram received. Runs until the socket errors or the task is cancelled.
pub async fn listen<F>(bind_ip: &str, port: u16, mut on_packet: F) -> Result<()>
where
    F: FnMut(SocketAddr, ArtDmx),
{
    let addr = format!("{bind_ip}:{port}");
    let sock = UdpSocket::bind(&addr)
        .await
        .with_context(|| format!("binding ArtNet listener to {addr}"))?;
    info!(%addr, "ArtNet listener bound");

    // Max ArtDmx = 18 header + 512 data = 530 bytes; round up.
    let mut buf = vec![0u8; 1024];
    loop {
        let (n, src) = sock.recv_from(&mut buf).await.context("recv_from")?;
        match parse_artdmx(&buf[..n]) {
            Some(pkt) => on_packet(src, pkt),
            None => trace!(%src, bytes = n, "ignored non-ArtDmx datagram"),
        }
    }
}

/// Log a one-line summary of an ArtDmx packet (used by the `monitor` command and
/// useful generally for debugging).
pub fn log_packet(src: SocketAddr, pkt: &ArtDmx) {
    let (net, sub_net, universe) = split_port_address(pkt.port_address);
    let preview: Vec<String> = pkt.data.iter().take(8).map(|b| format!("{b:3}")).collect();
    debug!(
        %src,
        port = pkt.port_address,
        net,
        sub_net,
        universe,
        seq = pkt.sequence,
        len = pkt.data.len(),
        "ArtDmx [{}{}]",
        preview.join(" "),
        if pkt.data.len() > 8 { " …" } else { "" },
    );
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
}
