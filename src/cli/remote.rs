// SPDX-License-Identifier: MPL-2.0

//! `tab-atelier remote …` — CRUD + live test for `RemoteEndpoint`s.
//!
//! Endpoints are stored in `preferences.json`. The subcommands compose
//! the [`crate::remote::Client`] polling thread without dragging in
//! any gpui code, so they work identically from both the GUI and
//! headless binaries.
//!
//! Subcommands:
//!
//! ```text
//! tab-atelier remote list
//! tab-atelier remote add    --label L --url U --token T [--no-pin] [--autoconnect]
//!                           [--cf-id ID --cf-secret SECRET]   # Cloudflare Access
//! tab-atelier remote remove <label-or-id>
//! tab-atelier remote test   <label-or-id>            # one-shot tab list
//! tab-atelier remote watch  <label-or-id>            # follow until Ctrl-C
//! tab-atelier remote attach <label-or-id> <tab>      # interactive mirror (sidecar)
//! tab-atelier remote put    <label-or-id> <local-path> [--tab T]
//! tab-atelier remote get    <label-or-id> <remote-path> [--tab T] [-o local-path]
//! tab-atelier remote pin-cert <https-url>            # print fingerprint
//! tab-atelier remote re-pin <label-or-id>            # re-capture pinned cert
//! ```

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::{RemoteEndpoint, fetch_cert_fingerprint, load_preferences, platform, remote, save_preferences};

/// `tab-atelier remote …`, parsed by clap like the rest of the CLI.
///
/// This subtree used to hand-roll its argument parsing, one `while` loop per
/// verb. That is what shipped `remote add proxy --url …` in the docs: nothing
/// could reject a positional argument the parser did not accept, `--help`
/// answered "unknown argument: --help" one level down, and each verb's usage
/// text was a string literal maintained by hand next to the `match` it was
/// supposed to describe. clap derives all three from the same declaration.
#[derive(Parser, Debug)]
#[command(
    name = "tab-atelier remote",
    about = "Talk to a remote tab-atelier or a tab-atelier-proxy",
    disable_help_flag = false
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List configured endpoints.
    List,
    /// Print this instance's sidecar token, for the peer's `remote add --token`.
    MyToken,
    /// Persist a new endpoint.
    Add(AddArgs),
    /// Drop an endpoint.
    #[command(alias = "rm")]
    Remove {
        /// Label or id.
        endpoint: String,
    },
    /// Connect, list the remote's tabs, exit.
    Test {
        /// Label or id.
        endpoint: String,
    },
    /// Follow scrollback events until Ctrl-C.
    Watch {
        /// Label or id.
        endpoint: String,
    },
    /// Interactive mirror of one remote tab.
    Attach {
        /// Label or id.
        endpoint: String,
        /// Tab name, id or `#index`.
        tab: String,
    },
    /// Upload a file into the remote tab's `inbox/`.
    Put(PutArgs),
    /// Download a file from the remote tab.
    Get(GetArgs),
    /// Print an HTTPS endpoint's cert SHA-256 fingerprint.
    #[command(name = "pin-cert", alias = "pin")]
    PinCert {
        /// The `https://…` URL to inspect.
        url: String,
    },
    /// Re-capture an endpoint's pinned cert.
    #[command(name = "re-pin", alias = "repin")]
    RePin {
        /// Label or id.
        endpoint: String,
    },
}

#[derive(Args, Debug)]
pub struct AddArgs {
    /// Short name for the endpoint, used everywhere else in place of the id.
    #[arg(long)]
    label: String,
    /// Base URL, e.g. `https://host:7891`.
    #[arg(long)]
    url: String,
    /// The peer's MASTER token — tabs, input, files. Not needed for a proxy.
    #[arg(long)]
    token: Option<String>,
    /// The peer's `relay token`, or a tab-atelier-proxy key.
    ///
    /// A tab-atelier-proxy is not a peer tab-atelier and has none of the tab
    /// endpoints, so relaying through one needs this and nothing else.
    /// Requiring a master token there would mean inventing a value to satisfy
    /// the parser, which teaches people to put junk in a credential field.
    #[arg(long)]
    relay_token: Option<String>,
    /// Pin this fingerprint instead of capturing one.
    #[arg(long)]
    cert_sha256: Option<String>,
    /// Cloudflare Access service token id.
    #[arg(long)]
    cf_id: Option<String>,
    /// Cloudflare Access service token secret.
    #[arg(long)]
    cf_secret: Option<String>,
    /// Skip TLS certificate pinning.
    #[arg(long)]
    no_pin: bool,
    /// Connect to this endpoint at startup.
    #[arg(long)]
    autoconnect: bool,
}

