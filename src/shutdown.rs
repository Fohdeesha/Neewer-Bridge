//! Stop requests: Ctrl-C everywhere, and SIGTERM on Unix (what `systemctl stop`
//! and a system shutdown send).
//!
//! The request is a **latch**, not an event. The bridge waits for a stop in
//! several places in turn — serving, backing off before a restart, waiting for
//! an adapter — and each of those used to create its own `ctrl_c()` future. A
//! signal that landed in the gap between two of them was swallowed: once tokio
//! has installed its handler the default "terminate" disposition is gone for
//! the life of the process, and a signal nobody is listening for at that moment
//! is simply dropped. Under systemd that meant a `stop` that did nothing until
//! `TimeoutStopSec` ran out and the process was killed.
//!
//! [`listen`] starts ONE task that owns the signal streams for the rest of the
//! process and records the first request; [`requested`] resolves at once if a
//! stop has already been asked for, so no request can fall between two waits.
//! A second request while the first is being handled exits immediately — the
//! operator's way out if a shutdown ever hangs.

use std::sync::{Once, OnceLock};

use tokio::sync::watch;
use tracing::{error, warn};

/// Exit status for a forced stop (a second Ctrl-C / SIGTERM): 128 + SIGINT, the
/// shell convention for "interrupted".
const FORCED_EXIT: i32 = 130;

fn latch() -> &'static watch::Sender<Option<&'static str>> {
    static LATCH: OnceLock<watch::Sender<Option<&'static str>>> = OnceLock::new();
    LATCH.get_or_init(|| watch::channel(None).0)
}

/// Start listening for stop requests. Idempotent; must be called from within a
/// tokio runtime. Commands that don't call it keep the default signal
/// behaviour (Ctrl-C ends the process at once), which is right for one-shot
/// tools like `scan`.
pub fn listen() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        tokio::spawn(async {
            let mut signals = Signals::install();
            let first = signals.next().await;
            latch().send_replace(Some(first));
            let second = signals.next().await;
            error!("{second} received again while shutting down — exiting immediately");
            std::process::exit(FORCED_EXIT);
        });
    });
}

/// Wait until a stop has been requested, returning what asked for it
/// (`"Ctrl-C"` or `"SIGTERM"`). Resolves immediately if one already has. Never
/// resolves if [`listen`] was not called.
pub async fn requested() -> &'static str {
    let mut rx = latch().subscribe();
    let signal = match rx.wait_for(Option::is_some).await {
        Ok(seen) => *seen,
        // The sender is a static and is never dropped.
        Err(_) => std::future::pending().await,
    };
    signal.expect("wait_for only returns a Some")
}

/// The signal streams, installed once and kept for the life of the process.
struct Signals {
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn install() -> Self {
        Self {
            #[cfg(unix)]
            terminate: match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!(error = %e, "could not listen for SIGTERM — only Ctrl-C will stop the bridge cleanly");
                    None
                }
            },
        }
    }

    /// The next stop request.
    async fn next(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            let terminate = async {
                match self.terminate.as_mut() {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = ctrl_c() => "Ctrl-C",
                _ = terminate => "SIGTERM",
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c().await;
            "Ctrl-C"
        }
    }
}

/// Ctrl-C, or never if its handler could not be installed (a failure that
/// would otherwise look exactly like a stop request).
async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        warn!(error = %e, "could not listen for Ctrl-C");
        std::future::pending::<()>().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // One test only: the latch is process-global, so a second test setting it
    // would race this one.
    #[tokio::test]
    async fn a_request_made_before_anyone_waits_is_not_lost() {
        assert!(
            tokio::time::timeout(Duration::from_millis(50), requested()).await.is_err(),
            "nothing has asked for a stop yet"
        );
        latch().send_replace(Some("SIGTERM"));
        // Two waits started AFTER the request — the gap a per-wait signal
        // future used to lose — both see it.
        assert_eq!(requested().await, "SIGTERM");
        assert_eq!(requested().await, "SIGTERM");
    }
}
