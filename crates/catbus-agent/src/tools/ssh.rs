// SPDX-License-Identifier: MPL-2.0

//! Two things an agent may do over SSH, and nothing else.
//!
//! `command` runs one non-interactive command on a host, optionally through a jump host. `keyscan`
//! reports a host's public keys, and can be asked to trust them.
//!
//! **What can be set is deliberately tiny: a destination, a jump host, and whether the agent is
//! forwarded.** No user, no `-o`, no `-i`, no tunnels. Each omission closes something specific:
//!
//! * **No `-o`** — the model cannot hand ssh an option, so it cannot reach `ProxyCommand` or
//!   `LocalCommand` through the argument vector itself.
//! * **No tunnels** — no `-L`, `-R` or `-D`. A tunnel opens a path into a network rather than running
//!   a command, and a refusal cannot describe a path.
//! * **No `-i`** — which key is used stays the operator's business, decided by their own config and
//!   agent. A tool that could name a key could use one the operator did not mean to offer.
//! * **No user field** — the login is whatever the operator's config says, so this cannot be used to
//!   try accounts.
//!
//! ## What "no `-o`" does *not* mean
//!
//! It does not mean a local command cannot run. **Your own ssh config still applies**, and a `Host`
//! stanza with a `ProxyCommand` runs when that host is connected to — for the destination and for the
//! jump host alike. An earlier version of this comment claimed otherwise, which was simply wrong:
//! `ssh somehost` runs `somehost`'s `ProxyCommand` whether or not this tool passes any `-o`.
//!
//! That is your configuration doing what you wrote, and it is deliberately left alone. Passing
//! `-oProxyCommand=none` would close it and would also break a bastion reachable only that way —
//! a real setup, and one this repository is already thinking about in `docs/ssh-agent-proxy.md`. The
//! guarantees this file actually offers are narrower, and worth stating exactly: **the model cannot
//! add an option, and it cannot name a host your lists do not permit.** What your config does for a
//! host you allowed is a decision you already made.
//!
//! ## The two lists, and why one is opt-out and the other opt-in
//!
//! A host is the only input that becomes an argument, so it is where the argument injection would
//! live: a string beginning `-` is read by ssh as an *option*, which is why a leading dash is refused
//! outright rather than escaped — no legitimate address starts with one. Beyond that, where a
//! connection may go is the operator's to decide, and there are two separate questions:
//!
//! * **`AllowedHosts`** limits *destinations*. Absent means no destination was restricted, which is
//!   what "the operator said nothing" has to mean for a tool whose basic job is connecting somewhere.
//!   It is a range, so it is opt-out.
//! * **`AllowedJumpHosts`** grants *jumping*. Absent means no jump host is permitted, and the refusal
//!   names the key to add. It is opt-in because a jump host is an extra way to connect rather than
//!   another destination: a machine the connection passes *through*, so allowing destinations does
//!   not imply allowing a route to them. Capabilities are granted; ranges are limited. The safe
//!   direction, since the permissive mistake would let an agent connect to a machine the destination
//!   list never mentioned.
//!
//! Both lists take an exact host or a leading `*.` for a domain and its subdomains, and neither
//! wildcard matches the bare domain. A jump host is checked against `AllowedJumpHosts` and not against
//! `AllowedHosts`: it is a route, not a destination.
//!
//! Non-interactive means non-interactive: `-T` so no pseudo-terminal is requested and
//! `BatchMode=yes` so ssh fails instead of prompting. An agent cannot answer a password prompt, and
//! a command that hangs waiting for one is worse than one that fails — it holds the turn until the
//! timeout. Stdin is `/dev/null` for the same reason.
//!
//! `StrictHostKeyChecking=yes` is passed explicitly, overriding a config that might say
//! `accept-new`. The effect is that an unknown host fails, with an error pointing at `keyscan`;
//! trusting a host is therefore always a deliberate act rather than a side effect of running a
//! command. That holds for the jump host too: an unknown bastion fails before the destination is
//! reached, which is the right order — the hop that cannot be verified is the one to stop at.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

/// A remote command may legitimately take a while — a build, a backup — so the default is generous
/// and the ceiling is a guard against a hung connection rather than a limit on the work.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_mins(2);
const MAX_COMMAND_TIMEOUT: Duration = Duration::from_mins(30);

/// How long `ssh-keyscan` waits for a host to answer. Short: a filtered port says nothing, and the
/// operator would rather be told than kept waiting.
const KEYSCAN_TIMEOUT_SECS: u64 = 10;

/// How long ssh itself waits to establish a connection, passed as `ConnectTimeout`.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// How much of a command's output is returned. The same reasoning as `Bash`: a build dumps thousands
/// of lines and the failure is at the end.
const MAX_OUTPUT: usize = 16_000;

/// The longest host this will accept. A DNS name is capped at 253 characters, and anything longer is
/// not an address.
const MAX_HOST_LEN: usize = 253;

/// The operator's limits, as the identity file's front matter states them.
///
/// One value rather than two parameters, because the checks are always made together and a caller that
/// passed one list without the other would be a bug waiting to happen. The two absences mean different
/// things — see [`Target::permitted`] and [`Target::permits_jump`] — so they are kept as `Option`s
/// rather than folded into empty `Vec`s.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Destinations. `None` is no restriction.
    pub allowed_hosts: Option<Vec<String>>,
    /// Jump hosts. `None` is *no jump host permitted*: opt-in, unlike the destinations.
    pub allowed_jump_hosts: Option<Vec<String>>,
}

/// Run an SSH action, refused unless the hosts it names are permitted.
///
/// The checks come first, before anything is read from the input beyond the host fields and before any
/// process starts: a host the operator did not allow should not reach a connect attempt, an error
/// message from ssh, or the network at all.
///
/// Both the destination and the jump are checked, and each against its own list — a routing hop is not
/// a destination, so allowing an agent to reach a machine does not allow it to pass through one.
pub async fn run_allowed(input: &serde_json::Value, cwd: &Path, policy: &Policy) -> Result<String, String> {
    // Parsed before the permission checks so that a malformed host is reported as malformed rather
    // than as unpermitted — the two are different problems and the second would be misleading.
    if let Some(host) = input.get("host").and_then(|v| v.as_str()) {
        Target::parse(host)?.permitted(policy.allowed_hosts.as_deref())?;
    }
    let jump = read_jump(input)?;
    if let Some(jump) = &jump {
        jump.permits_jump(policy.allowed_jump_hosts.as_deref())?;
    }
    run(input, cwd).await
}

/// Read and validate the `jump`, if one was asked for.
///
/// Validated by exactly the same parser as the destination, so the two cannot differ in what they
/// accept — a jump host is an address in the same sense, and the dash guard matters just as much there
/// (`-J -oProxyCommand=…` would otherwise be reachable through the jump).
fn read_jump(input: &serde_json::Value) -> Result<Option<Target>, String> {
    match input.get("jump").and_then(|v| v.as_str()).map(str::trim) {
        None | Some("") => Ok(None),
        Some(raw) => Target::parse(raw).map(Some).map_err(|e| format!("`jump`: {e}")),
    }
}