#[derive(Args, Debug)]
pub struct PutArgs {
    /// Label or id.
    endpoint: String,
    /// The local file to upload.
    local_path: PathBuf,
    /// Which tab (name, id or `#index`); default the active one.
    #[arg(long)]
    tab: Option<String>,
    /// Store it under this name instead of the local basename.
    #[arg(long, alias = "remote-path")]
    remote_name: Option<String>,
}

#[derive(Args, Debug)]
pub struct GetArgs {
    /// Label or id.
    endpoint: String,
    /// Path on the remote. MUST start with `inbox/` or `outbox/`.
    remote_path: String,
    /// Which tab (name, id or `#index`); default the active one.
    #[arg(long)]
    tab: Option<String>,
    /// Write here instead of the remote basename.
    #[arg(short = 'o', long = "output")]
    local_out: Option<PathBuf>,
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    // clap expects argv[0]; the caller hands us only what followed `remote`.
    let argv = std::iter::once("tab-atelier remote".to_owned()).chain(args.iter().cloned());
    let cli = match Cli::try_parse_from(argv) {
        Ok(c) => c,
        Err(e) => {
            // `--help` and `--version` arrive as errors carrying the text to
            // print; they are successful outcomes and must not exit non-zero.
            let ok = matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            );
            let _ = e.print();
            return i32::from(!ok) * 2;
        }
    };
    match cli.cmd {
        Cmd::List => cmd_list(),
        // Printed here, pasted into the PEER's `remote add --token`. Scoped to
        // the sidecar's own operations, unlike the master token.
        Cmd::MyToken => {
            println!("{}", crate::remote_token());
            eprintln!("# sidecar credential for THIS instance — on the peer, run:");
            eprintln!("#   tab-atelier remote add --label <name> --url <this-url> --token <above>");
            0
        }
        Cmd::Add(a) => cmd_add(a),
        Cmd::Remove { endpoint } => cmd_remove(&endpoint),
        Cmd::Test { endpoint } => cmd_test(&endpoint, false),
        Cmd::Watch { endpoint } => cmd_test(&endpoint, true),
        Cmd::Attach { endpoint, tab } => attach::run(&endpoint, &tab),
        Cmd::Put(a) => files::cmd_put(a),
        Cmd::Get(a) => files::cmd_get(a),
        Cmd::PinCert { url } => cmd_pin_cert(&url),
        Cmd::RePin { endpoint } => cmd_repin(&endpoint),
    }
}

mod attach;
mod files;
pub(crate) mod resolver;

fn cmd_list() -> i32 {
    let prefs = load_preferences(&platform::config_dir());
    if prefs.remote_endpoints.is_empty() {
        println!("(no endpoints configured — add with `tab-atelier remote add ...`)");
        return 0;
    }
    println!(
        "{:<22} {:<32} {:<12} {:<3} cert sha256 (12)",
        "label", "url", "id (8)", "AC"
    );
    for ep in &prefs.remote_endpoints {
        let ac = if ep.autoconnect { "✓" } else { "" };
        println!(
            "{:<22} {:<32} {:<12} {:<3} {}",
            truncate(&ep.label, 22),
            truncate(&ep.url, 32),
            truncate(&ep.id, 12),
            ac,
            truncate(&ep.cert_sha256, 24),
        );
    }
    0
}

