// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resource instrumentation for a tab's agent (Claude Code / catbus-agent).
//!
//! Two complementary probes answer "why does an idle agent eat resources
//! while it's meant to be sleeping":
//!
//! - **Sampler** (this module, driven from the app's 2 s persist tick):
//!   walks the agent's `/proc` subtree every tick and appends one JSONL
//!   line per tab recording `%CPU`, RSS, thread/process count and the
//!   **context-switch rate** — the tell-tale of a busy poll loop that
//!   burns CPU with nothing on screen. The log is append-only and per
//!   tab, so a future `tab-atelier` binary can `read_all()` a tab's
//!   timeline and decide how to poke it.
//! - **Tracer** (see [`crate::agent_launch_shell_suffix_instrumented`]):
//!   wraps the agent's `exec` with `strace -f -c` so the tab's whole
//!   session accumulates a syscall histogram, flushed to a per-session
//!   file when the agent finally exits.
//!
//! All instrumentation is **opt-in (default off)**, toggled per-flag via
//! `tab-atelier flags <name> on` (persisted) or the `TAB_ATELIER_*` env
//! vars — see [`INSTRUMENTATION_FLAGS`]. A normal run does nothing extra.
//! The JSONL schema is the stable tap contract — see [`ProbeLine`] and
//! `docs/agent-probe.md`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Kernel clock ticks per second (`sysconf(_SC_CLK_TCK)`). Fixed at 100
/// on every Linux we target; hard-coded because reading it means an
/// `unsafe` libc `sysconf` call and the crate denies `unsafe`.
const CLK_TCK: f64 = 100.0;

/// Rotate a per-tab probe log once it passes this size, keeping one
/// `.1` generation. Keeps an always-on sampler from filling the disk on
/// a long-lived tab (~2 KiB/min at 2 s ticks ⇒ a cap of a few hours).
const LOG_ROTATE_BYTES: u64 = 4 * 1024 * 1024;

/// One `/proc` snapshot of an agent's whole process subtree, summed.
///
/// Counters are cumulative (monotonic per process); deltas between two
/// samples become the rates in a [`ProbeLine`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcSample {
    /// `utime + stime` summed across the subtree, in clock ticks.
    pub cpu_ticks: u64,
    /// Resident set size summed across the subtree, in KiB.
    pub rss_kb: u64,
    /// Live thread count across the subtree.
    pub threads: u64,
    /// Process count in the subtree (agent + descendants).
    pub procs: u32,
    /// Voluntary context switches summed across the subtree.
    pub vol_ctxt: u64,
    /// Non-voluntary (pre-empted) context switches summed across the subtree.
    pub nonvol_ctxt: u64,
}

/// One appended line in a tab's probe log — the stable machine-readable
/// contract a future `tab-atelier` binary reads to understand a tab.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeLine {
    /// Wall-clock sample time, unix seconds (fractional).
    pub ts: f64,
    /// Seconds since the previous sample (the delta window).
    pub dt: f64,
    /// Sanitised tab name (matches the log filename).
    pub tab: String,
    /// The agent process (== PTY child, since agent tabs `exec claude`).
    pub pid: u32,
    /// `"idle"` (no descendant on-CPU) or `"working"` (a tool is running).
    pub state: String,
    /// CPU used over `dt`, as a percentage of one core (may exceed 100
    /// across threads/subprocesses). The headline "busy while sleeping".
    pub cpu_pct: f64,
    /// Resident memory of the subtree, MiB.
    pub rss_mb: f64,
    /// Live threads across the subtree.
    pub threads: u64,
    /// Processes in the subtree.
    pub procs: u32,
    /// Total context switches per second — a high rate at `state=idle`
    /// is the signature of a wakeup/poll loop.
    pub ctxsw_per_s: f64,
    /// Voluntary switches per second (blocking waits: sleeps, I/O).
    pub vol_per_s: f64,
    /// Non-voluntary switches per second (CPU pre-emption).
    pub nonvol_per_s: f64,
}

