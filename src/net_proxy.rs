// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A tiny loopback HTTPS CONNECT proxy that enforces a [`AllowSet`].
//!
//! This is the **unprivileged** half of allowlist mode (so it works in the
//! desktop GUI, no `CAP_NET_ADMIN`). A tab in allowlist mode gets
//! `HTTPS_PROXY=http://127.0.0.1:<port>` (and `ALL_PROXY`) injected; every
//! TLS connection the agent opens becomes a `CONNECT host:443` to us, and we
//! only splice it through when `AllowSet::host_allowed(host)` says yes. A
//! denied host gets `403`.
//!
//! Scope / threat model: this is a **cooperative** control — it relies on
//! the client honouring the proxy env vars. A process that ignores them and
//! dials out directly is not stopped here; that's what the privileged
//! headless nftables path is for. For the agent use-case (Claude Code,
//! curl, git over https — all proxy-aware) this is the right unprivileged
//! lever. Plain (non-TLS) HTTP is intentionally NOT forwarded: we fail
//! closed rather than implement a second, less-safe forwarding path.
//!
//! Implementation is blocking `std::net`, one thread per connection — proxy
//! traffic is one agent's handful of connections, so a thread pool would be
//! overkill. The accept loop owns a non-blocking listener and polls a
//! shutdown flag so [`ProxyHandle::drop`] tears it down promptly.

#[cfg(test)]
use std::io::Read;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::net_policy::AllowSet;

/// Handle to a running filtering proxy. Dropping it signals the accept loop
/// to stop and detaches; in-flight tunnels finish on their own.
pub struct ProxyHandle {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
}

impl ProxyHandle {
    /// The `127.0.0.1:<port>` the proxy is listening on. Feed this to the
    /// tab as `http://{addr}` in `HTTPS_PROXY` / `ALL_PROXY`.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://127.0.0.1:<port>` — ready to drop into a proxy env var.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop accepting new connections. Idempotent; also runs on drop.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind a filtering CONNECT proxy on a random loopback port.
///
/// Starts the accept loop on a background thread and returns the handle
/// (with the bound address) once the socket is listening, so the caller can
/// inject the URL before the tab's shell starts.
///
/// # Errors
/// Returns the underlying `io::Error` if binding the loopback socket,
/// reading its local address, or spawning the accept thread fails.
pub fn spawn(allow: AllowSet) -> std::io::Result<ProxyHandle> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let addr = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let allow = Arc::new(allow);
    let shutdown_loop = shutdown.clone();
    std::thread::Builder::new()
        .name("net-proxy".to_string())
        .spawn(move || accept_loop(&listener, &allow, &shutdown_loop))?;
    Ok(ProxyHandle { addr, shutdown })
}

fn accept_loop(listener: &TcpListener, allow: &Arc<AllowSet>, shutdown: &Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let allow = allow.clone();
                // Best-effort per-connection thread; if the OS can't spawn
                // one we just drop the connection.
                let _ = std::thread::Builder::new()
                    .name("net-proxy-conn".to_string())
                    .spawn(move || {
                        let _ = handle_connection(stream, &allow);
                    });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Nothing pending — nap so we don't busy-spin, then re-check
                // the shutdown flag.
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

/// Cap on the request header we'll buffer before giving up — a CONNECT line
/// plus a few headers is tiny; anything larger is junk or an attack.
const MAX_HEADER_BYTES: usize = 8 * 1024;

fn handle_connection(client: TcpStream, allow: &AllowSet) -> std::io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(client.try_clone()?);

    // Request line: `CONNECT host:port HTTP/1.1`.
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default().to_string();

    // Drain the remaining request headers (up to the blank line / cap) so
    // the client's write side is consumed before we reply.
    let mut consumed = request_line.len();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        consumed += n;
        if n == 0 || line == "\r\n" || line == "\n" || consumed > MAX_HEADER_BYTES {
            break;
        }
    }

    let mut client = client;
    if !method.eq_ignore_ascii_case("CONNECT") {
        // Only TLS tunnels are supported (see module docs). Anything else
        // fails closed.
        let _ = client.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n");
        return Ok(());
    }

    // `target` is host:port. host_allowed ignores the :port suffix and
    // handles literal-IP targets via the CIDR set.
    let host = target.rsplit_once(':').map_or(target.as_str(), |(h, _)| h);
    if host.is_empty() || !allow.host_allowed(&target) {
        let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n");
        return Ok(());
    }

    // Dial the real target. A connect failure is a 502 to the client.
    let Ok(upstream) = TcpStream::connect(&target) else {
        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n");
        return Ok(());
    };
    client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;

    // Hand off any bytes the BufReader already pulled past the headers
    // (TLS ClientHello can arrive in the same segment as the CONNECT) so
    // they aren't lost when we switch to raw splicing.
    let buffered = reader.buffer().to_vec();
    if !buffered.is_empty() {
        upstream.try_clone()?.write_all(&buffered)?;
    }

    splice(client, upstream);
    Ok(())
}

