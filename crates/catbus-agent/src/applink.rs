// SPDX-License-Identifier: MPL-2.0

//! Tell tab-atelier what this agent is doing — when tab-atelier is what is running it.
//!
//! `catbus-agent` is installed and used on its own, so nothing here may be required. It detects the
//! app instead of depending on it, the way a hook does: the app puts a tab id and a relay address in
//! the environment of every tab it opens, and this posts state to the app's own API if — and only
//! if — that address is local.
//!
//! Two things the app gets from this, and they are the same mechanism:
//!
//! * **the status LEDs.** The app shows a per-tab indicator from `state` and `agentKind`, which is
//!   how a glance tells you whether an agent is thinking or waiting. `catbus` is just another kind.
//! * **the session uuid, so the tab can be resumed.** `sessionId` is what the app stores as the
//!   tab's agent session and hands back through `--resume` when the tab is reopened. Without it, the
//!   app would reopen a catbus tab with a blank session and the conversation would appear lost.
//!   The agent mints its own id, so reporting it is the only way the app can learn it.
//!
//! ## Why loopback is a condition, not a detail
//!
//! The session id is not a secret in the way a token is, but it is not nothing either: it names a
//! transcript, and a transcript is a private conversation. The relay address in the environment can
//! point at a public host — that is the normal configuration — and the app's status endpoint does not
//! exist there. So the check is not "is it reachable" but "is it local": posting a session id to a
//! remote relay would be sending it to a machine that has no business knowing it, and a reachable
//! public host would accept the request and discard it, which is the worst of both.
//!
//! Best-effort throughout. A status report is the least important thing in this process: a failed
//! one is logged at `debug` and nothing else happens, because an app that is not running, busy, or
//! gone must not affect a turn.

use std::time::Duration;

/// How long a status POST may take. Short: this runs while a turn is starting, and the app is on the
/// same machine — anything that takes longer than this is not going to succeed usefully.
const TIMEOUT: Duration = Duration::from_millis(1500);

/// What the agent is doing, as the app's LED vocabulary puts it.
///
/// These three strings are the app's, not this module's invention — the same values the Claude Code
/// hook sends — because the indicator is drawn from a small fixed set and a new one would be ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// A turn is running.
    Thinking,
    /// Idle, waiting for the operator. What a finished turn looks like.
    Waiting,
    /// Gone. Cleared when the process exits, so the tab's indicator does not outlive the agent.
    Idle,
}

impl State {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
        }
    }
}

/// Where to report, if there is an app to report to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The app's own base, e.g. `http://127.0.0.1:7890`. No trailing path.
    pub base: String,
    pub token: String,
    pub tab_id: String,
}

impl Endpoint {
    /// The URL a status post goes to.
    ///
    /// The same route `tab-atelier set-status` uses, so a catbus tab and a claude tab are updated
    /// identically and the app needs nothing new to understand either.
    #[must_use]
    pub fn status_url(&self) -> String {
        format!("{}/tabs/by-id/{}/status", self.base, self.tab_id)
    }

    /// The body, in the shape the app parses. Field names are its own.
    ///
    /// An associated function rather than a method because nothing about the endpoint appears in it —
    /// the tab is identified in the URL, not the body. If a field is ever needed here it becomes a
    /// method again, which is the honest signal that the body and the destination are related.
    #[must_use]
    pub fn body(state: State, label: Option<&str>, session: &str) -> String {
        let mut body = serde_json::Map::new();
        body.insert("state".into(), serde_json::Value::String(state.as_str().to_owned()));
        // `catbus` is just another kind: the app accepts any string, which is how it already shows a
        // claude tab and this one with the same indicator.
        body.insert("agentKind".into(), serde_json::Value::String("catbus".to_owned()));
        // The uuid the app stores and resumes with. Minted by this agent, so this report is the only
        // way the app can ever learn it.
        body.insert("sessionId".into(), serde_json::Value::String(session.to_owned()));
        if let Some(label) = label.filter(|l| !l.is_empty()) {
            body.insert("label".into(), serde_json::Value::String(label.to_owned()));
        }
        serde_json::Value::Object(body).to_string()
    }
}

