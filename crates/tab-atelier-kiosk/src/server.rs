// SPDX-License-Identifier: MPL-2.0

//! The HTTP server: bind, accept, dispatch to an asset or a pane.

use std::sync::Arc;
use std::time::Duration;

use http::{Request, Response, StatusCode, header};
use http_body_util::{BodyExt as _, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::body::{BoxBody, full, json_response};
use crate::config::{Config, HopTimeouts};
use crate::daemon::Daemon;
use crate::panes::Panes;
use crate::remotes::Remote;
use crate::route::{self, Route};

/// Largest request body the Kiosk will read from a pane request.
///
/// The Kiosk is a browser front door, not an upload service; anything larger is
/// a mistake or an attack, and the limit is what keeps it out of memory.
const MAX_REQUEST_BODY: usize = 4 * 1024 * 1024;

/// Bind `cfg.bind` and serve until the process is killed.
///
/// # Errors
///
/// Returns an error if the address cannot be bound.
pub async fn run(cfg: Config) -> std::io::Result<()> {
    let listener = TcpListener::bind(cfg.bind).await?;
    serve(listener, cfg).await
}

/// Serve on an already-bound listener.
///
/// Split from [`run`] so a test can bind port 0 and learn the port it got
/// rather than guessing a free one and racing the machine.
///
/// # Errors
///
/// Does not return errors: a failed `accept` is retried and a failed connection
/// is dropped. The `Result` matches [`run`]'s shape for callers.
pub async fn serve(listener: TcpListener, cfg: Config) -> std::io::Result<()> {
    serve_with(listener, cfg, HopTimeouts::default()).await
}

/// [`serve`] with explicit hop deadlines, so a test can compress them and still
/// exercise the real timeout paths instead of waiting out the production values.
pub async fn serve_with(listener: TcpListener, cfg: Config, timeouts: HopTimeouts) -> std::io::Result<()> {
    let daemon = Arc::new(Daemon::new(timeouts));
    let panes = Arc::new(Panes::new(
        cfg.journal,
        cfg.token,
        cfg.remotes,
        cfg.intentions,
        cfg.web_root,
    ));
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            // A single failed accept (fd exhaustion, client gone) must not take
            // the kiosk down with it; the next connection is unaffected.
            Err(err) => {
                eprintln!("accept failed: {err}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let panes = Arc::clone(&panes);
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let panes = Arc::clone(&panes);
                let daemon = Arc::clone(&daemon);
                async move { Ok::<_, std::convert::Infallible>(handle(req, &panes, &daemon).await) }
            });
            // Connection-level errors (client hung up mid-response) are routine.
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// Serve one UI file from the web root.
///
/// `no-cache` on everything: the page names the other assets by path and a
/// built UI has no content hashes in its filenames, so a cached page paired
/// with a rebuilt script is the failure to avoid.
fn serve_asset(panes: &Panes, rel: &str) -> Response<BoxBody> {
    tab_atelier_web::assets::serve(panes.web_root(), rel).map_or_else(Panes::not_found, |mut response| {
        // Both a missing file and a rebuilt page want the same answer: send it
        // fresh, so a cached page is never paired with a script that changed.
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-cache"));
        response
    })
}

/// Serve a local asset or answer a pane.
async fn handle(req: Request<Incoming>, panes: &Panes, daemon: &Daemon) -> Response<BoxBody> {
    let (parts, body) = req.into_parts();
    let authorization = parts.headers.get(header::AUTHORIZATION).cloned();
    // The token may also arrive as `?token=` — a plain `<a href>` (the terminal
    // link) cannot set a header, and neither can an operator pasting a URL.
    let query_token = tab_atelier_web::query_token(parts.uri.query());
    let route = route::route(&Request::from_parts(parts.clone(), ()));

    // One gate for every pane and the remote API, before the match, rather than
    // the same check repeated in each arm. A method error is answered before it:
    // the verb is wrong whether or not the caller holds the token.
    if route.is_protected() && !panes.authorized(authorization.as_ref(), query_token.as_deref()) {
        return Panes::unauthorized();
    }

    match route {
        // A UI file, read from the web root on disk. Not embedded: a rebuilt
        // page is a copy, not a rebuild of the server.
        Route::Asset(rel) => serve_asset(panes, &rel),
        // Nothing the Kiosk serves. There is no single daemon to relay to — to
        // reach a machine's tabs, it is a configured remote, not a fallback.
        Route::NotFound => Panes::not_found(),
        Route::Decisions { include_archived } => panes.list(include_archived),
        Route::DecisionFile { path } => panes.file(&path),
        Route::DecisionRead { id } => panes.read(&id),
        Route::DecisionTranch { id } => match read_field(body, "verdict").await {
            Ok(verdict) => panes.tranch(&id, &verdict),
            Err(response) => response,
        },
        Route::Reports => panes.reports(),
        Route::Intent => match read_field(body, "content").await {
            Ok(content) => panes.intent(&content),
            Err(response) => response,
        },
        Route::IntentList => panes.intents_list(),
        Route::IntentNew => match read_json(body).await {
            Ok(value) => {
                let field = |name: &str| value.get(name).and_then(|v| v.as_str()).unwrap_or("");
                panes.intent_new(field("title"), field("repo"), field("pitch"))
            }
            Err(response) => response,
        },
        Route::IntentView { slug } => panes.intent_view(&slug),
        Route::IntentPromote { slug } => panes.intent_promote(&slug),
        Route::IntentAsk { slug } => match read_field(body, "text").await {
            Ok(text) => panes.intent_ask(&slug, &text),
            Err(response) => response,
        },
        Route::Catalog => Panes::catalog(),
        Route::RemotesList => panes.remotes_list(),
        Route::RemoteAdd => match read_json(body).await {
            Ok(value) => {
                let field = |name: &str| value.get(name).and_then(|v| v.as_str()).unwrap_or("");
                panes.remote_add(field("label"), field("url"), field("token"))
            }
            Err(response) => response,
        },
        Route::RemoteRemove { id } => panes.remote_remove(&id),
        Route::RemoteTabs { id } => remote_call(panes, daemon, &id, "/tabs", None).await,
        Route::RemoteTabCreate { id } => match read_json(body).await {
            Ok(spec) => remote_call(panes, daemon, &id, "/tabs", Some(spec)).await,
            Err(response) => response,
        },
        Route::RemoteTabClose { id, tab } => remote_tab_close(panes, daemon, &id, &tab).await,
        Route::RemoteTabMeta { id, tab } => match read_json(body).await {
            Ok(change) => remote_tab_meta(panes, daemon, &id, &tab, &change).await,
            Err(response) => response,
        },
        Route::RemoteTabView { id, tab } => remote_tab_view(panes, daemon, &id, &tab).await,
        Route::RemoteBroadcast { id } => remote_broadcast(panes, daemon, &id).await,
        Route::RemoteIntentLaunch { id, slug } => remote_intent_launch(panes, daemon, &id, &slug).await,
        Route::RemoteCards { id } => remote_cards(panes, daemon, &id).await,
        Route::Fleet => fleet(panes, daemon).await,
        Route::MethodNotAllowed(allow) => Panes::method_not_allowed(allow),
    }
}

/// `POST /kiosk/remotes/<id>/tabs/<uuid>/close` — close a tab on a remote.
///
/// The Kiosk knows the tab by uuid (that is what the fleet reports and what
/// `/tabs/by-id/` addresses), so the close is by uuid and survives a tab that
/// moved position.
async fn remote_tab_close(panes: &Panes, daemon: &Daemon, id: &str, tab: &str) -> Response<BoxBody> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Panes::remote_error(&error),
    };
    let daemon = daemon.clone();
    let path = format!("/tabs/by-id/{tab}");
    let answered = tokio::task::spawn_blocking(move || daemon.delete(&remote, &path)).await;
    match answered {
        Ok(Ok((status, _))) if (200..300).contains(&status) => {
            json_response(StatusCode::OK, &serde_json::json!({ "closed": tab }))
        }
        Ok(Ok((status, text))) => relay_response(status, text),
        _ => Panes::bad_gateway(),
    }
}

