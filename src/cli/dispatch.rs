// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Top-level clap dispatcher for `tab-atelier-headless`.
//!
//! Every subcommand the binary surfaces (`tabs`, `add`, `close`,
//! `rename`, `lock`, `unlock`, `input`, `output`, `share-link`,
//! `settings`, `bg-color`, `set-status`, `claude-hook`, `remote`)
//! becomes a typed variant on the `Commands` enum. The match arm
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
    /// Live tab listing — ratatui-rendered like the desktop's bottom bar.
    Tabs {
        /// Print a single snapshot then exit (script-friendly).
        #[arg(long)]
        once: bool,
    },

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
    BgColor {
        /// Apply to the global default in preferences.json instead of one tab.
        #[arg(long, short = 'g', conflicts_with = "tab")]
        global: bool,
        /// Tab index or UUID. Required unless `--global`.
        #[arg(required_unless_present = "global")]
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
        /// `addr:port` for the bundled happier-relay.
        #[arg(long)]
        happier_relay_addr: Option<String>,
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
        Commands::Tabs { once } => {
            let mut args: Vec<String> = Vec::new();
            if once {
                args.push("--once".into());
            }
            crate::cli::tabs::run(&args)
        }
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
            happier_relay_addr,
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
            if let Some(v) = happier_relay_addr {
                args.push("--happier-relay-addr".into());
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
        Commands::ClaudeHook { event } => crate::cli::claude_hook::run(&[event]),
        Commands::Remote { args } => crate::cli::remote::run(&args),
    };
    std::process::exit(code);
}
