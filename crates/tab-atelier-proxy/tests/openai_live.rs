// SPDX-License-Identifier: MPL-2.0

//! The `OpenAI` translator, against the real API.
//!
//! The offline unit tests live next to the code they cover. This file is the
//! part that needs the network: it takes an Anthropic-shaped request, runs it
//! through [`tab_atelier_proxy::openai::to_chat`], sends it to `OpenAI`, and
//! drives the reply back through the [`Translator`]. What it proves is the one
//! claim the design rests on — that the bytes coming out are the same
//! Anthropic-shaped bytes a client would have got from Anthropic itself, and
//! that the `usage::Sniffer` can bill them without knowing anything changed.
//!
//! It skips unless a key is available, so CI stays green without one:
//!
//! ```text
//! OPENAI_API_KEY=sk-... cargo test -p tab-atelier-proxy --test openai_live
//! ```
//!
//! or leave the key where the proxy itself would look for it, at
//! `~/.config/tab-atelier-proxy/provider-openai.key`. The proxy finds it there
//! because that is what `provider_key_path` resolves to, and the key is
//! re-read on every request — so a rotated key needs no restart.

use serde_json::{Value, json};
use tab_atelier_proxy::openai::{Translator, chat_url};
use tab_atelier_proxy::usage::Sniffer;

/// The model used for the cheap live checks. Luna is the fast tier and the
/// cheapest of the three, which is the right default for a test that runs on
/// somebody's real bill.
const LUNA: &str = "gpt-5.6-luna";

/// Resolve the key the way the proxy does, then the way a shell does.
fn api_key() -> Option<String> {
    if let Ok(key) = std::env::var("OPENAI_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key.trim().to_owned());
    }
    let path = dirs_config()?.join("tab-atelier-proxy/provider-openai.key");
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn dirs_config() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME").map_or_else(
        || std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config")),
        |dir| Some(std::path::PathBuf::from(dir)),
    )
}

/// A request shaped exactly as the proxy would hand one to `forward`: the
/// Anthropic body *after* `shape_body` has had its turn, so nothing here is
/// reaching `OpenAI` that the tool policy did not already approve.
fn anthropic_request(model: &str, prompt: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "system": "You are terse. Obey the instruction literally.",
        "messages": [{"role": "user", "content": prompt}],
    })
}

fn post_chat(key: &str, body: &Value) -> ureq::Body {
    // A non-2xx must come back as a response, not an error, so a 400 prints
    // the API's own explanation instead of a status code.
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let resp = agent
        .post(chat_url("https://api.openai.com/v1"))
        .header("authorization", &format!("Bearer {key}"))
        .header("content-type", "application/json")
        .send_json(body)
        .unwrap_or_else(|e| panic!("openai request failed: {e}"));
    resp.into_body()
}

/// Read the whole (non-streaming) body as JSON.
fn body_json(mut body: ureq::Body) -> Value {
    let text = body.read_to_string().unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("unparsable body: {e}\n{text}"));
    // `http_status_as_error` is off so a rejection arrives as a body. Catch it
    // here rather than letting it translate into an empty completion.
    assert!(value.get("error").is_none(), "openai rejected the request: {value}");
    value
}