/// `POST /kiosk/remotes/<id>/tabs/<uuid>/meta` — set one card label on a tab.
///
/// One key per call, matching the daemon's own `/tabs/by-id/<uuid>/meta`: the
/// card is a set of keys, and the operator edits one field at a time.
async fn remote_tab_meta(
    panes: &Panes,
    daemon: &Daemon,
    id: &str,
    tab: &str,
    change: &serde_json::Value,
) -> Response<BoxBody> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Panes::remote_error(&error),
    };
    let daemon = daemon.clone();
    let path = format!("/tabs/by-id/{tab}/meta");
    let body = change.clone();
    let answered = tokio::task::spawn_blocking(move || daemon.post(&remote, &path, &body)).await;
    match answered {
        Ok(Ok((status, text))) => relay_response(status, text),
        _ => Panes::bad_gateway(),
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
async fn remote_tab_view(panes: &Panes, daemon: &Daemon, id: &str, tab: &str) -> Response<BoxBody> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Panes::remote_error(&error),
    };
    let daemon = daemon.clone();
    let base = remote.url.clone();
    let share_path = format!("/tabs/by-id/{tab}/share");
    let answered = tokio::task::spawn_blocking(move || daemon.post(&remote, &share_path, &serde_json::json!({}))).await;
    let token = match answered {
        Ok(Ok((status, text))) if (200..300).contains(&status) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("rw").and_then(|t| t.as_str()).map(str::to_string)),
        _ => None,
    };
    let Some(token) = token else {
        return json_response(
            StatusCode::BAD_GATEWAY,
            &serde_json::json!({ "error": "could not mint a share token" }),
        );
    };
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, format!("{base}/tabs/by-id/{tab}/view?token={token}"))
        .body(full(""))
        .expect("static redirect")
}

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

