# Hardware bring-up procedure

The steps to validate the bridge end-to-end once a **USB Bluetooth adapter** is
plugged in and a **Neewer light** is powered on nearby. Every step is fully
scriptable (no interactive prompts), so it can be automated and the BLE traffic
inspected.

All commands assume `target\debug\neewer-bridge.exe` (or `cargo run --`).
Add `-v` to see BLE writes (hex), `-vv` to also see btleplug wire logs.

## 0. Confirm the adapter (and that address == real MAC)

```sh
neewer-bridge scan --all --seconds 8
```
Expected: a table listing BLE devices. The Neewer light shows with `TYPE=Neewer`.
**Key check:** its MAC column is a real 6-byte address (e.g. `C9:1F:…`), not
zeros — this confirms btleplug exposes the hardware MAC we bind by. Machine
form: `neewer-bridge scan --json --all` (keep `--all` in scripts — a plain
`scan` hides lights already bound in the config).

## 1. Prove the BLE control path + identify the driver

```sh
neewer-bridge test <MAC> --driver auto -v
```
- It blinks the fixture 3× (so you can confirm which physical light it is), then
  sets 5600K @ 50%.
- Watch the `-v` log for `BLE write bytes=…`. If the light doesn't react, retry
  with `--driver infinity` (newer MAC-addressed lights) or `--driver home`
  (`NH-*`). Note the driver that works — use it everywhere below.

## 2. Add the light to a config (non-interactive)

```sh
neewer-bridge add --config bringup.toml \
  --mac <MAC> --driver <classic|infinity|home> \
  --profile cct --universe 0 --address 1 --name "Bringup"
```
Writes a `[[lights]]` block. (`--blink` would also blink it; skip if step 1 did.)

## 3. Run the bridge (background, logging)

```sh
neewer-bridge -v run --config bringup.toml > bringup.log 2>&1 &
```
Expect in the log: adapter selected, `BLE scan started`, the actor `connecting`,
then `session active`. The light powers on (power_on_connect) and shows the
initial 50% / 3200K state.

## 4. Drive it and verify the exact bytes

Send DMX with the `artnet-send` helper and check the BLE writes in `bringup.log`.
For the **`cct` profile at address 1** (ch1 = Dimmer, ch2 = CCT) on a **classic**
light, the bridge should emit these exact frames (each `78 87 02 <brr> <cct> <ck>`):

| Command | DMX in | Mapped | Expected BLE write |
|---|---|---|---|
| `artnet-send --universe 0 --address 1 --channels 255,255` | dim 255, cct 255 | brr 100, cct 56 | `78 87 02 64 38 9d` |
| `artnet-send --universe 0 --address 1 --channels 128,255` | dim 128, cct 255 | brr 50, cct 56 | `78 87 02 32 38 6b` |
| `artnet-send --universe 0 --address 1 --channels 255,0`   | dim 255, cct 0   | brr 100, cct 32 | `78 87 02 64 20 85` |

(Infinity/Home lights emit different frames — see `src/driver.rs`. The point is
the logged bytes must match the encoder output for the mapped values, and the
light must visibly change.)

Stream to exercise the coalescing flush (only changed states should be written):
```sh
neewer-bridge artnet-send --universe 0 --address 1 --channels 200,128 --hz 40 --seconds 3
```
At 40 Hz in but a 15 Hz flush cap, you should see far fewer than 120 BLE writes,
and none while the value is constant.

## 5. Reliability checks

- **Reconnect:** power-cycle the light (or move it out of range). The log should
  show the session end, retries, and a clean reconnect — still bound to the same
  DMX channels. Confirm control resumes.
- **Liveness probe:** with `-vv`, idle the light (no ArtNet); every `probe_secs`
  you should see the read-probe activity; killing the link should trip the
  stale-session recycle after the configured failures.
- **Failsafe:** set `[failsafe] mode="poweroff"` / `timeout_secs=3`, run, send one
  packet, then stop. After ~3 s the log shows `ArtNet lost — applying failsafe`
  and a power-off write; resume sending → it powers back on.

## What to capture for the record

- `scan` output proving real MAC.
- A `bringup.log` excerpt showing mapped DMX → expected BLE bytes (step 4 table).
- A reconnect cycle in the log.
- Note any model-specific quirks (driver needed, whether a readable
  characteristic exists for the liveness probe, CCT range) back into the
  README's hardware notes.
