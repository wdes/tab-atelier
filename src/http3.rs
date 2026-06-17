// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Experimental HTTP/3 + WebTransport transport (behind `http3`).
//!
//! **Phase 1: proof of life** — the QUIC stack is absent unless the
//! feature is on.
//!
//! Why WebTransport and not "WebSocket over HTTP/3": browsers don't
//! ship WS-over-h3 (RFC 9220), so the HTTP/3 door for a browser is the
//! WebTransport API. Its payoff over our TCP WebSocket is only on lossy
//! / mobile links (no TCP head-of-line blocking; connection migration
//! across Wi-Fi↔cellular) — so this is a fallback-guarded *enhancement*,
//! never a replacement (and never carries keystrokes in QUIC 0-RTT,
//! which is replay-unsafe).
//!
//! Self-hosted cert story: instead of a CA-issued cert, the browser
//! accepts our short-lived self-signed cert by pinning its SHA-256 via
//! `new WebTransport(url, { serverCertificateHashes: [{algorithm:
//! "sha-256", value: <hash>}] })`. [`cert_hash_hex`] produces that hash
//! (the cert must be ECDSA with ≤ 14 days validity per the spec — which
//! is what wtransport's `self_signed` emits).
//!
//! Phase 2 will map the existing `TAG_IN` / `TAG_OUT` framing onto a
//! WebTransport bidirectional stream and reuse the `PtyRing` pump;
//! Phase 3 adds the client path + WS fallback. This module is just the
//! listener + cert plumbing that the rest builds on.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use wtransport::{Endpoint, Identity, ServerConfig};
use wtransport::{RecvStream, SendStream};

use crate::pty_ring::PtyRing;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

// Frame tags mirror the WebSocket transport (src/api_ws.rs) so the
// client uses one wire vocabulary across both. WebSocket gives message
// boundaries for free; a WebTransport *stream* is a raw byte stream, so
// every frame here is length-delimited: a 4-byte big-endian length L,
// then L bytes = tag(1) + payload.
/// Inbound tag — consumed by the Phase 3 handler that maps `TAG_IN`
/// onto `pending_input` (kept here so both transports share the table).
#[allow(dead_code)]
const TAG_IN: u8 = 0x01;
const TAG_OUT: u8 = 0x02;
/// Cap on a single inbound framed message (matches the WS 2 MiB limit)
/// so a malformed length can't make us allocate unbounded.
const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
/// Coalesce a burst of PTY output into one frame (same rationale as the
/// WS pump's `OUTPUT_DEBOUNCE_MS`).
const OUTPUT_DEBOUNCE: Duration = Duration::from_millis(2);

/// Length-delimit a `tag`+`payload` frame for a WebTransport stream.
#[must_use]
pub fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let body_len = 1 + payload.len();
    let mut out = Vec::with_capacity(4 + body_len);
    out.extend_from_slice(&(body_len as u32).to_be_bytes());
    out.push(tag);
    out.extend_from_slice(payload);
    out
}

/// Read exactly `buf.len()` bytes from `recv`. `Ok(false)` on a clean
/// EOF before the buffer is filled (peer finished the stream).
async fn read_exact(recv: &mut RecvStream, buf: &mut [u8]) -> Result<bool, BoxErr> {
    let mut filled = 0;
    while filled < buf.len() {
        match recv.read(&mut buf[filled..]).await? {
            Some(0) | None => return Ok(false),
            Some(n) => filled += n,
        }
    }
    Ok(true)
}

/// Read one length-delimited frame → `(tag, payload)`. `Ok(None)` at
/// end of stream.
///
/// # Errors
/// On a read error or a length over [`MAX_FRAME_BYTES`].
pub async fn read_frame(recv: &mut RecvStream) -> Result<Option<(u8, Vec<u8>)>, BoxErr> {
    let mut len_be = [0u8; 4];
    if !read_exact(recv, &mut len_be).await? {
        return Ok(None);
    }
    let len = u32::from_be_bytes(len_be) as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(format!("bad frame length {len}").into());
    }
    let mut body = vec![0u8; len];
    if !read_exact(recv, &mut body).await? {
        return Ok(None);
    }
    let tag = body[0];
    Ok(Some((tag, body.split_off(1))))
}

/// Stream new `PtyRing` bytes as length-delimited `TAG_OUT` frames over
/// a WebTransport send stream, starting at byte offset `since`.
///
/// Wakes on the ring's `Notify` (event-driven, like the WS pump) so a
/// shell echo flushes within microseconds, with a tiny debounce to
/// coalesce floods. Runs until the stream errors.
///
/// # Errors
/// On a stream write error or a poisoned ring lock.
// The chunk-copy guard is held across `total_len` + `since` for a
// consistent read; `significant_drop_tightening` can't model that
// dependency (same as the WS pump in api_ws.rs).
#[allow(clippy::significant_drop_tightening)]
pub async fn pump_output(send: &mut SendStream, ring: &Arc<Mutex<PtyRing>>, since: u64) -> Result<(), BoxErr> {
    let notify = {
        let r = ring.lock().map_err(|_| "ring lock poisoned")?;
        r.notifier()
    };
    let mut offset = since;
    loop {
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let chunk = {
            let r = ring.lock().map_err(|_| "ring lock poisoned")?;
            let total = r.total_len();
            if total == offset {
                Vec::new()
            } else {
                let bytes = r.since(offset);
                offset = total;
                bytes
            }
        };
        if !chunk.is_empty() {
            send.write_all(&frame(TAG_OUT, &chunk)).await?;
        }

        notified.await;
        tokio::time::sleep(OUTPUT_DEBOUNCE).await;
    }
}