pub async fn run(input: &serde_json::Value, _cwd: &Path) -> Result<String, String> {
    let action = input
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing action — one of: command, keyscan".to_string())?;
    let host = input
        .get("host")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing host — an address or `host:port`".to_string())?;
    let target = Target::parse(host)?;
    // Read here so both actions report it, even though only `command` can act on it.
    let forward_agent = input
        .get("forward_agent")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let jump = read_jump(input)?;

    match action {
        "command" => command(input, &target, jump.as_ref(), forward_agent).await,
        "keyscan" => keyscan(input, &target, forward_agent, jump.is_some()).await,
        other => Err(format!("unknown action `{other}` — one of: command, keyscan")),
    }
}

/// A validated destination: a host, and optionally a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: Option<u16>,
}

impl Target {
    /// Read an address, or say what is wrong with it.
    ///
    /// Four accepted shapes, because they are the four an operator writes: a name, a name and port,
    /// a bracketed IPv6 and port, and a bare IPv6. Each is validated against the characters its kind
    /// may contain, so nothing can arrive that ssh would read as something other than an address.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("a host is required".to_string());
        }
        if raw.len() > MAX_HOST_LEN {
            return Err(format!(
                "a host may be at most {MAX_HOST_LEN} characters, and this is {}",
                raw.len()
            ));
        }
        // The one input that could change what runs: an argument beginning `-` is an ssh option, and
        // `-oProxyCommand=…` would run a local command to reach the "host". Refused rather than
        // escaped, because there is no legitimate address that starts with a dash.
        if raw.starts_with('-') {
            return Err(format!(
                "`{raw}` may not begin with `-`: ssh would read it as an option rather than an \
                 address, which is the one way a host could change what executes. Give a hostname or \
                 an address."
            ));
        }

        if let Some(rest) = raw.strip_prefix('[') {
            // `[2001:db8::1]` or `[2001:db8::1]:2222`.
            let (host, after) = rest
                .split_once(']')
                .ok_or_else(|| format!("`{raw}` has a `[` with no matching `]`"))?;
            if !is_ipv6(host) {
                return Err(format!("`{host}` is not a bracketed address — brackets are for IPv6"));
            }
            let port = match after {
                "" => None,
                rest => Some(parse_port(rest.trim_start_matches(':'))?),
            };
            return Ok(Self {
                host: host.to_string(),
                port,
            });
        }

        match raw.matches(':').count() {
            // A name or an IPv4 address, with a port.
            1 => {
                let (host, port) = raw.split_once(':').expect("one colon");
                check_hostname(host)?;
                Ok(Self {
                    host: host.to_string(),
                    port: Some(parse_port(port)?),
                })
            }
            // More than one colon: a bare IPv6, which carries no port. A port with an IPv6 needs
            // brackets, because otherwise the last group of the address is indistinguishable from a
            // port number.
            2.. => {
                if !is_ipv6(raw) {
                    return Err(format!(
                        "`{raw}` has several `:` but is not an IPv6 address. For an address and a \
                         port, write `[::1]:2222`."
                    ));
                }
                Ok(Self {
                    host: raw.to_string(),
                    port: None,
                })
            }
            // No colon: a name or an IPv4 address.
            _ => {
                check_hostname(raw)?;
                Ok(Self {
                    host: raw.to_string(),
                    port: None,
                })
            }
        }
    }

    /// Whether the destination list permits this target.
    ///
    /// `None` is the file saying nothing about destinations, which is not the same as an empty list:
    /// no opinion versus nobody. An empty list denies everything, which is what a file that writes
    /// `AllowedHosts:` with no names has asked for.
    ///
    /// The port is deliberately not part of the policy. A host list is about *where*, and a file that
    /// wanted to allow one port on a machine but not another would be describing a service rather
    /// than a host — worth its own field if it is ever wanted, not a surprising meaning for `:22`.
    pub fn permitted(&self, allowed: Option<&[String]>) -> Result<(), String> {
        let Some(allowed) = allowed else {
            return Ok(());
        };
        if matches_list(&self.host, allowed) {
            return Ok(());
        }
        Err(format!(
            "`{}` is not in this session's AllowedHosts, and nothing outside that list may be \
             connected to. The list is: {}. It comes from the identity file's front matter, so it is \
             the operator's limit rather than something to work around — ask them to add the host if \
             it is needed.",
            self.display(),
            render_list(allowed)
        ))
    }

    /// Whether the jump list grants this host.
    ///
    /// **Opt-in, unlike the destination list**: `None` permits no jump host at all, because jumping is
    /// a capability rather than a range — a machine the connection passes through rather than another
    /// place to connect. See the module doc for why the two lists disagree about absence.
    ///
    /// The refusal names the key to add, so the first attempt at a jump is a discoverable failure
    /// rather than a dead end.
    pub fn permits_jump(&self, allowed: Option<&[String]>) -> Result<(), String> {
        match allowed {
            Some(list) if matches_list(&self.host, list) => Ok(()),
            Some(list) => Err(format!(
                "`{}` is not in this session's AllowedJumpHosts, so this session may not route \
                 through it. The list is: {}. A jump host is a machine the connection passes through \
                 rather than a destination, so permitting one is a separate decision from permitting \
                 where the session may go.",
                self.display(),
                render_list(list)
            )),
            None => Err(format!(
                "this session may not use `{}` as a jump host, because no AllowedJumpHosts is set. \
                 Jumping is opt-in: a jump host is a machine the connection passes through rather \
                 than a destination, so it needs its own grant. Add to the identity file's front \
                 matter:\n\n  AllowedJumpHosts: {}\n\nor ask the operator to. AllowedHosts does not \
                 imply it, since allowing destinations is not the same as allowing a route to them.",
                self.display(),
                self.host
            )),
        }
    }

    /// How the target is named in a message the operator reads.
    #[must_use]
    pub fn display(&self) -> String {
        match self.port {
            Some(port) if self.host.contains(':') => format!("[{}]:{port}", self.host),
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }
}

/// Whether a host appears in a list of exact hosts and `*.` wildcards.
///
/// One matcher for both lists, so the two cannot drift into disagreeing about what `*.x` means —
/// which would be a policy that reads the same and acts differently.
///
/// Two kinds of entry:
///
/// * an exact host, compared without case — `dc1.servers.example.org`;
/// * a leading `*.` for a domain and its subdomains — `*.servers.example.org` matches
///   `dc1.servers.example.org` but **not** `servers.example.org` itself, because a wildcard that also matched
///   the bare domain is how a limit quietly becomes wider than it reads. The dot boundary matters for
///   the same reason: `*.example.org` must not match `evilexample.com`.
fn matches_list(host: &str, entries: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    entries.iter().any(|entry| {
        let entry = entry.trim().to_ascii_lowercase();
        entry
            .strip_prefix("*.")
            .map_or_else(|| entry == host, |suffix| host.ends_with(&format!(".{suffix}")))
    })
}

