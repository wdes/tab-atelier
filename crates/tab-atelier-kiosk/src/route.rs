// SPDX-License-Identifier: MPL-2.0

//! Where a request goes: the interface and the Kiosk's own panes are ours, the
//! rest is the daemon's.

use http::{Method, Request};

use crate::assets;

/// What a request resolves to.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// Serve an embedded asset: `(body, content-type)`.
    Asset(&'static str, &'static str),
    /// The board: `GET /decisions`.
    Decisions {
        /// Whether archived decisions are included.
        include_archived: bool,
    },
    /// One file a decision links: `GET /decisions/file?path=…`.
    DecisionFile {
        /// The requested path, percent-decoded.
        path: String,
    },
    /// Mark a decision read: `POST /decisions/<id>/read`.
    DecisionRead {
        /// The decision id.
        id: String,
    },
    /// Rule a decision: `POST /decisions/<id>/tranch`.
    DecisionTranch {
        /// The decision id.
        id: String,
    },
    /// The reports tab: `GET /reports`, the outbox listing.
    Reports,
    /// Record an intention: `POST /intent`.
    Intent,
    /// `GET /intent/list` — the intentions, ready ones last.
    IntentList,
    /// `POST /intent/new` — start an intention.
    IntentNew,
    /// `GET /intent/<slug>` — one intention, whole.
    IntentView {
        /// The intention slug.
        slug: String,
    },
    /// `POST /intent/<slug>/promote` — move it to the ready pool.
    IntentPromote {
        /// The intention slug.
        slug: String,
    },
    /// `POST /intent/<slug>/ask` — append a turn to the intention.
    IntentAsk {
        /// The intention slug.
        slug: String,
    },
    /// The catalog tab: `GET /catalog/list`, the skills catalogue.
    Catalog,
    /// `GET /kiosk/remotes` — the servers the Kiosk aggregates.
    RemotesList,
    /// `POST /kiosk/remotes` — add a server.
    RemoteAdd,
    /// `POST /kiosk/remotes/<id>/remove` — forget a server.
    RemoteRemove {
        /// The remote id.
        id: String,
    },
    /// `GET /kiosk/remotes/<id>/tabs` — a remote's tabs, relayed.
    RemoteTabs {
        /// The remote id.
        id: String,
    },
    /// `POST /kiosk/remotes/<id>/tabs` — open a tab on a remote.
    RemoteTabCreate {
        /// The remote id.
        id: String,
    },
    /// `POST /kiosk/remotes/<id>/tabs/<uuid>/close` — close a tab on a remote.
    RemoteTabClose {
        /// The remote id.
        id: String,
        /// The tab's uuid.
        tab: String,
    },
    /// `POST /kiosk/remotes/<id>/tabs/<uuid>/meta` — set a tab's card labels.
    RemoteTabMeta {
        /// The remote id.
        id: String,
        /// The tab's uuid.
        tab: String,
    },
    /// `GET /kiosk/remotes/<id>/tabs/<uuid>/view` — mint a share token for the
    /// tab and redirect to its terminal viewer, where the operator can type.
    RemoteTabView {
        /// The remote id.
        id: String,
        /// The tab's uuid.
        tab: String,
    },
    /// `POST /kiosk/remotes/<id>/broadcast` — type the same message into every
    /// agent tab on a remote (they publish their card in response).
    RemoteBroadcast {
        /// The remote id.
        id: String,
    },
    /// `POST /kiosk/remotes/<id>/intent/<slug>/launch` — open a worker tab on a
    /// remote for an intention, and remember which tab it was.
    RemoteIntentLaunch {
        /// The remote id.
        id: String,
        /// The intention slug.
        slug: String,
    },
    /// A path the Kiosk serves, reached with a method it does not answer. The
    /// string is the value of the `Allow` header.
    MethodNotAllowed(&'static str),
    /// `GET /kiosk/remotes/<id>/cards` — a remote's agent cards, derived from
    /// the meta labels of its tabs.
    RemoteCards {
        /// The remote id.
        id: String,
    },
    /// `GET /kiosk/fleet` — every server and its tabs, aggregated server-side.
    Fleet,
    /// Nothing the Kiosk serves — a 404. There is no relay: the Kiosk is a
    /// multi-remote aggregator, not a proxy for one daemon.
    NotFound,
}