/// `POST /kiosk/remotes/<id>/broadcast` — type the same message into every agent
/// tab on a remote, so each agent publishes its card.
///
/// Only the tabs that are a **live external agent** get it — `agent_of`, not
/// merely "has an `agent_kind`": a bash shell has a kind too, and the brief was
/// once typed into bash sessions. A dead agent ("early try", agent gone) is
/// skipped as well. A locked tab refuses input (counted, not forced). The reply
/// says how many of how many were reached.
async fn remote_broadcast(panes: &Panes, daemon: &Daemon, id: &str) -> Response<BoxBody> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Panes::remote_error(&error),
    };
    let lister = daemon.clone();
    let listing = {
        let remote = remote.clone();
        tokio::task::spawn_blocking(move || lister.get(&remote, "/tabs")).await
    };
    let agent_ids: Vec<String> = match listing {
        Ok(Ok((status, text))) if (200..300).contains(&status) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("tabs")
                    .and_then(serde_json::Value::as_array)
                    .map(|tabs| crate::agent::broadcast_targets(tabs))
            })
            .unwrap_or_default(),
        _ => return Panes::bad_gateway(),
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
    json_response(StatusCode::OK, &serde_json::json!({ "started": total }))
}

/// Type `message` into a tab and submit it — the way `tab-atelier dispatch`
/// does, not the way a naive paste does.
///
/// A prompt and a trailing `\r` that arrive in the SAME write are treated by
/// `claude` as pasted multiline text: the `\r` becomes a newline inside the
/// paste, the tab shows `[Pasted text #1]` and sits idle waiting for a real
/// Enter. So: send the paste, watch the screen until it stops changing (the
/// paste has finished rendering), then send the `\r` as a SEPARATE write — the
/// same settle dance as `src/cli/delegate.rs`, with a floor and a cap so a
/// silent-but-busy tab still gets its Enter.
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

