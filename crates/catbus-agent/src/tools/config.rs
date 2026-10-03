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
    /// The SHA-256 of the program this tool runs, as lowercase hex.
    ///
    /// When set, the file the command names is hashed before the call, and the tool is refused if
    /// it does not match. This closes a real hole rather than a theoretical one. A project's
    /// wrapper scripts live in the checkout the session may write to, so without this a session
    /// could rewrite the script and have the rewritten version run on its very next call — a shell
    /// by another name, whatever `AllowedTools` says. With the digest pinned in *this* config,
    /// which is read once at launch, the rewritable file stops being the thing that decides what
    /// runs: the next call fails, and accepting the change takes an operator editing the config
    /// and relaunching.
    ///
    /// The digest covers the **program** — the first `argv` element — and nothing else. `argv` is
    /// fixed, so there is no argument to pin. A tool that needs several files covered should name
    /// one entry point that calls them, which is also what makes the digest easy to state and to
    /// keep current.
    #[serde(default)]
    pub sha256: Option<String>,
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
    /// Which PHP functions a `PHPUnit` run may not call.
    ///
    /// Absent means the tool's own default, which stops a test from spawning a process at all.
    /// It is here rather than hard-coded because a project whose own suite legitimately shells
    /// out (a PDF converter driving `node`, a hook guard testing `proc_open`) would otherwise be
    /// unable to run its tests through the agent. Setting it pins the list for that project;
    /// setting it empty removes the block, which is the operator saying they accept a test
    /// running arbitrary commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phpunit_disable_functions: Option<Vec<String>>,
}

const fn default_timeout() -> u64 {
    30
}

const fn yes() -> bool {
    true
}

/// Accepted by `--tools-config` in place of a JSON path.
///
/// The minimal set is four short names, and an operator who wants "no shell"
/// on one run should not have to create a file to say so — so
/// `--tools-config minimal` is the shorthand for
/// `{"allow": ["Read", "Write", "FileTree", "Grep"]}`. The set itself lives in
/// [`super::MINIMAL_TOOLS`], so the keyword and the documented list cannot
/// drift.
pub const MINIMAL_KEYWORD: &str = "minimal";

/// The config `--tools-config minimal` stands for.
///
/// Exposed rather than inlined into [`load`] so a caller building a set in
/// code asks for the same thing the keyword does, instead of re-listing the
/// names and inventing a second definition of "minimal".
#[must_use]
pub fn minimal_config() -> ToolConfig {
    ToolConfig {
        allow: Some(super::MINIMAL_TOOLS.iter().map(|name| (*name).to_owned()).collect()),
        ..ToolConfig::default()
    }
}

/// The tools this agent will offer, and how to run them.
///
/// Built once. Holding the resolved specs alongside the custom definitions
/// means `dispatch` never has to re-derive either.
#[derive(Debug, Clone)]
pub struct ToolSet {
    specs: Vec<Value>,
    custom: BTreeMap<String, CustomTool>,
    /// The operator's limits on where `SSH` may connect, from the identity file's front matter.
    ///
    /// On the tool set rather than checked in `main`, because the dispatcher is a method here and this
    /// is the only piece of policy a tool itself has to consult mid-call. See `ssh::Policy` for why
    /// the two lists' absences mean different things.
    policy: crate::tools::ssh::Policy,
    /// PHP functions a `PHPUnit` run may not call, from the config.
    ///
    /// `None` means the tool's own default. Held on the set for the same reason the SSH
    /// policy is: the dispatcher is a method here, and this is a limit a tool has to be
    /// handed rather than one it can look up.
    phpunit_functions: Option<Vec<String>>,
}

/// The name of the project-local tool config, under `<cwd>/.catbus/`.
///
/// Discovered rather than named by the launcher, which is the whole point: a project
/// describes its own tools, and the operator running a session in it should not have to
/// know that, any more than they have to pass the identity.
pub const PROJECT_TOOL_FILE: &str = "tools.toml";

