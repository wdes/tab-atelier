// SPDX-License-Identifier: MPL-2.0

//! Configurable tools: removing the built-in ones, and adding your own.
//!
//! Before this, the agent's tool set was a `vec![…]` literal in
//! [`crate::tools::builtin_specs`] and a `match` in `dispatch`, so changing it
//! meant a rebuild. An operator could not take away `Bash`, and could not add
//! `GitStatus` without compiling one.
//!
//! # Resolved once, at startup
//!
//! [`ToolSet::load`] runs in `main` and the resolved value is passed by
//! reference. It is deliberately **not** re-read per request, and that is a
//! cache decision as much as a design one: the tool array is the first element
//! of an Anthropic body, so an array that changes mid-session invalidates every
//! cache breakpoint behind it. A config file that could change while the agent
//! runs would reintroduce exactly the defect the rest of this change removes.
//!
//! # No shell
//!
//! A custom tool runs `argv` directly through `exec`, with parameters
//! substituted as **whole arguments**. Nothing is ever passed to `sh -c`, so a
//! parameter containing `;`, `$(…)`, backticks or a newline reaches the child
//! as literal text in a single argument. That is the property that makes this
//! safe to expose to a model, and [`ToolSet::expand`] is tested against it
//! rather than trusted.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool defined in the config file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CustomTool {
    /// The name the model calls.
    pub name: String,
    /// What the model is told it does. Worth writing carefully: this is the
    /// only thing the model has to decide when to reach for it.
    pub description: String,
    /// The JSON Schema for its arguments, `input_schema` shape.
    pub schema: Value,
    /// The command, with `{param}` placeholders substituted per argument.
    ///
    /// Each element becomes exactly one argv entry. `["git", "log", "-n",
    /// "{count}"]` runs `git log -n 5` for `count = 5`, and a `count` of
    /// `"5; rm -rf /"` runs `git log -n '5; rm -rf /'` — one argument, no shell.
    pub argv: Vec<String>,
    /// How long it may run before it is killed.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Whether auto mode grades it before running.
    ///
    /// Defaults to **true**, because the safe default for something that
    /// executes is that it is checked. A tool that only reads — the majority of
    /// what anyone would add — should say `"judged": false` explicitly, which is
    /// the right way round: the config author states a lower risk rather than
    /// the code assuming one.
    #[serde(default = "yes")]
    pub judged: bool,
}

/// The config file.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ToolConfig {
    /// Built-in tools to withhold. `["Bash"]` removes shell access entirely.
    #[serde(default)]
    pub disable: Vec<String>,
    /// When set, the ONLY built-in tools kept. Mutually exclusive with an
    /// unrelated `disable`, though listing a tool in both is harmless — it is
    /// dropped either way.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    /// Tools added on top.
    #[serde(default)]
    pub add: Vec<CustomTool>,
}

const fn default_timeout() -> u64 {
    30
}

const fn yes() -> bool {
    true
}

/// The tools this agent will offer, and how to run them.
///
/// Built once. Holding the resolved specs alongside the custom definitions
/// means `dispatch` never has to re-derive either.
#[derive(Debug, Clone)]
pub struct ToolSet {
    specs: Vec<Value>,
    custom: BTreeMap<String, CustomTool>,
}

