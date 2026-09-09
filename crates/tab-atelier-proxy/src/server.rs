// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! The HTTP surface: the proxied Anthropic path, the admin API behind it, and
//! the web UI.
//!
//! Two separate credentials, on purpose:
//!
//! * a **user key** ([`crate::users`]) opens `/relay/anthropic/*` and nothing
//!   else. It lives in a developer's `ANTHROPIC_BASE_URL` environment, so it
//!   travels widely and must not be able to administer anything.
//! * the **admin token** opens `/api/*` and the UI. It stays on the server.
//!
//! A user key rejected by `/api/users` is not a mistake to smooth over; it is
//! the boundary working.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response};

use crate::users::{self, Account, Store, constant_time_eq};
use crate::{account, egress, provider, qos, routing, usage};

type Body = http_body_util::combinators::BoxBody<Bytes, Infallible>;

/// Everything a request handler needs.
pub struct State {
    pub store: Mutex<Store>,
    /// Separate lock from the accounts: recording usage happens on every
    /// proxied request, and it must not queue behind an admin listing users.
    pub usage: Mutex<usage::Store>,
    /// Who goes next when the shared quota is tight.
    pub sched: Mutex<qos::Sched>,
    /// How much of the shared plan is left, polled from upstream.
    pub account: Mutex<account::Monitor>,
    /// Everywhere a request can go.
    pub registry: provider::Registry,
    /// Providers upstream has told us to leave alone, and until when (unix
    /// seconds). Keyed by provider id, because a 429 from one says nothing
    /// about another — that is the entire point of having more than one.
    pub provider_backoff: Mutex<std::collections::BTreeMap<String, u64>>,
    /// Woken when capacity frees up, so a queued call retries promptly instead
    /// of sitting out its full backoff.
    pub wake: tokio::sync::Notify,
    pub admin_token: String,
    pub web_root: Option<std::path::PathBuf>,
}

fn text(code: u16, msg: &str) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(msg.to_owned())).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

fn json(code: u16, body: &str) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        // The admin API is same-origin only: no CORS headers, so a page on
        // another origin cannot read a response even if it can send a request.
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body.to_owned())).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `/api/users/<account>/keys/<key>` → the two ids.
fn split_key_path(path: &str) -> (&str, &str) {
    let rest = path.trim_start_matches("/api/users/");
    rest.split_once("/keys/").unwrap_or((rest, ""))
}

/// One key, without its hash.
fn key_json(k: &users::Key) -> serde_json::Value {
    serde_json::json!({
        "id": k.id,
        "name": k.name,
        "created_at": k.created_at,
        "first_used_at": k.first_used_at,
        "last_used_at": k.last_used_at,
        "last_used_ip": k.last_used_ip,
        "disabled": k.disabled,
    })
}

fn account_json(a: &Account) -> serde_json::Value {
    // Never the key hash. It is not a secret you can use, but it is the input
    // to an offline guess, and the UI has no reason to hold it.
    serde_json::json!({
        "id": a.id,
        "first_name": a.first_name,
        "last_name": a.last_name,
        "email": a.email,
        "created_at": a.created_at,
        "disabled": a.disabled,
        "weight": a.weight,
        // Every key, each with its own history. The hash is never included:
        // it is not a usable secret, but it is the input to an offline guess
        // and the UI has no reason to hold it.
        "keys": a.keys.iter().map(|k| serde_json::json!({
            "id": k.id,
            "name": k.name,
            "created_at": k.created_at,
            "first_used_at": k.first_used_at,
            "last_used_at": k.last_used_at,
            "last_used_ip": k.last_used_ip,
            "disabled": k.disabled,
        })).collect::<Vec<_>>(),
        // The account's most recent activity across all its keys, for the
        // row summary.
        "last_used_at": a.keys.iter().filter_map(|k| k.last_used_at).max(),
        "has_key": a.keys.iter().any(users::Key::active),
    })
}

