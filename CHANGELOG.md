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

## [1.6.0] — 2026-08-26

Fixes from a further full-source audit. MINOR rather than PATCH because three
of them **reject input that used to be accepted** (see *Changed* below).

### Fixed

- **The discovery-scan coordinator could wedge permanently, leaving the adapter
  scanning continuously forever.** If a scan session was already active — ours,
  left behind by a `stop_scan` that didn't take, or stale in the BLE backend —
  BlueZ answers `Operation already in progress`, and the coordinator only warned
  and retried, so it could never get back in sync. Found on the test rig, where
  a live bridge had been in exactly that state for **15+ hours**: zero
  successful discoveries for the whole run, ~4 warnings a minute (each claiming
  "will retry" as though transient), and the adapter scanning non-stop — the
  precise load this module exists to prevent on a cheap controller. The failure
  path now clears the stale session and retries once, which a hardware
  reproduction proves recovers it (3 plain retries stay wedged; `stop_scan` then
  `start_scan` succeeds).
- **Log lines hid the cause of the errors they reported.** `error = %e` on an
  `anyhow` chain prints only the outermost context, so the wedge above surfaced
  as `start_scan failed (check Bluetooth permissions / adapter power)` with the
  actual `Operation already in progress` discarded — which is why it took a
  hardware session to diagnose at all. The scan-start failure and the four
  per-light failures that carry a context chain (`session ended`,
  `connect/verify failed`, `error listing peripherals`, `notify subscribe
  failed`) now print `{e:#}`.
- **`artnet-send --universe` was not range-checked, so it sent to a different
  universe than it reported.** The Art-Net Port-Address is 15 bits, so the
  encoder masks the `Net` byte — `--universe 40000` went out as universe 7232,
  and `--universe 32768` (one past the maximum, the likeliest typo there is)
  went out as **universe 0**, driving whatever is patched there while the
  success line still said 32768. Measured with a live `monitor`, not inferred.
  `--address` already had this check, as does a light's `universe` in config;
  this was the one path that skipped it. The error now names the universe the
  packet would actually have reached.
- **A model in the shipped catalog could not be added at all.** `ZRP` carries
  the app's `commandType = 3` while config validation capped `cmd_type` at 2, so
  `add` built an entry and then refused to write it — in the interactive path
  after the scan, the blink and every prompt, complaining about a field the user
  never typed. Only `cmd_type == 2` is special to the driver (MAC-embedded
  advanced frames), so the bound was simply wrong; it is now the catalog's real
  range, and a new test walks every catalog model through the validator so the
  two can never drift apart again.
- **`ota --settle-secs 0` skipped the pre-flash link check and still announced
  "link held steady — OK to proceed".** The check loop ran zero times, so
  `is_connected` was never called, on the one safety gate before a multi-minute
  firmware write. The connectivity check now runs at least once whatever the
  window is, and reports the elapsed time from the clock rather than a tick
  count.
- **A connection probe that recovered said so only at `debug`.** Every probe
  failure warns, so at the default `info` level a link blip that cleared after
  one or two misses left a `connection probe failed` warning standing with
  nothing after it — on exactly the marginal-RSSI fixtures where it fires most.
  The probe decision is now an explicit state machine (`Healthy` / `Recovered` /
  `Failed` / `Dead`) and the recovery edge logs at `info`. Same treatment for a
  notify stream that comes back after a subscribe failure warned about it.
- **Two lights sharing a name were indistinguishable in the log.** `add` fills
  the name from the model catalog, so adding two TL120Cs names **both** of them
  `TL120C`, and every per-light line (`connecting`, `session active`, `power
  on`, the starved-channel warning) carries only that name. Colliding labels now
  gain a MAC suffix — the short form where that separates them, the full MAC
  where it does not — and the `configuring light` startup line prints the label
  each fixture ended up with. Unique names are untouched.
- **`--seconds` did not bound `artnet-send`'s runtime.** The loop slept a whole
  packet period after its last send, so `--hz 0.001 --seconds 1` sent one packet
  and then sat for ~17 minutes. The wait is now clipped to what is left of the
  window (measured: 2 s instead of 1000 s; a normal 40 Hz stream is unchanged).
