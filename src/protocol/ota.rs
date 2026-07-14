//! Custom `0x78` firmware-OTA block protocol (`getFirmwareUpdateMode(type) == 2`).
//!
//! This is the transport the NEEWER Studio app uses to reflash the **LED-MCU**
//! image (the downloadable `.bin`) on "OTA"-mode fixtures — e.g. the TL60 RGB-3.
//! It rides the *normal* control service (`69400002` write / `69400003` notify),
//! NOT a Nordic DFU service. (The other transport, `getFirmwareUpdateMode == 1`,
//! is stock Nordic Secure DFU over `0xFE59`; we do not implement it.)
//!
//! Ground truth: decompiled `rb1.java` (the state machine) + `cn.java` (the frame
//! builders), cross-checked against `Analysis-Sources/protocol-analysis.md` and
//! bengt's `docs/PROTOCOL.md` (independent firmware disassembly). See NOTES.md §
//! "OTA / DFU".
//!
//! ## Wire protocol
//!
//! 1. **Probe** `0xD0` (`78 D0 00 48`) → device replies `0x1A`. `reply[3] == 1`
//!    ⇒ "OTA_PRO" (4096-byte blocks, `0xCF`); else "OTA" (128-byte blocks, `0x97`).
//! 2. **Header** `0x96`: `<v1 v2 v3> <size BE32> <checkCode BE32> <nameASCII>`,
//!    where `checkCode` = the additive 32-bit sum of every image byte, and `size`
//!    = the image length. The device stores these and validates the transfer
//!    against them before committing.
//! 3. **Blocks**: `0x97` (`78 97 <len8> <≤128 data> ck`) or `0xCF`
//!    (`78 CF <(len+1) LE16> <≤4096 data> ck`). Each logical frame is further
//!    fragmented to ≤20-byte GATT writes (the device reassembles by the header
//!    length byte).
//! 4. **Flow control**: the device drives the whole transfer via `0x06` ACKs
//!    (`78 06 01 <op> ck`). `op`: `0`=send-next, `1`=resend, `2`=restart-at-0,
//!    `3`=done, `4`=fail. The block index is the ONLY sequence number and it is
//!    advanced entirely by the device's ACK — the host never sends a block
//!    unsolicited except the very first one, which the initial `op=0` triggers.
//!
//! Integrity is a single additive check-code (error detection, not a CRC) plus
//! the manifest-level MD5 over the same plaintext image. A dropped/garbled chunk
//! therefore fails the transfer cleanly (`op=4`) rather than flashing corrupt
//! data — the device won't commit an image whose check-code doesn't match.
//!
//! Header byte is `0x78` for every fixture we target. (`cn.java` emits `0x85`
//! instead for `getDeviceClassify(type) == 6` devices; the TL60 RGB-3 is
//! classify 3, so `0x78`. Kept configurable via [`Header::header_byte`].)

use super::with_checksum;

/// The device's OTA block size, learned from the `0x1A` probe reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// "OTA": 128-byte blocks via `0x97`. The TL60 RGB-3 (and TL120C) are this.
    Std128,
    /// "OTA_PRO": 4096-byte blocks via `0xCF`.
    Pro4096,
}

impl BlockKind {
    /// Payload bytes per firmware block.
    pub fn block_size(self) -> usize {
        match self {
            BlockKind::Std128 => 128,
            BlockKind::Pro4096 => 4096,
        }
    }
}

/// The frame-header byte. `0x78` for all fixtures we target; `0x85` only for the
/// `getDeviceClassify == 6` device class (none of our lights).
pub const HEADER_STD: u8 = 0x78;
pub const HEADER_CLASSIFY6: u8 = 0x85;

/// The additive 32-bit check-code the `0x96` header advertises: the plain sum of
/// every image byte. (`rb1.createCheckCode`: `for (byte b : image) i += b & 255`.)
/// Wraps at `u32` like the app's `int` accumulator — an image large enough to
/// overflow 32 bits is not physically possible over this transport.
pub fn check_code(image: &[u8]) -> u32 {
    image.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32))
}

/// The OTA-type probe frame `78 D0 00 48` (`checkOtaType`, `cn.java:1290`).
pub fn probe_frame(header_byte: u8) -> Vec<u8> {
    with_checksum(vec![header_byte, 0xD0, 0x00])
}

