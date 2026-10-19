// SPDX-License-Identifier: MPL-2.0

//! The API document, derived from the routes and served, not written by hand.
//!
//! A hand-written spec drifts: someone adds a route and forgets the document,
//! and the two disagree until a client is surprised. This one is derived from
//! the routes themselves — okapi reads each route's own `#[get]`/`#[post]`, its
//! request guards and its return type — so the paths, the methods, the request
//! bodies and the responses cannot be wrong about what the code does.
//!
//! That leaves one way to lie: a route that is *not* annotated, which simply
//! does not appear. [`assert_covers`] is the test that closes it — every mounted
//! route must either be documented or be named in an allow-list with a reason,
//! and a documented operation must correspond to a mounted route. The allow-list
//! may only shrink: an entry that no longer matches anything is itself a
//! failure, so an exemption cannot be left behind.

use rocket::http::Method;
use rocket_okapi::handlers::OpenApiHandler;
use rocket_okapi::okapi;

/// The document, and the routes that serve it.
///
/// Returned together so an app mounts what it also serves — "the document
/// exists" and "the document is reachable" cannot drift apart.
pub struct Mounted {
    /// The `/openapi.json` route to add to the app.
    pub routes: Vec<rocket::Route>,
    /// The derived document, for the enforcement test.
    pub spec: okapi::openapi3::OpenApi,
}

/// Build the document from the app's own routes, and the route that serves it.
///
/// The one call an app makes. It takes what `openapi_get_routes_spec!` produced
/// — the routes and the document derived from them — pins the server URL, and
/// appends the `/openapi.json` endpoint.
///
/// # Panics
///
/// If the handler cannot be turned into a route, which is a bug in this crate
/// rather than in the caller.
#[must_use]
pub fn mount(handlers: (Vec<rocket::Route>, okapi::openapi3::OpenApi), base: &str) -> Mounted {
    let (mut routes, mut spec) = handlers;
    // okapi inserts a `servers` entry from the mount base when the document has
    // none; setting it here decides the server URLs in one place rather than by
    // wherever the app happens to mount.
    if spec.servers.is_empty() {
        spec.servers.push(okapi::openapi3::Server {
            url: base.to_owned(),
            ..okapi::openapi3::Server::default()
        });
    }
    routes.push(OpenApiHandler::new(spec.clone()).into_route("/openapi.json"));
    Mounted { routes, spec }
}

/// Re-export so an app writes one import for the derivation.
pub use rocket_okapi::openapi_get_routes_spec as routes_spec;

/// A route that is deliberately not documented, and why.
#[derive(Debug, Clone, Copy)]
pub struct Exempt {
    /// The method.
    pub method: Method,
    /// The path, in Rocket's `<x>`/`<x..>` form.
    pub path: &'static str,
    /// Why it is exempt — the reason a reader will judge.
    pub reason: &'static str,
}

/// Assert the document and the mounted routes are the same set.
///
/// The whole point of the crate's `OpenAPI` story: a route added without an
/// annotation fails here, and so does a documented operation with no route. The
/// exceptions are spelled out one by one, with a reason, and the list may only
/// shrink — an exception that matches nothing is a failure.
///
/// # Panics
///
/// With a list of the routes that broke a rule, so the fix is named rather than
/// guessed.
pub fn assert_covers(rocket: &rocket::Rocket<rocket::Build>, spec: &okapi::openapi3::OpenApi, exempt: &[Exempt]) {
    // The mounted (method, path) pairs, normalised the way okapi normalises a
    // route into a document path: Rocket's `<x>`/`<x..>` become `{x}`.
    //
    // `route.uri.path()` is already the canonical mounted path — the origin,
    // which is the base and the route concatenated. Prefixing `route.uri.base()`
    // as well double-counts the mount point, and a root mount (`"/"`) shows it
    // as a leading `//` that matches no document path.
    assert_covers_routes(
        rocket.routes().map(|route| {
            (
                route.method.to_string().to_ascii_uppercase(),
                route.uri.path().to_owned(),
            )
        }),
        spec,
        exempt,
    );
}

