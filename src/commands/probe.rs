//! `test` — the manual hardware-probe command and everything it drives.
//!
//! Split out of `commands/mod.rs` (which had grown to ~1500 lines) because this
//! is one self-contained job: connect to a single light and make it *prove*
//! something, one frame at a time, with a human watching. `mod.rs` keeps the
//! commands that read or write config and shovel ArtNet; the `ota` flasher has
//! its own module for the same reason.
//!
//! Two rules this module exists to enforce, both learned the hard way:
//!
//! - **Never report a frame you did not send.** Every id and parameter is
//!   range-checked at *both* ends before a plan is built ([`build_set_plan`]),
//!   and the encoder family is an enum ([`TestDriver`]) rather than a string
//!   matched with a fallback arm. A diagnostic tool that lies is worse than no
//!   tool.
//! - **Never leave a light strobing.** Every probe ends on a dim warm white; the
//!   light holds its last command forever.

use anyhow::{bail, Context, Result};
use btleplug::api::Characteristic;
use btleplug::platform::Peripheral;
use std::time::Duration;
use tracing::info;

use crate::ble;
use crate::config::{parse_mac, KNOWN_DRIVERS};
use crate::protocol::pixel::{self, Block};
use crate::protocol::{classic, home, infinity, queries};

use super::BLINK_STEP;

/// Which optional probes a `test` run should perform. A struct rather than a row
/// of positional `bool`s, so a call site can't silently transpose two of them.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestProbes {
    /// Cycle HSI red→green→blue (is this fixture RGB or bi-colour?).
    pub colors: bool,
    /// Probe the advanced modes: XY + a few FX effects.
    pub modes: bool,
    /// Probe per-segment PIXEL control (`0xB0`).
    pub pixel: bool,
    /// Read status (firmware/battery/temperature/state) and stop — non-mutating.
    pub status: bool,
}

/// Which encoder family `test` drives the light with.
///
/// Parsed once, up front. This used to be the raw `--driver` string matched at
/// three separate call sites, each with a `_ => classic` arm — so
/// `--driver classik` silently sent classic frames and reported nothing wrong,
/// and the "you used auto" hint at the end only fired for the exact literal.
/// Unknown values are now an error before the adapter is even opened, and the
/// exhaustive match means a future variant cannot be forgotten at one of the
/// three sites. (`add` and `Config::validate` already rejected unknown drivers;
/// `test` was the one path that didn't.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestDriver {
    /// Treated as classic for this manual test — real auto-detection by model
    /// lives in the driver layer, which needs a catalog entry `test` doesn't load.
    Auto,
    Classic,
    Infinity,
    Home,
}

impl TestDriver {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "auto" => TestDriver::Auto,
            "classic" => TestDriver::Classic,
            "infinity" => TestDriver::Infinity,
            "home" => TestDriver::Home,
            other => bail!(
                "unknown --driver {other:?}; expected one of {KNOWN_DRIVERS:?}"
            ),
        })
    }

    fn power(self, mac: [u8; 6], on: bool) -> Vec<u8> {
        match self {
            TestDriver::Infinity => infinity::power(mac, on),
            TestDriver::Home => home::power(on),
            TestDriver::Auto | TestDriver::Classic => classic::power(on),
        }
    }

    /// A known CCT baseline.
    ///
    /// ⚠️ cct4, NOT cct2. Hardware-proven on a TL120C (2026-08-25): the 2-byte
    /// `78 87 02 <brr> <cct>` form is IGNORED by a fixture that is running a
    /// pixel effect, while the app's 4-byte `78 87 04 <brr> <cct> <gm+50> 00`
    /// form exits it. Since this method's whole job in the `--pixel`/`--modes`
    /// probes is to return the light to a known CCT between latching effects,
    /// the 2-byte form silently did nothing there. cct4 is the frame NEEWER
    /// Studio itself sends and is accepted by every fixture tested (TL120C /
    /// TL21C / TL60 / TL97C). The `--set cct:` spec deliberately still sends the
    /// 2-byte form — that spec exists to probe *that* form (`cctgm:` is the
    /// 4-byte one).
    fn cct(self, mac: [u8; 6], brr: u8, cct: u8) -> Vec<u8> {
        match self {
            TestDriver::Infinity => infinity::cct(mac, brr, cct, 0),
            TestDriver::Home => home::cct(brr as u16 * 10, cct),
            TestDriver::Auto | TestDriver::Classic => classic::cct4(brr, cct, 0),
        }
    }

    /// Fully saturated HSI at full brightness — the RGB capability probe.
    fn hsi(self, mac: [u8; 6], hue: u16) -> Vec<u8> {
        match self {
            TestDriver::Infinity => infinity::hsi(mac, hue, 100, 100),
            TestDriver::Home => home::hsi(1000, hue, 100),
            TestDriver::Auto | TestDriver::Classic => classic::hsi(hue, 100, 100),
        }
    }
}

