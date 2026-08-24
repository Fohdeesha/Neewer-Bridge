# Changelog

All notable changes to Neewer-Bridge are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/):
**MAJOR** = breaking config/CLI/protocol-behaviour changes, **MINOR** = new
features (new commands, profiles, supported models), **PATCH** = fixes.

Release procedure: bump `version` in `Cargo.toml`, add an entry here, commit,
then tag `vX.Y.Z` and push the tag — GitHub Actions builds the Windows/Linux
binaries, runs the tests, and publishes the GitHub release automatically (with
this file's entry as the release notes). The binary prints its version on
startup (first log line) and via `neewer-bridge --version`.

## [1.2.0] — 2026-08-24

### Changed

- **The release zips now ship `config.example.toml` instead of `config.toml`.**
  Unzipping a new version over an existing install used to overwrite the live
  configuration — every light, address, and setting — because the archive
  contained a file with exactly that name. The bundled file is now a template
  the bridge never reads, so upgrading in place leaves `config.toml` alone.
  Copy it once (`cp config.example.toml config.toml`) when setting up, or let
  `add` do it for you.
- The shipped template no longer contains the developer's own fixtures, which a
  fresh install would otherwise have tried to connect to. It carries a
  commented-out `[[lights]]` block instead.

### Added

- `neewer-bridge add` creates `config.toml` from the bundled
  `config.example.toml` when there isn't one yet, so a first-run config keeps
  every documented default and comment rather than holding only the light that
  was just added.
- A missing config now says how to make one — naming the example file on disk —
  instead of `run` failing with a bare "No such file or directory".

## [1.1.2] — 2026-08-24

### Fixed

- **One light silently ignored while the others work.** If a light's DMX
  channels ran past the end of the data a console was actually sending (a wrong
  `address`, or a console configured for a smaller universe), the bridge skipped
  that light with nothing in the log — it just sat on its last colour, looking
  like dead hardware. It now warns once naming the light and its channel range,
  and logs again when the data covers it (the light still holds its last state,
  as before).
- **Firmware OTA header with a long device name.** The `0x96` header's
  single-byte length field wrapped for names of 245 characters or more (the name
  defaults to the firmware filename's stem), producing a frame whose declared
  length disagreed with its contents — which these two-chip fixtures re-frame
  by. The cosmetic name is now trimmed to fit.
- **`artnet-send` with an out-of-range `--address`.** Channel values placed past
  channel 512 were dropped when the packet was encoded, and the command still
  reported a successful send. It now refuses the patch with a clear message.
- **`test --set` with an out-of-range value.** Numbers wider than the field they
  land in wrapped silently (`…:300` became `44`), so the probe reported a value
  it never sent. Out-of-range arguments are now rejected; `--set raw:<hex>`
  remains the escape hatch for deliberately out-of-spec frames.

### Changed

- `run` validates its configuration up front, so a malformed config reported by
  a library caller is a plain startup error rather than a panic in a background
  task. (Nothing changes for the CLI, which already validated on load.)

## [1.1.1] — 2026-08-18

### Changed

- **Log timestamps are now local time in a short `MM-DD HH:MM:SS` form** (e.g.
  `07-10 09:25:01`) instead of the RFC-3339 UTC stamp
  (`2026-07-10T09:25:01.627109Z`). Applies to both the console and the log
  file. Nothing else about the log lines changed.

## [1.1.0] — 2026-07-20

### Added

- **Multiple ArtNet inputs with per-channel DMX merge.** The bridge can now
  listen for ArtNet on several sockets at once — extra UDP ports and/or
  different bind IPs — and merge the streams per channel before driving the
  lights, like a hardware DMX merger. Each `[[artnet.inputs]]` config block
  adds a listener (up to 8 total, each with optional `name`, `bind_ip`,
  `port`); the existing `[artnet] bind_ip`/`port` is always input 0.
- **Merge modes** (`[artnet] merge`): `htp` (highest takes precedence),
  `lowest`, and `ltp` (default) — latest takes precedence per channel, where
  "latest" means the source that most recently **changed** the value, not the
  one that last re-sent it: a source streaming unchanged refreshes never
  steals a channel back from a live override.
- **Source expiry** (`[artnet] merge_timeout_secs`, default 10, `0` = never):
  a source silent past the timeout is dropped from the merge — its HTP/lowest
  contribution disappears, and LTP channels it owned fall back to the most
  recently active remaining source. A channel with no live source holds its
  last value (total loss remains the `[failsafe]`'s job).
- `monitor` now listens on **every** configured input, tags each logged packet
  with its input label, and (with multiple inputs) logs the merged output
  whenever it changes — a live view of the exact merge pipeline `run` uses.

### Changed

- ArtDmx sequence tracking is now per input (so one console feeding two inputs
  keeps independent sequence streams). Single-input configs behave exactly as
  before; the merge settings are inactive until a second input is added.
- Config validation now rejects `[artnet] port = 0` (it would bind an
  arbitrary ephemeral port — never useful for a listener) and a wildcard
  (`0.0.0.0`) input sharing a port with another input (the OS would refuse
  the second bind at startup anyway; two *specific* IPs may share a port).

## [1.0.0] — 2026-07-18

First versioned release. Everything below is the state of the project as of
this date — hardware-validated on a five-light fleet (2× TL120C, TL60 RGB,
TL21C, TL97C) deployed on Linux/BlueZ.

### Core bridge

- ArtNet (ArtDmx) → Neewer BLE bridge: UDP listener, DMX channel mapper,
  one coalescing actor per configured light (`run`, the default command).
- Deterministic binding by **MAC address** — mapping is independent of
  power-on / discovery / connection order.
- Per-light DMX profiles: `cct`, `cct_gm`, `hsi`, `rgb` (R/G/B→HSI, the
  openHAB-friendly default), `rgbcw` (native 5-ch passthrough), `full`,
  `advanced` (10-ch mode-select: CCT/HSI/FX/RGBCW/XY bands), `pixel`
  (20-ch, 8-segment colour + 5 effects).
- Protocol drivers: classic `0x78` (incl. MAC-addressed Infinity frames,
  selected per model via `cmd_type`), Neewer Home `0x7A`. Encoders byte-exact
  against the decompiled NEEWER Studio app; 100+ unit tests.
- 141-model capability catalog (`models.toml`) extracted from the app;
  `add` auto-identifies a light and fills driver/profile/CCT range.
- Reliability: connection-health probing with recycle-on-failure, jittered
  reconnect backoff (no fleet-wide reconnect storms), write coalescing at a
  capped flush rate, on-demand duty-cycled discovery scanning (kind to cheap
  adapters), ArtDmx sequence tracking (stale-packet drop), configurable
  failsafe (`hold`/`blackout`/`poweroff`).
- Configurable logging: console + size-rotating file sinks with per-sink
  levels; startup logs the version, the loaded config path, and each light's
  profile/channel span.

### CLI

- `run` (default), `scan`, `add` (interactive blink-to-identify or scripted),
  `test` (guided one-frame probes, `--status`, `--colors`, `--modes`,
  `--pixel`, `raw:` spelunking), `ota` (firmware flashing over the custom
  `0x78` block protocol — check-code validated, no-brick by construction),
  `lights`, `models`, `adapters`, `inspect`, `monitor`, `artnet-send`.
- `--version` prints the release; the version is also the first startup
  log line.

[1.1.0]: https://github.com/Fohdeesha/Neewer-Bridge/releases/tag/v1.1.0
[1.0.0]: https://github.com/Fohdeesha/Neewer-Bridge/releases/tag/v1.0.0
