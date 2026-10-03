// SPDX-License-Identifier: MPL-2.0

//! Preview the identity rewrite without a running proxy.

use std::path::Path;

use serde_json::Value;
use tab_atelier_proxy::identity::{self, Vendor};

#[derive(Debug)]
enum PreviewError {
    Usage(&'static str),
    Read(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(message) => f.write_str(message),
            Self::Read(error) => write!(f, "read body: {error}"),
            Self::Json(error) => write!(f, "parse body: {error}"),
        }
    }
}

impl std::error::Error for PreviewError {}

fn parse_vendor(name: &str) -> Result<Vendor, PreviewError> {
    match name {
        "openai" => Ok(Vendor::Openai),
        "deepseek" => Ok(Vendor::Deepseek),
        "anthropic" => Ok(Vendor::Anthropic),
        _ => Err(PreviewError::Usage(
            "vendor must be one of: openai, deepseek, anthropic",
        )),
    }
}

fn lift_openai_system(mut body: Value) -> Value {
    if body.get("system").is_none()
        && let Some(Value::String(text)) = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| {
                messages
                    .iter()
                    .find(|message| message.get("role").and_then(Value::as_str) == Some("system"))
            })
            .and_then(|message| message.get("content"))
            .cloned()
    {
        let object = body.as_object_mut().expect("JSON body is an object");
        object.insert("system".to_owned(), Value::String(text));
    }
    body
}

fn preview_body(raw: &str, model: &str, vendor: Vendor) -> Result<String, PreviewError> {
    let body = serde_json::from_str::<Value>(raw).map_err(PreviewError::Json)?;
    let mut body = lift_openai_system(body);
    identity::apply(&mut body, vendor, model);

    let mut output = String::new();
    match body.get("system") {
        Some(Value::String(text)) => output.push_str(text),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    output.push_str(text);
                }
            }
        }
        _ => {}
    }
    Ok(output)
}

fn preview_file(path: &Path, model: &str, vendor: Vendor) -> Result<String, PreviewError> {
    let raw = std::fs::read_to_string(path).map_err(PreviewError::Read)?;
    preview_body(&raw, model, vendor)
}

const fn usage() -> &'static str {
    "usage: preview <body.json> [model] [openai|deepseek|anthropic]"
}

fn run(args: &[String]) -> Result<String, PreviewError> {
    let path = args.get(1).ok_or(PreviewError::Usage(usage()))?;
    let model = args.get(2).map_or("", String::as_str);
    let vendor = parse_vendor(args.get(3).map_or("openai", String::as_str))?;
    preview_file(Path::new(path), model, vendor)
}

fn main() {
    match run(&std::env::args().collect::<Vec<_>>()) {
        Ok(output) if output.is_empty() => eprintln!("no system prompt"),
        Ok(output) => print!("{output}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_arguments_return_usage() {
        let args = vec!["preview".to_owned()];
        assert!(matches!(run(&args), Err(PreviewError::Usage(_))));
    }

    #[test]
    fn unknown_vendor_is_rejected() {
        assert!(matches!(parse_vendor("typo"), Err(PreviewError::Usage(_))));
    }

    #[test]
    fn anthropic_string_system_prompt_is_previewed() {
        let body = r#"{"system":"hello"}"#;
        assert_eq!(preview_body(body, "m", Vendor::Anthropic).expect("preview"), "hello");
    }

    #[test]
    fn anthropic_block_system_prompt_is_concatenated() {
        let body = r#"{"system":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}"#;
        assert_eq!(preview_body(body, "m", Vendor::Anthropic).expect("preview"), "onetwo");
    }

    #[test]
    fn openai_system_message_is_lifted_before_rewriting() {
        let body = r#"{"messages":[{"role":"system","content":"hello"}]}"#;
        assert_eq!(preview_body(body, "m", Vendor::Openai).expect("preview"), "hello");
    }

    #[test]
    fn a_missing_system_prompt_is_an_empty_preview() {
        let body = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
        assert!(preview_body(body, "m", Vendor::Openai).expect("preview").is_empty());
    }

    #[test]
    fn malformed_json_is_reported() {
        assert!(matches!(
            preview_body("{", "m", Vendor::Openai),
            Err(PreviewError::Json(_))
        ));
    }

    #[test]
    fn missing_file_is_reported() {
        let path = std::env::temp_dir().join(format!("preview-missing-{}", std::process::id()));
        assert!(matches!(
            preview_file(&path, "m", Vendor::Openai),
            Err(PreviewError::Read(_))
        ));
    }

    #[test]
    fn an_openai_system_message_is_not_duplicated() {
        let body = r#"{"system":"existing","messages":[{"role":"system","content":"ignored"}]}"#;
        assert_eq!(preview_body(body, "m", Vendor::Openai).expect("preview"), "existing");
    }
}
