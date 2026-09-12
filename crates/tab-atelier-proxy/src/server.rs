// SPDX-License-Identifier: MPL-2.0

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
use crate::{account, classifier, egress, inspect, provider, qos, routing, usage};

type Body = http_body_util::combinators::BoxBody<Bytes, Infallible>;

/// Everything a request handler needs.
pub struct State {
    pub store: Mutex<Store>,
    /// Separate lock from the accounts: recording usage happens on every
    /// proxied request, and it must not queue behind an admin listing users.
    pub usage: Mutex<usage::Store>,
    /// Who goes next when the shared quota is tight.
    pub sched: Mutex<qos::Sched>,
    /// How much of the shared plan has been USED, polled from upstream.
    /// Consumed, not remaining: 1.0 means exhausted.
    pub account: Mutex<account::Monitor>,
    /// Everywhere a request can go.
    ///
    /// Behind a lock because it is editable at runtime: an operator adds a
    /// provider from the UI and the next request uses it, with no restart.
    /// Held only for the read, never across a request to an upstream.
    pub registry: Mutex<provider::Registry>,
    /// Where the registry is written back to when it changes.
    pub registry_path: std::path::PathBuf,
    /// Providers upstream has told us to leave alone, and until when (unix
    /// seconds). Keyed by provider id, because a 429 from one says nothing
    /// about another — that is the entire point of having more than one.
    pub provider_backoff: Mutex<std::collections::BTreeMap<String, u64>>,
    /// Woken when capacity frees up, so a queued call retries promptly instead
    /// of sitting out its full backoff.
    pub wake: tokio::sync::Notify,
    /// Captured requests, when inspection is armed. Off by default and
    /// self-disarming — see [`crate::inspect`].
    pub inspect: Mutex<inspect::Store>,
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
        // The pin, so a row can say where this person's work goes.
        "provider": a.provider,
        // This person's compaction level. Per ACCOUNT, not per provider — see
        // `set_user_compact` for why the two axes meet there.
        "compact": a.compact.as_str(),
        // The whole policy, not a summary of it: the UI edits it field by
        // field, so it needs the parts it is not currently changing.
        "tools": a.tools,
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
    // Repair the shared Claude login from a machine that still has a working
    // one. Body-carrying, so it is handled async like the proxy path.
    if path == "/me/credentials" {
        return Ok(me_credentials(req, &state, peer).await);
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
/// So the header is honoured only from a hop we have reason to trust — see
/// [`is_trusted_hop`]. Trusting it from anywhere else would let a caller write
/// its own address into the log by setting a header, which is worse than
/// recording nothing.
///
/// Both spellings are read. Caddy and nginx set `X-Real-IP` to a single
/// address; `X-Forwarded-For` is a comma-separated chain whose FIRST entry is
/// the original client. `X-Real-IP` is preferred because it is unambiguous —
/// an XFF chain can be extended by the client, and only the trusted hop's own
/// append is reliable.
fn client_ip(headers: &hyper::HeaderMap, peer: std::net::IpAddr) -> String {
    if !is_trusted_hop(peer) {
        return peer.to_string();
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    header("x-real-ip")
        .or_else(|| header("x-forwarded-for"))
        .unwrap_or_else(|| peer.to_string())
}

/// Whether a forwarding header from this peer should be believed.
///
/// Loopback is the obvious case. Private and unique-local addresses are here
/// because the terminator usually is not on loopback: put Caddy in a container
/// and it reaches the proxy from a bridge network, so every request arrived
/// from something like `172.31.112.6` and the header was discarded — which is
/// how a whole fleet came to be logged under one address, making the per-key
/// "last used from" column useless for the exact question it exists to answer.
///
/// The cost is stated plainly: anything already inside the private network can
/// claim any address. That is a trade this proxy is entitled to make — it is
/// documented as belonging behind a terminator, and the address is an audit
/// aid rather than an authorisation input. Nothing is granted by it.
/// One header, as received, for the arrival record.
///
/// Separate from [`client_ip`]'s inline reads because this reports rather than
/// decides: it keeps the raw value even when the peer is untrusted and the
/// header will be ignored, which is precisely the case worth seeing.
fn header_of(headers: &hyper::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

const fn is_trusted_hop(peer: std::net::IpAddr) -> bool {
    match peer {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return mapped.is_loopback() || mapped.is_private() || mapped.is_link_local();
            }
            let seg = v6.segments()[0];
            // fc00::/7 unique-local, fe80::/10 link-local.
            v6.is_loopback() || (seg & 0xfe00) == 0xfc00 || (seg & 0xffc0) == 0xfe80
        }
    }
}

/// Claude Code probes reachability with an unauthenticated HEAD/GET before it
/// has a credential to present. Answering 401 here reads as "this endpoint is
/// broken" and the client never attempts a real call, so this is answered
/// first — and kept to an exact path, because an unauthenticated branch is
/// security surface.
///
/// Split out of `anthropic` because it is the whole decision and reads better
/// as one unit.
fn reachability_probe(sub: &str, method: &Method) -> Option<Response<Body>> {
    if sub != "/api/hello" || !matches!(*method, Method::HEAD | Method::GET) {
        return None;
    }
    let body = if *method == Method::HEAD { "" } else { "{}" };
    Some(
        Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)).boxed())
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed())),
    )
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

    if let Some(probe) = reachability_probe(&sub, &method) {
        return probe;
    }

    let key = presented(&req);
    let ip = client_ip(req.headers(), peer);
    // Kept alongside the resolved answer, because "what did we record" and
    // "why" are different questions and only the first one was answerable.
    // Built here, where the raw headers and the socket peer are both in hand.
    let origin = inspect::Origin {
        peer: peer.to_string(),
        peer_trusted: is_trusted_hop(peer),
        client_ip: ip.clone(),
        x_real_ip: header_of(req.headers(), "x-real-ip"),
        x_forwarded_for: header_of(req.headers(), "x-forwarded-for"),
    };
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
    let client_headers = passthrough_headers(req.headers());
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
    let (body, route, compaction) = match shape_and_admit(&state, &account, body, metered).await {
        Ok(triple) => triple,
        Err(resp) => return resp,
    };

    let fwd = Forward {
        // Absent from the registry means `destination` falls back to the
        // egress's own Claude login, so an unknown id DOES spend the plan —
        // the same `is_none_or` the credential choice makes.
        uses_the_subscription: state
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&route.provider_id)
            .is_none_or(provider::Provider::uses_the_subscription),
        sub_pq,
        is_post,
        content_type,
        client_beta,
        client_headers,
        body,
        account_id: account.id.clone(),
        account_email: account.email.clone(),
        // Carried to the capture, which is the only place the saving is
        // visible: the panel shows the body as sent, so a compacted request
        // and a small one look identical without it.
        compaction,
        origin,
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
/// Returns the (possibly rewritten) body, what was swapped, and what
/// compaction removed, or the 429 to send back. Split out of `anthropic`
/// because it is the whole `QoS` decision and reads better as one unit than
/// inline in the middle of the forwarder.
async fn shape_and_admit(
    state: &Arc<State>,
    account: &Account,
    body: Bytes,
    metered: bool,
) -> Result<(Bytes, routing::Route, Option<inspect::Compaction>), Response<Body>> {
    if !metered {
        // Not a generation: it still has to go somewhere, but no class
        // reasoning applies. The pin still does — an account routed to a
        // provider does not get its metadata calls answered by a different
        // one, which would be a leak of exactly the thing the pin exists to
        // contain.
        let provider_id = account.provider.clone().unwrap_or_else(|| "anthropic".to_owned());
        return Ok((
            body,
            routing::Route {
                provider_id,
                model_id: String::new(),
                class: provider::Class::Balanced,
                kind: classifier::Kind::Work,
                changed_from: None,
                reason: None,
            },
            // Not a generation, so there was nothing to compact either.
            None,
        ));
    }
    // Choose the destination BEFORE admission, so the call is judged at the
    // price it will actually pay rather than the one it asked for.
    //
    // One parse answers both questions routing asks: which model the caller
    // named, and whether this is the conversation or the auto-mode classifier.
    // The classifier is routed differently — a mapping may not retarget it and
    // it is never compacted — so it has to be known before `choose`.
    let parsed = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let requested = parsed
        .as_ref()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_default();
    let kind = if parsed.as_ref().is_some_and(classifier::is_classifier) {
        classifier::Kind::Classifier
    } else {
        classifier::Kind::Work
    };
    let health = provider_health(state);
    // Scoped: the registry lock is a std Mutex, and holding one across an
    // await makes this future non-Send — which the compiler reports as a
    // spawn failure a long way from here. Taken only for the decision.
    let route = {
        let registry = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        routing::choose(
            &registry,
            &requested,
            account.provider.as_deref(),
            kind,
            &health,
            |v| std::env::var(v).ok(),
            usage::now_secs(),
        )
    };
    let Some(route) = route else {
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
    // Once per gated action, so it stays at debug: visible when an operator is
    // asking where the classifier went, silent otherwise. The mapping it did
    // NOT take is recorded too — that is the surprising half.
    if kind == classifier::Kind::Classifier {
        log::debug!(
            "proxy: {} auto-mode classifier {requested} → {}/{} (mappings not applied)",
            account.display_name(),
            route.provider_id,
            route.model_id
        );
    }
    let (body, compaction) = shape_body(
        &body,
        &route,
        &requested,
        account.compact,
        &account.tools,
        // True unconditionally — a judgement, not a reading of the provider.
        // Every hop here speaks the Messages wire (`Wire::Anthropic` is the
        // only variant in use), and whatever marks the client already puts
        // in `tools[]` it puts there today without us, so a far end that
        // rejected them would be failing requests the client was already
        // making on its own. Stripping marks we did not add would buy
        // nothing and cost the cache. The flag is here for a provider that
        // turns out to need it.
        true,
    );

    // Admission gates on the SUBSCRIPTION's budget, so it applies only to a
    // destination that spends it.
    //
    // It used to gate everything. The scheduler's whole model of "how much is
    // left" is `anthropic-ratelimit-*` headers and the plan monitor — one
    // quota, Anthropic's — so a request bound for a provider that bills
    // separately was being refused because an unrelated account was maxed out.
    // The symptom is a 429 naming a saturation the caller is not causing, and
    // the workaround is to disable the subscription entirely, which is exactly
    // the wrong answer: the far end that could have served it sits idle while
    // the plan it does not use is blamed.
    let on_subscription = state
        .registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&route.provider_id)
        .is_none_or(provider::Provider::uses_the_subscription);
    let est = qos::estimate_cost(&body);
    if on_subscription && let Err(retry_after) = admit(state, &account.id, account.weight, est).await {
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
    Ok((body, route, compaction))
}

/// Rewrite the outgoing request for its destination: the model, and the
/// account's compaction level.
///
/// Returns what compaction did alongside the body, so an inspection capture
/// can report it: the panel shows the body AS SENT, so without this a
/// compacted request is indistinguishable from a small one.
///
/// One parse for both mutations. The proxy has already paid to deserialize
/// this body to find `model`, and `estimate_cost` is about to deserialize it
/// again — a third pass for a second mutation would be pure waste on every
/// request.
///
/// Compaction runs BEFORE `estimate_cost`, which is the whole point of doing it
/// here: admission then scores the call at the size that will actually be sent
/// rather than at the size it arrived. Returns the body untouched when there is
/// nothing to do, which is the common case — every account defaults to `none`.
fn shape_body(
    body: &Bytes,
    route: &routing::Route,
    requested: &str,
    compact: crate::compact::Compact,
    policy: &crate::tools::Policy,
    takes_cache: bool,
) -> (Bytes, Option<inspect::Compaction>) {
    let rename = (route.model_id != requested).then_some(route.model_id.as_str());
    // The level is the account's, resolved by the caller from wherever the
    // store lives — not the provider's. It reads like a property of the hop,
    // because the harm it can do is one (see `Provider::compact_refusal`), but
    // the operator is reasoning about a PERSON, and routing picks the hop per
    // request: a level filed under a provider silently means something else the
    // moment that provider stops being where the traffic goes.
    //
    // The classifier is exempt by construction. Its transcript is TEXT inside a
    // single user turn rather than `tool_result` blocks, so today's pass would
    // find nothing — but that is a property of the current elision target, and
    // a pass must never be the thing that decides which part of a safety
    // judgement the judge gets to read. See `classifier::Kind::compacts`.
    let level = if route.kind.compacts() {
        compact
    } else {
        crate::compact::Compact::None
    };
    // The tool policy is exempt from the classifier for a sharper reason than
    // the level is: this pass ADDS tools, and the classifier is a judge written
    // to emit one tag. Handing it a toolkit changes what it is, not merely what
    // it reads. See `classifier::Kind::shapes_tools`.
    let policy = route.kind.shapes_tools().then_some(policy);
    // The tool policy joins the early-out rather than being checked after
    // it. An account with no policy must not pay for the parse and the
    // re-encode, and that is most accounts on most requests.
    if rename.is_none() && level.is_none() && policy.is_none_or(crate::tools::is_noop) {
        return (body.clone(), None);
    }

    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        // Not JSON. The old `rewrite_model` swallowed this and forwarded the
        // original, which is right — the far end gives a better error than we
        // could — and compaction has nothing to act on either way.
        return (body.clone(), None);
    };
    if let Some(model) = rename {
        v["model"] = serde_json::Value::String(model.to_owned());
    }
    let before = body.len();
    let elided = crate::compact::apply(&mut v, level);
    // After compaction, deliberately. Compaction walks `messages[]` and the
    // tool policy's pin rule reads that same array to decide what is still
    // live; running the policy first would pin tools against history that
    // compaction was about to elide.
    let governed = policy.map_or_else(crate::tools::Report::default, |policy| {
        crate::tools::apply(&mut v, policy, takes_cache)
    });
    let Ok(encoded) = serde_json::to_vec(&v) else {
        return (body.clone(), None);
    };
    if governed.changed() {
        log::info!(
            "proxy: tools {} offered → {} sent on {}: {} removed, {} added, {} pinned{}",
            governed.offered,
            governed.sent,
            route.provider_id,
            governed.removed.len(),
            governed.added,
            governed.pinned.len(),
            if governed.refused.is_empty() {
                String::new()
            } else {
                format!(
                    ", {} refused ({})",
                    governed.refused.len(),
                    governed
                        .refused
                        .iter()
                        .map(crate::tools::Refusal::describe)
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            }
        );
    }
    if elided.changed() {
        log::info!(
            "proxy: compacted {}/{} {before} → {} bytes: {} tool results elided ({} errors kept), \
             {} thinking dropped, {} write payloads stubbed, {} banners dropped",
            route.provider_id,
            route.model_id,
            encoded.len(),
            elided.tool_results_elided,
            elided.tool_results_kept_for_error,
            elided.thinking_dropped,
            elided.writes_elided,
            elided.banners_dropped
        );
    }
    // Recorded whenever a level was in force, even if it changed nothing:
    // "compaction is on and elided nothing" and "compaction is off" are
    // different answers, and the panel should be able to tell them apart.
    //
    // Tied to `level`, not to "the body was parsed". A request the tool policy
    // alone brought through this function has no level in force, and attaching
    // a record anyway would put a row in the panel reading `none` next to a
    // savings of zero — indistinguishable from a real pass that found nothing,
    // which is the one distinction this field exists to make.
    let record = if level.is_none() {
        None
    } else {
        Some(inspect::Compaction {
            level: level.as_str().to_owned(),
            bytes_before: u64::try_from(before).unwrap_or(u64::MAX),
            bytes_after: u64::try_from(encoded.len()).unwrap_or(u64::MAX),
            tool_results_elided: elided.tool_results_elided,
            tool_results_kept_for_error: elided.tool_results_kept_for_error,
            thinking_dropped: elided.thinking_dropped,
            writes_elided: elided.writes_elided,
            banners_dropped: elided.banners_dropped,
        })
    };
    (Bytes::from(encoded), record)
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
    let now = usage::now_secs();
    // One resolution for the whole response. Calling `span` again inside
    // `usage_json` would re-read the clock, and a response that straddled an
    // hour boundary would answer its totals and its series for two different
    // windows.
    let window = path_window(query).unwrap_or(usage::Window::Hours(24 * 7));
    let span = window.span(now);
    let body = {
        let u = state.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let per_user: Vec<_> = store
            .accounts()
            .iter()
            .map(|a| {
                let mut v = usage_json(&u, &a.id, span, now);
                if let Some(o) = v.as_object_mut() {
                    o.insert("user".to_owned(), account_json(a));
                }
                v
            })
            .collect();
        serde_json::json!({
            "window": window.token(),
            "hours": span.hours(),
            "window_start": crate::now_rfc3339_at(span.first),
            "window_end": crate::now_rfc3339_at(span.last),
            "users": per_user,
        })
        .to_string()
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
            // What upstream claims about the weekly window, and what we
            // actually watched it do. They disagree, routinely — see
            // `Sample::seven_day_resets` — so the UI shows both rather than
            // choosing one to present as fact.
            "weekly_last_drop": acct.last_weekly_drop().map(|(ts, from, to)| serde_json::json!({
                "ts": ts, "from": from, "to": to,
            })),
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
                "providers": state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner).summary(),
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
    /// The client's own Claude Code identity headers, forwarded verbatim.
    /// See [`passthrough_headers`].
    client_headers: Vec<(String, String)>,
    body: Bytes,
    weight: u32,
    /// Whether this call was admitted by the scheduler, and so has an estimate
    /// outstanding that must be settled.
    metered: bool,
    /// Where this is going, chosen by [`crate::routing`].
    route: routing::Route,
    /// Whether this request spends the shared Claude subscription.
    ///
    /// Resolved once, at the same moment the destination is, so the scheduler
    /// and the response feedback agree about which quota is in play. A request
    /// bound for a provider that bills separately must be neither gated by the
    /// subscription's budget nor able to spend it.
    uses_the_subscription: bool,
    /// What compaction removed on the way out, if it ran. Attached to the
    /// inspection capture — see [`inspect::Compaction`].
    compaction: Option<inspect::Compaction>,
    /// How the request arrived. See [`inspect::Origin`].
    origin: inspect::Origin,
    /// Who to bill. Carried down rather than looked up again, because by the
    /// time the response finishes the account may have been deleted — the call
    /// still happened and still spent tokens.
    account_id: String,
    /// For labelling a capture. Carried rather than looked up, for the same
    /// reason as `account_id`: by the time the response finishes the account
    /// may be gone, and the call still happened.
    account_email: String,
    state: Arc<State>,
}

/// Client headers the proxy forwards to Anthropic unchanged.
///
/// The client on the far side IS Claude Code, and Anthropic's OAuth path is
/// for Claude Code. Rebuilding the request from scratch — which is what this
/// used to do — replaced that fingerprint with `tab-atelier-proxy/0.5.0` and
/// dropped the session id, so every proxied call looked like an unknown client
/// and no support question about a session could be traced through.
///
/// An **allowlist**, not a denylist: a denylist forgets, and the things it
/// forgets here are `cookie`, `cf-access-*` and the caller's own
/// `authorization` — credentials for a different hop that must not be handed
/// to Anthropic. Anything not named is dropped.
///
/// Deliberately absent:
/// * `authorization` / `x-api-key` — the proxy substitutes its own credential;
///   the user key authenticates to the PROXY and is not an Anthropic one.
/// * `anthropic-beta` — merged separately, see [`claude_api::merge_beta`].
/// * `host`, `content-length`, `connection`, `accept-encoding` — hop-by-hop or
///   recomputed by the outgoing client.
const FORWARDED_HEADERS: &[&str] = &[
    "user-agent",
    "x-app",
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
    "x-client-app",
    "anthropic-dangerous-direct-browser-access",
    "anthropic-version",
    "accept",
];

/// Prefix allowlist, for the SDK's telemetry headers (`x-stainless-lang`,
/// `-os`, `-runtime`, `-retry-count`, …). They are enumerated by the SDK
/// version, not by us, so matching the prefix is what keeps this from going
/// stale on the client's next upgrade.
const FORWARDED_PREFIXES: &[&str] = &["x-stainless-"];

/// Collect the headers of [`FORWARDED_HEADERS`] / [`FORWARDED_PREFIXES`].
fn passthrough_headers(headers: &hyper::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let n = name.as_str();
            let keep = FORWARDED_HEADERS.contains(&n) || FORWARDED_PREFIXES.iter().any(|p| n.starts_with(p));
            // Non-UTF-8 header values cannot be re-sent and are not something
            // Anthropic emits; dropping one is better than failing the call.
            keep.then(|| value.to_str().ok().map(|v| (n.to_owned(), v.to_owned())))?
        })
        .collect()
}

