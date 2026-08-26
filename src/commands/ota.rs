//! `neewer-bridge ota` — flash firmware over the custom `0x78` block protocol.
//!
//! Split out of [`super`] because it is a self-contained state machine rather
//! than CLI glue: probe the device's block type (`0xD0`), send the `0x96`
//! header, then stream `0x97`/`0xCF` blocks driven entirely by the device's
//! `0x06` ACKs. The wire encoders it drives live in [`crate::protocol::ota`];
//! everything here is orchestration, safety checks and progress reporting.
//!
//! Safety: `--check` never writes a firmware byte, the real flash needs
//! `--confirm`, and the device validates an additive check-code before it
//! commits — so a dropped block fails cleanly with the old firmware intact.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use btleplug::api::Characteristic;
use btleplug::platform::Peripheral;
use tracing::{info, warn};

use crate::ble;
use crate::config::parse_mac;
use crate::protocol::queries;

/// Parse a "MAJOR.MINOR.PATCH" version string into 3 bytes for the OTA header.
pub fn parse_version_triplet(s: &str) -> Result<[u8; 3]> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 3 {
        bail!("version must be MAJOR.MINOR.PATCH (e.g. 3.0.5), got {s:?}");
    }
    let mut v = [0u8; 3];
    for (i, p) in parts.iter().enumerate() {
        v[i] = p
            .parse::<u8>()
            .map_err(|_| anyhow::anyhow!("version component {p:?} is not a 0-255 number"))?;
    }
    Ok(v)
}

/// Derive the firmware version from a Neewer OTA filename, which embeds it as
/// `V<maj>.<min>.<patch>` (e.g. `TL60-3_V3.0.5_20250908.bin`). Case-insensitive.
/// Returns `None` if no such marker parses — the caller then requires an explicit
/// `--version` (better than silently stamping a wrong default into the header).
pub fn version_from_filename(name: &str) -> Option<[u8; 3]> {
    let bytes = name.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c != b'V' && c != b'v' {
            continue;
        }
        // Candidate: digits '.' digits '.' digits immediately after the V. A
        // trailing dot is trimmed so `…V1.2.3.bin` doesn't capture the extension
        // separator as a fourth (empty) component.
        let rest = &name[i + 1..];
        let end = rest
            .char_indices()
            .find(|(_, ch)| !ch.is_ascii_digit() && *ch != '.')
            .map(|(j, _)| j)
            .unwrap_or(rest.len());
        if let Ok(v) = parse_version_triplet(rest[..end].trim_end_matches('.')) {
            return Some(v);
        }
    }
    None
}

/// Read the next inbound notify frame, or `None` on timeout.
async fn ota_next_frame(stream: &mut ble::NotifyStream, timeout: Duration) -> Option<Vec<u8>> {
    use futures::StreamExt;
    match tokio::time::timeout(timeout, stream.next()).await {
        Ok(Some(n)) => Some(n.value),
        _ => None,
    }
}