/// A list as it reads in a message, with the empty case named rather than left blank.
fn render_list(entries: &[String]) -> String {
    if entries.is_empty() {
        "(empty — nothing is allowed)".to_string()
    } else {
        entries.join(", ")
    }
}

/// The argument vector for one `ssh`, ending at the destination.
///
/// The caller appends the remote command, so this is what a test can assert on without a network: it
/// is the whole of what ssh is told about *where* and *how*, and every flag in it is a decision
/// written down rather than a default inherited from whatever config happens to be on the machine.
fn ssh_args(target: &Target, jump: Option<&Target>, forward_agent: bool, connect_timeout: u64) -> Vec<String> {
    let mut args = vec![
        // No pseudo-terminal. A command run for an agent has nothing to render into, and asking for
        // a tty is how a command that expects one hangs.
        "-T".to_string(),
        // Stated either way, so the config's default (or `ForwardAgent yes` in it) cannot decide.
        (if forward_agent { "-A" } else { "-a" }).to_string(),
        // Never prompt. An agent cannot answer, and a prompt that cannot be answered is a hang.
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={connect_timeout}"),
        // Explicit, overriding a config that says `accept-new`: an unknown host must fail with an
        // error pointing at `keyscan`, so trust is an act rather than a side effect.
        //
        // Stated once and inherited by the jump hop: ssh applies the same options to the connection it
        // makes to the bastion, so an unknown bastion fails before the destination is reached. That is
        // the correct order — the hop that cannot be verified is the one to stop at.
        "-o".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
    ];
    // The jump host, as a `[user@]host[:port]` list. The user part is never set: the login is the
    // operator's config, for the destination and for the hop alike.
    if let Some(jump) = jump {
        args.push("-J".to_string());
        args.push(jump.display());
    }
    if let Some(port) = target.port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    args.push(target.host.clone());
    args
}

/// Run one command on the host.
async fn command(
    input: &serde_json::Value,
    target: &Target,
    jump: Option<&Target>,
    forward_agent: bool,
) -> Result<String, String> {
    let remote = input
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| {
            "missing `command` — one non-interactive command line, run by the host's own shell. \
             Nothing may be set about how it is reached beyond the host and whether the agent is \
             forwarded."
                .to_string()
        })?;

    let timeout = input
        .get("timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_COMMAND_TIMEOUT, |s| {
            Duration::from_secs(s).min(MAX_COMMAND_TIMEOUT)
        });

    let mut args = ssh_args(target, jump, forward_agent, CONNECT_TIMEOUT_SECS);
    // The remote command is one argument, exactly as it would be on a command line. ssh hands it to
    // the host's shell unmodified, which is what makes a pipeline or a redirect work there — and is
    // also why nothing about the *local* invocation is a shell string.
    args.push(remote.to_string());

    let started = std::time::Instant::now();
    let mut command = Command::new("ssh");
    command
        .args(&args)
        // No local stdin, so a command that tries to read gets end-of-input rather than a hang.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("could not run ssh: {e}. Is it installed?")),
        Err(_) => {
            return Err(format!(
                "the command on {} did not finish within {}s and the connection was stopped. Raise \
                 `timeout_secs` (max {}) for something long-running, or run it detached on the host.",
                target.display(),
                timeout.as_secs(),
                MAX_COMMAND_TIMEOUT.as_secs()
            ));
        }
    };
    let seconds = (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // The one failure worth explaining rather than quoting: ssh's own message names neither the cause
    // nor the way out, and it is the failure an agent will hit first.
    if stderr.contains("Host key verification failed") {
        return Err(format!(
            "the host key for {} is not in this machine's known_hosts, and this tool does not trust \
             a host on its own. Run `keyscan` for it first — and check the fingerprints it returns are \
             the ones you expect, because keyscan believes whatever answers — then `keyscan` again \
             with `trust: true` to add them. After that this command will connect.",
            target.display()
        ));
    }

    let report = serde_json::json!({
        "host": target.display(),
        "command": remote,
        // Both reported because both change what the reader should assume about the result. With the
        // agent forwarded, a command on the host can use the operator's keys; through a jump, the
        // command ran on a machine reached by a route the operator had to grant separately.
        "agent_forwarded": forward_agent,
        "via_jump": jump.map(Target::display),
        "exit_code": output.status.code(),
        "succeeded": output.status.success(),
        "wall_seconds": seconds,
        "stdout": bound(&stdout),
        "stderr": bound(&stderr),
    });
    serde_json::to_string_pretty(&report).map_err(|e| format!("could not encode: {e}"))
}

