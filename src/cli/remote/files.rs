// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier remote put` + `tab-atelier remote get` — file
//! transport over the local HTTP API's `/tabs/{idx}/files` routes.
//!
//! `put` uploads a local file's bytes into the remote tab's
//! `inbox/<basename>`. `get` downloads a file (relative to the remote
//! tab's cwd) and writes it locally — defaults to the same basename
//! in the current dir, override with `-o <local-path>`.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use crate::RemoteEndpoint;
use crate::cli::remote::resolver;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn cmd_put(args: super::PutArgs) -> i32 {
    let super::PutArgs {
        endpoint: endpoint_key,
        local_path,
        tab: tab_arg,
        remote_name,
    } = args;

    let endpoint = match resolver::endpoint(&endpoint_key) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("tab-atelier remote put: {e}");
            return 1;
        }
    };
    resolver::warn_if_cert_drifted(&endpoint);

    let bytes = match std::fs::read(&local_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("tab-atelier remote put: read {}: {e}", local_path.display());
            return 1;
        }
    };
    let Some(basename) = local_path.file_name().and_then(|s| s.to_str()).map(str::to_string) else {
        eprintln!(
            "tab-atelier remote put: cannot derive filename from {}",
            local_path.display()
        );
        return 1;
    };
    let remote_name = remote_name.unwrap_or(basename);

    let remote_index = match resolve_tab_index(&endpoint, tab_arg.as_deref()) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("tab-atelier remote put: {e}");
            return 1;
        }
    };

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .user_agent(crate::remote::SIDECAR_USER_AGENT)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .disable_verification(true)
                .build(),
        )
        .build()
        .new_agent();
    let url = format!(
        "{}/tabs/{remote_index}/files?name={}",
        endpoint.url.trim_end_matches('/'),
        url_encode(&remote_name),
    );
    match crate::remote::authorized(agent.post(&url), &endpoint)
        .header("Content-Type", "application/octet-stream")
        .send(&bytes[..])
    {
        Ok(mut resp) => {
            let body: serde_json::Value = resp.body_mut().read_json().unwrap_or_default();
            let path = body.get("path").and_then(serde_json::Value::as_str).unwrap_or("?");
            println!("✓ uploaded {} bytes → {path}", bytes.len());
            0
        }
        Err(e) => {
            eprintln!("tab-atelier remote put: POST /files: {e}");
            1
        }
    }
}

pub fn cmd_get(args: super::GetArgs) -> i32 {
    let super::GetArgs {
        endpoint: endpoint_key,
        remote_path,
        tab: tab_arg,
        local_out,
    } = args;

    let endpoint = match resolver::endpoint(&endpoint_key) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("tab-atelier remote get: {e}");
            return 1;
        }
    };
    resolver::warn_if_cert_drifted(&endpoint);

    let remote_index = match resolve_tab_index(&endpoint, tab_arg.as_deref()) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!("tab-atelier remote get: {e}");
            return 1;
        }
    };

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .user_agent(crate::remote::SIDECAR_USER_AGENT)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .disable_verification(true)
                .build(),
        )
        .build()
        .new_agent();
    let url = format!(
        "{}/tabs/{remote_index}/files?path={}",
        endpoint.url.trim_end_matches('/'),
        url_encode(&remote_path),
    );
    let bytes = match crate::remote::authorized(agent.get(&url), &endpoint).call() {
        Ok(mut resp) => match resp.body_mut().read_to_vec() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("tab-atelier remote get: read body: {e}");
                return 1;
            }
        },
        Err(e) => {
            eprintln!("tab-atelier remote get: GET /files: {e}");
            return 1;
        }
    };

    let dest = local_out.unwrap_or_else(|| {
        let basename = std::path::Path::new(&remote_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("download");
        PathBuf::from(basename)
    });
    if let Err(e) = std::fs::write(&dest, &bytes) {
        eprintln!("tab-atelier remote get: write {}: {e}", dest.display());
        return 1;
    }
    let _ = std::io::stdout().flush();
    println!("✓ downloaded {} bytes → {}", bytes.len(), dest.display());
    0
}

/// Resolve the `--tab` argument (or default to the remote's currently
/// active tab) into a remote-side index. We refresh `/tabs` once via
/// the existing `Client::spawn` plumbing so the index reflects any
/// recent open/close on the remote.
fn resolve_tab_index(endpoint: &RemoteEndpoint, tab_arg: Option<&str>) -> Result<usize, String> {
    let client = crate::remote::Client::spawn(endpoint.clone())
        .ok_or_else(|| "could not start the remote client thread".to_owned())?;
    let tabs = resolver::wait_for_first_tabs(&client, Duration::from_secs(5))?;
    let tab = if let Some(arg) = tab_arg {
        resolver::pick_tab(&tabs, arg)?
    } else {
        tabs.iter()
            .find(|t| t.active_on_remote)
            .ok_or_else(|| "remote has no active tab; pass --tab <name|#idx>".to_string())?
    };
    Ok(tab.remote_index)
}

