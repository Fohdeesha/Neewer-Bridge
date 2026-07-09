# Neewer-Bridge

An efficient, reliable **ArtNet → Neewer Bluetooth** light bridge. It receives
ArtNet (DMX-over-UDP) and controls Neewer LED lights over their proprietary BLE
protocol. CLI, config-file driven, runs on **Windows and Linux**.

- Drives **multiple lights at once**, each bound to a fixed set of DMX channels.
- **Deterministic binding by MAC** — the DMX-channel → physical-light mapping is
  identical on every boot, independent of power-on or discovery order.
- Supports classic (`0x78`), Infinity (`0x78`+MAC), and Neewer Home (`0x7A`,
  `NH-*`) lights.
- Full built-in control: **CCT (+GM), HSI, CIE XY, and the 18-effect FX engine**
  — selectable live from a single DMX mode channel.
- **141-model capability catalog** so `add` auto-fills a light's driver, profile,
  and CCT range from its Bluetooth name.

> Status: hardware-validated end-to-end on a Neewer TL120C, TL21C and TL60 RGB —
> CCT, HSI, XY, RGBCW, GM, FX, and per-segment pixel all confirmed over a direct BLE
> connection (per-model: each fixture supports a different subset — see the
> hardware notes below). See `NOTES.md` for the full design, protocol
> reverse-engineering, and reliability notes.

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands) — every subcommand and flag
- [Configuration](#configuration)
- [DMX profiles](#dmx-profiles-channel-layouts)
- [FX effects](#fx-effects)
- [Drivers](#drivers)
- [Reliability](#reliability)
- [Troubleshooting](#troubleshooting)

## Install

Requires the [Rust toolchain](https://rustup.rs) (stable). On Windows the MSVC
toolchain is used (Visual Studio Build Tools / Community). A Bluetooth LE adapter
is needed to talk to lights (a USB BLE dongle works).

```sh
git clone <this repo>
cd Neewer-Bridge
cargo build --release
# binary at target/release/neewer-bridge  (neewer-bridge.exe on Windows)
```

Run it directly from `target/release/`, or via `cargo run --release -- <args>`.

## Quick start

The bridge looks for `config.toml` **next to the binary first**, then in the
working directory — it ships ready to edit, so there's nothing to copy or
rename, and you can run the binary from anywhere once the config sits beside it.
(Use `--config PATH` to point elsewhere.)

```sh
# 1. Confirm your Bluetooth adapter and see what's nearby
neewer-bridge scan

# 2. Pair a light interactively — it blinks the fixture so you know which is
#    which, identifies the model, and appends a [[lights]] entry to config.toml.
neewer-bridge add

# 3. Review the DMX channel mapping it produced
neewer-bridge lights

# 4. Run the bridge (`run` is the default — a bare `neewer-bridge` does the same)
neewer-bridge
```

Point your lighting console / software (QLC+, etc.) at this host's IP on the
configured ArtNet universe and DMX address.

## Commands

```
neewer-bridge [--config PATH] [-v|-vv] [COMMAND]
```

No command ⇒ `run` (launching the bare binary starts the bridge).

| Command | What it does |
|---|---|
| `adapters` | List BLE adapters (index + name) for the `[ble] adapter` setting. |
| `models` | List the built-in light-model catalog (what `add` matches against). |
| `lights` | Show configured lights and their absolute universe + DMX-channel mapping. |
| `scan` | Discover NEW lights (not already in the config); list name / MAC / RSSI. |
| `add` | Bind a light to the config (interactive, or non-interactive with `--mac`). |
| `inspect <MAC>` | Connect and dump a device's full GATT (identify unknown lights). |
| `test <MAC>` | Connect one light and prove control (blink + CCT, optional colour/mode probes). |
| `artnet-send` | Send ArtDmx to drive the bridge / a node without a console. |
| `monitor` | Listen for ArtNet and print received ArtDmx (no BLE needed). |
| `run` | The full bridge: ArtNet → mapper → per-light BLE actors. **The default** — running `neewer-bridge` with no command does this. |

### Global flags

| Flag | Default | Meaning |
|---|---|---|
| `--config PATH` | see right | Config file path. Default: `config.toml` next to the executable if present, else `config.toml` in the working directory. |
| `-v` | — | Debug logging for this crate (logs BLE writes as hex). |
| `-vv` | — | Trace + btleplug BLE wire logs. |
| `--version` | — | Print version. |
| `--help` | — | Help (works on any subcommand too, e.g. `add --help`). |

Logging destinations and default level come from the `[logging]` config section
(console and/or a size-rotating file — see [Configuration](#configuration)).
Precedence, highest first: `RUST_LOG` (e.g. `RUST_LOG=neewer_bridge=trace`) >
`-v`/`-vv` > `[logging] level`. Activity — connects, reconnects, power on/off,
failsafe — logs at `info`; every BLE command sent to a light logs at `debug`, so
setting `file_level = "debug"` keeps a full on-disk record while the console stays
clean at `info`.

`scan`, `add`, `test`, `inspect`, `monitor`, `adapters`, and `artnet-send` work
without a config file (they fall back to defaults); `lights` and `run` require a
valid config.

### Per-command flags

**`scan`** — discover lights. Lights already bound in the config are hidden (a
note says how many were skipped), so the list only shows what's new to add.
| Flag | Default | Meaning |
|---|---|---|
| `--seconds N` | `6` | Scan duration. |
| `--all` | off | List every BLE device — including already-configured lights (marked) and non-Neewer devices. |
| `--json` | off | Machine-readable JSON array (name / mac / rssi / neewer / configured). |

**`add`** — bind a light. With **no `--mac`** it runs interactively (scan →
blink-to-identify → prompts). With **`--mac`** it runs non-interactively
(scriptable). The model is identified from the light's Bluetooth name, so
driver / profile / CCT range are filled automatically; any flag overrides.
| Flag | Required? | Meaning |
|---|---|---|
| `--mac M` | — | Light MAC. If given, runs non-interactively. |
| `--universe N` | with `--mac` | ArtNet universe / Port-Address (0–32767). |
| `--address N` | with `--mac` | 1-based DMX start channel. |
| `--driver D` | — | `auto`\|`classic`\|`infinity`\|`home` (default: from model). |
| `--profile P` | — | `cct`\|`cct_gm`\|`hsi`\|`rgb`\|`rgbcw`\|`full`\|`advanced`\|`pixel` (default: from model). |
| `--name X` | — | Label (default: model name). |
| `--cct-min N` | — | CCT range min, raw ×100K (default: from model). |
| `--cct-max N` | — | CCT range max, raw ×100K (default: from model). |
| `--blink` | — | Blink the light to identify it (non-interactive mode). |

**`inspect <MAC>`** — dump GATT.
| Flag | Default | Meaning |
|---|---|---|
| `--seconds N` | `10` | How long to wait to find the device. |

**`test <MAC>`** — prove BLE control on one light.
| Flag | Default | Meaning |
|---|---|---|
| `--driver D` | `auto` | `classic`\|`infinity`\|`home`\|`auto`. |
| `--seconds N` | `8` | How long to wait to find the light. |
| `--colors` | off | After CCT, cycle HSI red→green→blue (RGB capability probe). |
| `--modes` | off | Probe the advanced modes: XY and a few FX effects (MAC-addressed frames). |
| `--pixel` | off | Probe per-segment PIXEL control (`0xB0`): cycle the 5 working pixel effects (ColorReplacement / Single/Two/Three-ColorMoving / Fire) so distinct bands/animations appear along the tube. TL-series pixel fixtures only (e.g. TL120C). |
| `--status` | off | Read device status — firmware version, battery, temperature, power/mode — and print the decoded replies. **Non-mutating** (no blink, no colour change), so it's safe to run against a light in use. |
| `--set SPEC` | — | Send ONE frame and hold it (guided one-at-a-time testing; the light keeps the state after disconnect). `SPEC` = `cct:<K>:<bri>` (2-byte form), `cctgm:<K>:<gm>:<bri>[:3\|4\|5]` (GM CCT, gm −50..50, optional frame form — default the app's 4-byte), `hsi:<hue>:<sat>:<bri>`, `xy:<x>:<y>:<bri>` (by-MAC `0xB7`), `xydirect:<x>:<y>:<bri>` (direct `0xB9`), `fx:<id>:<bri>` (MAC `0x91`), `fxdirect:<id>:<bri>` (direct `0x8B` — the TL21C's FX path), `scene:<id>:<bri>` (old 9-scene `0x88`), `pixel:<hue,…>:<eff>:<speed>`, `pixfx:<id>` (raw effect probe 1–10), `rgbcwmac:<r>:<g>:<b>[:<cw>:<ww>:<bri>]` (RGBCW via by-MAC `0xA9` — the production form; `rgbcw:…` = the direct `0xA8`, ignored on the TL120C), or `warmdim` (safe dim-warm end state). |

`test` blinks the light 3× (also a visual identify), sets 5600K @ 50%, then runs
any requested probes. FX and PIXEL latch the light into their mode — `test`
power-cycles to exit and restores white.

**`artnet-send`** — drive the bridge (or any ArtNet node) without a console.
| Flag | Default | Meaning |
|---|---|---|
| `--target IP` | `127.0.0.1` | Destination IP. |
| `--port P` | `6454` | Destination UDP port. |
| `--universe N` | `0` | ArtNet universe / Port-Address. |
| `--address N` | `1` | DMX start channel for the values. |
| `--channels a,b,c` | **required** | Comma-separated channel values (0–255). |
| `--hz H` | — | Stream at this rate; omit for a single packet. |
| `--seconds S` | `2.0` | Stream duration (with `--hz`). |

Example — set a light at universe 0, address 1, advanced profile, to FX Cop-Car
(ch1=80 selects the FX band, ch2=255 full dimmer, ch3=10 → effect #10):
```sh
neewer-bridge artnet-send --universe 0 --address 1 --channels 80,255,10
```

## Configuration

Edit `config.toml` (shipped ready to go — no copy/rename step). Lights are bound
by **MAC address** — the stable identity that makes the DMX→light mapping
deterministic across reboots.

```toml
[artnet]
bind_ip = "0.0.0.0"   # interface to receive ArtNet on (0.0.0.0 = all)
port    = 6454        # standard ArtNet port (configurable)

[ble]
adapter     = "default" # "default", an index ("0"), or a name substring
flush_hz    = 15        # max BLE state updates per light per second (coalescing cap)
probe_secs  = 20        # liveness-probe / status-query interval (stale-session detection)
refresh_secs = 900      # force-reconnect a fixture that never replies (e.g. TL60) every
                        # N s so a wedged-but-connected light self-heals; 0 disables

[failsafe]              # what to do when ArtNet data stops arriving
mode         = "hold"   # hold | blackout | poweroff
timeout_secs = 0        # 0 = never act (hold forever); >0 = act after N seconds

[logging]               # verbosity + destinations (console and/or rotating file)
level         = "info"  # trace | debug | info | warn | error (global default)
console       = true    # log to stderr (stdout stays clean for --json output)
# file        = "neewer-bridge.log"  # omit/empty = no file; rotated by size
# file_level  = "debug" # keep a full debug record on disk while console stays info
max_size_mb   = 10      # rotate the file past this size
max_files     = 5       # rotated files to keep

[[lights]]
mac      = "AA:BB:CC:DD:EE:FF"  # binding identity (required)
name     = "Key light"          # optional label
driver   = "auto"               # auto | classic | infinity | home
profile  = "advanced"           # cct | cct_gm | hsi | rgb | rgbcw | full | advanced | pixel
universe = 0                    # ArtNet 15-bit Port-Address (Net/Sub-Net/Universe)
address  = 1                    # 1-based DMX start channel
power_on_connect = true         # power the light on when it first connects
cct_min  = 32                   # CCT range min, raw ×100K (32 = 3200K)
cct_max  = 56                   # CCT range max, raw ×100K (56 = 5600K; TL120C = 25..100)
cmd_type = 2                    # advanced-mode frame family (the app's per-model
                                # commandType): 2 = MAC-embedded XY/RGBCW/FX frames
                                # (Infinity fixtures like the TL120C); 0/1 = direct
                                # frames (e.g. TL21C). `add` fills it automatically
                                # from the model catalog; default 2.
```

Add as many `[[lights]]` blocks as you have fixtures. The config is validated on
load: MAC format, known driver/profile/failsafe values, universe ≤ 32767, address
1–512, the whole profile fitting within 512 channels, ordered CCT range, and no
duplicate MACs. Run `neewer-bridge lights` to see the resulting channel map.

## DMX profiles (channel layouts)

All channels are 8-bit. The master **Dimmer** sets brightness only — it never
cuts power (lights are powered on and kept on; use the failsafe or remove power
externally to turn them off).

| Profile | Ch | Layout |
|---|---|---|
| `cct`      | 2  | 1 Dimmer · 2 CCT |
| `cct_gm`   | 3  | 1 Dimmer · 2 CCT · 3 GM |
| `hsi`      | 3  | 1 Dimmer · 2 Hue · 3 Saturation |
| `rgb`      | 3  | 1 Red · 2 Green · 3 Blue — converted to HSI internally (below) |
| `rgbcw`    | 5  | 1 Red · 2 Green · 3 Blue · 4 Cool White · 5 Warm White — direct (below) |
| `full`     | 5  | 1 Dimmer · 2 **Mode** · 3 CCT/Hue · 4 GM/Sat · 5 reserved |
| `advanced` | 10 | 1 **Mode-select** · 2 Dimmer · 3–10 mode-specific (below) |
| `pixel`    | 20 | 1 Dimmer · 2 Effect-select · 3 Speed · 4 Direction · 5–20 = 8×(Hue, Sat) — 5 animated effects (below) |

For **`full`**, the Mode channel (ch2) selects sub-mode live: `0–127` = CCT
(ch3=CCT, ch4=GM), `128–255` = HSI (ch3=Hue, ch4=Saturation).

For **`rgb`** (opt-in; any colour-capable model), three channels carry plain
R, G, B and the bridge converts them to HSI internally (hue/sat from the RGB
ratio, brightness = the max component). Because it drives the light's ordinary
HSI mode, it works on **every** colour fixture — including models with no native
RGBCW mode, such as the TL21C. In openHAB each light is a single 3-channel DMX
`color` thing, no white channels and no rules; white renders as desaturated HSI
through the RGB engine (for the dedicated CW/WW LED banks use `rgbcw` instead):

```
// things — one 3ch color thing per light
color tl60_rgb [ dmxid="21/3", fadetime=0 ]
// items
Color TL60_Colour "TL60 Colour" { channel="dmx:color:neewer:tl60_rgb:color" }
```

For **`rgbcw`** (opt-in; RGBCW-capable models such as the TL120C), five channels are
passed **straight through** to the light's native RGBCW mode — Red, Green, Blue,
Cool-White, Warm-White, one DMX channel per physical LED bank, with no colour-space
conversion. Each channel drives its own emitter, so you get independent colour + white
mixing (what `hsi`/`xy` can't do). Level rides in the channel values themselves.

This lays out exactly onto **openHAB's** native RGBCW support: a DMX `color` thing
(3ch RGB) plus a `tunablewhite` thing (2ch, in "cool white, warm white" order), patched
contiguously. Patch the light at DMX address `N`; the color thing gets channels
`N/3` and the tunablewhite gets `N+3,N+4`. Example for a light at `universe = 0`,
`address = 1`:

```
// things/neewer.things
Bridge dmx:artnet-bridge:neewer [ address="127.0.0.1", universe=0, refreshrate=30 ] {
    color        tl120c_rgb   [ dmxid="1/3", fadetime=0 ]
    tunablewhite tl120c_white [ dmxid="4,5", fadetime=0 ]
}
```
```
// items/neewer.items
Color  TL120C_Colour "Colour"          { channel="dmx:color:neewer:tl120c_rgb:color" }
Dimmer TL120C_White  "White [%d %%]"   { channel="dmx:tunablewhite:neewer:tl120c_white:brightness" }
Number TL120C_WTemp  "White temp"      { channel="dmx:tunablewhite:neewer:tl120c_white:color_temperature" }
```

Set the bridge's `[artnet] bind_ip` reachable from openHAB and point the thing's
`address` at the bridge host (`127.0.0.1` if co-located; default port 6454). `refreshrate`
is openHAB's Art-Net send rate — the bridge coalesces to `[ble] flush_hz` regardless.
Raise `fadetime` for smooth console-side fades. (No separate master-dimmer channel; the
color thing's own brightness scales R/G/B, tunablewhite's scales CW/WW. Use `advanced`'s
RGBCW band if you want a single master dimmer over all five.)

For **`advanced`** (the default for RGB-capable models), the Mode-select channel
(ch1) chooses among all built-in modes via value bands, and ch3–ch10 are
reinterpreted accordingly:

| ch1 band | Mode | ch3 | ch4 | ch5 | ch6 | ch7 | ch8 | ch9 |
|---|---|---|---|---|---|---|---|---|
| 0–31 | CCT | CCT | GM | — | — | — | — | — |
| 32–63 | HSI | Hue | Sat | — | — | — | — | — |
| 64–95 | FX | FX-id (1–18) | Speed | CCT | Hue | Sat/GM | Extra | 2nd-val |
| 128–159 | RGBCW | R | G | B | CW | WW | — | — |
| 192–231 | XY | X | Y | — | — | — | — | — |

(ch2 is always Dimmer. RGBCW (128–159) drives raw R/G/B + cool-white/warm-white
channels — independent LED mixing HSI/XY can't do. Bands 96–127, 160–191 and 232–255
are unimplemented and map to neutral white.)

For **`pixel`** (TL-series pixel fixtures such as the TL120C, **20 channels**), the
tube is split into **8 addressable segments**. ch1 = master Dimmer, ch2 =
**Effect-select**, ch3 = Speed/motion, ch4 = Direction, then ch5–ch20 are 8 ×
(Hue, Sat) pairs (seg1 Hue, seg1 Sat, seg2 Hue, …). The Effect channel picks the
pixel effect by value band — only the **5 effects that work over direct BLE** are
exposed (hardware-verified on the TL120C and TL60; of the 7 effect types these
models have, the other 2 are ignored unless relayed through a 2.4G hub):

| ch2 band | Effect | Segment meaning |
|---|---|---|
| `0–51` | ColorReplacement | all 8 segments = a spatial colour palette |
| `52–102` | SingleColorMoving | seg1 = background, seg2 = moving colour |
| `103–153` | TwoColorMoving | seg1 = background, seg2–3 = moving colours |
| `154–204` | ThreeColorMoving | seg1 = background, seg2–4 = moving colours |
| `205–255` | Fire | seg1 = background, seg2 = fire colour |

⚠️ **The pixel effects are animated** — the palette flows/moves along the tube;
Speed controls the rate (Speed 0 ≈ near-static). A **truly static** per-segment
render is **not available over BLE on the TL120C** (every pixel effect animates and
the firmware ignores the pause command). There is also **no per-segment brightness**
(one master dimmer). Uses the MAC-addressed `0xB0` pixel opcode; a non-pixel light
ignores it.

**Scaling:** Dimmer/Sat → 0–100%, Hue → 0–360°, GM → −50…+50, CCT → the model's
range (`cct_min`/`cct_max`), XY → CIE coordinate 0.0000–0.8000, RGBCW R/G/B/CW/WW →
raw 0–255, FX-id → 1–18, Speed → 1–10.

> **Hardware notes — the frame-form split (`cmd_type`):** whether the advanced-mode
> commands (XY / RGBCW / FX) are sent as **MAC-embedded** frames (`0xB7`/`0xA9`/`0x91`)
> or **direct** frames (`0xB9`/`0xA8`/`0x8B`) is per-model — the app's `commandType`,
> carried in the config's `cmd_type` field (`add` fills it from the catalog).
> - **TL120C (`cmd_type = 2`, verified 2026-07-01):** needs the MAC forms; ignores the
>   direct ones. CCT, HSI, XY, RGBCW, FX and per-segment pixel all work direct-BLE.
> - **TL21C (`cmd_type = 1`, verified 2026-07-02):** the mirror image — ignores every
>   MAC-embedded control frame; FX renders via the direct `0x8B` only. It has **no
>   XY/RGBCW/pixel** at all (those `advanced` bands are simply inert on it; CCT
>   2500–8500K, GM, HSI and all 18 FX work). Its GM was first mis-recorded as
>   "ignored" — the ±50 tint shift is just visually subtle (see the TL97C note).
> - **TL60 RGB (`cmd_type = 2`, verified 2026-07-03):** CCT 2500–10000K, **GM works**
>   (green/magenta tint renders over BLE), HSI, RGBCW (`0xA9`, incl. the CW/WW white
>   banks), all 18 FX (honours **both** the MAC `0x91` and direct `0x8B` forms), and
>   the same 5 pixel effects as the TL120C. **No XY** (both frame forms ignored; that
>   `advanced` band is inert on it).
> - **TL97C (`cmd_type = 1`, verified 2026-07-04):** a TL21C capability twin — CCT
>   2500–8500K, GM, HSI and all 18 FX work (FX via the direct `0x8B` only); no
>   XY/RGBCW/pixel, and it answers **no status reads at all** (not even battery).
>
> Other fixtures may differ — validate per model (`test --set` probes one frame at a
> time; see the test flags above). **GM caution:** the ±50 tint swing renders subtly —
> confirm GM on the fixture's display, not by eye (an eyeball-only probe once
> mis-labelled the TL21C "no GM").

## FX effects

The `advanced` profile's FX band (ch1 = 64–95) selects one of 18 built-in effects
on ch3 (scaled 1–18). ch4 sets speed (1–10); ch5–ch9 supply effect-specific
parameters (CCT, hue, saturation/GM, ember/colour/mode, and a second value for
loop effects).

| ID | Effect | ID | Effect | ID | Effect |
|---|---|---|---|---|---|
| 1 | Lightning | 7 | HUE-flash | 13 | CCT-loop |
| 2 | Paparazzi | 8 | CCT-pulse | 14 | INT-loop |
| 3 | Defective bulb | 9 | HUE-pulse | 15 | TV-screen |
| 4 | Explosion | 10 | Cop-Car | 16 | Fireworks |
| 5 | Welding | 11 | Candlelight | 17 | Party |
| 6 | CCT-flash | 12 | HUE-loop | 18 | Music |

FX is only available on models whose catalog entry has `supports_fx` (run
`neewer-bridge models`). The TL120C supports all 18.

## Drivers

`auto` detects Neewer Home (`NH-*`) lights and otherwise assumes classic. Set
`infinity` explicitly for newer MAC-addressed lights (auto can't detect those
reliably), or `home` for `NH-*` devices. Most current panels use `classic` —
the bridge connects directly over BLE, where the classic `0x78` frames work
(XY uses a MAC-addressed frame; FX uses the MAC-embedded effect frame, both of
which work over the direct connection).

## Reliability

Each light runs an independent supervisor that connects, verifies the link stays
alive (every `probe_secs`), and reconnects automatically — so a light that's
powered off, out of range, or briefly drops simply rejoins, still bound to its
configured DMX channels. Fast ArtNet input is coalesced to the light's flush rate
(`flush_hz`) to avoid overwhelming the BLE link. Validated by a 2-hour soak (zero
drops). The failsafe controls behaviour when ArtNet stops: `hold` keeps the last
state, `blackout` sets brightness 0, `poweroff` powers the light off — after
`timeout_secs` of silence.

**Detecting a wedged-but-connected light.** These are two-chip fixtures (a BLE
radio module in front of the LED MCU that runs the command parser), so a
`write-without-response` "succeeding" — or a generic GATT read completing — only
proves the *radio* answered; the command path can stall while the light sits at
its last colour, still showing "connected". The supervisor therefore verifies
liveness by a **reply**, not a successful write: it periodically sends a cheap
status query and requires a notify back; several silent probes recycle the link.
Some fixtures (e.g. this rig's TL60) answer no query at all — for those there is no
passive signal, so they get a **periodic forced reconnect** (`refresh_secs`), the
only thing that can clear such a stall on a light that can't be verified. Set
`refresh_secs = 0` to disable it; fixtures that reply are never force-refreshed.

Each supervisor also **reads device status** off the notify characteristic —
battery %, temperature, firmware version, and power/mode — querying on connect and
alongside each liveness probe. These are logged (at `info` when a value changes,
`debug` otherwise), so a running bridge surfaces battery/temperature per light. TL-
series (Infinity) fixtures only; Home (`NH-*`) lights are skipped.

## Troubleshooting

- **"no Bluetooth adapter found"** — the host has no Bluetooth radio enabled. Plug
  in a USB BLE adapter or run on a Bluetooth-equipped machine. `neewer-bridge
  adapters` lists what's available.
- **`scan` finds nothing / only phones** — a Neewer light **stops advertising
  while a phone app is connected to it**. Close the Neewer app (or disconnect it),
  put the light in Bluetooth/pairing mode, then retry; try `--seconds 12` or
  `--all`.
- **`scan` doesn't show a light you know is on** — lights already bound in the
  config are hidden (`scan` lists only new ones); the output notes how many were
  skipped. Use `--all` to see everything, configured lights marked.
- **A light doesn't respond to `test`/`run`** — try an explicit `--driver`
  (`infinity` for newer lights, `home` for `NH-*`). Use `-vv` to see BLE writes.
- **A mode does nothing** — confirm the model supports it (`neewer-bridge
  models`); bi-colour lights ignore HSI/XY/FX. Check the Mode-select channel is in
  the right band (see [DMX profiles](#dmx-profiles-channel-layouts)).
- **ArtNet not received** — verify the source targets this host's IP and the
  configured `universe`/`address`; use `neewer-bridge monitor` to watch packets,
  and `neewer-bridge lights` to confirm the expected channel mapping.
- **A light is stuck in an FX effect** — FX latches the fixture; power-cycle it
  (or run `neewer-bridge test <MAC>`, which power-cycles and restores white).

## License

MIT.