/// Pull a bearer-ish credential out of any header a client might use.
///
/// A claude client sends `x-api-key`; our own forwarding hop and the web UI
/// send `Authorization: Bearer`. Accepting both avoids the class of bug where
/// the credential is right and the envelope is not.
///
/// `x-admin-token` is the third, and it exists for a reason worth writing
/// down: `Authorization` is the header reverse proxies and auth modules are
/// most likely to consume or strip before it ever reaches a backend. When that
/// happens the proxy sees no credential at all and can only report that, which
/// looks exactly like a wrong token. A plainly-named custom header passes
/// through arrangements that eat the standard one, so the UI sends both and
/// whichever survives is used.
fn presented(req: &Request<Incoming>) -> String {
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    header("x-api-key")
        .or_else(|| header("x-admin-token"))
        .or_else(|| {
            req.headers()
                .get(hyper::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_default()
}

/// Serve one request.
///
/// # Errors
/// Never — the `Result` is `Infallible`, and exists only because hyper's
/// `service_fn` requires one. Failures become status codes, not errors: a
/// proxy that drops a connection instead of answering 502 tells the client
/// nothing about what went wrong.
pub async fn handle(
    req: Request<Incoming>,
    state: Arc<State>,
    peer: std::net::IpAddr,
) -> Result<Response<Body>, Infallible> {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();

    if path.starts_with("/relay/anthropic") {
        return Ok(anthropic(req, state, peer).await);
    }
    // The account's own statistics, opened by the account's own key — the
    // route a person hands to their agent. Deliberately NOT under /api/,
    // which is the admin surface: this one is meant to be given away.
    if path == "/me/usage" {
        return Ok(me_usage(&req, &state, peer));
    }
    if path.starts_with("/api/") {
        return Ok(admin(req, state).await);
    }
    if method == Method::GET || method == Method::HEAD {
        return Ok(web(&path, &state));
    }
    Ok(text(404, "not found"))
}

// ── the proxied path ────────────────────────────────────────────────

/// Look up the key's account and record that it was used, from where.
fn authenticate_and_stamp(state: &State, key: &str, ip: &str) -> Option<Account> {
    // The KEY is stamped, not the account: "last used from here" says nothing
    // when several keys share an account, which is the whole reason they are
    // named separately.
    let found = {
        let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let found = store.authenticate_key(key).map(|(a, k)| (a.clone(), k.id.clone()));
        if let Some((_, ref key_id)) = found {
            store.touch(key_id, Some(ip));
        }
        found
    };
    found.map(|(a, _)| a)
}

/// Who the caller actually is, as far as this proxy can honestly tell.
///
/// The socket's peer is the truth when the client connects directly. It is NOT
/// the truth behind the TLS terminator this is meant to run behind — there the
/// peer is always the terminator, and `X-Forwarded-For` carries the real
/// address.
///
/// So the header is honoured ONLY when the connection came from loopback,
/// which is where that terminator lives. Trusting it from anywhere else would
/// let a caller write its own address into the log by setting a header, which
/// is worse than recording nothing.
fn client_ip(req: &Request<Incoming>, peer: std::net::IpAddr) -> String {
    let forwarded = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    match forwarded {
        Some(fwd) if peer.is_loopback() => fwd.to_owned(),
        _ => peer.to_string(),
    }
}

async fn anthropic(req: Request<Incoming>, state: Arc<State>, peer: std::net::IpAddr) -> Response<Body> {
    let method = req.method().clone();
    let sub = req
        .uri()
        .path()
        .strip_prefix("/relay/anthropic")
        .unwrap_or("")
        .to_owned();
    let sub_pq = req.uri().query().map_or_else(|| sub.clone(), |q| format!("{sub}?{q}"));

    // Claude Code probes reachability with an UNAUTHENTICATED HEAD/GET before
    // it has a credential to present. Answering 401 here reads as "this
    // endpoint is broken" and the client never attempts a real call, so this
    // is answered first — and kept to an exact path, because an
    // unauthenticated branch is security surface.
    if sub == "/api/hello" && matches!(method, Method::HEAD | Method::GET) {
        let body = if method == Method::HEAD { "" } else { "{}" };
        return Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)).boxed())
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()));
    }

    let key = presented(&req);
    let ip = client_ip(&req, peer);
    let who = authenticate_and_stamp(&state, &key, &ip);
    let Some(account) = who else {
        // Say which of the two credentials was wrong without printing either.
        // "unauthorized" alone leaves an operator guessing between a revoked
        // account, a typo, and the admin token in the wrong place.
        let diagnosis = if key.is_empty() {
            "no key presented"
        } else if constant_time_eq(key.as_bytes(), state.admin_token.as_bytes()) {
            "that is the ADMIN token — the proxy path takes a user key"
        } else {
            "no active account has that key (revoked, disabled, or mistyped)"
        };
        log::warn!(
            "proxy: 401 on {method} /relay/anthropic{sub} ({} chars presented): {diagnosis}",
            key.chars().count()
        );
        return text(401, &format!("tab-atelier-proxy: unauthorized ({diagnosis})"));
    };
    log::info!(
        "proxy: {method} {sub} for {} <{}> from {ip}",
        account.display_name(),
        account.email
    );

    let client_beta = req
        .headers()
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let content_type = req
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let is_post = method == Method::POST;
    let (_parts, body) = req.into_parts();
    let body = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return text(400, "bad request body"),
    };

    // Only /v1/messages spends tokens; a metadata call should not queue
    // behind a fleet's generations.
    let metered = is_post && sub.contains("/messages");
    let (body, route) = match shape_and_admit(&state, &account, body, metered).await {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    let fwd = Forward {
        sub_pq,
        is_post,
        content_type,
        client_beta,
        body,
        account_id: account.id.clone(),
        weight: account.weight,
        metered,
        route: route.clone(),
        state: Arc::clone(&state),
    };
    // Bridge ureq's blocking reader to an async hyper stream: the blocking task
    // sends (status, content-type) over a oneshot, then pumps chunks over an
    // mpsc. Without this an SSE response would only reach the client once the
    // whole generation finished, which for a long answer looks like a hang.
    let (meta_tx, meta_rx) = tokio::sync::oneshot::channel::<Result<(u16, Option<String>), String>>();
    let (body_tx, body_rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    tokio::task::spawn_blocking(move || forward(&fwd, meta_tx, &body_tx));

    let meta = match meta_rx.await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return text(502, &format!("tab-atelier-proxy: {e}")),
        Err(_) => return text(502, "tab-atelier-proxy: forward task died"),
    };
    let stream = futures_util::stream::unfold(body_rx, |mut rx| async move {
        rx.recv().await.map(|b| (Ok::<_, Infallible>(Frame::data(b)), rx))
    });
    streamed(meta, stream, &route)
}

