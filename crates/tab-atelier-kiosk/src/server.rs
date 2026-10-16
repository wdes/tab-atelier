// SPDX-License-Identifier: MPL-2.0

//! The Kiosk's server: the Rocket it runs, and the work behind the routes that
//! reach past it to a remote.
//!
//! The route attributes are in [`crate::routes`]; this module holds what they
//! call into. A route that answers from the Kiosk's own state calls a
//! [`Panes`] method directly. One that talks to a *remote* — the fleet, a tab
//! list, a launch — comes here, because each is a blocking HTTP call to another
//! machine and so must run off the request thread.

use std::sync::Arc;
use std::time::Duration;

use rocket::http::Status;
use rocket::request::Request;
use rocket::{catch, catchers};
use serde_json::Value;

use tab_atelier_api::{ProblemResource, Reply};

use crate::auth::KioskAuth;
use crate::config::{Config, HopTimeouts};
use crate::daemon::Daemon;
use crate::panes::{self, Panes};
use crate::remotes::{Remote, RemoteError};
use crate::resources::{Answer, BroadcastStarted, Cards, Doc, Fleet, FleetServer, TabClosed, TabLaunched};

/// The message the "ask all agents" button types into every agent tab: it tells
/// the agent it is part of tab-atelier and how to publish its card, so the
/// dashboard can show what it does. One line per field, in the card's own keys.
const BROADCAST_MESSAGE: &str = "You are part of an agent system named tab-atelier. \
Please publish your agent card so the dashboard can show what you do - run each, with your own value:\n\
  tab-atelier set-meta assignment <what you are assigned to>\n\
  tab-atelier set-meta specialty <your role>\n\
  tab-atelier set-meta objective <what you are trying to achieve>\n\
  tab-atelier set-meta current_task <what you are doing right now>\n\
(a blank value clears a field.)";

/// The pause between two agents in a broadcast. Enough that N agents are a
/// trickle rather than a herd — they do not all wake and call the model at the
/// same instant — while a fleet of a few dozen still finishes in a few seconds.
const BROADCAST_GAP: Duration = Duration::from_millis(350);

// ---------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------

/// Build the Kiosk's Rocket, wired with the token, the panes and the routes.
///
/// Split from [`run`] so a test can drive the app in-process. `timeouts` are
/// the hop deadlines a remote call is held to; a test compresses them and still
/// exercises the real timeout paths instead of waiting out the production
/// values.
#[must_use]
pub fn build(cfg: Config, timeouts: HopTimeouts) -> rocket::Rocket<rocket::Build> {
    // A pane request is small; cap what is buffered so a large body is refused
    // rather than eaten. Merged as the config key rather than a `Limits` value,
    // so the type stays Rocket's business.
    let figment = rocket::Config::figment()
        .merge(("address", cfg.bind.ip().to_string()))
        .merge(("port", cfg.bind.port()))
        .merge(("limits.json", tab_atelier_api::MAX_BODY_BYTES))
        // The log is the daemon's; a front door that narrates every request is
        // noise on the terminal the operator is watching.
        .merge(("log_level", "critical"));
    let auth = Arc::new(KioskAuth {
        token: cfg.token.clone(),
    });
    let panes = Panes::new(cfg.journal, cfg.remotes, cfg.intentions, cfg.web_root, cfg.catalog);
    let (routes, spec) = crate::routes::all();
    let mounted = tab_atelier_api::mount((routes, spec), "/");
    let rocket = rocket::custom(figment)
        .manage(auth)
        .manage(panes)
        .manage(Daemon::new(timeouts))
        .mount("/", mounted.routes)
        .register("/", catchers());
    // Every route is either documented or listed as exempt, checked on every
    // boot — a route added without one panics here rather than shipping an API
    // whose document has quietly stopped matching it. The server is built this
    // one way, so the cost is a single pass over the mount table.
    tab_atelier_api::assert_covers(&rocket, &mounted.spec, &crate::routes::exempt());
    rocket
}

/// Run the Kiosk until the process ends.
///
/// # Errors
///
/// Returns an error if the address cannot be bound, or if the server stops on
/// an error rather than a signal.
pub async fn run(cfg: Config) -> std::io::Result<()> {
    build(cfg, HopTimeouts::default())
        .launch()
        .await
        .map(|_| ())
        .map_err(std::io::Error::other)
}

