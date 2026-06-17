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

use sha2::{Digest, Sha256};
use wtransport::{Endpoint, Identity, ServerConfig};

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

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

    #[test]
    fn self_signed_cert_hash_is_64_hex() {
        let id = self_signed(&["localhost", "127.0.0.1"]).expect("self-signed identity");
        let h = cert_hash_hex(&id);
        assert_eq!(h.len(), 64, "sha-256 hex must be 64 chars, got {h:?}");
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()), "hash must be hex: {h}");
    }
}
