// SPDX-License-Identifier: MPL-2.0

//! The HTTP side of the intention pane: the six routes the browser talks to.
//!
//! Split from [`crate::intent`], which owns the files. This module owns the
//! conversation — who starts a worker, how a message reaches it, what a reply
//! looks like — and it changes for different reasons than the file layer does.
//!
//! # The Kiosk orchestrates, the daemon executes
//!
//! Starting a worker is a `POST /tabs` to the daemon; asking it something is a
//! `POST /tabs/by-id/{id}/catbus/message`. This crate never spawns a process
//! itself and never talks to a model: it says what should happen and the daemon
//! does it. That keeps one authority over tabs, and it is why an intention
//! survives a kiosk restart — the files are here, the tabs are there.
//!
//! # The caller's token is the token
//!
//! Every route takes the browser's `?token=` and forwards it to the daemon
//! rather than holding one of its own. A Kiosk that kept a master token would
//! become a way to use it without knowing it, and its own access rules would
//! stop meaning anything.

use std::path::PathBuf;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Method, StatusCode};

use crate::intent::Intentions;
use crate::{BoxBody, MAX_REQUEST_BODY, Upstream, json_response};

/// Which `/intent` operation a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    /// `GET /intent/list`
    List,
    /// `POST /intent/new`
    New,
    /// `GET /intent/{slug}`
    Get { slug: String },
    /// `POST /intent/{slug}/ask`
    Ask { slug: String },
    /// `POST /intent/{slug}/promote`
    Promote { slug: String },
    /// `POST /intent/{slug}/launch`
    Launch { slug: String },
}

impl Call {
    /// Whether this operation reads the request body.
    ///
    /// `Launch` does not: everything it needs is in the path.
    #[must_use]
    pub const fn wants_body(&self) -> bool {
        matches!(self, Self::New | Self::Ask { .. })
    }
}

/// Parse a method and path into an intention operation.
///
/// Returns `None` for anything else under `/intent/`, which the caller then
/// proxies — a path this crate does not recognise is the daemon's business,
/// exactly as it is for every other path.
///
/// A slug is validated here rather than later so a route that cannot name a
/// file never reaches the file layer: `/intent/../../etc/passwd` is not a slug
/// and yields `None`.
/// Slugs a route name already occupies.
///
/// `GET /intent/list` answers with the list and `POST /intent/new` creates; a
/// file called `list.md` would be shadowed by the first, unreadable through the
/// Kiosk forever. Reserving the two names costs two titles and removes a class
/// of "why is my intention not there" that has no visible cause.
pub(crate) const RESERVED_SLUGS: [&str; 2] = ["list", "new"];

/// Is this slug free of the route names?
#[must_use]
pub(crate) fn is_usable_slug(slug: &str) -> bool {
    crate::intent::is_slug(slug) && !RESERVED_SLUGS.contains(&slug)
}

#[must_use]
pub(crate) fn call_for(method: &Method, path: &str) -> Option<Call> {
    let rest = path.strip_prefix("/intent")?;
    match (method, rest) {
        (&Method::GET, "/list") => Some(Call::List),
        (&Method::POST, "/new") => Some(Call::New),
        (&Method::GET, _) => {
            let slug = rest.strip_prefix('/')?;
            is_usable_slug(slug).then(|| Call::Get { slug: slug.to_string() })
        }
        (&Method::POST, _) => {
            let tail = rest.strip_prefix('/')?;
            let (suffix, build): (&str, fn(String) -> Call) = if tail.ends_with("/ask") {
                ("/ask", |slug| Call::Ask { slug })
            } else if tail.ends_with("/promote") {
                ("/promote", |slug| Call::Promote { slug })
            } else if tail.ends_with("/launch") {
                ("/launch", |slug| Call::Launch { slug })
            } else {
                return None;
            };
            let slug = tail.strip_suffix(suffix)?;
            is_usable_slug(slug).then(|| build(slug.to_string()))
        }
        _ => None,
    }
}

/// Everything the intention routes need: the files, and the daemon to reach.
pub(crate) struct State {
    pub(crate) intentions: Intentions,
    pub(crate) upstream: Arc<Upstream>,
    /// Command typed into a freshly created worker tab.
    ///
    /// Empty by default, and that default is deliberate: the command names a
    /// local agent runner (`catbus-run.sh` and the model it starts), which is a
    /// statement about the machine and not about this repository. An operator
    /// sets `TAB_ATELIER_INTENT_CMD`; without it an intention is still created
    /// and can be driven by hand, which is better than a hardcoded path that
    /// works on exactly one machine.
    pub(crate) worker_cmd: Option<String>,
}

