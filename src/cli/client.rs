// SPDX-License-Identifier: MPL-2.0

//! The one shared "client subcommand" router for BOTH editions.
//!
//! A *client* subcommand is one that only talks to the local HTTP API (or
//! does a self-contained local computation) — no gpui, no daemon launch. Both
//! binaries need the exact same set, and they used to declare it twice: the
//! headless daemon via `clap` ([`super::dispatch`]) and the GUI via a
//! hand-rolled `match` in `src/main.rs`. That split is why the two drifted —
//! a command added to one was missing from the other.
//!
//! [`dispatch`] is now the single place that maps a subcommand *name* to its
//! handler:
//! - **GUI** (`src/main.rs`) calls it directly on `argv[1]`; `None` means "not
//!   a client subcommand" so it falls through to launching the app.
//! - **headless** ([`super::dispatch`]) keeps its `clap` enum (for typed
//!   `--help` and validation) but its match arms reconstruct the `&[String]`
//!   form and forward here, so the name→handler wiring lives in one spot. The
//!   only arms that stay clap-native (absent here) are the ones with no raw
//!   `&[String]` handler and no GUI equivalent: `net-allow`, `net-stats`,
//!   `net-dns`, `net-default` (headless-only nftables) and `limit` (typed).
//!
//! Adding a client command is now one arm here (+ one `clap` variant in
//! `dispatch` if headless should list it in `--help`).

use std::time::Duration;

use super::{bench, bench_lag, brain, claude_hook, delegate, flags, logging, remote};
use super::{set_context, set_font, set_meta, set_status, share_link, team, tokens, upgrade};

// ── the local API endpoint ──────────────────────────────────────────
//
// Every client subcommand talks to the same local HTTP API, so they all need
// the same two things: where it is, and the token to present. This used to
// live in `share_link` and be imported from there by nine modules — while
// `set-status`, `set-context` and `set-meta` each re-derived it by hand from
// the environment alone, and so failed on any machine where the daemon runs
// as a service and the token is in a file. That is a discovery rule with a
// hole in it, not three little duplications.

#[derive(Debug, Clone)]
pub(crate) struct Endpoint {
    pub(crate) url: String,
    pub(crate) token: String,
}

/// Endpoint injected by the CLI tests, which point the verbs at an
/// in-process API server. Set through [`set_test_endpoint`]; `None` in every
/// other build, where discovery goes through env + the token files below.
#[cfg(test)]
static TEST_ENDPOINT: std::sync::Mutex<Option<Endpoint>> = std::sync::Mutex::new(None);

/// Point every verb at `ep` (or back at real discovery with `None`).
#[cfg(test)]
pub(crate) fn set_test_endpoint(ep: Option<Endpoint>) {
    *TEST_ENDPOINT.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = ep;
}

