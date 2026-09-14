// SPDX-License-Identifier: MPL-2.0

//! The Anthropic relay.
//!
//! The only route that carries a conversation, and the only one whose response
//! is a stream. Everything else in this tree is bookkeeping around it.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::Method;
use hyper::body::Frame;

use crate::http::guards::Arrival;
use crate::http::middleware::arrival::{header_of, is_trusted_hop};
use crate::server::{State, now_ms};
use crate::transport::{Reply, text};
use crate::users::Account;
use crate::{classifier, egress, inspect, openai, provider, qos, routing, usage};

/// One relayed request, already authenticated.
///
/// Grouped rather than passed one by one: the relay needs most of the request,
/// and a function taking seven arguments is where a caller starts putting them
/// in the wrong order. Everything here has already been through the guards —
/// the account is authenticated, the arrival is recorded, the body is read — so
/// there is nothing left for the relay to check before it forwards.
pub(crate) struct Relay<'a> {
    /// The account the presented key belongs to.
    pub account: &'a Account,
    /// Who called, and from where.
    pub arrival: &'a Arrival,
    /// The path below the relay's mount point, such as `/v1/messages`.
    ///
    /// A `String` rather than a slice because it is built from the route's path
    /// fragment, and the relay needs to hold it across the upstream call.
    pub sub_path: String,
    /// The query string, without the `?`, or empty.
    pub query: String,
    /// The payload, verbatim.
    pub body: Bytes,
}

impl<'a> Relay<'a> {
    /// Gather one request from what the guards already produced.
    ///
    /// The verb and the query string come from the arrival rather than from the
    /// route's typed parameters: this forwards a path, not a route, so what goes
    /// upstream must be what the client asked for — including a query string
    /// that nothing here parses.
    #[must_use]
    pub fn new(account: &'a Account, arrival: &'a Arrival, sub: &std::path::Path, body: Bytes) -> Self {
        Self {
            account,
            arrival,
            sub_path: format!("/{}", sub.display()),
            query: target_query(&arrival.target),
            body,
        }
    }
}

/// The query string of a target, or empty.
///
/// Split out because it is the one piece of path handling left: everything else
/// about a path is Rocket's now.
fn target_query(target: &str) -> String {
    target.split_once('?').map_or_else(String::new, |(_, q)| q.to_owned())
}