impl State {
    /// Read the intentions directory and the worker command from the
    /// environment.
    ///
    /// # Errors
    /// Fails when the directory cannot be created.
    pub(crate) fn from_env(upstream: Arc<Upstream>) -> Result<Self, String> {
        let dir = match std::env::var("TAB_ATELIER_INTENTIONS_DIR") {
            Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
            _ => crate::intent::default_dir()?,
        };
        let worker_cmd = std::env::var("TAB_ATELIER_INTENT_CMD")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Ok(Self {
            intentions: Intentions::new(dir)?,
            upstream,
            worker_cmd,
        })
    }

    /// The name a worker tab carries for `slug`, and the only thing that ties a
    /// tab back to its intention.
    ///
    /// A name rather than a stored id because the id does not exist when the
    /// creation is queued: `POST /tabs` returns before the tab is made. The
    /// name is deterministic, so the tab is findable as soon as it appears,
    /// with no polling window and nothing to keep in sync.
    fn worker_name(slug: &str) -> String {
        format!("Intention {slug}")
    }

    /// The id of the worker driving `slug`, if it is running.
    fn find_worker(&self, slug: &str, token: &str) -> Result<Option<String>, String> {
        let want = Self::worker_name(slug);
        let tabs = self.daemon_get(token, "/tabs")?;
        let parsed: serde_json::Value =
            serde_json::from_str(&tabs).map_err(|e| format!("daemon /tabs is not JSON: {e}"))?;
        let list = parsed
            .get("tabs")
            .and_then(serde_json::Value::as_array)
            .or_else(|| parsed.as_array())
            .ok_or_else(|| "daemon /tabs has no tab list".to_string())?;
        Ok(list
            .iter()
            .find(|t| t.get("name").and_then(serde_json::Value::as_str) == Some(want.as_str()))
            .and_then(|t| t.get("id").and_then(serde_json::Value::as_str))
            .map(str::to_string))
    }

    /// A `GET` against the daemon, with the caller's token.
    fn daemon_get(&self, token: &str, path: &str) -> Result<String, String> {
        let request = http::Request::builder()
            .method("GET")
            .uri(format!("{}{path}", self.upstream.base))
            .header("Authorization", format!("Bearer {token}"))
            .body(Vec::new())
            .map_err(|e| format!("bad request: {e}"))?;
        let mut response = self.upstream.agent.run(request).map_err(|e| format!("{e}"))?;
        response.body_mut().read_to_string().map_err(|e| format!("read: {e}"))
    }
}

/// One request's context: the shared state, plus the caller's token.
///
/// The token is a **parameter** rather than something the state remembers.
/// These routes hand their work to `spawn_blocking`, which runs on whichever
/// thread the pool picks — a thread-local set on the request's task would not
/// be there, and every daemon call would go out with an empty `Bearer`. Keeping
/// it in the value that crosses the boundary makes that impossible.
pub(crate) struct Ctx {
    pub(crate) state: Arc<State>,
    pub(crate) token: String,
}

/// Serve one intention request.
pub(crate) async fn handle(call: Call, body: Vec<u8>, token: String, state: Arc<State>) -> hyper::Response<BoxBody> {
    // The daemon calls inside are blocking. Running them here would stall every
    // other connection for the duration, which for `ask` is as long as the
    // model takes to answer — minutes, not milliseconds.
    let ctx = Ctx { state, token };
    let joined = tokio::task::spawn_blocking(move || dispatch(&call, &body, &ctx)).await;
    match joined {
        Ok(Ok(value)) => json_response(StatusCode::OK, &value),
        Ok(Err(failure)) => json_response(failure.status, &serde_json::json!({ "error": failure.message })),
        Err(join_err) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({ "error": format!("task panicked: {join_err}") }),
        ),
    }
}

/// A failure with the status the browser should see.
struct Failure {
    status: StatusCode,
    message: String,
}

impl Failure {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
    fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }
}

fn dispatch(call: &Call, body: &[u8], ctx: &Ctx) -> Result<serde_json::Value, Failure> {
    match call {
        Call::List => list(ctx),
        Call::New => new(body, ctx),
        Call::Get { slug } => get(slug, ctx),
        Call::Ask { slug } => ask(slug, body, ctx),
        Call::Promote { slug } => promote(slug, ctx),
        Call::Launch { slug } => launch(slug, ctx),
    }
}

fn list(ctx: &Ctx) -> Result<serde_json::Value, Failure> {
    let items = ctx.state.intentions.list().map_err(|e| Failure {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: e,
    })?;
    let rendered: Vec<serde_json::Value> = items
        .iter()
        .map(|s| {
            serde_json::json!({
                "slug": s.slug,
                "title": s.title,
                "repo": s.repo,
                "ready": s.ready,
            })
        })
        .collect();
    Ok(serde_json::json!({ "intentions": rendered }))
}