/// Hold the live connection for `settle` and confirm it stays up, issuing a cheap
/// GATT read each second. Returns an error if the link drops — the go/no-go gate
/// before committing to a multi-minute firmware transfer over a marginal link.
///
/// The connectivity check runs **at least once**, whatever `settle` is. As a
/// plain `while start.elapsed() < settle` loop, a zero window ran the body zero
/// times — `is_connected` was never called — and the function still announced
/// "link held steady — OK to proceed". That is the one safety gate standing
/// between a marginal link and a multi-minute firmware write, and it was
/// reporting a check it had not performed. `--settle-secs` is separately capped
/// below 1 at the CLI, so this is defence in depth for any other caller.
///
/// The elapsed time is now read from the clock rather than counted in ticks, so
/// the number in the log is what actually happened.
async fn ota_link_precheck(
    p: &Peripheral,
    write: &Characteristic,
    mac: [u8; 6],
    settle: Duration,
) -> Result<()> {
    let start = Instant::now();
    let mut checks = 0u32;
    loop {
        if !ble::is_connected(p).await {
            bail!(
                "link dropped after {:.1}s of the {:.0}s stability check — move the light \
                 (or the adapter) closer and retry; NOT flashing over a flaky link",
                start.elapsed().as_secs_f32(),
                settle.as_secs_f32()
            );
        }
        // A non-mutating version read as a keepalive / liveness poke.
        ble::write_command(p, write, &queries::version(mac)).await.ok();
        checks += 1;
        if start.elapsed() >= settle {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    info!(
        held_secs = start.elapsed().as_secs(),
        checks,
        "link held steady — OK to proceed"
    );
    Ok(())
}

/// Flash a firmware image to a light over the custom `0x78` OTA block protocol.
///
/// Safe by construction: `--check` only probes (never writes firmware), the real
/// flash requires `--confirm`, and a link-stability precheck aborts before any
/// firmware byte is sent if the connection will not hold. The device drives the
/// whole transfer via `0x06` ACKs and validates an additive check-code before
/// committing, so a dropped/garbled block fails cleanly (retryable), it does not
/// brick — the device stays in its bootloader and can be re-flashed.
///
/// Stop the main bridge first — it will fight this tool for the same adapter/MAC.
#[allow(clippy::too_many_arguments)]
pub async fn ota(
    adapter_selector: &str,
    mac: &str,
    file: &Path,
    version: [u8; 3],
    name: &str,
    confirm: bool,
    check_only: bool,
    settle_secs: u64,
    seconds: u64,
    chunk_delay_ms: u64,
) -> Result<()> {
    use crate::protocol::{ota, replies};
    let mac_bytes = parse_mac(mac)?;
    let chunk_delay = Duration::from_millis(chunk_delay_ms);

    // 1. Load + sanity-check the image (before touching BLE).
    let image = std::fs::read(file)
        .with_context(|| format!("reading firmware image {}", file.display()))?;
    if image.len() < 1024 {
        bail!("firmware image is only {} bytes — that is not a real image", image.len());
    }
    // Build the header here, from the image, as the SINGLE source of truth for
    // the two fields the device validates the whole flash against (`size` and
    // the additive `check_code`). These used to be re-derived by hand into a
    // struct literal further down while `Header::for_image` — which exists to do
    // exactly this — went unused, so the numbers logged below and the numbers on
    // the wire had two independent derivations.
    let header = ota::Header::for_image(&image, version, name);
    let size = header.size;
    let cc = header.check_code;
    // ARM Cortex-M images start with the initial stack pointer (a RAM address,
    // 0x2000_xxxx). If this word does not look like that, the file is probably a
    // truncated download or an HTML error page — refuse.
    let sp = u32::from_le_bytes([image[0], image[1], image[2], image[3]]);
    let looks_arm = (0x2000_0000..0x2008_0000).contains(&sp);
    info!(
        image = %file.display(),
        bytes = size,
        check_code = %format!("0x{cc:08X}"),
        initial_sp = %format!("0x{sp:08X}"),
        looks_arm,
        version = %format!("{}.{}.{}", version[0], version[1], version[2]),
        "firmware image loaded"
    );
    if !looks_arm {
        bail!(
            "image does not begin with an ARM Cortex-M stack pointer (got 0x{sp:08X}, \
             expected 0x2000_xxxx) — this does not look like a Neewer LED-MCU .bin; refusing"
        );
    }

    // 2. Connect + verify. OTA needs the notify characteristic for the ACK stream.
    let adapter = ble::acquire_adapter(adapter_selector).await?;
    let peripheral = ble::find_by_mac(&adapter, mac, Duration::from_secs(seconds)).await?;
    let adv_name = ble::peripheral_name(&peripheral).await;
    let rssi = ble::rssi(&peripheral).await;
    info!(ble_name = %adv_name, ?rssi, "found; connecting");
    let chars = ble::connect_and_verify(&peripheral).await?;
    let write = chars.write.clone();
    // Everything from here holds the light, so it runs inside one block with a
    // SINGLE exit that always releases it. `--check` and the missing-`--confirm`
    // refusal each disconnected by hand, but every `?` between the connect and
    // them — a light with no notify characteristic, a failed subscribe, the link
    // precheck failing, the type-probe write failing — returned with the
    // connection still open, so the fixture did not re-advertise until the OS
    // reaped the process. `Ok(true)` = flashed, so the post-flash read-back below
    // should run; `Ok(false)` = a clean stop without flashing (`--check`).
    let res: Result<bool> = async {
        // Which write mode we get matters a lot on a marginal link: write-WITH-response
        // ATT-acks every fragment (dropped chunks surface as errors we can act on),
        // write-WITHOUT-response is fire-and-forget (drops are silent → device resends).
        info!(
            props = ?write.properties,
            with_response = write.properties.contains(btleplug::api::CharPropFlags::WRITE),
            chunk_delay_ms,
            "write characteristic"
        );
        let notify = chars
            .notify
            .clone()
            .context("OTA needs the notify characteristic (69400003-…); not found on this light")?;
        let mut stream = ble::subscribe_notify(&peripheral, &notify).await?;

        // 3. Read current firmware version (best-effort, for the record).
        ble::write_command(&peripheral, &write, &queries::version(mac_bytes)).await.ok();
        if let Some(frame) = ota_next_frame(&mut stream, Duration::from_secs(2)).await {
            if let Some(reply) = replies::parse(&frame) {
                info!(current = %reply.summary(), "device reports");
            }
        }

        // 4. Link-stability precheck — the go/no-go gate for a marginal link.
        info!(secs = settle_secs, "checking link stability before flashing…");
        ota_link_precheck(&peripheral, &write, mac_bytes, Duration::from_secs(settle_secs)).await?;

        // The precheck's keepalive version reads elicit notify replies nothing has
        // consumed — up to ~settle_secs of them sit buffered in the stream. Drain
        // them now so the type probe below reads fresh frames; otherwise the stale
        // replies can exhaust its window and the 0x1A reply is never seen (the
        // block kind would silently default — wrong for an OTA_PRO fixture).
        let mut drained = 0u32;
        while ota_next_frame(&mut stream, Duration::from_millis(250)).await.is_some() {
            drained += 1;
        }
        if drained > 0 {
            info!(frames = drained, "drained stale notify frames from the precheck");
        }

        // 5. Probe the OTA block type (0xD0 → 0x1A). Non-type frames (status pushes)
        // don't consume the budget — only time does. Default to 128-byte blocks if
        // the device never answers (both our fixtures are Std128; an OTA_PRO device
        // would reject the header cleanly).
        ble::write_command(&peripheral, &write, &ota::probe_frame(ota::HEADER_STD)).await?;
        let mut kind = None;
        let probe_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < probe_deadline {
            match ota_next_frame(&mut stream, Duration::from_millis(600)).await {
                Some(f) => {
                    if let Some(k) = ota::parse_type_reply(&f) {
                        kind = Some(k);
                        break;
                    }
                }
                None => break, // 600 ms of post-drain silence: no reply is coming
            }
        }
        let kind = kind.unwrap_or_else(|| {
            warn!("no 0x1A OTA-type reply from the device — defaulting to 128-byte blocks");
            ota::BlockKind::Std128
        });
        let total_blocks = ota::block_count(image.len(), kind);
        info!(ota_type = ?kind, block_size = kind.block_size(), total_blocks, "OTA type resolved");

        if check_only {
            info!("--check only: NOT flashing. Link + OTA type verified above.");
            return Ok(false);
        }
        if !confirm {
            bail!(
                "refusing to flash without --confirm (this rewrites the LED-MCU firmware). \
                 The link held and the device is ready — re-run with --confirm to proceed."
            );
        }

        // 6. Run the header + block transfer. The single release point below
        //    covers every outcome — a failed flash must not leave a half-open
        //    link that keeps the radio from re-advertising.
        ota_transfer(
            &peripheral, &write, &mut stream, &image, kind, chunk_delay, total_blocks, &header,
        )
        .await?;
        Ok(true)
    }
    .await;
    // The one release point for the whole connected phase.
    let _ = ble::disconnect(&peripheral).await;
    if !res? {
        return Ok(()); // --check: probed only, nothing flashed.
    }

    // 7. Let the device reboot, then reconnect and read back the version. This is
    // entirely best-effort reporting: the flash already committed (the device sent
    // Done), so nothing here may fail the command — a light that boots slowly or
    // won't take a notify subscription right away is still a successful flash.
    info!("waiting 12s for the light to reboot on the new firmware…");
    tokio::time::sleep(Duration::from_secs(12)).await;
    match ble::find_by_mac(&adapter, mac, Duration::from_secs(20)).await {
        Ok(p2) => match ble::connect_and_verify(&p2).await {
            Ok(c2) => {
                if let Some(n2) = &c2.notify {
                    match ble::subscribe_notify(&p2, n2).await {
                        Ok(mut s2) => {
                            ble::write_command(&p2, &c2.write, &queries::version(mac_bytes))
                                .await
                                .ok();
                            if let Some(f) = ota_next_frame(&mut s2, Duration::from_secs(3)).await
                            {
                                if let Some(reply) = replies::parse(&f) {
                                    info!(post_flash = %reply.summary(), "reconnected — device reports");
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "post-flash notify subscribe failed (flash itself succeeded)")
                        }
                    }
                }
                let _ = ble::disconnect(&p2).await;
            }
            Err(e) => warn!(error = %e, "reconnected scan found the light but verify failed"),
        },
        Err(e) => warn!(error = %e, "could not find the light after reboot (it may still be booting)"),
    }
    info!("OTA finished.");
    Ok(())
}

/// The header + ACK-driven block transfer. Split out from [`ota`] so the caller
/// can disconnect the link cleanly whatever the outcome (a mid-flash abort must
/// not leave a half-open connection that stops the radio re-advertising).
///
/// Returns `Ok(())` only when the device sends the `Done` ACK (image committed).
#[allow(clippy::too_many_arguments)]
async fn ota_transfer(
    peripheral: &Peripheral,
    write: &Characteristic,
    stream: &mut ble::NotifyStream,
    image: &[u8],
    kind: crate::protocol::ota::BlockKind,
    chunk_delay: Duration,
    total_blocks: usize,
    header: &crate::protocol::ota::Header,
) -> Result<()> {
    use crate::protocol::{ota, replies};

    // The header already carries the image size the device was told to expect;
    // taking it as a separate argument too would let the progress figures and the
    // committed size disagree.
    let size = header.size;
    let header_frame = header.frame();
    info!(frame = %ble::hexstr(&header_frame), "sending OTA header (0x96)");

    let mut idx: i64 = -1; // block index; the first `Next` advances it to 0
    let mut started = false; // have we seen the first ACK yet?
    let mut header_attempts = 0u32;
    let mut resends = 0u64; // diagnostic: total resend requests over the transfer
    let mut last_pct = -1i32;
    let flash_start = Instant::now();

    ble::write_ota_frame(peripheral, write, &header_frame, chunk_delay).await?;

    loop {
        let Some(frame) = ota_next_frame(stream, Duration::from_secs(12)).await else {
            // Timeout.
            if !started {
                header_attempts += 1;
                if header_attempts >= 5 {
                    bail!("device never acknowledged the OTA header after 5 attempts — aborting");
                }
                warn!(attempt = header_attempts, "no ACK for header; resending");
                ble::write_ota_frame(peripheral, write, &header_frame, chunk_delay).await?;
                continue;
            }
            bail!(
                "device went silent mid-transfer at block {}/{} after {} resends — aborting \
                 (retry the flash; nothing was committed, the old firmware is intact)",
                idx.max(0),
                total_blocks,
                resends
            );
        };

        let Some(ack) = ota::parse_ack(&frame) else {
            // Not an ACK (a stray status reply); log and keep waiting.
            if let Some(reply) = replies::parse(&frame) {
                info!(notify = %reply.summary(), "non-ACK notify during OTA (ignored)");
            }
            continue;
        };
        started = true;

        match ack {
            ota::Ack::Next => {
                idx += 1;
                match ota::block_at(image, kind, idx as usize) {
                    Some(block) => {
                        let frame = ota::block_frame(kind, block, ota::HEADER_STD);
                        ble::write_ota_frame(peripheral, write, &frame, chunk_delay).await?;
                        let sent = (idx as usize) * kind.block_size() + block.len();
                        let pct = (sent as u64 * 100 / size as u64) as i32;
                        if pct >= last_pct + 2 || idx == 0 {
                            last_pct = pct;
                            info!(pct, block = idx, total = total_blocks, resends, "flashing");
                        }
                    }
                    None => {
                        // All blocks sent; device should send Done next.
                        info!("all blocks sent; awaiting device commit (Done)…");
                    }
                }
            }
            ota::Ack::Resend => {
                resends += 1;
                if idx >= 0 {
                    if let Some(block) = ota::block_at(image, kind, idx as usize) {
                        warn!(block = idx, resends, "device requested resend");
                        let frame = ota::block_frame(kind, block, ota::HEADER_STD);
                        ble::write_ota_frame(peripheral, write, &frame, chunk_delay).await?;
                    }
                } else {
                    // Resend before any block: the only thing sent so far is the
                    // header, so resend that. The app itself would do NOTHING
                    // here (its sendData() no-ops while its block counter is
                    // still -1, verified in the decompile), so a real device is
                    // not expected to ask - but stalling into the silence
                    // timeout with a misleading "mid-transfer" abort is strictly
                    // worse than a retry: re-sending a 0x96 header commits
                    // nothing (the device validates the check-code before Done),
                    // and op=2 Restart shows the protocol tolerates
                    // re-initialisation. Budgeted with the same counter as the
                    // header-timeout path so a rejecting device still aborts.
                    header_attempts += 1;
                    if header_attempts >= 5 {
                        bail!(
                            "device keeps requesting a resend of the OTA header \
                             ({header_attempts} attempts) — aborting (nothing was committed)"
                        );
                    }
                    warn!(
                        attempt = header_attempts,
                        "device requested a resend before any block — resending the header"
                    );
                    ble::write_ota_frame(peripheral, write, &header_frame, chunk_delay).await?;
                }
            }
            ota::Ack::Restart => {
                warn!("device requested restart from block 0");
                idx = 0;
                if let Some(block) = ota::block_at(image, kind, 0) {
                    let frame = ota::block_frame(kind, block, ota::HEADER_STD);
                    ble::write_ota_frame(peripheral, write, &frame, chunk_delay).await?;
                    last_pct = -1;
                }
            }
            ota::Ack::Done => {
                info!(
                    elapsed_secs = %format!("{:.1}", flash_start.elapsed().as_secs_f32()),
                    resends,
                    "OTA complete — device accepted the image and is committing/rebooting"
                );
                return Ok(());
            }
            ota::Ack::Fail => {
                bail!(
                    "device reported OTA FAILURE (op=4) at block {}/{} — the image was rejected \
                     (check-code mismatch or transfer error). Nothing was committed; retry.",
                    idx.max(0),
                    total_blocks
                );
            }
            ota::Ack::Unknown(op) => {
                warn!(op, "unknown OTA ACK op; ignoring");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_triplet_parses_and_rejects() {
        assert_eq!(parse_version_triplet("3.0.5").unwrap(), [3, 0, 5]);
        assert_eq!(parse_version_triplet("10.20.30").unwrap(), [10, 20, 30]);
        assert!(parse_version_triplet("3.0").is_err());
        assert!(parse_version_triplet("3.0.5.1").is_err());
        assert!(parse_version_triplet("3.0.x").is_err());
        assert!(parse_version_triplet("3.0.999").is_err()); // >255
    }

    #[test]
    fn version_from_real_neewer_filenames() {
        // The actual OTA-server naming scheme.
        assert_eq!(version_from_filename("TL60-3_V3.0.5_20250908.bin"), Some([3, 0, 5]));
        assert_eq!(version_from_filename("TL120-2_V2.0.5_20250905.bin"), Some([2, 0, 5]));
        // Case-insensitive marker.
        assert_eq!(version_from_filename("tube_v1.2.3.bin"), Some([1, 2, 3]));
        // A lone V that isn't a version marker doesn't fool it (later marker wins).
        assert_eq!(version_from_filename("Verbatim_V4.5.6.bin"), Some([4, 5, 6]));
        // No marker → None (caller must require --version).
        assert_eq!(version_from_filename("firmware.bin"), None);
        assert_eq!(version_from_filename("TL60_20250908.bin"), None);
        assert_eq!(version_from_filename("V3.0.bin"), None); // not a triplet
    }
}
