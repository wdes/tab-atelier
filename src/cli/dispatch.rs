// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Top-level clap dispatcher for `tab-atelier-headless`.
//!
//! Every subcommand the binary surfaces (`add`, `close`, `rename`,
//! `lock`, `unlock`, `input`, `output`, `share-link`, `settings`,
//! `bg-color`, `set-status`, `set-context`, `claude-hook`, `remote`,
//! `brain`, `schedule`) becomes a typed variant on the `Commands` enum. The match arm
//! in `dispatch()` reconstructs the legacy `Vec<String>` form each
//! inner CLI module already accepts, so this is a thin top-level
//! layer — the actual argument validation / HTTP calls live in
//! `src/cli/{share_link,tabs,remote,set_status,claude_hook}.rs`.
//!
//! Wins over the previous hand-rolled `match args[0].as_str()`:
//! - `tab-atelier-headless --help` lists every subcommand
//!   with one-line descriptions.
//! - `tab-atelier-headless <cmd> --help` shows that subcommand's
//!   flags + positional args.
//! - Unknown subcommand / missing positional → clap's standard
//!   error message + exit 2, never falls through to the daemon
//!   launch path (which used to silently start the server on a
//!   typo).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "tab-atelier-headless",
    version,
    about = "tab-atelier headless daemon + CLI",
    long_about = "Run with no subcommand to start the daemon. \
                  Subcommands talk to a running daemon via its local \
                  HTTP API (token + URL discovered from env / the \
                  service's state directory)."
)]
pub struct Cli {
    /// Start the daemon in read-only mode (no state changes
    /// persisted). Only meaningful when no subcommand is given.
    #[arg(long, global = true)]
    pub read_only: bool,

    /// Probe that rustls + crypto provider build cleanly, then exit.
    /// Used by CI; no daemon work happens.
    #[arg(long, global = true, hide = true)]
    pub check_crypto: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Create a new tab rooted at `<path>`, optionally renamed.
    Add {
        /// Working directory for the new tab.
        path: PathBuf,
        /// Optional name (defaults to the basename of `<path>`).
        name: Option<String>,
    },

    /// Close a tab.
    Close {
        /// Tab index or UUID.
        tab: String,
    },

    /// Rename a tab.
    Rename {
        /// Tab index or UUID.
        tab: String,
        /// New name.
        name: String,
    },

    /// Lock a tab — refuse every input source (API, GUI, share links).
    Lock {
        /// Tab index or UUID.
        tab: String,
    },

    /// Unlock a previously locked tab.
    Unlock {
        /// Tab index or UUID.
        tab: String,
    },

    /// Cut a tab's internet — respawn its shell inside a bubblewrap
    /// network namespace (loopback only). Needs `bubblewrap` installed.
    #[command(name = "net-off")]
    NetOff {
        /// Tab index or UUID.
        tab: String,
    },

    /// Restore a tab's internet — respawn its shell without the netns jail.
    #[command(name = "net-on")]
    NetOn {
        /// Tab index or UUID.
        tab: String,
    },

    /// Restrict a tab to an allowlist of destinations, enforced by per-tab
    /// nftables. Repeat `--preset`/`--domain`/`--cidr` to add entries;
    /// `--clear` removes the allowlist (tab returns to unrestricted).
    #[command(name = "net-allow")]
    NetAllow {
        /// Tab index or UUID.
        tab: String,
        /// Preset id (`claude-code`, `cloudflare`). Repeatable.
        #[arg(long = "preset")]
        presets: Vec<String>,
        /// Domain suffix to allow (e.g. `api.anthropic.com`, `*.example.com`). Repeatable.
        #[arg(long = "domain")]
        domains: Vec<String>,
        /// CIDR / IP to allow (e.g. `104.16.0.0/13`). Repeatable.
        #[arg(long = "cidr")]
        cidrs: Vec<String>,
        /// Add the given entries to the tab's CURRENT allowlist (merge).
        #[arg(long, conflicts_with_all = ["remove", "clear"])]
        add: bool,
        /// Remove the given entries from the tab's current allowlist.
        #[arg(long, conflicts_with_all = ["add", "clear"])]
        remove: bool,
        /// Clear the allowlist (tab returns to unrestricted internet).
        #[arg(long)]
        clear: bool,
    },