fn cmd_add(args: AddArgs) -> i32 {
    let AddArgs {
        label,
        url,
        token,
        relay_token,
        cert_sha256,
        cf_id,
        cf_secret,
        no_pin,
        autoconnect,
    } = args;
    if label.is_empty() || url.is_empty() {
        eprintln!("tab-atelier remote add: --label and --url cannot be empty");
        return 2;
    }
    // --token is the PEER'S MASTER TOKEN, for tabs/input/files. A
    // tab-atelier-proxy is not a peer tab-atelier and has none of those
    // endpoints, so relaying through one needs --relay-token and nothing else.
    // Requiring a master token there would mean inventing a value to satisfy
    // the parser, which teaches people to put junk in a credential field.
    let token = match token {
        Some(s) if !s.is_empty() => s,
        _ if relay_token.as_deref().is_some_and(|t| !t.is_empty()) => {
            eprintln!("tab-atelier remote add: no --token — relay-only endpoint (no tabs, input or files)");
            String::new()
        }
        _ => {
            eprintln!("tab-atelier remote add: --token is required (or --relay-token for a relay-only endpoint)");
            return 2;
        }
    };

    // Capture the TLS cert fingerprint via TOFU unless the caller
    // already provided one or opted out (plain HTTP endpoints).
    let fingerprint = if url.starts_with("https://") && !no_pin {
        match cert_sha256.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => match fetch_cert_fingerprint(&url) {
                Ok(fp) => fp,
                Err(e) => {
                    eprintln!("tab-atelier remote add: cert fingerprint capture failed: {e}");
                    eprintln!("    pass --cert-sha256 <hex> or --no-pin to override");
                    return 1;
                }
            },
        }
    } else {
        cert_sha256.unwrap_or_default()
    };

    let mut prefs = load_preferences(&platform::config_dir());
    if prefs
        .remote_endpoints
        .iter()
        .any(|e| e.label.eq_ignore_ascii_case(&label))
    {
        eprintln!("tab-atelier remote add: an endpoint with label {label:?} already exists");
        return 1;
    }
    let endpoint = RemoteEndpoint {
        id: uuid::Uuid::new_v4().to_string(),
        label: label.clone(),
        url,
        token,
        cert_sha256: fingerprint.clone(),
        autoconnect,
        cf_access_client_id: cf_id.unwrap_or_default().trim().to_string(),
        cf_access_client_secret: cf_secret.unwrap_or_default().trim().to_string(),
        relay_token: relay_token.unwrap_or_default().trim().to_string(),
    };
    prefs.remote_endpoints.push(endpoint);
    save_preferences(&platform::config_dir(), &prefs);
    println!("✓ added endpoint {label}");
    if !fingerprint.is_empty() {
        println!("  cert pinned: {fingerprint}");
    }
    0
}

fn cmd_remove(key: &str) -> i32 {
    let mut prefs = load_preferences(&platform::config_dir());
    let before = prefs.remote_endpoints.len();
    prefs
        .remote_endpoints
        .retain(|e| !(e.label.eq_ignore_ascii_case(key) || e.id == key));
    if prefs.remote_endpoints.len() == before {
        eprintln!("tab-atelier remote remove: no endpoint matched {key:?}");
        return 1;
    }
    save_preferences(&platform::config_dir(), &prefs);
    println!("✓ removed {key}");
    0
}

fn cmd_test(key: &str, watch: bool) -> i32 {
    let prefs = load_preferences(&platform::config_dir());
    let Some(endpoint) = prefs
        .remote_endpoints
        .into_iter()
        .find(|e| e.label.eq_ignore_ascii_case(key) || e.id == key)
    else {
        eprintln!("tab-atelier remote test: no endpoint matched {key:?}");
        return 1;
    };

    println!("Connecting to {} ({})…", endpoint.label, endpoint.url);
    let Some(client) = remote::Client::spawn(endpoint) else {
        eprintln!("error: could not start the remote client thread");
        return 1;
    };

    let mut seen_first_tabs = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(if watch { 600 } else { 10 });

    while std::time::Instant::now() < deadline {
        if crate::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            println!("(interrupted)");
            break;
        }
        match client.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(remote::RemoteEvent::Tabs { tabs, state }) => {
                if !seen_first_tabs {
                    println!("Connected. {} tab(s) on remote:", tabs.len());
                    seen_first_tabs = true;
                    for t in &tabs {
                        let badge = match t.agent_state.as_deref() {
                            Some(s) => format!(" [{s}]"),
                            None if t.agent_kind.is_some() => " [attached]".into(),
                            None => String::new(),
                        };
                        println!(
                            "  #{:<3} {:<24} cwd={}{}",
                            t.remote_index,
                            t.name,
                            t.cwd.as_deref().unwrap_or("-"),
                            badge
                        );
                    }
                    if !watch {
                        return 0;
                    }
                }
                if watch && matches!(state, remote::ConnectionState::Reconnecting { .. }) {
                    println!("⚠ reconnecting…");
                }
            }
            Ok(remote::RemoteEvent::Output {
                remote_id,
                bytes,
                total_len,
                ..
            }) => {
                if watch {
                    let preview = String::from_utf8_lossy(&bytes);
                    let preview = preview.trim_end();
                    let preview = if preview.chars().count() > 60 {
                        let head: String = preview.chars().take(57).collect();
                        format!("{head}…")
                    } else {
                        preview.to_string()
                    };
                    println!(
                        "[{}] +{} bytes (len={}) {}",
                        &remote_id[..8.min(remote_id.len())],
                        bytes.len(),
                        total_len,
                        preview,
                    );
                }
            }
            Ok(remote::RemoteEvent::Error { message }) => {
                eprintln!("⚠ {message}");
                if !watch && !seen_first_tabs {
                    return 1;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                eprintln!("client thread disconnected");
                return 1;
            }
        }
    }
    if !seen_first_tabs {
        eprintln!("timed out before first Tabs event");
        return 1;
    }
    0
}