/// Find the app, if this process is running inside one of its tabs.
///
/// `None` — the ordinary case for a standalone install — when the app has not named itself in the
/// environment, or when the address it gave is not local.
///
/// **The app names itself rather than the address being derived from the relay**, which matters more
/// than it looks. `TAB_ATELIER_API_URL` is the app saying "this is me, talk to me here", and the
/// Claude Code hook reads the same three variables for the same reason. Deriving the base from
/// `CATBUS_RELAY_URL` instead — which an earlier version did — means *any* local address the relay
/// happens to point at receives a session id: a mock relay in a test, a dev server, whatever else is
/// listening on loopback. It sent a status report to a test's mock relay and consumed the reply meant
/// for a turn, which is how the mistake was found.
///
/// The loopback check stays as defence in depth: the address comes from a variable, and a variable can
/// be wrong. Even a mistaken one cannot send a session id off this machine.
#[must_use]
pub fn endpoint() -> Option<Endpoint> {
    let tab_id = std::env::var("_TAB_ID").ok().filter(|id| !id.is_empty())?;
    let api = std::env::var("TAB_ATELIER_API_URL")
        .ok()
        .filter(|url| !url.is_empty())?;
    let token = std::env::var("TAB_ATELIER_API_TOKEN").ok().filter(|t| !t.is_empty())?;
    let base = local_base(&api)?;
    Some(Endpoint { base, token, tab_id })
}

/// The app's origin, if `address` points at a loopback host.
///
/// Any path is stripped: a status post goes to a route on the app itself, so what is wanted is the
/// origin, and an address that carried a path would otherwise produce `…/relay/tabs/by-id/…`.
///
/// Loopback is checked by *parsing* the URL rather than by matching a string, so `localhost`, `::1`
/// and `127.0.0.1` are all recognised and a hostname that merely looks local is not. Nothing is
/// resolved: a DNS name that happens to point at loopback is still a name from somewhere else, and
/// this decides whether to send a session id.
#[must_use]
pub fn local_base(relay: &str) -> Option<String> {
    let url = reqwest::Url::parse(relay).ok()?;
    let host = url.host_str()?;
    // Two tests rather than a name lookup: a literal address is checked as an address, and the one
    // hostname accepted is accepted by name. Nothing is resolved — a DNS name that happens to point at
    // loopback is still a name from somewhere else, and this decides whether to send a session id.
    // `host_str` keeps the brackets around an IPv6 literal, so they come off before it is parsed as
    // an address — `"[::1]".parse::<IpAddr>()` fails where `"::1"` succeeds, and the comment above
    // claims `::1` is recognised. Caught by this module's own test.
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    let is_local =
        host.eq_ignore_ascii_case("localhost") || bare.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if !is_local {
        return None;
    }
    // The original `host` keeps its brackets, because a URL needs them; only the parse wanted them
    // gone.
    let port = url.port().map_or(String::new(), |p| format!(":{p}"));
    Some(format!("{}://{host}{port}", url.scheme()))
}

