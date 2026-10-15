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
    /// The catalog tab: `GET /catalog/list`, the skills catalogue.
    Catalog,
    /// A path the Kiosk serves, reached with a method it does not answer. The
    /// string is the value of the `Allow` header.
    MethodNotAllowed(&'static str),
    /// Everything else: hand it to the upstream daemon.
    Proxy,
}

/// Classifies a request. The interface and the panes this crate answers itself
/// — the board, the reports listing, the intention, the catalogue — are ours;
/// every remaining call the page makes (the daemon's own API under `/api/*`,
/// and anything else the page learns to ask for) is the daemon's, relayed on
/// the same origin so the browser never makes a cross-origin request.
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
        (&Method::GET, "/decisions") => Route::Decisions {
            include_archived: query_param(query, "includeArchived").is_some_and(|v| v == "true" || v == "1"),
        },
        (&Method::GET, "/decisions/file") => {
            query_param(query, "path").map_or(Route::Proxy, |path| Route::DecisionFile { path })
        }
        // The reports tab lists the outbox; the intent tab records one.
        (&Method::GET, "/reports") => Route::Reports,
        (&Method::POST, "/intent") => Route::Intent,
        // The catalog tab reads the skills catalogue, which is the Kiosk's to
        // answer: the daemon has no such route (see `crate::panes::catalog`).
        (&Method::GET, "/catalog/list") => Route::Catalog,
        (&Method::POST, _) => decision_action(path),
        _ => Route::Proxy,
    };

    // A path this crate owns, reached with a method it does not answer, is a
    // 405 — NOT a fall-through to the daemon. Relayed, the daemon's own auth
    // gate answers 401 first, which reads as "your token is wrong" for what is
    // really "this method is not allowed here", and names a failure the caller
    // cannot act on.
    if resolved == Route::Proxy
        && let Some(allow) = allowed_methods(path)
    {
        return Route::MethodNotAllowed(allow);
    }
    resolved
}

/// The methods this crate answers for a path it owns, or `None` when the path
/// is not the Kiosk's at all and is therefore the daemon's to answer.
fn allowed_methods(path: &str) -> Option<&'static str> {
    match path {
        "/" | "/kiosk" | "/assets/kiosk.html" | "/assets/kiosk.css" | "/assets/kiosk.js" | "/decisions"
        | "/decisions/file" | "/reports" | "/catalog/list" => Some("GET, HEAD"),
        "/intent" => Some("POST"),
        // `/decisions/<id>/read|tranch` are POSTs; `decision_action` knows the
        // shape, so reuse it rather than repeat the split.
        _ => match decision_action(path) {
            Route::DecisionRead { .. } | Route::DecisionTranch { .. } => Some("POST"),
            _ => None,
        },
    }
}

/// `POST /decisions/<id>/read|tranch`, or `Proxy` for anything else.
fn decision_action(path: &str) -> Route {
    let Some(rest) = path.strip_prefix("/decisions/") else {
        return Route::Proxy;
    };
    let mut segments = rest.split('/');
    let (Some(id), Some(verb), None) = (segments.next(), segments.next(), segments.next()) else {
        return Route::Proxy;
    };
    if id.is_empty() {
        return Route::Proxy;
    }
    match verb {
        "read" => Route::DecisionRead { id: id.to_string() },
        "tranch" => Route::DecisionTranch { id: id.to_string() },
        _ => Route::Proxy,
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
        assert_eq!(classify(Method::POST, "/intent"), Route::Intent);
        assert_eq!(classify(Method::GET, "/catalog/list"), Route::Catalog);

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

        // What is genuinely not the Kiosk's belongs to the daemon, including an
        // unknown verb under a Kiosk path.
        for (method, uri) in [
            (Method::GET, "/api/users"),
            (Method::GET, "/tabs"),
            (Method::POST, "/decisions/a/delete"),
        ] {
            assert_eq!(
                classify(method.clone(), uri),
                Route::Proxy,
                "{method} {uri} is the daemon's"
            );
        }
    }

    /// Percent escapes and `+` decode, so a path with a slash survives the trip.
    #[test]
    fn query_values_are_decoded() {
        assert_eq!(percent_decode("a%2Fb%20c"), "a/b c");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
    }
}
