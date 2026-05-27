// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers shared by the `attach` / `put` / `get` subcommands —
//! endpoint lookup, tab-name resolution against `/tabs`, and a small
//! pre-flight fingerprint-drift check.

use std::time::Duration;

use crate::{RemoteEndpoint, fetch_cert_fingerprint, load_preferences, platform};

/// Find a configured endpoint by label or UUID.
pub fn endpoint(key: &str) -> Result<RemoteEndpoint, String> {
    let prefs = load_preferences(&platform::config_dir());
    prefs
        .remote_endpoints
        .into_iter()
        .find(|e| e.label.eq_ignore_ascii_case(key) || e.id == *key)
        .ok_or_else(|| format!("no endpoint matched {key:?}"))
}

/// Block on `Client::spawn(endpoint).rx` until either the first
/// `Tabs` event arrives (success) or we time out.
#[allow(clippy::match_same_arms)]
pub fn wait_for_first_tabs(
    client: &crate::remote::Client,
    timeout: Duration,
) -> Result<Vec<crate::remote::RemoteTabSnapshot>, String> {
    use crate::remote::RemoteEvent;
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if crate::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("interrupted".into());
        }
        match client.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(RemoteEvent::Tabs { tabs, .. }) => return Ok(tabs),
            Ok(RemoteEvent::Error { message }) => return Err(message),
            Ok(RemoteEvent::Output { .. }) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("client thread disconnected".into());
            }
        }
    }
    Err("timed out waiting for /tabs".into())
}

/// Resolve a tab argument (`#3`, a UUID, a name) against the remote's
/// current `/tabs` list. Returns the matching `remote_id` so callers
/// don't depend on indices that drift across reconciles.
pub fn pick_tab<'a>(
    tabs: &'a [crate::remote::RemoteTabSnapshot],
    arg: &str,
) -> Result<&'a crate::remote::RemoteTabSnapshot, String> {
    if let Some(rest) = arg.strip_prefix('#')
        && let Ok(idx) = rest.parse::<usize>()
    {
        return tabs
            .iter()
            .find(|t| t.remote_index == idx)
            .ok_or_else(|| format!("no tab at index #{idx}"));
    }
    let by_id = tabs.iter().find(|t| t.remote_id == arg);
    if let Some(t) = by_id {
        return Ok(t);
    }
    let matches: Vec<_> = tabs.iter().filter(|t| t.name.eq_ignore_ascii_case(arg)).collect();
    match matches.len() {
        0 => Err(format!(
            "no tab matched {arg:?} — available: {}",
            tabs.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
        1 => Ok(matches[0]),
        _ => Err(format!(
            "ambiguous tab name {arg:?} — {} candidates; disambiguate with #<index> or the UUID",
            matches.len()
        )),
    }
}

/// Warn (but don't fail) when the live cert fingerprint no longer
/// matches what's pinned. Lets the user opt into a `remote re-pin`
/// without losing the connection — this is a heads-up, not an
/// enforcement (Phase 2 TLS still uses `disable_verification`).
pub fn warn_if_cert_drifted(endpoint: &RemoteEndpoint) {
    if !endpoint.url.starts_with("https://") || endpoint.cert_sha256.is_empty() {
        return;
    }
    let Ok(live) = fetch_cert_fingerprint(&endpoint.url) else {
        // Network can't reach the endpoint yet — leave it to the
        // polling thread to surface the connection error.
        return;
    };
    if !live.eq_ignore_ascii_case(&endpoint.cert_sha256) {
        eprintln!("⚠ pinned cert fingerprint for {} has changed.", endpoint.label);
        eprintln!("  pinned: {}", endpoint.cert_sha256);
        eprintln!("  live:   {live}");
        eprintln!(
            "  run `tab-atelier remote re-pin {}` to accept the new cert.",
            endpoint.label
        );
    }
}