impl ToolSet {
    /// Every built-in tool, with nothing disabled.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            specs: crate::tools::builtin_specs(),
            custom: BTreeMap::new(),
            policy: crate::tools::ssh::Policy::default(),
            phpunit_functions: None,
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
        // The shorthand, checked before touching the filesystem: an operator
        // asking for `minimal` means the keyword whether or not a file of that
        // name happens to exist, and silently reading `./minimal` instead would
        // be a surprise in exactly the case where being explicit matters.
        if path.as_os_str() == MINIMAL_KEYWORD {
            return Self::from_config(minimal_config());
        }
        Self::from_config(Self::read(path)?)
    }

    /// Resolve the tool set from the launcher's config and the project's, layered.
    ///
    /// `launcher` is what `--tools-config` named — a path, the `minimal` keyword, or
    /// nothing; `cwd` is the directory the session runs in. Three things can contribute:
    /// the built-ins, the launcher's config, and `<cwd>/.catbus/tools.toml`.
    ///
    /// The project's file is *discovered* rather than named, for the reason the identity
    /// file is: a project describes the tools its own work needs, and the operator
    /// starting a session in it should not have to know to pass a second flag. It may
    /// **take away** a built-in (`disable`) and **add** tools of its own (`add`).
    ///
    /// It cannot widen. A project's `allow` is **refused** rather than ignored, because a
    /// second whitelist would make "which tools exist" the intersection of two lists the
    /// operator cannot see at once — and the identity file's `AllowedTools` is the
    /// instrument for narrowing the whole set, custom tools included.
    ///
    /// It *can* exempt a tool of its own from the auto-mode judge, since the file is the
    /// operator's; that is logged, not silently allowed. The controls that hold whatever this
    /// file says are `AllowedTools` — narrow-only, so a tool still has to be named to exist at
    /// all — and the session's gate mode.
    ///
    /// `minimal` is exempt: it is a deliberate lockdown (`Spawn`'s default, and how an
    /// operator says "no shell"), so a project file does not get to add to it.
    ///
    /// # Errors
    ///
    /// As [`Self::load`], plus a project file that cannot be read, parses as neither
    /// format its name promises, or collides with the launcher's own tools.
    pub fn load_layered(launcher: Option<&Path>, cwd: &Path) -> Result<Self, String> {
        if launcher.is_some_and(|p| p.as_os_str() == MINIMAL_KEYWORD) {
            log::info!(
                "tools: the minimal set (--tools-config minimal); a working directory's own \
                 tools config is not read"
            );
            return Self::load(launcher);
        }
        let launcher_name = launcher.map_or_else(|| "--tools-config".to_owned(), |p| p.display().to_string());
        let launcher_config = launcher.map(Self::read).transpose()?;
        let project_path = crate::identity::project_dir(cwd).join(PROJECT_TOOL_FILE);
        let project_config = Self::read_if_present(&project_path)?;
        if project_config.is_some() {
            log::info!("tools taken from {}", project_path.display());
        }
        let config = match (launcher_config, project_config) {
            (Some(base), Some(over)) => Self::merge(base, &launcher_name, over, &project_path.display().to_string())?,
            (Some(only), None) | (None, Some(only)) => only,
            // Neither file: the built-ins as such, unsorted, exactly as `load(None)` gives.
            (None, None) => return Ok(Self::builtin()),
        };
        Self::from_config(config)
    }

    /// Read and parse a config file, choosing the format by its extension.
    ///
    /// See [`Self::parse`] for the rule and why a file whose extension says nothing is tried
    /// both ways rather than sniffed.
    fn read(path: &Path) -> Result<ToolConfig, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("tools config {} could not be read: {e}", path.display()))?;
        Self::parse(&raw, path)
    }

    /// As [`Self::read`], but a missing file is `None` rather than an error.
    ///
    /// For the discovered project file only: not having one is the ordinary case, while a
    /// file that is present but unreadable is not — the same distinction the identity
    /// search draws.
    fn read_if_present(path: &Path) -> Result<Option<ToolConfig>, String> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Self::parse(&raw, path).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("tools config {} could not be read: {e}", path.display())),
        }
    }

    /// Parse a config body, the format decided by the extension.
    ///
    /// `.toml` is read as TOML and `.json` as JSON, so a file that parses as neither complains
    /// about the one format its name promised — a JSON body in a `.toml` file would otherwise
    /// load as something the operator did not write.
    ///
    /// A path with no extension to go on is tried as JSON first, because every config that
    /// existed before TOML was supported was JSON and an old path is exactly that, then as
    /// TOML. Both failures are reported together, so the message does not send the operator to
    /// the wrong format.
    fn parse(raw: &str, path: &Path) -> Result<ToolConfig, String> {
        match path.extension().and_then(std::ffi::OsStr::to_str) {
            Some("toml") => {
                toml::from_str(raw).map_err(|e| format!("tools config {} is not valid TOML: {e}", path.display()))
            }
            Some("json") => Self::from_json(raw, path),
            _ => match Self::from_json(raw, path) {
                Ok(config) => Ok(config),
                Err(json_error) => toml::from_str(raw)
                    .map_err(|toml_error| format!("{json_error}, and not valid TOML either: {toml_error}")),
            },
        }
    }

    /// The JSON arm, split out so its message is written once.
    fn from_json(raw: &str, path: &Path) -> Result<ToolConfig, String> {
        serde_json::from_str(raw).map_err(|e| format!("tools config {} is not valid JSON: {e}", path.display()))
    }

    /// Layer a project's config over the launcher's, into the one config that is built.
    ///
    /// Each field has a direction, and the direction is the point:
    ///
    /// - `disable` is the **union**. Either side withholds a built-in; neither can un-withhold.
    /// - `allow` is the **launcher's alone**. A project that carries one is refused — see
    ///   [`Self::load_layered`].
    /// - `add` is the launcher's tools followed by the project's, with a name either both
    ///   define or that a built-in already holds refused rather than resolved.
    /// - the PHP function list is the project's when it sets one, since the project is the
    ///   one whose tests are being run.
    ///
    /// The two names are for the refusals: a collision the operator cannot locate is one they
    /// cannot fix.
    fn merge(base: ToolConfig, base_name: &str, over: ToolConfig, over_name: &str) -> Result<ToolConfig, String> {
        let ToolConfig {
            mut disable,
            allow,
            mut add,
            phpunit_disable_functions,
        } = base;
        let ToolConfig {
            disable: more_disable,
            allow: over_allow,
            add: more_add,
            phpunit_disable_functions: over_functions,
        } = over;

        if over_allow.is_some() {
            return Err(format!(
                "{over_name} sets `allow`, which only the launcher's --tools-config may set — \
                 withhold a built-in here with `disable`, and narrow the whole set with \
                 AllowedTools in the identity file"
            ));
        }

        let builtins = crate::tools::builtin_specs();
        let is_builtin = |name: &str| {
            builtins
                .iter()
                .any(|s| s.get("name").and_then(Value::as_str) == Some(name))
        };

        for tool in &more_add {
            // Refused rather than resolved: whichever file lost would silently run argv the
            // other wrote, under a name the operator wrote for something else.
            if add.iter().any(|t| t.name == tool.name) {
                return Err(format!(
                    "custom tool {:?} is defined in both {base_name} and {over_name} — rename one",
                    tool.name
                ));
            }
            // A project may not take a built-in's name, least of all one the launcher just
            // withheld: the model's idea of `Bash` would then be wrong, which is the confusion
            // the same check inside one file already refuses.
            if is_builtin(&tool.name) {
                return Err(format!(
                    "{over_name} defines a tool named {:?}, which is a built-in; pick another name",
                    tool.name
                ));
            }
            // Exempting a capability from the judge is the operator's to give — this file is
            // theirs, and a project file is the natural place for it — but it is *announced*,
            // because the file sits in a directory the agent can write and an exemption that
            // appeared there without the operator noticing would be one they never granted.
            // The controls that do not depend on this file are `AllowedTools` (narrow-only, so
            // a tool still has to be named to exist) and the session's gate mode.
            if !tool.judged {
                log::warn!(
                    "{over_name} adds {:?} with `judged: false`: it will run ungraded in auto mode",
                    tool.name
                );
            }
        }

        // `disable` filters built-ins, so naming a custom tool does nothing at all. Say so,
        // rather than accept an entry that has no effect and looks like it did.
        for name in &more_disable {
            if add.iter().any(|t| t.name == *name) {
                return Err(format!(
                    "{over_name} disables {name:?}, which {base_name} adds as a custom tool — \
                     `disable` withholds built-ins; withhold a custom tool with AllowedTools in \
                     the identity file"
                ));
            }
        }

        disable.extend(more_disable);
        add.extend(more_add);
        Ok(ToolConfig {
            disable,
            allow,
            add,
            phpunit_disable_functions: over_functions.or(phpunit_disable_functions),
        })
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
        let ToolConfig {
            disable,
            allow,
            add,
            phpunit_disable_functions,
        } = config;
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
                "input_schema": with_worktree(tool.schema.clone()),
            }));
        }

        // Sorted by name, always. The array's order is content for cache
        // purposes, so it has to be a function of the config rather than of
        // `BTreeMap` iteration or the order an operator happened to type.
        specs.sort_by(|a, b| {
            let key = |v: &Value| v.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            key(a).cmp(&key(b))
        });

        Ok(Self {
            specs,
            custom,
            // A set built from a config file has no host policy of its own: the identity
            // file supplies one, through `with_allowed_hosts`.
            policy: crate::tools::ssh::Policy::default(),
            phpunit_functions: phpunit_disable_functions,
        })
    }

    /// The PHP functions a `PHPUnit` run may not call, if the config set any.
    ///
    /// `None` leaves it to the tool's own default. See the field.
    #[must_use]
    pub fn phpunit_disable_functions(&self) -> Option<&[String]> {
        self.phpunit_functions.as_deref()
    }

    /// Apply the operator's SSH limits.
    ///
    /// Called once, with the identity file's parsed front matter. Taking the whole policy rather than
    /// two arguments means a caller cannot apply one list and forget the other.
    #[must_use]
    pub fn with_ssh_policy(mut self, policy: crate::tools::ssh::Policy) -> Self {
        self.policy = policy;
        self
    }

    /// The SSH limits in force. See the field.
    #[must_use]
    pub const fn ssh_policy(&self) -> &crate::tools::ssh::Policy {
        &self.policy
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

    /// Every name this set offers, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.specs
            .iter()
            .filter_map(|s| s.get("name").and_then(Value::as_str))
            .collect()
    }

    /// Keep only the named tools, or fail saying what is available.
    ///
    /// Narrow-only by construction: a name this set does not already offer is an
    /// error rather than something to add. That direction matters because the
    /// caller is a *prompt file*, and a prompt file must not be able to grant a
    /// tool the launcher's `--tools-config` withheld — `AllowedTools` is a ceiling,
    /// not a request.
    ///
    /// An unknown name is an error rather than a silent drop, for the same reason
    /// a misspelled `--identity-file` is: a permission list with a typo in it
    /// would otherwise narrow to something the operator did not write, and say
    /// nothing about it.
    pub fn narrowed_to(&self, allowed: &[String]) -> Result<Self, String> {
        let available = self.names();
        let mut unknown: Vec<&str> = allowed
            .iter()
            .map(String::as_str)
            .filter(|name| !self.offers(name))
            .collect();
        if !unknown.is_empty() {
            unknown.sort_unstable();
            unknown.dedup();
            // "the launcher's set" rather than "this set", because the identity file is shared: the
            // same line is reported by a spawned child whose own set is far smaller, and a reader
            // then takes the list for their own tools. What is listed is the set being narrowed
            // *from*, which for the child is its parent's.
            return Err(format!(
                "unknown tool(s) in AllowedTools: {} — the launcher's set offers {}",
                unknown.join(", "),
                available.join(", ")
            ));
        }

        let narrowed = self.retaining(&allowed.iter().map(String::as_str).collect());
        if narrowed.specs.is_empty() {
            // Reachable only by intersecting two sources that share no tool. An
            // agent with no tools can do nothing at all, so it is a configuration
            // conflict to report rather than a state to run in.
            return Err(format!(
                "AllowedTools leaves no tools at all — it names {} but the launcher's set offers {}",
                allowed.join(", "),
                available.join(", ")
            ));
        }
        Ok(narrowed)
    }

    /// Keep the named tools this set already offers, and report the ones it could not grant.
    ///
    /// The lenient counterpart of [`Self::narrowed_to`], for a **sub-agent** — a child a tool call
    /// started. A child's tool set was chosen narrower by its parent before it ran, so a name in the
    /// identity file that the child does not offer cannot widen anything by being ignored: for a
    /// child the ceiling is vacuous rather than violated, and the names it lacks are simply not
    /// granted.
    ///
    /// Refusing instead is what made `Spawn` unusable in any project whose `.catbus/identity.md`
    /// names tools a `minimal` child does not have. The child died at startup applying a permission
    /// list written for the full session, and its parent was told the *tool set* was wrong — when
    /// what was wrong was reading a session's list as a child's contract. The failure is returned
    /// rather than logged here so the caller can name both sides of it.
    ///
    /// A result with no tools in it is returned as such rather than refused: the caller is the one
    /// that knows whether an agent with nothing can be useful, and for a child the answer is that it
    /// is not.
    pub fn capped_to(&self, allowed: &[String]) -> (Self, Vec<String>) {
        let mut ungranted: Vec<String> = allowed.iter().filter(|name| !self.offers(name)).cloned().collect();
        ungranted.sort_unstable();
        ungranted.dedup();
        (self.retaining(&allowed.iter().map(String::as_str).collect()), ungranted)
    }

    /// The filtering both of the two above share, so they cannot drift apart.
    ///
    /// Shared rather than written twice because this is a permission list: a `capped_to` that kept a
    /// different set from `narrowed_to` on the same input would grant in one path what the other
    /// refuses, and that is a divergence nobody would notice until it mattered.
    fn retaining(&self, keep: &std::collections::BTreeSet<&str>) -> Self {
        let specs: Vec<Value> = self
            .specs
            .iter()
            .filter(|spec| {
                spec.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| keep.contains(name))
            })
            .cloned()
            .collect();

        let custom = self
            .custom
            .iter()
            .filter(|(name, _)| keep.contains(name.as_str()))
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect();

        Self {
            specs,
            custom,
            // Carried through narrowing, so restricting the tools cannot silently drop the host
            // limit — the two are set independently and neither implies the other.
            policy: self.policy.clone(),
            // And the PHP function list for the same reason: `AllowedTools` is about which tools
            // exist, not about what a surviving one may do.
            phpunit_functions: self.phpunit_functions.clone(),
        }
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
        // A tool whose action decides cannot be answered by its name: `Git show` reads and `Git push`
        // writes; `Plouf files` reads and `Plouf index` writes. Answered as writing here — the safe
        // direction, since judging a read costs one judge call where failing to judge a write means it
        // happens unexamined. [`Self::call_changes_the_world`] gives the per-call answer.
        if crate::tools::action_decides_writes(name) {
            return true;
        }
        crate::tools::changes_the_world(name)
    }

    /// Whether **this call** changes the world, which for an action-based tool depends on the action.
    ///
    /// The judge site needs this rather than [`Self::changes_the_world`]: judging the tool name alone
    /// would spend a judge call on `git show`, and — worse — judge a push as though it were a read if
    /// the answer went the other way. One tool with eight actions means the question can only be
    /// answered from the input.
    #[must_use]
    pub fn call_changes_the_world(&self, name: &str, input: &serde_json::Value) -> bool {
        if !crate::tools::action_decides_writes(name) {
            return self.changes_the_world(name);
        }
        if !self.offers(name) {
            return false;
        }
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
        crate::tools::action_writes(name, action)
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

        // A custom tool honours `worktree` exactly as the built-ins do. Without this a wrapper
        // always ran in the shared checkout — so a project whose rule is "work in your own
        // worktree" could not follow it through *any* tool it was given, and the rule read as a
        // model failure rather than as a missing parameter.
        let cwd = super::checkout(input, cwd)?;
        let argv = Self::expand(tool, input)?;
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| format!("tool {:?} has an empty argv", tool.name))?;
        if let Some(expected) = tool.sha256.as_deref() {
            verify_sha256(tool, program, &cwd, expected)?;
        }
        let mut command = tokio::process::Command::new(program);
        command
            .args(rest)
            .current_dir(&cwd)
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