fn get(slug: &str, ctx: &Ctx) -> Result<serde_json::Value, Failure> {
    let raw = ctx.state.intentions.read(slug).map_err(Failure::not_found)?;
    // The worker is looked up live rather than read from the front matter: a
    // tab that has exited is not a tab, and a stale id in a file would make the
    // pane offer to talk to something that is gone.
    let worker = ctx.state.find_worker(slug, &ctx.token).ok().flatten();
    if let Some(id) = &worker {
        // Recorded for the next reader, but never trusted for liveness.
        let _ = ctx.state.intentions.set_tab(slug, id);
    }
    Ok(serde_json::json!({ "slug": slug, "markdown": raw, "worker": worker }))
}

fn new(body: &[u8], ctx: &Ctx) -> Result<serde_json::Value, Failure> {
    let parsed: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| Failure::bad_request(format!("bad JSON: {e}")))?;
    let field = |name: &str| {
        parsed
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let title = field("title");
    let repo = field("repo");
    let pitch = field("pitch");
    if title.is_empty() {
        return Err(Failure::bad_request("`title` is required"));
    }
    if repo.is_empty() {
        return Err(Failure::bad_request("`repo` is required"));
    }

    // Refused rather than suffixed: a PO who titles an intention "List" should
    // be told why, not silently given `list-2` and left wondering.
    if RESERVED_SLUGS.contains(&crate::intent::slugify(&title).as_str()) {
        return Err(Failure::bad_request(format!(
            "{:?} is a route name; pick another title",
            crate::intent::slugify(&title)
        )));
    }
    let summary = ctx
        .state
        .intentions
        .create(&title, &repo, &pitch)
        .map_err(Failure::bad_request)?;
    let slug = summary.slug.clone();

    // The worker is best-effort: an intention with no runner configured is
    // still a usable intention, and failing the creation because a model could
    // not be started would lose the PO's text.
    let spawned = spawn_worker(&slug, &repo, ctx);
    let (worker, note) = match spawned {
        Ok(Some(id)) => {
            // The worker is started with nothing appended; the intention goes
            // in as its first message. Failing to hand it over does not undo
            // the intention — the PO's text is the thing worth keeping — but it
            // is reported, because a worker that never got the brief would
            // otherwise answer as if it had.
            let note = open_with(&slug, &id, ctx).err();
            (Some(id), note)
        }
        Ok(None) => (
            None,
            Some("no worker command configured; set TAB_ATELIER_INTENT_CMD".to_string()),
        ),
        Err(err) => (None, Some(err)),
    };
    Ok(serde_json::json!({
        "slug": slug,
        "title": summary.title,
        "repo": summary.repo,
        "ready": false,
        "worker": worker,
        "note": note,
    }))
}

fn ask(slug: &str, body: &[u8], ctx: &Ctx) -> Result<serde_json::Value, Failure> {
    let parsed: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| Failure::bad_request(format!("bad JSON: {e}")))?;
    let text = parsed
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Failure::bad_request("`text` is required"))?;

    let tab = ctx
        .state
        .find_worker(slug, &ctx.token)
        .map_err(Failure::upstream)?
        .ok_or_else(|| Failure::bad_request("no worker is running for this intention"))?;

    // The PO's turn is written before the model is asked: if the answer never
    // comes — the worker died, the relay is down — the question is still in the
    // file and the intention is not left half-said.
    ctx.state
        .intentions
        .append_turn(slug, "Vous", text)
        .map_err(|e| Failure {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: e,
        })?;

    let reply = post_catbus_message(&tab, text, ctx)?;
    let raw = ctx
        .state
        .intentions
        .append_turn(slug, "Worker", &reply)
        .map_err(|e| Failure {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: e,
        })?;

    Ok(serde_json::json!({ "reply": reply, "markdown": raw }))
}

fn promote(slug: &str, ctx: &Ctx) -> Result<serde_json::Value, Failure> {
    ctx.state.intentions.promote(slug).map_err(Failure::not_found)?;
    Ok(serde_json::json!({ "slug": slug, "ready": true }))
}