    /// Show per-tab network metering (connections + egress bytes). Optional
    /// tab index/UUID to show just one.
    #[command(name = "net-stats")]
    NetStats {
        /// Tab index or UUID. Omit for all tabs.
        tab: Option<String>,
    },

    /// Show a domain-allowlist tab's resolver DNS log (allowed + DENIED
    /// queries with resolved IPs). Optional tab index/UUID.
    #[command(name = "net-dns")]
    NetDns {
        /// Tab index or UUID. Omit for all tabs.
        tab: Option<String>,
        /// Show only DENIED queries (what a tab tried to reach and couldn't).
        #[arg(long)]
        denied: bool,
    },

    /// Set the default allowlist applied to NEW tabs (written to
    /// preferences.json; applies to tabs created after the daemon restarts).
    /// `--clear` removes the default. Same flags as `net-allow`.
    #[command(name = "net-default")]
    NetDefault {
        #[arg(long = "preset")]
        presets: Vec<String>,
        #[arg(long = "domain")]
        domains: Vec<String>,
        #[arg(long = "cidr")]
        cidrs: Vec<String>,
        #[arg(long)]
        clear: bool,
    },

    /// Send keystrokes to a tab (`\n` / `\r` / `\t` / `\\` escapes interpreted).
    Input {
        /// Tab index or UUID.
        tab: String,
        /// Keystrokes to send. No trailing newline added — pass `\n` to run a command.
        text: String,
    },

    /// Print the tab's current scrollback to stdout.
    Output {
        /// Tab index or UUID.
        tab: String,
    },

    /// Print a browser URL for the xterm.js viewer.
    ShareLink {
        /// Tab index or UUID.
        tab: String,
        /// Read-only share link (recipient cannot type).
        #[arg(long, short)]
        ro: bool,
    },

    /// Set the per-tab viewer background color (or the global default with `--global`).
    ///
    /// Note: `--tab` is a flag (not a positional) so clap can model the
    /// mutual exclusion with `--global` at parse time without falling
    /// foul of "optional positional before required positional".
    BgColor {
        /// Apply to the global default in preferences.json instead of one tab.
        #[arg(long, short = 'g', conflicts_with = "tab")]
        global: bool,
        /// Tab index or UUID. Required unless `--global`.
        #[arg(long, short = 't', required_unless_present = "global", conflicts_with = "global")]
        tab: Option<String>,
        /// Color as `#RRGGBB`, or `clear` to remove an existing override.
        color: String,
    },

    /// Show or edit daemon settings (preferences.json).
    Settings {
        /// `addr:port` for the plain HTTP API listener.
        #[arg(long)]
        api_addr: Option<String>,
        /// `addr:port` for the TLS API listener.
        #[arg(long)]
        api_tls_addr: Option<String>,
        /// Public base URL for share links (empty string clears).
        #[arg(long)]
        share_url_base: Option<String>,
        /// Default PTY column count for new tabs (>= 4).
        #[arg(long)]
        pty_cols: Option<u16>,
        /// Default PTY row count for new tabs (>= 4).
        #[arg(long)]
        pty_rows: Option<u16>,
        /// Default tab background color (`#RRGGBB` or `clear`).
        #[arg(long)]
        bg_color: Option<String>,
    },

