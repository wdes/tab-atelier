# Agent resource probe

Instrumentation for the per-tab agent (Claude Code / catbus-agent) to
answer **"why does an idle agent keep eating CPU/RAM while it's meant to
be sleeping?"** — and to give a future `tab-atelier` binary a stable,
on-disk hook it can read to understand a tab and poke at it.

All instrumentation is **opt-in (off by default)** — a normal run does
nothing extra. Toggle each flag with `tab-atelier flags <name> on`
(persisted to `<state>/flags.json`, applied on next launch) or the
matching `TAB_ATELIER_*` env var:

| flag | env var | what |
|---|---|---|
| `frame-timing` | `TAB_ATELIER_FRAME_TIMING` | per-render-frame JSONL (idle-repaint debug) |
| `trace` | `TAB_ATELIER_AGENT_TRACE` | `strace -f -c` syscall histogram |
| `probe` | `TAB_ATELIER_AGENT_PROBE` | per-tick CPU/RSS/ctxsw sampler |
| `reap` | `TAB_ATELIER_AGENT_REAP` | kill leaked agent ghosts on desktop startup |

Resolution: env var wins, then persisted flag, then off. Run the CLI as
the daemon's user so it lands in the daemon's state dir, e.g.
`sudo -u tab-atelier tab-atelier-headless flags probe on`.

## 1. Sampler (`src/agent_probe.rs`)

Driven from the app's 2 s persist tick. For every agent tab it walks the
agent's `/proc` subtree (the PTY child *is* `claude`, since agent tabs
`exec claude`) and appends one JSON object per tick to:

```
<state>/agent_probe_tab-<name>.jsonl
```

where `<state>` is `$XDG_STATE_HOME/tab-atelier` (`~/.local/state/tab-atelier`,
or `/var/lib/tab-atelier` for the headless service user). The file is
append-only and rotates to `.jsonl.1` past 4 MiB.

### Line schema (`ProbeLine`) — the tap contract

| field          | meaning                                                            |
|----------------|-------------------------------------------------------------------|
| `ts`           | wall-clock sample time, unix seconds (fractional)                 |
| `dt`           | seconds since the previous sample (the delta window)              |
| `tab`          | sanitised tab name (matches the filename)                         |
| `pid`          | agent process (== PTY child)                                      |
| `state`        | `"idle"` (no descendant on-CPU) or `"working"` (a tool is running)|
| `cpu_pct`      | CPU over `dt`, % of one core (may exceed 100 across threads)       |
| `rss_mb`       | resident memory of the subtree, MiB                               |
| `threads`      | live threads across the subtree                                   |
| `procs`        | processes in the subtree                                          |
| `ctxsw_per_s`  | total context switches/s — **high at `idle` ⇒ a wakeup/poll loop**|
| `vol_per_s`    | voluntary switches/s (blocking waits: sleeps, I/O)                |
| `nonvol_per_s` | non-voluntary switches/s (CPU pre-emption)                        |

The headline "busy while sleeping" signals are `cpu_pct` and
`ctxsw_per_s` recorded while `state == "idle"`.

### Tapping in

```rust
use tab_atelier::agent_probe;
let base = agent_probe::state_base();
for tab in agent_probe::list_logs(&base) {
    if let Some(last) = agent_probe::read_latest(&base, &tab) {
        println!("{tab}: {:.1}% cpu, {:.0} ctxsw/s ({})", last.cpu_pct, last.ctxsw_per_s, last.state);
    }
    let _timeline = agent_probe::read_all(&base, &tab); // full history
}
```

Or from a shell: `tail -f ~/.local/state/tab-atelier/agent_probe_tab-*.jsonl | jq .`

### Enable

Off by default; `tab-atelier flags probe on` (or `TAB_ATELIER_AGENT_PROBE=1`).

## 2. Tracer (launch wrap)

`agent_launch_shell_suffix_instrumented` (used by every agent
launch/restore/respawn) wraps the agent under `strace -f -c` so the whole
session accumulates a **syscall histogram**, written to:

```
<state>/agent_trace_<kind>_<session>.txt
```

`strace -c` counts calls/time/errors without per-call logging (low
overhead vs. full tracing). The summary flushes **when the agent exits**
(e.g. you close the tab), so it captures the cumulative cost of a mostly
idle session — the syscalls a sleeping agent spins on.

### Enable / override

- Off by default; `tab-atelier flags trace on` (or `TAB_ATELIER_AGENT_TRACE=1`).
- `TAB_ATELIER_AGENT_TRACE=/path/to/tracer` uses a different tracer
  command instead of `strace` (and enables it).
- If no tracer is found on `PATH`, the agent launches bare (no failure).

## Related: GUI log access

The desktop app has no controlling terminal, so `log` records are
normally dropped. `init_gui_file_logging()` routes them to
`<state>/tab-atelier.log` when `TAB_ATELIER_LOG` (or `RUST_LOG`) is set —
e.g. `TAB_ATELIER_LOG=tab_atelier::input_lag=trace` persists the
keystroke trace (logs `key`/`key_char` for every key event, IME
included) for diagnosing input bugs without needing a terminal.

The `tab-atelier log input` CLI shortcut sets exactly that filter (it's
named after the trace target — it captures *all* input, not just IME
composition). `tab-atelier log off` disables; `tab-atelier log` shows the
current filter and log-file path.