/// Wrap the upstream stream in a response that says where it came from.
///
/// Never route silently: a caller that got something other than what it asked
/// for is entitled to know which provider and model answered, or results stop
/// being reproducible and a bug report names the wrong model.
fn streamed<S>(meta: (u16, Option<String>), stream: S, route: &routing::Route) -> Response<Body>
where
    S: futures_util::Stream<Item = Result<Frame<Bytes>, Infallible>> + Send + Sync + 'static,
{
    let mut builder = Response::builder().status(meta.0);
    if let Some(ct) = meta.1 {
        builder = builder.header("content-type", ct);
    }
    builder = builder.header(
        "x-tab-atelier-proxy-route",
        format!("{}/{}", route.provider_id, route.model_id),
    );
    // A request that named no model was not "rerouted from nothing" — there
    // was nothing to reroute from, and saying so would be noise.
    if let (Some(from), Some(reason)) = (route.changed_from.as_ref().filter(|f| !f.is_empty()), route.reason) {
        let name = match reason {
            "degraded" => "x-tab-atelier-proxy-degraded",
            _ => "x-tab-atelier-proxy-rerouted",
        };
        builder = builder.header(name, from.clone());
    }
    builder
        .body(StreamBody::new(stream).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// Degrade the request if the plan is tight, then wait for a turn.
///
/// Returns the (possibly rewritten) body and what was swapped, or the 429 to
/// send back. Split out of `anthropic` because it is the whole `QoS` decision
/// and reads better as one unit than inline in the middle of the forwarder.
async fn shape_and_admit(
    state: &Arc<State>,
    account: &Account,
    body: Bytes,
    metered: bool,
) -> Result<(Bytes, routing::Route), Response<Body>> {
    if !metered {
        // Not a generation: it still has to go somewhere, but no class
        // reasoning applies.
        return Ok((
            body,
            routing::Route {
                provider_id: "anthropic".to_owned(),
                model_id: String::new(),
                class: provider::Class::Balanced,
                changed_from: None,
                reason: None,
            },
        ));
    }
    // Choose the destination BEFORE admission, so the call is judged at the
    // price it will actually pay rather than the one it asked for.
    let requested = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_default();
    let health = provider_health(state);
    let Some(route) = routing::choose(&state.registry, &requested, &health, |v| std::env::var(v).ok()) else {
        // Nothing configured can serve this at any class. A guess would be
        // worse than saying so.
        return Err(text(
            503,
            "tab-atelier-proxy: no provider available for this request (all blocked, or none configured)",
        ));
    };
    if let Some(reason) = route.reason {
        log::info!(
            "proxy: {} {reason} {requested} → {}/{}",
            account.display_name(),
            route.provider_id,
            route.model_id
        );
    }
    let body = if route.model_id == requested {
        body
    } else {
        rewrite_model(&body, &route.model_id).map_or(body, Bytes::from)
    };

    let est = qos::estimate_cost(&body);
    if let Err(retry_after) = admit(state, &account.id, account.weight, est).await {
        log::warn!(
            "proxy: 429 for {} after waiting — retry in {retry_after}s",
            account.display_name()
        );
        let mut resp = text(
            429,
            &format!("tab-atelier-proxy: the shared quota is saturated; retry in {retry_after}s"),
        );
        if let Ok(v) = hyper::header::HeaderValue::from_str(&retry_after.to_string()) {
            resp.headers_mut().insert(hyper::header::RETRY_AFTER, v);
        }
        // A refused call still happened, and an operator looking at a quiet
        // graph should see the refusals.
        record(state, &account.id, None, usage::Tokens::default(), 429);
        return Err(resp);
    }
    Ok((body, route))
}

/// Swap the `model` field, leaving everything else exactly as it arrived.
fn rewrite_model(body: &[u8], model: &str) -> Option<Vec<u8>> {
    let mut v = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    *v.get_mut("model")? = serde_json::Value::String(model.to_owned());
    serde_json::to_vec(&v).ok()
}

/// What routing needs to know about each provider right now.
///
/// The subscription reports its own utilisation, so that is used directly.
/// Everyone else is judged only by whether they have recently refused us —
/// there is no equivalent signal, and inventing one would keep working
/// providers idle.
fn provider_health(state: &Arc<State>) -> impl Fn(&str) -> routing::Health + '_ {
    let now = usage::now_secs();
    let plan = state
        .account
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .utilization();
    let backoff = state
        .provider_backoff
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    move |id: &str| routing::Health {
        utilization: if id == "anthropic" { plan } else { None },
        backoff_secs: backoff.get(id).map_or(0, |until| until.saturating_sub(now)),
    }
}

/// Per-account usage for the dashboard.
fn usage_report(state: &Arc<State>, store: &Store, query: &str) -> Response<Body> {
    let hours = path_hours(query).unwrap_or(24 * 7).clamp(1, usage::RETAIN_HOURS);
    let body = {
        let u = state.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let per_user: Vec<_> = store
            .accounts()
            .iter()
            .map(|a| {
                let mut v = usage_json(&u, &a.id, hours);
                if let Some(o) = v.as_object_mut() {
                    o.insert("user".to_owned(), account_json(a));
                }
                v
            })
            .collect();
        serde_json::json!({ "hours": hours, "users": per_user }).to_string()
    };
    json(200, &body)
}

/// What the dashboard shows about pressure: what the plan says about itself,
/// and what the scheduler is doing about it.
fn pressure_json(state: &Arc<State>) -> Response<Body> {
    let now = now_ms();
    // One tiny scope per lock: both are on the request path, so neither is
    // held across the other or across building the response.
    let plan = {
        let acct = state.account.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        serde_json::json!({
            // The honest signal — upstream's own report about the shared plan,
            // not an inference from our accounting. `null` when the monitor
            // has gone quiet: `health` says why, and the UI must show that
            // rather than the last number it happened to see.
            "utilization": acct.utilization(),
            "latest": acct.latest(),
            "health": acct.health(usage::now_secs()),
            "history": acct.recent(),
        })
    };
    let scheduler = {
        let sched = state.sched.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        sched.snapshot(now)
    };
    json(
        200,
        &serde_json::json!({
            "plan": plan,
            "scheduler": scheduler,
            // The point above which a provider stops being first choice.
            // Routing prefers moving the work to another provider at this
            // level; only when none is left does the class step down.
            "strained_above": routing::STRAINED_ABOVE,
            "providers": state.registry.summary(),
        })
        .to_string(),
    )
}

/// Wait for the scheduler to let this call through.
///
/// Queue-then-reject: hold briefly for a turn, and past [`qos::MAX_WAIT`]
/// answer 429 with a figure the client can act on. Holding a connection open
/// indefinitely is worse for everyone — the client would retry anyway, and
/// meanwhile the socket and its task are spent doing nothing.
///
/// Returns `Err(retry_after_secs)` when the caller should be turned away.
async fn admit(state: &Arc<State>, id: &str, weight: u32, est: u64) -> Result<(), u64> {
    let started = std::time::Instant::now();
    loop {
        let decision = {
            let mut sched = state.sched.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            sched.try_admit(id, weight, est, now_ms(), started.elapsed())
        };
        match decision {
            qos::Decision::Go { .. } => return Ok(()),
            qos::Decision::Reject { retry_after } => return Err(retry_after),
            qos::Decision::Wait(d) => {
                // Woken early when a call settles and frees capacity, so a
                // queued request does not sit out a full sleep for nothing.
                let _ = tokio::time::timeout(d, state.wake.notified()).await;
            }
        }
    }
}

/// One request on its way upstream, moved into the blocking task.
struct Forward {
    sub_pq: String,
    is_post: bool,
    content_type: String,
    client_beta: Option<String>,
    body: Bytes,
    weight: u32,
    /// Whether this call was admitted by the scheduler, and so has an estimate
    /// outstanding that must be settled.
    metered: bool,
    /// Where this is going, chosen by [`crate::routing`].
    route: routing::Route,
    /// Who to bill. Carried down rather than looked up again, because by the
    /// time the response finishes the account may have been deleted — the call
    /// still happened and still spent tokens.
    account_id: String,
    state: Arc<State>,
}

/// The blocking half: authenticate to Anthropic, send, and pump the response.
///
/// The upstream status and body are passed through untouched, including error
/// statuses. A proxy that turned a 429 into its own 502 would hide both the
/// reason and the `retry-after` the client needs.
fn forward(
    f: &Forward,
    // By value: a oneshot Sender is consumed by `send`, which is also what
    // makes "exactly one answer" a type-level guarantee rather than a habit.
    meta_tx: tokio::sync::oneshot::Sender<Result<(u16, Option<String>), String>>,
    body_tx: &tokio::sync::mpsc::Sender<Bytes>,
) {
    let (base, auth) = match destination(&f.state, &f.route.provider_id) {
        Ok(d) => d,
        Err(e) => {
            let _ = meta_tx.send(Err(e));
            return;
        }
    };
    let url = format!("{base}{}", f.sub_pq);
    let agent = egress::relay_agent();
    let hdrs: Vec<(&str, String)> = vec![
        ("Content-Type", f.content_type.clone()),
        (auth.0, auth.1),
        ("anthropic-version", egress::ANTHROPIC_VERSION.to_owned()),
        // The client's own beta flags are merged in, not replaced: a body field
        // gated behind a flag the client opted into is rejected upstream as an
        // unknown input if only our flags survive.
        (
            "anthropic-beta",
            egress::merge_beta(f.client_beta.as_deref(), egress::ANTHROPIC_BETA),
        ),
    ];
    let sent = if f.is_post {
        let mut rb = agent.post(&url);
        for (k, v) in &hdrs {
            rb = rb.header(*k, v);
        }
        rb.send(&f.body[..])
    } else {
        let mut rb = agent.get(&url);
        for (k, v) in &hdrs {
            rb = rb.header(*k, v);
        }
        rb.call()
    };
    let mut resp = match sent {
        Ok(r) => r,
        Err(e) => {
            let _ = meta_tx.send(Err(format!("upstream: {e}")));
            return;
        }
    };
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // Capacity is measured, not invented: Anthropic reports what is left on
    // every response, so the scheduler tracks the real plan rather than a
    // number somebody typed into a config.
    let header_num = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
    };
    let remaining = header_num("anthropic-ratelimit-tokens-remaining");
    let reset_in = header_num("anthropic-ratelimit-tokens-reset");
    let retry_after = header_num("retry-after");
    observe_upstream(f, status, remaining, reset_in, retry_after);
    // Count what Anthropic says it billed, read off the response as it goes
    // past. Asking the client to report its own usage would be unenforceable,
    // and re-tokenising the prompt here would be a guess.
    let mut sniffer = usage::Sniffer::new(ctype.as_deref());
    if meta_tx.send(Ok((status, ctype))).is_err() {
        // The client is gone, but the call was still made and still cost
        // tokens — so it is recorded anyway, just without a body to read.
        record(&f.state, &f.account_id, None, usage::Tokens::default(), status);
        return;
    }
    let mut reader = resp.body_mut().as_reader();
    let mut buf = [0u8; 8192];
    loop {
        match std::io::Read::read(&mut reader, &mut buf) {
            Ok(0) | Err(_) => break, // EOF, or an upstream read error
            Ok(n) => {
                sniffer.feed(&buf[..n]);
                if body_tx.blocking_send(Bytes::copy_from_slice(&buf[..n])).is_err() {
                    break; // client hung up
                }
            }
        }
    }
    let (model, tokens) = sniffer.finish();
    record(&f.state, &f.account_id, model.as_deref(), tokens, status);
    if f.metered {
        // Replace the estimate with what it really cost. An underestimate is
        // owed back out of the next turn; an overestimate is credited, or a
        // cautious estimator would throttle its own account forever.
        let est = qos::estimate_cost(&f.body);
        let actual = tokens.total().max(1);
        f.state
            .sched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .settle(&f.account_id, est, actual, now_ms());
        // Capacity just freed up; let anyone queued try again immediately.
        f.state.wake.notify_waiters();
    }
    let _ = f.weight;
}