/// Bidirectionally copy between the two sockets until both directions hit
/// EOF, then shut everything down. One direction runs on a spawned thread,
/// the other inline.
fn splice(client: TcpStream, upstream: TcpStream) {
    // Clear the read timeout — a tunnelled connection can idle legitimately
    // (a long-poll, an open TLS session) and must not be torn down for it.
    let _ = client.set_read_timeout(None);
    let _ = upstream.set_read_timeout(None);

    let (Ok(mut c_rd), Ok(mut u_wr)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    let up_thread = std::thread::spawn(move || {
        let _ = std::io::copy(&mut c_rd, &mut u_wr);
        let _ = u_wr.shutdown(Shutdown::Write);
    });
    let mut u_rd = upstream;
    let mut c_wr = client;
    let _ = std::io::copy(&mut u_rd, &mut c_wr);
    let _ = c_wr.shutdown(Shutdown::Write);
    let _ = up_thread.join();
}

/// Read a complete HTTP response (status line + headers + any body up to
/// EOF) — only used by the tests to verify the proxy's replies.
#[cfg(test)]
fn read_http_response(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spin a one-shot echo server that, on the first connection, reads a
    /// line and writes it back. Returns its address.
    fn echo_server() -> SocketAddr {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut line = String::new();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let _ = r.read_line(&mut line);
                let _ = s.write_all(line.as_bytes());
            }
        });
        addr
    }

    fn connect_via(proxy: SocketAddr, target: &str) -> TcpStream {
        let mut s = TcpStream::connect(proxy).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
        s.write_all(req.as_bytes()).unwrap();
        s
    }

    #[test]
    fn allowed_host_tunnels_through() {
        let echo = echo_server();
        // Allow the loopback echo target by CIDR (literal-IP CONNECT).
        let allow = AllowSet::build(&[], &[], &["127.0.0.1/32".to_string()]);
        let proxy = spawn(allow).unwrap();
        let mut s = connect_via(proxy.addr(), &echo.to_string());

        // Read the proxy's "200 Connection established" line.
        let mut head = [0u8; 39];
        s.read_exact(&mut head).unwrap();
        assert!(
            String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"),
            "got: {}",
            String::from_utf8_lossy(&head)
        );

        // Now the tunnel is raw: send a line, expect it echoed.
        s.write_all(b"ping\n").unwrap();
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert_eq!(line, "ping\n");
    }

    #[test]
    fn denied_host_gets_403() {
        // Empty allow-set → everything denied.
        let proxy = spawn(AllowSet::default()).unwrap();
        let mut s = connect_via(proxy.addr(), "example.com:443");
        let resp = read_http_response(&mut s);
        assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp}");
    }

    #[test]
    fn non_connect_method_is_405() {
        let proxy = spawn(AllowSet::build(&[], &["example.com".to_string()], &[])).unwrap();
        let mut s = TcpStream::connect(proxy.addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .unwrap();
        let resp = read_http_response(&mut s);
        assert!(resp.starts_with("HTTP/1.1 405"), "got: {resp}");
    }

    #[test]
    fn url_is_loopback_http() {
        let proxy = spawn(AllowSet::default()).unwrap();
        assert!(proxy.url().starts_with("http://127.0.0.1:"));
    }
}