/// Report what this agent is doing, and forget about it.
///
/// Fire-and-forget by design. A turn must not wait on the app, and a failure here is not worth a
/// warning: an app that is stopped, restarting, or busy is a normal thing to find, and the LED being
/// stale for one turn is the whole of the consequence.
pub async fn report(endpoint: &Endpoint, state: State, label: Option<&str>, session: &str) {
    let client = match reqwest::Client::builder().timeout(TIMEOUT).build() {
        Ok(client) => client,
        Err(e) => {
            log::debug!("no http client for the status report: {e}");
            return;
        }
    };
    let request = client
        .post(endpoint.status_url())
        .header("content-type", "application/json")
        // The same header the app's own CLI sends, since this is the same endpoint.
        .header("x-api-key", &endpoint.token)
        .body(Endpoint::body(state, label, session));
    match request.send().await {
        Ok(response) if response.status().is_success() => {}
        // Not a warning: the app not being there is the ordinary case, and it already knows if it
        // asked for the report. `debug` so a puzzling indicator can still be traced.
        Ok(response) => log::debug!("the app refused a status report: {}", response.status()),
        Err(e) => log::debug!("could not reach the app to report status: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blank tab id is not a tab id, so a standalone run reports nothing.
    ///
    /// The environment itself cannot be set here — `set_var` is unsafe under edition 2024 and this
    /// crate denies `unsafe` — so the two halves are asserted apart: the filter's rule, and the
    /// address check. `endpoint` composes them and adds no other logic.
    #[test]
    fn a_blank_tab_id_is_not_a_tab_id() {
        let blank: Option<String> = Some(String::new()).filter(|id| !id.is_empty());
        assert_eq!(blank, None, "an empty `_TAB_ID` must read as absent");
        // And a real local relay is a candidate, which is what `endpoint` would pair it with.
        assert!(local_base("http://127.0.0.1:7890").is_some());
    }

    /// Only a local address is a candidate, whatever it is called.
    ///
    /// This is the condition that keeps a session id off a remote host. A public relay is the normal
    /// configuration, and posting a session id there would send it to a machine with no business
    /// knowing it — while the app's status endpoint does not exist there, so the request would also be
    /// pointless.
    #[test]
    fn only_a_loopback_relay_is_treated_as_the_app() {
        // Every local spelling.
        for local in [
            "http://127.0.0.1:7890",
            "http://localhost:7890",
            "http://[::1]:7890",
            "https://127.0.0.1:7890",
            // The bare origin, since the path is optional.
            "http://127.0.0.1:7890",
        ] {
            assert!(
                local_base(local).is_some(),
                "`{local}` is local and should be recognised"
            );
        }

        // And nothing that is not. A public host, a name that merely contains "localhost", a LAN
        // address, and something that is not a URL at all.
        for remote in [
            "https://relay.example.org",
            "http://notlocalhost.example.org",
            "http://localhost.evil.example.org",
            "http://192.0.2.10:7890",
            "http://10.1.2.3",
            "not a url",
            "",
        ] {
            assert!(
                local_base(remote).is_none(),
                "`{remote}` is not the app and must not be posted to"
            );
        }
    }

    /// The base is the origin, with the relay path stripped.
    #[test]
    fn the_relay_path_is_stripped_from_the_base() {
        assert_eq!(
            local_base("http://127.0.0.1:7890").as_deref(),
            Some("http://127.0.0.1:7890")
        );
        // A port-less URL keeps no colon, so the built URL is valid.
        assert_eq!(
            local_base("http://127.0.0.1/relay/anthropic").as_deref(),
            Some("http://127.0.0.1")
        );
    }

    /// The URL and body are the shape the app parses, which is the app's own — so this pins them.
    #[test]
    fn the_report_is_the_shape_the_app_reads() {
        let endpoint = Endpoint {
            base: "http://127.0.0.1:7890".to_owned(),
            token: "tok".to_owned(),
            tab_id: "tab-7".to_owned(),
        };
        assert_eq!(
            endpoint.status_url(),
            "http://127.0.0.1:7890/tabs/by-id/tab-7/status",
            "the same route the app's own set-status uses"
        );

        let body: serde_json::Value =
            serde_json::from_str(&Endpoint::body(State::Thinking, Some("Bash"), "sess-1")).expect("JSON");
        assert_eq!(body["state"], "thinking");
        // Any kind is accepted by the app, and `catbus` is the one that names this agent.
        assert_eq!(body["agentKind"], "catbus");
        // **The point of the whole module**: the uuid the app stores and resumes with.
        assert_eq!(body["sessionId"], "sess-1");
        assert_eq!(body["label"], "Bash", "the label is what the tool is");

        // No label when there is nothing worth labelling, rather than an empty one.
        let bare: serde_json::Value =
            serde_json::from_str(&Endpoint::body(State::Waiting, None, "sess-1")).expect("JSON");
        assert_eq!(bare["state"], "waiting");
        assert!(bare.get("label").is_none(), "{bare}");

        // And the three states are the app's own strings, since a new one would be ignored.
        assert_eq!(State::Thinking.as_str(), "thinking");
        assert_eq!(State::Waiting.as_str(), "waiting");
        assert_eq!(State::Idle.as_str(), "idle");
    }

    /// A URL that cannot be parsed is not an app.
    #[test]
    fn an_unparseable_address_is_not_an_app() {
        for bad in ["", "relay.example.org", "://", "http://"] {
            assert!(local_base(bad).is_none(), "`{bad}` must not read as the app");
        }
    }
}