/// The base URL and credential header for a chosen provider.
///
/// # Errors
/// The provider's credential is missing, which is a configuration problem
/// rather than something to paper over with an unauthenticated request.
fn destination(state: &State, provider_id: &str) -> Result<(String, (&'static str, String)), String> {
    let chosen = state.registry.get(provider_id);
    // An explicit upstream override replaces the SUBSCRIPTION's base URL —
    // that is what it exists for (a test's mock, or an ops redirect). It must
    // not silently retarget a third-party provider, whose URL is its identity.
    let uses_oauth = chosen.is_none_or(|p| matches!(p.auth, provider::Auth::ClaudeOauth));
    let base = match (uses_oauth, egress::upstream_override()) {
        (true, Some(o)) => o,
        _ => chosen.map_or_else(egress::upstream, |p| p.base_url.trim_end_matches('/').to_owned()),
    };

    // Credentials are fetched per request, never held: a refreshed OAuth token
    // is picked up without a restart, and nothing lands in a log.
    let auth = match chosen.map(|p| &p.auth) {
        None | Some(provider::Auth::ClaudeOauth) => {
            let t = egress::oauth_access_token().map_err(|e| format!("egress oauth: {e}"))?;
            ("Authorization", format!("Bearer {t}"))
        }
        // An Anthropic-compatible provider takes a key in x-api-key, the
        // convention its own SDKs use.
        Some(provider::Auth::ApiKeyEnv { var }) => match std::env::var(var) {
            Ok(k) if !k.trim().is_empty() => ("x-api-key", k),
            _ => return Err(format!("provider {provider_id} has no credential in ${var}")),
        },
    };
    Ok((base, auth))
}

/// Fold an upstream answer back into what the proxy knows.
///
/// A 429 stops traffic to THAT provider, not to every provider: its limit is
/// its own, and the next request routes elsewhere instead of waiting. Any
/// other answer clears the block — a provider that responds is working again.
fn observe_upstream(f: &Forward, status: u16, remaining: Option<u64>, reset_in: Option<u64>, retry_after: Option<u64>) {
    {
        let mut sched = f.state.sched.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if status == 429 {
            sched.on_429(retry_after, now_ms());
        } else {
            sched.observe(remaining, reset_in, now_ms());
        }
    }
    let mut blocked = f
        .state
        .provider_backoff
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if status == 429 {
        let wait = retry_after.unwrap_or(30).clamp(1, 300);
        blocked.insert(f.route.provider_id.clone(), usage::now_secs() + wait);
        log::warn!(
            "proxy: {} refused us (429) — routing elsewhere for {wait}s",
            f.route.provider_id
        );
    } else {
        blocked.remove(&f.route.provider_id);
    }
}

/// File one call against an account. Never fails the request it describes:
/// accounting is not worth a 502.
fn record(state: &State, account_id: &str, model: Option<&str>, tokens: usage::Tokens, status: u16) {
    let mut u = state.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    u.record(account_id, model, tokens, (200..300).contains(&status));
}

// ── an account's own statistics ─────────────────────────────────────

/// `?hours=N` off a query string, if present and sane.
fn path_hours(query: &str) -> Option<u64> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("hours="))
        .and_then(|v| v.parse::<u64>().ok())
}