/// `test` — connect to one light and prove the BLE path end to end:
/// verify GATT, blink power (also serves as visual identify), then set a known
/// CCT. Uses our real protocol encoders so this validates them on hardware.
///
/// Everything that can be decided without hardware — the driver name and the
/// whole `--set` spec — is resolved before the adapter is touched, so a typo
/// fails in milliseconds instead of after a scan and a connect.
pub async fn test(
    adapter_selector: &str,
    mac: &str,
    driver: &str,
    seconds: u64,
    probes: TestProbes,
    set: Option<&str>,
) -> Result<()> {
    let TestProbes { colors, modes, pixel: pixel_probe, status } = probes;
    let mac_bytes = parse_mac(mac)?;
    let drv = TestDriver::parse(driver)?;
    // Resolve `--set` BEFORE opening the adapter. `build_set_plan` is pure — it
    // needs nothing but the MAC and the spec — yet it used to run only after
    // `find_by_mac` (a scan of up to `--seconds`) and a full connect + notify
    // subscribe, so a misplaced colon cost the whole cycle before saying so.
    // `artnet_send` validates its flags before it even binds a socket, for
    // exactly this reason.
    let plan = set.map(|spec| build_set_plan(mac_bytes, spec)).transpose()?;

    let adapter = ble::acquire_adapter(adapter_selector).await?;
    let peripheral = ble::find_by_mac(&adapter, mac, Duration::from_secs(seconds)).await?;
    let chars = ble::connect_and_verify(&peripheral).await?;

    // Did we take the blink/CCT path (the only one that actually uses `drv`)?
    // `--status` and `--set` build their frames without it, so the "you used
    // auto" hint below must not fire for them — as it didn't before, when they
    // returned early.
    let used_driver = !status && plan.is_none();

    // Everything from here holds the light, so it runs inside one block with a
    // SINGLE exit that always releases it. The `--status` and `--set` paths each
    // used to disconnect by hand while every `?` on the main path (a failed
    // notify subscribe, a failed blink write, a probe error) returned with the
    // connection still open — so the fixture only re-advertised once the OS
    // reaped the process. One owner, one disconnect, no path that can skip it.
    let res: Result<()> = async {
        if let Some(notify) = &chars.notify {
            ble::spawn_notify_logger(&peripheral, notify).await?;
        }

        // Status read (`--status`): query firmware version / battery / temperature /
        // state and print the decoded replies. Non-mutating — no blink, no colour
        // change — so it's safe to run anytime. Short-circuits before the blink/CCT
        // sequence.
        if status {
            return test_status(&peripheral, &chars.write, mac_bytes).await;
        }

        // Single-frame set (`--set SPEC`): send exactly one frame (or pixel palette)
        // and hold it, for guided one-at-a-time testing. The light keeps the state
        // after disconnect. Short-circuits before the blink/CCT sequence.
        if let Some(plan) = plan {
            return test_set(&peripheral, &chars.write, plan).await;
        }

        info!(driver, "blinking light to identify (3×) — watch which fixture flashes");
        for n in 1..=3 {
            info!(blink = n, "power OFF");
            ble::write_command(&peripheral, &chars.write, &drv.power(mac_bytes, false)).await?;
            tokio::time::sleep(BLINK_STEP).await;
            info!(blink = n, "power ON");
            ble::write_command(&peripheral, &chars.write, &drv.power(mac_bytes, true)).await?;
            tokio::time::sleep(BLINK_STEP).await;
        }

        info!("setting CCT: 5600K @ 50% brightness");
        // cct raw 56 = 5600K for most lights; 50 = 50% brightness.
        ble::write_command(&peripheral, &chars.write, &drv.cct(mac_bytes, 50, 56)).await?;

        if colors {
            probe_colors(&peripheral, &chars.write, drv, mac_bytes).await?;
        }
        if modes {
            probe_modes(&peripheral, &chars.write, drv, mac_bytes).await?;
        }
        if pixel_probe {
            probe_pixel(&peripheral, &chars.write, drv, mac_bytes).await?;
        }

        // Give notifications a moment to arrive, then leave cleanly.
        tokio::time::sleep(Duration::from_millis(800)).await;
        info!("test complete; disconnecting");
        Ok(())
    }
    .await;

    if let Err(e) = ble::disconnect(&peripheral).await {
        // Non-fatal — log and move on.
        tracing::warn!(error = %e, "disconnect returned an error");
    }
    res?;

    if used_driver && drv == TestDriver::Auto {
        tracing::warn!(
            "--driver was 'auto'; sent CLASSIC commands. If nothing happened, retry with \
             --driver infinity (newer lights) or --driver home (NH-* devices)."
        );
    }
    Ok(())
}

