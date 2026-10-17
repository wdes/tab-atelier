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

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use crate::users::Store;
use crate::{account, inspect, provider, qos, usage};

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

impl State {
    /// A state with nothing configured, for tests that exercise a guard rather
    /// than the machinery behind it.
    ///
    /// Every store starts empty and no provider is registered, which is what
    /// makes this useful: a test that passes here is testing its own rule and
    /// not a fixture. The three stores that only load from disk are pointed at
    /// a path under `target/` that does not exist, which is how the server
    /// itself starts on a fresh installation.
    ///
    /// # Panics
    ///
    /// If the user store cannot be read. Under `target/`, beside the manifest,
    /// a missing file is a fresh store, so a panic here means a broken checkout
    /// rather than anything the test did.
    #[cfg(test)]
    #[must_use]
    pub fn for_tests(admin_token: String) -> Self {
        // A directory per call, not one shared by every test. The store is
        // persisted on every write, so tests running in parallel against one
        // file clobber each other's accounts and the failure looks like a bug
        // in the code under test. The integration tests already take a scratch
        // directory each for the same reason; this brings the unit tests in
        // line with them.
        // The system temp directory, not `target/`: this is scratch state that
        // is written on every store mutation, and a path inside the repository
        // shows up as untracked noise in `git status` on every test run.
        let dir = std::env::temp_dir().join("tab-atelier-tests").join(format!(
            "{:?}-{}",
            std::thread::current().id(),
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        Self {
            store: Mutex::new(Store::load(dir.join("users.json")).expect("fresh store")),
            usage: Mutex::new(usage::Store::load(&dir)),
            sched: Mutex::new(qos::Sched::new()),
            account: Mutex::new(account::Monitor::load(&dir)),
            registry: Mutex::new(provider::Registry::default()),
            registry_path: dir.join("providers.json"),
            provider_backoff: Mutex::new(std::collections::BTreeMap::new()),
            wake: tokio::sync::Notify::new(),
            inspect: Mutex::new(inspect::Store::load(&dir)),
            admin_token,
            web_root: None,
        }
    }
}

#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Start the proxy, and serve until the process ends.
///
/// The socket belongs to Rocket now. That is the point of the change: the
/// accept loop, the per-connection task, the header-read timeout and the HTTP
/// version are the framework's business, and each one used to be a hand-written
/// line here that no test covered. What stays in this module is the two things
/// that are genuinely ours — the state the routes read and the reply type they
/// return.
///
/// Build the proxy's Rocket: the routes, the document derived from them, the
/// catchers, and the state they read.
///
/// One definition of the app, shared by [`serve`] and [`serve_on`]: a route
/// mounted in one and not the other would be a difference no test could see.
fn build(addr: SocketAddr, state: Arc<State>) -> rocket::Rocket<rocket::Build> {
    let mounted = tab_atelier_api::mount(crate::http::routes::all_and_spec(), "/");
    let rocket = rocket::custom(tab_atelier_api::config(addr, crate::http::body::MAX_BODY_BYTES as u64))
        .mount("/", mounted.routes)
        .register("/", crate::http::routes::catchers())
        .manage(state);
    // Every route is documented or exempt, checked on every build — a route
    // added without either panics here rather than shipping an API whose
    // document has quietly stopped matching it. This is the whole reason the
    // document is derived: the check is the point, not the JSON.
    tab_atelier_api::assert_covers(&rocket, &mounted.spec, &crate::http::routes::exempt());
    rocket
}

/// # Errors
/// If the address cannot be bound — a port already in use, or one below 1024
/// without the privilege for it.
pub async fn serve(addr: SocketAddr, state: Arc<State>) -> Result<(), String> {
    let rocket = build(addr, state);
    rocket.launch().await.map(|_| ()).map_err(|e| format!("rocket: {e}"))
}

/// Serve on a port the kernel picks, for the tests that drive the real socket.
///
/// Returns the address it actually got, which is what makes a test able to
/// reach a socket on a shared machine without guessing at a free port.
///
/// # Errors
/// If the address cannot be bound, and from Rocket's own launch if it fails
/// after binding.
pub async fn serve_on(addr: SocketAddr, state: Arc<State>) -> Result<SocketAddr, String> {
    // Rocket binds inside `launch`, which does not return until the server
    // stops, so a caller asking for port 0 has no way to learn what it got.
    // Asking the kernel first and handing Rocket the answer closes that: there
    // is a window between the probe and the bind in which another process could
    // take the port, which is why this is for tests and not for `serve`.
    let addr = if addr.port() == 0 {
        let probe = std::net::TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
        let chosen = probe.local_addr().map_err(|e| format!("local_addr: {e}"))?;
        drop(probe);
        chosen
    } else {
        addr
    };
    log::info!("tab-atelier-proxy listening on http://{addr}");
    let rocket = build(addr, state);
    rocket.launch().await.map(|_| addr).map_err(|e| format!("rocket: {e}"))
}

#[cfg(test)]
mod tests {
    //! The socket-level tests live beside the handlers they drive; this module
    //! keeps the two checks that belong to the app as a whole — that every route
    //! is mounted, and that the document derived from them is served.

    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use rocket::http::Status;
    use rocket::local::blocking::Client;

    use super::State;

    fn app() -> Client {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        Client::tracked(super::build(addr, Arc::new(State::for_tests("t".to_owned())))).expect("a valid rocket")
    }

    /// Every route is mounted and covered.
    ///
    /// `build` already asserts coverage — a route added without a document path
    /// or an exempt entry panics there — so this drives that build and pins that
    /// the routes really are mounted, not merely listed.
    #[test]
    fn every_route_is_mounted_and_covered() {
        let client = app();
        assert!(
            client.rocket().routes().count() >= 33,
            "the documented and exempt routes are all mounted"
        );
    }

    /// How many responses are still untyped, pinned so the number can only fall.
    ///
    /// A route that answers a body nobody named — the relay passing an upstream
    /// answer through — shows up as an `x-dynamic-body` in the document. The
    /// count started at 31 (rung one: every route a bare `Reply`) and every
    /// payload that can be typed has been; the **6** that remain are the relay
    /// (`relay_post`, `relay_get`, `relay_hello`, `relay_hello_head`,
    /// `hello_get`, `hello_head`), whose body is genuinely the upstream's and so
    /// has no schema to write. That is the floor, not a backlog: a route added
    /// here that answers a `Reply` it *could* have typed raises this number and
    /// fails the test. Only re-pin downward, and only when a payload is typed.
    #[test]
    fn untyped_responses_do_not_grow() {
        let client = app();
        let body = client.get("/openapi.json").dispatch().into_string().expect("a body");
        let count = body.matches("x-dynamic-body").count();
        assert!(
            count <= 6,
            "x-dynamic-body responses: {count} (the floor is the 6 relay routes; was 31)"
        );
    }

    /// A guard's own refusal reaches the client — the bug the shared catchers
    /// fixed.
    ///
    /// The proxy used to answer a 401 with a fixed sentence, so the guard's
    /// careful "nothing arrived in Authorization: Bearer or x-api-key, which
    /// usually means something in front of the proxy stripped it" was thrown
    /// away and the operator had a status and no lead. The guard now leaves its
    /// message in the request and the catcher returns it; this asserts the
    /// wording, not merely the status, because the wording is the point.
    #[test]
    fn a_guard_refusal_says_what_was_actually_wrong() {
        let client = app();
        let response = client.get("/api/users").dispatch();
        assert_eq!(response.status(), Status::Unauthorized);
        let body = response.into_string().expect("a body");
        assert!(
            body.contains("no credential presented"),
            "the guard's own reason survives to the client: {body}"
        );
        assert!(
            body.contains("Authorization: Bearer"),
            "and it names what to look at: {body}"
        );
    }

    /// A different refusal produces a different body.
    ///
    /// The first test alone would pass if the catcher returned one fixed
    /// sentence that happened to answer the empty case. This drives a second
    /// guard outcome — a credential that arrived and is simply wrong — and the
    /// message has to change with it.
    #[test]
    fn each_refusal_carries_its_own_reason() {
        let client = app();
        let response = client
            .get("/api/users")
            .header(rocket::http::Header::new("authorization", "Bearer not-the-token"))
            .dispatch();
        assert_eq!(response.status(), Status::Unauthorized);
        let body = response.into_string().expect("a body");
        assert!(
            body.contains("token mismatch"),
            "a wrong credential is its own message: {body}"
        );
        assert!(
            !body.contains("no credential presented"),
            "and not the empty-credential one: {body}"
        );
    }

    /// The document is reachable, and it is the derived, well-formed one.
    ///
    /// It parses as JSON, names the routes, and carries the component schemas
    /// the resources generated — so an empty document, a hand-written stub, or
    /// a derivation that silently dropped its types would all fail. That is the
    /// whole claim: the spec comes from the routes and the structs, and cannot
    /// lag either.
    #[test]
    fn the_openapi_document_is_served() {
        let client = app();
        let response = client.get("/openapi.json").dispatch();
        assert_eq!(response.status(), Status::Ok);
        let body = response.into_string().expect("a body");
        let document: serde_json::Value = serde_json::from_str(&body).expect("the document is JSON");
        assert_eq!(document["openapi"], "3.0.0", "the version okapi writes");
        assert!(
            document["paths"]["/api/users"]["get"].is_object(),
            "the document names a real route, with its methods"
        );
        // A converted route answers a named resource, so the schema it came from
        // is in the components — proof the derivation reached the structs, not
        // just the paths.
        assert!(
            document["components"]["schemas"]["AccountsResource"].is_object(),
            "a typed response's schema is in the document"
        );
        // And a refusal is documented as the `default` response, not lost.
        assert!(
            document["components"]["schemas"]["ProblemResource"].is_object(),
            "the refusal shape is in the document"
        );
    }
}