/// Render one account's usage as JSON.
fn usage_json(u: &usage::Store, id: &str, hours: u64) -> serde_json::Value {
    let tok = |t: &usage::Tokens| {
        serde_json::json!({
            "input": t.input,
            "output": t.output,
            "cache_read": t.cache_read,
            "cache_write": t.cache_write,
            "total": t.total(),
        })
    };
    let window = |h: u64| {
        let (calls, errors, t) = u.totals(id, h);
        serde_json::json!({ "calls": calls, "errors": errors, "tokens": tok(&t) })
    };
    let series: Vec<_> = u
        .series(id, hours)
        .into_iter()
        .map(|b| {
            serde_json::json!({
                "hour": b.hour,
                "calls": b.calls,
                "errors": b.errors,
                "input": b.tokens.input,
                "output": b.tokens.output,
                "cache_read": b.tokens.cache_read,
                "cache_write": b.tokens.cache_write,
            })
        })
        .collect();
    let by_model: serde_json::Value = u.for_account(id).map_or_else(
        || serde_json::json!({}),
        |a| a.by_model.iter().map(|(m, t)| (m.clone(), tok(t))).collect(),
    );
    serde_json::json!({
        "all_time": window(0),
        "last_24h": window(24),
        "last_7d": window(24 * 7),
        "by_model": by_model,
        // Dense hourly buckets, oldest first, ending at the current hour.
        "series_hourly": series,
        "retained_hours": usage::RETAIN_HOURS,
    })
}