impl Route {
    /// Whether the route needs the pane token.
    ///
    /// The assets, a 404 and a method error do not; every pane and the remote
    /// API do. The server checks this once, before the match, rather than
    /// repeating the guard in each arm.
    #[must_use]
    pub const fn is_protected(&self) -> bool {
        !matches!(self, Self::Asset(..) | Self::NotFound | Self::MethodNotAllowed(..))
    }
}

/// Classifies a request. The Kiosk answers the interface and its own panes —
/// the board, the reports listing, the intentions, the catalogue, and the
/// multi-server API that reaches the configured remotes. Anything else is a 404:
/// there is no single daemon to relay to.
pub fn route(req: &Request<()>) -> Route {
    let uri = req.uri();
    let path = uri.path();
    let query = uri.query();

    // Only fetches can be an asset, but the panes are read by both GET and POST.
    let resolved = match (req.method(), path) {
        (&Method::GET | &Method::HEAD, "/" | "/kiosk" | "/assets/kiosk.html") => {
            Route::Asset(assets::HTML, "text/html; charset=utf-8")
        }
        (&Method::GET | &Method::HEAD, "/assets/kiosk.css") => Route::Asset(assets::CSS, "text/css; charset=utf-8"),
        (&Method::GET | &Method::HEAD, "/assets/kiosk.js") => {
            Route::Asset(assets::JS, "text/javascript; charset=utf-8")
        }
        (&Method::GET | &Method::HEAD, "/kiosk/manage") => {
            Route::Asset(assets::MANAGE_HTML, "text/html; charset=utf-8")
        }
        (&Method::GET | &Method::HEAD, "/assets/manage.css") => {
            Route::Asset(assets::MANAGE_CSS, "text/css; charset=utf-8")
        }
        (&Method::GET | &Method::HEAD, "/assets/manage.js") => {
            Route::Asset(assets::MANAGE_JS, "text/javascript; charset=utf-8")
        }
        (&Method::GET, "/decisions") => Route::Decisions {
            include_archived: query_param(query, "includeArchived").is_some_and(|v| v == "true" || v == "1"),
        },
        (&Method::GET, "/decisions/file") => {
            query_param(query, "path").map_or(Route::NotFound, |path| Route::DecisionFile { path })
        }
        // The reports tab lists the outbox; the intent tab records and works an
        // intention. The exact arms precede the family guard below, so
        // `/intent/list` and `/intent/new` are never read as slugs.
        (&Method::GET, "/reports") => Route::Reports,
        (&Method::POST, "/intent") => Route::Intent,
        (&Method::GET, "/intent/list") => Route::IntentList,
        (&Method::POST, "/intent/new") => Route::IntentNew,
        (method, p) if p.starts_with("/intent/") => intent_action(method, p),
        // The catalog tab reads the skills catalogue, which is the Kiosk's to
        // answer: the daemon has no such route (see `crate::panes::catalog`).
        (&Method::GET, "/catalog/list") => Route::Catalog,
        // The Kiosk's own multi-server API. Distinct from `/kiosk` (the page):
        // these reach a configured remote with its token, server-side.
        (&Method::GET, "/kiosk/remotes") => Route::RemotesList,
        (&Method::POST, "/kiosk/remotes") => Route::RemoteAdd,
        // The landing page: every server and its tabs, in one call.
        (&Method::GET, "/kiosk/fleet") => Route::Fleet,
        (method, p) if p.starts_with("/kiosk/remotes/") => remotes_action(method, p),
        (&Method::POST, _) => decision_action(path),
        _ => Route::NotFound,
    };

    // A path this crate owns, reached with a method it does not answer, is a
    // 405 — not a 404. The path exists; only the verb is wrong.
    if resolved == Route::NotFound
        && let Some(allow) = allowed_methods(path)
    {
        return Route::MethodNotAllowed(allow);
    }
    resolved
}

