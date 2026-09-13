// SPDX-License-Identifier: MPL-2.0

//! Throwaway preview harness: run the identity rewrite over a captured body and
//! print the resulting system prompt, so the effect on a real request can be
//! read without a running proxy.

use std::fs;

use serde_json::Value;
use tab_atelier_proxy::identity::{self, Vendor};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let raw = fs::read_to_string(&args[1]).expect("read body");
    let model = args.get(2).map_or("", String::as_str);
    let vendor = match args.get(3).map_or("openai", String::as_str) {
        "deepseek" => Vendor::Deepseek,
        "anthropic" => Vendor::Anthropic,
        _ => Vendor::Openai,
    };
    let mut body: serde_json::Value = serde_json::from_str(&raw).expect("parse body");
    // A captured body may be the outgoing OpenAI shape, where the system prompt
    // is the first message, rather than the Anthropic shape the rewrite runs on.
    // Lift it into the shape `apply` expects.
    if body.get("system").is_none()
        && let Some(Value::String(text)) = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|m| {
                m.iter()
                    .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
            })
            .and_then(|m| m.get("content"))
            .cloned()
    {
        body = serde_json::json!({ "system": text });
    }
    identity::apply(&mut body, vendor, model);
    match body.get("system") {
        Some(serde_json::Value::String(text)) => print!("{text}"),
        Some(serde_json::Value::Array(blocks)) => {
            for block in blocks {
                if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
                    print!("{text}");
                }
            }
        }
        _ => eprintln!("no system prompt"),
    }
}
