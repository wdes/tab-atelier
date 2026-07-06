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

pub fn cmd_put(args: &[String]) -> i32 {
    let mut positional: Vec<&String> = Vec::new();
    let mut tab_arg: Option<String> = None;
    let mut remote_name: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tab" => {
                i += 1;
                tab_arg = args.get(i).cloned();
            }
            "--remote-name" | "--remote-path" => {
                i += 1;
                remote_name = args.get(i).cloned();
            }
            other if other.starts_with("--") => {
                eprintln!("tab-atelier remote put: unknown argument: {other}");
                return 2;
            }
            _ => positional.push(&args[i]),
        }
        i += 1;
    }
    if positional.len() != 2 {
        eprintln!("usage: tab-atelier remote put <label-or-id> <local-path> [--tab T] [--remote-name N]");
        return 2;
    }
    let endpoint_key = positional[0].clone();
    let local_path = PathBuf::from(positional[1]);

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
    match agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", endpoint.token))
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

pub fn cmd_get(args: &[String]) -> i32 {
    let mut positional: Vec<&String> = Vec::new();
    let mut tab_arg: Option<String> = None;
    let mut local_out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tab" => {
                i += 1;
                tab_arg = args.get(i).cloned();
            }
            "-o" | "--output" => {
                i += 1;
                local_out = args.get(i).map(PathBuf::from);
            }
            other if other.starts_with("--") || other.starts_with('-') && other.len() == 2 => {
                eprintln!("tab-atelier remote get: unknown argument: {other}");
                return 2;
            }
            _ => positional.push(&args[i]),
        }
        i += 1;
    }
    if positional.len() != 2 {
        eprintln!("usage: tab-atelier remote get <label-or-id> <remote-path> [--tab T] [-o local-path]");
        return 2;
    }
    let endpoint_key = positional[0].clone();
    let remote_path = positional[1].clone();

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
    let bytes = match agent
        .get(&url)
        .header("Authorization", &format!("Bearer {}", endpoint.token))
        .call()
    {
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