/// `GET /me/usage` — the account's own numbers, opened by its own key.
///
/// This is the route a person gives to their agent, so it is shaped to be
/// read by one: totals for the windows anybody actually asks about, a
/// per-model breakdown, and a dense hourly series. It answers for the caller
/// and nobody else — the key names the account, so there is no id to pass and
/// no way to ask about someone else.
fn me_usage(req: &Request<Incoming>, state: &State, peer: std::net::IpAddr) -> Response<Body> {
    if req.method() != Method::GET {
        return json(405, r#"{"error":"GET only"}"#);
    }
    let key = presented(req);
    // Reading your own statistics is a use of the key like any other, and an
    // agent polling this is exactly the traffic someone reviewing access wants
    // to see.
    let account = authenticate_and_stamp(state, &key, &client_ip(req, peer));
    let Some(account) = account else {
        return json(
            401,
            r#"{"error":"present your proxy key (x-api-key or Authorization: Bearer)"}"#,
        );
    };
    let hours = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("hours="))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(24 * 7)
        .clamp(1, usage::RETAIN_HOURS);
    let mut body = {
        let u = state.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        usage_json(&u, &account.id, hours)
    };
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "account".to_owned(),
            serde_json::json!({
                "name": account.display_name(),
                "email": account.email,
            }),
        );
    }
    json(200, &body.to_string())
}

// ── the admin API ───────────────────────────────────────────────────