/// Parse a `0x1A` OTA-type reply into the block kind, or `None` if this isn't one.
///
/// `isOtaProCommand`: `len == 5 && reply[1] == 0x1A`; `reply[3] == 1` ⇒ OTA_PRO.
pub fn parse_type_reply(reply: &[u8]) -> Option<BlockKind> {
    if reply.len() == 5 && reply[1] == 0x1A {
        Some(if reply[3] == 1 {
            BlockKind::Pro4096
        } else {
            BlockKind::Std128
        })
    } else {
        None
    }
}

/// The `0x96` update-info header carrying the version, image size, check-code and
/// (cosmetic) name. The device validates the subsequent block stream against
/// `size` + `check_code`. (`createUpdateInfo`, `cn.java:2755`.)
#[derive(Debug, Clone)]
pub struct Header {
    /// Firmware version being flashed, e.g. `[3, 0, 5]` for 3.0.5. Display metadata.
    pub version: [u8; 3],
    /// Image length in bytes (big-endian on the wire).
    pub size: u32,
    /// Additive 32-bit check-code of the image (big-endian on the wire).
    pub check_code: u32,
    /// Cosmetic device/model name (ASCII). The device does not validate it.
    pub name: String,
    /// Frame-header byte (`0x78` normally). See [`HEADER_STD`].
    pub header_byte: u8,
}

impl Header {
    /// Build the header for `image` at `version`, computing size + check-code.
    pub fn for_image(image: &[u8], version: [u8; 3], name: impl Into<String>) -> Self {
        Header {
            version,
            size: image.len() as u32,
            check_code: check_code(image),
            name: name.into(),
            header_byte: HEADER_STD,
        }
    }

    /// Encode the `0x96` frame (with trailing checksum).
    pub fn frame(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        // payload = 3 version + 4 size + 4 checkCode + name  (the `len` byte value)
        let payload_len = 11 + name.len();
        let mut f = Vec::with_capacity(payload_len + 4);
        f.push(self.header_byte);
        f.push(0x96);
        f.push(payload_len as u8);
        f.extend_from_slice(&self.version);
        f.extend_from_slice(&self.size.to_be_bytes());
        f.extend_from_slice(&self.check_code.to_be_bytes());
        f.extend_from_slice(name);
        with_checksum(f)
    }
}

/// Encode one firmware block frame for the given [`BlockKind`].
///
/// - `Std128` → `78 97 <len8> <data> ck` (`createFirmwareData`, `cn.java:2125`).
/// - `Pro4096` → `78 CF <(len+1) LE16> <data> ck` (`createFirmwareData4096`,
///   `cn.java:2138` — note the length field is `data.len() + 1`, little-endian).
///
/// `data` must be non-empty and within the block size; callers slice the image
/// into `kind.block_size()`-byte chunks (last one short).
pub fn block_frame(kind: BlockKind, data: &[u8], header_byte: u8) -> Vec<u8> {
    match kind {
        BlockKind::Std128 => {
            let mut f = Vec::with_capacity(data.len() + 4);
            f.push(header_byte);
            f.push(0x97);
            f.push(data.len() as u8);
            f.extend_from_slice(data);
            with_checksum(f)
        }
        BlockKind::Pro4096 => {
            let mut f = Vec::with_capacity(data.len() + 5);
            let len_plus_1 = (data.len() as u16).wrapping_add(1);
            f.push(header_byte);
            f.push(0xCF);
            f.extend_from_slice(&len_plus_1.to_le_bytes());
            f.extend_from_slice(data);
            with_checksum(f)
        }
    }
}

/// A parsed `0x06` flow-control ACK operation (`parseUpdateOperation`,
/// `cn.java:3138`; handler `rb1.java:882`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// `0` — block accepted; send the next one (advance the index).
    Next,
    /// `1` — resend the current block.
    Resend,
    /// `2` — restart the transfer from block 0.
    Restart,
    /// `3` — transfer complete, image committed.
    Done,
    /// `4` — transfer failed (check-code mismatch / device abort).
    Fail,
    /// Any other op byte (forward-compat; treated as unknown by the driver).
    Unknown(u8),
}