impl ToolSet {
    /// Every built-in tool, with nothing disabled.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            specs: crate::tools::builtin_specs(),
            custom: BTreeMap::new(),
        }
    }

    /// Resolve the tool set from a config file, or the built-ins when none.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, is not valid JSON, names a tool in `add`
    /// that already exists, or defines a custom tool with an empty `argv`. All
    /// of these are startup failures: a tool set that silently differs from
    /// what the operator wrote is worse than one that refuses to start, because
    /// the model will be told it has a tool that behaves otherwise.
    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self::builtin());
        };
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("tools config {} could not be read: {e}", path.display()))?;
        let config: ToolConfig = serde_json::from_str(&raw)
            .map_err(|e| format!("tools config {} is not valid JSON: {e}", path.display()))?;
        Self::from_config(config)
    }

    /// Build from an already-parsed config.
    ///
    /// # Errors
    ///
    /// As [`Self::load`], minus the file handling.
    pub fn from_config(config: ToolConfig) -> Result<Self, String> {
        // Destructured rather than used through `config`, which is what clippy
        // means by "passed by value but not consumed": taking ownership and
        // then only borrowing reads as a mistake, and destructuring makes the
        // three pieces this actually works from explicit.
        let ToolConfig { disable, allow, add } = config;
        let mut custom = BTreeMap::new();
        for tool in add {
            if tool.name.trim().is_empty() {
                return Err("a custom tool has no name".into());
            }
            if tool.argv.is_empty() {
                return Err(format!("custom tool {:?} has an empty argv", tool.name));
            }
            // An `{name?}` is only understood when it is the entire element,
            // because that is the only shape where "drop it" is unambiguous.
            // Embedded in a longer element — `-n{count?}` — dropping would lose
            // the flag too and keeping would pass a bare `-n`, so it is refused
            // where an operator will see it rather than misread at runtime.
            for element in &tool.argv {
                if element.contains("?}") && Self::optional_name(element).is_none() {
                    return Err(format!(
                        "custom tool {:?} has {element:?}, but an optional placeholder must be the \
                         whole argument — write \"{{name?}}\" as its own argv entry",
                        tool.name
                    ));
                }
            }
            if custom.contains_key(&tool.name) {
                return Err(format!("custom tool {:?} is defined twice", tool.name));
            }
            custom.insert(tool.name.clone(), tool.clone());
        }

        let disabled: std::collections::BTreeSet<&str> = disable.iter().map(String::as_str).collect();
        let allowed: Option<std::collections::BTreeSet<&str>> =
            allow.as_ref().map(|list| list.iter().map(String::as_str).collect());

        let mut specs: Vec<Value> = crate::tools::builtin_specs()
            .into_iter()
            .filter(|spec| {
                let Some(name) = spec.get("name").and_then(Value::as_str) else {
                    return false;
                };
                if disabled.contains(name) {
                    return false;
                }
                allowed.as_ref().is_none_or(|keep| keep.contains(name))
            })
            .collect();

        // A custom tool may not shadow a built-in that survived the filter.
        // Replacing `Read` with something else would mean the model's
        // understanding of a familiar name is wrong, which is worse than a
        // refusal to start.
        for tool in custom.values() {
            if specs
                .iter()
                .any(|s| s.get("name").and_then(Value::as_str) == Some(tool.name.as_str()))
            {
                return Err(format!(
                    "custom tool {:?} would shadow a built-in tool of the same name; \
                     disable the built-in first or pick another name",
                    tool.name
                ));
            }
            specs.push(serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.schema,
            }));
        }

        // Sorted by name, always. The array's order is content for cache
        // purposes, so it has to be a function of the config rather than of
        // `BTreeMap` iteration or the order an operator happened to type.
        specs.sort_by(|a, b| {
            let key = |v: &Value| v.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            key(a).cmp(&key(b))
        });

        Ok(Self { specs, custom })
    }

    /// The specs to send, in a stable order.
    #[must_use]
    pub fn specs(&self) -> &[Value] {
        &self.specs
    }

    /// Whether a tool name is one this set offers.
    #[must_use]
    pub fn offers(&self, name: &str) -> bool {
        self.custom.contains_key(name)
            || self
                .specs
                .iter()
                .any(|s| s.get("name").and_then(Value::as_str) == Some(name))
    }

    /// Whether auto mode should grade this tool before running it.
    ///
    /// Built-in write tools always. A custom tool when it says so, which
    /// defaults to yes.
    ///
    /// A tool this set does not offer is never judged, whatever its name would
    /// mean elsewhere: [`ToolSet::from_config`] may have disabled `Bash`, and
    /// answering "yes, grade it" for a tool the model cannot call would be a
    /// true statement about a name rather than about this set.
    #[must_use]
    pub fn changes_the_world(&self, name: &str) -> bool {
        if !self.offers(name) {
            return false;
        }
        if let Some(tool) = self.custom.get(name) {
            return tool.judged;
        }
        crate::tools::changes_the_world(name)
    }

    /// Substitute `{param}` placeholders into `argv`.
    ///
    /// Each argv element is produced whole: a placeholder that is an entire
    /// element is replaced by the value, and one embedded in a longer element
    /// (`--count={count}`) is spliced into it. Either way the result is a single
    /// argument — there is no point at which a value becomes syntax, because
    /// nothing here ever builds a command line for a shell to read.
    ///
    /// A placeholder with no matching argument is an error rather than an empty
    /// string: silently passing `""` where a path was expected produces a
    /// command that runs against the wrong thing.
    ///
    /// A placeholder written `{name?}` is **optional**: when the argument is
    /// absent the whole argv element is dropped. That is what makes
    /// `["cargo", "test", "{filter?}"]` run plain `cargo test` when no filter
    /// was given, which is the common case — without it the model would have to
    /// supply a filter it does not have, or the element would expand to the
    /// empty string and cargo would receive a blank argument.
    ///
    /// The element must be *exactly* the optional placeholder. `-n{count?}`
    /// is refused at config load, because dropping that element would lose the
    /// flag while keeping nothing, and keeping it would pass a bare `-n`.
    fn expand(tool: &CustomTool, input: &Value) -> Result<Vec<String>, String> {
        let mut argv = Vec::with_capacity(tool.argv.len());
        for element in &tool.argv {
            // The optional form first, because it is the whole element or
            // nothing at all: `{name?}` becomes exactly the value, or the
            // element is dropped.
            if let Some(name) = Self::optional_name(element) {
                if let Some(value) = input.get(name).filter(|v| !v.is_null()) {
                    argv.push(Self::value_to_argument(&tool.name, name, value)?);
                }
                continue;
            }
            // Otherwise every placeholder is required. Substitution is textual
            // within the element, so `--max-count={count}` becomes
            // `--max-count=5` as one argument.
            let mut out = String::new();
            let mut rest = element.as_str();
            while let Some(open) = rest.find('{') {
                let Some(close) = rest[open..].find('}') else {
                    // An unmatched `{` is literal text, as in a shell glob.
                    break;
                };
                out.push_str(&rest[..open]);
                let name = &rest[open + 1..open + close];
                let value = input
                    .get(name)
                    .ok_or_else(|| format!("tool {:?} needs an argument {name:?} that was not supplied", tool.name))?;
                out.push_str(&Self::value_to_argument(&tool.name, name, value)?);
                rest = &rest[open + close + 1..];
            }
            out.push_str(rest);
            argv.push(out);
        }
        Ok(argv)
    }

    /// The name inside an optional placeholder, when the whole element is one.
    ///
    /// `"{filter?}"` → `Some("filter")`. Anything else — `"-n{count?}"`, a
    /// placeholder embedded in text — is `None`, and validation refuses it at
    /// startup rather than dropping the flag while keeping nothing.
    fn optional_name(element: &str) -> Option<&str> {
        element
            .strip_prefix('{')?
            .strip_suffix("?}")?
            .split('}')
            .next()
            .filter(|name| !name.is_empty())
    }

    /// One JSON value as a single argv string.
    ///
    /// Strings pass through unchanged. Numbers and booleans are stringified,
    /// because a schema may reasonably declare `"count": {"type": "integer"}` and
    /// the model will send `5`, not `"5"`. Anything else is refused rather than
    /// rendered: there is no single argument an object or an array could mean, and
    /// inventing one — `[object Object]`, a comma-joined list — would be a quiet
    /// way to run a command against the wrong thing.
    fn value_to_argument(tool: &str, name: &str, value: &Value) -> Result<String, String> {
        match value {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            other => Err(format!(
                "tool {tool:?} argument {name:?} must be a string, number or boolean, not {}",
                match other {
                    Value::Null => "null",
                    Value::Array(_) => "an array",
                    _ => "an object",
                }
            )),
        }
    }

    /// The custom tool by that name, if this set has one.
    #[must_use]
    pub(crate) fn custom_tool(&self, name: &str) -> Option<&CustomTool> {
        self.custom.get(name)
    }

    /// Run a custom tool by name.
    ///
    /// Never through a shell — see the module note. The child's stdout is the
    /// result, and a non-zero exit is reported with its stderr so the model can
    /// see what went wrong rather than only that something did.
    pub(crate) async fn run_custom(&self, tool: &CustomTool, input: &Value, cwd: &Path) -> Result<String, String> {
        use tokio::io::AsyncReadExt as _;

        let argv = Self::expand(tool, input)?;
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| format!("tool {:?} has an empty argv", tool.name))?;
        let mut command = tokio::process::Command::new(program);
        command
            .args(rest)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // A tool that outlives its agent is a leak: the child would keep
            // running after a cancelled turn.
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("tool {:?} could not start {program:?}: {e}", tool.name))?;

        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_string(&mut stdout).await;
        }
        if let Some(mut err) = child.stderr.take() {
            let _ = err.read_to_string(&mut stderr).await;
        }
        let status = match tokio::time::timeout(Duration::from_secs(tool.timeout_secs), child.wait()).await {
            Err(_) => {
                // `kill_on_drop` has already signalled it by the time this
                // returns; the child is dropped at the end of the scope.
                return Err(format!(
                    "tool {:?} did not finish within {}s and was stopped",
                    tool.name, tool.timeout_secs
                ));
            }
            Ok(Err(e)) => return Err(format!("tool {:?} could not be waited for: {e}", tool.name)),
            Ok(Ok(status)) => status,
        };

        if status.success() {
            return Ok(truncate_output(&stdout));
        }
        Err(format!(
            "tool {:?} exited with {status}\n{}{}",
            tool.name,
            truncate_output(&stdout),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!("\nstderr:\n{}", truncate_output(&stderr))
            }
        ))
    }
}

