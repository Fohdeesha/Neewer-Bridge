# Neewer-Bridge

An efficient, reliable **ArtNet → Neewer Bluetooth** light bridge. It receives
ArtNet (DMX-over-UDP) and controls Neewer LED lights over their proprietary BLE
protocol. CLI, config-file driven, runs on **Windows and Linux**.

- Drives **multiple lights at once**, each bound to a fixed set of DMX channels.
- **Deterministic binding by MAC** — the DMX-channel → physical-light mapping is
  identical on every boot, independent of power-on or discovery order.
- Supports classic (`0x78`), Infinity (`0x78`+MAC), and Neewer Home (`0x7A`,
  `NH-*`) lights.
- Full built-in control: **CCT (+GM), HSI, raw RGBCW, CIE XY, and the 18-effect
  FX engine** — selectable live from a single DMX mode channel.
- **141-model capability catalog** so `add` auto-fills a light's driver, profile,
  and CCT range from its Bluetooth name.

> Status: hardware-validated end-to-end on a Neewer TL120C — CCT, HSI, RGBCW, XY,
> and FX all confirmed over a direct BLE connection. See `NOTES.md` for the full
> design, protocol reverse-engineering, and reliability notes.

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

```sh
# 1. Confirm your Bluetooth adapter and see what's nearby
neewer-bridge scan

# 2. Pair a light interactively — it blinks the fixture so you know which is
#    which, identifies the model, and writes a [[lights]] entry to your config.
neewer-bridge add --config neewer-bridge.toml

# 3. Review the DMX channel mapping it produced
neewer-bridge lights --config neewer-bridge.toml

# 4. Run the bridge
neewer-bridge run --config neewer-bridge.toml
```

Point your lighting console / software (QLC+, etc.) at this host's IP on the
configured ArtNet universe and DMX address.

## Commands

```
neewer-bridge [--config PATH] [-v|-vv] <COMMAND>
```

| Command | What it does |
|---|---|
| `adapters` | List BLE adapters (index + name) for the `[ble] adapter` setting. |
| `models` | List the built-in light-model catalog (what `add` matches against). |
| `lights` | Show configured lights and their absolute universe + DMX-channel mapping. |
| `scan` | Discover lights; list name / MAC / RSSI. |
| `add` | Bind a light to the config (interactive, or non-interactive with `--mac`). |
| `inspect <MAC>` | Connect and dump a device's full GATT (identify unknown lights). |
| `test <MAC>` | Connect one light and prove control (blink + CCT, optional colour/mode probes). |
| `artnet-send` | Send ArtDmx to drive the bridge / a node without a console. |
| `monitor` | Listen for ArtNet and print received ArtDmx (no BLE needed). |
| `run` | The full bridge: ArtNet → mapper → per-light BLE actors. |

### Global flags

| Flag | Default | Meaning |
|---|---|---|
| `--config PATH` | `neewer-bridge.toml` | Config file path. |
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

**`scan`** — discover lights.
| Flag | Default | Meaning |
|---|---|---|
| `--seconds N` | `6` | Scan duration. |
| `--all` | off | List every BLE device, not just Neewer lights. |
| `--json` | off | Machine-readable JSON array (name / mac / rssi / neewer). |

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
| `--profile P` | — | `cct`\|`cct_gm`\|`hsi`\|`full`\|`advanced` (default: from model). |
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
| `--modes` | off | Probe the advanced modes: RGBCW, XY, and a few FX effects. |

`test` blinks the light 3× (also a visual identify), sets 5600K @ 50%, then runs
any requested probes. FX latches the light into effect mode — `test` power-cycles
to exit and restores white.

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

See `config.example.toml`. Lights are bound by **MAC address** — the stable
identity that makes the DMX→light mapping deterministic across reboots.

```toml
[artnet]
bind_ip = "0.0.0.0"   # interface to receive ArtNet on (0.0.0.0 = all)
port    = 6454        # standard ArtNet port (configurable)

[ble]
adapter    = "default"  # "default", an index ("0"), or a name substring
flush_hz   = 15         # max BLE state updates per light per second (coalescing cap)
probe_secs = 20         # liveness GATT-probe interval (stale-session detection)

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
profile  = "advanced"           # cct | cct_gm | hsi | full | advanced
universe = 0                    # ArtNet 15-bit Port-Address (Net/Sub-Net/Universe)
address  = 1                    # 1-based DMX start channel
power_on_connect = true         # power the light on when it first connects
cct_min  = 32                   # CCT range min, raw ×100K (32 = 3200K)
cct_max  = 56                   # CCT range max, raw ×100K (56 = 5600K; TL120C = 25..100)
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
| `full`     | 5  | 1 Dimmer · 2 **Mode** · 3 CCT/Hue · 4 GM/Sat · 5 reserved |
| `advanced` | 10 | 1 **Mode-select** · 2 Dimmer · 3–10 mode-specific (below) |

For **`full`**, the Mode channel (ch2) selects sub-mode live: `0–127` = CCT
(ch3=CCT, ch4=GM), `128–255` = HSI (ch3=Hue, ch4=Saturation).

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

(ch2 is always Dimmer. Bands 96–127 / 160–191 / 232–255 are unimplemented and map
to neutral white.)

**Scaling:** Dimmer/Sat → 0–100%, Hue → 0–360°, GM → −50…+50, CCT → the model's
range (`cct_min`/`cct_max`), R/G/B/CW/WW → 0–255 raw, XY → CIE coordinate
0.0000–0.8000, FX-id → 1–18, Speed → 1–10.

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
(RGBCW/XY are direct classic frames; FX uses the MAC-embedded effect frame, which
works over the direct connection).

## Reliability

Each light runs an independent supervisor that connects, keeps the connection
alive with a periodic GATT read probe (`probe_secs`), and reconnects
automatically — so a light that's powered off, out of range, or briefly drops
simply rejoins, still bound to its configured DMX channels. Fast ArtNet input is
coalesced to the light's flush rate (`flush_hz`) to avoid overwhelming the BLE
link. Validated by a 2-hour soak (zero drops). The failsafe controls behaviour
when ArtNet stops: `hold` keeps the last state, `blackout` sets brightness 0,
`poweroff` powers the light off — after `timeout_secs` of silence.

## Troubleshooting

- **"no Bluetooth adapter found"** — the host has no Bluetooth radio enabled. Plug
  in a USB BLE adapter or run on a Bluetooth-equipped machine. `neewer-bridge
  adapters` lists what's available.
- **`scan` finds nothing / only phones** — a Neewer light **stops advertising
  while a phone app is connected to it**. Close the Neewer app (or disconnect it),
  put the light in Bluetooth/pairing mode, then retry; try `--seconds 12` or
  `--all`.
- **A light doesn't respond to `test`/`run`** — try an explicit `--driver`
  (`infinity` for newer lights, `home` for `NH-*`). Use `-vv` to see BLE writes.
- **A mode does nothing** — confirm the model supports it (`neewer-bridge
  models`); bi-colour lights ignore HSI/RGBCW/XY/FX. Check the Mode-select channel
  is in the right band (see [DMX profiles](#dmx-profiles-channel-layouts)).
- **ArtNet not received** — verify the source targets this host's IP and the
  configured `universe`/`address`; use `neewer-bridge monitor` to watch packets,
  and `neewer-bridge lights` to confirm the expected channel mapping.
- **A light is stuck in an FX effect** — FX latches the fixture; power-cycle it
  (or run `neewer-bridge test <MAC>`, which power-cycles and restores white).

## License

MIT.
