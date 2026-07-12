// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier-headless bench-lag` — web-viewer input-lag self-test.
//!
//! Connects to a running tab's viewer WebSocket as a client and times
//! the keystroke→echo round-trip: send a benign `x` (`TAG_IN`), wait
//! for the echoed bytes to come back (`TAG_OUT`), record the elapsed,
//! then send a Backspace to erase the `x` so the prompt line is left
//! unchanged. Reports min / median / p95 / mean over N samples.
//!
//! This is the same measurement an external script would do, but baked
//! into the binary so the daemon can benchmark its own end-to-end
//! latency (network + input-drain tick + PTY echo + output-pump tick +
//! WS) with no extra tooling.
//!
//! Scope: measures the FULL round-trip including the network to the
//! host you point it at. Point it at `127.0.0.1` to isolate the
//! server-side tick floor; point it at a remote to include real
//! network latency. `ws://` only — the TLS endpoint isn't needed to
//! measure the server's own latency.

use std::io::ErrorKind;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

const TAG_IN: u8 = 0x01;
const TAG_OUT: u8 = 0x02;

/// Turn the viewer URL the user opens in a browser
/// (`http://host/tabs/by-id/<id>/view?token=…`) into the WebSocket URL
/// the client connects to (`ws://host/tabs/by-id/<id>/ws?token=…&since=0`).
/// Also accepts an already-`ws://`/`wss://` URL (passed through, with
/// `since=0` ensured).
fn to_ws_url(view: &str) -> Result<String, String> {
    let (scheme, rest) = if let Some(r) = view.strip_prefix("https://") {
        ("wss://", r)
    } else if let Some(r) = view.strip_prefix("http://") {
        ("ws://", r)
    } else if let Some(r) = view.strip_prefix("wss://") {
        ("wss://", r)
    } else if let Some(r) = view.strip_prefix("ws://") {
        ("ws://", r)
    } else {
        return Err("URL must start with http://, https://, ws:// or wss://".into());
    };

    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    // Normalise the path to end with `/ws`.
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix("/view").map_or_else(
        || {
            if path.ends_with("/ws") {
                path.to_string()
            } else {
                format!("{path}/ws")
            }
        },
        |p| format!("{p}/ws"),
    );

    let mut url = format!("{scheme}{path}");
    match query {
        Some(q) => {
            url.push('?');
            url.push_str(q);
            if !q.split('&').any(|kv| kv.starts_with("since=")) {
                url.push_str("&since=0");
            }
        }
        None => url.push_str("?since=0"),
    }
    Ok(url)
}

/// True iff the error is a read-timeout (no data within the socket's
/// read timeout) rather than a real connection failure.
fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut))
}

/// Set a per-read timeout on the underlying TCP stream so reads return
/// instead of blocking forever. Returns false for a TLS stream (we
/// only wire the plain `ws://` path).
fn set_read_timeout(sock: &mut WebSocket<MaybeTlsStream<TcpStream>>, t: Duration) -> bool {
    match sock.get_mut() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(t)).is_ok(),
        _ => false,
    }
}

/// Read and discard frames until a read times out (the socket has been
/// quiet for one read-timeout interval) — used to swallow the initial
/// `since=0` backlog burst and any straggler output between samples.
fn drain(sock: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    // Reads stop at the first error: a read-timeout (socket quiet for
    // one interval) or a closed connection.
    while sock.read().is_ok() {}
}

/// Per-sample probe bytes. Unique per sample so the echo matcher can't
/// confuse unrelated output with our keystrokes — the old fixed `x`
/// matched any `x` a busy tab happened to print and inflated runs
/// (issue #9 item 4). Plain ASCII so every shell echoes it verbatim.
fn nonce(sample: usize) -> Vec<u8> {
    format!("q{sample:03}z").into_bytes()
}

