// SPDX-License-Identifier: MPL-2.0

//! Tools the proxy answers itself rather than forwarding.
//!
//! A *server tool* is declared in `tools[]` with a versioned `type` and resolved
//! at the far end — `web_search_20250305` is the familiar one. The far end is
//! not always willing to: `DeepSeek`'s Anthropic-shaped endpoint accepts exactly
//! two `type` values and answers a 400 for the whole body otherwise, so a
//! declaration it does not recognise is a request that never reaches a model.
//!
//! That is the hook this module turns into a feature. An operator declares one
//! of these names in a person's tool policy `add` list the way they would
//! declare any other injected tool; the policy pass hands the name here instead
//! of forwarding the declaration, and the proxy supplies the data itself. What
//! is invented never leaves for the upstream, so nothing can be refused over it.
//!
//! The declaration is deliberately **not** rebuilt as a callable function tool.
//! A callable tool is one the model may call, and a call to a tool the client
//! has never heard of returns to the client as a call it cannot run. The data
//! goes in as an aside on the caller's own turn instead, so the model reads the
//! data without ever being able to ask for it. The client stores only the
//! model's answer, never this aside, so the injection does not accumulate
//! across turns.
//!
//! It is also deliberately not a fabricated `tool_use`/`tool_result` exchange.
//! That shape means inventing an assistant turn the model never took, and any
//! vendor running in a thinking mode rejects one: `DeepSeek`'s Anthropic endpoint
//! requires an assistant turn's `thinking` blocks be passed back with it, and a
//! turn that did not happen has none to pass. A text block cannot trip that.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::egress;

/// The name an operator declares in a policy's `add` list.
pub const CLOUDFLARE_IPS: &str = "cloudflare_ips";

/// Cloudflare's published ranges, v4 then v6, exactly as asked for. Both are
/// plain text, one CIDR per line.
const SOURCES: [&str; 2] = ["https://www.cloudflare.com/ips-v4", "https://www.cloudflare.com/ips-v6"];

/// Cloudflare's lists change rarely, and an opted-in account carries the tool on
/// every request, so fetching per request would put two GETs in front of every
/// model call. The ceiling is this window: a range announced inside it is not
/// seen until it lapses, which is the right trade for a value that is closer to
/// documentation than to telemetry.
const TTL: Duration = Duration::from_mins(5);

/// A fetch that never returns is worse than one that fails, because it holds
/// the request open. The shared egress agent sets a connect timeout only — LLM
/// streams legitimately run for minutes — so this one is set per request.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The endpoints are a few hundred bytes each today. The cap exists because the
/// body is injected into a prompt, and a prompt is billed by the token.
const MAX_BYTES: u64 = 64 * 1024;

static CACHE: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();

/// Whether this name is one the proxy answers rather than forwards.
///
/// Matched the way every other name in this engine is — case-insensitively — so
/// a policy keyed `Cloudflare_IPS` does not quietly do nothing.
#[must_use]
pub const fn is_local(name: &str) -> bool {
    name.eq_ignore_ascii_case(CLOUDFLARE_IPS)
}

/// Put the tool's data in `messages[]`, if the policy asked for it.
///
/// `names` is whatever the policy's `add` list resolved to as local; only
/// [`CLOUDFLARE_IPS`] is understood today, and an unrecognised name is dropped
/// rather than injected as data for a lookup that never happened.
pub fn inject(body: &mut Value, names: &[String]) {
    if !names.iter().any(|name| is_local(name)) {
        return;
    }
    let text = ranges().unwrap_or_else(|err| {
        log::warn!("proxy: {CLOUDFLARE_IPS}: {err}");
        // A real tool that fails still hands back something saying so, and the
        // model gets to decide what that means. Silence would be a stranger
        // failure: the aside is in the prompt either way.
        format!("{CLOUDFLARE_IPS} could not be fetched: {err}")
    });
    inject_note(body, &text);
}

/// The fetched data as an aside on the conversation's last turn, so the model
/// reads the caller's question, then the data, then answers with it in hand.
///
/// The data joins a trailing user turn rather than starting a second user
/// message: two user turns in a row is a shape some vendors reject, and the
/// caller's question and this aside are one turn's worth of reading anyway.
/// When the caller ended on an assistant turn — a prefill — there is nothing to
/// join, so a user turn is added.
fn inject_note(body: &mut Value, text: &str) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let note = json!({ "type": "text", "text": format!("{CLOUDFLARE_IPS}:\n{text}") });
    let ends_on_a_user_turn = messages
        .last()
        .and_then(|last| last.get("role"))
        .and_then(Value::as_str)
        == Some("user");
    if ends_on_a_user_turn {
        if let Some(last) = messages.last_mut() {
            append_note(last, note);
        }
        return;
    }
    messages.push(json!({ "role": "user", "content": [note] }));
}

