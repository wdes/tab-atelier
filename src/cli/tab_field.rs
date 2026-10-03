// SPDX-License-Identifier: MPL-2.0

//! Shared runner for the single-field "set a tab field" CLIs.
//!
//! `set-assignment`, `set-rehome-status` and the agent-card `set-*` verbs are
//! ~80 % identical: the same `[--tab <id>] [--clear] <value…>` parse loop, the
//! same silent-no-op-outside-a-tab env guard, the same `_TAB_ID` fallback, and
//! the same `POST /tabs/by-id/{id}/{verb}` with a one-field JSON body. This
//! collapses them into one [`Field`] descriptor + [`post`], with the parse
//! extracted as a PURE, unit-tested [`parse`].

use clap::Parser;
use std::time::Duration;

/// What a single-field CLI's args resolve to (pure parse result).
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Set the field to this value (`<value…>` joined, trimmed non-empty).
    Set(String),
    /// Clear the field (`--clear`).
    Clear,
    /// Print usage and exit 0 (`-h` / `--help`).
    Help,
}

/// A parsed single-field invocation: the action + the optional `--tab <id>`.
#[derive(Debug, PartialEq, Eq)]
pub struct Parsed {
    pub action: Action,
    pub tab: Option<String>,
}

/// PURE parse of `[--tab <id>] [--clear] <value…>` — no env, no IO.
///
/// `name` is the subcommand name, only used to format the messages.
///
/// # Errors
/// Returns `Err((2, msg))` on any usage error (dangling `--tab`, unknown flag,
/// nothing to set).
pub fn parse(name: &str, args: &[String]) -> Result<Parsed, (i32, String)> {
    let raw = match Raw::try_parse_from(std::iter::once(name.to_owned()).chain(args.iter().cloned())) {
        Ok(raw) => raw,
        Err(err) if err.kind() == clap::error::ErrorKind::DisplayHelp => {
            return Ok(Parsed {
                action: Action::Help,
                tab: None,
            });
        }
        Err(err) => return Err((2, err.to_string())),
    };
    if raw.clear {
        return Ok(Parsed {
            action: Action::Clear,
            tab: raw.tab,
        });
    }
    let value = raw.parts.join(" ").trim().to_string();
    if value.is_empty() {
        return Err((
            2,
            format!("{name}: nothing to set — pass a value, or --clear (see --help)"),
        ));
    }
    Ok(Parsed {
        action: Action::Set(value),
        tab: raw.tab,
    })
}

#[derive(Parser, Debug)]
#[command(name = "tab-atelier", disable_help_subcommand = true)]
struct Raw {
    #[arg(long, value_name = "id")]
    tab: Option<String>,
    #[arg(long)]
    clear: bool,
    #[arg(value_name = "value")]
    parts: Vec<String>,
}

/// One single-field tab CLI, driving [`post`].
pub struct Field {
    /// Subcommand name, for error/usage prefixes (`"set-assignment"`).
    pub name: &'static str,
    /// URL segment: `POST /tabs/by-id/{id}/{verb}` (`"assignment"`, `"rehome"`).
    pub verb: &'static str,
    /// JSON body field name (usually == `verb`, but `rehome` → `rehome_status`).
    pub json_field: &'static str,
    /// Printed on a successful set / clear.
    pub set_msg: &'static str,
    pub clear_msg: &'static str,
    /// Full `--help` text.
    pub usage: &'static str,
    /// Map an HTTP error status to a friendly message (`set-rehome` maps 400).
    pub status_err: fn(u16) -> Option<&'static str>,
}

/// Read the `(url, token)` env pair, or `None` outside a tab (silent no-op).
pub(crate) fn api_env() -> Option<(String, String)> {
    super::client::discover_endpoint()
        .ok()
        .map(|endpoint| (endpoint.url, endpoint.token))
}

/// A 2 s-timeout ureq agent — the one used by every tab CLI.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .new_agent()
}