/// RGB capability probe (`--colors`): cycle saturated red→green→blue via HSI so a
/// human can SEE whether the light is RGB or bi-colour (a bi-colour fixture stays
/// white / ignores it).
async fn probe_colors(
    p: &Peripheral,
    write: &Characteristic,
    drv: TestDriver,
    mac: [u8; 6],
) -> Result<()> {
    info!("RGB capability probe — watch for colour changes (bi-color lights stay white/ignore)");
    for (hue, label) in [(0u16, "RED"), (120, "GREEN"), (240, "BLUE")] {
        info!(hue, "HSI {label} @ 100% sat / 100% brightness");
        ble::write_command(p, write, &drv.hsi(mac, hue)).await?;
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    // Return to a comfortable dim warm white so we never leave it on a colour.
    info!("restoring dim warm white (2700K @ 12%)");
    ble::write_command(p, write, &drv.cct(mac, 12, 27)).await?;
    Ok(())
}

/// Advanced-mode probe (`--modes`): exercise XY and a couple of FX effects (the
/// modes that work over direct BLE on Infinity fixtures). Both use MAC-addressed
/// frames — the TL120C ignores the direct `0xB9`/`0x88` forms. (RGBCW is not
/// probed here — use `--set rgbcwmac:…` for that.) Watch the light to confirm
/// each mode engages.
async fn probe_modes(
    p: &Peripheral,
    write: &Characteristic,
    drv: TestDriver,
    mac: [u8; 6],
) -> Result<()> {
    info!("XY probe — CIE coordinate (MAC-addressed 0xB7, as the bridge sends)");
    for (label, x, y) in
        [("D65 white", 3127u16, 3290u16), ("deep red", 6400, 3300), ("green", 3000, 6000)]
    {
        info!(x, y, "XY {label}");
        ble::write_command(p, write, &classic::xy_mac(mac, 100, x, y)).await?;
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }

    info!("FX probe — built-in effect engine (0x91, MAC-embedded)");
    for (label, bytes) in [
        ("Lightning", infinity::fx(mac, 1, 100, 56, 0, 0, 0, 5, 0, 0)),
        ("HUE-pulse (blue)", infinity::fx(mac, 9, 100, 0, 0, 240, 100, 6, 0, 0)),
        ("Cop-Car (red/blue)", infinity::fx(mac, 10, 100, 0, 0, 0, 0, 7, 2, 0)),
    ] {
        info!("FX {label}");
        ble::write_command(p, write, &bytes).await?;
        tokio::time::sleep(Duration::from_millis(2500)).await;
    }

    // FX may latch the light into effect mode; power-cycle restores direct
    // control (per protocol-analysis.md), then leave a dim warm white — never
    // leave the light strobing an effect (it holds the last command forever).
    info!("exiting FX (power-cycle) and restoring dim warm white (2700K @ 12%)");
    ble::write_command(p, write, &drv.power(mac, false)).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    ble::write_command(p, write, &drv.power(mac, true)).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    ble::write_command(p, write, &drv.cct(mac, 12, 27)).await?;
    Ok(())
}

/// Per-segment PIXEL probe (`--pixel`, `0xB0` MAC-embedded). Paints the tube with
/// multi-colour palettes so distinct bands appear along its length — the "set
/// different areas to different values" capability. TL-series pixel fixtures
/// only (verified on TL120C); other lights ignore it. Each palette is sent as
/// its param sub-frame then its colour sub-frame(s), spaced ~80 ms as the app
/// does, and long palettes are chunked to ≤20-byte GATT writes by write_command.
async fn probe_pixel(
    p: &Peripheral,
    write: &Characteristic,
    drv: TestDriver,
    mac: [u8; 6],
) -> Result<()> {
    info!("PIXEL probe — per-segment colour + effects (0xB0); watch the tube");
    // The 5 pixel effects that work over direct BLE on the TL120C. For the
    // moving/fire effects, segment 0 is the background and the rest are the
    // effect's colours. A CCT frame is sent between demos as a visible
    // separator + known baseline, NOT because a latch needs clearing (that
    // claim was disproven on hardware 2026-08-24 — see the loop below).
    let demos: [(&str, u8, Vec<Block>); 4] = [
        (
            "ColorReplacement: red|green|blue|yellow bands",
            1,
            vec![
                Block::Hsi { hue: 0, sat: 100 },
                Block::Hsi { hue: 120, sat: 100 },
                Block::Hsi { hue: 240, sat: 100 },
                Block::Hsi { hue: 55, sat: 100 },
            ],
        ),
        (
            "TwoColorMoving: red+blue over dark bg",
            4,
            vec![Block::Off, Block::Hsi { hue: 0, sat: 100 }, Block::Hsi { hue: 240, sat: 100 }],
        ),
        (
            "ThreeColorMoving: red/green/blue over dark bg",
            5,
            vec![
                Block::Off,
                Block::Hsi { hue: 0, sat: 100 },
                Block::Hsi { hue: 120, sat: 100 },
                Block::Hsi { hue: 240, sat: 100 },
            ],
        ),
        ("Fire: orange flicker over dark bg", 7, vec![Block::Off, Block::Hsi { hue: 25, sat: 100 }]),
    ];
    for (label, effect, blocks) in &demos {
        // A CCT-white beat between demos. This is presentation, not
        // protocol: it visually separates one effect from the next and
        // leaves the tube white if the next effect is ignored, so the
        // operator can tell "ignored" from "worked". A running pixel effect
        // does NOT need clearing — hardware-disproven on a TL120C
        // (2026-08-24): pixel→pixel palette AND effect+speed changes both
        // took effect with zero CCT frames on the wire.
        ble::write_command(p, write, &drv.cct(mac, 50, 56)).await?;
        tokio::time::sleep(Duration::from_millis(700)).await;
        info!("PIXEL {label}");
        for frame in pixel::paint(mac, blocks, 100, *effect, 40, 1) {
            ble::write_command_chunked(p, write, &frame).await?;
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        tokio::time::sleep(Duration::from_millis(2800)).await;
    }

    // Best-effort: leave the light on a comfortable dim warm white. After a long
    // rapid probe the write-without-response link can jam (frames silently
    // dropped), so this may not always take — if the light is left on an effect,
    // run `neewer-bridge test <MAC> --set warmdim` (a fresh connection is
    // reliable). The production bridge (`run`) doesn't have this issue: its
    // per-light actor paces writes and reconnects on a stale link.
    //
    // `classic::cct4` directly rather than `drv.cct`, deliberately: this must be
    // byte-for-byte the frame `--set warmdim` sends, because that is the
    // documented recovery when this best-effort restore doesn't take. Every
    // pixel-capable fixture is classic-family anyway.
    info!("restoring dim warm white (2700K @ 12%); if it sticks on an effect, run `--set warmdim`");
    for _ in 0..3 {
        ble::write_command(p, write, &classic::cct4(12, 27, 0)).await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    Ok(())
}

/// A few hand-tuned FX presets so `--set fx:<id>` renders a recognisable effect.
/// Ids without a preset fall back to generic params.
///
/// Returns the effect parameters `(cct, gm, hue, sat, speed, extra, val2)` — the
/// tail of `fx_data`, shared verbatim by both frame forms below (the MAC wrapper
/// is the only difference between them, so the table must not be duplicated).
fn fx_preset_params(id: u8) -> (u8, i8, u16, u8, u8, u8, u16) {
    match id {
        1 => (56, 0, 0, 0, 5, 0, 0),      // Lightning
        9 => (0, 0, 240, 100, 6, 0, 0),   // HUE-pulse (blue)
        10 => (0, 0, 0, 0, 7, 2, 0),      // Cop-Car
        11 => (32, 0, 0, 0, 4, 0, 0),     // Candlelight
        12 => (0, 0, 0, 100, 5, 0, 0),    // HUE-loop
        _ => (56, 0, 0, 100, 5, 0, 0),    // generic
    }
}

/// A preset as the MAC-embedded `0x91` frame (`cmd_type == 2` fixtures).
fn fx_preset(mac: [u8; 6], id: u8, bri: u8) -> Vec<u8> {
    let (cct, gm, hue, sat, speed, extra, val2) = fx_preset_params(id);
    infinity::fx(mac, id, bri, cct, gm, hue, sat, speed, extra, val2)
}

/// The same presets as the DIRECT `0x8B` frame (no MAC wrapper) — what the app's
/// `setRGBLightValue(EFFECT_MODE_OLD, …)` path sends.
fn fx_preset_direct(id: u8, bri: u8) -> Vec<u8> {
    let (cct, gm, hue, sat, speed, extra, val2) = fx_preset_params(id);
    infinity::fx_direct(id, bri, cct, gm, hue, sat, speed, extra, val2)
}

/// Build the exact frames for one pixel effect id (1..=10, the `PixelEffectType`
/// wire type) using the app's own default parameters from `createPixelEffectData`
/// plus the Pixel-N-Model defaults (decompiled), with `running=PLAY`. This is the
/// exhaustive hardware probe for `test --set pixfx:<id>` — effects 8/9/10 remap
/// their wire id to 10/11/12 (the app does this). Returns (frames, effect name).
fn build_pixel_effect_test(mac: [u8; 6], id: u8) -> (Vec<Vec<u8>>, String) {
    let hsi = |h: u16, s: u8| Block::Hsi { hue: h, sat: s }.bytes();
    let cct = |c: u8, g: i8| Block::Cct { cct: c, gm: g }.bytes();
    let off = Block::Off.bytes();
    let run = pixel::RUN_PLAY; // 1
    // Each arm: (params effectData, palette effectData, effect name). subIndex 0 =
    // params, subIndex 1 = colours. Wire id is the first byte of each sub-frame.
    let pal = |wire: u8, colours: &[[u8; 3]]| {
        let mut c = vec![wire, 1u8];
        for b in colours {
            c.extend_from_slice(b);
        }
        c
    };
    let (params, palette, name): (Vec<u8>, Vec<u8>, &str) = match id {
        1 => (
            vec![1, 0, 50, 2, 40, 1, run], // bri,colorNum,speed,dir,running
            pal(1, &[hsi(0, 100), hsi(240, 100)]),
            "ColorReplacement",
        ),
        2 => (
            vec![2, 0, 50, 40, 1, 0, run], // bri,speed,dir,transition,running
            pal(2, &[hsi(0, 100), hsi(240, 100)]),
            "ColorAlternate",
        ),
        3 => (
            vec![3, 0, 50, 50, 0, 40, 1, 0, run], // colorBri,bgBri,way,speed,dir,movement,running
            pal(3, &[cct(55, 0), hsi(0, 100)]),    // bg + 1 moving colour
            "SingleColorMoving",
        ),
        4 => (
            vec![4, 0, 50, 50, 0, 40, 1, 0, run],
            pal(4, &[cct(55, 0), hsi(0, 100), hsi(60, 100)]), // bg + 2
            "TwoColorMoving",
        ),
        5 => (
            vec![5, 0, 50, 50, 0, 40, 1, 0, run],
            pal(5, &[cct(55, 0), hsi(0, 100), hsi(60, 100), hsi(120, 100)]), // bg + 3
            "ThreeColorMoving",
        ),
        6 => (
            vec![6, 0, 50, 40, 1, run], // bri,speed,dir,running
            pal(6, &[hsi(0, 100), hsi(240, 100)]),
            "Colorful",
        ),
        7 => (
            vec![7, 0, 50, 100, 50, 20, 0, run], // briLo,briHi,bgBri,speed,orientation,running
            pal(7, &[off, hsi(30, 100)]),         // bg + fire colour
            "Fire",
        ),
        8 => (
            vec![10, 0, 50, 2, 40, 1, 0, run], // wire 10; bri,colorNum,speed,dir,sectionType,running
            pal(10, &[hsi(0, 100), hsi(240, 100)]),
            "ColorGradient",
        ),
        9 => (
            vec![11, 0, 50, 3, 40, 1, 0, run], // wire 11; bri,colorNum,speed,dir,satType,running
            pal(11, &[hsi(0, 100), hsi(60, 100), hsi(120, 100)]),
            "Trail",
        ),
        _ => (
            vec![12, 0, 50, 3, 40, 1, run], // wire 12; bri,colorNum,speed,dir,running
            pal(12, &[hsi(0, 100), hsi(60, 100), hsi(120, 100)]),
            "ColorShift",
        ),
    };
    (
        vec![pixel::raw_frame(mac, &params), pixel::raw_frame(mac, &palette)],
        format!("effect {id} ({name})"),
    )
}

/// Read device status: send the MAC-addressed version / battery / temperature /
/// state queries and let the notify logger print the decoded replies. Non-mutating
/// (no blink, no output change), so it's safe to run against a light in use. The
/// replies arrive asynchronously on the notify characteristic — hence the settle
/// wait at the end. The TL120C firmware handles these MAC reads (the direct `0x80`/
/// `0x85` version/state queries are dropped by the firmware).
async fn test_status(p: &Peripheral, write: &Characteristic, mac: [u8; 6]) -> Result<()> {
    info!("reading status (version / battery / temperature / state); decoded replies below");
    for (label, frame) in [
        ("version (0x9E)", queries::version(mac)),
        ("battery (0x95)", queries::battery(mac)),
        ("temperature (0xB3)", queries::temperature(mac)),
        ("state (0x8E)", queries::state(mac)),
    ] {
        info!(query = label, "→ query");
        ble::write_command(p, write, &frame).await?;
        // Space the queries so the light's reply queue isn't outrun.
        tokio::time::sleep(Duration::from_millis(350)).await;
    }
    // Let any late replies land before we disconnect (decoded by spawn_notify_logger).
    info!("waiting for replies…");
    tokio::time::sleep(Duration::from_millis(2000)).await;
    info!("status read complete");
    Ok(())
}

/// What one `--set SPEC` resolves to: the frame(s) to send, a human description,
/// and whether to paint CCT-white first as a known baseline (`reset`).
struct SetPlan {
    frames: Vec<Vec<u8>>,
    desc: String,
    reset: bool,
}

/// Parse a `test --set` spec into the frames it means. Pure (no BLE), so every
/// spec form and its argument validation is unit-testable — the encoders it
/// calls are already byte-pinned by their own tests.
fn build_set_plan(mac: [u8; 6], spec: &str) -> Result<SetPlan> {
    let parts: Vec<&str> = spec.split(':').collect();
    let get = |i: usize| parts.get(i).copied();
    let num = |i: usize, what: &str| -> Result<u32> {
        get(i)
            .with_context(|| format!("--set {spec}: missing {what}"))?
            .parse::<u32>()
            .with_context(|| format!("--set {spec}: {what} must be a number"))
    };
    // Range-checked variants. Casting a parsed u32 straight to u8/u16 used to
    // wrap silently — `bri:300` became 44 and the probe reported the value it
    // never sent, which is the worst possible behaviour in a diagnostic tool.
    // (`--set raw:<hex>` remains the escape hatch for deliberately out-of-spec
    // frames.)
    let bounded = |i: usize, what: &str, max: u32| -> Result<u32> {
        let v = num(i, what)?;
        if v > max {
            bail!("--set {spec}: {what} must be 0..={max}, got {v}");
        }
        Ok(v)
    };
    let u8n = |i: usize, what: &str| -> Result<u8> { Ok(bounded(i, what, 255)? as u8) };
    let pct = |i: usize, what: &str| -> Result<u8> { Ok(bounded(i, what, 100)? as u8) };
    // Effect ids need a LOWER bound too. `bounded` alone let `0` through, and
    // the builders' catch-all arms then quietly emitted a DIFFERENT effect from
    // the one the description reported: `fx:0` sends effect 1 (Lightning),
    // `pixfx:0` sends ColorShift. Reporting a value you did not send is exactly
    // what the range checks exist to prevent.
    let id_in = |i: usize, what: &str, lo: u32, hi: u32| -> Result<u8> {
        let v = num(i, what)?;
        if !(lo..=hi).contains(&v) {
            bail!("--set {spec}: {what} must be {lo}..={hi}, got {v}");
        }
        Ok(v as u8)
    };

    // Build the frame(s) + a human description. `reset` = paint CCT-white first,
    // so that a frame this fixture IGNORES leaves the light white rather than
    // holding its previous look — that is what makes "worked" and "ignored"
    // distinguishable in a one-at-a-time probe.
    let (frames, desc, reset): (Vec<Vec<u8>>, String, bool) = match parts[0] {
        // cct4: this is the documented "get the light back to something safe"
        // escape hatch, so it must work even from a running pixel/FX effect —
        // which the 2-byte form does NOT (HW-proven 2026-08-25).
        "warmdim" => (vec![classic::cct4(12, 27, 0)], "dim warm white 2700K @ 12%".into(), false),
        "cct" => {
            let (k, bri) = (bounded(1, "kelvin", 25_500)?, pct(2, "bri")?);
            (vec![classic::cct2(bri, (k / 100) as u8)], format!("CCT {k}K @ {bri}%"), false)
        }
        "cctgm" => {
            // GM CCT probe. Optional 4th part = frame form: 4 (default; the app's
            // cct4), 3 (GL1-family cct3) or 5 (RGB62-family cct_gm5). gm -50..=50.
            let (k, bri) = (bounded(1, "kelvin", 25_500)?, pct(3, "bri")?);
            let gm: i8 = get(2)
                .with_context(|| format!("--set {spec}: missing gm"))?
                .parse()
                .with_context(|| format!("--set {spec}: gm must be -50..=50"))?;
            // The encoders CLAMP gm to ±50, so an out-of-range value would go
            // out as ±50 while the description below echoed what was typed.
            if !(-50..=50).contains(&gm) {
                bail!("--set {spec}: gm must be -50..=50, got {gm}");
            }
            let cct = (k / 100) as u8;
            let (frame, form) = match get(4) {
                Some("3") => (classic::cct3(bri, cct, gm), 3),
                Some("5") => (classic::cct_gm5(bri, cct, gm), 5),
                _ => (classic::cct4(bri, cct, gm), 4),
            };
            (vec![frame], format!("CCT{form} {k}K gm{gm:+} @ {bri}%"), false)
        }
        "hsi" => {
            let (hue, sat, bri) = (bounded(1, "hue", 360)? as u16, pct(2, "sat")?, pct(3, "bri")?);
            (vec![classic::hsi(hue, sat, bri)], format!("HSI hue={hue} sat={sat} @ {bri}%"), true)
        }
        "xy" => {
            let (x, y, bri) = (bounded(1, "x", 8000)? as u16, bounded(2, "y", 8000)? as u16, pct(3, "bri")?);
            (vec![classic::xy_mac(mac, bri, x, y)], format!("XY by-MAC 0xB7 x={x} y={y} @ {bri}%"), true)
        }
        "xydirect" => {
            // Direct 0xB9 — ignored on commandType==2 (Infinity) fixtures like the
            // TL120C, but the form the app sends to everything else. Probe both.
            let (x, y, bri) = (bounded(1, "x", 8000)? as u16, bounded(2, "y", 8000)? as u16, pct(3, "bri")?);
            (vec![classic::xy(bri, x, y)], format!("XY direct 0xB9 x={x} y={y} @ {bri}%"), true)
        }
        "fxdirect" => {
            // Direct 0x8B — the 18-effect payload without the MAC wrapper
            // (`setRGBLightValue(EFFECT_MODE_OLD,…)`, cn.java:3458). For fixtures
            // that ignore the MAC 0x91 form.
            let (id, bri) = (id_in(1, "id", 1, 18)?, pct(2, "bri")?);
            (vec![fx_preset_direct(id, bri)], format!("FX direct 0x8B #{id} @ {bri}%"), true)
        }
        "scene" => {
            // Old 9-scene 0x88 — dropped by TL120C firmware; non-Infinity fixtures
            // may honour it. reset=true so an ignored frame leaves plain white.
            let (id, bri) = (id_in(1, "scene id", 1, 9)?, pct(2, "bri")?);
            (vec![classic::scene(bri, id)], format!("SCENE 0x88 #{id} @ {bri}%"), true)
        }
        "fx" => {
            let (id, bri) = (id_in(1, "id", 1, 18)?, pct(2, "bri")?);
            (vec![fx_preset(mac, id, bri)], format!("FX #{id} @ {bri}%"), true)
        }
        "pixel" => {
            let blocks: Vec<Block> = get(1)
                .context("--set pixel:<hue,hue,...>:<eff>:<speed>: missing hues")?
                .split(',')
                .map(|h| -> Result<Block> {
                    let hue: u16 = h.trim().parse().context("bad hue")?;
                    // `Block::bytes` wraps hue modulo 360, so an out-of-range
                    // value would render as a colour other than the one asked
                    // for (hue 400 → 40, orange instead of nothing).
                    if hue > 360 {
                        bail!("--set {spec}: hue must be 0..=360, got {hue}");
                    }
                    Ok(Block::Hsi { hue, sat: 100 })
                })
                .collect::<Result<_>>()?;
            let (eff, speed) = (id_in(2, "effect", 1, 10)?, pct(3, "speed")?);
            let n = blocks.len();
            // Only five pixel effects render over direct BLE; `paint` falls the
            // rest back to ColorReplacement. Report what actually goes on the
            // wire rather than echoing the id that was asked for.
            let rendered = pixel::rendered_effect(eff);
            let desc = if rendered == eff {
                format!("PIXEL {n} seg eff={eff} speed={speed}")
            } else {
                format!(
                    "PIXEL {n} seg eff={eff} → not supported over direct BLE, \
                     sending {rendered} (ColorReplacement) speed={speed}"
                )
            };
            // reset=true is a DIAGNOSTIC baseline, not a latch clear: painting
            // CCT-white first means an ignored pixel frame leaves the tube white
            // instead of holding whatever it showed before, so "worked" and
            // "ignored" are distinguishable. (The old justification — "a running
            // pixel effect ignores a new palette/effect until a CCT frame clears
            // the latch" — was DISPROVEN on a TL120C 2026-08-24: red→green and
            // ColorReplacement→TwoColorMoving both took effect with zero CCT
            // frames on the wire. The production `pixel` profile sends no CCT
            // clear and is correct.)
            (pixel::paint(mac, &blocks, 100, eff, speed, 1), desc, true)
        }
        "pixfx" => {
            // Exhaustive per-effect probe: build effect `id` (1..=10) with the app's
            // own default params from the decompile.
            let id = id_in(1, "effect id", 1, 10)?;
            let (frames, name) = build_pixel_effect_test(mac, id);
            (frames, format!("PIXEL {name}"), true)
        }
        "rgbcw" | "rgbcwmac" => {
            // RGBCW probe. `rgbcwmac` (by-MAC 0xA9) is the WORKING production form
            // (hardware-confirmed 2026-07-01); `rgbcw` (direct 0xA8) is IGNORED on the
            // TL120C and kept only to demonstrate that. reset=true paints CCT-white
            // first, so an ignored frame leaves the light white, a working one jumps
            // to the R/G/B/CW/WW mix.
            // Spec: rgbcw:<r>:<g>:<b>[:<cw>:<ww>:<bri>]  (values 0..=255; bri 0..=100).
            let optu8 = |i: usize, what: &str, dflt: u8| -> Result<u8> {
                get(i)
                    .map(|s| s.parse::<u8>().with_context(|| format!("--set {spec}: {what} must be 0..=255")))
                    .transpose()
                    .map(|o| o.unwrap_or(dflt))
            };
            let (r, g, b) = (u8n(1, "r")?, u8n(2, "g")?, u8n(3, "b")?);
            let (cw, ww, bri) = (optu8(4, "cw", 0)?, optu8(5, "ww", 0)?, optu8(6, "bri", 100)?);
            // bri is a percentage like every other probe's: without this check a
            // `bri:200` would be sent verbatim and the probe would report a value
            // outside the documented 0..=100 range ("never send what you didn't
            // report" - the same rule the `bounded` helpers exist for).
            if bri > 100 {
                bail!("--set {spec}: bri must be 0..=100, got {bri}");
            }
            let (frame, form) = if parts[0] == "rgbcwmac" {
                (classic::rgbcw_mac(mac, bri, r, g, b, cw, ww, 0), "by-MAC 0xA9 (production form — should render)")
            } else {
                (classic::rgbcw(bri, r, g, b, cw, ww, 0), "direct 0xA8 (ignored on TL120C — should stay white)")
            };
            (
                vec![frame],
                format!("RGBCW {form}: R={r} G={g} B={b} CW={cw} WW={ww} @ {bri}%"),
                true,
            )
        }
        "raw" => {
            // Send an arbitrary frame VERBATIM (the whole frame incl. its checksum is
            // supplied) — protocol spelunking, e.g. the OTA-type probe `raw:78D00048`
            // (78 D0 00 48). No CCT-clear first: exactly these bytes and nothing else.
            // Any notify reply is decoded/logged by the notify logger (raw hex at -v).
            let hex: String =
                parts[1..].join("").chars().filter(|c| c.is_ascii_hexdigit()).collect();
            if hex.is_empty() || !hex.len().is_multiple_of(2) {
                bail!("--set raw: give an even-length hex frame, e.g. raw:78D00048");
            }
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex validated"))
                .collect();
            let shown = bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
            (vec![bytes], format!("raw frame [{shown}]"), false)
        }
        other => bail!("--set: unknown spec kind '{other}' (cct|cctgm|hsi|xy|xydirect|scene|fx|fxdirect|pixel|pixfx|warmdim|rgbcw|rgbcwmac|raw)"),
    };
    Ok(SetPlan { frames, desc, reset })
}

/// Send one frame (or pixel palette) described by `spec` and hold it — the engine
/// behind `test --set`, for guided one-value-at-a-time hardware testing. The light
/// keeps the state after disconnect.
///
/// Most specs paint a CCT-white frame first (`reset`). That is a DIAGNOSTIC
/// BASELINE — an ignored frame then leaves the light white instead of holding
/// its previous look, so you can tell "the fixture ignored this" from "nothing
/// was sent". It is NOT a latch clear: pixel→pixel changes were shown to take
/// effect with no CCT frame at all (TL120C, 2026-08-24).
/// Takes an already-built [`SetPlan`]: parsing happens in [`test`] before the
/// adapter is opened, so this only ever runs against a spec already known good.
async fn test_set(p: &Peripheral, write: &Characteristic, plan: SetPlan) -> Result<()> {
    let SetPlan { frames, desc, reset } = plan;

    if reset {
        info!("painting CCT-white first as a known baseline (an ignored frame will stay white)");
        ble::write_command(p, write, &classic::cct4(50, 56, 0)).await?;
        tokio::time::sleep(Duration::from_millis(900)).await;
    }

    info!("SET {desc}");
    // Send the frame(s) a few times over ~4s while connected so it's easy to watch
    // (pixel palettes are multi-frame, spaced ~80ms and MTU-chunked by the BLE layer).
    for round in 0..4 {
        for frame in &frames {
            ble::write_command_chunked(p, write, frame).await?;
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        if round < 3 {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }
    info!("held; light retains this state after disconnect");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MAC: [u8; 6] = [0xD6, 0x50, 0xF2, 0xF6, 0xBB, 0x1B];

    #[test]
    fn unknown_driver_names_are_rejected_not_silently_treated_as_classic() {
        // The whole point of the enum: `--driver classik` used to fall through a
        // `_ =>` arm and send classic frames while reporting nothing wrong, and
        // the "you used auto" hint only fired for the exact literal "auto".
        for good in KNOWN_DRIVERS {
            TestDriver::parse(good).unwrap_or_else(|e| panic!("{good} must parse: {e:#}"));
        }
        for bad in ["classik", "Classic", "", "auto ", "infinity2", "0x78"] {
            let err = TestDriver::parse(bad).expect_err("must be rejected");
            let msg = format!("{err:#}");
            assert!(msg.contains("unknown --driver"), "{bad}: {msg}");
            assert!(msg.contains("classic"), "the error must list the valid names: {msg}");
        }
    }

    #[test]
    fn each_driver_emits_its_own_familys_frames() {
        // These three used to be closures inside `test`, one per frame kind, each
        // re-matching the driver string. Pinning them keeps the dispatch honest
        // and documents that `auto` deliberately means classic here.
        for drv in [TestDriver::Auto, TestDriver::Classic] {
            assert_eq!(drv.power(TEST_MAC, true), classic::power(true));
            assert_eq!(drv.power(TEST_MAC, false), classic::power(false));
            // cct4, NOT cct2 — the 2-byte form is ignored by a fixture running a
            // pixel effect, which is exactly when a baseline is needed.
            assert_eq!(drv.cct(TEST_MAC, 50, 56), classic::cct4(50, 56, 0));
            assert_eq!(drv.hsi(TEST_MAC, 120), classic::hsi(120, 100, 100));
        }
        let inf = TestDriver::Infinity;
        assert_eq!(inf.power(TEST_MAC, true), infinity::power(TEST_MAC, true));
        assert_eq!(inf.cct(TEST_MAC, 50, 56), infinity::cct(TEST_MAC, 50, 56, 0));
        assert_eq!(inf.hsi(TEST_MAC, 120), infinity::hsi(TEST_MAC, 120, 100, 100));
        let home_drv = TestDriver::Home;
        assert_eq!(home_drv.power(TEST_MAC, true), home::power(true));
        // Home brightness is the native 0..=1000 range, so the 0..=100 percentage
        // is scaled by 10 — dropping that would dim the light to a tenth.
        assert_eq!(home_drv.cct(TEST_MAC, 50, 56), home::cct(500, 56));
        assert_eq!(home_drv.hsi(TEST_MAC, 120), home::hsi(1000, 120, 100));
    }

    #[test]
    fn set_specs_build_the_expected_frames() {
        let plan = build_set_plan(TEST_MAC, "cct:5600:40").unwrap();
        assert_eq!(plan.frames, vec![classic::cct2(40, 56)]);
        assert!(!plan.reset, "a CCT spec is already its own baseline");

        let plan = build_set_plan(TEST_MAC, "hsi:120:100:80").unwrap();
        assert_eq!(plan.frames, vec![classic::hsi(120, 100, 80)]);
        assert!(plan.reset, "non-CCT specs paint a CCT-white baseline first");

        // GM CCT: the 4-byte app form by default, 3/5-byte on request.
        assert_eq!(
            build_set_plan(TEST_MAC, "cctgm:5600:-50:40").unwrap().frames,
            vec![classic::cct4(40, 56, -50)]
        );
        assert_eq!(
            build_set_plan(TEST_MAC, "cctgm:5600:10:40:5").unwrap().frames,
            vec![classic::cct_gm5(40, 56, 10)]
        );

        // MAC vs direct forms stay distinct (the commandType split).
        assert_eq!(
            build_set_plan(TEST_MAC, "xy:3127:3290:80").unwrap().frames,
            vec![classic::xy_mac(TEST_MAC, 80, 3127, 3290)]
        );
        assert_eq!(
            build_set_plan(TEST_MAC, "xydirect:3127:3290:80").unwrap().frames,
            vec![classic::xy(80, 3127, 3290)]
        );
        assert_eq!(
            build_set_plan(TEST_MAC, "rgbcwmac:255:0:0").unwrap().frames,
            vec![classic::rgbcw_mac(TEST_MAC, 100, 255, 0, 0, 0, 0, 0)]
        );

        // `warmdim` is the documented escape hatch — "if the light is stuck on an
        // effect, run --set warmdim" — so it MUST use the 4-byte form. The 2-byte
        // form is ignored by a fixture running a pixel effect (HW-proven on a
        // TL120C 2026-08-25: cct2 left it scrolling, cct4 exited it), which made
        // the escape hatch silently useless in exactly the case it exists for.
        assert_eq!(
            build_set_plan(TEST_MAC, "warmdim").unwrap().frames,
            vec![classic::cct4(12, 27, 0)],
            "warmdim must send the 4-byte CCT form or it cannot exit pixel/FX mode"
        );
        // ...but `cct:` still probes the 2-byte form on purpose; `cctgm:` is the
        // 4-byte one. Those two specs exist to tell the forms apart.
        assert_eq!(
            build_set_plan(TEST_MAC, "cct:5600:40").unwrap().frames,
            vec![classic::cct2(40, 56)]
        );

        // `raw` sends the given bytes verbatim, with no baseline frame.
        let plan = build_set_plan(TEST_MAC, "raw:78D00048").unwrap();
        assert_eq!(plan.frames, vec![vec![0x78, 0xD0, 0x00, 0x48]]);
        assert!(!plan.reset);

        // Pixel emits its params frame plus palette frame(s).
        assert_eq!(build_set_plan(TEST_MAC, "pixel:0,240:1:40").unwrap().frames.len(), 2);
    }

    #[test]
    fn set_specs_reject_out_of_range_values() {
        // These used to wrap silently: bri 300 became 44 and the tool logged a
        // value it had not sent — the worst outcome for a diagnostic probe.
        for spec in [
            "cct:5600:300",       // brightness > 100
            "hsi:400:100:80",     // hue > 360
            "hsi:120:200:80",     // saturation > 100
            "xy:9000:3290:80",    // x past the 0.8000 coordinate max
            "fx:99:80",           // no such effect
            "scene:20:80",        // 9-scene family only has 1..=9
            "rgbcw:300:0:0",      // channel > 255
            "rgbcw:10:10:10:0:0:200",    // bri > 100 (percentage, not a channel)
            "rgbcwmac:10:10:10:0:0:101", // same rule on the by-MAC form
        ] {
            assert!(build_set_plan(TEST_MAC, spec).is_err(), "{spec} should be rejected");
        }

        // Malformed / unknown specs stay errors, and none of them panic.
        for spec in ["", "nope:1", "cct", "cct:5600", "cct:abc:40", "raw:", "raw:78D0004"] {
            assert!(build_set_plan(TEST_MAC, spec).is_err(), "{spec:?} should be rejected");
        }
    }

    #[test]
    fn set_specs_never_send_an_effect_other_than_the_one_reported() {
        // Only the UPPER bound used to be checked, so `0` slipped through into
        // the builders' catch-all arms: `fx:0` went out as effect 1 (Lightning)
        // and `pixfx:0` as ColorShift, both while the log line said "#0". A
        // diagnostic tool reporting a value it did not send is worse than no
        // tool, which is why every id now carries a lower bound.
        for spec in [
            "fx:0:80",           // effect ids start at 1
            "fxdirect:0:80",
            "scene:0:80",
            "pixfx:0",
            "pixel:0,240:0:40",  // pixel effect id
            "cctgm:5600:99:40",  // gm is CLAMPED to ±50 by the encoder
            "cctgm:5600:-99:40",
            "pixel:400,0:1:40",  // hue is WRAPPED modulo 360 by Block::bytes
        ] {
            assert!(build_set_plan(TEST_MAC, spec).is_err(), "{spec} should be rejected");
        }

        // The bounds stay inclusive at both ends of every valid range.
        for spec in [
            "fx:1:80", "fx:18:80", "fxdirect:1:80", "fxdirect:18:80",
            "scene:1:80", "scene:9:80", "pixfx:1", "pixfx:10",
            "pixel:0,240:1:40", "pixel:0,240:10:40", "pixel:360,0:1:40",
            "cctgm:5600:50:40", "cctgm:5600:-50:40",
        ] {
            assert!(build_set_plan(TEST_MAC, spec).is_ok(), "{spec} should be accepted");
        }

        // A pixel id that IS in range but doesn't render over direct BLE must
        // say what actually goes on the wire, not echo what was asked for.
        let plan = build_set_plan(TEST_MAC, "pixel:0,240:2:40").unwrap();
        assert!(plan.desc.contains("eff=2"), "must still name what was asked: {}", plan.desc);
        assert!(
            plan.desc.contains("ColorReplacement"),
            "must disclose the fallback: {}",
            plan.desc
        );
        assert_eq!(plan.frames[0][9], pixel::rendered_effect(2), "wire id must match the desc");
        // A supported id reports itself with no caveat.
        let plan = build_set_plan(TEST_MAC, "pixel:0,240:4:40").unwrap();
        assert!(plan.desc.contains("eff=4") && !plan.desc.contains("ColorReplacement"));
    }

    #[test]
    fn fx_preset_table_matches_the_hand_tuned_values() {
        // `fx_preset`/`fx_preset_direct` used to be two hand-maintained copies of
        // one table. These are the exact literals they held, so the shared
        // `fx_preset_params` that replaced them cannot silently change a probe.
        for (id, cct, gm, hue, sat, speed, extra, val2) in [
            (1u8, 56u8, 0i8, 0u16, 0u8, 5u8, 0u8, 0u16), // Lightning
            (9, 0, 0, 240, 100, 6, 0, 0),                // HUE-pulse (blue)
            (10, 0, 0, 0, 0, 7, 2, 0),                   // Cop-Car
            (11, 32, 0, 0, 0, 4, 0, 0),                  // Candlelight
            (12, 0, 0, 0, 100, 5, 0, 0),                 // HUE-loop
            (3, 56, 0, 0, 100, 5, 0, 0),                 // generic fallback
        ] {
            assert_eq!(
                fx_preset(TEST_MAC, id, 80),
                infinity::fx(TEST_MAC, id, 80, cct, gm, hue, sat, speed, extra, val2),
                "MAC-form FX preset {id} drifted"
            );
            assert_eq!(
                fx_preset_direct(id, 80),
                infinity::fx_direct(id, 80, cct, gm, hue, sat, speed, extra, val2),
                "direct-form FX preset {id} drifted"
            );
        }
    }

    #[test]
    fn fx_presets_carry_the_same_payload_in_both_frame_forms() {
        // `78 91 <len> <MAC6> 8B <payload> ck` vs `78 8B <len> <payload> ck`:
        // the wrapper differs, the effect parameters must not.
        for id in 1..=18u8 {
            let m = fx_preset(TEST_MAC, id, 80);
            let d = fx_preset_direct(id, 80);
            assert_eq!(
                &m[10..m.len() - 1],
                &d[3..d.len() - 1],
                "FX preset {id} differs between the MAC and direct frame forms"
            );
        }
    }
}