/// Forward one request to a provider, and stream the answer back.
///
/// This is the whole point of the proxy: nothing here parses the conversation,
/// because a relay that understands what it forwards breaks the day the
/// envelope changes. What it does understand is the request's *shape* — which
/// model was named, how large it is, which provider can take it — and that is
/// what [`shape_and_admit`] decides.
pub(crate) async fn anthropic(state: &Arc<State>, r: Relay<'_>) -> Reply {
    let Relay {
        account,
        arrival,
        sub_path: sub,
        query,
        body,
    } = r;
    let method = arrival.method.clone();
    let headers = &arrival.headers;
    let sub_pq = if query.is_empty() {
        sub.clone()
    } else {
        format!("{sub}?{query}")
    };
    let peer = arrival.peer;
    let ip = arrival.ip.clone();
    // Built here, where the raw headers and the socket peer are both in hand.
    // Kept alongside the resolved answer, because "what did we record" and
    // "why" are different questions and only the first one was answerable.
    let origin = inspect::Origin {
        peer: peer.to_string(),
        peer_trusted: is_trusted_hop(peer),
        client_ip: ip.clone(),
        x_real_ip: header_of(&arrival.headers, "x-real-ip"),
        x_forwarded_for: header_of(&arrival.headers, "x-forwarded-for"),
    };
    log::info!(
        "proxy: {method} {sub} for {} <{}> from {ip}",
        account.display_name(),
        account.email
    );

    let client_beta = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let client_headers = passthrough_headers(headers);
    let content_type = headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let is_post = method == Method::POST;
    // Already read at the edge into `InReq`, so the relay gets its bytes
    // without owning the request. Cloned because shaping takes it by value and
    // the capture path still needs the original below.
    let body = body.clone();

    // Only /v1/messages spends tokens; a metadata call should not queue
    // behind a fleet's generations.
    let metered = is_post && sub.contains("/messages");
    let (body, route, compaction, local_tools) = match shape_and_admit(state, account, body, metered).await {
        Ok(quad) => quad,
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
        local_tools,
        route: route.clone(),
        state: Arc::clone(state),
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

pub(crate) fn streamed<S>(meta: (u16, Option<String>), stream: S, route: &routing::Route) -> Reply
where
    S: futures_util::Stream<Item = Result<Frame<Bytes>, Infallible>> + Send + Sync + 'static,
{
    let mut reply = Reply::stream(meta.0, StreamBody::new(stream).boxed());
    if let Some(ct) = meta.1 {
        reply = reply.with_header("content-type", ct);
    }
    reply = reply.with_header(
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
        reply = reply.with_header(name, from.clone());
    }
    reply
}

pub(crate) fn pick_route(
    state: &Arc<State>,
    account: &Account,
    requested: &str,
    kind: classifier::Kind,
    health: &dyn Fn(&str) -> routing::Health,
) -> Option<routing::Route> {
    let pin = account.model.as_deref();
    let env = |v: &str| std::env::var(v).ok();
    let now = usage::now_secs();
    // Scoped: the registry lock is a std Mutex, and holding one across an
    // await makes this future non-Send — which the compiler reports as a
    // spawn failure a long way from here. Taken only for the decision.
    let registry = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    pin.map_or_else(
        || {
            routing::choose(
                &registry,
                requested,
                account.provider.as_deref(),
                kind,
                health,
                env,
                now,
            )
        },
        |pin| routing::choose_exact(&registry, pin, account.provider.as_deref(), kind, health, env, now),
    )
}

pub(crate) async fn shape_and_admit(
    state: &Arc<State>,
    account: &Account,
    body: Bytes,
    metered: bool,
) -> Result<(Bytes, routing::Route, Option<inspect::Compaction>, Vec<String>), Reply> {
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
            // …and no tools were shaped to find a local one in.
            Vec::new(),
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
    // A per-person model pin is resolved in place of the name the caller used,
    // but the body is left carrying the caller's name on purpose: `shape_body`
    // renames the request to `route.model_id`, which the pin has just made the
    // pinned id. Overwriting `requested` itself would make the two equal, the
    // rename a no-op, and the pin silently do nothing but choose a route.
    //
    // The pin decides WHERE, not whether. An id no configured provider serves
    // fails to route rather than falling back — the same contract as the
    // provider pin above, and the reason both are enforced rather than
    // preferred.
    //
    // Resolved EXACTLY, not as a class hint: someone who pins `gpt-5.6-luna`
    // means that model, and letting the ordinary ladder answer with whichever
    // fast model is cheapest would quietly serve a different vendor than the
    // one they chose.
    let pin = account.model.as_deref();
    let Some(mut route) = pick_route(state, account, &requested, kind, &health) else {
        // Nothing configured can serve this at any class. A guess would be
        // worse than saying so.
        return Err(text(
            503,
            "tab-atelier-proxy: no provider available for this request (all blocked, or none configured)",
        ));
    };
    // The pin overrides the name the caller used, so record what it overrode —
    // the client is entitled to know it was answered by a model it did not
    // name. Left to here rather than done inside routing because only this
    // function parsed the request and saw the original.
    if pin.is_some() && route.model_id != requested {
        route.changed_from = Some(requested.clone());
    }
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
    // What the far end is. It decides whether the client's claim to be Claude
    // Code is true — and so whether anything about it should be rewritten — and
    // whether admission applies at all, since only a destination that spends the
    // subscription is gated on it.
    let (on_subscription, vendor) = {
        let registry = state.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.get(&route.provider_id).map_or_else(
            || (true, crate::identity::Vendor::Anthropic),
            |p| (p.uses_the_subscription(), crate::identity::Vendor::of(p)),
        )
    };
    let (body, compaction, local) = shape_body(
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
        vendor,
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
    let est = qos::estimate_cost(&body);
    if on_subscription && let Err(retry_after) = admit(state, &account.id, account.weight, est).await {
        log::warn!(
            "proxy: 429 for {} after waiting — retry in {retry_after}s",
            account.display_name()
        );
        let resp = text(
            429,
            &format!("tab-atelier-proxy: the shared quota is saturated; retry in {retry_after}s"),
        )
        .with_header("retry-after", retry_after.to_string());
        // A refused call still happened, and an operator looking at a quiet
        // graph should see the refusals.
        record(state, &account.id, None, usage::Tokens::default(), 429);
        return Err(resp);
    }
    Ok((body, route, compaction, local))
}

pub(crate) fn shape_body(
    body: &Bytes,
    route: &routing::Route,
    requested: &str,
    compact: crate::compact::Compact,
    policy: &crate::tools::Policy,
    takes_cache: bool,
    vendor: crate::identity::Vendor,
) -> (Bytes, Option<inspect::Compaction>, Vec<String>) {
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
    // Did the client claim to be Claude Code to a model that is not Claude?
    // Anthropic's own requests are the one case where the claim is true, and
    // so the one case left alone.
    let rewrites_identity = vendor != crate::identity::Vendor::Anthropic;
    // The tool policy joins the early-out rather than being checked after
    // it. An account with no policy must not pay for the parse and the
    // re-encode, and that is most accounts on most requests.
    if rename.is_none() && level.is_none() && policy.is_none_or(crate::tools::is_noop) && !rewrites_identity {
        return (body.clone(), None, Vec::new());
    }

    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
        // Not JSON. The old `rewrite_model` swallowed this and forwarded the
        // original, which is right — the far end gives a better error than we
        // could — and compaction has nothing to act on either way.
        return (body.clone(), None, Vec::new());
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
    // After the tool policy, deliberately too: the identity rewrite drops the
    // sentence forbidding the Agent tool once no such tool is left, and "left"
    // means what the policy sent rather than what the client offered. It also
    // puts an added tool's description through the same prose rewrite, which
    // running before the policy would miss.
    if rewrites_identity {
        crate::identity::apply(&mut v, vendor, &route.model_id);
    }
    let Ok(encoded) = serde_json::to_vec(&v) else {
        return (body.clone(), None, Vec::new());
    };
    if governed.changed() {
        log::info!(
            "proxy: tools {} offered → {} sent on {}: {} removed, {} added, {} pinned, {} local{}",
            governed.offered,
            governed.sent,
            route.provider_id,
            governed.removed.len(),
            governed.added,
            governed.pinned.len(),
            governed.local.len(),
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
             {} thinking dropped, {} write payloads stubbed, {} notices dropped",
            route.provider_id,
            route.model_id,
            encoded.len(),
            elided.tool_results_elided,
            elided.tool_results_kept_for_error,
            elided.thinking_dropped,
            elided.writes_elided,
            elided.notices_dropped
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
            tool_results_kept_small: elided.tool_results_kept_small,
            thinking_dropped: elided.thinking_dropped,
            writes_elided: elided.writes_elided,
            writes_kept_for_error: elided.writes_kept_for_error,
            notices_dropped: elided.notices_dropped,
        })
    };
    (Bytes::from(encoded), record, governed.local)
}

pub(crate) fn provider_health(state: &Arc<State>) -> impl Fn(&str) -> routing::Health + '_ {
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

pub(crate) async fn admit(state: &Arc<State>, id: &str, weight: u32, est: u64) -> Result<(), u64> {
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

pub(crate) struct Forward {
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
    /// Names the policy resolved to a tool this proxy answers itself, so the
    /// forwarder can put the result in the body. Empty for almost every
    /// request — see [`crate::localtool`].
    local_tools: Vec<String>,
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
pub(crate) fn passthrough_headers(headers: &hyper::HeaderMap) -> Vec<(String, String)> {
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

pub(crate) fn begin_capture(
    f: &Forward,
    path: &str,
    body: &[u8],
    hdrs: &[(String, String)],
) -> Option<inspect::Capture> {
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
        path,
        provider: &f.route.provider_id,
        kind: f.route.kind,
        headers: hdrs,
        body,
        origin: Some(f.origin.clone()),
    }))
}

pub(crate) fn finish_capture(
    f: &Forward,
    pending: Option<inspect::Capture>,
    status: u16,
    tokens: &usage::Tokens,
    excerpt: &[u8],
    dropped: usize,
    model: Option<&str>,
) {
    let Some(mut c) = pending else { return };
    c.status = Some(status);
    c.tokens = Some(*tokens);
    // The reply as it went to the client, capped by `tap`. Kept verbatim --
    // newlines and all -- because the panel is where a reply is read
    // properly, and reflowing it here would destroy the shape being read.
    c.response_excerpt = (!excerpt.is_empty()).then(|| String::from_utf8_lossy(excerpt).into_owned());
    c.response_truncated = dropped > 0;
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

pub(crate) fn tap(excerpt: &mut Vec<u8>, bytes: &[u8]) -> usize {
    let room = inspect::MAX_EXCERPT.saturating_sub(excerpt.len());
    let kept = bytes.len().min(room);
    excerpt.extend_from_slice(&bytes[..kept]);
    bytes.len() - kept
}

const MAX_LOGGED_REPLY: usize = 512;

/// A reply as one log line.
///
/// Newlines are escaped rather than kept: a multi-line reply would otherwise
/// become a dozen log records, and the prefix that says which request it
/// belongs to would land on the first of them only.
pub(crate) fn one_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    for c in text.chars() {
        if out.len() >= MAX_LOGGED_REPLY {
            out.push('…');
            break;
        }
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

pub(crate) fn pump<R: std::io::Read>(
    reader: &mut R,
    wire: provider::Wire,
    is_sse: bool,
    status: u16,
    sniffer: &mut usage::Sniffer,
    excerpt: &mut Vec<u8>,
    body_tx: &tokio::sync::mpsc::Sender<Bytes>,
) -> usize {
    let mut dropped = 0usize;
    let mut buf = [0u8; 8192];
    if wire != provider::Wire::Openai {
        loop {
            match std::io::Read::read(reader, &mut buf) {
                Ok(0) | Err(_) => break, // EOF, or an upstream read error
                Ok(n) => {
                    sniffer.feed(&buf[..n]);
                    dropped += tap(excerpt, &buf[..n]);
                    if body_tx.blocking_send(Bytes::copy_from_slice(&buf[..n])).is_err() {
                        break; // client hung up
                    }
                }
            }
        }
        return dropped;
    }
    if is_sse {
        let mut translator = openai::Translator::new();
        loop {
            match std::io::Read::read(reader, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut gone = false;
                    for chunk in translator.feed(&buf[..n]) {
                        sniffer.feed(&chunk);
                        dropped += tap(excerpt, &chunk);
                        if body_tx.blocking_send(chunk).is_err() {
                            gone = true;
                            break;
                        }
                    }
                    if gone {
                        break;
                    }
                }
            }
        }
        // The tail that closes any block left open and emits the final usage.
        // Without it a well-formed upstream response ends as a truncated one.
        for chunk in translator.finish() {
            sniffer.feed(&chunk);
            dropped += tap(excerpt, &chunk);
            if body_tx.blocking_send(chunk).is_err() {
                break;
            }
        }
        return dropped;
    }
    // One JSON object, which for this vendor is also every error. Buffered
    // because a single object cannot be translated in pieces.
    let mut raw = Vec::new();
    loop {
        match std::io::Read::read(reader, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
    let out = if (200..300).contains(&status) {
        let parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
        openai::from_chat(&parsed)
    } else {
        // An error keeps the vendor's status but wears Anthropic's shape, so a
        // client that only understands one dialect still reads the message.
        openai::to_anthropic_error(status, &raw)
    };
    let bytes = Bytes::from(serde_json::to_vec(&out).unwrap_or_default());
    sniffer.feed(&bytes);
    dropped += tap(excerpt, &bytes);
    let _ = body_tx.blocking_send(bytes);
    dropped
}

pub(crate) fn upstream_headers(
    f: &Forward,
    auth: (&'static str, String),
    wire: provider::Wire,
) -> Vec<(String, String)> {
    let mut hdrs: Vec<(String, String)> = vec![
        ("Content-Type".to_owned(), f.content_type.clone()),
        (auth.0.to_owned(), auth.1),
    ];
    if wire != provider::Wire::Anthropic {
        // No vendor on this hop is Anthropic, so there is no Claude Code
        // identity to preserve and the request would otherwise arrive with no
        // User-Agent at all — every real client sends one, and some providers
        // reject or throttle a request without it.
        hdrs.push(("User-Agent".to_owned(), egress::USER_AGENT.to_owned()));
        return hdrs;
    }
    // The client's own beta flags are merged in, not replaced: a body field
    // gated behind a flag the client opted into is rejected upstream as an
    // unknown input if only our flags survive.
    hdrs.push((
        "anthropic-beta".to_owned(),
        egress::merge_beta(f.client_beta.as_deref(), egress::ANTHROPIC_BETA),
    ));
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
    hdrs
}

pub(crate) fn forward(
    f: &Forward,
    // By value: a oneshot Sender is consumed by `send`, which is also what
    // makes "exactly one answer" a type-level guarantee rather than a habit.
    meta_tx: tokio::sync::oneshot::Sender<Result<(u16, Option<String>), String>>,
    body_tx: &tokio::sync::mpsc::Sender<Bytes>,
) {
    let (base, auth, wire) = match destination(&f.state, &f.route.provider_id) {
        Ok(d) => d,
        Err(e) => {
            let _ = meta_tx.send(Err(e));
            return;
        }
    };
    // The path and the body both depend on the dialect, and both are needed
    // before the headers so the capture below records what actually left.
    let (path, body) = upstream_body(f, wire);
    let url = upstream_url(&base, wire, &path);
    let agent = egress::relay_agent();
    let hdrs = upstream_headers(f, auth, wire);
    // The request as it will actually leave: after routing rewrote the model,
    // after the beta flags were merged, with the proxy's credential in place.
    // That is the thing nobody can otherwise see, and it is the whole reason
    // inspection exists. Scrubbing happens inside `inspect::capture`.
    let mut pending = begin_capture(f, &path, &body, &hdrs);

    let sent = if f.is_post {
        let mut rb = agent.post(&url);
        for (k, v) in &hdrs {
            rb = rb.header(k.as_str(), v);
        }
        rb.send(&body[..])
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
    let upstream_ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // How the body is read is the upstream's call, not the client's: a vendor
    // may answer a `stream: true` request with one buffered object.
    let is_sse = upstream_ctype.as_deref().is_some_and(|c| c.contains("event-stream"));
    // What the client is told it is getting. A translated stream is Anthropic
    // SSE whatever the vendor called it, and passing `application/json` through
    // would have the client buffer a live stream into one unparsable object.
    let ctype = if wire == provider::Wire::Openai {
        Some(
            if is_sse {
                "text/event-stream"
            } else {
                "application/json"
            }
            .to_owned(),
        )
    } else {
        upstream_ctype.clone()
    };
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
    // Count what the vendor says it billed, read off the translated response as
    // it goes past. Asking the client to report its own usage would be
    // unenforceable, and re-tokenising the prompt here would be a guess.
    let mut sniffer = usage::Sniffer::new(upstream_ctype.as_deref());
    if meta_tx.send(Ok((status, ctype))).is_err() {
        // The client is gone, but the call was still made and still cost
        // tokens — so it is recorded anyway, just without a body to read.
        record(&f.state, &f.account_id, None, usage::Tokens::default(), status);
        // The capture is still filed: the request WAS made and the client
        // hanging up does not unmake it. No token counts, because the
        // response was never read.
        finish_capture(f, pending.take(), status, &usage::Tokens::default(), &[], 0, None);
        return;
    }
    let mut reader = resp.body_mut().as_reader();
    let mut excerpt = Vec::new();
    let dropped = pump(&mut reader, wire, is_sse, status, &mut sniffer, &mut excerpt, body_tx);
    let (model, tokens) = sniffer.finish();
    record(&f.state, &f.account_id, model.as_deref(), tokens, status);
    log::info!(
        "proxy: {status} for {}: {} in / {} out ({} cache read, {} cache write) reply: {}",
        model.as_deref().unwrap_or(&f.route.model_id),
        tokens.input,
        tokens.output,
        tokens.cache_read,
        tokens.cache_write,
        one_line(&excerpt)
    );
    finish_capture(f, pending.take(), status, &tokens, &excerpt, dropped, model.as_deref());
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

pub(crate) fn destination(
    state: &State,
    provider_id: &str,
) -> Result<(String, (&'static str, String), provider::Wire), String> {
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
    let wire = chosen.map_or(provider::Wire::Anthropic, |p| p.wire);
    let auth = match chosen.map(|p| &p.auth) {
        None | Some(provider::Auth::ClaudeOauth) => {
            let t = egress::oauth_access_token().map_err(|e| format!("egress oauth: {e}"))?;
            ("Authorization", format!("Bearer {t}"))
        }
        // A third-party provider takes its key in the header its own API uses:
        // Anthropic-compatible services follow the Anthropic SDKs (`x-api-key`),
        // OpenAI follows the OpenAI SDKs (`Authorization: Bearer`). Env var or
        // file is the provider's choice; the route only cares that one resolved.
        Some(auth) => {
            let key = auth.secret_with(|v| std::env::var(v).ok())?;
            match wire {
                provider::Wire::Openai => ("Authorization", format!("Bearer {key}")),
                provider::Wire::Anthropic => ("x-api-key", key),
            }
        }
    };
    Ok((base, auth, wire))
}

pub(crate) fn observe_upstream(
    f: &Forward,
    status: u16,
    remaining: Option<u64>,
    reset_in: Option<u64>,
    retry_after: Option<u64>,
) {
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

pub(crate) fn record(state: &State, account_id: &str, model: Option<&str>, tokens: usage::Tokens, status: u16) {
    let mut u = state.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    u.record(account_id, model, tokens, (200..300).contains(&status));
}

pub(crate) fn upstream_url(base: &str, wire: provider::Wire, path: &str) -> String {
    match wire {
        provider::Wire::Openai => openai::chat_url(base),
        provider::Wire::Anthropic => format!("{base}{path}"),
    }
}

pub(crate) fn upstream_body(f: &Forward, wire: provider::Wire) -> (String, Bytes) {
    match wire {
        provider::Wire::Anthropic => {
            if f.local_tools.is_empty() {
                return (f.sub_pq.clone(), f.body.clone());
            }
            let body = serde_json::from_slice::<serde_json::Value>(&f.body).map_or_else(
                |_| f.body.clone(),
                |mut parsed| {
                    crate::localtool::inject(&mut parsed, &f.local_tools);
                    Bytes::from(serde_json::to_vec(&parsed).unwrap_or_default())
                },
            );
            (f.sub_pq.clone(), body)
        }
        provider::Wire::Openai => {
            // Validated as JSON on the way in, so a parse failure here would be
            // a bug rather than bad input; `Null` translates to the minimal
            // valid request instead of panicking in a proxy.
            let mut parsed: serde_json::Value = serde_json::from_slice(&f.body).unwrap_or(serde_json::Value::Null);
            crate::localtool::inject(&mut parsed, &f.local_tools);
            (
                "/v1/chat/completions".to_owned(),
                Bytes::from(serde_json::to_vec(&openai::to_chat(&parsed)).unwrap_or_default()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `OpenAI` base URL is written with the version in it, and so is the
    /// path.
    ///
    /// The provider is configured as `https://api.openai.com/v1` and the relay
    /// forwards `/v1/chat/completions` — joining them the obvious way gives
    /// `/v1/v1/chat/completions` and a 404 from the provider that names nothing
    /// about the proxy. `openai::chat_url` is what strips the duplicate.
    #[test]
    fn an_openai_base_is_not_versioned_twice() {
        assert_eq!(
            upstream_url(
                "https://api.openai.com/v1",
                provider::Wire::Openai,
                "/v1/chat/completions"
            ),
            "https://api.openai.com/v1/chat/completions"
        );
        // Anthropic's base carries no version, so the path supplies it and
        // there is nothing to reconcile.
        assert_eq!(
            upstream_url("https://api.anthropic.com", provider::Wire::Anthropic, "/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    /// The shaping pass reports what it removed, and a level that is on but has
    /// nothing to remove still reports.
    ///
    /// The distinction matters because "on, and elided nothing" and "off" are
    /// different answers to the operator asking why a transcript was not
    /// compacted — and `None` is how this signals the second.
    #[test]
    fn shaping_reports_what_compaction_removed() {
        let route = routing::Route {
            provider_id: "p".to_owned(),
            model_id: "m".to_owned(),
            class: provider::Class::Balanced,
            // Work, not the classifier: the classifier is exempt from
            // compaction by construction, so it would report nothing and this
            // test would pass for the wrong reason.
            kind: classifier::Kind::Work,
            changed_from: None,
            reason: None,
        };
        // Ten tool-result turns, of which the keep window leaves six.
        let turns: Vec<String> = (0..10)
            .map(|i| {
                format!(
                    r#"{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"call_{i:02}","content":"{}"}}]}}"#,
                    "r".repeat(500)
                )
            })
            .collect();
        let body = Bytes::from(format!(r#"{{"model":"m","messages":[{}]}}"#, turns.join(",")));
        let before = body.len();

        let (after, record, _) = shape_body(
            &body,
            &route,
            "m",
            crate::compact::Compact::Tools,
            &crate::tools::Policy::default(),
            true,
            crate::identity::Vendor::Anthropic,
        );
        let record = record.expect("a level was in force, so a record comes back");
        assert_eq!(record.level, "tools");
        assert_eq!(record.tool_results_elided, 4, "six of ten are inside the keep window");
        assert_eq!(record.bytes_before, u64::try_from(before).expect("fits"));
        assert_eq!(record.bytes_after, u64::try_from(after.len()).expect("fits"));
        assert!(record.saved() > 0, "the body did shrink");
        assert!(after.len() < before);

        let plain = Bytes::from(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#);
        let (_, quiet, _) = shape_body(
            &plain,
            &route,
            "m",
            crate::compact::Compact::All,
            &crate::tools::Policy::default(),
            true,
            crate::identity::Vendor::Anthropic,
        );
        let quiet = quiet.expect("still a record");
        assert_eq!(quiet.tool_results_elided, 0);
        assert_eq!(quiet.level, "all");
    }

    /// The classifier is never compacted, whatever the account's level is.
    ///
    /// Its transcript is the conversation under judgement, and trimming it
    /// would mean the safety decision was made on a redacted copy — with the
    /// redaction chosen by the pass that reads it. This is a property of what
    /// the classifier IS, so it holds even at the most aggressive level.
    #[test]
    fn the_classifier_transcript_is_never_compacted() {
        let route = routing::Route {
            provider_id: "p".to_owned(),
            model_id: "m".to_owned(),
            class: provider::Class::Fast,
            kind: classifier::Kind::Classifier,
            changed_from: None,
            reason: None,
        };
        let turns: Vec<String> = (0..10)
            .map(|i| {
                format!(
                    r#"{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"call_{i:02}","content":"{}"}}]}}"#,
                    "r".repeat(500)
                )
            })
            .collect();
        let body = Bytes::from(format!(r#"{{"model":"m","messages":[{}]}}"#, turns.join(",")));

        let (after, record, _) = shape_body(
            &body,
            &route,
            "m",
            crate::compact::Compact::All,
            &crate::tools::Policy::default(),
            true,
            crate::identity::Vendor::Anthropic,
        );
        assert!(record.is_none(), "the classifier is exempt, so nothing is reported");
        assert_eq!(after, body, "and the body is untouched");
    }

    /// A request that named a model the router did not choose is rewritten.
    ///
    /// This is the fallback path: the account asked for something unavailable
    /// and routing picked another hop, so the payload has to name what is
    /// actually being called or the provider refuses it.
    #[test]
    fn a_model_the_router_overrode_is_rewritten_in_the_body() {
        let route = routing::Route {
            provider_id: "p".to_owned(),
            model_id: "chosen-by-router".to_owned(),
            class: provider::Class::Balanced,
            kind: classifier::Kind::Work,
            changed_from: Some("asked-for".to_owned()),
            reason: None,
        };
        let body = Bytes::from(r#"{"model":"asked-for","messages":[]}"#);

        let (after, _, local) = shape_body(
            &body,
            &route,
            "asked-for",
            crate::compact::Compact::None,
            &crate::tools::Policy::default(),
            true,
            crate::identity::Vendor::Anthropic,
        );
        let text = String::from_utf8_lossy(&after);
        assert!(text.contains("chosen-by-router"), "{text}");
        assert!(
            !text.contains("\"asked-for\""),
            "the model actually being called is the one now named: {text}"
        );
        assert!(
            local.is_empty(),
            "no local tool was asked for, so none is injected: {local:?}"
        );
    }

    /// A request that named the model the router chose is left alone.
    ///
    /// Re-serializing it would drop any field this proxy does not model, so the
    /// body is passed through byte for byte when there is nothing to change.
    #[test]
    fn a_clean_body_is_passed_through_byte_for_byte() {
        let route = routing::Route {
            provider_id: "p".to_owned(),
            model_id: "m".to_owned(),
            class: provider::Class::Balanced,
            kind: classifier::Kind::Work,
            changed_from: None,
            reason: None,
        };
        // A field the proxy has no type for: if the body were rebuilt from a
        // parsed value, this would vanish.
        let body = Bytes::from(r#"{"model":"m","messages":[],"future_field":{"x":1}}"#);

        let (after, _, local) = shape_body(
            &body,
            &route,
            "m",
            crate::compact::Compact::None,
            &crate::tools::Policy::default(),
            true,
            crate::identity::Vendor::Anthropic,
        );
        assert_eq!(after, body, "nothing to change, so nothing is rebuilt");
        assert!(local.is_empty());
    }
}
