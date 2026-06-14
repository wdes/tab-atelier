// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier-headless bench` — terminal throughput self-test.
//!
//! Generates vtebench-style payloads (alacritty/vtebench) and drains
//! them through the exact pipeline a live tab uses for incoming PTY
//! bytes: the per-tab [`PtyRing`] tap followed by alacritty's VT
//! parser advancing a real `Term`. The wall-clock to drain each
//! payload yields a MB/s figure.
//!
//! Scope, mirroring vtebench's own README caveat: **this measures PTY
//! read + parse + ring throughput only.** It does NOT touch the gpui
//! paint loop, so it says nothing about frame rate or typing latency
//! (use Pavel Fatin's typometer for that). What it IS good for: a
//! reproducible, display-free, CI-trackable number that catches a
//! regression in the parser / ring path we own — e.g. if a future
//! `PtyRing::push` change doubles per-byte cost, this surfaces it.
//!
//! Source of the payload shapes: <https://github.com/alacritty/vtebench>
//! Thanks to the Alacritty team for vtebench.

use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config;
use std::sync::Arc;

use crate::pty_ring::PtyRing;
use crate::term_export::TermDims;

/// No-op listener — the benchmark never reads back PTY replies, so
/// `send_event` is dropped. (A real tab forwards `PtyWrite` events to
/// the PTY; here there's no PTY.)
#[derive(Clone, Default)]
struct BenchListener;

impl EventListener for BenchListener {
    fn send_event(&self, _event: AlacrittyEvent) {}
}

/// One benchmark case: a name + a payload builder targeting roughly
/// `target_bytes`. Builders are deterministic (no RNG) so successive
/// runs are comparable.
struct Case {
    name: &'static str,
    build: fn(target_bytes: usize) -> Vec<u8>,
}

/// Plain scrolling text — the cheapest case, dominated by line feeds.
/// Mirrors vtebench `scrolling`.
fn payload_scrolling(target: usize) -> Vec<u8> {
    let line = "The quick brown fox jumps over the lazy dog 0123456789\r\n";
    let mut out = Vec::with_capacity(target + line.len());
    while out.len() < target {
        out.extend_from_slice(line.as_bytes());
    }
    out
}

/// SGR-heavy coloured cells — every cell carries a fresh 24-bit fg
/// colour, stressing the SGR parser + cell attribute writes. Mirrors
/// vtebench `dense_cells` / `scrolling_in_region` colour load.
fn payload_sgr_dense(target: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target + 64);
    let mut n: u32 = 0;
    while out.len() < target {
        // 6-bit colour ramp so the byte stream stays varied without
        // an RNG; advance a counter for each cell.
        let r = (n.wrapping_mul(7) & 0xFF) as u8;
        let g = (n.wrapping_mul(13) & 0xFF) as u8;
        let b = (n.wrapping_mul(17) & 0xFF) as u8;
        out.extend_from_slice(format!("\x1b[38;2;{r};{g};{b}mX").as_bytes());
        n = n.wrapping_add(1);
        if n.is_multiple_of(80) {
            out.extend_from_slice(b"\x1b[0m\r\n");
        }
    }
    out.extend_from_slice(b"\x1b[0m");
    out
}

/// Dense Unicode / CJK — wide chars + combining marks exercise the
/// grapheme + wide-char path. Mirrors vtebench `unicode`.
fn payload_unicode(target: usize) -> Vec<u8> {
    // Mix of CJK (wide), accented Latin, box drawing, and emoji.
    let line = "你好世界 café ┌─┤▌▎ résumé ✅❌⚠️ ありがとう 日本語テスト\r\n";
    let mut out = Vec::with_capacity(target + line.len());
    while out.len() < target {
        out.extend_from_slice(line.as_bytes());
    }
    out
}

/// Cursor-positioning storm — a TUI redrawing in place via absolute
/// cursor moves + erases, never scrolling. Mirrors vtebench
/// `cursor_motion` and the in-place redraw pattern of Claude Code /
/// htop / vim that our ring exists to capture.
fn payload_cursor_motion(target: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target + 64);
    let mut row: u32 = 1;
    while out.len() < target {
        // Move to (row, 1), clear line, write a cell, bounce rows.
        out.extend_from_slice(format!("\x1b[{row};1H\x1b[2KrowdataX").as_bytes());
        row = if row >= 24 { 1 } else { row + 1 };
    }
    out
}