fn launch(slug: &str, ctx: &Ctx) -> Result<serde_json::Value, Failure> {
    let (_, ready) = ctx.state.intentions.path_of(slug).map_err(Failure::not_found)?;
    if !ready {
        return Err(Failure::bad_request(
            "this intention is not in READY-intentions yet; promote it first",
        ));
    }
    let repo = ctx
        .state
        .intentions
        .read(slug)
        .ok()
        .and_then(|raw| front_matter_repo(&raw))
        .unwrap_or_default();
    match spawn_worker(slug, &repo, ctx) {
        Ok(Some(id)) => Ok(serde_json::json!({ "slug": slug, "worker": id })),
        Ok(None) => Err(Failure::bad_request(
            "no worker command configured; set TAB_ATELIER_INTENT_CMD",
        )),
        Err(err) => Err(Failure::upstream(err)),
    }
}

/// The `repo:` line of an intention's front matter.
///
/// Scans only between the `---` fences, so a `repo:` appearing later in the
/// conversation — a PO quoting one — is not mistaken for the field.
fn front_matter_repo(raw: &str) -> Option<String> {
    let mut lines = raw.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    lines
        .take_while(|line| line.trim_end() != "---")
        .find_map(|line| line.strip_prefix("repo:"))
        .map(|value| value.trim().to_string())
}

/// Ask for a worker tab and return its id once it exists.
///
/// `Ok(None)` means no command is configured — not an error, just an intention
/// without an agent. The id is found by name after the request, because
/// `POST /tabs` queues the creation and returns before the tab is made.
///
/// The command is started **exactly as configured**, with nothing appended. The
/// first version of this passed `--intention <path>`; no agent has that flag,
/// and inventing one would make the Kiosk work with exactly the runner it was
/// written against. The intention reaches the worker as its **first message**
/// instead — see [`open_with`] — which any agent can read and which puts the
/// brief in the conversation where the PO can see it.
fn spawn_worker(slug: &str, repo: &str, ctx: &Ctx) -> Result<Option<String>, String> {
    let Some(cmd) = ctx.state.worker_cmd.as_deref() else {
        return Ok(None);
    };
    // Checked before starting anything: an intention that cannot be read is not
    // worth a tab, and the error has to arrive before the side effect.
    ctx.state.intentions.path_of(slug)?;
    let payload = serde_json::json!({
        "cwd": repo,
        "name": State::worker_name(slug),
        "cmd": cmd,
    });
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("{}/tabs", ctx.state.upstream.base))
        .header("Authorization", format!("Bearer {}", ctx.token))
        .header("Content-Type", "application/json")
        .body(payload.to_string().into_bytes())
        .map_err(|e| format!("bad request: {e}"))?;
    self::run(ctx, request)?;

    // The daemon creates the tab on its next tick, so the name is not there
    // yet. A short bounded wait turns that into an id without a polling API;
    // giving up leaves the worker to be found on the next read, which is why
    // the return is optional rather than an error.
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Some(id) = ctx.state.find_worker(slug, &ctx.token)? {
            let _ = ctx.state.intentions.set_tab(slug, &id);
            return Ok(Some(id));
        }
    }
    // The tab was asked for and never appeared. Reported rather than folded
    // into `Ok(None)`: "no command is configured" and "the daemon accepted the
    // request and made nothing" look identical from the pane and have nothing
    // in common, and telling them apart is the difference between checking the
    // environment and checking the daemon. It cost an hour to learn.
    Err(format!(
        "the daemon accepted the tab request but no tab named {:?} appeared within 2s; \
         check that the daemon was built with `cmd`/`name` support in POST /tabs",
        State::worker_name(slug)
    ))
}

/// Hand the worker its intention and return whatever it answers.
///
/// Best-effort by design: the tab exists but the agent inside it may still be
/// starting, so a failure here does not undo the intention. It is reported so
/// the pane can say "the worker did not take the brief" rather than showing a
/// silence with no cause.
fn open_with(slug: &str, tab_id: &str, ctx: &Ctx) -> Result<String, String> {
    let raw = ctx.state.intentions.read(slug)?;
    let briefing = format!(
        "Tu es le worker de diagnostic d'une intention. Voici le fichier :\n\n{raw}\n\n\
         Objectif : circonscrire cette intention — perimetre, contraintes, criteres \
         d'acceptation — en quelques tours. Tu es en LECTURE SEULE : tu ne modifies \
         aucun fichier sauf ce .md, que tu completes au fil de la discussion."
    );
    post_catbus_message(tab_id, &briefing, ctx).map_err(|e| e.message)
}