fn cmd_repin(key: &str) -> i32 {
    let mut prefs = load_preferences(&platform::config_dir());
    let Some(ep) = prefs
        .remote_endpoints
        .iter_mut()
        .find(|e| e.label.eq_ignore_ascii_case(key) || e.id == key)
    else {
        eprintln!("tab-atelier remote re-pin: no endpoint matched {key:?}");
        return 1;
    };
    if !ep.url.starts_with("https://") {
        eprintln!("tab-atelier remote re-pin: endpoint {key:?} is plain HTTP — nothing to pin");
        return 1;
    }
    match fetch_cert_fingerprint(&ep.url) {
        Ok(fp) => {
            if fp.eq_ignore_ascii_case(&ep.cert_sha256) {
                println!("✓ fingerprint unchanged: {fp}");
            } else {
                println!("⚠ fingerprint changed");
                println!("  old: {}", ep.cert_sha256);
                println!("  new: {fp}");
                ep.cert_sha256 = fp;
                save_preferences(&platform::config_dir(), &prefs);
                println!("✓ updated");
            }
            0
        }
        Err(e) => {
            eprintln!("tab-atelier remote re-pin: {e}");
            1
        }
    }
}

fn cmd_pin_cert(url: &str) -> i32 {
    match fetch_cert_fingerprint(url) {
        Ok(fp) => {
            println!("{fp}");
            0
        }
        Err(e) => {
            eprintln!("tab-atelier remote pin-cert: {e}");
            1
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_subcommand_table_routes_or_explains_itself() {
        // `list` only reads the preference file; every other verb either
        // mutates it or needs a remote host, so this covers the routing and
        // the two no-op paths without touching the machine.
        assert_eq!(run(&argv(&["list"])), 0);
        assert_eq!(run(&argv(&["--help"])), 0);
        assert_eq!(run(&argv(&["help"])), 0);
        assert_eq!(run(&argv(&[])), 2, "no subcommand is usage");
        assert_eq!(run(&argv(&["not-a-verb"])), 2);
        // Each mutating verb validates its own arguments first.
        assert_eq!(run(&argv(&["add"])), 2);
        assert_eq!(run(&argv(&["remove"])), 2);
        assert_eq!(run(&argv(&["rm"])), 2);
        assert_eq!(run(&argv(&["pin-cert"])), 2);
        assert_eq!(run(&argv(&["re-pin"])), 2);
    }

    #[test]
    fn every_documented_remote_add_would_actually_parse() {
        // `tab-atelier remote add proxy --url …` shipped in docs/proxy.md and
        // failed on the first paste: the label is a named argument. Docs are
        // copy-pasted verbatim, so they are held to the real parser — this
        // asks clap itself rather than a hand-maintained table of flags,
        // which could drift out of step with the parser it describes.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = vec![root.join("README.md")];
        if let Ok(dir) = std::fs::read_dir(root.join("docs")) {
            files.extend(
                dir.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "md")),
            );
        }
        // The proxy's web UI hands out this command too, and it is the copy
        // people actually use — it appears beside a freshly minted key, once.
        // Scanning only the markdown let the UI keep shipping the broken
        // `remote add proxy --url …` form long after the docs were corrected,
        // so a user pasted it and got "unknown argument: proxy" from a page
        // that had just told them it would work.
        files.push(root.join("crates/tab-atelier-proxy/assets/app.js"));
        files.push(root.join("crates/tab-atelier-proxy/assets/index.html"));
        assert!(files.len() > 1, "expected docs/*.md to be readable");

        let mut checked = 0usize;
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // Re-join shell line continuations so a multi-line example is
            // validated as the single command a reader would run.
            let joined = text.replace("\\\n", " ");
            for line in joined.lines() {
                let Some((_, rest)) = line.split_once("tab-atelier remote add") else {
                    continue;
                };
                let tokens: Vec<String> = rest
                    .split_whitespace()
                    // Backticks and quotes from markdown, and the trailing
                    // `",` that ends a line of a JS string array.
                    .map(|t| t.trim_matches(['`', '"', '\'', ',']).to_owned())
                    .take_while(|t| !t.is_empty() && t != "#" && t != "&&")
                    // Prose ellipsis standing in for "the other flags", and
                    // the placeholders a reader is meant to substitute.
                    .filter(|t| t != "…" && t != "...")
                    .collect();
                if tokens.is_empty() {
                    continue;
                }
                checked += 1;
                let argv = ["tab-atelier remote".to_owned(), "add".to_owned()]
                    .into_iter()
                    .chain(tokens);
                if let Err(e) = Cli::try_parse_from(argv) {
                    // MissingRequiredArgument is fine: a doc line may show
                    // only the flags it is talking about. An argument the
                    // parser does not know is not.
                    assert_eq!(
                        e.kind(),
                        clap::error::ErrorKind::MissingRequiredArgument,
                        "{}: `remote add` example does not parse ({:?})\n  line: {}\n  {e}",
                        path.display(),
                        e.kind(),
                        line.trim(),
                    );
                }
            }
        }
        assert!(
            checked > 0,
            "found no documented `remote add` examples — did the docs move?"
        );
    }

    #[test]
    fn truncate_keeps_the_table_aligned() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly-10", 10), "exactly-10");
        let cut = truncate("a-very-long-endpoint-name", 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'), "elided with an ellipsis, not a hard cut");
        // Multi-byte input must not panic on a mid-char boundary.
        assert!(truncate("héllo-wörld-ünicode", 8).chars().count() <= 8);
    }

    fn rargs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn remote_add_refuses_an_incomplete_endpoint() {
        // Every one of these returns before touching preferences, which is
        // what makes them safe to assert on: a half-specified endpoint that
        // got written would fail later, at connect time, far from the typo.
        assert_eq!(super::run(&rargs(&["add"])), 2, "no label");
        assert_eq!(super::run(&rargs(&["add", "--label", "peer"])), 2, "no url");
        assert_eq!(
            super::run(&rargs(&["add", "--label", "peer", "--url", "http://x:1"])),
            2,
            "no token"
        );
        assert_eq!(super::run(&rargs(&["add", "--nope", "x"])), 2, "unknown flag");
        // The label written positionally — `remote add proxy --url …` — is
        // the mistake a copy-paste from the docs produced, so it is pinned.
        assert_eq!(
            super::run(&rargs(&["add", "proxy", "--url", "http://x:1"])),
            2,
            "positional label"
        );
        // A flag with no value must not swallow the next flag as its argument.
        assert_eq!(super::run(&rargs(&["add", "--label"])), 2);
        assert_eq!(super::run(&rargs(&["add", "--url"])), 2);
        assert_eq!(super::run(&rargs(&["add", "--token"])), 2);
    }

    #[test]
    fn remote_verbs_validate_before_they_act() {
        // An unknown action is a usage error, not a silent no-op.
        assert_eq!(super::run(&rargs(&["frobnicate"])), 2);
        assert_eq!(super::run(&rargs(&[])), 2);
        // remove needs something to remove; a bare `remove` must not delete
        // the first endpoint it finds.
        assert_eq!(super::run(&rargs(&["remove"])), 2);
        // pin-cert needs a URL, and refuses a non-https one — pinning a plain
        // http endpoint would capture nothing while looking like it worked.
        assert_eq!(super::run(&rargs(&["pin-cert"])), 2);
        assert_ne!(super::run(&rargs(&["pin-cert", "http://example.com"])), 0);
        // Listing is always safe, with or without endpoints configured.
        assert_eq!(super::run(&rargs(&["list"])), 0);
    }

    #[test]
    fn removing_an_unknown_label_fails_instead_of_succeeding_quietly() {
        // Exit code matters: a script that removes a stale peer must be able
        // to tell "it was not there" from "it is gone now".
        assert_ne!(super::run(&rargs(&["remove", "definitely-not-an-endpoint-xyz"])), 0);
    }
}