/// Where a tool's program actually is, so it can be hashed.
///
/// The three cases the OS itself distinguishes: an absolute path, a path relative to the directory
/// the tool runs in, and a bare name to be looked for on `PATH`. Resolving the third means the
/// check follows the same search the spawn will, rather than hashing something the process would
/// never have run.
fn program_path(program: &str, cwd: &Path) -> Option<std::path::PathBuf> {
    let as_path = Path::new(program);
    if as_path.is_absolute() {
        return Some(as_path.to_path_buf());
    }
    if program.contains(std::path::MAIN_SEPARATOR) {
        return Some(cwd.join(as_path));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// SHA-256 of a file, as lowercase hex.
fn sha256_of(path: &Path) -> std::io::Result<String> {
    use sha2::Digest as _;
    let mut file = std::fs::File::open(path)?;
    // `Sha256` implements `io::Write`, so the file streams through it instead of
    // being read into memory — a script is small, but the habit is the point.
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Refuse a tool whose program is not the file that was approved.
///
/// Fails closed at every step: a program that cannot be found, or read, is refused rather than run
/// unverified. A check that quietly passes when it cannot do its job is worse than no check, since
/// the config author is relying on it.
fn verify_sha256(tool: &CustomTool, program: &str, cwd: &Path, expected: &str) -> Result<(), String> {
    let expected = expected.trim();
    let path = program_path(program, cwd).ok_or_else(|| {
        format!(
            "tool {:?} was refused before it ran: it pins a sha256, but its program {program:?} \
             could not be found to hash. Fix the path, or drop the `sha256` field.",
            tool.name
        )
    })?;
    let found = sha256_of(&path).map_err(|e| {
        format!(
            "tool {:?} was refused before it ran: it pins a sha256, but {} could not be read to \
             hash it ({e}).",
            tool.name,
            path.display()
        )
    })?;
    if found.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    Err(format!(
        "tool {:?} was refused before it ran: {} is not the file that was approved.\n  \
         expected sha256 {expected}\n  \
         found          {found}\n\
         It has changed since the digest was recorded. This check exists so that a rewritten \
         script does not run without someone deciding it should — do not work around it, and do \
         not assume the new contents do what the description says. Report that the file changed. \
         If the change was intended, an operator updates this tool's `sha256` in the tool config \
         and relaunches, which is the decision this check is holding open.",
        tool.name,
        path.display()
    ))
}

/// A custom tool's schema, with the `worktree` property every process-running tool offers.
///
/// The built-ins all carry it (see [`super::checkout_property`]), and a project wrapper that could
/// not would be the one tool a worktree rule could not be followed through — which is the failure
/// this closes. A tool that declares its own `worktree` keeps it: the author's wording is about
/// their own tool, and the property means the same thing either way.
fn with_worktree(mut schema: Value) -> Value {
    let Some(object) = schema.as_object_mut() else {
        // A schema that is not an object is the author's problem, not ours to repair here; leave
        // it exactly as written so the model still sees what they wrote.
        return schema;
    };
    let properties = object.entry("properties").or_insert_with(|| serde_json::json!({}));
    if let Some(map) = properties.as_object_mut() {
        map.entry("worktree").or_insert_with(super::checkout_property);
    }
    schema
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
            sha256: None,
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
        // Exact set, not a count: a count tells you *that* it changed, this
        // tells you *what* changed, and adding a built-in should be a decision
        // someone makes here rather than a side effect of registering a spec.
        // Spawn is the most recent addition (FileTree before it).
        let mut expected = vec![
            "AskUserQuestion",
            "Bash",
            "Bun",
            "Composer",
            "Delegate",
            "Edit",
            "FileTree",
            "Git",
            "Grep",
            "ListAgents",
            "PHPUnit",
            "Plouf",
            "Read",
            "SSH",
            "Spawn",
            "Tasks",
            "Write",
        ];
        expected.sort_unstable();
        let mut actual = names(&set);
        actual.sort_unstable();
        assert_eq!(actual, expected);
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

    /// `AllowedTools` narrows the launch's tool set and can only narrow it.
    ///
    /// This is the property that makes the prompt file a ceiling rather than a
    /// request: a name the launcher's `--tools-config` withheld must be refused,
    /// not granted, or a prompt file could hand itself a tool the operator
    /// deliberately left out.
    #[test]
    fn narrowing_keeps_only_the_named_tools() {
        let set = ToolSet::from_config(ToolConfig {
            allow: Some(vec!["Read".into(), "Edit".into(), "Bash".into()]),
            ..ToolConfig::default()
        })
        .expect("valid");

        let narrowed = set.narrowed_to(&["Read".to_owned()]).expect("Read is offered");
        assert_eq!(names(&narrowed), vec!["Read"]);
        // The original is untouched — the caller may still want the wider set for
        // the log line that explains what was removed.
        assert_eq!(names(&set), vec!["Bash", "Edit", "Read"]);
    }

    /// A name the set does not offer is an error, not a silent drop: a typo in a
    /// permission list would otherwise narrow to something nobody wrote, and say
    /// nothing about it.
    #[test]
    fn narrowing_refuses_a_tool_the_set_does_not_offer() {
        let set = ToolSet::from_config(ToolConfig {
            allow: Some(vec!["Read".into()]),
            ..ToolConfig::default()
        })
        .expect("valid");

        let err = set.narrowed_to(&["Read".to_owned(), "Bash".to_owned()]).unwrap_err();
        assert!(err.contains("Bash"), "the error must name the offender: {err}");
        assert!(err.contains("offers Read"), "and what was available: {err}");
    }

    /// Narrowing to nothing is a configuration conflict, not a runnable state: an
    /// agent with no tools can do nothing, so it is better to refuse at start-up.
    #[test]
    fn narrowing_to_nothing_is_refused() {
        let set = ToolSet::from_config(ToolConfig {
            allow: Some(vec!["Read".into()]),
            ..ToolConfig::default()
        })
        .expect("valid");
        // `allow` was already narrowed, so naming a tool that is not in it is the
        // unknown-name path; an empty list is the empty path.
        let err = set.narrowed_to(&[]).unwrap_err();
        assert!(err.contains("no tools at all"), "{err}");
    }

    /// A sub-agent is capped, not held to the ceiling: the tools it does offer are granted, and the
    /// names it does not offer are reported rather than refused.
    ///
    /// This is the difference that made `Spawn` unusable. A child's set was chosen narrower by its
    /// parent before it ran, so an identity naming a tool the child lacks cannot widen anything — the
    /// list is vacuous for the child, not violated by it. Refusing instead killed the child at
    /// start-up over a permission list written for the full session it was spawned from.
    #[test]
    fn capping_grants_what_is_offered_and_reports_the_rest() {
        let set = ToolSet::from_config(ToolConfig {
            allow: Some(vec!["Read".into(), "Edit".into()]),
            ..ToolConfig::default()
        })
        .expect("valid");

        let (capped, ungranted) = set.capped_to(&[
            "Read".to_owned(),
            "Bash".to_owned(),
            "Spawn".to_owned(),
            "Git".to_owned(),
        ]);
        assert_eq!(names(&capped), vec!["Read"], "the tool it has is granted");
        assert_eq!(
            ungranted,
            vec!["Bash", "Git", "Spawn"],
            "and the ones it does not have are named, sorted, so the caller can say which"
        );
        // The original is untouched. `Edit` was not asked for, so it is not in the result — a cap
        // narrows to what was named, exactly as `narrowed_to` does.
        assert_eq!(names(&set), vec!["Edit", "Read"]);
    }

    /// Capping to nothing is a result, not a refusal — the caller decides whether an agent with no
    /// tools is useful, and for a sub-agent the caller already knows it is not.
    #[test]
    fn capping_to_nothing_returns_an_empty_set_rather_than_an_error() {
        let set = ToolSet::from_config(ToolConfig {
            allow: Some(vec!["Read".into()]),
            ..ToolConfig::default()
        })
        .expect("valid");

        let (capped, ungranted) = set.capped_to(&["Bash".to_owned(), "Git".to_owned()]);
        assert!(capped.specs().is_empty(), "nothing was offered, so nothing is granted");
        assert_eq!(ungranted, vec!["Bash", "Git"], "and both are reported");
    }

    /// The host policy and the PHP function list survive a cap, for the same reason they survive
    /// `narrowed_to`: they are set independently of which tools exist, and neither implies the other.
    ///
    /// Asserted separately from the `narrowed_to` case because the two now share one filter — this
    /// is the test that would catch the shared helper being changed in a way that only one of the
    /// two callers wanted.
    #[test]
    fn capping_keeps_the_policy_and_the_php_functions() {
        let dir = tempfile::tempdir().unwrap();
        let catbus = dir.path().join(".catbus");
        std::fs::create_dir_all(&catbus).unwrap();
        let base = dir.path().join("launcher.toml");
        std::fs::write(&base, r#"phpunit_disable_functions = ["exec"]"#).unwrap();

        let set = ToolSet::load_layered(Some(&base), dir.path()).expect("valid");
        // A launcher config that sets only the function list leaves the full built-in tool set, so
        // `Read` is offered and capped to — and `GiteaPr` stands for the realistic case: a name the
        // identity carries because a project file defines it, which this set has never heard of.
        let (capped, ungranted) = set.capped_to(&["Read".to_owned(), "GiteaPr".to_owned()]);
        assert_eq!(names(&capped), vec!["Read"]);
        assert_eq!(ungranted, vec!["GiteaPr"]);
        assert!(
            capped.phpunit_disable_functions().is_some(),
            "a cap is about which tools exist, not about what a surviving one may do"
        );
    }

    /// The example config the README points at must actually work.
    ///
    /// It did not, and nothing noticed. `examples/tools.json` declared
    /// `--max-count={max?}`, which the validator refuses — an optional placeholder must be a
    /// whole argv element — so loading the documented example failed outright. The test that
    /// exercises custom tools passes its own inline config, which is why the breakage survived:
    /// the file the README calls "a working set" was never loaded by anything.
    ///
    /// A shipped example is a promise, so it is read from disk and validated here.
    #[test]
    fn the_shipped_example_config_loads() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("tools.json");
        let raw = std::fs::read_to_string(&path).expect("the example should exist");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("and be valid JSON");

        // Loading is the real assertion: `load` runs the whole validator, including the rule
        // the example used to break.
        let set = ToolSet::load(Some(&path)).expect("the shipped example must load");

        // And every tool it names survived, so a config cannot load into an empty set.
        for tool in parsed["add"].as_array().expect("an `add` list") {
            let name = tool["name"].as_str().expect("a name");
            assert!(
                set.offers(name),
                "the example declares {name}, which did not survive loading"
            );
        }
        // The custom names must not collide with a built-in, either — the other way this file
        // can stop working as the built-in list grows. Checked through `builtin()` rather than
        // a list of names written out here, so it follows the real set.
        let builtins = ToolSet::builtin();
        for tool in parsed["add"].as_array().expect("an `add` list") {
            let name = tool["name"].as_str().expect("a name");
            assert!(
                !builtins.offers(name),
                "{name} is a built-in's name, so a config adding it would make the agent \
                 refuse to start. Rename it in the example."
            );
        }
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

    /// Every custom tool offers `worktree`, so a wrapper can be pointed at a checkout.
    ///
    /// This mirrors `tools::tests::the_tools_that_run_a_process_offer_a_worktree` for the built-ins.
    /// Without it the one tool a project defines to honour its own "work in your worktree" rule is
    /// the one tool that cannot — and it fails quietly, by running in the shared checkout.
    #[test]
    fn a_custom_tool_offers_the_worktree_property() {
        let config = ToolConfig {
            add: vec![tool(&["git", "status"])],
            ..ToolConfig::default()
        };
        let set = ToolSet::from_config(config).expect("a plain tool is fine");
        let spec = set
            .specs()
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some("T"))
            .expect("the custom tool is in the set");
        let properties = &spec["input_schema"]["properties"];
        assert!(
            properties.get("worktree").is_some(),
            "a custom tool must be offered `worktree`: {spec}"
        );
    }

    /// A tool that declares its own `worktree` keeps the author's wording, not ours.
    #[test]
    fn a_custom_tool_keeping_its_own_worktree_property_is_left_alone() {
        let mut authored = tool(&["git", "status"]);
        authored.schema = json!({
            "type": "object",
            "properties": { "worktree": { "type": "string", "description": "the author's words" } }
        });
        let config = ToolConfig {
            add: vec![authored],
            ..ToolConfig::default()
        };
        let set = ToolSet::from_config(config).expect("fine");
        let spec = set
            .specs()
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some("T"))
            .expect("present");
        assert_eq!(
            spec["input_schema"]["properties"]["worktree"]["description"], "the author's words",
            "the author's description must survive"
        );
    }

    /// A custom tool naming `worktree` runs there, not in the session directory.
    #[tokio::test]
    async fn a_custom_tool_runs_in_the_worktree_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().join("shared");
        std::fs::create_dir_all(&here).unwrap();
        std::fs::write(dir.path().join("cwd.marker"), "cwd").unwrap();
        std::fs::write(here.join("where.marker"), "worktree").unwrap();

        // The command names which directory it started in via a marker file, so the assertion does
        // not depend on canonicalisation of the temp path.
        let set = ToolSet::from_config(ToolConfig {
            add: vec![tool(&["sh", "-c", "test -f where.marker && echo worktree || echo cwd"])],
            ..ToolConfig::default()
        })
        .expect("fine");

        let tool = set.custom_tool("T").expect("the tool is registered");
        let out = set
            .run_custom(tool, &json!({ "worktree": "shared" }), dir.path())
            .await
            .expect("the command runs");
        assert_eq!(out.trim(), "worktree", "the tool ran in the worktree it named");

        let out = set
            .run_custom(tool, &json!({}), dir.path())
            .await
            .expect("the command runs");
        assert_eq!(out.trim(), "cwd", "and in the session directory with none");
    }

    /// `worktree` may not name a place outside the working directory, exactly as for a built-in.
    #[tokio::test]
    async fn a_custom_tool_refuses_a_worktree_outside_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let set = ToolSet::from_config(ToolConfig {
            add: vec![tool(&["true"])],
            ..ToolConfig::default()
        })
        .expect("fine");
        let tool = set.custom_tool("T").expect("registered");
        let err = set
            .run_custom(tool, &json!({ "worktree": "../escape" }), dir.path())
            .await
            .expect_err("an escaping worktree is refused");
        assert!(err.contains(".."), "the refusal must explain itself: {err}");
    }

    /// A small executable, which is what a pinned tool's program is.
    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn pinned(name: &str, digest: Option<String>) -> ToolSet {
        // `./` so the program is found in the directory the tool runs in — the
        // same relative form a project config uses, and the only form that works
        // for a file that is not on `PATH`.
        let program = format!("./{name}");
        let mut t = tool(&[&program]);
        t.sha256 = digest;
        ToolSet::from_config(ToolConfig {
            add: vec![t],
            ..ToolConfig::default()
        })
        .expect("a pinned tool is valid")
    }

    /// A digest that matches lets the tool run, and the path is resolved relative
    /// to the directory the tool runs in.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_tool_pinned_to_its_program_runs_when_the_digest_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "ok.sh", "#!/bin/sh\necho approved\n");
        let digest = sha256_of(&path).unwrap();
        let set = pinned("ok.sh", Some(digest));
        let out = set
            .run_custom(set.custom_tool("T").unwrap(), &json!({}), dir.path())
            .await
            .expect("a matching digest runs");
        assert_eq!(out.trim(), "approved");
    }

    /// The point of the field: the digest is recorded, the file is rewritten
    /// afterwards, and the rewrite does not get to run.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_tool_whose_program_changed_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "swap.sh", "#!/bin/sh\necho approved\n");
        let approved = sha256_of(&path).unwrap();
        // Exactly what a session holding Write could do between two calls.
        std::fs::write(&path, "#!/bin/sh\necho tampered\n").unwrap();

        let set = pinned("swap.sh", Some(approved.clone()));
        let err = set
            .run_custom(set.custom_tool("T").unwrap(), &json!({}), dir.path())
            .await
            .expect_err("a rewritten program is refused");

        assert!(err.contains("not the file that was approved"), "{err}");
        assert!(err.contains(&approved), "the refusal names what was approved: {err}");
        assert!(
            err.contains(&sha256_of(&path).unwrap()),
            "and what is actually there: {err}"
        );
    }

    /// Fail closed: a program that cannot be found at all is refused, not run unverified.
    ///
    /// A check that passes when it cannot do its job is worse than no check,
    /// because whoever wrote the config is relying on it.
    ///
    /// A bare name, so the lookup goes to `PATH` and finds nothing — the branch
    /// where the program cannot even be *named*.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_tool_pinned_to_a_program_that_cannot_be_found_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = tool(&["definitely-not-a-real-program-2f8c1a"]);
        t.sha256 = Some("00".repeat(32));
        let set = ToolSet::from_config(ToolConfig {
            add: vec![t],
            ..ToolConfig::default()
        })
        .unwrap();
        let err = set
            .run_custom(set.custom_tool("T").unwrap(), &json!({}), dir.path())
            .await
            .expect_err("a program that cannot be hashed is refused");
        assert!(err.contains("could not be found to hash"), "{err}");
    }

    /// The same refusal by the other branch: a relative path that names nothing
    /// can be resolved, and then fails to read. Both fail closed, and a caller
    /// should be told which happened.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_tool_pinned_to_a_path_that_does_not_exist_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let set = pinned("absent.sh", Some("00".repeat(32)));
        let err = set
            .run_custom(set.custom_tool("T").unwrap(), &json!({}), dir.path())
            .await
            .expect_err("a file that cannot be read is refused");
        assert!(err.contains("could not be read to hash"), "{err}");
    }

    /// A digest pasted in upper case still matches — the value is hex, not a
    /// spelling.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_digest_recorded_in_upper_case_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = script(dir.path(), "caps.sh", "#!/bin/sh\necho fine\n");
        let set = pinned("caps.sh", Some(sha256_of(&path).unwrap().to_uppercase()));
        let out = set
            .run_custom(set.custom_tool("T").unwrap(), &json!({}), dir.path())
            .await
            .expect("case does not matter");
        assert_eq!(out.trim(), "fine");
    }

    /// A tool with no digest is unaffected — the field is opt-in.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_tool_without_a_digest_runs_as_before() {
        let dir = tempfile::tempdir().unwrap();
        script(dir.path(), "plain.sh", "#!/bin/sh\necho plain\n");
        let set = pinned("plain.sh", None);
        let out = set
            .run_custom(set.custom_tool("T").unwrap(), &json!({}), dir.path())
            .await
            .expect("unpinned tools are not checked");
        assert_eq!(out.trim(), "plain");
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

    /// A file named `.toml` is read as TOML, and the `add` entries come through with the same
    /// shape JSON gives — the point of supporting the format at all.
    #[test]
    fn a_toml_config_loads_the_same_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("tools.toml");
        std::fs::write(
            &toml_path,
            r#"
disable = ["Bash"]

[[add]]
name = "GiteaPr"
description = "Read a pull request."
judged = false
timeout_secs = 120
schema = { type = "object", properties = { pr = { type = "string" } }, required = ["pr"] }
argv = ["scripts/claude-gitea-pr.sh", "--get", "{pr}"]
"#,
        )
        .unwrap();
        let set = ToolSet::load(Some(&toml_path)).expect("valid TOML");
        assert!(!names(&set).iter().any(|n| n == "Bash"), "disable honoured");
        assert!(names(&set).iter().any(|n| n == "GiteaPr"), "custom tool added");
        assert!(!set.changes_the_world("GiteaPr"), "judged: false carried through");
    }

    /// A JSON body in a `.toml` file is refused, and the message says which format the name
    /// promised — a file that loads as the format it did not claim would be a surprise.
    #[test]
    fn the_extension_decides_the_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tools.toml");
        std::fs::write(&path, r#"{"disable": ["Bash"]}"#).unwrap();
        let err = ToolSet::load(Some(&path)).expect_err("JSON in a .toml file");
        assert!(err.contains("TOML"), "{err}");

        // A `.json` file holding TOML gets the JSON complaint.
        let json_path = dir.path().join("tools.json");
        std::fs::write(&json_path, "disable = [\"Bash\"]\n").unwrap();
        let err = ToolSet::load(Some(&json_path)).expect_err("TOML in a .json file");
        assert!(err.contains("JSON"), "{err}");
    }

    /// An extension-less path — the shape every config had before TOML existed — still reads as
    /// JSON, and also as TOML, so neither old path nor a new nameless one is stranded.
    #[test]
    fn a_path_with_no_extension_is_tried_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tools");
        std::fs::write(&path, r#"{"disable": ["Bash"]}"#).unwrap();
        assert!(
            !names(&ToolSet::load(Some(&path)).expect("JSON"))
                .iter()
                .any(|n| n == "Bash")
        );

        std::fs::write(&path, "disable = [\"Bash\"]\n").unwrap();
        assert!(
            !names(&ToolSet::load(Some(&path)).expect("TOML"))
                .iter()
                .any(|n| n == "Bash")
        );

        // And something that is neither names both, so the operator is not sent to the wrong one.
        std::fs::write(&path, "this is not a config").unwrap();
        let err = ToolSet::load(Some(&path)).expect_err("garbage");
        assert!(err.contains("JSON") && err.contains("TOML"), "{err}");
    }

    /// The project file is found without being named, its tools are added, and its `disable` is
    /// honoured — the feature the whole change exists for.
    #[test]
    fn the_project_file_is_discovered_and_adds_tools() {
        let dir = tempfile::tempdir().unwrap();
        let catbus = dir.path().join(".catbus");
        std::fs::create_dir_all(&catbus).unwrap();
        std::fs::write(
            catbus.join(PROJECT_TOOL_FILE),
            r#"
disable = ["Bash"]

[[add]]
name = "GiteaPr"
description = "Read a pull request."
schema = { type = "object", properties = {} }
argv = ["scripts/claude-gitea-pr.sh", "--get", "1"]
"#,
        )
        .unwrap();
        let set = ToolSet::load_layered(None, dir.path()).expect("valid");
        assert!(names(&set).iter().any(|n| n == "GiteaPr"), "project tool added");
        assert!(!names(&set).iter().any(|n| n == "Bash"), "project disable honoured");
    }

    /// `minimal` is a deliberate lockdown, so a project file does not get to add to it: the
    /// keyword is taken at its word and the directory is not read.
    #[test]
    fn minimal_is_not_widened_by_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let catbus = dir.path().join(".catbus");
        std::fs::create_dir_all(&catbus).unwrap();
        std::fs::write(
            catbus.join(PROJECT_TOOL_FILE),
            "[[add]]\nname = \"Sneak\"\ndescription = \"d\"\nschema = { type = \"object\" }\nargv = [\"true\"]\n",
        )
        .unwrap();
        let set = ToolSet::load_layered(Some(Path::new(MINIMAL_KEYWORD)), dir.path()).expect("valid");
        assert!(!names(&set).iter().any(|n| n == "Sneak"), "minimal stayed minimal");
    }

    /// A project file may withhold a built-in and add tools, but may not set `allow`: a second
    /// whitelist would make the live set the intersection of two lists, and `AllowedTools` is the
    /// instrument for narrowing.
    #[test]
    fn a_project_file_cannot_widen_or_whitelist() {
        let dir = tempfile::tempdir().unwrap();
        let catbus = dir.path().join(".catbus");
        std::fs::create_dir_all(&catbus).unwrap();
        let base = dir.path().join("launcher.toml");
        std::fs::write(&base, r#"disable = ["Bash"]"#).unwrap();

        std::fs::write(catbus.join(PROJECT_TOOL_FILE), r#"allow = ["Read"]"#).unwrap();
        let err = ToolSet::load_layered(Some(&base), dir.path()).expect_err("allow is refused");
        assert!(err.contains("allow"), "{err}");

        // No `allow`: the launcher's disable survives and the project's is added to it.
        std::fs::write(catbus.join(PROJECT_TOOL_FILE), r#"disable = ["Bun"]"#).unwrap();
        let set = ToolSet::load_layered(Some(&base), dir.path()).expect("valid");
        let names = names(&set);
        assert!(!names.iter().any(|n| n == "Bash"), "launcher withheld");
        assert!(!names.iter().any(|n| n == "Bun"), "and the project withheld another");
    }

    /// A name defined by both files is refused, and the message names both — a collision the
    /// operator cannot locate is one they cannot fix. So is a project tool named after a built-in.
    #[test]
    fn a_collision_between_the_two_files_is_refused_with_both_names() {
        let dir = tempfile::tempdir().unwrap();
        let catbus = dir.path().join(".catbus");
        std::fs::create_dir_all(&catbus).unwrap();
        let base = dir.path().join("launcher.toml");
        let entry = |name: &str| {
            format!(
                "[[add]]\nname = \"{name}\"\ndescription = \"d\"\nschema = {{ type = \"object\" }}\nargv = [\"true\"]\n"
            )
        };
        std::fs::write(&base, entry("Clash")).unwrap();

        std::fs::write(catbus.join(PROJECT_TOOL_FILE), entry("Clash")).unwrap();
        let err = ToolSet::load_layered(Some(&base), dir.path()).expect_err("collision");
        assert!(err.contains("Clash") && err.contains("launcher.toml"), "{err}");

        // A project tool named for a built-in is refused too: the model's idea of `Bash` would
        // otherwise be a script someone else wrote.
        std::fs::write(catbus.join(PROJECT_TOOL_FILE), entry("Bash")).unwrap();
        let err = ToolSet::load_layered(Some(&base), dir.path()).expect_err("shadows a built-in");
        assert!(err.contains("Bash") && err.contains("built-in"), "{err}");
    }

    /// A `disable` naming a custom tool does nothing — it filters built-ins — so it is refused
    /// rather than accepted as an entry that looks like it had an effect.
    #[test]
    fn disabling_a_custom_tool_is_refused_as_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let catbus = dir.path().join(".catbus");
        std::fs::create_dir_all(&catbus).unwrap();
        let base = dir.path().join("launcher.toml");
        std::fs::write(
            &base,
            "[[add]]\nname = \"Mine\"\ndescription = \"d\"\nschema = { type = \"object\" }\nargv = [\"true\"]\n",
        )
        .unwrap();
        std::fs::write(catbus.join(PROJECT_TOOL_FILE), r#"disable = ["Mine"]"#).unwrap();
        let err = ToolSet::load_layered(Some(&base), dir.path()).expect_err("no-op disable");
        assert!(err.contains("Mine"), "{err}");
    }

    /// The PHP function list rides on the set and survives narrowing, like the host policy: which
    /// tools exist and what a surviving one may do are independent.
    #[test]
    fn the_php_function_list_is_carried_and_is_the_projects() {
        let dir = tempfile::tempdir().unwrap();
        let catbus = dir.path().join(".catbus");
        std::fs::create_dir_all(&catbus).unwrap();
        let base = dir.path().join("launcher.toml");
        std::fs::write(&base, r#"phpunit_disable_functions = ["exec"]"#).unwrap();
        std::fs::write(
            catbus.join(PROJECT_TOOL_FILE),
            r#"phpunit_disable_functions = ["shell_exec", "proc_open"]"#,
        )
        .unwrap();

        let set = ToolSet::load_layered(Some(&base), dir.path()).expect("valid");
        assert_eq!(
            set.phpunit_disable_functions(),
            Some(["shell_exec".to_owned(), "proc_open".to_owned()].as_slice()),
            "the project's tests are the ones being run, so its list wins"
        );

        // `narrowed_to` is for `AllowedTools`, not for this, and must not drop it.
        let narrowed = set.narrowed_to(&["Read".to_owned()]).expect("Read survives");
        assert!(narrowed.phpunit_disable_functions().is_some(), "survives narrowing");

        // Nothing set: the tool's own default applies, which the getter reports as `None`.
        assert_eq!(ToolSet::builtin().phpunit_disable_functions(), None);
    }

    /// The project file is read only when it is there; a broken one is an error, not a silence.
    #[test]
    fn a_missing_project_file_is_fine_and_a_broken_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            ToolSet::load_layered(None, dir.path()).is_ok(),
            "no .catbus, no problem"
        );

        let catbus = dir.path().join(".catbus");
        std::fs::create_dir_all(&catbus).unwrap();
        std::fs::write(catbus.join(PROJECT_TOOL_FILE), "not a config at all").unwrap();
        let err = ToolSet::load_layered(None, dir.path()).expect_err("broken");
        assert!(err.contains(PROJECT_TOOL_FILE), "names the file: {err}");
    }

    /// A top-level key written *after* an `[[add]]` table silently becomes a field of that
    /// tool in TOML, so a mistyped layout loses the setting rather than erroring. This pins the
    /// shape the documents prescribe — top-level keys first — so a config that looks right but
    /// is laid out wrong is caught by the round trip it is meant to survive.
    #[test]
    fn a_top_level_key_after_a_table_is_the_trap_it_looks_like() {
        let good = "disable = [\"Bash\"]\n\n[[add]]\nname = \"T\"\ndescription = \"d\"\nschema = { type = \"object\" }\nargv = [\"true\"]\n";
        let config: ToolConfig = toml::from_str(good).unwrap();
        assert_eq!(config.disable, ["Bash"], "top-level key before the table");

        // The same key at the end is absorbed by the last tool and the top level stays empty —
        // which is why the documents show the keys first, and why this test exists at all.
        let bad = "[[add]]\nname = \"T\"\ndescription = \"d\"\nschema = { type = \"object\" }\nargv = [\"true\"]\ndisable = [\"Bash\"]\n";
        let config: ToolConfig = toml::from_str(bad).unwrap();
        assert!(config.disable.is_empty(), "absorbed by the table above it");
    }

    /// `sha256` survives the trip in from a file, and an absent one is `None`.
    ///
    /// The execution tests build the struct in memory, which would still pass if the field were
    /// dropped while parsing — and a dropped field means the check never runs, silently, which is
    /// the one way this feature can fail without saying so.
    #[test]
    fn a_pinned_digest_survives_the_toml_round_trip() {
        let pinned = "[[add]]\nname = \"T\"\ndescription = \"d\"\n\
                      schema = { type = \"object\" }\nargv = [\"./x.sh\"]\nsha256 = \"abc123\"\n";
        let config: ToolConfig = toml::from_str(pinned).unwrap();
        assert_eq!(config.add[0].sha256.as_deref(), Some("abc123"));

        // The same table without it, so a tool that never asked to be pinned is not refused for a
        // field it does not have.
        let plain = "[[add]]\nname = \"T\"\ndescription = \"d\"\n\
                     schema = { type = \"object\" }\nargv = [\"./x.sh\"]\n";
        let config: ToolConfig = toml::from_str(plain).unwrap();
        assert_eq!(config.add[0].sha256, None);
    }
}