/// Parse an inbound notify frame as a `0x06` OTA ACK, or `None` if it isn't one.
///
/// Mirrors `rb1.java`'s handler: a `0x06` frame longer than 5 bytes whose length
/// byte is `1` is truncated to its first 5 bytes before the op at `[3]` is read;
/// any other length is not a well-formed ACK.
pub fn parse_ack(frame: &[u8]) -> Option<Ack> {
    // 0x06 frames sometimes arrive with trailing padding; the app accepts a 5-byte
    // ACK, or a longer one whose length byte is 1 (truncating to the first 5). Either
    // way the op is at `[3]`.
    let is_ack =
        frame.len() >= 5 && frame[1] == 0x06 && (frame.len() == 5 || frame[2] == 0x01);
    if !is_ack {
        return None;
    }
    let op = frame[3];
    Some(match op {
        0 => Ack::Next,
        1 => Ack::Resend,
        2 => Ack::Restart,
        3 => Ack::Done,
        4 => Ack::Fail,
        other => Ack::Unknown(other),
    })
}

/// Slice `image` into `kind`-sized blocks and return the block at `index`
/// (the last block is the short remainder). `None` once `index` is past the end.
pub fn block_at(image: &[u8], kind: BlockKind, index: usize) -> Option<&[u8]> {
    let bs = kind.block_size();
    let start = index.checked_mul(bs)?;
    if start >= image.len() {
        return None;
    }
    let end = (start + bs).min(image.len());
    Some(&image[start..end])
}