/// `GET /kiosk/fleet` — every configured server and its tabs, with their cards.
///
/// The servers are the ones the operator added in Serveurs and no others — the
/// Kiosk does not invent a local one. It reaches each with ITS token (never the
/// browser's) and returns what the landing page shows per tab: its id, name,
/// cwd, and its **meta** — which is the agent card (assignment, specialty,
/// orchestrator, …). The scrollback is left out; a landing page does not need
/// it. A server that cannot be reached is reported as `{error: unreachable}`
/// rather than failing the whole list: one down server must not blank the fleet.
async fn fleet(panes: &Panes, daemon: &Daemon) -> Response<BoxBody> {
    let mut servers = Vec::new();
    for remote in panes.fleet_targets() {
        let id = remote.id.clone();
        let label = remote.label.clone();
        let daemon = daemon.clone();
        let answered = tokio::task::spawn_blocking(move || daemon.get(&remote, "/tabs")).await;
        let entry = match answered {
            Ok(Ok((status, text))) if (200..300).contains(&status) => {
                let tabs = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|value| value.get("tabs").cloned())
                    .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
                let lean: Vec<serde_json::Value> = tabs
                    .as_array()
                    .map(|tabs| {
                        tabs.iter()
                            // The daemon's own auto-injector tab ("brain") is not
                            // the operator's: it is a session-less harness tab with
                            // no led, and it does not belong in the user's list.
                            .filter(|tab| tab.get("agent_kind").and_then(serde_json::Value::as_str) != Some("brain"))
                            .map(|tab| {
                                serde_json::json!({
                                    "id": tab.get("id"),
                                    "name": tab.get("name"),
                                    "cwd": tab.get("cwd"),
                                    // The card: the operator's labels for this tab.
                                    "meta": tab.get("meta"),
                                    // And how it is doing — the led the card shows,
                                    // plus the agent kind ("is a claude in it?").
                                    "led": tab.get("led"),
                                    // Only a real external agent — a shell's kind
                                    // is not an agent, so no badge for it.
                                    "agent_kind": crate::agent::agent_of(tab),
                                    "agent_state": tab.get("agent_state"),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::json!({ "id": id, "label": label, "tabs": lean })
            }
            _ => serde_json::json!({ "id": id, "label": label, "error": "unreachable" }),
        };
        servers.push(entry);
    }
    json_response(StatusCode::OK, &serde_json::json!({ "servers": servers }))
}

/// `GET /kiosk/remotes/<id>/cards` — a remote's agent cards, derived from the
/// meta labels of its tabs.
///
/// The daemon already lists each tab's `meta`; this shapes the conventional set
/// into a card, so the dashboard reads one shape instead of knowing the keys.
/// Tabs with no card keys are omitted — a plain tab is not reported as an empty
/// card.
async fn remote_cards(panes: &Panes, daemon: &Daemon, id: &str) -> Response<BoxBody> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Panes::remote_error(&error),
    };
    let daemon = daemon.clone();
    let answered = tokio::task::spawn_blocking(move || daemon.get(&remote, "/tabs")).await;
    let Ok(Ok((status, text))) = answered else {
        return Panes::bad_gateway();
    };
    if !(200..300).contains(&status) {
        return relay_response(status, text);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Panes::bad_gateway();
    };
    let cards: Vec<serde_json::Value> = value
        .get("tabs")
        .and_then(|tabs| tabs.as_array())
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
    json_response(StatusCode::OK, &serde_json::json!({ "cards": cards }))
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
async fn remote_intent_launch(panes: &Panes, daemon: &Daemon, id: &str, slug: &str) -> Response<BoxBody> {
    // The intention is ours: read it before reaching out, so an unknown slug is
    // a 404 and no remote is asked to spawn anything.
    if panes.intentions().read(slug).is_err() {
        return json_response(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "error": "no such intention" }),
        );
    }
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Panes::remote_error(&error),
    };
    let spec = serde_json::json!({ "sync": true, "meta": { "intent": slug } });
    let daemon = daemon.clone();
    let answered = tokio::task::spawn_blocking(move || daemon.post(&remote, "/tabs", &spec)).await;
    let Ok(Ok((status, text))) = answered else {
        return Panes::bad_gateway();
    };
    if !(200..300).contains(&status) {
        return relay_response(status, text);
    }
    // The daemon's `sync` reply carries `{"id": "<uuid>", …}`; record it so the
    // intention can find its worker again.
    let tab_id = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.get("id").and_then(|id| id.as_str()).map(str::to_string));
    if let Some(tab_id) = &tab_id {
        let _ = panes.intentions().set_tab(slug, tab_id);
    }
    json_response(
        StatusCode::OK,
        &serde_json::json!({ "slug": slug, "tab": tab_id, "launched": tab_id.is_some() }),
    )
}

/// Reaches a configured remote with its token, server-side, and relays the
/// answer. The browser sent only the Kiosk token; the remote's never leaves here.
async fn remote_call(
    panes: &Panes,
    daemon: &Daemon,
    id: &str,
    path: &str,
    spec: Option<serde_json::Value>,
) -> Response<BoxBody> {
    let remote = match panes.remote(id) {
        Ok(remote) => remote,
        Err(error) => return Panes::remote_error(&error),
    };
    // ureq is blocking, so the hop runs on a blocking thread and the reactor
    // keeps serving other connections while a slow remote answers.
    let daemon = daemon.clone();
    let path = path.to_string();
    let answered = tokio::task::spawn_blocking(move || {
        spec.map_or_else(
            || daemon.get(&remote, &path),
            |value| daemon.post(&remote, &path, &value),
        )
    })
    .await;
    match answered {
        Ok(Ok((status, text))) => relay_response(status, text),
        // The reason may name an internal host: the caller is told only that the
        // remote is the one that failed.
        _ => Panes::bad_gateway(),
    }
}

/// Passes a remote's own answer through: status, and the JSON body as it came.
fn relay_response(status: u16, text: String) -> Response<BoxBody> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY))
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(text))
        .expect("a status and body from a real response")
}

/// Reads a JSON request body, bounded like any relayed body.
async fn read_json(body: Incoming) -> Result<serde_json::Value, Response<BoxBody>> {
    let bytes = match Limited::new(body, MAX_REQUEST_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Err(Panes::bad_request()),
    };
    serde_json::from_slice(&bytes).map_err(|_| Panes::bad_request())
}

/// Reads one string field out of a JSON request body, bounded like any relayed
/// body. A missing field and malformed JSON are the same 400: the pane knows
/// what it asked for and got neither.
async fn read_field(body: Incoming, field: &str) -> Result<String, Response<BoxBody>> {
    let bytes = match Limited::new(body, MAX_REQUEST_BODY).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Err(Panes::bad_request()),
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| value.get(field).and_then(|v| v.as_str()).map(str::to_string))
        .ok_or_else(Panes::bad_request)
}