/// The same rule for a server that does not mount through Rocket.
///
/// The daemon keeps its own listener — it needs the WebSocket upgrade, the SSE
/// relay and HTTP/3, none of which fit a Rocket route — so it cannot hand this
/// function a `Rocket<Build>`. It can hand it the routes it serves. Taking the
/// pairs rather than the framework is what lets one rule police every server in
/// the workspace instead of only the two that moved.
///
/// Paths are given in either notation: `<x>`/`<x..>` from Rocket, or `{x}` from
/// a server that already writes them that way. Both normalise to `{x}`, which is
/// what `okapi` puts in the document.
///
/// # Panics
///
/// With a list of the routes that broke a rule, so the fix is named rather than
/// guessed.
pub fn assert_covers_routes(
    mounted_routes: impl Iterator<Item = (String, String)>,
    spec: &okapi::openapi3::OpenApi,
    exempt: &[Exempt],
) {
    use std::collections::BTreeSet;

    let mounted: BTreeSet<(String, String)> = mounted_routes
        .map(|(method, path)| (method.to_ascii_uppercase(), normalise(&path)))
        .collect();

    // What the document says it offers. `PathItem` holds one optional operation
    // per verb rather than a map, so the verbs are walked by hand.
    let mut documented: BTreeSet<(String, String)> = BTreeSet::new();
    for (path, item) in &spec.paths {
        for (method, op) in [
            ("GET", &item.get),
            ("PUT", &item.put),
            ("POST", &item.post),
            ("DELETE", &item.delete),
            ("OPTIONS", &item.options),
            ("HEAD", &item.head),
            ("PATCH", &item.patch),
            ("TRACE", &item.trace),
        ] {
            if op.is_some() {
                documented.insert((method.to_owned(), normalise(path)));
            }
        }
    }

    let exempt_set: BTreeSet<(String, String)> = exempt
        .iter()
        .map(|e| (e.method.to_string().to_ascii_uppercase(), normalise(e.path)))
        .collect();

    // 1. Every mounted route is documented or exempt.
    let undocumented: Vec<String> = mounted
        .difference(&documented)
        .filter(|pair| !exempt_set.contains(*pair))
        .map(|(m, p)| format!("{m} {p}"))
        .collect();
    assert!(
        undocumented.is_empty(),
        "these routes are mounted but not in the API document, and not exempt:\n  {}\n\
         Add `#[openapi(tag = \"…\")]` above the route, or an Exempt entry with a reason.",
        undocumented.join("\n  ")
    );

    // 2. Every documented operation corresponds to a mounted route.
    let stale: Vec<String> = documented
        .difference(&mounted)
        .map(|(m, p)| format!("{m} {p}"))
        .collect();
    assert!(
        stale.is_empty(),
        "the document promises routes that are not mounted:\n  {}",
        stale.join("\n  ")
    );

    // 3. Every exemption still matches something — so the list only shrinks.
    let dead: Vec<String> = exempt_set
        .iter()
        .filter(|pair| !mounted.contains(*pair))
        .map(|(m, p)| format!("{m} {p}"))
        .collect();
    assert!(
        dead.is_empty(),
        "these exemptions match no mounted route any more — remove them:\n  {}",
        dead.join("\n  ")
    );
}

/// `<x>` and `<x..>` become `{x}`, the form okapi writes into the document.
fn normalise(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                out.push('{');
                for c in chars.by_ref() {
                    if c == '>' {
                        break;
                    }
                    // A catch-all's `..` is not part of the name.
                    if c != '.' {
                        out.push(c);
                    }
                }
                out.push('}');
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::normalise;

    #[test]
    fn a_rocket_param_becomes_a_document_param() {
        assert_eq!(normalise("/tabs/by-id/<uuid>/view"), "/tabs/by-id/{uuid}/view");
    }

    #[test]
    fn a_catch_all_drops_its_dots() {
        assert_eq!(normalise("/assets/<rel..>"), "/assets/{rel}");
    }

    #[test]
    fn a_plain_path_is_untouched() {
        assert_eq!(normalise("/api/hello"), "/api/hello");
        assert_eq!(normalise("/"), "/");
    }

    #[test]
    fn two_params_in_one_path_both_convert() {
        assert_eq!(normalise("/a/<x>/b/<y>"), "/a/{x}/b/{y}");
    }
}