    /// Publish an agent state for the current tab.
    ///
    /// Used by Claude Code / catbus-agent hooks. Silently no-ops outside a tab-atelier tab.
    SetStatus {
        /// `idle`, `thinking`, `waiting`, or `error`.
        state: String,
        /// Optional human-readable status label.
        #[arg(long)]
        label: Option<String>,
        /// Durable agent session UUID.
        #[arg(long)]
        session: Option<String>,
        /// Agent CLI kind (`catbus`, `claude`, …).
        #[arg(long)]
        kind: Option<String>,
        /// Agent is in plan / read-only mode.
        #[arg(long, conflicts_with = "no_plan")]
        plan: bool,
        /// Agent is NOT in plan mode (clears the flag).
        #[arg(long)]
        no_plan: bool,
    },

    /// Label the current tab with what it's working on (PR/issue/task).
    ///
    /// Shows as a hover tooltip on the GUI tab name and on `/tabs`.
    /// `--tab <id>` targets another tab; `--clear` removes the label.
    SetContext {
        /// Passed straight through to `cli::set_context::run`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Print the master API token, so the local API can be called
    /// without locating the `api.token` state file.
    Token,

    /// Revoke every tab's per-tab share tokens — all outstanding share
    /// links 401; a fresh token is minted on the next share.
    RotateTokens,

    /// Hot-swap the master API token. Every client/link carrying the old
    /// token 401s; the new token is written to `api.token`.
    ResetMasterToken,

    /// Bridge a Claude Code hook event to set-status. Reads JSON from stdin.
    ClaudeHook {
        /// Event name (`session-start`, `pre-tool`, …).
        event: String,
    },

    /// Talk to a remote tab-atelier (HTTPS API of a saved `RemoteEndpoint`).
    Remote {
        /// Passed straight through to `cli::remote::run`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Hand work to another tab's agent (or a fresh `--new` tab) and
    /// optionally `--wait` for it to go idle and report back.
    Dispatch {
        /// Passed straight through to `cli::delegate::run`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// `⛑ brain` — watch every tab for known agent-failure signatures
    /// (Anthropic API unreachable, 5xx, etc.) and auto-send `continue`
    /// to stuck agents. Best run as its own tab.
    Brain {
        /// Scan once and exit (instead of looping forever).
        #[arg(long)]
        once: bool,
        /// Seconds between scans. Default 5.
        #[arg(long)]
        interval: Option<u64>,
    },

    /// Off-hours auto-lock — OSM `opening_hours` rule + IANA timezone.
    ///
    /// Set: `schedule <tab> "Mo-Fr 09:00-18:00" --tz Europe/Paris`.
    /// Clear: `schedule <tab> --clear`. Outside the rule's open windows
    /// every write (input, inbox upload, manual unlock) is refused
    /// with 423; viewers see the lock reason in the `X-Tab-Locked-
    /// Reason: schedule` header.
    Schedule {
        /// Tab index or UUID.
        tab: String,
        /// OSM `opening_hours` rule (e.g. `Mo-Fr 09:00-18:00`,
        /// `Mo-Fr 09:00-12:30,13:30-18:00; PH off`, `24/7`).
        /// Required unless `--clear`.
        rule: Option<String>,
        /// IANA timezone name (e.g. `Europe/Paris`, `America/New_York`,
        /// `Asia/Tokyo`, `UTC`). Required unless `--clear`.
        #[arg(long, conflicts_with = "clear")]
        tz: Option<String>,
        /// Drop the schedule on this tab — tab returns to always-open
        /// (unless still manually locked).
        #[arg(long, conflicts_with = "rule")]
        clear: bool,
    },

    /// List tabs with their lock status (`open`, `locked (manual)`,
    /// `locked (schedule)`) — quick read of the running headless's
    /// state without poking through the JSON API by hand.
    Tabs {
        /// Dump the raw `/tabs` JSON instead of the formatted table —
        /// for scripts that want to consume the full payload.
        #[arg(long)]
        json: bool,
    },

    /// Toggle agent-instrumentation flags (persisted to flags.json,
    /// applied on next agent launch / daemon restart) — so a systemd
    /// daemon needn't have env vars edited. `flags` shows all;
    /// `flags frame-timing on`, `flags trace off`, `flags probe default`.
    /// Env vars still win when set.
    Flags {
        /// `<name> <on|off|default>`, or empty to show all flags.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Enable/disable the file logger from the shell (persisted, applied
    /// on next start). `log input` traces every keystroke (IME included),
    /// `log off` disables, `log <filter>` sets any `env_logger` filter.
    /// Env vars (`TAB_ATELIER_LOG` / `RUST_LOG`) still win when set.
    Log {
        /// `input` | `on` | `off` | a raw `env_logger` filter (may be
        /// several words). Omit to print the current filter + log paths.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Terminal throughput self-test — drains vtebench-style payloads
    /// through the `PtyRing` + alacritty parser and reports MiB/s.
    /// Measures PTY-read/parse only (not paint or typing latency).
    /// No display or running daemon required.
    Bench {
        /// Payload size per case in MiB (default 64).
        #[arg(long)]
        mb: Option<usize>,
        /// Repeat each case N times, report the best (default 3).
        #[arg(long)]
        iterations: Option<usize>,
        /// Grid columns (default 200).
        #[arg(long)]
        cols: Option<usize>,
        /// Grid rows (default 50).
        #[arg(long)]
        rows: Option<usize>,
    },

    /// Input-lag self-test — connects to a running tab's web-viewer
    /// WebSocket and measures the keystroke→echo round-trip (sends a
    /// benign `x`, times the echoed output, then erases it). Reports
    /// min / median / p95 / mean. Needs a running daemon.
    ///
    /// Pass the viewer URL you'd open in a browser — the
    /// `<base>/tabs/by-id/<uuid>/view?token=<tok>` link. Point it at
    /// `127.0.0.1` to isolate the server-side tick floor, or a remote
    /// host to include real network latency.
    BenchLag {
        /// Viewer URL (`…/view?token=…`) or a direct `ws…/ws?token=…`.
        url: String,
        /// Number of keystroke samples to time (default 25).
        #[arg(long)]
        samples: Option<usize>,
    },

    /// List sibling tabs (teammates) — name, state, cwd, hover context — so a
    /// Claude can pick a collaborator or wait on one by name.
    Peers {
        /// Show every tab, not just Claude sessions.
        #[arg(long)]
        all: bool,
    },

    /// Post a message to the shared blackboard every tab can read (broadcast).
    Note {
        /// The message.
        msg: String,
        /// Optional channel, filtered by `notes --topic`.
        #[arg(long)]
        topic: Option<String>,
        /// Who's posting (your tab name); shown to readers.
        #[arg(long)]
        from: Option<String>,
    },

    /// Read the shared blackboard.
    Notes {
        /// Only this channel.
        #[arg(long)]
        topic: Option<String>,
        /// Skip notes before this index (for incremental polling).
        #[arg(long)]
        since: Option<usize>,
    },

    /// Copy a file into a peer tab's `inbox/` so its agent can pick it up.
    Handoff {
        /// File to hand off.
        file: PathBuf,
        /// Target tab — name, index, or UUID.
        tab: String,
    },

    /// Read a peer tab's current screen (ANSI-stripped, last N lines).
    Peek {
        /// Target tab — name, index, or UUID.
        tab: String,
        /// How many trailing lines to show (default 40).
        #[arg(long)]
        lines: Option<usize>,
        /// Keep ANSI escapes instead of stripping them.
        #[arg(long)]
        raw: bool,
    },
}

/// Returns true iff a subcommand was dispatched (caller should not
/// fall through to the daemon-launch path). Process exit happens
/// inside the dispatched subcommand for code-path consistency.
#[must_use]
pub fn dispatch(cli: Cli) -> bool {
    let Some(cmd) = cli.command else {
        return false;
    };
    let code = match cmd {
        Commands::Add { path, name } => {
            let mut args = vec![path.to_string_lossy().into_owned()];
            if let Some(n) = name {
                args.push(n);
            }
            crate::cli::share_link::add(&args)
        }
        Commands::Close { tab } => crate::cli::share_link::close(&[tab]),
        Commands::Rename { tab, name } => crate::cli::share_link::rename(&[tab, name]),
        Commands::Lock { tab } => crate::cli::share_link::lock(&[tab]),
        Commands::Unlock { tab } => crate::cli::share_link::unlock(&[tab]),
        Commands::NetOff { tab } => crate::cli::share_link::net_off(&[tab]),
        Commands::NetOn { tab } => crate::cli::share_link::net_on(&[tab]),
        Commands::NetAllow {
            tab,
            presets,
            domains,
            cidrs,
            add,
            remove,
            clear,
        } => crate::cli::share_link::net_allow(&tab, &presets, &domains, &cidrs, clear, add, remove),
        Commands::NetStats { tab } => crate::cli::share_link::net_stats(tab.as_deref()),
        Commands::NetDns { tab, denied } => crate::cli::share_link::net_dns(tab.as_deref(), denied),
        Commands::NetDefault {
            presets,
            domains,
            cidrs,
            clear,
        } => crate::cli::share_link::net_default(&presets, &domains, &cidrs, clear),
        Commands::Input { tab, text } => crate::cli::share_link::send_input(&[tab, text]),
        Commands::Output { tab } => crate::cli::share_link::output(&[tab]),
        Commands::ShareLink { tab, ro } => {
            let mut args = vec![tab];
            if ro {
                args.push("--ro".into());
            }
            crate::cli::share_link::run(&args)
        }
        Commands::BgColor { global, tab, color } => {
            let mut args: Vec<String> = Vec::new();
            if global {
                args.push("--global".into());
                args.push(color);
            } else if let Some(tab) = tab {
                args.push(tab);
                args.push(color);
            }
            crate::cli::share_link::bg_color(&args)
        }
        Commands::Settings {
            api_addr,
            api_tls_addr,
            share_url_base,
            pty_cols,
            pty_rows,
            bg_color,
        } => {
            let mut args: Vec<String> = Vec::new();
            if let Some(v) = api_addr {
                args.push("--api-addr".into());
                args.push(v);
            }
            if let Some(v) = api_tls_addr {
                args.push("--api-tls-addr".into());
                args.push(v);
            }
            if let Some(v) = share_url_base {
                args.push("--share-url-base".into());
                args.push(v);
            }
            if let Some(v) = pty_cols {
                args.push("--pty-cols".into());
                args.push(v.to_string());
            }
            if let Some(v) = pty_rows {
                args.push("--pty-rows".into());
                args.push(v.to_string());
            }
            if let Some(v) = bg_color {
                args.push("--bg-color".into());
                args.push(v);
            }
            crate::cli::share_link::ports(&args)
        }
        Commands::SetStatus {
            state,
            label,
            session,
            kind,
            plan,
            no_plan,
        } => {
            let mut args = vec![state];
            if let Some(v) = label {
                args.push("--label".into());
                args.push(v);
            }
            if let Some(v) = session {
                args.push("--session".into());
                args.push(v);
            }
            if let Some(v) = kind {
                args.push("--kind".into());
                args.push(v);
            }
            if plan {
                args.push("--plan".into());
            }
            if no_plan {
                args.push("--no-plan".into());
            }
            crate::cli::set_status::run(&args)
        }
        Commands::SetContext { args } => crate::cli::set_context::run(&args),
        Commands::Token => crate::cli::tokens::show(&[]),
        Commands::RotateTokens => crate::cli::tokens::rotate(&[]),
        Commands::ResetMasterToken => crate::cli::tokens::reset_master(&[]),
        Commands::ClaudeHook { event } => crate::cli::claude_hook::run(&[event]),
        Commands::Remote { args } => crate::cli::remote::run(&args),
        Commands::Dispatch { args } => crate::cli::delegate::run(&args),
        Commands::Peers { all } => crate::cli::team::peers(all),
        Commands::Note { msg, topic, from } => crate::cli::team::note(topic, from, &msg),
        Commands::Notes { topic, since } => crate::cli::team::notes(topic.as_deref(), since),
        Commands::Handoff { file, tab } => crate::cli::team::handoff(&file, &tab),
        Commands::Peek { tab, lines, raw } => crate::cli::team::peek(&tab, lines.unwrap_or(40), raw),
        Commands::Brain { once, interval } => {
            let mut args: Vec<String> = Vec::new();
            if once {
                args.push("--once".into());
            }
            if let Some(s) = interval {
                args.push("--interval".into());
                args.push(s.to_string());
            }
            crate::cli::brain::run(&args)
        }
        Commands::Schedule { tab, rule, tz, clear } => {
            let mut args: Vec<String> = vec![tab];
            if clear {
                args.push("--clear".into());
            } else if let Some(r) = rule {
                args.push(r);
                if let Some(z) = tz {
                    args.push("--tz".into());
                    args.push(z);
                }
            }
            crate::cli::share_link::schedule(&args)
        }
        Commands::Flags { args } => crate::cli::flags::run(&args),
        Commands::Log { args } => crate::cli::logging::run(&args),
        Commands::Tabs { json } => {
            let args = if json { vec!["--json".to_string()] } else { vec![] };
            crate::cli::share_link::tabs(&args)
        }
        Commands::Bench {
            mb,
            iterations,
            cols,
            rows,
        } => {
            let mut args: Vec<String> = Vec::new();
            if let Some(n) = mb {
                args.push("--mb".into());
                args.push(n.to_string());
            }
            if let Some(n) = iterations {
                args.push("--iterations".into());
                args.push(n.to_string());
            }
            if let Some(n) = cols {
                args.push("--cols".into());
                args.push(n.to_string());
            }
            if let Some(n) = rows {
                args.push("--rows".into());
                args.push(n.to_string());
            }
            crate::cli::bench::run(&args)
        }
        Commands::BenchLag { url, samples } => crate::cli::bench_lag::run(&url, samples.unwrap_or(25)),
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Catches accidental subcommand drift: every variant must keep
    /// parsing from a representative command line.
    #[test]
    fn each_subcommand_round_trips_through_parse_from() {
        let cases: Vec<(&[&str], &str)> = vec![
            (&["tab-atelier-headless"], "no subcommand"),
            (&["tab-atelier-headless", "add", "/tmp"], "add path"),
            (&["tab-atelier-headless", "add", "/tmp", "name"], "add path name"),
            (&["tab-atelier-headless", "close", "0"], "close idx"),
            (&["tab-atelier-headless", "rename", "0", "newname"], "rename"),
            (&["tab-atelier-headless", "lock", "0"], "lock"),
            (&["tab-atelier-headless", "unlock", "0"], "unlock"),
            (&["tab-atelier-headless", "input", "0", "ls"], "input"),
            (&["tab-atelier-headless", "output", "0"], "output"),
            (&["tab-atelier-headless", "share-link", "0"], "share-link"),
            (&["tab-atelier-headless", "share-link", "0", "--ro"], "share-link --ro"),
            (
                &["tab-atelier-headless", "bg-color", "--global", "#002451"],
                "bg-color global",
            ),
            (
                &["tab-atelier-headless", "bg-color", "--tab", "0", "#112233"],
                "bg-color --tab",
            ),
            (
                &["tab-atelier-headless", "bg-color", "--tab", "0", "clear"],
                "bg-color --tab clear",
            ),
            (&["tab-atelier-headless", "settings"], "settings (no flags)"),
            (
                &[
                    "tab-atelier-headless",
                    "settings",
                    "--bg-color",
                    "#111111",
                    "--pty-cols",
                    "200",
                ],
                "settings --flag",
            ),
            (&["tab-atelier-headless", "set-status", "thinking"], "set-status state"),
            (
                &["tab-atelier-headless", "set-status", "waiting", "--label", "tool: Bash"],
                "set-status with --label",
            ),
            (
                &["tab-atelier-headless", "claude-hook", "pre-tool"],
                "claude-hook event",
            ),
            (
                &[
                    "tab-atelier-headless",
                    "remote",
                    "add",
                    "--label",
                    "L",
                    "--url",
                    "U",
                    "--token",
                    "T",
                ],
                "remote pass-through",
            ),
            (
                &[
                    "tab-atelier-headless",
                    "schedule",
                    "0",
                    "Mo-Fr 09:00-18:00",
                    "--tz",
                    "Europe/Paris",
                ],
                "schedule set",
            ),
            (&["tab-atelier-headless", "schedule", "0", "--clear"], "schedule clear"),
        ];
        for (argv, label) in cases {
            let _ = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("parse failed for {label}: {e}"));
        }
    }

    /// `bg-color --global` and `bg-color --tab` must be mutually
    /// exclusive — clap enforces this via `conflicts_with`. Regression
    /// guard: removing the attribute would silently let both through
    /// and the dispatcher's branch would pick one arbitrarily.
    #[test]
    fn bg_color_global_and_tab_are_mutually_exclusive() {
        let err = Cli::try_parse_from(["tab-atelier-headless", "bg-color", "--global", "--tab", "0", "#111111"])
            .expect_err("must conflict");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot be used with") || msg.contains("conflict"),
            "expected conflict error, got: {msg}"
        );
    }

    /// `bg-color` needs either `--global` or a TAB. Bare colour alone
    /// should fail at parse time, not silently no-op in the dispatcher.
    #[test]
    fn bg_color_without_tab_or_global_fails() {
        let err = Cli::try_parse_from(["tab-atelier-headless", "bg-color", "#112233"]).expect_err("missing tab");
        let msg = err.to_string();
        assert!(
            msg.contains("required") || msg.contains("missing"),
            "expected required-arg error, got: {msg}"
        );
    }

    /// Unknown subcommands must error out — they used to silently
    /// fall through to the daemon launch path under the hand-rolled
    /// router, which booted the server on a typo.
    #[test]
    fn unknown_subcommand_errors() {
        let err = Cli::try_parse_from(["tab-atelier-headless", "nope-does-not-exist"]).expect_err("unknown subcommand");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::InvalidSubcommand,
            "expected InvalidSubcommand, got: {:?}",
            err.kind()
        );
    }

    /// The global flags propagate when given before OR after a
    /// subcommand. Tests that `global = true` on the arg attr works.
    #[test]
    fn read_only_works_globally() {
        let pre = Cli::try_parse_from(["tab-atelier-headless", "--read-only"]).unwrap();
        assert!(pre.read_only);
        assert!(pre.command.is_none());

        let mid = Cli::try_parse_from(["tab-atelier-headless", "lock", "0", "--read-only"]).unwrap();
        assert!(mid.read_only);
        assert!(matches!(mid.command, Some(Commands::Lock { .. })));
    }

    /// The clap `Command` builds without panicking — catches mistakes
    /// like duplicated arg names or conflicting `conflicts_with`
    /// targets that only surface at runtime otherwise.
    #[test]
    fn command_factory_builds() {
        let cmd = Cli::command();
        let names: Vec<&str> = cmd.get_subcommands().map(clap::Command::get_name).collect();
        for expected in [
            "add",
            "close",
            "rename",
            "lock",
            "unlock",
            "input",
            "output",
            "share-link",
            "bg-color",
            "settings",
            "set-status",
            "claude-hook",
            "remote",
            "schedule",
        ] {
            assert!(
                names.contains(&expected),
                "missing subcommand {expected}, got: {names:?}"
            );
        }
    }
}
