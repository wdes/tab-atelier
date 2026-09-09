// @licence MPL-2.0 https://mozilla.org/MPL/2.0/

//! `tab-atelier gossip` — anti-entropy between hosts' blackboards.
//!
//! One round per peer: pull what they have, push what we have. Both sides are
//! set unions keyed by entry id, so the exchange is idempotent — running it
//! twice, or in both directions at once, or against a peer that has already
//! heard everything, changes nothing and costs one scan.
//!
//! That is the entire cross-host design. There is no leader, no membership
//! agreement, no ordering: the blackboard is a grow-only set, and grow-only
//! sets converge under any pattern of exchange (Demers et al., epidemic
//! algorithms, 1987; Shapiro et al., CRDTs, 2011). Two hosts that gossip once
//! agree; a host that was offline catches up on its next round without anyone
//! tracking that it was away.
//!
//! Claims deliberately do not travel. A lease is host-local mutual exclusion,
//! and honouring a remote host's lease would mean trusting its clock — so a
//! task announced on one host can be taken on either, and the losing agent
//! finds out at `done` time. That is the correct trade for a system whose unit
//! of work is "spend some tokens looking at a file", not "move money".

use super::share_link::agent;
use super::team::{Note, merge_into_blackboard, read_blackboard};

fn usage() {
    eprintln!(
        "usage: tab-atelier gossip [--peer <label-or-id>] [--pull-only] [--quiet]\n\
         Exchange blackboard entries with configured remotes (all of them by default).\n\
         Safe to run on a timer: the merge is a set union, so repeats are free."
    );
}

/// What one exchange with one peer did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Round {
    pub peer: String,
    /// The host name the peer answered with — learned, then remembered, so a
    /// later `take` knows which endpoint speaks for a task's home.
    pub origin: Option<String>,
    /// Entries they had that we didn't.
    pub pulled: usize,
    /// Entries we had that they didn't.
    pub pushed: usize,
    pub error: Option<String>,
}

/// Render one round the way `gossip` prints it.
#[must_use]
pub fn format_round(r: &Round) -> String {
    r.error.as_ref().map_or_else(
        || {
            let who = r.origin.as_ref().map_or_else(String::new, |o| format!(" ({o})"));
            format!("{}{who}: pulled {} pushed {}", r.peer, r.pulled, r.pushed)
        },
        |e| format!("{}: {e}", r.peer),
    )
}

/// Pull a peer's entries, merge them, then offer ours.
///
/// Push happens after the pull so the batch we send already includes anything
/// they just taught us — one round then leaves both sides fully converged
/// rather than needing a second pass.
fn exchange(ep: &crate::RemoteEndpoint, pull_only: bool) -> Round {
    let (label, url, token) = (ep.label.as_str(), ep.url.as_str(), ep.token.as_str());
    let mut round = Round {
        peer: label.to_owned(),
        ..Round::default()
    };
    let theirs: Vec<Note> = match agent()
        .get(format!("{url}/blackboard"))
        .header("Authorization", format!("Bearer {token}"))
        .call()
    {
        Ok(mut resp) => match resp.body_mut().read_to_string() {
            Ok(body) => {
                let doc: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                // The response names the host that answered, which is how the
                // fleet's directory gets built: nobody registers anything, a
                // gossip round is enough to learn who speaks for which name.
                if let Some(origin) = doc.get("origin").and_then(serde_json::Value::as_str) {
                    crate::federation::remember(origin, &ep.id);
                    round.origin = Some(origin.to_owned());
                }
                doc.get("entries")
                    .cloned()
                    .and_then(|e| serde_json::from_value(e).ok())
                    .unwrap_or_default()
            }
            Err(e) => {
                round.error = Some(format!("read: {e}"));
                return round;
            }
        },
        Err(e) => {
            round.error = Some(e.to_string());
            return round;
        }
    };
    match merge_into_blackboard(&theirs) {
        Ok(n) => round.pulled = n,
        Err(e) => {
            round.error = Some(e);
            return round;
        }
    }
    if pull_only {
        return round;
    }
    let ours = read_blackboard();
    match agent()
        .post(format!("{url}/blackboard"))
        .header("Authorization", format!("Bearer {token}"))
        .send_json(serde_json::json!({ "entries": ours }))
    {
        Ok(mut resp) => {
            round.pushed = resp
                .body_mut()
                .read_to_string()
                .ok()
                .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                .and_then(|v| v.get("merged").and_then(serde_json::Value::as_u64))
                .unwrap_or(0) as usize;
        }
        Err(e) => round.error = Some(e.to_string()),
    }
    round
}

/// One exchange with every configured remote, results returned rather than
/// printed — the daemon's periodic sweep calls this, and a background thread
/// should log, not write to stdout.
#[must_use]
pub fn sweep_all() -> Vec<Round> {
    let prefs = crate::load_preferences(&crate::platform::config_dir());
    prefs.remote_endpoints.iter().map(|ep| exchange(ep, false)).collect()
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut peer: Option<String> = None;
    let mut pull_only = false;
    let mut quiet = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--peer" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("gossip: --peer expects a label or id");
                    return 2;
                };
                peer = Some(v.clone());
            }
            "--pull-only" => pull_only = true,
            "--quiet" | "-q" => quiet = true,
            "-h" | "--help" => {
                usage();
                return 0;
            }
            other => {
                eprintln!("gossip: unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let prefs = crate::load_preferences(&crate::platform::config_dir());
    let selected: Vec<&crate::RemoteEndpoint> = prefs
        .remote_endpoints
        .iter()
        .filter(|e| peer.as_ref().is_none_or(|p| &e.label == p || &e.id == p))
        .collect();
    if selected.is_empty() {
        if let Some(p) = peer {
            eprintln!("gossip: no endpoint matching {p:?}");
            return 1;
        }
        println!("(no remotes configured — `tab-atelier remote add …` first)");
        return 0;
    }
    let mut failed = false;
    for ep in selected {
        let round = exchange(ep, pull_only);
        failed |= round.error.is_some();
        if !quiet || round.error.is_some() {
            println!("{}", format_round(&round));
        }
    }
    i32::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_render_counts_or_the_failure() {
        let r = Round {
            peer: "build-box".into(),
            origin: None,
            pulled: 3,
            pushed: 5,
            error: None,
        };
        assert_eq!(format_round(&r), "build-box: pulled 3 pushed 5");
        // Once the peer names itself, say so — the label is ours, the origin
        // is theirs, and a task's home is expressed in theirs.
        let named = Round {
            origin: Some("colossus".into()),
            ..r
        };
        assert_eq!(format_round(&named), "build-box (colossus): pulled 3 pushed 5");
        // A peer that is down must be visible rather than reported as a
        // successful no-op round — silent convergence failure is the thing
        // that makes distributed state mysterious.
        let bad = Round {
            peer: "build-box".into(),
            error: Some("connection refused".into()),
            ..Round::default()
        };
        assert_eq!(format_round(&bad), "build-box: connection refused");
    }

    #[test]
    fn args_are_validated() {
        let argv = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(run(&argv(&["--peer"])), 2);
        assert_eq!(run(&argv(&["--nope"])), 2);
        assert_eq!(run(&argv(&["--help"])), 0);
    }
}