/// Minimal application/x-www-form-urlencoded encoder for query
/// values. The relay calls `url_decode` on the receiving end.
fn url_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{RemoteEndpoint, resolve_tab_index, url_encode};

    /// A server that answers every request with `body`.
    ///
    /// `resolve_tab_index` spawns the real sidecar client, which polls — so a
    /// one-shot responder deadlocks it. The thread is detached; it dies with
    /// the test process.
    fn serve_tabs(body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = write!(
                    sock,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn endpoint(url: String) -> RemoteEndpoint {
        RemoteEndpoint {
            id: "ep".into(),
            label: "peer".into(),
            url,
            token: "t".into(),
            relay_token: String::new(),
            cert_sha256: String::new(),
            cf_access_client_id: String::new(),
            cf_access_client_secret: String::new(),
            autoconnect: false,
        }
    }

    #[test]
    fn a_filename_survives_the_url_intact() {
        assert_eq!(url_encode("report.md"), "report.md");
        // A space or a slash in a name must not split the path or silently
        // land the file somewhere else on the remote.
        assert_eq!(url_encode("my report.md"), "my%20report.md");
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode("q?x=1&y=2"), "q%3Fx%3D1%26y%3D2");
        assert_eq!(url_encode("100%"), "100%25");
        // Unreserved characters stay readable — an encoder that escapes
        // everything makes every log line unreadable for no gain.
        assert_eq!(url_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(url_encode(""), "");
    }

    #[test]
    fn a_tab_argument_resolves_against_the_remotes_list() {
        let body = r#"{"tabs":[
            {"id":"uuid-a","index":0,"name":"build","active":false},
            {"id":"uuid-b","index":1,"name":"deploy","active":true}
        ]}"#;
        let url = serve_tabs(body);
        // No argument: whichever tab the remote says is active — NOT index 0.
        assert_eq!(resolve_tab_index(&endpoint(url.clone()), None).ok(), Some(1));
        assert_eq!(resolve_tab_index(&endpoint(url.clone()), Some("build")).ok(), Some(0));
        // An index needs the `#` sigil (`--tab '#1'`). A bare number is
        // treated as a NAME, so `--tab 1` looks for a tab called "1" and
        // fails — which is safer than silently using position 1, but is easy
        // to get wrong from the shell, where `#` also starts a comment.
        assert_eq!(resolve_tab_index(&endpoint(url.clone()), Some("#1")).ok(), Some(1));
        assert!(resolve_tab_index(&endpoint(url.clone()), Some("1")).is_err());
        assert_eq!(resolve_tab_index(&endpoint(url.clone()), Some("uuid-a")).ok(), Some(0));
        // A name nothing matches is an error, not a default — putting a file
        // in the wrong tab is worse than not putting it at all.
        assert!(resolve_tab_index(&endpoint(url), Some("ghost")).is_err());
        // With no active tab and no argument, say so rather than guessing.
        let none_active = serve_tabs(r#"{"tabs":[{"id":"a","index":0,"name":"x","active":false}]}"#);
        assert!(resolve_tab_index(&endpoint(none_active), None).is_err());
    }

    #[test]
    fn an_unreachable_remote_is_an_error_not_tab_zero() {
        // Nothing listens on port 1. The failure must surface here rather
        // than becoming a default index that uploads to a stranger's tab.
        assert!(resolve_tab_index(&endpoint("http://127.0.0.1:1".into()), None).is_err());
    }

    #[test]
    fn put_and_get_refuse_incomplete_commands() {
        // Driven through the real `remote` parser, so these assert what a
        // person typing the command actually gets. clap rejects all of them
        // before any network or filesystem work, which is what makes them
        // safe to assert on.
        let run = |v: &[&str]| super::super::run(&v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
        assert_eq!(run(&["put"]), 2, "put with no arguments");
        assert_eq!(run(&["get"]), 2, "get with no arguments");
        assert_eq!(run(&["put", "peer"]), 2, "put with no local path");
        // A flag with no value must not swallow the next argument.
        assert_eq!(run(&["put", "peer", "f", "--tab"]), 2);
        // Unknown flags are caught rather than ignored.
        assert_eq!(run(&["get", "peer", "inbox/x", "--nope"]), 2);
        // `--help` is a success everywhere, not "unknown argument".
        assert_eq!(run(&["put", "--help"]), 0);
        assert_eq!(run(&["get", "--help"]), 0);
        // A complete command still fails, on the unknown endpoint rather than
        // on parsing — proof the arguments got through.
        assert_eq!(
            run(&["put", "no-such-endpoint", "/nonexistent/definitely-not-here.txt"]),
            1
        );
    }
}
