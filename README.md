# Neewer-Bridge

Control Neewer LED lights from anything that speaks ArtNet (QLC+, a lighting
console, openHAB...). The bridge receives DMX over the network and drives the
lights over Bluetooth. Windows and Linux, command line, one config file.
Tested against real TL120C, TL21C, TL60 RGB, and TL97C fixtures.

## Getting started

1. Download the zip for your OS from the
   [releases page](https://github.com/Fohdeesha/Neewer-Bridge/releases) and
   unzip it anywhere. You need a Bluetooth LE adapter; a cheap USB dongle is
   fine. (Or [build from source](#building-from-source).)

2. Add your lights. Power them on and make sure the Neewer phone app isn't
   connected to them (a light stops advertising while the app holds it), then:

   ```
   neewer-bridge add
   ```

   This scans, blinks the light it found so you know which one it is, asks for
   an ArtNet universe and DMX address, and writes the entry to `config.toml`.
   Run it once per light. The model is recognized from the Bluetooth name
   (141 models known), so the driver, channel profile, and CCT range are filled
   in for you. `neewer-bridge lights` shows the channel map you ended up with.

3. Start the bridge:

   ```
   neewer-bridge
   ```

4. Point your ArtNet source at this machine's IP (standard port 6454) on the
   universe and address you picked.

Lights are bound by MAC address, so the DMX mapping survives reboots and
doesn't depend on what order things power on in. A light that drops, loses
power, or goes out of range reconnects on its own.

## Commands

Run `neewer-bridge <command> --help` for the flags on each.

| Command | What it does |
|---|---|
| `run` | The bridge itself. This is the default, so plain `neewer-bridge` works. |
| `scan` | List nearby lights (name, MAC, signal). |
| `add` | Add a light to the config. Interactive, or scriptable with `--mac`. |
| `lights` | Show configured lights and their DMX channel mapping. |
| `test <MAC>` | Connect one light and prove control: blink, set white, plus optional colour/mode/status probes. |
| `monitor` | Print incoming ArtNet without touching any lights. |
| `artnet-send` | Send test DMX values without needing a console. |
| `models` | List the built-in model catalog. |
| `adapters` | List Bluetooth adapters. |
| `inspect <MAC>` | Dump a device's GATT services (for unknown lights). |
| `ota <MAC>` | Flash Neewer firmware to a light. `--check` dry-runs, `--confirm` writes. Stop the bridge first. |

`-v` turns on debug logging (shows every BLE write as hex), `-vv` adds the
Bluetooth wire logs. `--config PATH` points at a different config file; by
default the bridge uses `config.toml` next to the binary if there is one,
otherwise the one in the current directory.

## Configuration

`add` writes light entries for you, and the shipped `config.toml` documents
every field, so you rarely need to touch this by hand. The short version:

```toml
[artnet]
bind_ip = "0.0.0.0"
port    = 6454

[ble]
adapter  = "default"    # or an index ("0") or a name substring
flush_hz = 15           # max BLE updates per light per second

[failsafe]              # what to do when ArtNet stops arriving
mode         = "hold"   # hold | blackout | poweroff
timeout_secs = 0        # 0 = hold the last state forever

[logging]
level   = "info"
console = true
# file  = "neewer-bridge.log"   # optional rotating log file

[[lights]]
mac      = "AA:BB:CC:DD:EE:FF"
name     = "Key light"
profile  = "rgb"        # channel layout, see below
universe = 0
address  = 1            # 1-based DMX start channel
```

A broken config is a hard error; the bridge never silently falls back to
defaults. The startup log prints the absolute path of the config it loaded.

## DMX profiles

The profile sets a light's channel layout. `add` picks a sensible one for the
model; change it in the config if you want something else.

| Profile | Ch | Layout |
|---|---|---|
| `cct`      | 2  | Dimmer, CCT |
| `cct_gm`   | 3  | Dimmer, CCT, green/magenta |
| `hsi`      | 3  | Dimmer, Hue, Saturation |
| `rgb`      | 3  | Red, Green, Blue |
| `rgbcw`    | 5  | Red, Green, Blue, Cool White, Warm White |
| `full`     | 5  | Dimmer, Mode (CCT/HSI), CCT/Hue, GM/Sat |
| `advanced` | 10 | Mode-select, Dimmer, then 8 mode-specific channels |
| `pixel`    | 20 | Dimmer, Effect, Speed, Direction, then 8 pairs of Hue+Sat |

`rgb` is the easiest way to get colour: three plain R/G/B channels, converted
to the light's HSI mode internally, so it works on every colour model. In
openHAB that's a single 3-channel DMX `color` thing per light, no rules needed.

`rgbcw` passes all five channels straight through to the light's native RGBCW
mode, one channel per LED bank (needs a model that has one, like the TL120C).
It lines up with openHAB's `color` (3ch) + `tunablewhite` (2ch) things patched
back to back.

`advanced` exposes every built-in mode through one mode-select channel (ch1),
with ch2 as dimmer and ch3 onward reinterpreted per mode:

| ch1 | Mode | Channels |
|---|---|---|
| 0–31 | CCT | ch3 CCT, ch4 GM |
| 32–63 | HSI | ch3 Hue, ch4 Sat |
| 64–95 | FX | ch3 effect 1–18, ch4 speed, ch5–9 effect params |
| 128–159 | RGBCW | ch3–7 R, G, B, CW, WW |
| 192–231 | XY | ch3 X, ch4 Y |

The 18 FX effects: Lightning, Paparazzi, Defective bulb, Explosion, Welding,
CCT-flash, HUE-flash, CCT-pulse, HUE-pulse, Cop-Car, Candlelight, HUE-loop,
CCT-loop, INT-loop, TV-screen, Fireworks, Party, Music.

`pixel` (TL-series tubes) splits the tube into 8 segments and drives one of 5
animated effects across them, selected on ch2: ColorReplacement (0–51),
SingleColorMoving (52–102), TwoColorMoving (103–153), ThreeColorMoving
(154–204), Fire (205–255). For the moving/fire effects, segment 1 is the
background colour. The effects always animate; there is no truly static
per-segment mode over BLE.

Not every model supports every mode (XY, RGBCW, and pixel in particular vary).
The catalog knows what each model can do (`neewer-bridge models`); a mode the
light doesn't have is simply ignored.

## Multiple DMX sources

The bridge can listen on several ports/IPs at once and merge the streams per
channel, like a hardware DMX merger. Typical setup: openHAB drives the everyday
state on the standard port while a console overrides on a second one.

```toml
[artnet]
merge = "ltp"            # ltp | htp | lowest
merge_timeout_secs = 10  # silent source drops out of the merge; 0 = never

[[artnet.inputs]]        # each block adds another listener
name = "console"
port = 6455
```

`htp` takes the highest value per channel, `lowest` the lowest. `ltp` (the
default) gives each channel to whichever source changed it most recently.
That's last-*changed*, not last-received, so a source re-streaming the same
values never steals a channel back from one that actually changed it. When a
source goes silent its channels fall back to the remaining sources.
`neewer-bridge monitor` prints each input's packets plus the merged result,
which is the easiest way to check a merge setup.

## Troubleshooting

- **Everything is white and colour changes only the brightness.** The lights
  are on the wrong profile, usually because the bridge loaded a different
  config than you think: it prefers `config.toml` next to the binary over the
  one in your working directory. Run `neewer-bridge lights` to see which
  profile each light actually loaded, and check the `loaded config` line in the
  startup log.
- **`scan` finds nothing, or only phones.** A Neewer light stops advertising
  while the phone app is connected to it. Close the app, put the light in
  Bluetooth mode, retry.
- **`scan` doesn't show a light you know is on.** Already-configured lights
  are hidden; use `scan --all` to see everything.
- **A light stopped responding and is stuck on its last colour.** Almost
  always a weak Bluetooth link. Move the light or the adapter closer and it
  reconnects on its own; RSSI is logged per light so you can spot a marginal
  placement. If it stays dead even right next to the adapter, power-cycle it.
- **A light is stuck in an FX effect.** Effects latch. Power-cycle it, or run
  `neewer-bridge test <MAC>`, which resets it to white.
- **ArtNet isn't arriving.** Check the source targets this host's IP and the
  right universe. `neewer-bridge monitor` shows what's actually being received.

## Building from source

Needs the [Rust toolchain](https://rustup.rs). On Linux, also
`libdbus-1-dev` and `pkg-config`. On Windows, the MSVC build tools.

```sh
git clone https://github.com/Fohdeesha/Neewer-Bridge.git
cd Neewer-Bridge
cargo build --release
# binary at target/release/neewer-bridge
```

## License

MIT — see [LICENSE](LICENSE).
