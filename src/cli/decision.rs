// SPDX-License-Identifier: MPL-2.0

//! `tab-atelier decision` — put a question in front of a person, and read what is waiting.
//!
//! The Kiosk renders the decision journal, but the thing that *needs* a decision is usually a build
//! hook, a dispatcher or an agent that has no browser open. This is how such a producer reaches the
//! board: `decision open` appends, `decision list` reads back what is still waiting.
//!
//! Appending is idempotent by id. A cron, a crashed hook or a retry after a timeout will post the
//! same decision more than once, and the verb reports which of the four things happened rather than
//! a bare "ok" — see `Appended` in `tab-atelier-decisions`.

use std::io::Read as _;

/// `tab-atelier decision <open|list>`.
#[derive(clap::Parser, Debug)]
#[command(
    name = "tab-atelier decision",
    about = "Raise a decision for a person, or read the ones waiting",
    long_about = "Raise a decision for a person to rule on, or list the ones already waiting. \
                  Appending is idempotent by id: re-posting the same decision changes nothing, and \
                  re-posting an id that has already been ruled leaves the ruling alone.",
    after_help = "Examples:\n  \
                  tab-atelier decision open --id deploy-42 --title \"Ship 0.5.0?\" --why \"tag is ready\"\n  \
                  echo '{\"id\":\"deploy-42\",\"state\":\"open\",\"title\":\"Ship 0.5.0?\"}' | tab-atelier decision open -\n  \
                  tab-atelier decision list\n  \
                  tab-atelier decision list --all"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Verb,
}

#[derive(clap::Subcommand, Debug)]
enum Verb {
    /// Put a decision on the board (idempotent by id).
    Open {
        /// Its identity. Re-posting the same id is how a producer updates its own open question.
        #[arg(long)]
        id: String,
        /// The one-line question a person reads first.
        #[arg(long)]
        title: Option<String>,
        /// Why this is gated on a person at all.
        #[arg(long)]
        why: Option<String>,
        /// The project it belongs to.
        #[arg(long)]
        project: Option<String>,
        /// A suggestion the reader can accept.
        #[arg(long)]
        reco: Option<String>,
        /// Read the whole decision as JSON from this path, or `-` for stdin.
        ///
        /// For a producer that already builds the object: it passes everything in one document
        /// rather than mapping each field onto a flag, and the daemon validates it by parsing into
        /// the type the journal stores.
        #[arg(long, value_name = "FILE")]
        json: Option<String>,
    },
    /// The decisions waiting for a person.
    List {
        /// Include the ones already ruled.
        #[arg(long)]
        all: bool,
        /// One decision, by id, as JSON.
        #[arg(long)]
        id: Option<String>,
    },
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let cli = match super::parse::<Cli>("tab-atelier decision", args) {
        Ok(c) => c,
        Err(code) => return code,
    };
    // Not `discover_endpoint()?`-and-bail: this verb is expected to be usable from a hook that
    // might run outside a tab, and a silent 0 there would hide a misconfiguration. The API's
    // address is discovered the same way every other client verb does it, and a failure to find it
    // is reported because the caller asked for something that needs it.
    let Ok(ep) = super::client::discover_endpoint() else {
        eprintln!("decision: no tab-atelier API to talk to (is the daemon running?)");
        return 1;
    };
    match cli.cmd {
        Verb::Open {
            id,
            title,
            why,
            project,
            reco,
            json,
        } => open(
            &ep,
            &id,
            title.as_deref(),
            why.as_deref(),
            project.as_deref(),
            reco.as_deref(),
            json.as_deref(),
        ),
        Verb::List { all, id } => list(&ep, all, id.as_deref()),
    }
}

fn open(
    ep: &super::client::Endpoint,
    id: &str,
    title: Option<&str>,
    why: Option<&str>,
    project: Option<&str>,
    reco: Option<&str>,
    json: Option<&str>,
) -> i32 {
    let body = match json {
        // A whole document: the producer knows its own shape, and the daemon is what validates it.
        Some(path) => match read_json_source(path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("decision open: {e}");
                return 1;
            }
        },
        None => serde_json::json!({
            "id": id,
            "state": "open",
            "title": title,
            "whyGated": why,
            "project": project,
            "reco": reco,
        })
        .to_string(),
    };
    match super::client::api_post_json_to(ep, "/decisions", body) {
        Ok(answer) => {
            // The daemon's word, not ours: it is the one that knows whether this id was new, a
            // repeat, or already ruled — and a CLI that printed "added" for all three would be
            // lying to a cron.
            let result = answer.get("result").and_then(|v| v.as_str()).unwrap_or("?");
            let wrote = answer
                .get("wrote")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let shown_id = answer.get("id").and_then(|v| v.as_str()).unwrap_or(id);
            match (result, wrote) {
                ("appended", _) => println!("✓ {shown_id} added to the board"),
                ("updated", _) => println!("✓ {shown_id} updated (still waiting)"),
                ("already-present", _) => println!("= {shown_id} was already there; nothing changed"),
                // The one that matters: a producer re-posting over a settled decision must be told
                // plainly, because its belief about the world is out of date.
                ("already-ruled", _) => {
                    println!("= {shown_id} has already been ruled; the verdict was left alone");
                }
                _ => println!("✓ {shown_id}: {result}"),
            }
            0
        }
        Err(e) => {
            eprintln!("decision open: {e}");
            1
        }
    }
}

/// A `--json` source: a path, or `-` for standard input.
fn read_json_source(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("reading stdin: {e}"))?;
        return Ok(text);
    }
    std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))
}

fn list(ep: &super::client::Endpoint, all: bool, id: Option<&str>) -> i32 {
    let path = if all {
        "/decisions?includeArchived=true"
    } else {
        "/decisions"
    };
    let Ok(answer) = super::client::api_get_json(ep, path) else {
        eprintln!("decision list: the daemon did not answer");
        return 1;
    };
    let Some(decisions) = answer.get("decisions").and_then(|v| v.as_array()) else {
        eprintln!("decision list: unexpected answer");
        return 1;
    };
    // One decision by id is a different question, and answers as JSON so it can be piped.
    if let Some(want) = id {
        return decisions
            .iter()
            .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(want))
            .map_or_else(
                || {
                    eprintln!("decision list: no decision with id {want}");
                    1
                },
                |found| {
                    serde_json::to_string_pretty(found).map_or_else(
                        |e| {
                            eprintln!("decision list: {e}");
                            1
                        },
                        |pretty| {
                            println!("{pretty}");
                            0
                        },
                    )
                },
            );
    }
    if decisions.is_empty() {
        println!("nothing waiting");
        return 0;
    }
    // One line each, id first so it is greppable: a reader wants to see what needs a ruling, and
    // the full object is one `--id` away.
    for d in decisions {
        let get = |k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("");
        let id = get("id");
        let state = get("state");
        let title = get("title");
        let project = get("project");
        if project.is_empty() {
            println!("{id}\\t{state}\\t{title}");
        } else {
            println!("{id}\\t{state}\\t[{project}] {title}");
        }
    }
    0
}