/// `GET|POST /intent/<slug>[/<verb>]`, or `NotFound` for anything else.
fn intent_action(method: &Method, path: &str) -> Route {
    let mut segments = path["/intent/".len()..].split('/');
    let (Some(slug), verb) = (segments.next(), segments.next()) else {
        return Route::NotFound;
    };
    if slug.is_empty() {
        return Route::NotFound;
    }
    match (method, verb, segments.next()) {
        (&Method::GET, None, _) => Route::IntentView { slug: slug.to_string() },
        (&Method::POST, Some("promote"), None) => Route::IntentPromote { slug: slug.to_string() },
        (&Method::POST, Some("ask"), None) => Route::IntentAsk { slug: slug.to_string() },
        _ => Route::NotFound,
    }
}

/// The methods this crate answers for a path it owns, or `None` when the path
/// is not the Kiosk's at all and is therefore the daemon's to answer.
fn allowed_methods(path: &str) -> Option<&'static str> {
    match path {
        "/" | "/kiosk" | "/assets/kiosk.html" | "/assets/kiosk.css" | "/assets/kiosk.js" | "/decisions"
        | "/decisions/file" | "/reports" | "/catalog/list" | "/kiosk/manage" | "/assets/manage.css"
        | "/assets/manage.js" | "/intent/list" => Some("GET, HEAD"),
        "/intent" | "/intent/new" => Some("POST"),
        _ if path.starts_with("/intent/") => {
            let mut segments = path["/intent/".len()..].split('/');
            match (segments.next(), segments.next()) {
                (Some(slug), None) if !slug.is_empty() => Some("GET, HEAD"),
                (Some(_), Some("promote" | "ask")) => Some("POST"),
                _ => None,
            }
        }
        "/kiosk/remotes" => Some("GET, POST"),
        "/kiosk/fleet" => Some("GET, HEAD"),
        _ if path.starts_with("/kiosk/remotes/") => {
            let segments: Vec<&str> = path["/kiosk/remotes/".len()..].split('/').collect();
            match segments.as_slice() {
                [id, "remove"] if !id.is_empty() => Some("POST"),
                [id, "tabs"] if !id.is_empty() => Some("GET, POST"),
                [id, "tabs", tab, "close" | "meta"] if !id.is_empty() && !tab.is_empty() => Some("POST"),
                [id, "tabs", tab, "view"] if !id.is_empty() && !tab.is_empty() => Some("GET, HEAD"),
                [id, "broadcast"] if !id.is_empty() => Some("POST"),
                [id, "cards"] if !id.is_empty() => Some("GET, HEAD"),
                [id, "intent", slug, "launch"] if !id.is_empty() && !slug.is_empty() => Some("POST"),
                _ => None,
            }
        }
        // `/decisions/<id>/read|tranch` are POSTs; `decision_action` knows the
        // shape, so reuse it rather than repeat the split.
        _ => match decision_action(path) {
            Route::DecisionRead { .. } | Route::DecisionTranch { .. } => Some("POST"),
            _ => None,
        },
    }
}

/// `GET|POST /kiosk/remotes/<id>/<verb>`, or `NotFound` for anything else.
fn remotes_action(method: &Method, path: &str) -> Route {
    let Some(rest) = path.strip_prefix("/kiosk/remotes/") else {
        return Route::NotFound;
    };
    let segments: Vec<&str> = rest.split('/').collect();
    match (method, segments.as_slice()) {
        (&Method::POST, [id, "remove"]) if !id.is_empty() => Route::RemoteRemove { id: id.to_string() },
        (&Method::GET, [id, "tabs"]) if !id.is_empty() => Route::RemoteTabs { id: id.to_string() },
        (&Method::POST, [id, "tabs"]) if !id.is_empty() => Route::RemoteTabCreate { id: id.to_string() },
        (&Method::POST, [id, "tabs", tab, "close"]) if !id.is_empty() && !tab.is_empty() => Route::RemoteTabClose {
            id: id.to_string(),
            tab: tab.to_string(),
        },
        (&Method::POST, [id, "tabs", tab, "meta"]) if !id.is_empty() && !tab.is_empty() => Route::RemoteTabMeta {
            id: id.to_string(),
            tab: tab.to_string(),
        },
        (&Method::GET, [id, "tabs", tab, "view"]) if !id.is_empty() && !tab.is_empty() => Route::RemoteTabView {
            id: id.to_string(),
            tab: tab.to_string(),
        },
        (&Method::POST, [id, "broadcast"]) if !id.is_empty() => Route::RemoteBroadcast { id: id.to_string() },
        (&Method::GET, [id, "cards"]) if !id.is_empty() => Route::RemoteCards { id: id.to_string() },
        // The one remote action that is about an intention rather than a plain
        // tab: it spawns a worker for `slug` and remembers which tab it was.
        (&Method::POST, [id, "intent", slug, "launch"]) if !id.is_empty() && !slug.is_empty() => {
            Route::RemoteIntentLaunch {
                id: id.to_string(),
                slug: slug.to_string(),
            }
        }
        _ => Route::NotFound,
    }
}