/// [`run`], for a caller that thinks of it as serving.
///
/// # Errors
///
/// As [`run`].
pub async fn serve(cfg: Config) -> std::io::Result<()> {
    run(cfg).await
}

// ---------------------------------------------------------------------------
// Catchers
// ---------------------------------------------------------------------------

/// The error pages, off [`tab_atelier_api`] except the 404.
///
/// Every code but one is the shared answer. The 404 is this crate's because
/// Rocket folds a method mismatch into a 404 and never raises a 405, and the
/// `Allow` list is the part an operator needs — see [`not_found`]. The shared
/// `not_found` is therefore not registered: two `#[catch(404)]` is not a thing.
fn catchers() -> Vec<rocket::Catcher> {
    use tab_atelier_api::catcher;
    catchers![
        catcher::bad_request,
        catcher::unauthorized,
        catcher::forbidden,
        not_found,
        catcher::payload_too_large,
        catcher::unprocessable,
        catcher::unavailable,
    ]
}

/// A 404, or a 405 when the path is a route's but the method is not.
///
/// Rocket raises no 405 of its own: a request whose path matches a route but
/// whose method does not falls through to 404. The difference matters to an
/// operator — "this pane is not here" and "you paged the wrong verb" are
/// different mistakes — so it is drawn here. The `Allow` list is read back out
/// of the mounted routes rather than kept as a second table, so it cannot drift.
#[catch(404)]
fn not_found(req: &Request<'_>) -> Reply {
    let path = req.uri().path().as_str();
    let mut methods: Vec<String> = req
        .rocket()
        .routes()
        .filter(|route| path_matches(route.uri.path(), path))
        .map(|route| route.method.to_string())
        .collect();
    methods.sort();
    methods.dedup();
    if methods.is_empty() {
        return tab_atelier_api::problem(404, "not found");
    }
    tab_atelier_api::problem(405, "method not allowed").with_header("allow", methods.join(", "))
}

/// Whether a mounted route's templated path matches a concrete request path.
///
/// Segment-wise: `<id>` matches any one segment, `<rest..>` matches the
/// remainder. The same shape Rocket matched, read back out of the mount table.
fn path_matches(template: &str, path: &str) -> bool {
    let template: Vec<&str> = template.trim_matches('/').split('/').collect();
    let path: Vec<&str> = path.trim_matches('/').split('/').collect();
    let mut at = 0;
    for segment in &template {
        if segment.starts_with('<') && segment.contains("..") {
            return true;
        }
        let Some(got) = path.get(at) else { return false };
        if !segment.starts_with('<') && segment != got {
            return false;
        }
        at += 1;
    }
    at == path.len()
}

// ---------------------------------------------------------------------------
// Assets
// ---------------------------------------------------------------------------

/// Serve one UI file from the web root.
///
/// `no-cache` on everything: the page names the other assets by path and a
/// built UI has no content hashes in its filenames, so a cached page paired
/// with a rebuilt script is the failure to avoid. A missing file is the 404 the
/// UI never sees.
#[must_use]
pub fn serve_asset(panes: &Panes, rel: &str) -> Reply {
    tab_atelier_web::assets::read(panes.web_root(), rel).map_or_else(
        || tab_atelier_api::problem(404, "not found"),
        |(content_type, bytes)| {
            Reply::bytes(200, bytes)
                .with_header("content-type", content_type.to_owned())
                .with_header("cache-control", "no-cache".to_owned())
        },
    )
}

// ---------------------------------------------------------------------------
// The fleet
// ---------------------------------------------------------------------------

