//! Per-segment PIXEL control (opcode `0xB0`, MAC-addressed) — the "set different
//! areas of the tube to different colours" mode of TL-series pixel fixtures
//! (TL120C verified). This is a DIFFERENT subsystem from Streamer (`0xBF`/`0xC0`,
//! TL60-only, which black-screens a TL120C); pixel `0xB0` works over a direct BLE
//! connection.
//!
//! Reverse-engineered from an HCI capture of NEEWER Studio driving a TL120C and
//! verified live (red+blue, magenta+cyan, fire palettes all rendered as distinct
//! bands along the tube). Cross-checked against `verygeeky/neewer-lights`
//! (`pixel.py`, `docs/PROTOCOL.md`) — see NOTES.md §3 / protocol-analysis.md.
//!
//! # Wire format
//! "Pixel" is NOT a raw per-LED framebuffer: you pick an effect id + scalar
//! params + a palette of up to 8 colour blocks; the firmware renders that named
//! effect, distributing the palette colours as spatial bands along the LEDs.
//! Each logical frame is a standard Neewer frame `78 B0 <LEN> <MAC6>
//! <effectData…> <ck>` where `LEN = 6 (MAC) + effectData.len()` (the app's own
//! rule; note `pixel.py` mis-computes the params LEN as `0x0a` — the firmware
//! tolerates it by reading offsets, but the real capture is `0x0d`, which we use).
//!
//! `effectData` is `[effectId, subIndex, payload…]`:
//! - subIndex 0 = scalar params. For effect 1 (ColorReplacement):
//!   `[bri, colorNum, speed, dir, running]` (from `cn.java:2488`).
//! - subIndex 1 = palette colour blocks 0..5 (each a 3-byte block).
//! - subIndex 2 = palette colour blocks 6..7 (only if >6 colours).
//!
//! Each sub-frame is written as its own GATT write (the app spaces them ~80 ms;
//! long palettes are additionally chunked to ≤20 payload bytes and reassembled by
//! the device using the header LEN — see the actor/driver layer).

use super::with_checksum;

const PREFIX: u8 = 0x78;
const TAG_PIXEL: u8 = 0xB0;

/// Default effect used for static per-segment colour: effect 1 (ColorReplacement).
pub const EFFECT_COLOR_REPLACEMENT: u8 = 1;

/// One palette colour block — the 3-byte `createColoByteArray` encoding shared by
/// every pixel/streamer colour payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    /// HSI segment: hue 0..=359, saturation 0..=100.
    Hsi { hue: u16, sat: u8 },
    /// White segment: raw CCT value (e.g. 32=3200K) + GM (-50..=50, wire +50).
    Cct { cct: u8, gm: i8 },
    /// Dark/off segment.
    Off,
}

impl Block {
    /// Encode this block as its 3 wire bytes.
    ///
    /// - HSI: `[(hue>>8)&0x0F | 0x10, hue&0xFF, sat]` — the `0x10` high-nibble flag
    ///   marks the block HSI (vs CCT's `0x00`, off's `0x20`).
    /// - CCT: `[0x00, cct, gm+50]`
    /// - Off: `[0x20, 0x00, 0x00]`
    pub fn bytes(self) -> [u8; 3] {
        match self {
            Block::Hsi { hue, sat } => {
                let hue = hue % 360;
                [
                    (((hue >> 8) & 0x0F) as u8) | 0x10,
                    (hue & 0xFF) as u8,
                    sat.min(100),
                ]
            }
            Block::Cct { cct, gm } => [0x00, cct, super::gm_byte(gm)],
            Block::Off => [0x20, 0x00, 0x00],
        }
    }
}

/// Wrap one `effectData` array into a full `78 B0 <LEN> <MAC6> <effectData> <ck>`
/// frame. `LEN = 6 + effect_data.len()` (MAC bytes + the effect-data bytes),
/// matching NEEWER Studio's own captured frames.
fn frame(mac: [u8; 6], effect_data: &[u8]) -> Vec<u8> {
    let len = (6 + effect_data.len()) as u8;
    let mut f = vec![PREFIX, TAG_PIXEL, len];
    f.extend_from_slice(&mac);
    f.extend_from_slice(effect_data);
    with_checksum(f)
}

