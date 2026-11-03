//! systemd service notifications (`sd_notify`) for the packaged `Type=notify`
//! unit: READY when the bridge is up, a one-line STATUS that `systemctl status`
//! shows (how many lights are connected, or what it is waiting for), and
//! STOPPING on the way out.
//!
//! Everything here is best effort and a no-op unless systemd started us with a
//! notification socket (`$NOTIFY_SOCKET`) — a bridge run by hand, in a screen
//! session or on Windows never notices. The protocol is one datagram of
//! `KEY=value` lines to that socket; nothing in it is worth a dependency.

use std::sync::atomic::{AtomicBool, Ordering};

/// Tell systemd the service is up. Sent once; later calls do nothing, so every
/// path that reaches "running" (or "waiting for hardware", which is also up)
/// can call it without coordinating.
pub fn ready() {
    static SENT: AtomicBool = AtomicBool::new(false);
    if !SENT.swap(true, Ordering::SeqCst) {
        send("READY=1");
    }
}

/// Replace the status line `systemctl status` shows.
pub fn status(text: &str) {
    send(&format!("STATUS={}", one_line(text)));
}

/// Tell systemd the service is shutting down on purpose.
pub fn stopping() {
    send("STOPPING=1");
}

/// Whether systemd is listening for our notifications at all — lets a caller
/// skip work (a periodic status task) that only exists to feed them.
pub fn enabled() -> bool {
    std::env::var_os("NOTIFY_SOCKET").is_some_and(|s| !s.is_empty())
}

/// STATUS is a single `KEY=value` line: a newline inside it would start a new
/// assignment, so fold any into spaces.
fn one_line(text: &str) -> String {
    text.split(['\r', '\n']).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
}

#[cfg(target_os = "linux")]
fn send(state: &str) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET").filter(|p| !p.is_empty()) else {
        return;
    };
    if let Err(e) = send_to(&path, state) {
        tracing::debug!(error = %e, state, "could not notify systemd");
    }
}

/// One notification datagram to the socket at `path` (`$NOTIFY_SOCKET`).
#[cfg(target_os = "linux")]
fn send_to(path: &std::ffi::OsStr, state: &str) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let sock = UnixDatagram::unbound()?;
    // `@name` is a socket in the abstract namespace (no file on disk).
    let addr = match path.as_bytes().strip_prefix(b"@") {
        Some(name) => SocketAddr::from_abstract_name(name)?,
        None => SocketAddr::from_pathname(path)?,
    };
    sock.send_to_addr(state.as_bytes(), &addr)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn send(_state: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_text_is_kept_to_one_line() {
        assert_eq!(one_line("running — 4 of 5 lights connected"), "running — 4 of 5 lights connected");
        assert_eq!(one_line("bridge failed:\nport in use\r\n"), "bridge failed: port in use");
        assert_eq!(one_line(""), "");
    }

    /// The real protocol: a datagram socket standing in for systemd's, both
    /// address forms systemd uses, and the exact bytes that arrive. (Through
    /// `send_to`, not `$NOTIFY_SOCKET` — the environment is process-wide and
    /// other tests run in parallel.)
    #[cfg(target_os = "linux")]
    #[test]
    fn notifications_reach_the_socket_systemd_names() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::{SocketAddr, UnixDatagram};
        let mut buf = [0u8; 256];

        let dir = std::env::temp_dir().join(format!("nb-sdnotify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notify.sock");
        let _ = std::fs::remove_file(&path);
        let on_disk = UnixDatagram::bind(&path).unwrap();
        send_to(path.as_os_str(), "READY=1").unwrap();
        let n = on_disk.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
        let _ = std::fs::remove_dir_all(&dir);

        let name = format!("nb-sdnotify-test-{}", std::process::id());
        let abstract_sock =
            UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
        send_to(format!("@{name}").as_ref(), "STOPPING=1").unwrap();
        let n = abstract_sock.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"STOPPING=1");
    }
}