/// `GET /kiosk/fleet` — every configured server and its tabs, with their cards.
///
/// The servers are the ones the operator added in Serveurs and no others — the
/// Kiosk does not invent a local one. Each is reached with ITS token (never the
/// browser's). A server that cannot be reached is reported as
/// `{error: unreachable}` rather than failing the whole list: one down server
/// must not blank the fleet.
pub async fn fleet(panes: &Panes, daemon: &Daemon) -> Answer<Fleet> {
    let mut servers = Vec::new();
    for remote in panes.fleet_targets() {
        let id = remote.id.clone();
        let label = remote.label.clone();
        let lister = daemon.clone();
        let answered = {
            let remote = remote.clone();
            tokio::task::spawn_blocking(move || lister.get(&remote, "/tabs")).await
        };
        let server = match answered {
            Ok(Ok((status, text))) if (200..300).contains(&status) => FleetServer {
                id,
                label,
                tabs: Some(lean_tabs(&text)),
                error: None,
            },
            _ => FleetServer {
                id,
                label,
                tabs: None,
                error: Some("unreachable".to_owned()),
            },
        };
        servers.push(server);
    }
    Ok(Doc::ok(Fleet { servers }))
}

/// The tabs a landing page needs, token-stripped and brain-filtered.
///
/// The daemon's own auto-injector tab ("brain") is not the operator's: it is a
/// session-less harness tab with no led, and it does not belong in the user's
/// list. The scrollback is left out too — a landing page does not need it. Only
/// the tab's own fields are passed on; the remote's token is never among them.
fn lean_tabs(text: &str) -> Vec<Value> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.get("tabs").and_then(Value::as_array).cloned())
        .map(|tabs| {
            tabs.iter()
                .filter(|tab| tab.get("agent_kind").and_then(Value::as_str) != Some("brain"))
                .map(|tab| {
                    serde_json::json!({
                        "id": tab.get("id"),
                        "name": tab.get("name"),
                        "cwd": tab.get("cwd"),
                        // The card: the operator's labels for this tab.
                        "meta": tab.get("meta"),
                        // And how it is doing — the led the card shows, plus the
                        // agent kind ("is a claude in it?").
                        "led": tab.get("led"),
                        // Only a real external agent — a shell's kind is not an
                        // agent, so no badge for it.
                        "agent_kind": crate::agent::agent_of(tab),
                        "agent_state": tab.get("agent_state"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// A remote's tabs
// ---------------------------------------------------------------------------

/// `GET /kiosk/remotes/<id>/tabs` — a remote's tab list, relayed.
pub async fn remote_tabs(panes: &Panes, daemon: &Daemon, id: &str) -> Answer<Value> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Err(panes::remote_problem(&error)),
    };
    match hop(daemon, remote, "/tabs".to_owned(), None).await {
        Ok((status, text)) => relayed(status, &text),
        Err(()) => Err(Panes::bad_gateway()),
    }
}

/// `POST /kiosk/remotes/<id>/tabs` — open a tab on a remote, relayed.
pub async fn remote_tab_create(panes: &Panes, daemon: &Daemon, id: &str, spec: &Value) -> Answer<Value> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Err(panes::remote_problem(&error)),
    };
    match hop(daemon, remote, "/tabs".to_owned(), Some(spec.clone())).await {
        Ok((status, text)) => relayed(status, &text),
        Err(()) => Err(Panes::bad_gateway()),
    }
}

/// `POST /kiosk/remotes/<id>/tabs/<uuid>/close` — close a tab on a remote.
///
/// The Kiosk knows the tab by uuid (that is what the fleet reports and what
/// `/tabs/by-id/` addresses), so the close is by uuid and survives a tab that
/// moved position.
pub async fn remote_tab_close(panes: &Panes, daemon: &Daemon, id: &str, tab: &str) -> Answer<TabClosed> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Err(panes::remote_problem(&error)),
    };
    let path = format!("/tabs/by-id/{tab}");
    let caller = daemon.clone();
    let answered = tokio::task::spawn_blocking(move || caller.delete(&remote, &path)).await;
    match answered {
        Ok(Ok((status, _))) if (200..300).contains(&status) => Ok(Doc::ok(TabClosed { closed: tab.to_owned() })),
        Ok(Ok((status, text))) => Err(refused(status, &text)),
        _ => Err(Panes::bad_gateway()),
    }
}

/// `POST /kiosk/remotes/<id>/tabs/<uuid>/meta` — set one card label on a tab.
///
/// One key per call, matching the daemon's own `/tabs/by-id/<uuid>/meta`: the
/// card is a set of keys, and the operator edits one field at a time.
pub async fn remote_tab_meta(panes: &Panes, daemon: &Daemon, id: &str, tab: &str, change: &Value) -> Answer<Value> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Err(panes::remote_problem(&error)),
    };
    let path = format!("/tabs/by-id/{tab}/meta");
    match hop(daemon, remote, path, Some(change.clone())).await {
        Ok((status, text)) => relayed(status, &text),
        Err(()) => Err(Panes::bad_gateway()),
    }
}