/// Attach `note` to a message's content, whatever shape that content is in.
///
/// A caller's prompt arrives either as a bare string or as an array of blocks.
/// The string is wrapped in a text block rather than replaced, so nothing the
/// caller wrote is lost.
fn append_note(message: &mut Value, note: Value) {
    match message.get_mut("content") {
        Some(Value::Array(blocks)) => blocks.push(note),
        Some(slot) => {
            let prior = match slot {
                Value::String(prompt) => {
                    vec![json!({ "type": "text", "text": std::mem::take(prompt) })]
                }
                _ => Vec::new(),
            };
            *slot = Value::Array(prior.into_iter().chain([note]).collect());
        }
        None => message["content"] = Value::Array(vec![note]),
    }
}

/// The concatenated lists, cached for [`TTL`].
fn ranges() -> Result<String, String> {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some((at, text)) = guard.as_ref()
        && at.elapsed() < TTL
    {
        return Ok(text.clone());
    }
    let text = fetch()?;
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), text.clone()));
    }
    Ok(text)
}

fn fetch() -> Result<String, String> {
    let [v4_url, v6_url] = SOURCES;
    let agent = egress::relay_agent();
    let v4 = get(&agent, v4_url)?;
    let v6 = get(&agent, v6_url)?;
    Ok(render(&v4, &v6))
}

fn get(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let mut response = agent
        .get(url)
        .config()
        .timeout_global(Some(TIMEOUT))
        .build()
        .header("User-Agent", crate::egress::USER_AGENT)
        .call()
        .map_err(|err| format!("{url}: {err}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("{url}: HTTP {status}"));
    }
    response
        .body_mut()
        .with_config()
        .limit(MAX_BYTES)
        .read_to_string()
        .map_err(|err| format!("{url}: {err}"))
}

/// One list under the other, each trimmed, with no blank line for a list that
/// came back empty.
fn render(v4: &str, v6: &str) -> String {
    let mut out = String::new();
    for block in [v4, v6] {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(block);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_names_match_loosely() {
        assert!(is_local("cloudflare_ips"));
        assert!(is_local("Cloudflare_IPS"));
        assert!(!is_local("web_search"));
    }

    #[test]
    fn the_two_lists_are_concatenated_in_order() {
        assert_eq!(render("1.0.0.0/24\n", "2001:db8::/32\n"), "1.0.0.0/24\n2001:db8::/32");
    }

    #[test]
    fn an_empty_list_leaves_no_blank_line() {
        assert_eq!(render("1.0.0.0/24", "  \n"), "1.0.0.0/24");
        assert_eq!(render("", ""), "");
    }

    #[test]
    fn the_result_joins_the_callers_last_turn() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        inject_note(&mut body, "1.1.1.1/32");
        assert_eq!(
            body["messages"].as_array().map(Vec::len),
            Some(1),
            "no message is invented: the aside joins the caller's turn"
        );
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("the string content became blocks");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hi", "the caller's prompt survives");
        assert_eq!(content[1]["type"], "text");
        assert!(
            content[1]["text"].as_str().is_some_and(|t| t.contains("1.1.1.1/32")),
            "the data is in the aside: {}",
            content[1]["text"]
        );
    }

    /// The regression this exists for: `DeepSeek`'s Anthropic endpoint rejects a
    /// thinking-mode assistant turn that carries no `thinking` block, and a
    /// fabricated `tool_use` turn carried none.
    #[test]
    fn no_assistant_turn_is_invented() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        inject_note(&mut body, "1.1.1.1/32");
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|m| m["role"].as_str())
            .collect();
        assert_eq!(roles, ["user"], "the model is never told it acted");
        let types: Vec<&str> = body["messages"][0]["content"]
            .as_array()
            .expect("blocks")
            .iter()
            .filter_map(|b| b["type"].as_str())
            .collect();
        assert_eq!(types, ["text", "text"], "no tool_use, no tool_result");
    }

    #[test]
    fn a_prefill_gets_a_turn_of_its_own() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "Sure"},
            ]
        });
        inject_note(&mut body, "1.1.1.1/32");
        let messages = body["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "text");
    }

    #[test]
    fn a_body_without_messages_is_left_alone() {
        let mut body = json!({"tools": []});
        inject_note(&mut body, "1.1.1.1/32");
        assert_eq!(body, json!({"tools": []}));
    }

    /// The short-circuit matters for more than tidiness: reaching the fetch
    /// would make this test hit the network.
    #[test]
    fn a_name_that_is_not_local_injects_nothing() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        inject(&mut body, &["something_else".to_owned()]);
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
    }
}