/// The load-bearing test: a full streamed turn, translated, then billed.
#[test]
fn a_live_streamed_turn_translates_and_bills() {
    let Some(key) = api_key() else {
        eprintln!("skipping: no OPENAI_API_KEY and no provider-openai.key");
        return;
    };

    // 1. Anthropic request in. `stream` must be set here rather than after,
    // because it is what makes `to_chat` ask OpenAI to report usage — the
    // final chunk is the only place a stream tells you what it cost.
    let anthropic = json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 64,
        "stream": true,
        "system": "You are terse. Obey the instruction literally.",
        "messages": [{"role": "user", "content": "Reply with exactly: pong"}],
    });
    let mut chat = tab_atelier_proxy::openai::to_chat(&anthropic);

    // The model the client asked for is Anthropic's; the provider is OpenAI's.
    // In production `shape_body` has already rewritten `model` to the id this
    // provider serves (that is what routing and the per-user pin do), so this
    // is the one field the test sets by hand.
    chat["model"] = json!(LUNA);

    // 2. Down the wire.
    let mut reader = post_chat(&key, &chat).into_reader();

    // 3. Back out, as Anthropic-shaped SSE.
    let mut translator = Translator::new();
    let mut sniffer = Sniffer::new(Some("text/event-stream"));
    let mut out = Vec::new();
    let mut buf = vec![0_u8; 4096];
    loop {
        let n = std::io::Read::read(&mut reader, &mut buf).expect("read chunk");
        if n == 0 {
            break;
        }
        for frame in translator.feed(&buf[..n]) {
            sniffer.feed(&frame);
            out.extend_from_slice(&frame);
        }
    }
    for frame in translator.finish() {
        sniffer.feed(&frame);
        out.extend_from_slice(&frame);
    }
    let sse = String::from_utf8_lossy(&out).into_owned();

    // 4. The event vocabulary must be Anthropic's, in Anthropic's order. This
    // is what the real client parses; if the shape is wrong it fails there.
    let names: Vec<&str> = sse.lines().filter_map(|line| line.strip_prefix("event: ")).collect();
    assert_eq!(
        names.first().copied(),
        Some("message_start"),
        "first event was {:?}\n{sse}",
        names.first()
    );
    assert!(names.contains(&"content_block_delta"), "no text arrived\n{sse}");
    assert_eq!(
        names.last().copied(),
        Some("message_stop"),
        "last event was {:?}\n{sse}",
        names.last()
    );
    assert!(!names.contains(&"error"), "the stream carried an error event\n{sse}");

    // 5. The text must actually be there.
    assert!(sse.contains("pong"), "expected the model to say pong\n{sse}");

    // 6. And the split must bill. This is the part that matters: no code in
    // the billing path knows OpenAI exists.
    let (model, tokens) = sniffer.finish();
    // The model OpenAI actually served. In production `shape_body` has
    // already rewritten the client's Anthropic id to this one before the body
    // reaches `forward`, so a client is told the truth about where its tokens
    // went rather than the name it asked with.
    assert_eq!(model.as_deref(), Some(LUNA));
    assert!(tokens.input > 0, "input tokens not counted: {tokens:?}");
    assert!(tokens.output > 0, "output tokens not counted: {tokens:?}");
    eprintln!("live stream ok: {tokens:?} via {LUNA}");
}