/// Cap a tool's output so one command cannot fill the context window.
///
/// The same reasoning as compaction's: an unbounded result is a result that
/// eventually costs more than the conversation it was gathered for. 64 KB is
/// well above any useful command output and well below a context window.
fn truncate_output(text: &str) -> String {
    const MAX: usize = 64 * 1024;
    if text.len() <= MAX {
        return text.to_owned();
    }
    // Cut on a character boundary, and say what was lost so the model knows it
    // is reading a prefix rather than the whole thing.
    let mut end = MAX;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated at {MAX} bytes]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(argv: &[&str]) -> CustomTool {
        CustomTool {
            name: "T".into(),
            description: "d".into(),
            schema: json!({"type": "object"}),
            argv: argv.iter().map(|s| (*s).to_owned()).collect(),
            timeout_secs: 5,
            judged: true,
        }
    }

    fn names(set: &ToolSet) -> Vec<String> {
        set.specs()
            .iter()
            .filter_map(|s| s.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    #[test]
    fn the_default_set_is_every_builtin() {
        let set = ToolSet::builtin();
        assert_eq!(names(&set).len(), 6);
        assert!(set.offers("Bash"));
    }

    #[test]
    fn disable_removes_a_builtin() {
        let set = ToolSet::from_config(ToolConfig {
            disable: vec!["Bash".into()],
            ..ToolConfig::default()
        })
        .expect("valid");
        assert!(!set.offers("Bash"), "Bash must be gone");
        assert!(!names(&set).contains(&"Bash".to_owned()));
        assert!(set.offers("Read"), "the others stay");
        // And it is no longer something auto mode needs to grade, because it
        // cannot be called at all.
        assert!(!set.changes_the_world("Bash"));
    }

    #[test]
    fn allow_keeps_only_what_is_listed() {
        let set = ToolSet::from_config(ToolConfig {
            allow: Some(vec!["Read".into(), "Edit".into()]),
            ..ToolConfig::default()
        })
        .expect("valid");
        // Sorted, because a stable order is a cache property.
        assert_eq!(names(&set), vec!["Edit", "Read"]);
    }

    /// The order of the specs is content: the tool array leads the body, so a
    /// set that reordered itself between runs would invalidate the cache on
    /// every restart.
    #[test]
    fn the_order_does_not_depend_on_how_the_config_was_written() {
        let one = ToolSet::from_config(ToolConfig {
            add: vec![
                CustomTool {
                    name: "Zeta".into(),
                    ..tool(&["true"])
                },
                CustomTool {
                    name: "Alpha".into(),
                    ..tool(&["true"])
                },
            ],
            ..ToolConfig::default()
        })
        .expect("valid");
        let other = ToolSet::from_config(ToolConfig {
            add: vec![
                CustomTool {
                    name: "Alpha".into(),
                    ..tool(&["true"])
                },
                CustomTool {
                    name: "Zeta".into(),
                    ..tool(&["true"])
                },
            ],
            ..ToolConfig::default()
        })
        .expect("valid");
        assert_eq!(names(&one), names(&other));
        let listed = names(&one);
        let mut sorted = listed.clone();
        sorted.sort();
        assert_eq!(listed, sorted);
    }

    #[test]
    fn a_duplicate_or_empty_tool_is_refused_at_startup() {
        let dup = ToolConfig {
            add: vec![
                CustomTool {
                    name: "Same".into(),
                    ..tool(&["true"])
                },
                CustomTool {
                    name: "Same".into(),
                    ..tool(&["true"])
                },
            ],
            ..ToolConfig::default()
        };
        assert!(ToolSet::from_config(dup).is_err());

        let empty = ToolConfig {
            add: vec![CustomTool {
                name: "NoArgs".into(),
                ..tool(&[])
            }],
            ..ToolConfig::default()
        };
        assert!(ToolSet::from_config(empty).is_err(), "an empty argv runs nothing");
    }

    /// Shadowing `Read` would leave the model's understanding of a name it
    /// knows pointing at something else. Refusing to start is the honest
    /// response.
    #[test]
    fn a_custom_tool_may_not_shadow_a_live_builtin() {
        let clash = ToolConfig {
            add: vec![CustomTool {
                name: "Read".into(),
                ..tool(&["cat", "{path}"])
            }],
            ..ToolConfig::default()
        };
        assert!(ToolSet::from_config(clash).is_err());

        // …but once the built-in is disabled the name is free, because nothing
        // is left for it to shadow.
        let fine = ToolConfig {
            disable: vec!["Read".into()],
            add: vec![CustomTool {
                name: "Read".into(),
                ..tool(&["cat", "{path}"])
            }],
            ..ToolConfig::default()
        };
        assert!(ToolSet::from_config(fine).is_ok());
    }

    /// The property that makes custom tools safe to hand a model. This is the
    /// test the module note promises, and it fails the moment anyone reaches
    /// for `sh -c`.
    #[test]
    fn an_argument_is_one_argument_however_hostile_it_looks() {
        let t = tool(&["git", "log", "-n", "{count}"]);
        for hostile in ["5; rm -rf /", "$(whoami)", "`id`", "a\nb", "*", "&& evil"] {
            let argv = ToolSet::expand(&t, &json!({ "count": hostile })).expect("expands");
            assert_eq!(argv.len(), 4, "the shape of argv is fixed by the config");
            assert_eq!(argv[3], hostile, "the value arrives whole and unmodified");
            assert_eq!(argv[0], "git");
            assert_eq!(argv[2], "-n");
        }
    }

    #[test]
    fn a_placeholder_embedded_in_an_element_is_spliced_into_that_one_element() {
        let t = tool(&["rg", "--glob={pattern}", "{path}"]);
        let argv = ToolSet::expand(&t, &json!({ "pattern": "*.rs", "path": "src" })).expect("expands");
        assert_eq!(argv, vec!["rg", "--glob=*.rs", "src"]);
    }

    #[test]
    fn numbers_and_booleans_become_arguments_and_nothing_else_does() {
        let t = tool(&["x", "{n}", "{b}"]);
        assert_eq!(
            ToolSet::expand(&t, &json!({ "n": 5, "b": true })).expect("expands"),
            vec!["x", "5", "true"]
        );
        // An object has no single-argument meaning, so it is refused rather
        // than rendered into something arbitrary like `[object Object]`.
        assert!(ToolSet::expand(&t, &json!({ "n": {"a": 1}, "b": true })).is_err());
        assert!(
            ToolSet::expand(&t, &json!({ "b": true })).is_err(),
            "a missing argument is an error"
        );
    }

    /// `{name?}` is the whole element or nothing, which is what lets
    /// `["cargo", "test", "{filter?}"]` run plain `cargo test` when no filter
    /// was given. Without it the model would have to invent a filter, or the
    /// element would expand to `""` and cargo would receive a blank argument.
    #[test]
    fn an_optional_placeholder_is_dropped_when_absent() {
        let t = tool(&["cargo", "test", "{filter?}"]);

        // Absent, null: dropped, and the command is still well-formed.
        assert_eq!(ToolSet::expand(&t, &json!({})).expect("expands"), vec!["cargo", "test"]);
        assert_eq!(
            ToolSet::expand(&t, &json!({ "filter": null })).expect("expands"),
            vec!["cargo", "test"]
        );

        // Present: substituted as its own argument.
        assert_eq!(
            ToolSet::expand(&t, &json!({ "filter": "caching" })).expect("expands"),
            vec!["cargo", "test", "caching"]
        );

        // And still one argument, however hostile the value.
        assert_eq!(
            ToolSet::expand(&t, &json!({ "filter": "a; rm -rf /" })).expect("expands"),
            vec!["cargo", "test", "a; rm -rf /"]
        );
    }

    /// An optional placeholder must be an entire element. `-n{max?}` is refused
    /// at startup because neither reading is right: dropping it loses the flag,
    /// keeping it passes a bare `-n`.
    #[test]
    fn an_optional_placeholder_must_be_a_whole_argument() {
        let bad = ToolConfig {
            add: vec![CustomTool {
                name: "Bad".into(),
                ..tool(&["git", "log", "-n{max?}"])
            }],
            ..ToolConfig::default()
        };
        let err = ToolSet::from_config(bad).expect_err("should refuse");
        assert!(err.contains("whole argument"), "{err}");

        let fine = ToolConfig {
            add: vec![CustomTool {
                name: "Fine".into(),
                ..tool(&["git", "log", "-n", "{max?}"])
            }],
            ..ToolConfig::default()
        };
        assert!(ToolSet::from_config(fine).is_ok());
    }

    /// A required placeholder is still required — the optional form must not
    /// have relaxed anything.
    #[test]
    fn a_required_placeholder_is_still_required() {
        let t = tool(&["git", "log", "--max-count={max}"]);
        assert!(ToolSet::expand(&t, &json!({})).is_err());
        assert_eq!(
            ToolSet::expand(&t, &json!({ "max": 3 })).expect("expands"),
            vec!["git", "log", "--max-count=3"]
        );
    }

    #[test]
    fn output_is_capped_and_says_so() {
        let short = "ok";
        assert_eq!(truncate_output(short), "ok");
        let long = "x".repeat(70 * 1024);
        let cut = truncate_output(&long);
        assert!(cut.len() < long.len());
        assert!(cut.contains("truncated"), "the model must know it is a prefix");
    }

    /// A custom tool that executes is judged unless it says otherwise, and one
    /// that says otherwise is not.
    #[test]
    fn a_custom_tool_is_judged_by_default() {
        let set = ToolSet::from_config(ToolConfig {
            add: vec![
                CustomTool {
                    name: "Default".into(),
                    ..tool(&["true"])
                },
                CustomTool {
                    name: "Exempt".into(),
                    judged: false,
                    ..tool(&["true"])
                },
            ],
            ..ToolConfig::default()
        })
        .expect("valid");
        assert!(set.changes_the_world("Default"), "safe by default");
        assert!(!set.changes_the_world("Exempt"), "and explicit when not");
        assert!(!set.changes_the_world("Never"), "an unknown tool is not judged");
    }
}