/// Wrap an arbitrary `effect_data` byte slice into a full `0xB0` pixel frame —
/// public escape hatch for building effects this module doesn't model directly
/// (used by the `test --set pixfx:<id>` hardware probe).
pub fn raw_frame(mac: [u8; 6], effect_data: &[u8]) -> Vec<u8> {
    frame(mac, effect_data)
}

/// The scalar-params sub-frame (subIndex 0) for effect 1 (ColorReplacement):
/// `78 B0 0D <MAC6> 01 00 <bri> <colorNum> <speed> <dir> <running> <ck>`.
///
/// `bri` 0..=100 (master brightness), `color_num` = number of palette colours,
/// `speed` 0..=100 (0x2e≈46 observed), `dir` 0/1 travel direction, `running`
/// 0/1. Captured app values were `bri=0x32 speed=0x2e dir=01 running=01`.
pub fn effect1_params(mac: [u8; 6], bri: u8, color_num: u8, speed: u8, dir: u8, running: u8) -> Vec<u8> {
    frame(
        mac,
        &[
            EFFECT_COLOR_REPLACEMENT,
            0x00,
            bri.min(100),
            color_num,
            speed,
            dir,
            running,
        ],
    )
}

/// Params sub-frame for the moving effects — SingleColorMoving (wire 3),
/// TwoColorMoving (4), ThreeColorMoving (5): `[wire, 0, colorBri, bgBri, way,
/// speed, dir, movement, running]` (`createPixelEffectData` cases 3-5). `way` 0 =
/// one direction, 1 = bounce; `movement` is an effect-specific motion mode. The
/// palette sub-frame is `background` then the 1/2/3 moving colours.
#[allow(clippy::too_many_arguments)]
pub fn moving_params(
    mac: [u8; 6], wire: u8, color_bri: u8, bg_bri: u8, way: u8, speed: u8, dir: u8, movement: u8, running: u8,
) -> Vec<u8> {
    frame(mac, &[wire, 0x00, color_bri.min(100), bg_bri.min(100), way, speed, dir, movement, running])
}

/// Params sub-frame for the "Fire" effect (wire 7): `[7, 0, briLo, briHi, bgBri,
/// speed, orientation, running]` (`createPixelEffectData` case 7). `briLo`/`briHi`
/// bound the flicker brightness range. The palette sub-frame is `background` then
/// the fire colour.
pub fn fire_params(mac: [u8; 6], bri_lo: u8, bri_hi: u8, bg_bri: u8, speed: u8, orientation: u8, running: u8) -> Vec<u8> {
    frame(mac, &[7, 0x00, bri_lo.min(100), bri_hi.min(100), bg_bri.min(100), speed, orientation, running])
}

/// A palette sub-frame: `78 B0 <LEN> <MAC6> <effectId> <subIndex> <blocks…> <ck>`.
/// `sub_index` is 1 for colours 0..5 and 2 for colours 6..7.
pub fn palette(mac: [u8; 6], effect_id: u8, sub_index: u8, blocks: &[Block]) -> Vec<u8> {
    let mut data = vec![effect_id, sub_index];
    for b in blocks {
        data.extend_from_slice(&b.bytes());
    }
    frame(mac, &data)
}

/// `runningStatus` PLAY — the only usefully-supported state on the TL120C. STOP
/// blanks the output and PAUSE is ignored (the effect keeps animating), so a
/// truly static per-segment render is NOT available over BLE on this fixture:
/// every pixel effect animates (or collapses to a single colour at speed 0).
/// See NOTES.md §3.3 / §10.
pub const RUN_PLAY: u8 = 1;

/// The pixel effects that render over a DIRECT BLE connection on the TL120C
/// (`commandType == 2`). Hardware-verified 2026-07-01: of the app's 10 pixel
/// effects, exactly these 5 work when the `0xB0` frame is sent straight to the
/// light; the other 5 (ColorAlternate, Colorful, ColorGradient, Trail, ColorShift)
/// are silently ignored — they only work relayed through the 2.4G hub. See
/// NOTES.md §3.3.
pub const EFFECT_COLOR_REPLACEMENT_ID: u8 = 1;
pub const EFFECT_SINGLE_MOVING: u8 = 3;
pub const EFFECT_TWO_MOVING: u8 = 4;
pub const EFFECT_THREE_MOVING: u8 = 5;
pub const EFFECT_FIRE: u8 = 7;