/// Non-streaming is the path a health check or a non-SSE client takes. Same
/// request, one JSON reply, translated on the way home.
#[test]
fn a_live_unstreamed_turn_translates() {
    let Some(key) = api_key() else {
        eprintln!("skipping: no key");
        return;
    };

    let anthropic = anthropic_request(LUNA, "Reply with exactly: pong");
    let mut chat = tab_atelier_proxy::openai::to_chat(&anthropic);
    chat["model"] = json!(LUNA);
    chat["stream"] = json!(false);

    let reply = body_json(post_chat(&key, &chat));
    let translated = tab_atelier_proxy::openai::from_chat(&reply);

    assert_eq!(translated["type"], json!("message"));
    assert_eq!(translated["role"], json!("assistant"));
    assert_eq!(translated["stop_reason"], json!("end_turn"));

    let text = translated["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<String>();
    assert!(text.contains("pong"), "got {text:?}");

    let usage = &translated["usage"];
    assert!(
        usage["input_tokens"].as_u64().is_some_and(|n| n > 0),
        "input_tokens missing: {usage}"
    );
    assert!(
        usage["output_tokens"].as_u64().is_some_and(|n| n > 0),
        "output_tokens missing: {usage}"
    );
    eprintln!("live unstreamed ok: {usage}");
}

/// A tool call, translated the other way: the client sent `tools`, `OpenAI`
/// decides to call one, and the block the client sees must be `tool_use`.
#[test]
fn a_live_tool_call_comes_back_as_a_tool_use_block() {
    let Some(key) = api_key() else {
        eprintln!("skipping: no key");
        return;
    };

    let mut anthropic = anthropic_request(LUNA, "What is the weather in Paris?");
    anthropic["tools"] = json!([{
        "name": "get_weather",
        "description": "Get the current weather for a city.",
        "input_schema": {
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }
    }]);
    anthropic["tool_choice"] = json!({"type": "any"});

    let mut chat = tab_atelier_proxy::openai::to_chat(&anthropic);
    chat["model"] = json!(LUNA);
    chat["stream"] = json!(false);

    let reply = body_json(post_chat(&key, &chat));
    let translated = tab_atelier_proxy::openai::from_chat(&reply);

    let call = translated["content"]
        .as_array()
        .expect("content array")
        .iter()
        .find(|block| block["type"] == json!("tool_use"))
        .unwrap_or_else(|| panic!("no tool_use block in {translated}"));
    assert_eq!(call["name"], json!("get_weather"));
    assert!(
        call["input"]["city"].as_str().is_some_and(|c| !c.is_empty()),
        "arguments did not survive translation: {call}"
    );
    assert_eq!(translated["stop_reason"], json!("tool_use"));
    eprintln!("live tool call ok: {call}");
}

/// The real-world path: a coding agent streams *and* sends tools, so the tool
/// arguments arrive as a run of `function.arguments` fragments that have to be
/// stitched back into one JSON object and re-framed as `input_json_delta`.
#[test]
fn a_live_streamed_tool_call_reassembles_the_arguments() {
    let Some(key) = api_key() else {
        eprintln!("skipping: no key");
        return;
    };

    let anthropic = json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 256,
        "stream": true,
        "messages": [{"role": "user", "content": "What is the weather in Paris and in Lyon?"}],
        "tools": [{
            "name": "get_weather",
            "description": "Get the current weather for a city.",
            "input_schema": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        }],
        "tool_choice": {"type": "any"},
    });
    let mut chat = tab_atelier_proxy::openai::to_chat(&anthropic);
    chat["model"] = json!(LUNA);

    let mut reader = post_chat(&key, &chat).into_reader();
    let mut translator = Translator::new();
    let mut sniffer = Sniffer::new(Some("text/event-stream"));
    let mut out = Vec::new();
    let mut buf = vec![0_u8; 4096];
    loop {
        let n = std::io::Read::read(&mut reader, &mut buf).expect("read chunk");
        if n == 0 {
            break;
        }
        for frame in translator.feed(&buf[..n]) {
            sniffer.feed(&frame);
            out.extend_from_slice(&frame);
        }
    }
    for frame in translator.finish() {
        sniffer.feed(&frame);
        out.extend_from_slice(&frame);
    }
    let sse = String::from_utf8_lossy(&out).into_owned();

    // The block must be announced as a tool_use up front, then filled in
    // through deltas — never leaked as a text block.
    assert!(
        sse.contains(r#""type":"tool_use""#),
        "no tool_use block was opened\n{sse}"
    );
    assert!(
        sse.contains("input_json_delta"),
        "no argument deltas were emitted\n{sse}"
    );

    // The reassembled arguments must be one parseable JSON object naming the
    // city. Pull them back out of the deltas the way a client would: grouped
    // by block index, because a model may answer with several parallel calls
    // and each one gets its own block.
    let mut partials: std::collections::BTreeMap<u64, String> = std::collections::BTreeMap::new();
    for line in sse.lines().filter_map(|l| l.strip_prefix("data: ")) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let (Some(index), Some(json)) = (event["index"].as_u64(), event["delta"]["partial_json"].as_str()) {
            partials.entry(index).or_default().push_str(json);
        }
    }
    assert!(!partials.is_empty(), "no argument deltas\n{sse}");

    let mut cities = Vec::new();
    for (index, partial) in &partials {
        let args: Value = serde_json::from_str(partial)
            .unwrap_or_else(|e| panic!("block {index} did not reassemble: {e}\nraw: {partial}\n{sse}"));
        if let Some(city) = args["city"].as_str() {
            cities.push(city.to_owned());
        }
    }
    assert!(
        cities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("paris") || c.eq_ignore_ascii_case("lyon")),
        "unexpected cities in {cities:?}"
    );

    // And a tool call must be billed as a tool call, not an utterance.
    let (_model, tokens) = sniffer.finish();
    assert!(tokens.input > 0 && tokens.output > 0, "not billed: {tokens:?}");
    eprintln!("live streamed tool call ok: {cities:?}");
}