/// `POST /decisions/<id>/read|tranch`, or `NotFound` for anything else.
fn decision_action(path: &str) -> Route {
    let Some(rest) = path.strip_prefix("/decisions/") else {
        return Route::NotFound;
    };
    let mut segments = rest.split('/');
    let (Some(id), Some(verb), None) = (segments.next(), segments.next(), segments.next()) else {
        return Route::NotFound;
    };
    if id.is_empty() {
        return Route::NotFound;
    }
    match verb {
        "read" => Route::DecisionRead { id: id.to_string() },
        "tranch" => Route::DecisionTranch { id: id.to_string() },
        _ => Route::NotFound,
    }
}

/// The value of `key` in a query string, percent-decoded.
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| percent_decode(value))
}

/// Decodes `%XX` escapes and `+`, the encoding a browser puts in a query.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                if let Some(byte) = hex {
                    out.push(byte);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{Route, percent_decode, route};
    use crate::assets;
    use http::{Method, Request};

    fn classify(method: Method, uri: &str) -> Route {
        let req = Request::builder().method(method).uri(uri).body(()).expect("request");
        route(&req)
    }

    /// The classification is the crate: the page and its assets are ours, the
    /// panes are ours, and every other call is the daemon's.
    #[test]
    fn routes_are_classified_locally_or_proxied() {
        assert_eq!(
            classify(Method::GET, "/"),
            Route::Asset(assets::HTML, "text/html; charset=utf-8")
        );
        assert_eq!(
            classify(Method::GET, "/kiosk"),
            Route::Asset(assets::HTML, "text/html; charset=utf-8")
        );
        assert_eq!(
            classify(Method::GET, "/assets/kiosk.js"),
            Route::Asset(assets::JS, "text/javascript; charset=utf-8")
        );

        assert_eq!(
            classify(Method::GET, "/decisions"),
            Route::Decisions {
                include_archived: false
            }
        );
        assert_eq!(
            classify(Method::GET, "/decisions?includeArchived=true"),
            Route::Decisions { include_archived: true }
        );
        assert_eq!(
            classify(Method::GET, "/decisions/file?path=outbox%2Fx.md"),
            Route::DecisionFile {
                path: "outbox/x.md".to_string()
            }
        );
        assert_eq!(
            classify(Method::POST, "/decisions/a/read"),
            Route::DecisionRead { id: "a".to_string() }
        );
        assert_eq!(
            classify(Method::POST, "/decisions/a/tranch"),
            Route::DecisionTranch { id: "a".to_string() }
        );
        assert_eq!(classify(Method::GET, "/reports"), Route::Reports);
        assert_eq!(classify(Method::GET, "/catalog/list"), Route::Catalog);
        assert_eq!(
            classify(Method::GET, "/kiosk/manage"),
            Route::Asset(assets::MANAGE_HTML, "text/html; charset=utf-8")
        );

        // A path the Kiosk owns, reached with the wrong method, is a 405 — not a
        // relay. Relayed, the daemon's auth gate answers a misleading 401 for
        // what is really "this method is not allowed here".
        assert_eq!(classify(Method::GET, "/intent"), Route::MethodNotAllowed("POST"));
        assert_eq!(classify(Method::POST, "/reports"), Route::MethodNotAllowed("GET, HEAD"));
        assert_eq!(
            classify(Method::POST, "/catalog/list"),
            Route::MethodNotAllowed("GET, HEAD")
        );
        assert_eq!(
            classify(Method::GET, "/decisions/a/read"),
            Route::MethodNotAllowed("POST")
        );
        assert_eq!(
            classify(Method::POST, "/kiosk/manage"),
            Route::MethodNotAllowed("GET, HEAD")
        );

        // What is genuinely not the Kiosk's belongs to the daemon, including an
        // unknown verb under a Kiosk path.
        for (method, uri) in [
            (Method::GET, "/api/users"),
            (Method::GET, "/tabs"),
            (Method::POST, "/decisions/a/delete"),
        ] {
            assert_eq!(
                classify(method.clone(), uri),
                Route::NotFound,
                "{method} {uri} is the daemon's"
            );
        }
    }

    /// The intent family and the multi-server API classify as their own routes.
    #[test]
    fn the_intent_and_remote_families_are_classified() {
        assert_eq!(classify(Method::POST, "/intent"), Route::Intent);
        assert_eq!(classify(Method::GET, "/intent/list"), Route::IntentList);
        assert_eq!(classify(Method::POST, "/intent/new"), Route::IntentNew);
        assert_eq!(
            classify(Method::GET, "/intent/my-slug"),
            Route::IntentView {
                slug: "my-slug".to_string()
            }
        );
        assert_eq!(
            classify(Method::POST, "/intent/my-slug/promote"),
            Route::IntentPromote {
                slug: "my-slug".to_string()
            }
        );
        assert_eq!(
            classify(Method::POST, "/intent/my-slug/ask"),
            Route::IntentAsk {
                slug: "my-slug".to_string()
            }
        );
        // The exact arms win over the `<slug>` family: `/intent/list` is a list,
        // not an intention named "list".
        assert_ne!(
            classify(Method::GET, "/intent/list"),
            Route::IntentView {
                slug: "list".to_string()
            }
        );

        // The Kiosk's own multi-server API.
        assert_eq!(classify(Method::GET, "/kiosk/remotes"), Route::RemotesList);
        assert_eq!(classify(Method::POST, "/kiosk/remotes"), Route::RemoteAdd);
        assert_eq!(classify(Method::GET, "/kiosk/fleet"), Route::Fleet);
        assert_eq!(
            classify(Method::POST, "/kiosk/remotes/abc/remove"),
            Route::RemoteRemove { id: "abc".to_string() }
        );
        assert_eq!(
            classify(Method::GET, "/kiosk/remotes/abc/tabs"),
            Route::RemoteTabs { id: "abc".to_string() }
        );
        assert_eq!(
            classify(Method::POST, "/kiosk/remotes/abc/tabs"),
            Route::RemoteTabCreate { id: "abc".to_string() }
        );
        assert_eq!(
            classify(Method::POST, "/kiosk/remotes/abc/tabs/t1/close"),
            Route::RemoteTabClose {
                id: "abc".to_string(),
                tab: "t1".to_string()
            }
        );
        assert_eq!(
            classify(Method::POST, "/kiosk/remotes/abc/tabs/t1/meta"),
            Route::RemoteTabMeta {
                id: "abc".to_string(),
                tab: "t1".to_string()
            }
        );
        assert_eq!(
            classify(Method::GET, "/kiosk/remotes/abc/cards"),
            Route::RemoteCards { id: "abc".to_string() }
        );
        assert_eq!(
            classify(Method::POST, "/kiosk/remotes/abc/intent/my-slug/launch"),
            Route::RemoteIntentLaunch {
                id: "abc".to_string(),
                slug: "my-slug".to_string()
            }
        );
        // A remote path that is not a known shape is the daemon's, not a 405.
        assert_eq!(classify(Method::GET, "/kiosk/remotes/abc/nope"), Route::NotFound);
        assert_eq!(classify(Method::GET, "/kiosk/remotes/abc/intent/x"), Route::NotFound);
    }

    /// Percent escapes and `+` decode, so a path with a slash survives the trip.
    #[test]
    fn query_values_are_decoded() {
        assert_eq!(percent_decode("a%2Fb%20c"), "a/b c");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
    }
}