/// Per-tab sampler state: the previous [`ProcSample`] and its timestamp,
/// keyed by sanitised tab name, so each tick can compute deltas.
#[derive(Default)]
pub struct AgentProbe {
    prev: HashMap<String, (ProcSample, SystemTime)>,
}

impl AgentProbe {
    /// Sample the agent rooted at `pid`, and — once a previous sample
    /// exists to diff against — append a [`ProbeLine`] to the tab's log.
    ///
    /// `state` is the caller's activity classification (`"idle"` /
    /// `"working"`). No-op (and no allocation of a line) on the first
    /// sample of a tab, when the probe is disabled, or when `pid` has no
    /// readable `/proc` entry. `now` is injected so callers can batch a
    /// single timestamp across a tick (and for deterministic tests).
    pub fn observe(&mut self, base: &Path, tab: &str, pid: u32, state: &str, now: SystemTime) {
        if !probe_enabled() {
            return;
        }
        let Some(cur) = sample_tree(pid) else {
            // Agent gone mid-tick — drop the stale baseline so a reused
            // pid can't produce a bogus negative-clamped delta later.
            self.prev.remove(tab);
            return;
        };
        if let Some((prev, prev_t)) = self.prev.get(tab).copied()
            && let Ok(dt) = now.duration_since(prev_t)
        {
            let dt = dt.as_secs_f64();
            if dt > 0.0
                && let Some(line) = build_line(tab, pid, state, prev, cur, dt, unix_secs(now))
            {
                append_line(base, tab, &line);
            }
        }
        self.prev.insert(tab.to_string(), (cur, now));
    }

    /// Forget a tab's baseline (call on close so its map entry doesn't
    /// linger for the process lifetime).
    pub fn forget(&mut self, tab: &str) {
        self.prev.remove(tab);
    }
}

/// Assemble a [`ProbeLine`] from two samples and the window `dt`.
/// Pure — the unit-testable core of the sampler.
#[must_use]
fn build_line(
    tab: &str,
    pid: u32,
    state: &str,
    prev: ProcSample,
    cur: ProcSample,
    dt: f64,
    ts: f64,
) -> Option<ProbeLine> {
    if dt <= 0.0 {
        return None;
    }
    Some(ProbeLine {
        ts,
        dt,
        tab: tab.to_string(),
        pid,
        state: state.to_string(),
        cpu_pct: rate(prev.cpu_ticks, cur.cpu_ticks, dt) / CLK_TCK * 100.0,
        rss_mb: cur.rss_kb as f64 / 1024.0,
        threads: cur.threads,
        procs: cur.procs,
        ctxsw_per_s: rate(prev.vol_ctxt + prev.nonvol_ctxt, cur.vol_ctxt + cur.nonvol_ctxt, dt),
        vol_per_s: rate(prev.vol_ctxt, cur.vol_ctxt, dt),
        nonvol_per_s: rate(prev.nonvol_ctxt, cur.nonvol_ctxt, dt),
    })
}

/// Per-second rate of a monotonic counter over `dt`. Clamps a counter
/// that went backwards (subtree membership changed / pid reused) to 0
/// rather than emitting a nonsense negative spike.
#[must_use]
fn rate(prev: u64, cur: u64, dt: f64) -> f64 {
    cur.saturating_sub(prev) as f64 / dt
}