- **`inspect`, `ota` and `test` could return while still holding the light.**
  Each had hand-placed disconnects on some paths, but every `?` between the
  connect and them returned with the connection open, so the fixture did not
  re-advertise until the OS reaped the process. Each command now has a single
  release point that no path can skip.
- **`HARDWARE-BRINGUP.md` told the operator to look for log lines that no longer
  exist** (`BLE scan started`; `ArtNet lost — applying failsafe`, which is now
  per-universe). Corrected against the source, along with the liveness-probe
  step — a healthy probe is deliberately silent.

### Changed

- **`[logging] console = false` with no `file` is now refused.** It installed no
  log destination at all, so the binary wrote nothing whatsoever — measured: a
  failing command exited 1 with zero bytes of output, the error included. Set a
  `file`, or leave the console on.
- **`ota --settle-secs` must be 1..=3600.** A zero-second stability check is not
  one (see above).
- **`artnet-send --universe` must be 0..=32767**, the real Port-Address range.
- The bring-up byte-table test moved from `tests/` into the library. It was in a
  git-excluded directory, so it had never been in a clone and never ran in CI —
  the tracked, public doc it pins had no guard against drifting from the code.

## [1.5.0] — 2026-08-25

### Fixed

- **`artnet-send` could not reach an IPv6 or a broadcast target.** The sender
  socket was hard-bound to IPv4 and never enabled broadcast, so `--target
  localhost` on a dual-stack machine (where the name resolves to `::1` first)
  and `--target ::1` both died with a bare `os error 10047` /
  `EAFNOSUPPORT`, and `--target 255.255.255.255` with `os error 10013` /
  `EACCES` — broadcast being ordinary Art-Net addressing. The target is now
  resolved once, up front, IPv4 preferred (Art-Net is an IPv4 protocol and the
  bridge's own default bind is `0.0.0.0`); the socket is bound to the matching
  family; and broadcast is enabled for IPv4 destinations. A hostname is also no
  longer re-resolved for every packet of a stream.
- **`-v` could make a log sink *less* verbose.** The flag replaced the
  configured level outright, so a sink set to `file_level = "trace"` was
  silently demoted to `debug` the moment `-v` was passed — the flag documented
  as "increase verbosity" removed records. `-v`/`-vv` now raise the level and
  never lower it.
- **The failsafe never said when ArtNet came back.** It warned once on entering
  `blackout`/`poweroff` and then went quiet, so a log left running for weeks
  kept a "signal lost" warning as its last word on a universe that had long
  since recovered. It now logs a matching "ArtNet resumed … failsafe released"
  line, as every other latched warning in the bridge already did.
- **Two spellings of one bind address passed config validation and then failed
  at bind.** `::1` and `0:0:0:0:0:0:0:1` on the same port were compared as text,
  so the duplicate-input check missed them and the bridge died with a bare
  `EADDRINUSE` instead of naming the two inputs. Addresses are compared parsed
  now, matching what the wildcard check already did.
- A pixel frame built with an over-long palette through the public
  `pixel::palette` / `pixel::raw_frame` API would declare a wrapped length byte
  (83 colour blocks declared LEN 1 for a 257-byte payload). Over-long effect
  data is clamped, so the declared length always describes the real payload —
  the same guard `ota::block_frame` and the OTA header already carry. Not
  reachable through the bridge or any profile, which cap the palette at 8.

### Changed

- **`add --mac` now requires `--universe` and `--address` at argument parsing.**
  It reported the missing flag before, but only *after* creating `config.toml`
  from the shipped example — so a failed `add` left a config behind that turned
  the next run's actionable "no config file" hint into a confusing "no
  `[[lights]]` configured".
- **`test` rejects flag combinations it cannot honour** instead of silently
  ignoring one. `--status` short-circuits before everything else and `--set`
  before the probes, so `--status --set …` ran only the status read (having
  already validated and rejected a bad spec it would never send) and `--set …
  --colors` ran only the `--set`. `--status`, `--set`, and the visual probes
  (`--colors`/`--modes`/`--pixel`) are now mutually exclusive; the probes still
  combine freely with each other.
- The bridge no longer builds merge state for universes nothing is patched to.
  ArtNet is routinely broadcast, so `run` could allocate a lane set and run a
  full 512-channel merge for every foreign universe on the wire, up to its
  1024-universe cap, for results nothing would ever read. `monitor` still shows
  every universe — it exists to report what is actually arriving.

## [1.4.0] — 2026-08-25

### Changed

- **A key the config doesn't recognise is now a hard error.** A typo was
  silently dropped and the default used in its place, so `flush_hzz = 40` ran
  the bridge at 15 Hz and `adress = 26` left a light on channel 1, with nothing
  in the log to say why. Every section rejects unknown keys now, naming the bad
  one and listing the valid ones. A config carrying a stray key that used to
  load will need it removed or corrected.

## [1.3.0] — 2026-08-25

### Fixed

- **The ArtNet-loss failsafe could never fire on a shared lighting network.**
  Any valid ArtDmx reaching the port reset the idle clock, whatever universe it
  was for — so on a network where consoles and nodes broadcast ArtNet (the
  normal case), traffic for universes the bridge drives nothing on kept
  `blackout`/`poweroff` permanently at bay. Only ArtDmx for a universe that has
  a configured light now counts as signal.
- **The failsafe is now timed per universe.** It used to be one global clock, so
  a live source on one universe vouched for every other: lights on a universe
  whose source had died held their last state indefinitely. Each universe is
  timed on its own and only its own lights are acted on.
- **A light that was never discovered logged nothing at `info`.** A light that
  is powered off, out of range, or has a mistyped `mac` was completely silent
  after the startup line (only a light that connects *and fails* warned). The
  wait is now announced once and re-reported every 60 s while it continues.
- The `ota` header-resend abort message had ~30 stray spaces in the middle of
  it (a lost line continuation).
- `add --profile`'s help text omitted `rgb`, and `test --set`'s omitted
  `raw:<hex>` and `cctgm:`.
- **`artnet-send` crashed with a raw panic on some flag values.** `--seconds=-5`,
  `--seconds nan` and `--hz 1e-30` all reached `Duration::from_secs_f64`, which
  panics on a negative, NaN or overflowing value. Both flags are now range-checked
  up front (`--hz` 0.001-10000, `--seconds` greater than 0 up to 86400) and report
  a normal error. `--hz 0` and negative rates are errors too, where they previously
  fell through to a silent single-packet send.
- **`test --set` could send a different effect from the one it printed.** Only the
  upper bound of each id was checked, so `0` reached the builders' catch-all arms:
  `fx:0` went out as effect 1 (Lightning) and `pixfx:0` as ColorShift, both logged
  as "#0". Every id now carries a lower bound, `cctgm`'s green/magenta is rejected
  outside ±50 instead of being silently clamped, and `pixel:` hues outside 0-360
  are rejected instead of being wrapped modulo 360. A `pixel:` effect that is in
  range but doesn't render over direct BLE now says it is falling back to
  ColorReplacement rather than echoing the id it was given.
- **`add` rewrites the config atomically** (temp file, flush, rename) instead of
  truncating it in place. A crash or power loss mid-write could previously leave
  the live `config.toml` empty or half-written — the one file the
  `config.example.toml` split exists to protect. Existing file permissions are
  carried over.
- **`test --set warmdim` could not actually rescue a light stuck on an effect.**
  It sent the 2-byte CCT frame, which a fixture running a pixel effect ignores
  outright (hardware-proven on a TL120C: the tube kept scrolling through it, and
  through a pixel STOP frame too). The app's 4-byte CCT form exits the effect
  immediately, as does an HSI frame — so the bridge itself recovers a light stuck
  in pixel mode as soon as it next drives it; this only ever affected the probe
  commands. `warmdim`, the `--pixel`/`--modes` between-demo baseline, and
  `test --set`'s reset frame now all use the 4-byte form, so the documented
  escape hatch works in the one situation it exists for. `--set cct:` still emits
  the 2-byte form on purpose — that spec exists to probe it, and `cctgm:` probes
  the 4-byte one.
- **Retracted a false protocol claim about pixel mode.** The docs and probe code
  said a running pixel effect ignores a new effect/palette until a CCT frame
  clears a "latch". Hardware-disproven on a TL120C: from a running effect, both a
  palette change and an effect+speed change took effect immediately with zero CCT
  frames on the wire. The `pixel` DMX profile was always correct — it sends no CCT
  clear and follows live changes normally. No behaviour changed; the probe
  commands still paint a CCT-white baseline first, but for its real reason (a
  frame the fixture ignores then leaves the light white, so "ignored" is
  distinguishable from "worked").
- **A Bluetooth adapter that went away turned into a permanent warning flood.**
  While any light is disconnected the bridge polls the adapter's device list
  every 2 seconds, and every failure logged a warning — so an unplugged dongle
  or a Bluetooth stack that stopped answering produced roughly thirty lines a
  minute, per missing light, for as long as it lasted, churning the rotating log
  file. The first failure still warns immediately; repeats are capped at one a
  minute and carry the number suppressed, so the real rate stays visible.
- **The discovery-scan coordinator and the ArtNet-loss failsafe were
  unsupervised.** Both loop forever and both were spawned detached, so a panic
  in either was silent: no light would ever be discovered again, or the rig
  would never go safe, while the bridge carried on logging as though healthy.
  Both now bring the process down with a named error, exactly as a crashed light
  actor or ArtNet listener already did.
- **`test --driver` accepted any string.** An unrecognised name fell through to
  classic frames with no complaint, so `--driver classik` silently probed the
  wrong protocol family — and the "you used auto, so these were classic
  commands" hint at the end only fired for the exact string `auto`. Unknown
  names are now rejected before the adapter is even opened, listing the valid
  ones, as `add` and config validation already did.
- **`scan` could leave a scan running on the adapter.** If listing peripherals
  failed after the scan window, the error propagated without stopping the scan —
  the same bug already fixed in `find_by_mac`, and precisely the adapter load
  the duty-cycled scan coordinator exists to avoid.
- **Config validation missed some spellings of the wildcard bind address.**
  `::0` and `0:0:0:0:0:0:0:0` are the same address as `::`, but only three
  literal strings were compared, so sharing a port with one of them produced a
  bare "address already in use" at bind time instead of the explanatory error.
  Addresses are parsed now rather than string-matched.
- **The OTA block encoder could emit a wrapped length byte** for a block longer
  than the device's negotiated block size — 300 bytes declared 44, and a
  65535-byte "OTA_PRO" block declared 0. Not reachable from the flasher, which
  slices to the right size, but a malformed frame in the middle of a firmware
  write is the one thing this module must never produce; over-long data is now
  clamped, as the header's name field already was.
- **On Windows the startup log printed the config path in extended-length form**
  (`\\?\C:\...`) while every other mention of the same file — including the
  "exists but FAILED to load" warning on the next line — printed it plainly.
  That line is the first troubleshooting step in the README, so one run no
  longer shows one path two ways.
- The ArtNet sequence tracker's defensive key cap now holds *at* its limit
  rather than one over it, and only a genuinely new sender can trigger the
  reset. It used to clear the whole table on any packet once over the cap,
  discarding live sequence state for senders that were already tracked.

### Added

- The bridge warns when two ArtNet sources send the same universe to a single
  input. Merging works per input, not per sender, so they share a merge lane and
  the merge rules can no longer tell them apart — `ltp` overrides stop being
  sticky. The fix is a separate `[[artnet.inputs]]` entry per source, which the
  README and `config.example.toml` now say explicitly.

### Changed

- Config validation rejects three things it used to accept silently: two ArtNet
  inputs resolving to the same log label (which made `monitor`/`run` lines
  impossible to attribute), and `[logging] max_size_mb`/`max_files` of `0` when
  a log file is configured (they read as "unlimited" but were clamped to 1, so
  you silently got a 1 MB, 1-file log).
- `neewer-bridge lights` with no config file now reports how to create one,
  like `run` does, instead of a bare "No such file or directory".
- Commands that never read the config (`adapters`, `models`, `artnet-send`) no
  longer warn that it is missing — that is normal for them, and they are exactly
  what a new user runs before creating one. A config that exists but fails to
  load is still reported for every command, since that also means `[logging]`
  quietly fell back to defaults.
- Declared a minimum supported Rust version of **1.88** (`rust-version` in
  `Cargo.toml`), verified by building and testing the locked tree on it. The
  floor comes from a transitive dependency, not this crate's own code. An older
  toolchain now fails with "requires rustc 1.88" instead of a wall of compile
  errors.

### Performance

- **Finding a light by MAC no longer costs a D-Bus round trip per nearby
  device.** `find_scanned` (polled every 2 s per disconnected light) and
  `find_by_mac` asked *every* discovered peripheral for its properties just to
  compare addresses; on BlueZ that is a `get_device_info` call each, plus a
  string allocation. They now match on the address the adapter's own device
  listing already provided — free — and ask only the matching peripheral.
  Measured on the test rig (28 BLE devices in range, two lights searching):
  **313 → 12 `org.bluez.Device1.GetAll` calls per 30 s, a ~23× reduction**, on
  the same adapter the duty-cycled scan exists to keep unloaded. A peripheral
  whose properties can't be read is also no longer discarded outright: it is
  matched by MAC and connected to anyway.

### Internal

- The config file is read and parsed once per invocation instead of two or three
  times, and stat'ed once — so the config state the startup log announces is
  provably the one the command acts on.
- The OTA flashing command moved to its own module (`src/commands/ota.rs`); no
  behaviour change.
- The fatal-error path drops the logging guards before exiting, so a queued
  final `ERROR` cannot be lost with the non-blocking file writer.
- `Config::load_or_default` (unreachable after the single-parse change) was
  replaced by `Config::for_command`, which the binary actually calls — one
  definition of the missing-vs-broken rule instead of two, one of them dead.
- `pixel::rendered_effect` is now the single source of `paint`'s
  unsupported-effect fallback, so a caller can report what really goes on the
  wire without duplicating the dispatch table.
- Corrected comments that still described the always-on discovery scan removed
  in 1.1.x (`ble::start_scan`, `bridge`'s module doc), and the merge
  shared-source warning's stated granularity (it latches per input, not per
  input and universe).
- The `test` command and its probe machinery — `--colors`, `--modes`,
  `--pixel`, `--status` and the whole `--set` spec language — moved to
  `src/commands/probe.rs`, leaving `commands/mod.rs` to the commands that read
  or write config or shovel ArtNet. `test` itself is orchestration now, with
  each probe its own function; the encoder family it drives is an enum instead
  of a string re-matched at three call sites.
- `test --set` parses its spec before opening the adapter, so a typo fails in
  milliseconds instead of after a scan and a full connect. (`artnet-send`
  already validated its flags before binding a socket, for the same reason.)
- The OTA command builds its header with `Header::for_image` instead of a hand
  written struct literal: the image size and check-code — the two fields the
  device validates the whole transfer against — had two independent derivations,
  one of which was never called.
- `home::brightness` gained the byte-pinned test each of its siblings already
  had, and now records why the bridge itself never calls it.

## [1.2.1] — 2026-08-24

### Fixed

- **A single oversized UDP datagram could crash the whole bridge on Windows.**
  `recv_from` into the old 1024-byte buffer fails with WSAEMSGSIZE there (Linux
  silently truncates instead), and a listener error is deliberately fatal — so
  any packet over 1024 bytes sent to the ArtNet port took the bridge down. The
  receive buffer now covers the maximum UDP payload, and Windows' spurious
  connection-reset notifications on UDP sockets are ignored instead of fatal.
- **`power_on_connect = false` powered the light OFF at connect.** It now means
  what it says: the bridge sends nothing at all to that light — no power, no
  colour — until the first ArtNet data for it arrives. (The failsafe's poweroff
  still applies after reconnects once the light is being driven.)
- `add` could replace a config file it failed to read (permissions, disk error,
  non-UTF-8 bytes) with one containing only the new light; every read failure
  except "file does not exist" is now a hard error that leaves the file alone.
- A crashed per-light task now brings the bridge down with a clear error
  (restartable by a supervisor) instead of leaving that one light silently
  dead — stuck on its last colour, never reconnecting — while the bridge
  reported healthy.
- Status notifications with a valid-looking header but a corrupt body are now
  dropped: replies are checksum-verified whenever the complete frame is present
  (truncated notifications still decode, matching the official app's behaviour).
- The model catalog carried the CB200B Pro twice under two spellings; the
  duplicate is folded away (140 models) and a test now rejects any future
  case-insensitive duplicates.
- `test --set rgbcw:` accepted a brightness up to 255 and sent it as-is; it is
  now validated to the documented 0–100 like every other probe.
- A light name containing the DEL control character no longer makes `add` fail.
- `ota`: a resend request arriving before the first firmware block now re-sends
  the OTA header (bounded retries) instead of stalling into a timeout.

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