/// Report the host's public keys, and optionally trust them.
async fn keyscan(
    input: &serde_json::Value,
    target: &Target,
    forward_agent: bool,
    asked_to_jump: bool,
) -> Result<String, String> {
    let mut args = vec!["-T".to_string(), KEYSCAN_TIMEOUT_SECS.to_string()];
    if let Some(port) = target.port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    args.push(target.host.clone());

    let mut command = Command::new("ssh-keyscan");
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A few seconds longer than keyscan's own timeout, so ssh-keyscan is the thing that gives up
        // and reports which host did not answer — rather than this killing it and losing that.
        .kill_on_drop(true);

    let output = match tokio::time::timeout(Duration::from_secs(KEYSCAN_TIMEOUT_SECS + 5), command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("could not run ssh-keyscan: {e}. Is it installed?")),
        Err(_) => {
            return Err(format!(
                "ssh-keyscan did not return within {}s for {}. A host that answers slowly, or a \
                 filtered port, looks like this.",
                KEYSCAN_TIMEOUT_SECS + 5,
                target.display()
            ));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let keys: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect();

    // Only the lines that name this host, and only those shaped like a key: the append below edits a
    // file ssh trusts, so nothing about it is taken on faith.
    let usable: Vec<&String> = keys.iter().filter(|line| is_host_key_line(line)).collect();

    let trust = input.get("trust").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let mut report = serde_json::Map::new();
    report.insert("host".into(), serde_json::json!(target.display()));
    report.insert("count".into(), serde_json::json!(usable.len()));
    report.insert("keys".into(), serde_json::json!(usable));

    if trust {
        let written = trust_keys(&usable)?;
        report.insert(
            "trusted".into(),
            serde_json::json!({ "written": written.added, "file": written.path.display().to_string(), "already_known": written.known }),
        );
        // Said every time, because it is the thing that makes this dangerous: ssh-keyscan asks
        // whoever answers and has no way to know it is the right machine. The operator is the only
        // one who can check a fingerprint.
        report.insert(
            "warning".into(),
            serde_json::json!(
                "these keys came from whatever answered at that address; they were not verified \
                 against anything. Compare the fingerprints with the host's own before relying on \
                 them."
            ),
        );
    } else if usable.is_empty() {
        // Not an error: a host with no reachable sshd is an answer, and one worth reading as itself.
        report.insert(
            "note".into(),
            serde_json::json!(format!(
                "no keys came back from {}. Nothing was trusted. The address may be wrong, the port \
                 closed, or the service not ssh.",
                target.display()
            )),
        );
    }

    if forward_agent {
        // Stated rather than silently ignored: a caller that asked for it should learn it means
        // nothing here, rather than assuming forwarding happened.
        report.insert(
            "note_forward_agent".into(),
            serde_json::json!("keyscan opens no session, so `forward_agent` has no effect on it"),
        );
    }
    if asked_to_jump {
        // The same courtesy for `jump`: `ssh-keyscan` has no way to route through another host, and a
        // caller who asked for one should be told it did not happen rather than assuming it did. It
        // also matters for reading the result: these keys are the destination's own, seen directly.
        report.insert(
            "note_jump".into(),
            serde_json::json!(
                "keyscan connects directly and cannot route through a jump host, so `jump` had no \
                 effect — the keys below came from the destination itself"
            ),
        );
    }

    serde_json::to_string_pretty(&serde_json::Value::Object(report)).map_err(|e| format!("could not encode: {e}"))
}

/// What a trusted append did.
#[derive(Debug)]
struct Trusted {
    added: usize,
    known: usize,
    path: PathBuf,
}

/// Append host keys to `~/.ssh/known_hosts`, without duplicating what is there.
///
/// Deliberately not `ssh-keyscan … >> known_hosts`, which is the one-liner everyone writes: a shell
/// redirect would append a second copy every time, and would create the file world-readable under
/// whatever umask is in force. A file ssh trusts deserves the few extra lines — directory `0700`,
/// file `0600`, and no line added twice.
fn trust_keys(usable: &[&String]) -> Result<Trusted, String> {
    if usable.is_empty() {
        return Err(
            "there are no keys to trust: ssh-keyscan returned none for this address. Nothing was \
             written — trusting nothing would leave known_hosts unchanged and the caller believing \
             otherwise."
                .to_string(),
        );
    }
    let home = std::env::var_os("HOME").ok_or_else(|| "no $HOME, so no known_hosts".to_string())?;
    trust_keys_in(Path::new(&home), usable)
}

/// [`trust_keys`] with the home directory passed in.
///
/// Split out because the test needs one, and edition 2024 makes `set_var` `unsafe` — which this crate
/// denies outright, so a test cannot set `HOME` even to point at a temp dir. The same shape `tasks`
/// uses for its state directory, for the same reason.
fn trust_keys_in(home: &Path, usable: &[&String]) -> Result<Trusted, String> {
    let dir = home.join(".ssh");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    set_mode(&dir, 0o700)?;

    let path = dir.join("known_hosts");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let already: std::collections::HashSet<&str> = existing.lines().map(str::trim).collect();

    let mut fresh: Vec<&String> = Vec::new();
    for line in usable {
        if !already.contains(line.as_str()) {
            fresh.push(line);
        }
    }
    let known = usable.len() - fresh.len();

    if !fresh.is_empty() {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        // A leading newline when the existing file does not end in one, so the appended key cannot
        // be joined onto a truncated last line and become a key nobody signed.
        if !existing.is_empty() && !existing.ends_with('\n') {
            let _ = file.write_all(b"\n");
        }
        for line in &fresh {
            let _ = writeln!(file, "{line}");
        }
    }
    set_mode(&path, 0o600)?;

    Ok(Trusted {
        added: fresh.len(),
        known,
        path,
    })
}

/// Restrict a path to `mode`, reporting why if it cannot be done.
///
/// A failure is an error rather than a warning: a `known_hosts` that is readable by others is a
/// smaller problem than a silent one, but the caller asked to trust a key and should hear that the
/// result is not what was intended.
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("cannot set {:o} on {}: {e}", mode, path.display()))
}

/// Whether a line is shaped like a host key entry.
///
/// Three fields at least — address, key type, base64 key — which is what `ssh-keyscan` emits and what
/// `known_hosts` holds. A structural check rather than a parse: this decides what may be appended to
/// a file ssh trusts, so a line that is not shaped like a key is not added, whatever it says.
fn is_host_key_line(line: &str) -> bool {
    let mut fields = line.split_whitespace();
    let (Some(address), Some(kind), Some(key)) = (fields.next(), fields.next(), fields.next()) else {
        return false;
    };
    if address.is_empty() || key.is_empty() {
        return false;
    }
    // A key type ssh would recognise, so a stray line of prose is not appended as though it were a
    // key. Not an allow-list of algorithms — that would age — but the shape of the name.
    kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-")
}

/// A valid hostname or IPv4 address.
fn check_hostname(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("the host is empty".to_string());
    }
    if host.len() > MAX_HOST_LEN {
        return Err(format!("a host may be at most {MAX_HOST_LEN} characters"));
    }
    // Letters, digits, dots, dashes and underscores. No `@`, no space, no `$`, no semicolon: nothing
    // that a shell or ssh could read as more than a name. IPv6 has its own path, through brackets.
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(format!(
            "`{host}` contains something a hostname may not: letters, digits, dots, dashes and \
             underscores only. This tool sets no user — the login comes from your own ssh config."
        ));
    }
    Ok(())
}

/// A port number, as a number.
fn parse_port(raw: &str) -> Result<u16, String> {
    // Deliberately not trimmed. `host: 22` is a typo, and this file's whole value is that nothing
    // ambiguous is accepted — the message below promises that nothing but digits may follow a host,
    // and trimming would quietly make that untrue. The whole input is trimmed once, in `Target::parse`.
    if raw.is_empty() {
        return Err("there is a `:` but no port after it".to_string());
    }
    if !raw.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "`{raw}` is not a port number. A port is digits — nothing else can follow a host."
        ));
    }
    raw.parse::<u16>()
        .map_err(|_| format!("`{raw}` is not a port number between 1 and 65535"))
        .and_then(|port| {
            if port == 0 {
                Err("port 0 is not a destination".to_string())
            } else {
                Ok(port)
            }
        })
}

/// Whether the text is an IPv6 address: hex digits, dots and colons, and at least one colon.
fn is_ipv6(text: &str) -> bool {
    !text.is_empty() && text.contains(':') && text.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
}

/// Keep the end of long output, saying how much was dropped.
///
/// Same reasoning as `Bash`: a build prints thousands of lines and the failure is in the last few, so
/// the tail is what is worth keeping.
fn bound(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.len() <= MAX_OUTPUT {
        return trimmed.to_owned();
    }
    let start = trimmed.len() - MAX_OUTPUT;
    let start = (start..=trimmed.len())
        .find(|i| trimmed.is_char_boundary(*i))
        .unwrap_or(trimmed.len());
    format!("[…{} bytes dropped…]\n{}", start, &trimmed[start..])
}