/// BFS the `/proc` subtree rooted at `root_pid`, summing per-process
/// counters. `None` only when the root itself has already vanished.
#[must_use]
pub fn sample_tree(root_pid: u32) -> Option<ProcSample> {
    use std::fmt::Write as _;
    let mut acc = ProcSample::default();
    let mut seen_root = false;
    let mut path = String::with_capacity(48);
    let mut queue = vec![root_pid];
    while let Some(pid) = queue.pop() {
        path.clear();
        let _ = write!(path, "/proc/{pid}/stat");
        let Ok(stat) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some((cpu_ticks, threads)) = parse_stat(&stat) else {
            continue;
        };
        seen_root = true;
        acc.cpu_ticks += cpu_ticks;
        acc.threads += threads;
        acc.procs += 1;
        path.clear();
        let _ = write!(path, "/proc/{pid}/status");
        if let Ok(status) = std::fs::read_to_string(&path) {
            let (rss, vol, nonvol) = parse_status(&status);
            acc.rss_kb += rss;
            acc.vol_ctxt += vol;
            acc.nonvol_ctxt += nonvol;
        }
        path.clear();
        let _ = write!(path, "/proc/{pid}/task/{pid}/children");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            queue.extend(raw.split_ascii_whitespace().filter_map(|s| s.parse::<u32>().ok()));
        }
    }
    seen_root.then_some(acc)
}

/// Extract `(utime+stime, num_threads)` from a `/proc/<pid>/stat` line.
///
/// The `comm` field (2nd) can contain spaces and parens, so we key off
/// the **last** `)`; the space-separated fields after it start at field
/// 3 (`state`), making `utime`=idx 11, `stime`=idx 12, `num_threads`=idx 17.
#[must_use]
fn parse_stat(stat: &str) -> Option<(u64, u64)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_ascii_whitespace().collect();
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    let threads: u64 = f.get(17)?.parse().ok()?;
    Some((utime + stime, threads))
}

/// Pull `(VmRSS_kB, voluntary_ctxt, nonvoluntary_ctxt)` out of a
/// `/proc/<pid>/status` blob. Missing fields default to 0.
#[must_use]
fn parse_status(status: &str) -> (u64, u64, u64) {
    let mut rss = 0;
    let mut vol = 0;
    let mut nonvol = 0;
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            rss = first_u64(v);
        } else if let Some(v) = line.strip_prefix("voluntary_ctxt_switches:") {
            vol = first_u64(v);
        } else if let Some(v) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            nonvol = first_u64(v);
        }
    }
    (rss, vol, nonvol)
}

/// First whitespace-separated integer in `s` (e.g. `"\t 512 kB"` ⇒ 512).
#[must_use]
fn first_u64(s: &str) -> u64 {
    s.split_ascii_whitespace()
        .next()
        .and_then(|t| t.parse().ok())
        .unwrap_or(0)
}

/// The instrumentation toggles surfaced by `tab-atelier flags`:
/// `(cli-name, env-var, default, one-line help)`.
///
/// All are resolved via [`flag_enabled`] — the env var wins at runtime,
/// else the persisted `<state>/flags.json` (so a systemd daemon that
/// can't easily set env can still be toggled with `flags <name> on`),
/// else the default here. All default **off** — the instrumentation is
/// fully opt-in; a normal run does nothing extra.
pub const INSTRUMENTATION_FLAGS: &[(&str, &str, bool, &str)] = &[
    (
        "frame-timing",
        "TAB_ATELIER_FRAME_TIMING",
        false,
        "per-render-frame JSONL (idle-repaint debug); heavy",
    ),
    (
        "trace",
        "TAB_ATELIER_AGENT_TRACE",
        false,
        "strace -f -c syscall histogram per agent session",
    ),
    (
        "probe",
        "TAB_ATELIER_AGENT_PROBE",
        false,
        "per-tick CPU/RSS/ctxsw sampler",
    ),
    (
        "reap",
        "TAB_ATELIER_AGENT_REAP",
        false,
        "kill leaked agent ghosts on desktop startup",
    ),
];

/// Resolve `cli-name` (e.g. `frame-timing`) to its env-var key.
#[must_use]
pub fn flag_env_var(cli_name: &str) -> Option<&'static str> {
    INSTRUMENTATION_FLAGS
        .iter()
        .find(|(n, ..)| *n == cli_name)
        .map(|(_, env, ..)| *env)
}