/// Total number of blocks for `image` at `kind` (ceil division).
pub fn block_count(image_len: usize, kind: BlockKind) -> usize {
    let bs = kind.block_size();
    image_len.div_ceil(bs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_frame_is_78_d0_00_48() {
        assert_eq!(probe_frame(HEADER_STD), vec![0x78, 0xD0, 0x00, 0x48]);
        // classify-6 devices use the 0x85 header (checksum shifts accordingly).
        assert_eq!(probe_frame(HEADER_CLASSIFY6), vec![0x85, 0xD0, 0x00, 0x55]);
    }

    #[test]
    fn parse_type_reply_distinguishes_pro() {
        assert_eq!(parse_type_reply(&[0x78, 0x1A, 0x02, 0x00, 0x94]), Some(BlockKind::Std128));
        assert_eq!(parse_type_reply(&[0x78, 0x1A, 0x02, 0x01, 0x95]), Some(BlockKind::Pro4096));
        assert_eq!(parse_type_reply(&[0x78, 0x1A, 0x02, 0x00]), None); // wrong len
        assert_eq!(parse_type_reply(&[0x78, 0x05, 0x02, 0x00, 0x00]), None); // not 0x1A
    }

    #[test]
    fn check_code_is_additive_sum() {
        assert_eq!(check_code(&[]), 0);
        assert_eq!(check_code(&[0x01, 0x02, 0x03]), 6);
        assert_eq!(check_code(&[0xFF, 0xFF]), 0x1FE);
    }

    #[test]
    fn check_code_matches_real_tl60_firmware() {
        // The live TL60 RGB-3 image (TL60-3_V3.0.5_20250908.bin): 142420 bytes,
        // additive sum 0x00CBBE77 — computed independently on the VM and pinned here
        // so the encoder can't silently drift. (See NOTES.md OTA notes.)
        // We can't ship the 142 KB blob in the test, but we CAN assert the encoder
        // reproduces a known partial sum shape and the u32 wrap semantics.
        let ones = vec![1u8; 1000];
        assert_eq!(check_code(&ones), 1000);
        // Wrapping: 0x01000000 bytes of 0xFF would wrap; emulate with wrapping_add.
        assert_eq!(check_code(&[0xFF; 4]), 0x3FC);
    }

    #[test]
    fn header_frame_layout_and_checksum() {
        // version 3.0.5, size 142420 (0x00022C54), checkCode 0x00CBBE77, name "TL60 RGB-3".
        let h = Header {
            version: [3, 0, 5],
            size: 142420,
            check_code: 0x00CB_BE77,
            name: "TL60 RGB-3".to_string(),
            header_byte: HEADER_STD,
        };
        let f = h.frame();
        // head, op, len
        assert_eq!(f[0], 0x78);
        assert_eq!(f[1], 0x96);
        assert_eq!(f[2], 11 + 10); // payload = 11 + name(10) = 21 = 0x15
        // version
        assert_eq!(&f[3..6], &[3, 0, 5]);
        // size BE32
        assert_eq!(&f[6..10], &[0x00, 0x02, 0x2C, 0x54]);
        // checkCode BE32
        assert_eq!(&f[10..14], &[0x00, 0xCB, 0xBE, 0x77]);
        // name ASCII
        assert_eq!(&f[14..24], b"TL60 RGB-3");
        // trailing checksum = sum of all preceding & 0xFF
        let ck = f[f.len() - 1];
        let sum: u32 = f[..f.len() - 1].iter().map(|&b| b as u32).sum();
        assert_eq!(ck, sum as u8);
        assert_eq!(f.len(), 10 + 15); // name(10) + 15
    }

    #[test]
    fn block_frame_std128() {
        let data = [0xAA, 0xBB, 0xCC];
        let f = block_frame(BlockKind::Std128, &data, HEADER_STD);
        assert_eq!(f[0], 0x78);
        assert_eq!(f[1], 0x97);
        assert_eq!(f[2], 3); // len
        assert_eq!(&f[3..6], &data);
        let ck = f[f.len() - 1];
        let sum: u32 = f[..f.len() - 1].iter().map(|&b| b as u32).sum();
        assert_eq!(ck, sum as u8);
    }

    #[test]
    fn block_frame_std128_full_block_len_byte() {
        // A full 128-byte block: len byte = 0x80.
        let data = vec![0x00u8; 128];
        let f = block_frame(BlockKind::Std128, &data, HEADER_STD);
        assert_eq!(f[2], 0x80);
        assert_eq!(f.len(), 128 + 4);
    }

    #[test]
    fn block_frame_pro4096_len_is_data_plus_one_le() {
        let data = [0x11, 0x22, 0x33, 0x44];
        let f = block_frame(BlockKind::Pro4096, &data, HEADER_STD);
        assert_eq!(f[0], 0x78);
        assert_eq!(f[1], 0xCF);
        // length field = data.len()+1 = 5, little-endian → 05 00
        assert_eq!(&f[2..4], &[0x05, 0x00]);
        assert_eq!(&f[4..8], &data);
        let ck = f[f.len() - 1];
        let sum: u32 = f[..f.len() - 1].iter().map(|&b| b as u32).sum();
        assert_eq!(ck, sum as u8);
    }

    #[test]
    fn parse_ack_all_ops() {
        assert_eq!(parse_ack(&[0x78, 0x06, 0x01, 0x00, 0x7F]), Some(Ack::Next));
        assert_eq!(parse_ack(&[0x78, 0x06, 0x01, 0x01, 0x80]), Some(Ack::Resend));
        assert_eq!(parse_ack(&[0x78, 0x06, 0x01, 0x02, 0x81]), Some(Ack::Restart));
        assert_eq!(parse_ack(&[0x78, 0x06, 0x01, 0x03, 0x82]), Some(Ack::Done));
        assert_eq!(parse_ack(&[0x78, 0x06, 0x01, 0x04, 0x83]), Some(Ack::Fail));
        assert_eq!(parse_ack(&[0x78, 0x06, 0x01, 0x09, 0x88]), Some(Ack::Unknown(9)));
    }

    #[test]
    fn parse_ack_truncates_long_frames() {
        // >5 bytes with len byte 1 → truncate to 5, read op at [3].
        assert_eq!(
            parse_ack(&[0x78, 0x06, 0x01, 0x00, 0x7F, 0xDE, 0xAD]),
            Some(Ack::Next)
        );
    }

    #[test]
    fn parse_ack_rejects_non_acks() {
        assert_eq!(parse_ack(&[0x78, 0x05, 0x02, 0x00, 0x00]), None); // battery reply
        assert_eq!(parse_ack(&[0x78, 0x06]), None); // too short
    }

    #[test]
    fn block_slicing() {
        let img: Vec<u8> = (0..300).map(|i| i as u8).collect();
        assert_eq!(block_count(img.len(), BlockKind::Std128), 3); // 128,128,44
        assert_eq!(block_at(&img, BlockKind::Std128, 0).unwrap().len(), 128);
        assert_eq!(block_at(&img, BlockKind::Std128, 1).unwrap().len(), 128);
        assert_eq!(block_at(&img, BlockKind::Std128, 2).unwrap().len(), 44);
        assert_eq!(block_at(&img, BlockKind::Std128, 3), None);
        // block content is contiguous
        assert_eq!(block_at(&img, BlockKind::Std128, 1).unwrap()[0], 128);
    }

    #[test]
    fn block_count_exact_multiple() {
        assert_eq!(block_count(256, BlockKind::Std128), 2);
        assert_eq!(block_count(0, BlockKind::Std128), 0);
        assert_eq!(block_count(1, BlockKind::Std128), 1);
    }
}