/// Snapshot the outgoing request, when inspection is armed.
///
/// Separate from [`forward`] so the lock is taken and released in one small
/// scope rather than living across the send — a blocking upstream call holding
/// the inspection mutex would stall the admin page behind an LLM stream.
fn begin_capture(f: &Forward, hdrs: &[(String, String)]) -> Option<inspect::Capture> {
    // Read the flag and release the lock before building anything: the guard
    // must not be alive across the scrub-and-clamp below, let alone the send.
    let armed = {
        let ins = f
            .state
            .inspect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ins.armed(usage::now_secs())
    };
    if !armed {
        return None;
    }
    Some(inspect::capture(&inspect::Outgoing {
        ts: crate::now_rfc3339(),
        account_id: &f.account_id,
        account_email: &f.account_email,
        method: if f.is_post { "POST" } else { "GET" },
        path: &f.sub_pq,
        provider: &f.route.provider_id,
        kind: f.route.kind,
        headers: hdrs,
        body: &f.body,
        origin: Some(f.origin.clone()),
    }))
}

/// Stamp what upstream answered onto a pending capture and store it.
///
/// Called at the end of the stream, because the token counts only exist once
/// the response has been read to the end — they come from the same parse the
/// billing path uses, so the panel and the usage graph cannot disagree.
fn finish_capture(
    f: &Forward,
    pending: Option<inspect::Capture>,
    status: u16,
    tokens: &usage::Tokens,
    model: Option<&str>,
) {
    let Some(mut c) = pending else { return };
    c.status = Some(status);
    c.tokens = Some(*tokens);
    // What compaction removed from THIS request, so the panel can show the
    // saving rather than only the compacted result.
    c.compaction.clone_from(&f.compaction);
    // Whether this capture's own client_ip can be trusted at all, recorded
    // beside it rather than left to be inferred from the log.
    c.origin = Some(f.origin.clone());
    // What upstream REPORTED the model as, when it was the routing that chose
    // it. Usually the same as what we sent; different means the far end
    // substituted, which is worth seeing.
    if let Some(m) = model
        && c.model.as_deref() != Some(m)
    {
        c.model = Some(m.to_owned());
    }
    let mut ins = f
        .state
        .inspect
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ins.push(c);
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
    let mut hdrs: Vec<(String, String)> = vec![
        ("Content-Type".to_owned(), f.content_type.clone()),
        (auth.0.to_owned(), auth.1),
        // The client's own beta flags are merged in, not replaced: a body field
        // gated behind a flag the client opted into is rejected upstream as an
        // unknown input if only our flags survive.
        (
            "anthropic-beta".to_owned(),
            egress::merge_beta(f.client_beta.as_deref(), egress::ANTHROPIC_BETA),
        ),
    ];
    // The client's Claude Code identity travels with the request — it is the
    // fingerprint Anthropic's OAuth path expects, and we are not it.
    hdrs.extend(f.client_headers.iter().cloned());
    // A client that sent none of them (a curl smoke test, another SDK) still
    // has to look like Claude Code upstream, so fill in what is missing rather
    // than either overriding the real client or sending nothing.
    for (k, v) in claude_api::api_headers(None) {
        if !hdrs.iter().any(|(n, _)| n.eq_ignore_ascii_case(k)) {
            hdrs.push((k.to_owned(), v));
        }
    }
    // The request as it will actually leave: after routing rewrote the model,
    // after the beta flags were merged, with the proxy's credential in place.
    // That is the thing nobody can otherwise see, and it is the whole reason
    // inspection exists. Scrubbing happens inside `inspect::capture`.
    let mut pending = begin_capture(f, &hdrs);

    let sent = if f.is_post {
        let mut rb = agent.post(&url);
        for (k, v) in &hdrs {
            rb = rb.header(k.as_str(), v);
        }
        rb.send(&f.body[..])
    } else {
        let mut rb = agent.get(&url);
        for (k, v) in &hdrs {
            rb = rb.header(k.as_str(), v);
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
        // The capture is still filed: the request WAS made and the client
        // hanging up does not unmake it. No token counts, because the
        // response was never read.
        finish_capture(f, pending.take(), status, &usage::Tokens::default(), None);
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
    finish_capture(f, pending.take(), status, &tokens, model.as_deref());
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
    // Cloned out rather than held: the guard must not live across the network
    // work below, and a Provider is a handful of strings.
    let chosen = {
        let registry = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.get(provider_id).cloned()
    };
    let chosen = chosen.as_ref();
    // Belt to the candidate filter's braces. `choose` never returns an
    // unusable provider, but the metadata path names one directly from an
    // account's pin — so the refusal lives here too, where the credential is
    // actually attached to a URL.
    if let Some(p) = chosen
        && let Some(why) = p.unusable_reason()
    {
        return Err(why);
    }
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
        // convention its own SDKs use. Env var or file is the provider's
        // choice; the route only cares that one resolved.
        Some(auth) => {
            let key = auth.secret_with(|v| std::env::var(v).ok())?;
            ("x-api-key", key)
        }
    };
    Ok((base, auth))
}

/// Fold an upstream answer back into what the proxy knows.
///
/// A 429 stops traffic to THAT provider, not to every provider: its limit is
/// its own, and the next request routes elsewhere instead of waiting. Any
/// other answer clears the block — a provider that responds is working again.
fn observe_upstream(f: &Forward, status: u16, remaining: Option<u64>, reset_in: Option<u64>, retry_after: Option<u64>) {
    // The scheduler measures ONE quota — the subscription's. Feeding it a
    // second provider's answers merges two limits that have nothing to do with
    // each other: a 429 from anywhere set a GLOBAL backoff, so a provider
    // refusing traffic stopped the subscription from being used at all, and
    // vice versa. That is not a subtle degradation; it takes a working far end
    // out of service because an unrelated one is busy.
    //
    // `provider_backoff`, below, is the per-provider mechanism and always runs.
    if f.uses_the_subscription {
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

/// `?window=` off a query string, if present and sane.
///
/// An unrecognised token falls back to the default rather than 400ing: this is
/// a GET the dashboard re-issues on a timer, and a stale bookmark naming a
/// window that was removed should show a graph, not an error page.
fn path_window(query: &str) -> Option<usage::Window> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("window="))
        .and_then(usage::Window::parse)
}

/// Render one account's usage as JSON.
///
/// `now` is passed rather than read here, so the requested window and the two
/// fixed ones are resolved from one reading of the clock.
fn usage_json(u: &usage::Store, id: &str, span: usage::Span, now: u64) -> serde_json::Value {
    let tok = |t: &usage::Tokens| {
        serde_json::json!({
            "input": t.input,
            "output": t.output,
            "cache_read": t.cache_read,
            "cache_write": t.cache_write,
            "total": t.total(),
        })
    };
    let window = |w: usage::Window| {
        let (calls, errors, t) = u.totals(id, w.span(now));
        serde_json::json!({ "calls": calls, "errors": errors, "tokens": tok(&t) })
    };
    let series: Vec<_> = u
        .series(id, span)
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
        // The two fixed windows stay: they are what the account table and the
        // "average tokens per call" column read, and they must mean the same
        // thing whatever the graph is currently zoomed to. `span` is the
        // caller's requested window, carried separately.
        "window": {
            "start": crate::now_rfc3339_at(span.first),
            "end": crate::now_rfc3339_at(span.last),
            "hours": span.hours(),
        },
        "all_time": window(usage::Window::All),
        "last_24h": window(usage::Window::Hours(24)),
        "last_7d": window(usage::Window::Hours(24 * 7)),
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
    let account = authenticate_and_stamp(state, &key, &client_ip(req.headers(), peer));
    let Some(account) = account else {
        return json(
            401,
            r#"{"error":"present your proxy key (x-api-key or Authorization: Bearer)"}"#,
        );
    };
    let now = usage::now_secs();
    let window = path_window(req.uri().query().unwrap_or_default()).unwrap_or(usage::Window::Hours(24 * 7));
    let mut body = {
        let u = state.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        usage_json(&u, &account.id, window.span(now), now)
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

/// `POST /me/credentials` — repair the proxy's Claude login over the network.
///
/// The proxy authenticates to Anthropic with one shared Claude OAuth
/// credential, and that credential dies whenever somebody logs in again
/// elsewhere: a refresh rotates the refresh token and revokes every other
/// copy. Until now the only fix was shell access to the host, which is a poor
/// answer when the person who can fix it is the one holding a laptop with a
/// working login.
///
/// Two guards, in [`egress::repair_credentials`], make this safe to expose to
/// anyone with a user key: it only acts when the installed credential is
/// already dead (verified against Anthropic, not assumed), and only accepts a
/// replacement belonging to the same Anthropic account. So the worst outcome
/// is that a broken proxy gets restored to the account it already had.
///
/// The credential itself is never logged. What IS logged is who did it, which
/// is the part worth being able to review afterwards.
async fn me_credentials(req: Request<Incoming>, state: &Arc<State>, peer: std::net::IpAddr) -> Response<Body> {
    if req.method() != Method::POST {
        return json(405, r#"{"error":"POST only"}"#);
    }
    let key = presented(&req);
    let ip = client_ip(req.headers(), peer);
    let Some(account) = authenticate_and_stamp(state, &key, &ip) else {
        return json(
            401,
            r#"{"error":"present your proxy key (x-api-key or Authorization: Bearer)"}"#,
        );
    };
    let body = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return json(400, r#"{"error":"unreadable body"}"#),
    };
    let Ok(raw) = String::from_utf8(body.to_vec()) else {
        return json(400, r#"{"error":"body is not UTF-8"}"#);
    };

    // Blocking: it makes up to two calls to Anthropic.
    let outcome = tokio::task::spawn_blocking(move || egress::repair_credentials(&raw)).await;
    match outcome {
        Ok(Ok(imported)) => {
            log::warn!(
                "claude credential replaced via /me/credentials by {} <{}> from {ip}",
                account.display_name(),
                account.email,
            );
            let who = imported.identity.map(|i| i.email).unwrap_or_default();
            json(
                200,
                &serde_json::json!({ "installed": true, "account": who }).to_string(),
            )
        }
        Ok(Err(e)) => {
            log::warn!(
                "credential repair refused for {} <{}> from {ip}: {e}",
                account.display_name(),
                account.email,
            );
            // 409: the request was well-formed and authenticated, the state
            // just does not permit it — a working proxy, or somebody else's
            // account. Distinct from 400 so a client can tell "nothing to fix"
            // from "you sent rubbish".
            json(409, &serde_json::json!({ "error": e }).to_string())
        }
        Err(_) => json(500, r#"{"error":"credential repair panicked"}"#),
    }
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
        // Provider configuration. Read-only here; mutations go through
        // `mutate`, which holds the registry lock for the whole edit.
        (&Method::GET, "/api/providers") => return providers_json(&state),
        (&Method::GET, "/api/usage") => {
            let store = state.store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            return usage_report(&state, &store, &query);
        }
        // Inspection. Admin-only, like everything under /api — a capture is a
        // prompt, so a user key must never be able to read one, not even its
        // own account's.
        (&Method::GET, "/api/inspect") => {
            let ins = state.inspect.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = usage::now_secs();
            return json(
                200,
                &serde_json::json!({
                    "armed": ins.armed(now),
                    "armed_until": ins.armed_until(),
                    "seconds_left": ins.armed_until().saturating_sub(now),
                    "max_arm_minutes": inspect::MAX_ARM_MINUTES,
                    "captures": ins.recent(),
                })
                .to_string(),
            );
        }
        (&Method::POST, "/api/inspect") => {
            let minutes = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("minutes").and_then(serde_json::Value::as_u64))
                .unwrap_or(15);
            // Scoped so the guard is gone before the log call: a mutex held
            // across formatting is a mutex held for no reason.
            let until = {
                let mut ins = state.inspect.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                ins.arm(usage::now_secs(), minutes)
            };
            log::warn!(
                "proxy: request inspection ARMED until {} — captures contain prompts",
                crate::now_rfc3339_at(until)
            );
            return json(200, &serde_json::json!({ "armed_until": until }).to_string());
        }
        // Disarm and forget. One button, because "stop recording" and "and
        // delete what you recorded" are the same intention in practice.
        (&Method::DELETE, "/api/inspect") => {
            let cleared = {
                let mut ins = state.inspect.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                ins.disarm();
                ins.clear()
            };
            return match cleared {
                Ok(()) => {
                    log::info!("proxy: request inspection disarmed and captures cleared");
                    json(200, r#"{"ok":true}"#)
                }
                Err(e) => json(500, &serde_json::json!({ "error": e }).to_string()),
            };
        }
        _ => {}
    }
    mutate(&state, &method, &path, &body)
}

/// Everything the providers panel needs, and no credential anywhere in it.
fn providers_json(state: &State) -> Response<Body> {
    // Everything is lifted out and the registry lock released before the
    // response is built: it is a std Mutex on a request path, and holding it
    // across a JSON allocation buys nothing.
    let (presets, list, mappings) = {
        let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = usage::now_secs();
        let presets: Vec<_> = provider::Preset::ALL
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id(),
                    "label": p.label(),
                    // Shown before anyone commits to adding it, so the endpoint
                    // and the models are visible up front.
                    "base_url": p.provider(std::path::Path::new("/")).base_url,
                    "configured": reg.get(p.id()).is_some(),
                })
            })
            .collect();
        let list: Vec<_> = reg
            .providers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "base_url": p.base_url,
                    "preference": p.preference,
                    "enabled": p.enabled,
                    "peak_now": p.peak_now(now),
                    "peak": p.peak,
                    // Whether the UI may offer anything but `none` for traffic
                    // through here, and why not. Level-independent: the harm
                    // is a property of the HOP, so any non-none level meets it
                    // equally, and the account's level is what decides whether
                    // the warning is shown. Sent so the reason is the server's,
                    // not a second copy of the rule in TypeScript.
                    "compact_refusal": p.compact_refusal(crate::compact::Compact::Tools),
                    // Whether the credential resolves — never the credential.
                    "ready": p.credential_ready(),
                    "auth": match &p.auth {
                        provider::Auth::ClaudeOauth => "claude_oauth",
                        provider::Auth::ApiKeyEnv { .. } => "api_key_env",
                        provider::Auth::ApiKeyFile { .. } => "api_key_file",
                    },
                    "models": p.models.iter().map(|m| serde_json::json!({
                        "id": m.id,
                        "class": m.class,
                        "relative_cost": m.relative_cost,
                        "cost_now": p.cost_at(m, now),
                        "deprecated": m.deprecated,
                        "note": m.note,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        (presets, list, reg.mappings.clone())
    };
    json(
        200,
        &serde_json::json!({
            "providers": list,
            "presets": presets,
            "mappings": mappings,
            // The compaction levels and their labels come from the server so
            // the wording lives in one place — the enum the routing reads is
            // the same one the UI renders.
            "compact_levels": crate::compact::Compact::ALL
                .into_iter()
                .map(|c| serde_json::json!({"value": c.as_str(), "label": c.label()}))
                .collect::<Vec<_>>(),
        })
        .to_string(),
    )
}

/// Rotate one provider's key, leaving everything else alone.
fn rotate_provider_key(state: &Arc<State>, id: &str, key: &str) -> Response<Body> {
    if key.trim().is_empty() {
        return json(400, r#"{"error":"no key given"}"#);
    }
    let auth = {
        let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.get(id).map(|p| p.auth.clone())
    };
    // WHICH kinds of provider can hold a key at all, checked before anything
    // is written.
    //
    // This used to accept a key for any provider and write it to a file. For a
    // `claude_oauth` provider that file is never read — the egress resolves
    // the host's own login instead — so the write silently did nothing and
    // left a live Anthropic credential on disk that no code path consults.
    // Worse than useless: an operator who later changed the auth kind would
    // find a key they had forgotten they pasted, suddenly in use.
    let Some(auth) = auth else {
        return json(404, r#"{"error":"no such provider"}"#);
    };
    // Refused BEFORE anything is written, and refused on the server as well as
    // in the UI: a disabled button is a hint, and the API is reachable without
    // one.
    if let Some(reason) = auth.no_key_reason(id) {
        return json(400, &serde_json::json!({ "error": reason }).to_string());
    }
    let path = provider::provider_key_path(&registry_dir(state), id);
    if let Err(e) = provider::write_provider_key(&path, key) {
        return json(500, &serde_json::json!({ "error": e }).to_string());
    }
    // No restart: the credential is read per request, which is the whole
    // reason it lives in a file rather than in the service's environment.
    log::info!("proxy: provider {id} key rotated");
    json(200, r#"{"ok":true}"#)
}

/// Forget a provider, and unpin anyone routed to it.
///
/// Leaving pins behind would strand those accounts on a provider that no
/// longer exists — a 503 on their next request, caused by an admin action
/// somewhere else entirely.
fn remove_provider(state: &Arc<State>, store: &mut Store, id: &str) -> Response<Body> {
    let pinned_here: Vec<String> = store
        .accounts()
        .iter()
        .filter(|a| a.provider.as_deref() == Some(id))
        .map(Account::display_name)
        .collect();
    for who in &pinned_here {
        if let Err(e) = store.set_provider(who, None) {
            log::error!("proxy: could not unpin {who} from {id}: {e}");
        }
    }
    if !pinned_here.is_empty() {
        log::warn!(
            "proxy: provider {id} removed — unpinned {} (they had been routed there)",
            pinned_here.join(", ")
        );
    }

    let mut reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !reg.remove(id) {
        return json(404, r#"{"error":"no such provider"}"#);
    }
    let saved = reg.save(&state.registry_path);
    drop(reg);
    if let Err(e) = saved {
        return json(500, &serde_json::json!({ "error": e }).to_string());
    }
    json(200, r#"{"ok":true}"#)
}

/// "When someone asks for X, use Y."
fn add_mapping(state: &Arc<State>, from: &str, to: &str, note: &str) -> Response<Body> {
    if from.is_empty() || to.is_empty() {
        return json(400, r#"{"error":"a mapping needs both a from and a to"}"#);
    }
    if from == to {
        // Not an error, a no-op — and storing it would leave a row in the
        // table that looks like a decision.
        return json(400, r#"{"error":"a mapping from a name to itself does nothing"}"#);
    }
    let mut reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    reg.set_mapping(from, to, (!note.is_empty()).then(|| note.to_owned()));
    let saved = reg.save(&state.registry_path);
    drop(reg);
    if let Err(e) = saved {
        return json(500, &serde_json::json!({ "error": e }).to_string());
    }
    json(200, r#"{"ok":true}"#)
}

fn remove_mapping(state: &Arc<State>, from: &str) -> Response<Body> {
    let mut reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !reg.remove_mapping(from) {
        return json(404, r#"{"error":"no such mapping"}"#);
    }
    let saved = reg.save(&state.registry_path);
    drop(reg);
    if let Err(e) = saved {
        return json(500, &serde_json::json!({ "error": e }).to_string());
    }
    json(200, r#"{"ok":true}"#)
}

/// Pin an account to a provider, or clear the pin.
/// Set an account's compaction level.
///
/// The level is the ACCOUNT's ([`Account::compact`]) but the objection is the
/// HOP's ([`provider::Provider::compact_refusal`]): compacting through the
/// subscription invalidates the cache breakpoints that are the only reason it
/// is affordable, so a level there costs money and buys nothing. The two axes
/// meet here because this is the one place that knows both — the person being
/// configured, and every place their traffic could actually go.
fn set_user_compact(state: &Arc<State>, store: &mut Store, who: &str, wanted: &str) -> Response<Body> {
    let Some(level) = crate::compact::Compact::ALL
        .into_iter()
        .find(|c| c.as_str() == wanted.trim())
    else {
        return json(
            400,
            &serde_json::json!({ "error": format!("unknown compaction level {wanted:?}") }).to_string(),
        );
    };
    // Refused on the SERVER, not only by the option the UI disables. A setting
    // that cannot be correct should not be offerable, and the browser is not
    // the only way in — this API is authenticated, not private.
    if !level.is_none()
        && let Some(why) = compact_refusal_for(state, store, who)
    {
        return json(400, &serde_json::json!({ "error": why }).to_string());
    }
    match store.set_compact(who, level) {
        Ok(a) => json(200, &serde_json::json!({ "user": account_json(&a) }).to_string()),
        Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
    }
}

/// Why compaction would only cost money for this account, if it would.
///
/// [`provider::Provider::compact_refusal`] answers for ONE hop. This asks it of
/// the account's destination: its pin when it has one, since that is the only
/// place its traffic may go, and otherwise every enabled provider the router
/// could pick. Any of them is enough to refuse. The router chooses per request,
/// so an account that MIGHT be sent through the subscription is one whose
/// compaction level is not free — and a refusal that only fired when there was
/// no alternative would be a refusal that fired too late to be useful.
fn compact_refusal_for(state: &Arc<State>, store: &Store, who: &str) -> Option<String> {
    let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // The hop's objection is level-independent — it is the breaker on
    // `compact != none` — so any non-`none` level stands in for the rest.
    let ask = |p: &provider::Provider| p.compact_refusal(crate::compact::Compact::Tools);
    store.find(who).and_then(|a| a.provider.as_deref()).map_or_else(
        || reg.providers.iter().filter(|p| p.enabled).find_map(ask),
        |id| reg.get(id).and_then(ask),
    )
}

/// Replace an account's tool policy.
///
/// Whole-object, not field at a time. `mode`, `disable`, `allow` and `add` are
/// one decision — `allow` means nothing without the mode that reads it, and
/// `mode: allow` with no list is `none` wearing a different name — so replacing
/// the lot is the only edit that cannot leave a half-applied policy behind.
/// The UI sends the whole object on every save, so there is nothing to merge.
fn set_user_tools(store: &mut Store, who: &str, incoming: Option<&serde_json::Value>) -> Response<Body> {
    let Some(value) = incoming else {
        return json(400, &serde_json::json!({ "error": "missing `tools`" }).to_string());
    };
    // Deserialized rather than hand-read field by field, so the wire shape and
    // the stored shape cannot drift. An unknown `mode` fails here, which is the
    // point: `allow` typed as `allowed` must not quietly become the default
    // mode, because the default mode offers everything.
    let policy = match serde_json::from_value::<crate::tools::Policy>(value.clone()) {
        Ok(p) => p,
        Err(e) => {
            return json(
                400,
                &serde_json::json!({ "error": format!("bad tool policy: {e}") }).to_string(),
            );
        }
    };
    if let Err(e) = crate::tools::validate(&policy) {
        return json(400, &serde_json::json!({ "error": e }).to_string());
    }
    match store.set_tools(who, policy) {
        Ok(a) => json(200, &serde_json::json!({ "user": account_json(&a) }).to_string()),
        Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
    }
}

fn set_user_provider(state: &Arc<State>, store: &mut Store, who: &str, wanted: &str) -> Response<Body> {
    let wanted = wanted.trim();
    // Validated HERE, where the registry is in hand, rather than in users.rs
    // which knows about people and not about destinations. A pin to a typo
    // would otherwise be a 503 nobody could explain.
    if !wanted.is_empty() {
        let known = {
            let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            reg.get(wanted).is_some()
        };
        if !known {
            return json(
                400,
                &serde_json::json!({ "error": format!("no provider {wanted:?}") }).to_string(),
            );
        }
        // A pin to a provider that can never be used is a 503 on every request
        // from that account, caused by an admin action somewhere else. Refuse
        // it here, where there is a message to give.
        let why = {
            let reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            reg.get(wanted).and_then(provider::Provider::unusable_reason)
        };
        if let Some(why) = why {
            return json(400, &serde_json::json!({ "error": why }).to_string());
        }
    }
    match store.set_provider(who, (!wanted.is_empty()).then_some(wanted)) {
        Ok(a) => json(200, &serde_json::json!({ "user": account_json(&a) }).to_string()),
        Err(e) => json(404, &serde_json::json!({ "error": e.to_string() }).to_string()),
    }
}

/// Add or update a provider from the UI's form.
///
/// Either a preset — one field, everything else supplied from code — or a
/// hand-written one with a base URL, a model list and a key.
fn save_provider(state: &Arc<State>, field: &dyn Fn(&str) -> String, body: &Bytes) -> Response<Body> {
    let dir = registry_dir(state);
    let mut reg = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    let preset = provider::Preset::ALL
        .iter()
        .find(|p| p.id() == field("preset").as_str())
        .copied();
    let mut new = if let Some(p) = preset {
        p.provider(&dir)
    } else {
        {
            let id = field("id");
            if id.is_empty() {
                return json(400, r#"{"error":"a provider needs an id"}"#);
            }
            let base_url = field("base_url");
            if base_url.is_empty() {
                return json(400, r#"{"error":"a provider needs a base_url"}"#);
            }
            let models = match provider::parse_models(&field("models")) {
                Ok(m) => m,
                Err(e) => return json(400, &serde_json::json!({ "error": e }).to_string()),
            };
            provider::Provider {
                id: id.clone(),
                wire: provider::Wire::Anthropic,
                base_url: base_url.trim_end_matches('/').to_owned(),
                auth: provider::Auth::ApiKeyFile {
                    path: provider::provider_key_path(&dir, &id).display().to_string(),
                },
                models,
                preference: serde_json::from_slice::<serde_json::Value>(body)
                    .ok()
                    .and_then(|v| v.get("preference").and_then(serde_json::Value::as_i64))
                    .and_then(|n| i32::try_from(n).ok())
                    .unwrap_or(10),
                enabled: true,
                peak: None,
            }
        }
    };
    // On update, everything the form CANNOT express is preserved.
    //
    // The form shows a base URL, models and a key. It has no field for the
    // auth kind, the preference or the peak schedule — so a save used to
    // overwrite them with whatever the request implied. For `auth` that was
    // not cosmetic: saving the subscription's own row through the form rewrote
    // it from `claude_oauth` to `api_key_file`, which left it with no
    // credential, out of the candidate list, and the whole proxy falling back
    // to nothing. A partial view must save partially.
    //
    // `enabled` is the exception: the table DOES show it, so a request that
    // names it is taken at its word and one that does not is preserved. That
    // second half is what keeps the older form working.
    let enabled = flag_from(body, &field("enabled"));
    if let Some(old) = reg.get(&new.id) {
        new.preference = old.preference;
        new.enabled = enabled.unwrap_or(old.enabled);
        new.auth = old.auth.clone();
        new.peak.clone_from(&old.peak);
    } else if let Some(wanted) = enabled {
        new.enabled = wanted;
    }

    let id = new.id.clone();

    // The key, if one was pasted. Written to its own 0600 file BEFORE the
    // registry mentions it, so a failed write cannot leave a provider pointing
    // at a file that is not there.
    let key = field("key");
    if !key.trim().is_empty()
        && let Err(e) =
            provider::write_provider_key(std::path::Path::new(&provider::provider_key_path(&dir, &id)), &key)
    {
        return json(500, &serde_json::json!({ "error": e }).to_string());
    }

    reg.upsert(new);
    let saved = reg.save(&state.registry_path);
    // Scoped: the lock must not be held across the log line below.
    drop(reg);
    if let Err(e) = saved {
        return json(500, &serde_json::json!({ "error": e }).to_string());
    }
    log::info!("proxy: provider {id} saved");
    json(200, &serde_json::json!({ "id": id }).to_string())
}

/// The account-mutating half of the admin API.
///
/// The directory `providers.json` lives in, which is where key files go too,
/// is [`registry_dir`].
fn registry_dir(state: &State) -> std::path::PathBuf {
    state
        .registry_path
        .parent()
        .map_or_else(|| std::path::PathBuf::from("."), std::path::Path::to_path_buf)
}

/// Create an account. Mints no key, on purpose: a key is named for the place
/// it will be used, and one handed out at signup is the one that gets deployed
/// unnamed. The UI asks for a place and calls `POST /keys` next.
fn add_user(store: &mut Store, field: &dyn Fn(&str) -> String) -> Response<Body> {
    match store.add(&field("first_name"), &field("last_name"), &field("email")) {
        Ok(a) => json(201, &serde_json::json!({ "user": account_json(&a) }).to_string()),
        Err(e) => json(400, &serde_json::json!({ "error": e.to_string() }).to_string()),
    }
}

/// A boolean named in a request body, if one was named.
///
/// `None` for an absent field, which is not the same as an explicit `false` —
/// the save path uses that difference to decide whether the request is setting
/// the value or merely not mentioning it. Without it, every older caller that
/// does not know about a new flag would silently reset it.
fn flag_from(body: &Bytes, named: &str) -> Option<bool> {
    if named == "true" {
        return Some(true);
    }
    if named == "false" {
        return Some(false);
    }
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
}

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
        // Add or update a provider. Either a preset (one field: which) or a
        // hand-written one — base URL, models, key.
        (&Method::POST, "/api/providers") => save_provider(state, &field, body),
        // Rotate a provider's key without touching anything else.
        (&Method::POST, p) if p.starts_with("/api/providers/") && p.ends_with("/key") => {
            let id = p.trim_start_matches("/api/providers/").trim_end_matches("/key");
            rotate_provider_key(state, id, &field("key"))
        }
        (&Method::DELETE, p) if p.starts_with("/api/providers/") => {
            remove_provider(state, &mut store, p.trim_start_matches("/api/providers/"))
        }
        // Mappings. One table, global, applied before routing.
        (&Method::POST, "/api/mappings") => add_mapping(state, &field("from"), &field("to"), &field("note")),
        (&Method::DELETE, p) if p.starts_with("/api/mappings/") => {
            remove_mapping(state, p.trim_start_matches("/api/mappings/"))
        }
        // Pin an account to a provider, or clear the pin.
        (&Method::POST, p) if p.starts_with("/api/users/") && p.ends_with("/provider") => {
            let who = p.trim_start_matches("/api/users/").trim_end_matches("/provider");
            set_user_provider(state, &mut store, who, &field("provider"))
        }
        // The account's compaction level. Per PERSON, not per provider: the
        // operator reasoning about it is looking at a person, and routing picks
        // the hop per request — a level filed under a provider silently means
        // something else the moment that provider stops being where the traffic
        // goes. The hop still gets a say, because the harm is a property of the
        // hop; see the refusal below.
        (&Method::POST, p) if p.ends_with("/compact") => {
            set_user_compact(state, &mut store, &who("/compact"), &field("compact"))
        }
        // The account's tool policy. Suffix-matched like the rest, but handed
        // the whole request rather than a `field`-mangled string: it is an
        // object, and flattening it to a string would only be to flatten it
        // back. See `set_user_tools` for why the write is whole-object.
        (&Method::POST, p) if p.ends_with("/tools") => set_user_tools(&mut store, &who("/tools"), parsed.get("tools")),
        (&Method::POST, "/api/users") => add_user(&mut store, &field),
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

    /// The terminator is rarely on loopback, and a whole fleet logged under
    /// one address is the symptom.
    #[test]
    fn a_forwarding_header_is_believed_from_the_terminator_and_nowhere_else() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().expect("parse");
        // The address this actually failed on: Caddy in a container, reaching
        // the proxy across a bridge network. Not loopback, so the header used
        // to be discarded and every client was recorded as the container.
        assert!(is_trusted_hop(ip("172.31.112.6")), "RFC1918 is where terminators live");
        assert!(is_trusted_hop(ip("127.0.0.1")));
        assert!(is_trusted_hop(ip("::1")));
        assert!(is_trusted_hop(ip("10.1.2.3")));
        assert!(is_trusted_hop(ip("192.168.1.1")));
        assert!(is_trusted_hop(ip("fd00::1")), "unique-local");
        assert!(is_trusted_hop(ip("::ffff:10.0.0.1")), "v4-mapped private");

        // A public peer is a client talking to us directly. If it could set
        // its own X-Real-IP, the audit column would record whatever it liked.
        assert!(!is_trusted_hop(ip("203.0.113.7")));
        assert!(!is_trusted_hop(ip("2001:db8::1")));
        assert!(!is_trusted_hop(ip("::ffff:203.0.113.7")), "v4-mapped public");
    }

    #[test]
    fn the_real_client_is_taken_from_the_header_the_terminator_sets() {
        let build = |pairs: &[(&str, &str)]| {
            let mut h = hyper::HeaderMap::new();
            for (k, v) in pairs {
                h.insert(
                    hyper::header::HeaderName::from_bytes(k.as_bytes()).expect("name"),
                    v.parse().expect("value"),
                );
            }
            h
        };
        let ip = |s: &str| s.parse::<std::net::IpAddr>().expect("parse");
        let caddy = ip("172.31.112.6");

        // Caddy's X-Real-IP wins: it is a single unambiguous address, whereas
        // an XFF chain can have been extended by the client before it arrived.
        let req = build(&[("x-real-ip", "203.0.113.9"), ("x-forwarded-for", "198.51.100.1")]);
        assert_eq!(client_ip(&req, caddy), "203.0.113.9");

        // XFF alone still works, and the FIRST entry is the original client.
        let req = build(&[("x-forwarded-for", "198.51.100.1, 172.31.112.6")]);
        assert_eq!(client_ip(&req, caddy), "198.51.100.1");

        // No header: the socket peer is all we honestly have.
        assert_eq!(client_ip(&build(&[]), caddy), "172.31.112.6");

        // From an untrusted peer the headers are ignored outright — otherwise
        // any caller could write its own address into the audit log.
        let req = build(&[("x-real-ip", "10.0.0.1")]);
        assert_eq!(client_ip(&req, ip("203.0.113.7")), "203.0.113.7");
    }

    /// An inspection capture must carry what compaction removed.
    ///
    /// The panel renders the body AS SENT, so without this a compacted request
    /// is indistinguishable from one that was simply small — and "is
    /// compaction on, and is it doing anything" is exactly the question an
    /// operator opens the panel with.
    #[test]
    fn shape_body_reports_what_compaction_removed() {
        let route = routing::Route {
            provider_id: "p".to_owned(),
            model_id: "m".to_owned(),
            class: provider::Class::Balanced,
            // Work, not the classifier: the classifier is exempt from
            // compaction by construction, so it would report nothing and the
            // test would pass for the wrong reason.
            kind: classifier::Kind::Work,
            changed_from: None,
            reason: None,
        };
        // Ten tool-result turns, so six survive the window.
        let mut turns = Vec::new();
        for i in 0..10 {
            turns.push(format!(
                r#"{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"call_{i:02}","content":"{}"}}]}}"#,
                "r".repeat(500)
            ));
        }
        let body = Bytes::from(format!(r#"{{"model":"m","messages":[{}]}}"#, turns.join(",")));
        let before = body.len();

        let (after, record) = shape_body(
            &body,
            &route,
            "m",
            crate::compact::Compact::Tools,
            &crate::tools::Policy::default(),
            true,
        );
        let record = record.expect("a level was in force, so a record must come back");
        assert_eq!(record.level, "tools");
        assert_eq!(record.tool_results_elided, 4, "six of ten are inside the keep window");
        assert_eq!(record.bytes_before, u64::try_from(before).expect("fits"));
        assert_eq!(record.bytes_after, u64::try_from(after.len()).expect("fits"));
        assert!(record.saved() > 0, "the body did shrink");
        assert!(after.len() < before);

        // A level that is set but has nothing to remove still reports, because
        // "on, and elided nothing" is a different answer from "off".
        let plain = Bytes::from(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#);
        let (_, quiet) = shape_body(
            &plain,
            &route,
            "m",
            crate::compact::Compact::All,
            &crate::tools::Policy::default(),
            true,
        );
        let quiet = quiet.expect("still a record");
        assert_eq!(quiet.tool_results_elided, 0);
        assert_eq!(quiet.saved(), 0);

        // …and `none` reports nothing at all, which is the distinction the UI
        // draws by leaving the cell blank.
        let (untouched, off) = shape_body(
            &plain,
            &route,
            "m",
            crate::compact::Compact::None,
            &crate::tools::Policy::default(),
            true,
        );
        assert!(off.is_none(), "no level, no record");
        assert_eq!(untouched, plain, "and the bytes are the ones that arrived");
    }

    #[test]
    fn the_web_root_cannot_be_climbed_out_of() {
        let state = State {
            store: Mutex::new(Store::load(std::env::temp_dir().join("ta-proxy-web-test.json")).expect("store")),
            usage: Mutex::new(usage::Store::load(std::env::temp_dir().join("ta-proxy-web-usage"))),
            sched: Mutex::new(qos::Sched::new()),
            account: Mutex::new(account::Monitor::load(std::env::temp_dir())),
            inspect: Mutex::new(inspect::Store::load(std::env::temp_dir())),
            wake: tokio::sync::Notify::new(),
            registry: Mutex::new(provider::Registry::default()),
            registry_path: std::env::temp_dir().join("ta-proxy-providers-test.json"),
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
            provider: None,
            compact: crate::compact::Compact::None,
            tools: crate::tools::Policy::default(),
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