/// The tool's schema.
#[must_use]
pub fn spec() -> serde_json::Value {
    serde_json::json!({
        "name": "SSH",
        "description": "Two things over SSH, and nothing else may be configured. `command` runs one \
                        non-interactive command on a host and returns its output, optionally routed \
                        through a jump host. `keyscan` returns a host's public keys, and with \
                        `trust: true` adds them to known_hosts. Settable: a host, a jump host, and \
                        whether to forward the SSH agent — no user, no extra ssh options, no tunnels, \
                        no key selection. The host must be in the identity file's `AllowedHosts`, and \
                        a jump host in its `AllowedJumpHosts`, which is opt-in. It runs a command on \
                        another machine, so it is judged in auto mode and refused in plan-mode.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["command", "keyscan"] },
                "host": {
                    "type": "string",
                    "description": "A hostname, an IPv4 address, or either with a port: `host`, `10.0.0.1:2222`, `[2001:db8::1]:2222`. No user — the login comes from your own ssh config."
                },
                "jump": {
                    "type": "string",
                    "description": "Route through this host (ssh -J), `host` or `host:port`. It needs its own grant: the identity file must list it in `AllowedJumpHosts`, which is opt-in — `AllowedHosts` permits destinations, not routes to them. A jump is a machine the connection passes through. `keyscan` cannot use one and says so."
                },
                "forward_agent": {
                    "type": "boolean",
                    "description": "Forward the SSH agent for the session (ssh -A). Off unless asked. With it, a command on the host can use your keys; that is the whole point of the flag and the whole risk of it. Ignored by `keyscan`, which opens no session."
                },
                "command": {
                    "type": "string",
                    "description": "For `command`: one non-interactive command line, run by the host's shell. It cannot prompt — ssh runs with no terminal and batch mode, and an unanswerable prompt fails rather than hangs."
                },
                "trust": {
                    "type": "boolean",
                    "description": "For `keyscan`: also append the keys to ~/.ssh/known_hosts. They are not verified against anything — compare the fingerprints first."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "For `command`: override the 120s default. Capped at 1800."
                }
            },
            "required": ["action", "host"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four shapes an operator writes.
    #[test]
    fn an_address_parses_into_a_host_and_an_optional_port() {
        let plain = Target::parse("dc1.servers.example.org").expect("a name");
        assert_eq!(plain.host, "dc1.servers.example.org");
        assert_eq!(plain.port, None);

        let with_port = Target::parse("10.0.0.1:2222").expect("an address and port");
        assert_eq!(with_port.host, "10.0.0.1");
        assert_eq!(with_port.port, Some(2222));

        let bracketed = Target::parse("[2001:db8::1]:22").expect("a bracketed address");
        assert_eq!(bracketed.host, "2001:db8::1");
        assert_eq!(bracketed.port, Some(22));

        let bare_v6 = Target::parse("2001:db8::1").expect("a bare address");
        assert_eq!(bare_v6.host, "2001:db8::1");
        assert_eq!(bare_v6.port, None);

        // Whitespace is tolerated, because a copy-paste carries it.
        assert_eq!(
            Target::parse("  host.example.org  ").expect("trimmed").host,
            "host.example.org"
        );
        // And a name may carry a dash or an underscore, which is why those are allowed.
        assert!(Target::parse("my-host_1.example.org").is_ok());
    }

    /// The safety property: a host cannot become an ssh option.
    ///
    /// This is the injection the whole validation exists for. `-oProxyCommand=…` would run a local
    /// command to reach the "host", so a bare `-` at the front is refused rather than escaped —
    /// there is no legitimate address that begins with one.
    #[test]
    fn a_host_that_would_be_an_option_is_refused() {
        for hostile in [
            "-oProxyCommand=/bin/sh -c whoami",
            "-oProxyCommand=rm -rf /",
            "--config=/dev/null",
            "-J jump.example",
            "-L 8080:localhost:80",
            "-i /tmp/key",
        ] {
            let err = Target::parse(hostile).expect_err("must be refused");
            assert!(
                err.contains("may not begin with `-`"),
                "the message must name the reason for {hostile}: {err}"
            );
        }
    }

    /// Everything a hostname may not contain, refused.
    ///
    /// Nothing here is escaped or quoted: the point is that no such address exists, so refusing keeps
    /// ssh seeing only arguments this file built.
    #[test]
    fn a_host_with_anything_else_in_it_is_refused() {
        for bad in [
            "host; rm -rf /",
            "host|cat /etc/passwd",
            "host&&whoami",
            "host$(whoami)",
            "host`id`",
            "host name",
            "user@host",
            "host>file",
            "host\\x00",
            "*",
            "",
            "   ",
        ] {
            assert!(Target::parse(bad).is_err(), "`{bad}` must not be accepted as a host");
        }
        // A host longer than a DNS name.
        let long = "a".repeat(MAX_HOST_LEN + 1);
        assert!(Target::parse(&long).is_err(), "an over-long host must be refused");
    }

    /// A colon means a port, and a port is digits.
    #[test]
    fn a_colon_must_be_followed_by_a_real_port() {
        assert!(Target::parse("host:22").is_ok());
        assert!(Target::parse("host:65535").is_ok());
        for bad in [
            "host:",
            "host:abc",
            "host:22a",
            "host:-1",
            "host:0",
            "host:65536",
            "host: 22",
        ] {
            assert!(Target::parse(bad).is_err(), "`{bad}` must not be accepted");
        }
        // An unclosed bracket, and brackets around something that is not an address.
        assert!(Target::parse("[::1").is_err());
        assert!(Target::parse("[not-an-address]:22").is_err());
        // Several colons that are not an address: the message says how to write a port with one.
        let err = Target::parse("host:22:22").expect_err("not an address");
        assert!(err.contains("[::1]:2222"), "the message should show the form: {err}");
    }

    /// The argv is what ssh is told about where and how, and every flag in it is a decision.
    #[test]
    fn the_ssh_arguments_state_every_choice_explicitly() {
        let target = Target::parse("dc1.example:2222").expect("parsed");
        let args = ssh_args(&target, None, false, 10);

        // No terminal and no prompting: an agent cannot answer, so a prompt must be impossible.
        assert!(args.contains(&"-T".to_string()), "{args:?}");
        assert!(args.contains(&"BatchMode=yes".to_string()), "{args:?}");
        // Forwarding is stated either way, so a config that says `ForwardAgent yes` cannot decide it.
        assert!(args.contains(&"-a".to_string()), "off must be explicit: {args:?}");
        let forwarded = ssh_args(&target, None, true, 10);
        assert!(forwarded.contains(&"-A".to_string()), "{forwarded:?}");
        assert!(!forwarded.contains(&"-a".to_string()), "not both: {forwarded:?}");
        // Unknown hosts fail rather than being accepted, so trusting is a deliberate act.
        assert!(args.contains(&"StrictHostKeyChecking=yes".to_string()), "{args:?}");
        // The port, then the host, last.
        assert_eq!(args[args.len() - 3..], ["-p", "2222", "dc1.example"], "{args:?}");
        // And nothing that reaches a third machine or a local command.
        for forbidden in ["-J", "-L", "-R", "-D", "-oProxyCommand", "-i", "-F", "-l"] {
            assert!(
                !args.iter().any(|a| a == forbidden),
                "{forbidden} must not appear: {args:?}"
            );
        }
        // Every `-o` in it is a setting this file chose, never a caller's.
        let options: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-o")
            .filter_map(|(i, _)| args.get(i + 1))
            .collect();
        assert_eq!(
            options,
            ["BatchMode=yes", "ConnectTimeout=10", "StrictHostKeyChecking=yes"],
            "{args:?}"
        );
    }

    /// A portless host has no `-p`, and the host is still last.
    #[test]
    fn a_portless_target_gets_no_port_flag() {
        let target = Target::parse("host.example.org").expect("parsed");
        let args = ssh_args(&target, None, false, 10);
        assert!(!args.contains(&"-p".to_string()), "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some("host.example.org"), "{args:?}");
    }

    /// A line that is not shaped like a key is not appended to a file ssh trusts.
    #[test]
    fn only_key_shaped_lines_are_worth_trusting() {
        assert!(is_host_key_line(
            "host.example.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI..."
        ));
        assert!(is_host_key_line("10.0.0.1 ecdsa-sha2-nistp256 AAAAE2VjZHNh..."));
        assert!(is_host_key_line("[2001:db8::1]:22 sk-ssh-ed25519@openssh.com AAAA..."));
        // Not keys: prose, a comment, an empty line, a two-field line, a name that is not a type.
        assert!(!is_host_key_line("# host.example.org SSH-2.0-OpenSSH_10.0"));
        assert!(!is_host_key_line("host.example.org ssh-ed25519"));
        assert!(!is_host_key_line("host.example.org not-a-key-type AAAA"));
        assert!(!is_host_key_line("just some words here"));
        assert!(!is_host_key_line(""));
    }

    /// Trusting writes a file with the permissions ssh expects, and does not duplicate.
    ///
    /// The append is driven through a real `HOME`, because that is what it edits — the same reason
    /// the harness test uses the real home rather than copying config around.
    #[test]
    fn trusting_appends_once_with_the_right_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().unwrap();

        let key = "host.example.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIexamplekey".to_string();
        let lines = vec![&key];

        let first = trust_keys_in(home.path(), &lines).expect("writes");
        assert_eq!(first.added, 1);
        assert_eq!(first.known, 0);
        let path = home.path().join(".ssh").join("known_hosts");
        assert_eq!(first.path, path);
        let written = std::fs::read_to_string(&path).expect("a file");
        assert!(written.contains("ssh-ed25519"), "{written}");

        // The permissions are the point of doing this here rather than with a shell redirect: 0600,
        // and 0700 on the directory holding it.
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "known_hosts must not be readable by others"
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
            0o700,
            "~/.ssh must not be readable by others"
        );

        // A second run adds nothing, and says the key was already known.
        let second = trust_keys_in(home.path(), &lines).expect("writes again");
        assert_eq!(second.added, 0, "the same key must not be appended twice");
        assert_eq!(second.known, 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().matches("ssh-ed25519").count(),
            1,
            "one copy only"
        );

        // A file left without a trailing newline: the appended key must start on its own line, or it
        // would be joined to the previous one and become a key nobody signed.
        std::fs::write(&path, "other.example ssh-ed25519 AAAAsomething").unwrap();
        let third_key = "third.example ssh-ed25519 AAAAnother".to_string();
        trust_keys_in(home.path(), &[&third_key]).expect("writes");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("AAAAsomething\nthird.example"),
            "the append must not run a key onto the previous line: {text:?}"
        );
    }

    /// Trusting nothing is an error, not a silent success.
    ///
    /// A caller that asked to trust a host and got no keys must not be told it worked: `known_hosts`
    /// would be unchanged while the reply read as though it had been written.
    #[test]
    fn trusting_nothing_is_refused() {
        let err = trust_keys(&[]).expect_err("must refuse");
        assert!(err.contains("no keys to trust"), "{err}");
        assert!(err.contains("Nothing was written"), "{err}");
    }

    /// The output bound keeps the end, and cuts on a character boundary.
    #[test]
    fn long_output_keeps_its_tail() {
        // Built without `format!` or a `Write` trait: an anonymous (`as _`) import is not visible
        // through `use super::*`, so a macro needing `fmt::Write` would have to be imported inside
        // this module too — and pushing the pieces is clearer than that.
        let mut text = String::new();
        for i in 0..5_000 {
            text.push_str("line ");
            text.push_str(&i.to_string());
            text.push('\n');
        }
        let bounded = bound(&text);
        assert!(bounded.starts_with("[…"), "{bounded:.40}");
        assert!(bounded.contains("line 4999"), "the tail is what matters");
        assert!(bounded.len() < text.len());
        // Multi-byte content must not panic the slice.
        let multibyte = "é".repeat(MAX_OUTPUT * 2);
        let bounded = bound(&multibyte);
        assert!(bounded.starts_with("[…"));
        assert!(bounded.ends_with('é'));
        assert_eq!(bound("short"), "short");
    }

    /// The schema says what may be set, and does not offer what may not.
    ///
    /// An address, a jump host and agent-forwarding are offered; a user, a key, an ssh option and a
    /// tunnel are not. The list of property names is asserted exactly rather than by absence, so
    /// adding a field means this test is edited deliberately — which is what should happen for
    /// something that widens what the tool can be told.
    #[test]
    fn the_schema_offers_nothing_beyond_addresses_and_forwarding() {
        let spec = spec();
        assert_eq!(spec["name"], "SSH");
        let properties = spec["input_schema"]["properties"].as_object().expect("properties");
        let offered: Vec<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(
            offered,
            [
                "action",
                "command",
                "forward_agent",
                "host",
                "jump",
                "timeout_secs",
                "trust"
            ],
            "the schema must not offer a user, a key, or an ssh option"
        );
        assert_eq!(spec["input_schema"]["required"], serde_json::json!(["action", "host"]));
        // And the description says the omissions are the point, since a model reads it.
        //
        // "no jump host" was on this list until jumping was added. It is gone rather than kept, so the
        // test states what is true now: the remaining omissions, and that a jump needs a grant of its
        // own rather than riding on the destination list.
        let described = spec["description"].as_str().unwrap_or_default();
        for phrase in ["no user", "no tunnels", "no key selection"] {
            assert!(described.contains(phrase), "should mention `{phrase}`: {described}");
        }
        assert!(
            described.contains("AllowedJumpHosts"),
            "and that a jump is granted separately: {described}"
        );
        assert!(described.contains("AllowedHosts"), "as is the destination: {described}");
    }

    /// An unknown action is refused, naming the real ones.
    #[tokio::test]
    async fn an_unknown_action_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            &serde_json::json!({"action": "tunnel", "host": "host.example.org"}),
            dir.path(),
        )
        .await
        .expect_err("must refuse");
        assert!(err.contains("unknown action `tunnel`"), "{err}");
        assert!(err.contains("command, keyscan"), "{err}");

        // A missing action or host, likewise.
        let no_action = run(&serde_json::json!({"host": "h"}), dir.path()).await.unwrap_err();
        assert!(no_action.contains("missing action"), "{no_action}");
        let no_host = run(&serde_json::json!({"action": "keyscan"}), dir.path())
            .await
            .unwrap_err();
        assert!(no_host.contains("missing host"), "{no_host}");
    }

    /// `command` needs a command, and refuses a hostile host before running anything.
    #[tokio::test]
    async fn a_command_needs_a_command_and_a_safe_host() {
        let dir = tempfile::tempdir().unwrap();
        let missing = run(
            &serde_json::json!({"action": "command", "host": "host.example.org"}),
            dir.path(),
        )
        .await
        .expect_err("must refuse");
        assert!(missing.contains("missing `command`"), "{missing}");

        let blank = run(
            &serde_json::json!({"action": "command", "host": "host.example.org", "command": "   "}),
            dir.path(),
        )
        .await
        .expect_err("must refuse");
        assert!(
            blank.contains("missing `command`"),
            "a blank command is not one: {blank}"
        );

        // The host is validated before ssh is invoked, so this never spawns anything.
        let hostile = run(
            &serde_json::json!({
                "action": "command",
                "host": "-oProxyCommand=whoami",
                "command": "id"
            }),
            dir.path(),
        )
        .await
        .expect_err("must refuse");
        assert!(hostile.contains("may not begin with `-`"), "{hostile}");
    }

    /// An allow-list permits, and denies, exactly what it reads as.
    ///
    /// The operator's requirement: the identity file forces which hosts `SSH` may reach. `None` is the
    /// file saying nothing — no opinion, so nothing is refused — while an *empty* list is a file that
    /// wrote `AllowedHosts:` with no names, which denies everything. Those two must not be conflated:
    /// one is silence and the other is a prohibition.
    #[test]
    fn an_allow_list_permits_only_what_it_names() {
        let listed = |hosts: &[&str]| -> Vec<String> { hosts.iter().map(|h| (*h).to_string()).collect() };
        let host = Target::parse("dc1.servers.example.org").expect("parsed");

        // No list: no opinion.
        assert!(host.permitted(None).is_ok(), "silence is not a prohibition");

        // An exact host, with case and surrounding space tolerated — an operator writing
        // `DC1.SERVERS.EXAMPLE.ORG` in a config meant that host, and refusing over case would be a
        // puzzle rather than a limit.
        assert!(host.permitted(Some(&listed(&["dc1.servers.example.org"]))).is_ok());
        assert!(host.permitted(Some(&listed(&[" DC1.SERVERS.EXAMPLE.ORG "]))).is_ok());

        // A wildcard covers subdomains…
        assert!(host.permitted(Some(&listed(&["*.servers.example.org"]))).is_ok());
        // …and not the bare domain, which is the whole reason for the leading `*.`: a wildcard that
        // also matched `servers.example.org` would be wider than it reads.
        let bare = Target::parse("servers.example.org").expect("parsed");
        assert!(
            bare.permitted(Some(&listed(&["*.servers.example.org"]))).is_err(),
            "`*.example.org` must not match `example.org` itself"
        );
        // And not a domain that merely ends with the same letters.
        let sneak = Target::parse("evilservers.example.org").expect("parsed");
        assert!(
            sneak.permitted(Some(&listed(&["*.servers.example.org"]))).is_err(),
            "a suffix match must respect the dot boundary"
        );

        // A host not in the list is refused, and the message says where the limit comes from — an
        // agent reading it should understand this is the operator's decision, not a bug to work
        // around.
        let err = host.permitted(Some(&listed(&["other.example"]))).unwrap_err();
        assert!(err.contains("dc1.servers.example.org"), "{err}");
        assert!(err.contains("AllowedHosts"), "{err}");
        assert!(err.contains("other.example"), "the list is shown: {err}");
        assert!(
            err.contains("ask them to add the host"),
            "and the way out is named rather than implied: {err}"
        );

        // An empty list denies everything, and says so rather than printing nothing.
        let empty: Vec<String> = Vec::new();
        let err = host.permitted(Some(&empty)).unwrap_err();
        // "nothing is allowed" rather than "no host": the wording is shared with the jump list, where a
        // host is not what is being denied.
        assert!(err.contains("empty — nothing is allowed"), "{err}");

        // A port is not part of the policy: the list is about where, so an allowed host on another
        // port is still that host.
        let ported = Target::parse("dc1.servers.example.org:2222").expect("parsed");
        assert!(ported.permitted(Some(&listed(&["dc1.servers.example.org"]))).is_ok());
    }

    /// The refusal happens before anything is run.
    ///
    /// `run_allowed` is what the dispatcher calls, and its order matters: a host the operator did not
    /// allow must not reach a connect attempt, an ssh error message, or the network. Asserted by its
    /// being refused with no ssh installed anywhere in the test's path — a `command` action would
    /// otherwise spawn one and fail differently.
    #[tokio::test]
    async fn an_unpermitted_host_is_refused_before_anything_runs() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec!["allowed.example".to_string()];

        let err = run_allowed(
            &serde_json::json!({
                "action": "command",
                "host": "forbidden.example",
                "command": "id"
            }),
            dir.path(),
            &Policy {
                allowed_hosts: Some(allowed.clone()),
                ..Policy::default()
            },
        )
        .await
        .expect_err("must be refused");
        assert!(err.contains("AllowedHosts"), "{err}");
        assert!(
            !err.contains("could not run ssh"),
            "ssh must not have been invoked at all: {err}"
        );

        // A permitted host passes the check and gets as far as running ssh — which fails in this
        // environment because there is nothing to connect to, and that failure is ssh's, proving the
        // gate opened.
        let outcome = run_allowed(
            &serde_json::json!({
                "action": "command",
                "host": "allowed.example",
                "command": "id",
                "timeout_secs": 1
            }),
            dir.path(),
            &Policy {
                allowed_hosts: Some(allowed.clone()),
                ..Policy::default()
            },
        )
        .await;
        // A machine with no route to `allowed.example` may fail either way; what matters is that any
        // failure is not this tool's own list.
        if let Err(e) = outcome {
            assert!(
                !e.contains("AllowedHosts"),
                "a permitted host must not be refused by the list: {e}"
            );
        }
    }

    /// A malformed host is reported as malformed, not as unpermitted.
    ///
    /// The two are different problems — one is a typo, the other is a decision — and answering a typo
    /// with "not in your list" would send the reader looking at the wrong thing.
    #[tokio::test]
    async fn a_malformed_host_is_not_reported_as_unpermitted() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec!["*.example".to_string()];
        for bad in ["-oProxyCommand=x", "host;rm", "user@host"] {
            let err = run_allowed(
                &serde_json::json!({"action": "command", "host": bad, "command": "id"}),
                dir.path(),
                &Policy {
                    allowed_hosts: Some(allowed.clone()),
                    ..Policy::default()
                },
            )
            .await
            .expect_err("must be refused");
            assert!(
                !err.contains("AllowedHosts"),
                "`{bad}` is malformed, not unpermitted: {err}"
            );
        }
    }

    /// A jump host is granted, not limited: absent means none is permitted.
    ///
    /// This is the asymmetry with `AllowedHosts`, and it is the safety-relevant half. Destinations are
    /// a *range* — silence has to mean unrestricted, since a tool whose job is connecting somewhere
    /// cannot read silence as "nowhere" — while jumping is a *capability*, so silence means it was not
    /// granted. Getting this backwards would let an agent route a connection through a machine the
    /// destination list never mentioned.
    #[test]
    fn a_jump_host_needs_its_own_grant() {
        let bastion = Target::parse("bastion.example").expect("parsed");
        let listed = |hosts: &[&str]| -> Vec<String> { hosts.iter().map(|h| (*h).to_string()).collect() };

        // **The asymmetry.** No jump list at all: refused, where no host list means allowed.
        let err = bastion.permits_jump(None).unwrap_err();
        assert!(err.contains("AllowedJumpHosts"), "{err}");
        assert!(
            err.contains("Jumping is opt-in"),
            "the refusal should say why it is not implied: {err}"
        );
        // And it names the exact line to add, so the first attempt is a discoverable failure rather
        // than a dead end.
        assert!(
            err.contains("AllowedJumpHosts: bastion.example"),
            "the message should be copy-pasteable: {err}"
        );
        assert!(
            err.contains("AllowedHosts does not imply it"),
            "and should say what does not grant it: {err}"
        );

        // Granted: permitted.
        assert!(bastion.permits_jump(Some(&listed(&["bastion.example"]))).is_ok());
        // A wildcard, with the same dot-boundary rule as the destination list.
        assert!(bastion.permits_jump(Some(&listed(&["*.example"]))).is_ok());
        // A host that is not listed: refused, and the list is shown.
        let err = bastion
            .permits_jump(Some(&listed(&["other-bastion.example"])))
            .unwrap_err();
        assert!(err.contains("other-bastion.example"), "{err}");
        assert!(err.contains("not in this session's AllowedJumpHosts"), "{err}");

        // An empty list grants nothing, like every empty list.
        let err = bastion.permits_jump(Some(&Vec::new())).unwrap_err();
        assert!(err.contains("empty — nothing is allowed"), "{err}");

        // A port is not part of the jump policy either, but the jump's own port is carried in the
        // `-J` value — see the argv test.
        let ported = Target::parse("bastion.example:2222").expect("parsed");
        assert!(ported.permits_jump(Some(&listed(&["bastion.example"]))).is_ok());
    }

    /// The jump reaches the argv as `-J`, and its port rides along in that one value.
    ///
    /// `-J` takes a `[user@]host[:port]` list rather than a bare host, so the port has to be part of
    /// the jump argument — a separate `-p` would set the *destination's* port, which is a different
    /// thing and would look like it worked.
    #[test]
    fn a_jump_host_becomes_one_argument_carrying_its_own_port() {
        let target = Target::parse("inner.example").expect("parsed");
        let jump = Target::parse("bastion.example:2222").expect("parsed");

        let plain = ssh_args(&target, None, false, 10);
        assert!(
            !plain.contains(&"-J".to_string()),
            "no jump asked for means no -J: {plain:?}"
        );

        let routed = ssh_args(&target, Some(&jump), false, 10);
        let at = routed.iter().position(|a| a == "-J").expect("a jump must produce -J");
        assert_eq!(routed[at + 1], "bastion.example:2222", "{routed:?}");
        // The destination is still last, so `-J`'s value cannot be mistaken for it.
        assert_eq!(routed.last().map(String::as_str), Some("inner.example"), "{routed:?}");
        // Nothing about the jump introduces a `-o`, an `-i` or a tunnel — the jump is an address in
        // the same sense as the destination, passed through the same validation.
        let options = routed
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-o")
            .filter_map(|(i, _)| routed.get(i + 1))
            .collect::<Vec<_>>();
        assert_eq!(
            options,
            ["BatchMode=yes", "ConnectTimeout=10", "StrictHostKeyChecking=yes"],
            "the jump must not add an option: {routed:?}"
        );
    }

    /// A hostile jump is refused by the same parser as a hostile destination.
    ///
    /// `-J` takes a value that ssh splits on commas and at signs, so a jump beginning `-` would be an
    /// injection point in its own right — `-J -oProxyCommand=…` is the shape to refuse.
    #[tokio::test]
    async fn a_hostile_jump_is_refused_before_anything_runs() {
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy {
            allowed_hosts: Some(vec!["*.example".to_string()]),
            // Even *granting* a hostile entry must not make it usable: the value is validated as an
            // address first.
            allowed_jump_hosts: Some(vec!["-oProxyCommand=id".to_string()]),
        };
        let err = run_allowed(
            &serde_json::json!({
                "action": "command",
                "host": "inner.example",
                "jump": "-oProxyCommand=id",
                "command": "id"
            }),
            dir.path(),
            &policy,
        )
        .await
        .expect_err("must be refused");
        assert!(
            err.contains("`jump`:") && err.contains("may not begin with `-`"),
            "the jump is malformed, and the message says which field: {err}"
        );
        assert!(
            !err.contains("could not run ssh"),
            "ssh must not have been invoked: {err}"
        );
    }

    /// A jump that is not granted is refused before anything runs, and names what to add.
    #[tokio::test]
    async fn an_ungranted_jump_is_refused_with_the_key_to_add() {
        let dir = tempfile::tempdir().unwrap();
        // Destinations are restricted but no jump is granted: the case the asymmetry is about.
        let policy = Policy {
            allowed_hosts: Some(vec!["inner.example".to_string()]),
            allowed_jump_hosts: None,
        };
        let err = run_allowed(
            &serde_json::json!({
                "action": "command",
                "host": "inner.example",
                "jump": "bastion.example",
                "command": "id"
            }),
            dir.path(),
            &policy,
        )
        .await
        .expect_err("must be refused");
        assert!(err.contains("AllowedJumpHosts"), "{err}");
        assert!(err.contains("AllowedJumpHosts: bastion.example"), "{err}");
        assert!(
            !err.contains("could not run ssh"),
            "ssh must not have been invoked: {err}"
        );

        // And an empty `jump` is not a jump: the field absent and the field blank mean the same, so a
        // caller that always sends the key does not get refused for it.
        let ok = run_allowed(
            &serde_json::json!({
                "action": "command",
                "host": "inner.example",
                "jump": "  ",
                "command": "id",
                "timeout_secs": 1
            }),
            dir.path(),
            &policy,
        )
        .await;
        if let Err(e) = ok {
            assert!(
                !e.contains("AllowedJumpHosts"),
                "a blank jump is no jump, not an ungranted one: {e}"
            );
        }
    }

    /// The schema offers `jump` and says a grant is needed.
    #[test]
    fn the_schema_offers_a_jump() {
        let spec = spec();
        let properties = spec["input_schema"]["properties"].as_object().expect("properties");
        assert!(properties.contains_key("jump"), "the jump must be settable");
        let described = properties["jump"]["description"].as_str().unwrap_or_default();
        assert!(
            described.contains("AllowedJumpHosts"),
            "and the schema must say a grant is needed: {described}"
        );
        // Still no user, no key, no option.
        for forbidden in ["user", "identity", "key", "options", "tunnel", "proxy"] {
            assert!(!properties.contains_key(forbidden), "`{forbidden}` must not be offered");
        }
    }
}