/// URL a daemon bound, read from the `api.url` the server writes beside its
/// token; [`DEFAULT_LOOPBACK_URL`] when absent (an older daemon, or one that
/// couldn't write its state dir).
///
/// Pairing the URL with the token file we just matched matters: the two must
/// describe the SAME instance, or we authenticate against one daemon with
/// another's credential and get a 401 that blames the token.
fn endpoint_url_beside(token_path: &std::path::Path) -> String {
    token_path
        .parent()
        .map(|d| d.join("api.url"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_LOOPBACK_URL.to_owned())
}

/// Where a daemon lives unless it published otherwise.
pub(crate) const DEFAULT_LOOPBACK_URL: &str = "http://127.0.0.1:7890";

pub(crate) fn discover_endpoint() -> Result<Endpoint, String> {
    #[cfg(test)]
    {
        let injected = TEST_ENDPOINT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(ep) = injected {
            return Ok(ep);
        }
    }
    if let (Ok(url), Ok(token)) = (
        std::env::var("TAB_ATELIER_API_URL"),
        std::env::var("TAB_ATELIER_API_TOKEN"),
    ) {
        return Ok(Endpoint { url, token });
    }
    // Order matters: the system-service install runs under
    // HOME=/var/lib/tab-atelier so XDG_STATE_HOME resolves to
    // `/var/lib/tab-atelier/.local/state`. Check that path FIRST so
    // a stale per-user token (left over from a direct
    // `tab-atelier-headless` invocation as root) doesn't trump the
    // live daemon's token. Per-user comes after for non-service installs.
    let candidates = [
        std::path::PathBuf::from("/var/lib/tab-atelier/.local/state/tab-atelier/api.token"),
        std::path::PathBuf::from("/var/lib/tab-atelier/api.token"),
        crate::platform::state_base_dir().join("tab-atelier").join("api.token"),
    ];
    let mut tried = Vec::new();
    for path in &candidates {
        tried.push(path.display().to_string());
        if let Ok(t) = std::fs::read_to_string(path) {
            let token = t.trim().to_string();
            if !token.is_empty() {
                return Ok(Endpoint {
                    url: endpoint_url_beside(path),
                    token,
                });
            }
        }
    }
    Err(format!("no api.token found (tried env vars + {})", tried.join(", ")))
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into()
}

/// An authenticated request builder, for callers that must inspect the raw
/// response.
///
/// Several verbs match on a specific upstream status — `412` means the daemon
/// has no bubblewrap, `501` means per-tab ssh-agent needs the headless
/// edition — and turn it into a sentence a person can act on. A helper that
/// flattened everything to `Result<_, String>` would throw that away, so
/// these keep the `ureq` error and take only the header from here.
pub(crate) fn authed_post(ep: &Endpoint, path: &str) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
    agent()
        .post(format!("{}{path}", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
}

/// The `GET` counterpart of [`authed_post`].
pub(crate) fn authed_get(ep: &Endpoint, path: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    agent()
        .get(format!("{}{path}", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
}

/// A `GET` returning parsed JSON.
///
/// # Errors
/// The request failed, or the body was not JSON.
pub(crate) fn api_get_json(ep: &Endpoint, path: &str) -> Result<serde_json::Value, String> {
    agent()
        .get(format!("{}{path}", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .call()
        .map_err(|e| format!("GET {path}: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("parse {path}: {e}"))
}

/// A `DELETE`, for the routes that remove something.
///
/// # Errors
/// The request failed or the daemon refused it.
pub(crate) fn api_delete(ep: &Endpoint, path: &str) -> Result<(), String> {
    agent()
        .delete(format!("{}{path}", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .call()
        .map(|_| ())
        .map_err(|e| format!("DELETE {path}: {e}"))
}

/// A `POST` to the local API, authenticated.
///
/// Takes an already-discovered endpoint rather than discovering one, because
/// the callers must tell "there is no daemon here" (a silent no-op, so a
/// shell hook outside a tab never blocks) apart from "the POST failed" (a
/// real error worth reporting). Collapsing the two would make one of them
/// lie.
///
/// The bearer header was hand-built at three call sites and imported from a
/// fourth; a typo there is a 401 that blames the token, so it is written once.
///
/// # Errors
/// The request failed or the daemon refused it.
pub(crate) fn api_post_to(ep: &Endpoint, path: &str, body: String) -> Result<(), String> {
    agent()
        .post(format!("{}{path}", ep.url))
        .header("Authorization", format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json")
        .send(body)
        .map(|_| ())
        .map_err(|e| format!("POST {path}: {e}"))
}

/// Dispatch a client subcommand by name against raw `&[String]` args.
///
/// Returns `Some(exit_code)` when `name` is a recognized client subcommand
/// (the handler ran), or `None` when it isn't — the caller then does whatever
/// "not a subcommand" means for it (GUI: launch the app; headless: let `clap`
/// report the unknown command).
#[must_use]
pub fn dispatch(name: &str, rest: &[String]) -> Option<i32> {
    let code = match name {
        // Agent state / identity plumbing.
        "set-status" => set_status::run(rest),
        "set-font" => set_font::run(rest),
        "set-context" => set_context::run(rest),
        "set-meta" => set_meta::run(rest),
        "style" => super::style::run(rest),
        "token" => tokens::show(rest),
        "rotate-tokens" => tokens::rotate(rest),
        "reset-master-token" => tokens::reset_master(rest),
        // Hot-swap the running instance onto the newly installed binary,
        // keeping every tab's shell alive across the exec.
        "upgrade" => upgrade::run(rest),
        "claude-hook" => claude_hook::run(rest),
        // Orchestration / teamwork.
        "dispatch" => delegate::run(rest),
        "remote" => remote::run(rest),
        "brain" => brain::run(rest),
        "schedule" => share_link::schedule(rest),
        "log" => logging::run(rest),
        "logs" => super::logs::run(rest),
        "flags" => flags::run(rest),
        // `tabs`/`list` share the richer `share_link::tabs` (full-UUID +
        // lock/viewer status + `--json`) across both editions, replacing the
        // GUI's old bare `team::tabs`.
        "tabs" | "list" => share_link::tabs(rest),
        "peers" => team::peers(rest.iter().any(|a| a == "--all")),
        "peek" => team::run_peek(rest),
        // Self-organising fleet: contract net over the blackboard, plus the
        // lease that keeps two agents off the same task.
        "announce" => super::work::announce(rest),
        "bid" => super::work::bid(rest),
        "award" => super::work::award(rest),
        "take" => super::work::take(rest),
        "done" => super::work::done(rest),
        "tasks" => super::work::tasks(rest),
        "wait" => super::await_task::run(rest),
        "fleet" => super::work::fleet(rest),
        "brief" => super::brief::run(rest),
        "gossip" => super::gossip::run(rest),
        "prune" => super::prune::run(rest),
        "backlog" => super::backlog::run(rest),
        "note" => team::run_note(rest),
        "notes" => team::run_notes(rest),
        "handoff" => team::run_handoff(rest),
        // Per-tab resource caps (cgroup v2).
        "limit" => share_link::limit_cli(rest),
        // Plain tab commands (POST to the local API).
        "add" => share_link::add(rest),
        "close" => share_link::close(rest),
        "rename" => share_link::rename(rest),
        "lock" => share_link::lock(rest),
        "unlock" => share_link::unlock(rest),
        "input" => share_link::send_input(rest),
        "output" => share_link::output(rest),
        "stats" => share_link::stats_cli(rest),
        "share-link" => share_link::run(rest),
        "bg-color" => share_link::bg_color(rest),
        // Airgap toggle — the one network control the GUI also enforces (netns
        // respawn); the allowlist/resolver commands are headless-only and live
        // in `dispatch`, not here.
        "net-off" => share_link::net_off(rest),
        "net-on" => share_link::net_on(rest),
        // Host config (writes preferences.json) + local self-tests.
        "settings" | "ports" => share_link::ports(rest),
        "bench" => bench::run(rest),
        "bench-lag" => bench_lag::run_cli(rest),
        _ => return None,
    };
    Some(code)
}

/// [`dispatch`] for a caller that KNOWS `name` is a shared client subcommand.
///
/// The headless `clap` arms forward here after typed parsing. A `None` return
/// would mean the two lists fell out of sync (a programmer error), so it
/// surfaces loudly and exits 2 rather than silently misbehaving.
#[must_use]
pub fn run(name: &str, rest: &[String]) -> i32 {
    dispatch(name, rest).unwrap_or_else(|| {
        eprintln!("internal error: '{name}' is not in the shared client dispatch table");
        2
    })
}

#[cfg(test)]
mod tests {
    /// No verb may re-derive the API endpoint from the environment.
    ///
    /// `set-status`, `set-context` and `set-meta` each did, reading
    /// `TAB_ATELIER_API_URL`/`TAB_ATELIER_API_TOKEN` and returning 0 — silent
    /// success — when they were unset. On a machine where the daemon runs as
    /// a service the vars are not exported and the token lives in a file, so
    /// those three did nothing at all while `share-link` worked fine.
    ///
    /// Checked by reading the sources rather than by behaviour, deliberately:
    /// a behavioural test would have to unset process-wide environment
    /// variables to be meaningful, which is both unsound under parallel test
    /// execution and forbidden here (`unsafe_code`). It also silently passes
    /// when the developer happens to be running inside a tab-atelier tab,
    /// where those vars ARE set — which is exactly how this went unnoticed.
    #[test]
    fn no_verb_reads_the_api_endpoint_out_of_the_environment() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli");
        let mut offenders = Vec::new();
        let mut checked = 0;
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                // This module is where the reading is SUPPOSED to happen.
                if path.file_name().is_some_and(|f| f == "client.rs") {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                checked += 1;
                for var in ["TAB_ATELIER_API_URL", "TAB_ATELIER_API_TOKEN"] {
                    if src.contains(&format!("env::var(\"{var}\")")) {
                        offenders.push(format!("{} reads {var}", path.display()));
                    }
                }
            }
        }
        assert!(checked > 10, "only scanned {checked} cli modules — did they move?");
        assert!(
            offenders.is_empty(),
            "these bypass `discover_endpoint()`, so they silently no-op against a daemon \
             whose token is in a file rather than the environment:\n  {}",
            offenders.join("\n  ")
        );
    }

    #[test]
    fn discovery_follows_the_port_the_daemon_published() {
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("api.token");
        std::fs::write(&token, "0123456789abcdef0123456789abcdef").unwrap();
        // No api.url (an older daemon): fall back to the documented default
        // rather than refusing to talk to it at all.
        assert_eq!(super::endpoint_url_beside(&token), super::DEFAULT_LOOPBACK_URL);
        // Published: follow it, or a daemon on a non-default port gets a token
        // meant for it sent to whatever holds 7890 — a 401 that reads like a
        // credential problem.
        std::fs::write(dir.path().join("api.url"), "http://127.0.0.1:7899\n").unwrap();
        assert_eq!(super::endpoint_url_beside(&token), "http://127.0.0.1:7899");
        // An empty/blank file is treated as absent, not as an empty URL.
        std::fs::write(dir.path().join("api.url"), "  \n").unwrap();
        assert_eq!(super::endpoint_url_beside(&token), super::DEFAULT_LOOPBACK_URL);
    }
}