/// Shared HTTP tail: `POST /tabs/by-id/{tab_id}/{verb}` with a Bearer + JSON.
///
/// # Errors
/// Propagates the ureq transport / status error.
pub fn send(
    api_url: &str,
    api_token: &str,
    tab_id: &str,
    verb: &str,
    body: &str,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    agent()
        .post(format!("{api_url}/tabs/by-id/{tab_id}/{verb}"))
        .header("Authorization", &format!("Bearer {api_token}"))
        .header("Content-Type", "application/json")
        .send(body)
}

/// The full runner for a single-field tab CLI.
///
/// parse → env guard → resolve tab → POST (silent no-op outside a tab; `_TAB_ID`
/// fallback; status mapping).
#[must_use]
pub fn post(field: &Field, args: &[String]) -> i32 {
    let parsed = match parse(field.name, args) {
        Ok(p) => p,
        Err((code, msg)) => {
            eprintln!("{msg}");
            return code;
        }
    };
    if parsed.action == Action::Help {
        eprintln!("{}", field.usage);
        return 0;
    }
    // Outside a tab-atelier tab the API env isn't exported — silent no-op.
    let Some((api_url, api_token)) = api_env() else {
        return 0;
    };
    let value = match parsed.action {
        Action::Clear => None,
        Action::Set(v) => Some(v),
        Action::Help => unreachable!("handled above"),
    };
    let Some(tab_id) = parsed
        .tab
        .or_else(|| std::env::var("_TAB_ID").ok())
        .filter(|s| !s.is_empty())
    else {
        eprintln!(
            "{}: TAB_ATELIER env present but _TAB_ID unset — pass --tab <id>",
            field.name
        );
        return 1;
    };
    let cleared = value.is_none();
    let body = serde_json::json!({ field.json_field: value }).to_string();
    match send(&api_url, &api_token, &tab_id, field.verb, &body) {
        Ok(_) => {
            println!("{}", if cleared { field.clear_msg } else { field.set_msg });
            0
        }
        Err(ureq::Error::StatusCode(code)) if (field.status_err)(code).is_some() => {
            eprintln!("{}: {}", field.name, (field.status_err)(code).unwrap_or_default());
            1
        }
        Err(e) => {
            eprintln!("{}: {e}", field.name);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_set_joins_and_trims_value() {
        let p = parse("set-x", &["build/implementer".into()]).unwrap();
        assert_eq!(p.action, Action::Set("build/implementer".into()));
        assert_eq!(p.tab, None);
        let p = parse("set-x", &["  hello".into(), "world  ".into()]).unwrap();
        assert_eq!(p.action, Action::Set("hello world".into()));
    }

    #[test]
    fn parse_reads_tab_override() {
        let p = parse("set-x", &["--tab".into(), "old-uuid".into(), "review/reviewer".into()]).unwrap();
        assert_eq!(p.tab.as_deref(), Some("old-uuid"));
        assert_eq!(p.action, Action::Set("review/reviewer".into()));
    }

    #[test]
    fn parse_clear_and_help() {
        assert_eq!(parse("set-x", &["--clear".into()]).unwrap().action, Action::Clear);
        assert_eq!(parse("set-x", &["-h".into()]).unwrap().action, Action::Help);
        assert_eq!(parse("set-x", &["--help".into()]).unwrap().action, Action::Help);
        assert_eq!(
            parse("set-x", &["v".into(), "--clear".into()]).unwrap().action,
            Action::Clear
        );
    }

    #[test]
    fn parse_rejects_dangling_tab_flag() {
        let (code, msg) = parse("set-x", &["--tab".into()]).unwrap_err();
        assert_eq!(code, 2);
        assert!(msg.contains("--tab"), "{msg}");
    }

    #[test]
    fn parse_unknown_flag_errors() {
        let (code, msg) = parse("set-x", &["--bogus".into()]).unwrap_err();
        assert_eq!(code, 2);
        assert!(msg.contains("--bogus"), "{msg}");
    }

    #[test]
    fn parse_errors_on_empty() {
        assert_eq!(parse("set-x", &[]).unwrap_err().0, 2);
        assert_eq!(parse("set-x", &["   ".into()]).unwrap_err().0, 2);
    }
}