/// Post a prompt to a tab's agent and return its reply.
fn post_catbus_message(tab_id: &str, text: &str, ctx: &Ctx) -> Result<String, Failure> {
    let payload = serde_json::json!({ "text": text });
    let request = http::Request::builder()
        .method("POST")
        .uri(format!(
            "{}/tabs/by-id/{tab_id}/catbus/message",
            ctx.state.upstream.base
        ))
        .header("Authorization", format!("Bearer {}", ctx.token))
        .header("Content-Type", "application/json")
        .body(payload.to_string().into_bytes())
        .map_err(|e| Failure::upstream(format!("bad request: {e}")))?;
    let raw = run(ctx, request).map_err(Failure::upstream)?;
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| Failure::upstream(format!("daemon reply is not JSON: {e}")))?;
    parsed
        .get("reply")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Failure::upstream("daemon reply carried no `reply` field"))
}

/// Run a blocking request and return its body as text.
fn run(ctx: &Ctx, request: http::Request<Vec<u8>>) -> Result<String, String> {
    let mut response = ctx
        .state
        .upstream
        .agent
        .run(request)
        .map_err(|e| format!("upstream: {e}"))?;
    response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read body: {e}"))
}

/// The largest body an intention request may carry.
pub(crate) const MAX_INTENT_BODY: usize = MAX_REQUEST_BODY;

/// Read the request body, bounded.
///
/// # Errors
/// Fails when the body is unreadable or over the limit.
pub(crate) async fn read_body(body: Incoming) -> Result<Vec<u8>, String> {
    use http_body_util::BodyExt as _;
    let collected = http_body_util::Limited::new(body, MAX_INTENT_BODY)
        .collect()
        .await
        .map_err(|e| {
            if e.is::<http_body_util::LengthLimitError>() {
                "body_too_large".to_string()
            } else {
                format!("unreadable body: {e}")
            }
        })?;
    Ok(collected.to_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::{Call, call_for};
    use hyper::Method;

    /// Each route maps to its operation, and the slug comes out of the path.
    #[test]
    fn the_six_routes_map_to_their_operations() {
        assert_eq!(call_for(&Method::GET, "/intent/list"), Some(Call::List));
        assert_eq!(call_for(&Method::POST, "/intent/new"), Some(Call::New));
        assert_eq!(
            call_for(&Method::GET, "/intent/conges-ete"),
            Some(Call::Get {
                slug: "conges-ete".into()
            })
        );
        assert_eq!(
            call_for(&Method::POST, "/intent/conges-ete/ask"),
            Some(Call::Ask {
                slug: "conges-ete".into()
            })
        );
        assert_eq!(
            call_for(&Method::POST, "/intent/conges-ete/promote"),
            Some(Call::Promote {
                slug: "conges-ete".into()
            })
        );
        assert_eq!(
            call_for(&Method::POST, "/intent/conges-ete/launch"),
            Some(Call::Launch {
                slug: "conges-ete".into()
            })
        );
    }

    /// The wrong method is not a route: a `POST /intent/list` is the daemon's
    /// business, and answering it here would shadow whatever it decides.
    #[test]
    fn the_wrong_method_is_not_an_intention_route() {
        assert_eq!(call_for(&Method::POST, "/intent/list"), None);
        assert_eq!(call_for(&Method::GET, "/intent/new"), None);
        assert_eq!(call_for(&Method::GET, "/intent/x/ask"), None, "ask is a POST");
        assert_eq!(call_for(&Method::PUT, "/intent/x"), None);
    }

    /// A path that is not a route is not ours — it falls through to the proxy
    /// rather than 404ing here.
    #[test]
    fn unknown_paths_are_left_to_the_daemon() {
        for path in ["/decisions", "/intent", "/intent/x/y/z", "/intents", "/intent/x/nope"] {
            assert_eq!(call_for(&Method::GET, path), None, "{path} should not be ours");
            assert_eq!(call_for(&Method::POST, path), None, "{path} should not be ours");
        }
    }

    /// A slug is checked at the route, so a path that cannot name a file never
    /// reaches the file layer.
    #[test]
    fn a_path_that_is_not_a_slug_is_refused() {
        for path in [
            "/intent/../../etc/passwd",
            "/intent/UPPER",
            "/intent/has space",
            "/intent/-leading",
        ] {
            assert_eq!(call_for(&Method::GET, path), None, "{path} must be refused");
        }
    }

    /// The two route names are not slugs, so an intention titled "List" cannot
    /// be created and then be unreachable behind `GET /intent/list`.
    #[test]
    fn the_route_names_are_not_usable_slugs() {
        for name in ["list", "new"] {
            assert!(!super::is_usable_slug(name), "{name} is a route name");
            assert!(
                crate::intent::is_slug(name),
                "{name} is otherwise a well-formed slug, which is why it needs reserving"
            );
        }
        assert!(super::is_usable_slug("liste"));
        assert!(super::is_usable_slug("nouvelle"));
    }
}