/// `GET /kiosk/remotes/<id>/tabs/<uuid>/view` — mint a share token for the tab
/// on the remote, then redirect the browser to that remote's terminal viewer.
///
/// The viewer is served by the remote daemon, which owns the tab, so this 302s
/// to `<remote>/tabs/by-id/<uuid>/view?token=<share>`. The token is the tab's
/// own per-tab RW share token, minted via the daemon's
/// `POST /tabs/by-id/<uuid>/share` — never the remote's master, so the browser
/// tab holds a credential good for one tab and rotatable with `rotate-tokens`.
/// The operator lands in edit mode and can type.
pub async fn remote_tab_view(panes: &Panes, daemon: &Daemon, id: &str, tab: &str) -> Reply {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(RemoteError::NotFound) => return tab_atelier_api::problem(404, "no such server"),
        Err(_) => return tab_atelier_api::problem(503, "the servers are unavailable"),
    };
    let base = remote.url.clone();
    let share_path = format!("/tabs/by-id/{tab}/share");
    let caller = daemon.clone();
    let answered = tokio::task::spawn_blocking(move || caller.post(&remote, &share_path, &serde_json::json!({}))).await;
    let token = match answered {
        Ok(Ok((status, text))) if (200..300).contains(&status) => serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value.get("rw").and_then(Value::as_str).map(str::to_owned)),
        _ => None,
    };
    let Some(token) = token else {
        return tab_atelier_api::problem(502, "could not mint a share token");
    };
    Reply::empty(302).with_header("location", format!("{base}/tabs/by-id/{tab}/view?token={token}"))
}

