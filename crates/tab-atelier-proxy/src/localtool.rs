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
//! goes in as a completed exchange instead — the call already made, the result
//! already in hand — so the model reads the data without ever being able to ask
//! for it. The client stores only the model's answer, never this exchange, so
//! the injection does not accumulate across turns.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::egress;

/// The name an operator declares in a policy's `add` list.
pub const CLOUDFLARE_IPS: &str = "cloudflare_ips";

/// Cloudflare's published ranges, v4 then v6, exactly as asked for. Both are
/// plain text, one CIDR per line.
const SOURCES: [&str; 2] = ["https://www.cloudflare.com/ips-v4", "https://www.cloudflare.com/ips-v6"];

/// A version and a project URL, which is what an operator on the other side of
/// a log line or an abuse report needs in order to know who is calling.
const USER_AGENT: &str = concat!(
    "tab-atelier-proxy/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/wdes/tab-atelier)"
);

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

/// Fixed rather than generated: the exchange lives for exactly one request, and
/// the client never echoes it back, so there is nothing for two ids to collide
/// with. A random one would only make the capture harder to read.
const TOOL_USE_ID: &str = "srvtoolu_cloudflare_ips";

static CACHE: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();

/// Whether this name is one the proxy answers rather than forwards.
///
/// Matched the way every other name in this engine is — case-insensitively — so
/// a policy keyed `Cloudflare_IPS` does not quietly do nothing.
#[must_use]
pub const fn is_local(name: &str) -> bool {
    name.eq_ignore_ascii_case(CLOUDFLARE_IPS)
}

/// Put the tool's result in `messages[]`, if the policy asked for it.
///
/// `names` is whatever the policy's `add` list resolved to as local; only
/// [`CLOUDFLARE_IPS`] is understood today, and an unrecognised name is dropped
/// rather than injected as a result for a call that never happened.
pub fn inject(body: &mut Value, names: &[String]) {
    if !names.iter().any(|name| is_local(name)) {
        return;
    }
    let text = ranges().unwrap_or_else(|err| {
        log::warn!("proxy: {CLOUDFLARE_IPS}: {err}");
        // A real tool that fails still hands back a result saying so, and the
        // model gets to decide what that means. Silence would be a stranger
        // failure: the tool call is in the transcript either way.
        format!("{CLOUDFLARE_IPS} could not be fetched: {err}")
    });
    inject_exchange(body, &text);
}

/// The completed call-and-result pair, appended after the caller's messages.
///
/// It goes last so the model reads the caller's question, then the data, then
/// answers the question with the data in hand.
fn inject_exchange(body: &mut Value, text: &str) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    messages.push(json!({
        "role": "assistant",
        "content": [{
            "type": "tool_use",
            "id": TOOL_USE_ID,
            "name": CLOUDFLARE_IPS,
            "input": {},
        }],
    }));
    messages.push(json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": TOOL_USE_ID,
            "content": text,
        }],
    }));
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
        .header("User-Agent", USER_AGENT)
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
    fn the_result_is_appended_as_a_completed_exchange() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        inject_exchange(&mut body, "1.1.1.1/32");
        assert_eq!(
            body["messages"].as_array().map(Vec::len),
            Some(3),
            "the exchange is two messages on top of the caller's one"
        );
        let call = &body["messages"][1];
        assert_eq!(call["role"], "assistant");
        assert_eq!(call["content"][0]["type"], "tool_use");
        assert_eq!(call["content"][0]["name"], CLOUDFLARE_IPS);
        let result = &body["messages"][2];
        assert_eq!(result["role"], "user");
        assert_eq!(result["content"][0]["type"], "tool_result");
        assert_eq!(result["content"][0]["content"], "1.1.1.1/32");
        assert_eq!(
            call["content"][0]["id"], result["content"][0]["tool_use_id"],
            "the result must name the call it answers"
        );
    }

    #[test]
    fn a_body_without_messages_is_left_alone() {
        let mut body = json!({"tools": []});
        inject_exchange(&mut body, "1.1.1.1/32");
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
