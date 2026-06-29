# Neewer-Bridge

An efficient, reliable **ArtNet → Neewer Bluetooth** light bridge. It receives
ArtNet (DMX-over-UDP) and controls Neewer LED lights over their proprietary BLE
protocol. CLI, config-file driven, runs on **Windows and Linux**.

- Drives **multiple lights at once**, each bound to a fixed set of DMX channels.
- **Deterministic binding by MAC** — the DMX-channel → physical-light mapping is
  identical on every boot, independent of power-on or discovery order.
- Supports classic (`0x78`), Infinity (`0x78`+MAC), and Neewer Home (`0x7A`,
  `NH-*`) lights.

> Status: the full receive→map→BLE pipeline is implemented and unit-tested.
> Live BLE validation requires a Bluetooth adapter + a light (see
> [Troubleshooting](#troubleshooting)). See `NOTES.md` for the full design and
> reverse-engineering notes.

## Install

Requires the [Rust toolchain](https://rustup.rs) (stable). On Windows the MSVC
toolchain is used (Visual Studio Build Tools / Community).

```sh
git clone <this repo>
cd Neewer-Bridge
cargo build --release
# binary at target/release/neewer-bridge
```

## Quick start

```sh
# 1. See what's nearby (also confirms your Bluetooth adapter works)
neewer-bridge scan

# 2. Pair a light interactively: it blinks the fixture so you know which is which,
#    then writes a [[lights]] entry to your config.
neewer-bridge add --config neewer-bridge.toml

# 3. Run the bridge
neewer-bridge run --config neewer-bridge.toml
```

Point your lighting console / software (QLC+, etc.) at this host's IP on the
configured ArtNet universe and address.

## Commands

| Command | What it does |
|---|---|
| `scan [--seconds N] [--all]` | Discover lights; list name / MAC / RSSI. |
| `add` | Interactive pairing: scan → blink-to-identify → append to config. |
| `test <MAC> [--driver classic\|infinity\|home\|auto]` | Connect one light and prove control (blink + set CCT). |
| `monitor` | Listen for ArtNet and print received ArtDmx (no BLE needed). |
| `run` | The full bridge: ArtNet → mapper → per-light BLE. |

Global flags: `--config PATH` (default `neewer-bridge.toml`), `-v`/`-vv` for more
logging (`-vv` includes BLE wire logs). `RUST_LOG` overrides, e.g.
`RUST_LOG=neewer_bridge=trace`.

`scan`, `add`, `test`, and `monitor` work without a config file; `run` requires a
valid one.

## Configuration

See `config.example.toml`. Lights are bound by **MAC address**.

```toml
[artnet]
bind_ip = "0.0.0.0"   # interface to receive ArtNet on
port    = 6454        # standard ArtNet port (configurable)

[ble]
adapter    = "default"
flush_hz   = 15       # max BLE updates per light per second
probe_secs = 20       # connection liveness probe interval

[failsafe]            # what to do when ArtNet stops arriving
mode         = "hold" # hold | blackout | poweroff
timeout_secs = 0      # 0 = never act; >0 = act after N seconds of silence

[[lights]]
mac      = "AA:BB:CC:DD:EE:FF"  # binding identity (required)
name     = "Key light"
driver   = "auto"               # auto | classic | infinity | home
profile  = "full"               # cct | cct_gm | hsi | full
universe = 0                    # ArtNet 15-bit Port-Address (Net/Sub-Net/Universe)
address  = 1                    # 1-based DMX start channel
power_on_connect = true
```

### DMX profiles (channel layouts)

All channels are 8-bit. The master **Dimmer** sets brightness only — it never
cuts power (lights are powered on and kept on; use the failsafe or remove power
externally to turn them off).

| Profile | Ch | Layout |
|---|---|---|
| `cct`    | 2 | 1 Dimmer · 2 CCT |
| `cct_gm` | 3 | 1 Dimmer · 2 CCT · 3 GM |
| `hsi`    | 3 | 1 Dimmer · 2 Hue · 3 Saturation |
| `full`   | 5 | 1 Dimmer · 2 **Mode** · 3 CCT/Hue · 4 GM/Sat · 5 reserved |

For `full`, the **Mode** channel selects sub-mode live: `0–127` = CCT (white,
ch3=CCT, ch4=GM), `128–255` = HSI (colour, ch3=Hue, ch4=Saturation).

Scaling: Dimmer/Sat → 0–100%, Hue → 0–360°, GM → −50…+50, CCT → the model's
range (default 3200K–5600K). FX/scene effects are not exposed yet.

### Drivers

`auto` detects Neewer Home (`NH-*`) lights and otherwise assumes classic. Set
`infinity` explicitly for newer MAC-addressed lights (auto can't detect those
reliably), or `home` for `NH-*` devices.

## Reliability

Each light runs an independent supervisor that connects, keeps the connection
alive with a periodic GATT read probe, and reconnects automatically — so a light
that's powered off, out of range, or briefly drops simply rejoins, still bound to
its configured DMX channels. Fast ArtNet input is coalesced to the light's flush
rate to avoid overwhelming the BLE link.

## Troubleshooting

- **"no Bluetooth adapter found"** — the host has no Bluetooth radio enabled. Plug
  in a USB BLE adapter or run on a Bluetooth-equipped machine.
- **`scan` finds nothing** — power the light on, bring it close, try
  `--seconds 12` or `--all`.
- **A light doesn't respond to `test`/`run`** — try an explicit `--driver`
  (`infinity` for newer lights, `home` for `NH-*`). Use `-vv` to see BLE writes.
- **ArtNet not received** — verify the source targets this host's IP and the
  configured `universe`/`address`; use `neewer-bridge monitor` to watch packets.

## License

MIT.
