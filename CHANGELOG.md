# Changelog

All notable changes to Neewer-Bridge are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/):
**MAJOR** = breaking config/CLI/protocol-behaviour changes, **MINOR** = new
features (new commands, profiles, supported models), **PATCH** = fixes.

Release procedure: bump `version` in `Cargo.toml`, add an entry here, commit,
then tag `vX.Y.Z`. The binary prints its version on startup (first log line)
and via `neewer-bridge --version`.

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

[1.0.0]: https://github.com/Fohdeesha/Neewer-Bridge/releases/tag/v1.0.0