async fn admin(req: Request<Incoming>, state: Arc<State>) -> Response<Body> {
    if state.admin_token.is_empty() {
        return json(503, r#"{"error":"no admin token configured"}"#);
    }
    let offered = presented(&req);
    if !constant_time_eq(offered.as_bytes(), state.admin_token.as_bytes()) {
        // "admin token required" alone leaves an operator with nothing to act
        // on — the same unhelpful 401 the proxy path deliberately avoids. Say
        // what was wrong without printing anyone's secret.
        let why = if offered.is_empty() {
            "no credential presented — nothing arrived in Authorization, x-admin-token or x-api-key, \
             which usually means something in front of the proxy stripped it"
        } else if state
            .store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .authenticate(&offered)
            .is_some()
        {
            "that is a USER key — it opens the Anthropic path and /me/usage, never this API"
        } else if offered.trim() != offered {
            "the token has leading or trailing whitespace — it was probably pasted with a newline"
        } else {
            "token mismatch — check `tab-atelier-proxy admin-token` ON THE SERVER, as the service user"
        };
        log::warn!("admin: 401 ({} chars presented): {why}", offered.chars().count());
        return json(
            401,
            &serde_json::json!({ "error": format!("admin token required: {why}") }).to_string(),
        );
    }
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or_default().to_owned();
    let (_parts, body) = req.into_parts();
    let body = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return json(400, r#"{"error":"bad body"}"#),
    };

    // Reports first: they take their own locks, so they must not run while the
    // account store is held for a mutation.
    match (&method, path.as_str()) {
        (&Method::GET, "/api/pressure") => return pressure_json(&state),
        (&Method::GET, "/api/usage") => {
            let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            return usage_report(&state, &store, &query);
        }
        _ => {}
    }
    mutate(&state, &method, &path, &body)
}

/// The account-mutating half of the admin API.
fn mutate(state: &Arc<State>, method: &Method, path: &str, body: &Bytes) -> Response<Body> {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).unwrap_or(serde_json::Value::Null);
    let field = |k: &str| parsed.get(k).and_then(|x| x.as_str()).unwrap_or("").to_owned();
    let who = |suffix: &str| {
        path.trim_start_matches("/api/users/")
            .trim_end_matches(suffix)
            .to_owned()
    };
    let mut store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    match (method, path) {
        (&Method::GET, "/api/users") => {
            let list: Vec<_> = store.accounts().iter().map(account_json).collect();
            json(200, &serde_json::json!({ "users": list }).to_string())
        }
        (&Method::POST, "/api/users") => {
            match store.add(&field("first_name"), &field("last_name"), &field("email")) {
                // The key is in this response and in no other, ever. The UI
                // shows it once and says so.
                Ok((a, key)) => json(
                    201,
                    &serde_json::json!({ "user": account_json(&a), "key": key }).to_string(),
                ),
                Err(e) => json(400, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        // Add a named key. Replaces the old `/rotate`: adding first and
        // removing later means a machine can be moved across without a moment
        // where nothing works.
        (&Method::POST, p) if p.ends_with("/keys") => {
            let name = field("name");
            let name = if name.is_empty() { "new" } else { &name };
            match store.add_key(&who("/keys"), name) {
                Ok((k, secret)) => json(
                    201,
                    // The secret is in this response and no other, ever.
                    &serde_json::json!({ "key": key_json(&k), "secret": secret }).to_string(),
                ),
                Err(e) => json(400, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        (&Method::DELETE, p) if p.contains("/keys/") => {
            let (owner, key_ref) = split_key_path(p);
            match store.remove_key(owner, key_ref) {
                Ok(k) => json(200, &serde_json::json!({ "removed": key_json(&k) }).to_string()),
                Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        (&Method::POST, p) if p.contains("/keys/") && p.ends_with("/disabled") => {
            let (owner, key_ref) = split_key_path(p.trim_end_matches("/disabled"));
            let disabled = parsed
                .get("disabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            match store.set_key_disabled(owner, key_ref, disabled) {
                Ok(k) => json(200, &serde_json::json!({ "key": key_json(&k) }).to_string()),
                Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        (&Method::POST, p) if p.ends_with("/disabled") => {
            let disabled = parsed
                .get("disabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            match store.set_disabled(&who("/disabled"), disabled) {
                Ok(a) => json(200, &serde_json::json!({ "user": account_json(&a) }).to_string()),
                Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        (&Method::POST, p) if p.ends_with("/weight") => {
            let weight = parsed
                .get("weight")
                .and_then(serde_json::Value::as_u64)
                .and_then(|w| u32::try_from(w).ok())
                .unwrap_or(1);
            match store.set_weight(&who("/weight"), weight) {
                Ok(a) => json(200, &serde_json::json!({ "user": account_json(&a) }).to_string()),
                Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        (&Method::DELETE, p) if p.starts_with("/api/users/") => {
            match store.remove(p.trim_start_matches("/api/users/")) {
                Ok(a) => {
                    // Deleting someone forgets their history too, or "remove"
                    // would leave their numbers on the dashboard forever with
                    // no name attached to them.
                    state
                        .usage
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .forget(&a.id);
                    json(200, &serde_json::json!({ "removed": account_json(&a) }).to_string())
                }
                Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        }
        _ => json(404, r#"{"error":"no such endpoint"}"#),
    }
}

// ── the web UI ──────────────────────────────────────────────────────

/// Libraries the distribution already packages, so this does not have to.
///
/// Debian ships Bootstrap as `libjs-bootstrap5`, which the .deb depends on.
/// Serving it from there rather than committing a copy means it is patched by
/// `apt upgrade` like everything else on the machine, and the repository does
/// not carry a third-party CSS blob nobody reviews.
///
/// It also removes the symlink that used to be needed to run from a source
/// checkout — an absolute link into /usr/share that was broken on any machine
/// without the package, and one more thing to explain.
///
/// Vue is NOT here because Debian does not package it (checked: no `libjs-vue`
/// or `node-vue` in trixie), so that one stays vendored, pinned by checksum.
fn distro_asset(rel: &str) -> Option<&'static str> {
    match rel {
        "vendor/bootstrap.min.css" => Some("/usr/share/javascript/bootstrap5/css/bootstrap.min.css"),
        _ => None,
    }
}

/// Serve the UI from `web_root`.
///
/// Path traversal is refused by rejecting any `..` component outright rather
/// than canonicalising and comparing: there is nothing under this root a
/// caller should reach by climbing, so the simplest rule that cannot be
/// subtly wrong is the right one.
fn web(path: &str, state: &State) -> Response<Body> {
    let Some(root) = state.web_root.as_ref() else {
        return text(404, "web UI not installed");
    };
    let rel = match path.trim_start_matches('/') {
        "" => "index.html",
        other => other,
    };
    if rel.split('/').any(|c| c == ".." || c == "." || c.is_empty()) {
        return text(400, "bad path");
    }
    let full = root.join(rel);
    let bytes = match std::fs::read(&full) {
        Ok(b) => b,
        // Not in our tree — try the distribution's copy before giving up.
        Err(_) => match distro_asset(rel).and_then(|p| std::fs::read(p).ok()) {
            Some(b) => b,
            None => return text(404, "not found"),
        },
    };
    let ctype = match full.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("map" | "json") => "application/json",
        _ => "application/octet-stream",
    };
    Response::builder()
        .status(200)
        .header("content-type", ctype)
        // The UI holds an admin token in memory; keep it out of any embedding
        // page and out of a referrer.
        .header("x-content-type-options", "nosniff")
        .header("x-frame-options", "DENY")
        .header("referrer-policy", "no-referrer")
        .body(Full::new(Bytes::from(bytes)).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// Run until Ctrl-C.
///
/// # Errors
/// The address could not be bound.
pub async fn serve(addr: SocketAddr, state: Arc<State>) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    log::info!("tab-atelier-proxy listening on http://{addr}");
    serve_on(listener, state).await
}

/// Serve on a listener that is already bound.
///
/// Split out so a test can bind port 0, learn which port it got, and then hand
/// the listener over — the alternative is guessing a free port, which is flaky
/// on a busy machine.
///
/// # Errors
/// Does not return in normal operation.
pub async fn serve_on(listener: tokio::net::TcpListener, state: Arc<State>) -> Result<(), String> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                log::warn!("accept: {e}");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(stream);
            let svc = service_fn(move |req| handle(req, Arc::clone(&state), peer.ip()));
            let _ = hyper::server::conn::http1::Builder::new()
                .keep_alive(true)
                .timer(hyper_util::rt::TokioTimer::new())
                // Slow-loris guard: bound how long a client may take to dribble
                // in its headers, or one byte every few seconds ties up a task
                // forever and the accept loop spawns one per connection.
                .header_read_timeout(std::time::Duration::from_secs(30))
                .serve_connection(io, svc)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_web_root_cannot_be_climbed_out_of() {
        let state = State {
            store: Mutex::new(Store::load(std::env::temp_dir().join("ta-proxy-web-test.json")).expect("store")),
            usage: Mutex::new(usage::Store::load(std::env::temp_dir().join("ta-proxy-web-usage"))),
            sched: Mutex::new(qos::Sched::new()),
            account: Mutex::new(account::Monitor::load(std::env::temp_dir())),
            wake: tokio::sync::Notify::new(),
            registry: provider::Registry::default(),
            provider_backoff: Mutex::new(std::collections::BTreeMap::new()),
            admin_token: "t".to_owned(),
            web_root: Some(std::path::PathBuf::from("/usr/share/tab-atelier-proxy/web")),
        };
        for attack in [
            "/../../../../etc/passwd",
            "/vendor/../../../etc/shadow",
            "/./secret",
            "//etc/passwd",
        ] {
            let resp = web(attack, &state);
            assert!(
                resp.status() == 400 || resp.status() == 404,
                "{attack} was not refused: {}",
                resp.status()
            );
        }
    }

    /// Bootstrap comes from the distribution, so neither the repository nor
    /// the package carries a copy — and a source checkout needs no symlink.
    #[test]
    fn bootstrap_is_served_from_the_distribution_package() {
        assert_eq!(
            distro_asset("vendor/bootstrap.min.css"),
            Some("/usr/share/javascript/bootstrap5/css/bootstrap.min.css")
        );
        // Only the libraries the distribution actually packages. Vue is not
        // one of them, so it must stay vendored rather than 404 at runtime.
        assert_eq!(distro_asset("vendor/vue.global.prod.js"), None);
        assert_eq!(distro_asset("app.js"), None);
        assert_eq!(distro_asset("../../../etc/passwd"), None);

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        assert!(
            !root.join("vendor/bootstrap.min.css").exists(),
            "a local copy would shadow the distribution's and stop getting updates"
        );
        assert!(
            root.join("vendor/vue.global.prod.js").is_file(),
            "Vue has no distribution package, so it has to be in the tree"
        );
    }

    #[test]
    fn an_account_serialises_without_its_hash() {
        let a = Account {
            id: "id-1".to_owned(),
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
            email: "ada@example.org".to_owned(),
            created_at: 1,
            keys: vec![users::Key {
                id: "k-1".to_owned(),
                name: "laptop".to_owned(),
                hash: "deadbeef".to_owned(),
                created_at: 1,
                first_used_at: None,
                last_used_at: None,
                last_used_ip: None,
                disabled: false,
            }],
            key_hash: String::new(),
            legacy_first_used_at: None,
            legacy_last_used_at: None,
            legacy_last_used_ip: None,
            disabled: false,
            weight: 1,
        };
        let json = account_json(&a).to_string();
        assert!(
            !json.contains("deadbeef"),
            "no key hash may reach the UI, on the account or on any of its keys: {json}"
        );
        assert!(json.contains("laptop"), "keys are listed by name: {json}");
        assert!(json.contains("ada@example.org"));
        assert!(json.contains("\"has_key\":true"));
    }
}

#[cfg(test)]
mod ui_tests {
    /// The UI's HTML must be well-formed enough for Vue to compile it.
    ///
    /// Vue compiles `index.html`'s body as a template at runtime, so a
    /// malformed comment or an unbalanced tag is not a cosmetic problem: the
    /// compile throws and NOTHING renders. A blank page with one console line
    /// is the worst failure mode in this UI, and it has happened twice — once
    /// from a `->` typo closing a comment, which swallowed the rest of the
    /// template.
    ///
    /// This checks the two structural properties that caused it. It is not an
    /// HTML parser and does not try to be; `@vue/compiler-dom` is the real
    /// authority, but requiring node in the test run to reach it would cost
    /// more than it is worth for the failure it catches.
    #[test]
    fn the_ui_html_is_structurally_sound() {
        let html = include_str!("../assets/index.html");

        // Every comment must close. An unclosed one eats everything after it.
        let opens = html.matches("<!--").count();
        let closes = html.matches("-->").count();
        assert_eq!(
            opens, closes,
            "unbalanced HTML comments in index.html ({opens} <!-- vs {closes} -->) — \
             an unclosed comment swallows the rest of the template and Vue renders nothing"
        );

        // And the elements Vue cares about must balance.
        let mut depth: std::collections::BTreeMap<&str, i32> = std::collections::BTreeMap::new();
        for tag in [
            "div", "table", "tbody", "thead", "tr", "td", "th", "form", "template", "p", "span",
        ] {
            let open = html.matches(&format!("<{tag} ")).count() + html.matches(&format!("<{tag}>")).count();
            let close = html.matches(&format!("</{tag}>")).count();
            if open != close {
                depth.insert(
                    tag,
                    i32::try_from(open).unwrap_or(0) - i32::try_from(close).unwrap_or(0),
                );
            }
        }
        assert!(depth.is_empty(), "unbalanced tags in index.html: {depth:?}");

        // Self-closing syntax on a component is the subtle one, and it is
        // invisible to a string-based template compiler.
        //
        // `<my-chart />` is honoured when Vue compiles a template STRING. This
        // file is an in-DOM template: the browser's HTML parser gets it first,
        // and HTML has no self-closing syntax for non-void elements. The slash
        // is ignored, the tag stays open, and every following sibling becomes
        // a CHILD of the component — which shows up as a baffling
        // "v-else has no adjacent v-if" from somewhere far below.
        for (i, line) in html.lines().enumerate() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix('<') {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                    .collect();
                // Hyphen means a custom element: a Vue component, not HTML.
                if name.contains('-') {
                    assert!(
                        !line.trim_end().ends_with("/>"),
                        "index.html:{}: <{name} … /> is self-closed. HTML ignores that, so the tag \
                         stays open and the rest of the template becomes its children:\n  {line}",
                        i + 1
                    );
                }
            }
        }
    }
}
