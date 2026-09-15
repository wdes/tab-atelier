// SPDX-License-Identifier: MPL-2.0

//! Execution of proxy CLI commands. Argument parsing stays in `main.rs`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tab_atelier_proxy::now_rfc3339;
use tab_atelier_proxy::users::{Account, Store};
use tab_atelier_proxy::{admin_token, config_dir, server, state_dir, web_root};

/// Pin an account to a provider, or clear the pin.
pub fn set_provider(who: &str, provider: &str) -> Result<(), String> {
    let mut s = store()?;
    let wanted = provider.trim();
    // Validated here, where the registry is in hand: a pin to a typo would
    // otherwise be a 503 nobody could explain.
    if !wanted.is_empty() {
        let path = config_dir()?.join("providers.json");
        let reg = tab_atelier_proxy::provider::Registry::load(&path);
        if reg.get(wanted).is_none() {
            return Err(format!(
                "no provider {wanted:?} in {} — configured: {}",
                path.display(),
                reg.providers
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let a = s
        .set_provider(who, (!wanted.is_empty()).then_some(wanted))
        .map_err(|e| e.to_string())?;
    if wanted.is_empty() {
        println!("{} <{}> — normal routing", a.display_name(), a.email);
    } else {
        println!("{} <{}> — every request goes to {wanted}", a.display_name(), a.email);
    }
    Ok(())
}

/// How often to ask Anthropic how much of the plan has been used.
///
/// Five minutes: the five-hour window moves slowly enough that finer polling
/// buys nothing, and this is a call against the same quota it is measuring.
const USAGE_POLL: std::time::Duration = std::time::Duration::from_mins(5);

/// Longest gap between polls once they start failing.
///
/// An hour, because the failures worth backing off from are not transient:
/// observed in production, `/api/oauth/usage` answered 429 and then every
/// five-minute poll for the next ten hours failed too, first on the rate limit
/// and later on an expired refresh token. Retrying an endpoint that is telling
/// us to stop, at full rate, against the same quota it measures, is the one
/// thing guaranteed not to help.
const USAGE_POLL_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_hours(1);

/// How long to wait after `failures` consecutive failed polls.
///
/// Doubles from the normal interval and stops at [`USAGE_POLL_MAX_BACKOFF`];
/// zero failures is the normal interval, so the healthy path is unchanged.
fn usage_poll_delay(failures: u32) -> std::time::Duration {
    if failures == 0 {
        return USAGE_POLL;
    }
    USAGE_POLL
        .saturating_mul(1u32 << failures.min(5))
        .min(USAGE_POLL_MAX_BACKOFF)
}

/// Keep the shared plan's utilisation current, in the background.
///
/// Every reading is appended — successes and failures alike — so a gap in the
/// log always means "the monitor was not running", never "a call failed and
/// nobody said so". Same JSONL shape as
/// `.claude/scripts/claude-usage-monitor.mjs`, so tooling that reads those
/// files reads these.
async fn poll_account_usage(state: Arc<server::State>) {
    let mut failures: u32 = 0;
    loop {
        let sample = tokio::task::spawn_blocking(|| {
            let ts = now_rfc3339();
            match tab_atelier_proxy::egress::account_usage() {
                Ok((status, body)) => tab_atelier_proxy::account::parse_usage(status, &body, ts),
                Err(e) => tab_atelier_proxy::account::Sample {
                    ts,
                    error: Some(e),
                    ..tab_atelier_proxy::account::Sample::default()
                },
            }
        })
        .await;
        if let Ok(sample) = sample {
            match sample.utilization() {
                Some(u) => log::info!(
                    "plan utilisation: {:.0}%",
                    tab_atelier_proxy::account::as_fraction(u) * 100.0
                ),
                None => log::warn!("plan utilisation unknown: {:?}", sample.error),
            }
            failures = if sample.is_ok() { 0 } else { failures.saturating_add(1) };
            state
                .account
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(sample);
        } else {
            // The blocking task panicked. Nothing was appended, so without
            // this the loop would spin at full rate on a fault it never
            // records — the one shape of failure the JSONL cannot show.
            log::error!("plan utilisation poll panicked");
            failures = failures.saturating_add(1);
        }
        let delay = usage_poll_delay(failures);
        if failures > 0 {
            log::warn!(
                "plan utilisation: {failures} consecutive failures, next poll in {}s",
                delay.as_secs()
            );
        }
        tokio::time::sleep(delay).await;
    }
}

/// Install a Claude login copied from a machine that could perform it.
///
/// Reads the file, or stdin when there is none — piping is the natural shape
/// for this, since the credential should not be sitting in a second file on
/// the way over.
pub fn import_credentials(from: Option<&std::path::Path>) -> Result<(), String> {
    let raw = if let Some(path) = from {
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?
    } else {
        use std::io::Read as _;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("read stdin: {e}"))?;
        buf
    };
    let imported = tab_atelier_proxy::egress::import_credentials(&raw)?;
    println!("installed {}", imported.path.display());
    // The access token's expiry is the cheap proof that a LIVE credential
    // arrived rather than a stale file someone had lying around — the most
    // likely mistake when copying between machines.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    if imported.expires_at > now {
        println!(
            "  access token valid for another {} min",
            (imported.expires_at - now) / 60_000
        );
    } else {
        println!("  access token already expired — the refresh token will be used on the next call");
    }
    if !imported.scopes.is_empty() {
        println!("  scopes: {}", imported.scopes.join(" "));
    }
    println!("restart the service so the running proxy picks it up:");
    println!("  systemctl restart tab-atelier-proxy");
    Ok(())
}

/// Print what routing would DO for a request that asked for each canonical
/// model, and report whether anything had nowhere to go.
///
/// This is the question an operator has after adding a provider or writing a
/// mapping, and the one thing the probes above cannot answer: they exercise
/// the subscription's credential directly rather than the decision that picks
/// a destination. Returns true when something could not be routed.
fn report_routing(registry: &tab_atelier_proxy::provider::Registry) -> bool {
    // Healthy throughout: this is an offline check, and a provider in backoff
    // would otherwise read as misconfigured.
    let healthy = |_: &str| tab_atelier_proxy::routing::Health::default();
    let mut failed = false;
    println!("\nrouting (healthy providers)");
    for model in ["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5-20251001"] {
        // No account, so no pin: this is what an unpinned person gets.
        let chosen = tab_atelier_proxy::routing::choose(
            registry,
            model,
            None,
            // Work, not the classifier: `doctor` reports where conversations
            // go. The classifier is routed off the raw name and would only
            // confuse the table.
            tab_atelier_proxy::classifier::Kind::Work,
            &healthy,
            |v| std::env::var(v).ok(),
            tab_atelier_proxy::usage::now_secs(),
        );
        let Some(route) = chosen else {
            println!("  {model:<26} -> NOTHING can serve this");
            failed = true;
            continue;
        };
        let why = match route.reason {
            Some(r) => format!(" ({r} from {})", route.changed_from.as_deref().unwrap_or("?")),
            None => String::new(),
        };
        println!("  {:<26} -> {}/{}{why}", model, route.provider_id, route.model_id);
    }
    if registry.mappings.is_empty() {
        println!("  no mappings configured");
    }
    failed
}

/// Time the trip to Anthropic and print where it went.
///
/// Exits non-zero if any stage failed, so it is usable as a health check in a
/// script or a monitoring probe rather than only by eye.
pub fn ping(model: &str, count: u32) -> Result<(), String> {
    println!("upstream: {}", tab_atelier_proxy::egress::upstream());
    let mut round_trips = Vec::new();
    let mut failed = false;
    for i in 0..count.max(1) {
        if count > 1 {
            println!("\n— probe {} of {count}", i + 1);
        }
        for p in tab_atelier_proxy::egress::probe_round_trip(model) {
            let ms = p.elapsed.as_secs_f64() * 1000.0;
            let status = p.status.map_or_else(|| "   —".to_owned(), |s| format!("{s:>4}"));
            println!("  {:<12} {status}  {ms:>8.0} ms   {}", p.label, p.note);
            if p.note.starts_with("FAILED") || p.status.is_some_and(|s| !(200..300).contains(&s)) {
                failed = true;
            }
            if p.label == "round trip" {
                round_trips.push(ms);
            }
        }
    }
    if round_trips.len() > 1 {
        // Min and max, not an average: what people feel is the slow one, and a
        // mean hides it behind the fast ones.
        let min = round_trips.iter().copied().fold(f64::INFINITY, f64::min);
        let max = round_trips.iter().copied().fold(0.0_f64, f64::max);
        // A probe count is a handful; f64::from a u32 is exact and needs no cast lint.
        let mean = round_trips.iter().sum::<f64>() / f64::from(u32::try_from(round_trips.len()).unwrap_or(1));
        println!(
            "\nround trip over {} probes: min {min:.0} ms · mean {mean:.0} ms · max {max:.0} ms",
            round_trips.len()
        );
    }
    // Every other provider, with its OWN credential. The stages above answer
    // "is Anthropic reachable"; this answers "does the second provider work",
    // which is the question an operator has just created by adding one and
    // the one that otherwise stays invisible until a reroute fails mid-turn.
    let providers_path = config_dir()?.join("providers.json");
    let registry = tab_atelier_proxy::provider::Registry::load(&providers_path);
    let others: Vec<_> = registry
        .providers
        .iter()
        .filter(|p| !matches!(p.auth, tab_atelier_proxy::provider::Auth::ClaudeOauth))
        .collect();
    if !others.is_empty() {
        println!(
            "
providers ({})",
            providers_path.display()
        );
        for p in others {
            let state = if !p.enabled {
                "disabled"
            } else if p.credential_ready() {
                "enabled"
            } else {
                "NO CREDENTIAL"
            };
            println!("  {:<14} {state}", p.id);
            if !p.enabled || !p.credential_ready() {
                failed = true;
                continue;
            }
            // One model per provider, so a multi-model provider costs one
            // probe and not a bill. The first current one is representative:
            // what this measures is reachability and authentication.
            let Some(model) = p
                .models
                .iter()
                .find(|m| !m.deprecated && m.class == tab_atelier_proxy::provider::Class::Fast)
                .or_else(|| p.models.iter().find(|m| !m.deprecated))
            else {
                println!("    no current model to probe");
                failed = true;
                continue;
            };
            let probe = tab_atelier_proxy::egress::probe_provider(p, &model.id);
            let ms = probe.elapsed.as_secs_f64() * 1000.0;
            let status = probe.status.map_or_else(|| "   —".to_owned(), |s| format!("{s:>4}"));
            println!("    {:<12} {status}  {ms:>8.0} ms   {}", probe.label, probe.note);
            if probe.note.starts_with("FAILED") || probe.status.is_some_and(|s| !(200..300).contains(&s)) {
                failed = true;
            }
        }
    }

    failed |= report_routing(&registry);

    if failed {
        return Err("at least one stage failed — see above".to_owned());
    }
    Ok(())
}

/// Move a secret that used to live in the state directory.
///
/// Both files moved when secrets were split out of `~/.local/state`, and
/// missing one is worse than missing both: `admin.token` is MINTED when
/// absent, so an unmigrated upgrade silently issues a new token and the one
/// the operator wrote down starts answering "admin token required".
fn migrate_secret(name: &str) -> Result<std::path::PathBuf, String> {
    let dir = config_dir()?;
    let path = dir.join(name);
    let legacy = state_dir().map(|d| d.join(name)).ok().filter(|p| p.exists());
    if let (false, Some(legacy)) = (path.exists(), legacy) {
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        if std::fs::rename(&legacy, &path).is_ok() {
            log::info!("moved {name} to {} (secrets belong under config)", path.display());
        }
    }
    Ok(path)
}

pub fn store() -> Result<Store, String> {
    // Two possible homes for the file that decides access is exactly the
    // ambiguity worth spending a rename to kill.
    let path = migrate_secret("users.json")?;
    Store::load(path).map_err(|e| e.to_string())
}

/// The admin token, after moving it out of the old location if it is there.
pub fn admin_token_migrated() -> Result<String, String> {
    migrate_secret("admin.token")?;
    admin_token(&config_dir()?)
}

/// "3 min ago" — relative, so no timezone has to be decided here.
fn ago(t: u64) -> String {
    let secs = tab_atelier_proxy::users::now_secs().saturating_sub(t);
    match secs {
        0..=90 => "just now".to_owned(),
        91..=5400 => format!("{} min ago", secs / 60),
        5401..=172_800 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86400),
    }
}

/// Every key an account holds, and when each was last seen.
pub fn list_keys(who: &str) -> Result<(), String> {
    let store = store()?;
    let account = store.find(who).ok_or_else(|| format!("no such account: {who}"))?;
    if account.keys.is_empty() {
        println!("{} has no keys — `add-key {who} <name>`", account.display_name());
        return Ok(());
    }
    for key in &account.keys {
        let state = if key.disabled { "disabled" } else { "active" };
        let first = key.first_used_at.map_or_else(|| "never used".to_owned(), ago);
        let from = key.last_used_ip.as_deref().unwrap_or("-");
        println!("  {:<16} {state:<9} first {first:<14} last from {from}", key.name);
    }
    Ok(())
}

/// Revoke one key, leaving the account's others alone.
pub fn remove_key(who: &str, key: &str) -> Result<(), String> {
    let mut store = store()?;
    let removed = store.remove_key(who, key).map_err(|error| error.to_string())?;
    println!(
        "removed key {:?} — the account's other keys are unaffected",
        removed.name
    );
    Ok(())
}

/// Switch an account off, or back on.
pub fn set_disabled(who: &str, disabled: bool) -> Result<(), String> {
    let mut store = store()?;
    let account = store.set_disabled(who, disabled).map_err(|error| error.to_string())?;
    if disabled {
        println!(
            "disabled {} <{}> — their key is refused from now on",
            account.display_name(),
            account.email
        );
    } else {
        println!(
            "enabled {} <{}> — their existing key works again",
            account.display_name(),
            account.email
        );
    }
    Ok(())
}

/// Remove an account and everything under it.
pub fn remove_account(who: &str) -> Result<(), String> {
    let mut store = store()?;
    let account = store.remove(who).map_err(|error| error.to_string())?;
    println!("removed {} <{}>", account.display_name(), account.email);
    Ok(())
}

pub fn print_account(a: &Account) {
    let state = if a.disabled {
        "disabled"
    } else if a.keys.iter().any(tab_atelier_proxy::users::Key::active) {
        "active"
    } else {
        "no key"
    };
    let last = a
        .keys
        .iter()
        .filter_map(|k| k.last_used_at)
        .max()
        .map_or_else(|| "never used".to_owned(), ago);
    let keys = a.keys.len();
    println!(
        "{:<30}  {:<26}  {state:<8}  {keys} key(s)  {last}",
        a.display_name(),
        a.email
    );
}

/// The one moment a key exists in readable form. Say so, rather than letting
/// someone discover it when they come back for it.
/// Start the proxy and serve until Ctrl-C.
///
/// # Errors
/// The listen address is unusable, the state directories cannot be read, or
/// the runtime fails to start.
pub fn serve(listen: &str) -> Result<(), String> {
    let addr: SocketAddr = listen.parse().map_err(|e| format!("bad --listen {listen}: {e}"))?;
    let state_path = state_dir()?;
    let token = admin_token_migrated()?;
    let store = store()?;

    // First run leaves an editable providers.json rather than a mystery: the
    // default is the subscription alone, which is what the proxy did before
    // providers existed.
    let providers_path = config_dir()?.join("providers.json");
    let registry = tab_atelier_proxy::provider::Registry::load(&providers_path);
    if !providers_path.exists() {
        let _ = registry.save(&providers_path);
        log::info!("wrote {} — edit it to add providers", providers_path.display());
    }
    // Say where the token was read from: "admin token required" with no idea
    // which file the server is actually using is a miserable thing to debug.
    log::info!("admin token: {}", config_dir()?.join("admin.token").display());
    log::info!(
        "routing across {} provider(s): {:?}",
        registry.providers.len(),
        registry.summary()
    );

    let root = web_root();
    if root.is_none() {
        log::warn!("no web UI found — account management is CLI-only in this install");
    }
    let state = Arc::new(server::State {
        store: Mutex::new(store),
        usage: Mutex::new(tab_atelier_proxy::usage::Store::load(state_path.join("usage"))),
        sched: Mutex::new(tab_atelier_proxy::qos::Sched::new()),
        account: Mutex::new(tab_atelier_proxy::account::Monitor::load(&state_path)),
        inspect: Mutex::new(tab_atelier_proxy::inspect::Store::load(&state_path)),
        wake: tokio::sync::Notify::new(),
        registry: Mutex::new(registry),
        registry_path: providers_path,
        provider_backoff: Mutex::new(std::collections::BTreeMap::new()),
        admin_token: token,
        web_root: root,
    });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    rt.block_on(async {
        tokio::spawn(poll_account_usage(Arc::clone(&state)));
        tokio::select! {
            r = server::serve(addr, state) => r,
            _ = tokio::signal::ctrl_c() => {
                log::info!("shutting down");
                Ok(())
            }
        }
    })
}

/// Create an account, and say what to do next.
///
/// The second line names the command that mints the first key. An account with
/// no key authenticates nobody, so a bare "added" reads as a finished setup
/// when it is a half-finished one.
pub fn add_account(first: &str, last: &str, email: &str) -> Result<(), String> {
    let mut store = store()?;
    let account = store.add(first, last, email).map_err(|error| error.to_string())?;
    println!("added {} <{}>", account.display_name(), account.email);
    println!("  next: tab-atelier-proxy add-key {} <laptop|ci|...>", account.email);
    Ok(())
}

/// Every account, one per line.
pub fn list_accounts() -> Result<(), String> {
    let store = store()?;
    if store.accounts().is_empty() {
        println!("no accounts yet — `tab-atelier-proxy add <first> <last> <email>`");
        return Ok(());
    }
    for account in store.accounts() {
        print_account(account);
    }
    Ok(())
}

/// Mint a key, and print the secret once.
///
/// The secret is not recoverable — the store keeps a hash — so a client that
/// loses this output has to mint another. The instructions say so, because the
/// failure otherwise appears later as an unexplained 401.
pub fn mint_key(who: &str, name: &str) -> Result<(), String> {
    let mut store = store()?;
    let (key, secret) = store.add_key(who, name).map_err(|error| error.to_string())?;
    let display = store.find(who).map_or_else(|| who.to_owned(), Account::display_name);
    println!("{display} <{who}> — key {:?}", key.name);
    println!("  key: {secret}");
    println!("  This is the only time it is shown — the proxy stores a hash.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_poll_interval_is_the_normal_one_before_any_failure() {
        assert_eq!(usage_poll_delay(0), USAGE_POLL);
    }

    /// The interval doubles from five minutes, and the hour cap stops it early.
    ///
    /// Doubling 300s four times reaches 4800s, past the 3600s ceiling, so the
    /// ladder is 10, 20, 40 and then flat at 60 minutes rather than the eight
    /// steps the `min(5)` alone would suggest. Asserting the cap from the fifth
    /// step onwards is the point: a test that expected `USAGE_POLL * 32` here
    /// would fail while the backoff was working correctly.
    #[test]
    fn the_poll_interval_doubles_until_the_ceiling_stops_it() {
        assert_eq!(usage_poll_delay(1), std::time::Duration::from_mins(10));
        assert_eq!(usage_poll_delay(2), std::time::Duration::from_mins(20));
        assert_eq!(usage_poll_delay(3), std::time::Duration::from_mins(40));

        // 300s * 2^4 = 4800s, which the cap truncates; everything after is flat.
        for failures in [4_u32, 5, 32, 1_000, u32::MAX] {
            assert_eq!(
                usage_poll_delay(failures),
                USAGE_POLL_MAX_BACKOFF,
                "failures={failures} should sit at the ceiling"
            );
        }
    }

    /// A long outage saturates rather than wrapping.
    ///
    /// The shift is `1u32 << failures.min(5)`. Without the `min` — or with a
    /// wider shift — a large failure count wraps, producing a delay of zero and
    /// turning a backoff into a hot loop against an upstream that is already
    /// failing. A zero delay would also make this loop spin the CPU.
    #[test]
    fn a_long_outage_saturates_the_backoff_instead_of_wrapping() {
        for failures in [1_u32, 5, 6, 100, 1_000, u32::MAX] {
            let delay = usage_poll_delay(failures);
            assert!(
                delay >= USAGE_POLL && delay <= USAGE_POLL_MAX_BACKOFF,
                "failures={failures} produced {delay:?}, outside [1x, cap]"
            );
        }
        assert_eq!(usage_poll_delay(u32::MAX), USAGE_POLL_MAX_BACKOFF);
    }
}