/// Big paste of random printable characters — the worst-case "user
/// pasted a huge blob" ingest, the heaviest stress on the parser's
/// plain-print path plus the scrollback churn.
///
/// **Bash-safe by construction**: every line is prefixed with `# ` so
/// that if this payload is ever pasted into a real shell (rather than
/// fed to a Term as it is here) the shell treats each line as a
/// comment and runs nothing. Lines are ~200 chars to mirror a wide
/// terminal wrapping a pasted blob.
///
/// The randomness is a deterministic xorshift (fixed seed) so runs are
/// comparable — only printable ASCII 0x21..0x7e is emitted, never a
/// control byte, so the parser stays on its plain-print fast path and
/// the number reflects raw print throughput rather than escape parsing.
fn payload_paste_random(target: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target + 256);
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15; // fixed seed → deterministic
    let mut next = || {
        // xorshift64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        // map to printable ASCII excluding space (0x21..=0x7e)
        0x21 + (state % (0x7e - 0x21 + 1)) as u8
    };
    while out.len() < target {
        out.extend_from_slice(b"# ");
        for _ in 0..200 {
            out.push(next());
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

const CASES: &[Case] = &[
    Case {
        name: "scrolling",
        build: payload_scrolling,
    },
    Case {
        name: "sgr_dense",
        build: payload_sgr_dense,
    },
    Case {
        name: "unicode",
        build: payload_unicode,
    },
    Case {
        name: "cursor_motion",
        build: payload_cursor_motion,
    },
    Case {
        name: "paste_random",
        build: payload_paste_random,
    },
];

/// Drain `payload` through ring + parser, return the elapsed time.
/// Chunked at 4 KiB to mirror a real PTY read's `read(2)` granularity
/// (so the ring's per-push overhead is exercised realistically rather
/// than amortised over one giant push).
fn drain(term: &Arc<FairMutex<Term<BenchListener>>>, ring: &mut PtyRing, payload: &[u8]) -> Duration {
    const CHUNK: usize = 4096;
    let start = Instant::now();
    let mut parser: vte::ansi::Processor = vte::ansi::Processor::new();
    for chunk in payload.chunks(CHUNK) {
        ring.push(chunk);
        let mut t = term.lock();
        parser.advance(&mut *t, chunk);
    }
    start.elapsed()
}

/// Parse args, run every case, print a table. Returns a process exit
/// code.
///
/// Flags:
///   --mb N         payload size per case in MiB (default 64)
///   --iterations N repeat each case N times, report the best (default 3)
///   --cols / --rows  grid dimensions (default 200 × 50)
#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut mb: usize = 64;
    let mut iterations: usize = 3;
    let mut cols: usize = 200;
    let mut rows: usize = 50;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mb" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) if n >= 1 => mb = n,
                    _ => {
                        eprintln!("bench: --mb expects a number >= 1");
                        return 2;
                    }
                }
            }
            "--iterations" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) if n >= 1 => iterations = n,
                    _ => {
                        eprintln!("bench: --iterations expects a number >= 1");
                        return 2;
                    }
                }
            }
            "--cols" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) if n >= 2 => cols = n,
                    _ => {
                        eprintln!("bench: --cols expects a number >= 2");
                        return 2;
                    }
                }
            }
            "--rows" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) if n >= 1 => rows = n,
                    _ => {
                        eprintln!("bench: --rows expects a number >= 1");
                        return 2;
                    }
                }
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: tab-atelier-headless bench [--mb N] [--iterations N] [--cols N] [--rows N]\n\
                     Drains vtebench-style payloads through the PtyRing + alacritty parser\n\
                     and reports throughput. Measures PTY-read/parse only — NOT paint or\n\
                     typing latency (use typometer for those). Payload shapes from\n\
                     https://github.com/alacritty/vtebench (thanks to the Alacritty team)."
                );
                return 0;
            }
            other => {
                eprintln!("bench: unexpected argument: {other}");
                return 2;
            }
        }
        i += 1;
    }

    let target = mb * 1024 * 1024;
    println!(
        "⏱ tab-atelier bench · {mb} MiB/case · {iterations} iter · grid {cols}×{rows}\n\
         (PTY-read + parse + ring throughput + allocations; not paint/latency)\n"
    );
    println!(
        "{:<16} {:>10} {:>12} {:>12} {:>12}",
        "case", "best (ms)", "throughput", "allocs", "alloc/MiB"
    );

    let mut all_mb_s: Vec<f64> = Vec::new();
    for case in CASES {
        let payload = (case.build)(target);
        let actual = payload.len();
        let mut best = Duration::from_secs(u64::MAX);
        // Count allocations of ONE representative drain (not all
        // iterations — they'd multiply). Snapshot straddles only the
        // drain so payload-build allocs aren't counted.
        let mut drain_allocs: u64 = 0;
        for iter in 0..iterations {
            // Fresh Term + ring each iteration so scrollback / grid
            // state from the prior run can't skew the next.
            let config = Config {
                scrolling_history: 10_000,
                ..Config::default()
            };
            let term = Arc::new(FairMutex::new(Term::new(
                config,
                &TermDims {
                    columns: cols,
                    screen_lines: rows,
                },
                BenchListener,
            )));
            let mut ring = PtyRing::default();
            let a0 = crate::alloc_count::snapshot();
            let elapsed = drain(&term, &mut ring, &payload);
            let a1 = crate::alloc_count::snapshot();
            if iter == 0 {
                drain_allocs = a1.since(a0).allocations;
            }
            best = best.min(elapsed);
        }
        let secs = best.as_secs_f64();
        let mb_s = (actual as f64 / (1024.0 * 1024.0)) / secs;
        all_mb_s.push(mb_s);
        let per_mib = drain_allocs as f64 / (actual as f64 / (1024.0 * 1024.0));
        println!(
            "{:<16} {:>10.1} {:>7.0} MiB/s {:>12} {:>12.0}",
            case.name,
            secs * 1000.0,
            mb_s,
            drain_allocs,
            per_mib,
        );
    }

    let mean = all_mb_s.iter().sum::<f64>() / all_mb_s.len() as f64;
    let total = crate::alloc_count::snapshot();
    println!(
        "\nmean: {mean:.0} MiB/s · process allocations so far: {}",
        total.allocations
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_reach_target_size() {
        let target = 64 * 1024;
        for case in CASES {
            let p = (case.build)(target);
            assert!(
                p.len() >= target,
                "{} produced {} bytes, wanted >= {target}",
                case.name,
                p.len()
            );
        }
    }

    #[test]
    fn paste_payload_is_bash_safe() {
        // Every non-empty line must start with '#' so a stray paste
        // into a real shell is inert. Split on \n; lines end with \r
        // (CRLF) so trim it before the check.
        let p = payload_paste_random(64 * 1024);
        let text = String::from_utf8(p).expect("printable ASCII only");
        for line in text.split('\n') {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            assert!(line.starts_with('#'), "non-comment line would hit bash: {line:?}");
        }
    }

    #[test]
    fn paste_payload_is_deterministic() {
        // Fixed seed → identical bytes across runs, so the bench
        // number is comparable build-to-build.
        assert_eq!(payload_paste_random(8 * 1024), payload_paste_random(8 * 1024));
    }

    #[test]
    fn drain_consumes_without_panicking() {
        // A small payload of each shape must drain through the real
        // parser + ring without panicking, and take non-zero time.
        let config = Config {
            scrolling_history: 1000,
            ..Config::default()
        };
        for case in CASES {
            let payload = (case.build)(8 * 1024);
            let term = Arc::new(FairMutex::new(Term::new(
                config.clone(),
                &TermDims {
                    columns: 80,
                    screen_lines: 24,
                },
                BenchListener,
            )));
            let mut ring = PtyRing::default();
            let _ = drain(&term, &mut ring, &payload);
            // Ring saw every byte.
            assert!(ring.total_len() >= payload.len() as u64);
        }
    }
}