/// `true` only when the `probe` flag resolves on. Default OFF (opt-in).
#[must_use]
pub fn probe_enabled() -> bool {
    flag_enabled("TAB_ATELIER_AGENT_PROBE", false)
}

/// `true` only when the `trace` flag resolves on. Default OFF (opt-in);
/// the tracer additionally needs `strace` on `PATH`.
#[must_use]
pub fn trace_enabled() -> bool {
    flag_enabled("TAB_ATELIER_AGENT_TRACE", false)
}

/// `true` only when the `frame-timing` flag resolves on. Default OFF: it
/// makes the agent append a JSONL record on *every* render frame (via
/// `CLAUDE_CODE_FRAME_TIMING_LOG`), so it stays opt-in.
#[must_use]
pub fn frame_timing_enabled() -> bool {
    flag_enabled("TAB_ATELIER_FRAME_TIMING", false)
}

/// Parse a bool-ish string: `Some(true)` for on-values, `Some(false)`
/// for off-values, `None` if unset/unrecognised.
#[must_use]
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// The persisted instrumentation-flags file: `<state>/flags.json`.
#[must_use]
pub fn flags_path(base: &Path) -> PathBuf {
    crate::state_dir(base).join("flags.json")
}

/// Read the persisted flag map (env-var key → bool). Empty on any miss.
#[must_use]
pub fn read_flags(base: &Path) -> std::collections::BTreeMap<String, bool> {
    std::fs::read_to_string(flags_path(base))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Resolve a flag: env var wins, then persisted `flags.json`, then default.
#[must_use]
pub fn flag_enabled(env_var: &str, default: bool) -> bool {
    resolve_flag(
        std::env::var(env_var).ok().and_then(|v| parse_bool(&v)),
        read_flags(&state_base()).get(env_var).copied(),
        default,
    )
}

/// Pure precedence: env → persisted → default.
#[must_use]
fn resolve_flag(env: Option<bool>, persisted: Option<bool>, default: bool) -> bool {
    env.or(persisted).unwrap_or(default)
}

/// Persist a flag (`Some`) or clear it back to env/default (`None`).
/// Takes effect on the next agent launch / daemon restart.
///
/// # Errors
/// Propagates create-dir / write / remove failures.
pub fn set_persisted_flag(base: &Path, env_var: &str, value: Option<bool>) -> std::io::Result<()> {
    let mut flags = read_flags(base);
    match value {
        Some(b) => {
            flags.insert(env_var.to_string(), b);
        }
        None => {
            flags.remove(env_var);
        }
    }
    let path = flags_path(base);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if flags.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
    }
    std::fs::write(&path, serde_json::to_string_pretty(&flags).unwrap_or_default())
}

/// The per-tab probe log path: `<state>/agent_probe_tab-<name>.jsonl`.
#[must_use]
pub fn log_path(base: &Path, tab: &str) -> PathBuf {
    crate::state_dir(base).join(format!("agent_probe_tab-{}.jsonl", crate::sanitize_tab_filename(tab)))
}

/// The per-session tracer output path: `<state>/agent_trace_<kind>_<session>.txt`.
#[must_use]
pub fn trace_log_path(base: &Path, kind: &str, session: &str) -> PathBuf {
    crate::state_dir(base).join(format!(
        "agent_trace_{}_{}.txt",
        crate::sanitize_tab_filename(kind),
        crate::sanitize_tab_filename(session)
    ))
}

/// The per-session render frame-timing log: `<state>/agent_frames_<kind>_<session>.jsonl`.
///
/// One JSON object per rendered frame (`total`/`renderer`/`diff`/`yoga`/
/// `patches`/…), written by the agent when `CLAUDE_CODE_FRAME_TIMING_LOG`
/// points here. A healthy idle tab writes ~nothing after settling; a tab
/// stuck repainting fills this continuously — the idle-CPU smoking gun.
#[must_use]
pub fn frame_log_path(base: &Path, kind: &str, session: &str) -> PathBuf {
    crate::state_dir(base).join(format!(
        "agent_frames_{}_{}.jsonl",
        crate::sanitize_tab_filename(kind),
        crate::sanitize_tab_filename(session)
    ))
}