/// `GET /kiosk/remotes/<id>/cards` — a remote's agent cards, derived from the
/// meta labels of its tabs.
///
/// The daemon already lists each tab's `meta`; this shapes the conventional set
/// into a card, so the dashboard reads one shape instead of knowing the keys.
/// Tabs with no card keys are omitted — a plain tab is not reported as an empty
/// card.
pub async fn remote_cards(panes: &Panes, daemon: &Daemon, id: &str) -> Answer<Cards> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Err(panes::remote_problem(&error)),
    };
    let lister = daemon.clone();
    let answered = tokio::task::spawn_blocking(move || lister.get(&remote, "/tabs")).await;
    let Ok(Ok((status, text))) = answered else {
        return Err(Panes::bad_gateway());
    };
    if !(200..300).contains(&status) {
        return Err(refused(status, &text));
    }
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Err(Panes::bad_gateway());
    };
    let cards: Vec<Value> = value
        .get("tabs")
        .and_then(Value::as_array)
        .map(|tabs| {
            tabs.iter()
                .filter_map(|tab| {
                    let meta: std::collections::BTreeMap<String, String> = tab
                        .get("meta")
                        .and_then(|meta| serde_json::from_value(meta.clone()).ok())
                        .unwrap_or_default();
                    let card = crate::card::card_from_meta(&meta)?;
                    Some(serde_json::json!({
                        "id": tab.get("id"),
                        "name": tab.get("name"),
                        "card": card,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Doc::ok(Cards { cards }))
}

/// `POST /kiosk/remotes/<id>/broadcast` — type the same message into every
/// agent tab on a remote, so each agent publishes its card.
///
/// Only the tabs that are a **live external agent** get it — `agent_of`, not
/// merely "has an `agent_kind`": a bash shell has a kind too, and the brief was
/// once typed into bash sessions. A dead agent ("early try", agent gone) is
/// skipped as well. The reply says how many were queued for.
pub async fn remote_broadcast(panes: &Panes, daemon: &Daemon, id: &str) -> Answer<BroadcastStarted> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Err(panes::remote_problem(&error)),
    };
    let lister = daemon.clone();
    let listing = {
        let remote = remote.clone();
        tokio::task::spawn_blocking(move || lister.get(&remote, "/tabs")).await
    };
    let agent_ids: Vec<String> = match listing {
        Ok(Ok((status, text))) if (200..300).contains(&status) => serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("tabs")
                    .and_then(Value::as_array)
                    .map(|tabs| crate::agent::broadcast_targets(tabs))
            })
            .unwrap_or_default(),
        _ => return Err(Panes::bad_gateway()),
    };
    let total = agent_ids.len();
    // Round-robin the START: each broadcast begins at the next tab, so repeated
    // fan-outs do not all hammer whichever tab sorts first. `scan_order` is the
    // same sweep `brain.rs` uses, shared through `tab-atelier-pace`.
    let order = tab_atelier_pace::scan_order(total, panes.next_broadcast_cursor(total));
    let ordered: Vec<String> = order.into_iter().map(|i| agent_ids[i].clone()).collect();
    // Send in the BACKGROUND, one tab at a time with a gap between them. Hitting
    // N agents in the same instant is a thundering herd — they wake together and
    // contend for the model at once. The request returns immediately; the
    // fan-out drains behind it, spread out.
    let daemon = daemon.clone();
    let remote = remote.clone();
    tokio::spawn(async move {
        for tab in ordered {
            let _ = submit_to_tab(&daemon, &remote, &tab, BROADCAST_MESSAGE).await;
            tokio::time::sleep(BROADCAST_GAP).await;
        }
    });
    Ok(Doc::ok(BroadcastStarted { started: total }))
}

/// `POST /kiosk/remotes/<id>/intent/<slug>/launch` — open a worker tab on a
/// remote for an intention, and remember which tab it was.
///
/// The Kiosk owns the intention (a local file) and the remote owns the tab. The
/// tab is asked for `sync` so its uuid comes back rather than being guessed; the
/// intention is tagged onto it as `meta.intent`, so the worker can tell which
/// intention it is for, and the tab id is written back to the intention file.
///
/// What this does NOT do — because `POST /tabs` has no way to — is hand the
/// worker the intention's text as a prompt. The brief stays in the file the
/// operator can read; injecting it is a daemon feature that does not exist yet.
pub async fn remote_intent_launch(panes: &Panes, daemon: &Daemon, id: &str, slug: &str) -> Answer<TabLaunched> {
    // The intention is ours: read it before reaching out, so an unknown slug is
    // a 404 and no remote is asked to spawn anything.
    if panes.intentions().read(slug).is_err() {
        return Err(Doc::problem(404, "no such intention"));
    }
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Err(panes::remote_problem(&error)),
    };
    let spec = serde_json::json!({ "sync": true, "meta": { "intent": slug } });
    match hop(daemon, remote, "/tabs".to_owned(), Some(spec)).await {
        Ok((status, text)) if (200..300).contains(&status) => {
            // The daemon's `sync` reply carries `{"id": "<uuid>", …}`; record it
            // so the intention can find its worker again.
            let tab_id = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned));
            if let Some(tab_id) = &tab_id {
                let _ = panes.intentions().set_tab(slug, tab_id);
            }
            Ok(Doc::ok(TabLaunched {
                slug: slug.to_owned(),
                launched: tab_id.is_some(),
                tab: tab_id,
            }))
        }
        Ok((status, text)) => Err(refused(status, &text)),
        Err(()) => Err(Panes::bad_gateway()),
    }
}

// ---------------------------------------------------------------------------
// Reaching a remote
// ---------------------------------------------------------------------------

/// GET or POST a remote path on a blocking thread, so the reactor keeps serving
/// while a slow remote answers. `None` spec means GET.
async fn hop(daemon: &Daemon, remote: Remote, path: String, spec: Option<Value>) -> Result<(u16, String), ()> {
    let daemon = daemon.clone();
    tokio::task::spawn_blocking(move || {
        spec.map_or_else(
            || daemon.get(&remote, &path),
            |value| daemon.post(&remote, &path, &value),
        )
    })
    .await
    .map_err(|_| ())?
    .map_err(|_| ())
}