/// Generate a short-lived self-signed identity covering `sans` (e.g.
/// `["localhost", "127.0.0.1", "lan.example.com"]`). The browser trusts
/// it via [`cert_hash_hex`], so no CA is involved.
///
/// # Errors
/// Propagates wtransport's error if a SAN is invalid.
pub fn self_signed(sans: &[&str]) -> Result<Identity, BoxErr> {
    Ok(Identity::self_signed(sans.iter().copied())?)
}

/// Lowercase-hex SHA-256 of the leaf certificate's DER — the exact
/// value a browser passes in `serverCertificateHashes`. Empty string
/// only if the chain is somehow empty.
#[must_use]
pub fn cert_hash_hex(identity: &Identity) -> String {
    use std::fmt::Write as _;
    let chain = identity.certificate_chain();
    let Some(cert) = chain.as_slice().first() else {
        return String::new();
    };
    let digest = Sha256::digest(cert.der());
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Minimal WebTransport echo server — Phase-1 proof of life.
///
/// Binds the UDP socket, then for each session echoes every
/// bidirectional stream back to the client. Real `TAG_*` framing
/// arrives in Phase 2.
///
/// # Errors
/// Returns if the endpoint can't bind (e.g. port in use).
pub async fn run_echo(addr: SocketAddr, identity: Identity) -> Result<(), BoxErr> {
    let config = ServerConfig::builder()
        .with_bind_address(addr)
        .with_identity(identity)
        .build();
    let server = Endpoint::server(config)?;
    log::info!("http3/webtransport echo listening on udp/{addr}");
    loop {
        let incoming = server.accept().await;
        tokio::spawn(async move {
            if let Err(e) = handle_session(incoming).await {
                log::debug!("http3 session ended: {e}");
            }
        });
    }
}

async fn handle_session(incoming: wtransport::endpoint::IncomingSession) -> Result<(), BoxErr> {
    let request = incoming.await?;
    let connection = request.accept().await?;
    loop {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let mut buf = vec![0u8; 8192];
        // wtransport's `read` yields `Some(n)` per chunk, `None` at the
        // end of the stream.
        while let Some(n) = recv.read(&mut buf).await? {
            send.write_all(&buf[..n]).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wtransport::{ClientConfig, Endpoint};

    /// End-to-end over real QUIC: the self-signed cert is accepted by
    /// the client purely via its SHA-256 (the WebTransport
    /// `serverCertificateHashes` flow), then a framed message
    /// round-trips through a bidi stream. Proves the transport + cert
    /// pinning + framing all work together.
    #[tokio::test]
    async fn webtransport_framed_roundtrip_with_cert_hash() {
        let identity = self_signed(&["localhost", "127.0.0.1"]).expect("identity");
        let digest = identity.certificate_chain().as_slice()[0].hash();

        let server = Endpoint::server(
            ServerConfig::builder()
                .with_bind_address("127.0.0.1:0".parse().unwrap())
                .with_identity(identity)
                .build(),
        )
        .expect("server endpoint");
        let port = server.local_addr().expect("local addr").port();

        // Server: accept one session, echo one framed message.
        tokio::spawn(async move {
            let incoming = server.accept().await;
            let conn = incoming.await.unwrap().accept().await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            if let Some((tag, payload)) = read_frame(&mut recv).await.unwrap() {
                send.write_all(&frame(tag, &payload)).await.unwrap();
                send.finish().await.unwrap();
            }
        });

        // Client: pin the cert by hash, open a bidi stream, round-trip.
        let client = Endpoint::client(
            ClientConfig::builder()
                .with_bind_default()
                .with_server_certificate_hashes([digest])
                .build(),
        )
        .expect("client endpoint");
        let conn = client
            .connect(format!("https://127.0.0.1:{port}"))
            .await
            .expect("connect");
        let (mut send, mut recv) = conn.open_bi().await.expect("open_bi").await.expect("bi");
        send.write_all(&frame(TAG_IN, b"ping")).await.expect("write");
        let (tag, payload) = read_frame(&mut recv).await.expect("read").expect("frame");
        assert_eq!(tag, TAG_IN);
        assert_eq!(payload, b"ping");
    }

    #[test]
    fn frame_is_length_delimited() {
        // [00 00 00 06][01][h e l l o]
        let f = frame(TAG_IN, b"hello");
        assert_eq!(&f[..4], &6u32.to_be_bytes(), "4-byte BE length = 1 tag + 5 payload");
        assert_eq!(f[4], TAG_IN);
        assert_eq!(&f[5..], b"hello");
        // Empty payload still carries the tag → body length 1.
        let e = frame(TAG_OUT, b"");
        assert_eq!(&e[..4], &1u32.to_be_bytes());
        assert_eq!(e[4], TAG_OUT);
        assert_eq!(e.len(), 5);
    }

    #[test]
    fn self_signed_cert_hash_is_64_hex() {
        let id = self_signed(&["localhost", "127.0.0.1"]).expect("self-signed identity");
        let h = cert_hash_hex(&id);
        assert_eq!(h.len(), 64, "sha-256 hex must be 64 chars, got {h:?}");
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()), "hash must be hex: {h}");
    }
}