/// Whether the accumulated echo stream contains the nonce. The echo can
/// arrive split across several `TAG_OUT` frames, so the caller appends
/// each frame's payload and re-checks the whole buffer.
fn contains_seq(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[must_use]
pub fn run(view_url: &str, samples: usize) -> i32 {
    let ws_url = match to_ws_url(view_url) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("bench-lag: {e}");
            return 2;
        }
    };
    eprintln!("bench-lag: connecting {ws_url}");
    let (mut sock, _resp) = match connect(&ws_url) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("bench-lag: connect failed: {e}");
            return 1;
        }
    };
    if !set_read_timeout(&mut sock, Duration::from_millis(50)) {
        eprintln!("bench-lag: wss/TLS stream — read timeout unsupported here; use the ws:// URL");
        return 1;
    }

    // Swallow the initial backlog replay before timing.
    drain(&mut sock);

    let mut lags: Vec<f64> = Vec::with_capacity(samples);
    for sample in 0..samples {
        drain(&mut sock);
        let probe = nonce(sample);
        let mut frame = Vec::with_capacity(1 + probe.len());
        frame.push(TAG_IN);
        frame.extend_from_slice(&probe);
        let t0 = Instant::now();
        if sock.send(Message::Binary(frame.into())).is_err() {
            break;
        }
        let deadline = t0 + Duration::from_secs(2);
        let mut echoed: Vec<u8> = Vec::new();
        while Instant::now() < deadline {
            match sock.read() {
                Ok(Message::Binary(b)) if b.first() == Some(&TAG_OUT) => {
                    echoed.extend_from_slice(&b[1..]);
                    if contains_seq(&echoed, &probe) {
                        lags.push(t0.elapsed().as_secs_f64() * 1000.0);
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) if is_timeout(&e) => {}
                Err(_) => break,
            }
        }
        // Erase the probe so the prompt line is net-unchanged: ONE
        // input frame carrying a backspace per probe byte.
        let mut erase = vec![TAG_IN];
        erase.resize(1 + probe.len(), 0x7f);
        let _ = sock.send(Message::Binary(erase.into()));
        std::thread::sleep(Duration::from_millis(120));
    }
    let _ = sock.close(None);

    report(&lags, samples);
    0
}

fn report(lags: &[f64], requested: usize) {
    if lags.is_empty() {
        println!(
            "bench-lag: no samples captured — no echo within 2 s. \
             Check the tab/token, and that the tab is at a shell that \
             echoes typed characters."
        );
        return;
    }
    let mut v = lags.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let pct = |q: f64| v[((q * n as f64) as usize).min(n - 1)];
    let mean = v.iter().sum::<f64>() / n as f64;
    println!("⏱ tab-atelier input-lag · keystroke→echo round-trip · {n}/{requested} samples");
    println!(
        "  min {:.1}  median {:.1}  p95 {:.1}  max {:.1}  mean {:.1}  (ms)",
        v[0],
        pct(0.5),
        pct(0.95),
        v[n - 1],
        mean,
    );
}

#[cfg(test)]
mod tests {
    use super::{contains_seq, nonce, to_ws_url};

    #[test]
    fn nonces_are_unique_and_shell_safe() {
        let all: Vec<Vec<u8>> = (0..200).map(nonce).collect();
        for (i, n) in all.iter().enumerate() {
            assert!(n.iter().all(u8::is_ascii_alphanumeric), "echoes verbatim");
            assert!(!all[..i].contains(n), "sample {i} nonce repeats");
        }
    }

    #[test]
    fn echo_matches_across_frame_boundaries_only_on_the_real_nonce() {
        let probe = nonce(7);
        // Unrelated output containing plain letters must NOT match.
        assert!(!contains_seq(b"onstff q00z blah", &probe));
        // The echo split across two frames matches once accumulated.
        let (a, b) = probe.split_at(2);
        let mut buf = b"prompt$ ".to_vec();
        buf.extend_from_slice(a);
        assert!(!contains_seq(&buf, &probe), "half a nonce is not a match");
        buf.extend_from_slice(b);
        assert!(contains_seq(&buf, &probe));
    }

    #[test]
    fn view_url_becomes_ws_with_since() {
        assert_eq!(
            to_ws_url("http://127.0.0.1:7890/tabs/by-id/abc/view?token=T").unwrap(),
            "ws://127.0.0.1:7890/tabs/by-id/abc/ws?token=T&since=0"
        );
    }

    #[test]
    fn https_view_becomes_wss() {
        assert_eq!(
            to_ws_url("https://h/tabs/by-id/x/view?token=T").unwrap(),
            "wss://h/tabs/by-id/x/ws?token=T&since=0"
        );
    }

    #[test]
    fn direct_ws_url_passes_through_and_keeps_since() {
        assert_eq!(
            to_ws_url("ws://h/tabs/by-id/x/ws?token=T&since=42").unwrap(),
            "ws://h/tabs/by-id/x/ws?token=T&since=42"
        );
    }

    #[test]
    fn no_query_gets_since() {
        assert_eq!(to_ws_url("http://h/tabs/3/view").unwrap(), "ws://h/tabs/3/ws?since=0");
    }

    #[test]
    fn rejects_unknown_scheme() {
        assert!(to_ws_url("ftp://h/x").is_err());
    }
}