/// The tool policy runs on the Anthropic body *before* translation, so what a
/// client's `disable`/`rewrite`/`add` did has to survive the hop to `OpenAI`'s
/// shape. The rewrite rules earn their keep here: `Provider` exists precisely
/// because "US-only" describes Anthropic's search backend, and this is a hop
/// that is not Anthropic.
#[test]
fn the_tool_policy_survives_translation_to_openai() {
    use std::collections::BTreeMap;

    use tab_atelier_proxy::tools::{self, Mode, Normalise, Policy};

    let mut rewrite = BTreeMap::new();
    rewrite.insert(
        "WebFetch".to_owned(),
        vec![
            Normalise::Provider,
            Normalise::Replaced {
                find: "harness".to_owned(),
                replace: "sandbox".to_owned(),
            },
        ],
    );
    let policy = Policy {
        mode: Mode::All,
        disable: vec!["WebSearch".to_owned()],
        allow: Vec::new(),
        add: vec![json!({
            "name": "get_weather",
            "description": "Get the current weather for a city.",
            "input_schema": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        })],
        rewrite,
    };

    let mut body = json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {
                "name": "WebSearch",
                "description": "Search the web.",
                "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}
            },
            {
                "name": "WebFetch",
                "description": "Fetch a page. The backend is US-only. \
                                Keep it inside the harness limits.",
                "input_schema": {"type": "object", "properties": {"url": {"type": "string"}}}
            }
        ]
    });

    let report = tools::apply(&mut body, &policy, true);
    assert_eq!(report.removed, vec!["WebSearch".to_owned()]);
    assert!(report.changed());

    let chat = tab_atelier_proxy::openai::to_chat(&body);
    let names: Vec<&str> = chat["tools"]
        .as_array()
        .expect("tools survived translation")
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(names, ["WebFetch", "get_weather"], "disable/add did not reach OpenAI");

    let fetch = &chat["tools"][0]["function"]["description"];
    let fetch = fetch.as_str().unwrap_or_default();
    assert!(
        !fetch.to_ascii_lowercase().contains("us-only"),
        "provider claim leaked: {fetch}"
    );
    assert!(fetch.contains("sandbox"), "literal swap did not reach OpenAI: {fetch}");
    assert!(!fetch.contains("harness"), "old literal survived: {fetch}");

    // An added tool keeps its schema — the point of `add` is a real contract.
    assert_eq!(
        chat["tools"][1]["function"]["parameters"]["properties"]["city"]["type"],
        "string"
    );

    // And tools are present, so the reasoning switch must be set or Chat
    // Completions rejects the whole request.
    assert_eq!(chat["reasoning_effort"], "none");
}

/// The `systems` and `temperature` and `top_p` fields the proxy emits must
/// survive the trip — the leading system prompt is where the tool policy
/// rewrite lands, so losing it would silently drop a policy.
#[test]
fn to_chat_carries_the_system_prompt_and_drops_anthropic_only_fields() {
    let anthropic = json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 1024,
        "temperature": 0.3,
        "top_p": 0.9,
        "stop_sequences": ["STOP"],
        "system": [{"type": "text", "text": "policy: allow get_weather"}],
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "metadata": {"user_id": "nope"}
    });
    let chat = tab_atelier_proxy::openai::to_chat(&anthropic);

    assert_eq!(
        chat["messages"][0]["role"],
        json!("system"),
        "the system prompt must lead the message list"
    );
    assert_eq!(chat["messages"][0]["content"], json!("policy: allow get_weather"));
    assert_eq!(chat["max_completion_tokens"], json!(1024));
    assert_eq!(chat["temperature"], json!(0.3));
    assert_eq!(chat["top_p"], json!(0.9));
    assert_eq!(chat["stop"], json!(["STOP"]));
    // Anthropic-only knobs have no Chat Completions equivalent and must not
    // be forwarded blindly.
    assert!(chat.get("thinking").is_none(), "thinking leaked: {chat}");
    assert!(chat.get("metadata").is_none(), "metadata leaked: {chat}");
    assert!(chat.get("max_tokens").is_none(), "max_tokens leaked: {chat}");
}