/// Build the ordered GATT-write frames to render `blocks` across the tube using
/// pixel `effect` (one of the 5 working ids above; anything else → ColorReplacement):
/// the params sub-frame, then the palette sub-frame(s). `bri` is master brightness
/// (0..=100); `speed` is the animation rate (0 collapses to a near-static single
/// colour, >0 animates); `dir` is the flow direction / fire orientation.
///
/// Segment semantics depend on the effect:
/// - **ColorReplacement**: `blocks` = the palette (up to 8 spatial colour bands).
/// - **Moving (Single/Two/Three)**: `blocks[0]` = background, `blocks[1..]` = the
///   1/2/3 moving colours.
/// - **Fire**: `blocks[0]` = background, `blocks[1]` = fire colour.
pub fn paint(mac: [u8; 6], blocks: &[Block], bri: u8, effect: u8, speed: u8, dir: u8) -> Vec<Vec<u8>> {
    let blocks = &blocks[..blocks.len().min(8)];
    let bg = blocks.first().copied().unwrap_or(Block::Off);
    match effect {
        EFFECT_SINGLE_MOVING | EFFECT_TWO_MOVING | EFFECT_THREE_MOVING => {
            let n = (effect - 2) as usize; // 3→1, 4→2, 5→3 moving colours
            let params = moving_params(mac, effect, bri, bri, 0, speed, dir, 0, RUN_PLAY);
            let mut colours = vec![bg];
            colours.extend(blocks.iter().skip(1).take(n).copied());
            // Pad with red if the palette is short so the effect still renders.
            while colours.len() < n + 1 {
                colours.push(Block::Hsi { hue: 0, sat: 100 });
            }
            vec![params, palette(mac, effect, 1, &colours)]
        }
        EFFECT_FIRE => {
            let fire = blocks.get(1).copied().unwrap_or(Block::Hsi { hue: 30, sat: 100 });
            let params = fire_params(mac, bri / 2, bri, bri / 2, speed, dir, RUN_PLAY);
            vec![params, palette(mac, EFFECT_FIRE, 1, &[bg, fire])]
        }
        _ => {
            // ColorReplacement (default): the 8-segment spatial palette.
            let params = effect1_params(mac, bri, blocks.len() as u8, speed, dir, RUN_PLAY);
            let mut frames = vec![params];
            frames.push(palette(mac, EFFECT_COLOR_REPLACEMENT, 1, &blocks[..blocks.len().min(6)]));
            if blocks.len() > 6 {
                frames.push(palette(mac, EFFECT_COLOR_REPLACEMENT, 2, &blocks[6..]));
            }
            frames
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect::<Vec<_>>().join("")
    }

    // The TL120C from the verified captures.
    const MAC: [u8; 6] = [0xCC, 0x8D, 0xBE, 0xBB, 0x25, 0xB0];

    #[test]
    fn block_encodings_match_captures() {
        // hue 0 (red) -> flag 0x10, lo 0x00, sat 100
        assert_eq!(Block::Hsi { hue: 0, sat: 100 }.bytes(), [0x10, 0x00, 100]);
        // hue 240 (blue) -> 0x00F0: flag 0x10, lo 0xF0
        assert_eq!(Block::Hsi { hue: 240, sat: 100 }.bytes(), [0x10, 0xF0, 100]);
        // hue 300 -> 0x012C: high byte 1 | 0x10 = 0x11, lo 0x2C
        assert_eq!(Block::Hsi { hue: 300, sat: 100 }.bytes(), [0x11, 0x2C, 100]);
        // hue 115 / 90 from the captured app palette
        assert_eq!(Block::Hsi { hue: 115, sat: 100 }.bytes(), [0x10, 0x73, 100]);
        assert_eq!(Block::Hsi { hue: 90, sat: 100 }.bytes(), [0x10, 0x5A, 100]);
        // CCT 3200K neutral GM
        assert_eq!(Block::Cct { cct: 32, gm: 0 }.bytes(), [0x00, 32, 50]);
        // off
        assert_eq!(Block::Off.bytes(), [0x20, 0x00, 0x00]);
    }

    #[test]
    fn params_frame_matches_captured_app_frame() {
        // Captured (app's own checksum): 78 b0 0d cc8dbebb25b0 01 00 32 02 2e 01 01 41
        let f = effect1_params(MAC, 0x32, 0x02, 0x2e, 0x01, 0x01);
        assert_eq!(hex(&f), "78b00dcc8dbebb25b001003202 2e0101 41".replace(' ', ""));
    }

    #[test]
    fn palette_frame_matches_captured_app_frame() {
        // Captured: 78 b0 0e cc8dbebb25b0 01 01 10 73 64 10 5a 64 94  (hue115 + hue90)
        let f = palette(
            MAC,
            EFFECT_COLOR_REPLACEMENT,
            1,
            &[Block::Hsi { hue: 115, sat: 100 }, Block::Hsi { hue: 90, sat: 100 }],
        );
        assert_eq!(hex(&f), "78b00ecc8dbebb25b00101107364105a6494");
    }

    #[test]
    fn crafted_red_blue_matches_verified_frame() {
        // Crafted-and-rendered red+blue: 78 b0 0e cc8dbebb25b0 01 01 10 00 64 10 f0 64 b7
        let f = palette(
            MAC,
            EFFECT_COLOR_REPLACEMENT,
            1,
            &[Block::Hsi { hue: 0, sat: 100 }, Block::Hsi { hue: 240, sat: 100 }],
        );
        assert_eq!(hex(&f), "78b00ecc8dbebb25b00101100064 10f064 b7".replace(' ', ""));
    }

    #[test]
    fn len_byte_is_mac_plus_effectdata() {
        // Both captured frames: LEN = 6 (MAC) + effectData length.
        let p = effect1_params(MAC, 0x32, 2, 0x2e, 1, 1); // effectData = 7 bytes
        assert_eq!(p[2], 0x0d);
        let q = palette(MAC, 1, 1, &[Block::Off]); // effectData = 2 + 3 = 5 bytes
        assert_eq!(q[2], 0x0b);
    }

    #[test]
    fn paint_effect1_splits_palette_past_six_colours() {
        let blocks: Vec<Block> = (0..8).map(|i| Block::Hsi { hue: i * 40, sat: 100 }).collect();
        let frames = paint(MAC, &blocks, 100, EFFECT_COLOR_REPLACEMENT, 30, 1);
        // params + palette(0..6) + palette(6..8) = 3 frames.
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0][9], 1); // effectId in params
        assert_eq!(frames[0][15], RUN_PLAY); // running byte
        assert_eq!(frames[0][12], 8); // colorNum
        assert_eq!(frames[1][9], 1); // palette effectId
        assert_eq!(frames[1][10], 1); // subIndex 1
        assert_eq!(frames[2][10], 2); // subIndex 2
    }

    #[test]
    fn paint_moving_effect_uses_bg_plus_n_colours() {
        // TwoColorMoving (wire 4): params [4,0,colorBri,bgBri,way,speed,dir,move,run]
        // then palette = background + 2 moving colours.
        let blocks = [
            Block::Off,                       // background
            Block::Hsi { hue: 0, sat: 100 },  // moving 1
            Block::Hsi { hue: 60, sat: 100 }, // moving 2
        ];
        let frames = paint(MAC, &blocks, 100, EFFECT_TWO_MOVING, 40, 1);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0][9], 4); // params wire id
        assert_eq!(frames[0][10], 0); // subIndex 0
        // effectData: [4,0,colorBri,bgBri,way,speed,dir,movement,running] → running at [17]
        assert_eq!(frames[0][16], 0); // movement
        assert_eq!(frames[0][17], RUN_PLAY); // running byte (last param)
        assert_eq!(frames[1][9], 4); // palette wire id
        assert_eq!(frames[1][10], 1); // subIndex 1
        // palette effectData after [wire,sub] = bg(3) + 2×3 = 9 bytes; LEN = 6+2+9=17.
        assert_eq!(frames[1][2], (6 + 2 + 9) as u8);
    }

    #[test]
    fn paint_fire_effect_is_params_plus_bg_and_fire() {
        let frames = paint(MAC, &[Block::Off, Block::Hsi { hue: 30, sat: 100 }], 100, EFFECT_FIRE, 20, 0);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0][9], 7); // fire wire id
        // params: [7,0, briLo=50, briHi=100, bgBri=50, speed=20, orientation=0, run]
        assert_eq!(&frames[0][11..14], &[50, 100, 50]);
        assert_eq!(frames[1][9], 7); // palette wire id
    }
}