/// Append one JSONL line to the tab's probe log, rotating first if the
/// file has grown past [`LOG_ROTATE_BYTES`]. Best-effort — a failed
/// write must never disturb the tick.
fn append_line(base: &Path, tab: &str, line: &ProbeLine) {
    let Ok(json) = serde_json::to_string(line) else {
        return;
    };
    let path = log_path(base, tab);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{json}");
    }
}

/// Read a tab's whole probe timeline (oldest → newest), skipping any
/// unparseable line. The primary "tap" a future binary calls.
#[must_use]
pub fn read_all(base: &Path, tab: &str) -> Vec<ProbeLine> {
    let Ok(f) = std::fs::File::open(log_path(base, tab)) else {
        return Vec::new();
    };
    BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

/// The most recent probe line for a tab, if any.
#[must_use]
pub fn read_latest(base: &Path, tab: &str) -> Option<ProbeLine> {
    read_all(base, tab).pop()
}

/// List the tab names that currently have a probe log under `base`.
#[must_use]
pub fn list_logs(base: &Path) -> Vec<String> {
    let dir = crate::state_dir(base);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| {
            n.strip_prefix("agent_probe_tab-")?
                .strip_suffix(".jsonl")
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

/// State base directory (`$XDG_STATE_HOME` or `~/.local/state`).
///
/// Mirrors `platform::state_base_dir()` without the GUI-only cfg baggage
/// — the tracer path builder in `lib.rs` needs it on every target.
#[must_use]
pub fn state_base() -> PathBuf {
    std::env::var("XDG_STATE_HOME").map_or_else(
        |_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local/state")
        },
        PathBuf::from,
    )
}

/// Resolve the tracer binary (`strace`) on `PATH`.
///
/// Honours an explicit override in `TAB_ATELIER_AGENT_TRACE` when it
/// names a path/command rather than an on/off toggle. `None` ⇒ no tracer
/// available, launch bare. Returns the resolved absolute path so the
/// wrapped `exec` line doesn't depend on the agent shell's `PATH`.
#[must_use]
pub fn resolve_tracer() -> Option<String> {
    // A non-bool env value names a tracer command and implies enabled
    // (else, since tracing is off by default, the override would be
    // silently ignored). Otherwise honour the on/off flag.
    let env = std::env::var("TAB_ATELIER_AGENT_TRACE").ok();
    let path_override = env
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty() && parse_bool(v).is_none());
    if path_override.is_none() && !trace_enabled() {
        return None;
    }
    which(path_override.unwrap_or("strace"))
}

/// Minimal `PATH` lookup — absolute/relative names are returned as-is if
/// they exist and are a file; bare names are searched across `$PATH`.
#[must_use]
fn which(name: &str) -> Option<String> {
    if name.contains('/') {
        return Path::new(name).is_file().then(|| name.to_string());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Single-quote a string for safe inclusion in a `sh -c` command line.
#[must_use]
pub fn sh_squote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[must_use]
fn unix_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_parens_in_comm() {
        // comm = "(sh)" — embedded parens must not fool the field split.
        let stat = "42 ((sh)) S 1 42 42 0 -1 0 0 0 0 0 \
                    100 40 0 0 20 0 7 0 0 0 0";
        let (cpu, threads) = parse_stat(stat).unwrap();
        assert_eq!(cpu, 140); // utime 100 + stime 40
        assert_eq!(threads, 7);
    }

    #[test]
    fn parses_status_fields() {
        let status = "Name:\tclaude\nVmRSS:\t  524288 kB\n\
                      voluntary_ctxt_switches:\t1200\n\
                      nonvoluntary_ctxt_switches:\t34\n";
        assert_eq!(parse_status(status), (524_288, 1200, 34));
    }

    #[test]
    fn rate_clamps_backwards_counter() {
        assert!((rate(10, 20, 2.0) - 5.0).abs() < 1e-9);
        assert!(rate(20, 10, 2.0).abs() < f64::EPSILON); // reset ⇒ clamp, no negative spike
    }

    #[test]
    fn build_line_computes_cpu_and_switch_rates() {
        let prev = ProcSample {
            cpu_ticks: 100,
            rss_kb: 1024,
            threads: 4,
            procs: 2,
            vol_ctxt: 500,
            nonvol_ctxt: 100,
        };
        // +200 ticks over 2 s = 1.0 core-second ⇒ 100% of one core.
        let cur = ProcSample {
            cpu_ticks: 300,
            rss_kb: 2048,
            threads: 5,
            procs: 3,
            vol_ctxt: 2500,
            nonvol_ctxt: 300,
        };
        let l = build_line("t", 7, "idle", prev, cur, 2.0, 123.0).unwrap();
        assert!((l.cpu_pct - 100.0).abs() < 1e-9);
        assert!((l.rss_mb - 2.0).abs() < 1e-9);
        assert!((l.ctxsw_per_s - 1100.0).abs() < 1e-9); // (2800-600)/2
        assert!((l.vol_per_s - 1000.0).abs() < 1e-9); // (2500-500)/2
        assert_eq!(l.procs, 3);
        assert_eq!(l.state, "idle");
    }

    #[test]
    fn instrumentation_is_off_by_default() {
        // No env / no persisted flag in the test process ⇒ all opt-in
        // flags resolve off. (frame-timing too; reap lives in agent_reaper.)
        assert!(!probe_enabled());
        assert!(!trace_enabled());
        assert!(!frame_timing_enabled());
        // Every declared flag defaults off.
        assert!(INSTRUMENTATION_FLAGS.iter().all(|(_, _, default, _)| !*default));
    }

    #[test]
    fn flag_precedence_env_then_persisted_then_default() {
        // env wins over persisted, persisted over default.
        assert!(resolve_flag(Some(true), Some(false), false));
        assert!(!resolve_flag(Some(false), Some(true), true));
        assert!(resolve_flag(None, Some(true), false));
        assert!(!resolve_flag(None, Some(false), true));
        assert!(resolve_flag(None, None, true));
        assert!(!resolve_flag(None, None, false));
    }

    #[test]
    fn parse_bool_recognises_on_off() {
        assert_eq!(parse_bool("on"), Some(true));
        assert_eq!(parse_bool(" TRUE "), Some(true));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("no"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn flag_env_var_maps_cli_names() {
        assert_eq!(flag_env_var("frame-timing"), Some("TAB_ATELIER_FRAME_TIMING"));
        assert_eq!(flag_env_var("reap"), Some("TAB_ATELIER_AGENT_REAP"));
        assert_eq!(flag_env_var("nope"), None);
    }

    #[test]
    fn log_and_trace_paths_are_per_tab() {
        // sanitize_tab_filename appends a `-<crc32>` suffix, so assert the
        // stable prefix/extension rather than the exact interior.
        let base = Path::new("/tmp/ta-test");
        let p = log_path(base, "my tab");
        assert!(
            p.to_string_lossy().contains("agent_probe_tab-my_tab-"),
            "{}",
            p.display()
        );
        assert_eq!(p.extension().and_then(|e| e.to_str()), Some("jsonl"));
        let t = trace_log_path(base, "claude", "abc-123");
        assert!(t.to_string_lossy().contains("agent_trace_claude-"), "{}", t.display());
        assert_eq!(t.extension().and_then(|e| e.to_str()), Some("txt"));
    }
}