/// Pass a remote's own answer through: its status, and its body as it came.
///
/// A body that is not JSON (a proxy's HTML error page, say) is folded into this
/// API's error shape rather than relayed blind — one shape for every refusal.
fn relayed(status: u16, text: &str) -> Answer<Value> {
    serde_json::from_str::<Value>(text).map_or_else(
        |_| Err(refused(status, text)),
        |value| Ok(Doc::with_status(Status::new(status), value)),
    )
}

/// A refusal from a remote, as this API's error shape.
///
/// The remote's own message is kept when it sent one (a 400 from its schema
/// validator is worth reading); otherwise a neutral one is used. Nothing about
/// *which* machine failed is added — the caller already knows.
fn refused(status: u16, text: &str) -> Doc<ProblemResource> {
    let message = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "the server refused the request".to_owned());
    Doc::problem(status, &message)
}

// ---------------------------------------------------------------------------
// Typing into a tab
// ---------------------------------------------------------------------------

/// Type `message` into a tab and submit it — the way `tab-atelier dispatch`
/// does, not the way a naive paste does.
///
/// A prompt and a trailing `\r` that arrive in the SAME write are treated by
/// `claude` as pasted multiline text: the `\r` becomes a newline inside the
/// paste, the tab shows `[Pasted text #1]` and sits idle waiting for a real
/// Enter. So: send the paste, watch the screen until it stops changing (the
/// paste has finished rendering), then send the `\r` as a SEPARATE write — with
/// a floor and a cap so a silent-but-busy tab still gets its Enter.
///
/// Returns whether the Enter was sent.
async fn submit_to_tab(daemon: &Daemon, remote: &Remote, tab: &str, message: &str) -> bool {
    const FLOOR: Duration = Duration::from_millis(400);
    const QUIET: Duration = Duration::from_millis(300);
    const POLL: Duration = Duration::from_millis(80);
    const CAP: Duration = Duration::from_secs(5);
    let input = format!("/tabs/by-id/{tab}/input");
    let output = format!("/tabs/by-id/{tab}/output");

    // 1. The paste — WITHOUT the carriage return.
    if !post_bytes(daemon, remote, &input, message.as_bytes().to_vec()).await {
        return false;
    }
    // 2. Wait for the paste to settle: an unconditional floor, then poll the
    //    screen until it has been unchanged for QUIET, capped at CAP.
    tokio::time::sleep(FLOOR).await;
    let start = std::time::Instant::now();
    let mut last = read_screen(daemon, remote, &output).await;
    let mut stable = std::time::Instant::now();
    while start.elapsed() < CAP {
        if stable.elapsed() >= QUIET {
            break;
        }
        tokio::time::sleep(POLL).await;
        let now = read_screen(daemon, remote, &output).await;
        if now != last {
            last = now;
            stable = std::time::Instant::now();
        }
    }
    // 3. The Enter — a SEPARATE write, so it submits rather than joining the
    //    paste. Sent TWICE: a multi-line paste can consume the first Enter to
    //    insert the block (the "[Pasted text]" marker) and only need the second
    //    to send it. Two Enters on an already-sent line submit an empty one,
    //    which is harmless — the message was already delivered.
    let sent = post_bytes(daemon, remote, &input, b"\r".to_vec()).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    post_bytes(daemon, remote, &input, b"\r".to_vec()).await;
    sent
}

/// POST raw bytes to a remote tab path; true on a 2xx.
async fn post_bytes(daemon: &Daemon, remote: &Remote, path: &str, bytes: Vec<u8>) -> bool {
    let daemon = daemon.clone();
    let remote = remote.clone();
    let path = path.to_string();
    matches!(
        tokio::task::spawn_blocking(move || daemon.post_raw(&remote, &path, bytes)).await,
        Ok(Ok((status, _))) if (200..300).contains(&status)
    )
}

/// GET a remote tab path, returning the body (the screen) or empty.
async fn read_screen(daemon: &Daemon, remote: &Remote, path: &str) -> String {
    let daemon = daemon.clone();
    let remote = remote.clone();
    let path = path.to_string();
    match tokio::task::spawn_blocking(move || daemon.get(&remote, &path)).await {
        Ok(Ok((status, text))) if (200..300).contains(&status) => text,
        _ => String::new(),
    }
}
