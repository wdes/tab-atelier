// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI for the proxy: run it, and manage the people allowed to use it.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use tab_atelier_proxy::users::{Account, Store};
use tab_atelier_proxy::{admin_token, config_dir, server, state_dir, web_root};

#[derive(Parser)]
#[command(
    name = "tab-atelier-proxy",
    version,
    about = "Multi-user Anthropic API proxy for tab-atelier"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy.
    Serve {
        /// Address to listen on. Defaults to loopback: put a TLS terminator in
        /// front rather than exposing this directly, since it carries keys.
        #[arg(long, default_value = "127.0.0.1:7900")]
        listen: String,
    },
    /// Print the admin token (minting it on first run).
    AdminToken,
    /// Add an account and print its key. The key is shown once.
    Add {
        first_name: String,
        last_name: String,
        email: String,
    },
    /// List accounts.
    List,
    /// Replace an account's key, invalidating the old one.
    Rotate { who: String },
    /// Refuse an account's key without deleting the account.
    Disable { who: String },
    /// Let a disabled account back in.
    Enable { who: String },
    /// Delete an account.
    Remove { who: String },
}

/// How often to ask Anthropic how much of the plan is left.
///
/// Five minutes: the five-hour window moves slowly enough that finer polling
/// buys nothing, and this is a call against the same quota it is measuring.
const USAGE_POLL: std::time::Duration = std::time::Duration::from_mins(5);

/// Keep the shared plan's utilisation current, in the background.
///
/// Every reading is appended — successes and failures alike — so a gap in the
/// log always means "the monitor was not running", never "a call failed and
/// nobody said so". Same JSONL shape as
/// `.claude/scripts/claude-usage-monitor.mjs`, so tooling that reads those
/// files reads these.
async fn poll_account_usage(state: Arc<server::State>) {
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
            state
                .account
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(sample);
        }
        tokio::time::sleep(USAGE_POLL).await;
    }
}

/// RFC3339 in UTC, without pulling in a date library for one format string.
fn now_rfc3339() -> String {
    now_rfc3339_at(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
}

/// The conversion, taking the instant, so it can be tested against known
/// dates instead of whatever the clock says.
fn now_rfc3339_at(secs: u64) -> String {
    let (hour, minute, second) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    // Civil-from-days (Howard Hinnant's algorithm), so the timestamp is a real
    // date rather than an epoch count nobody can read on a dashboard.
    let shifted = i64::try_from(secs / 86_400).unwrap_or(0) + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_pos = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_pos + 2) / 5 + 1;
    let month = if month_pos < 10 { month_pos + 3 } else { month_pos - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn store() -> Result<Store, String> {
    let dir = config_dir()?;
    let path = dir.join("users.json");
    // The accounts used to sit in the state dir. Move them once rather than
    // reading from either place forever — two possible homes for the file that
    // decides access is exactly the ambiguity worth spending a rename to kill.
    let legacy = state_dir().map(|d| d.join("users.json")).ok().filter(|p| p.exists());
    if let (false, Some(legacy)) = (path.exists(), legacy) {
        let _ = std::fs::create_dir_all(&dir);
        if std::fs::rename(&legacy, &path).is_ok() {
            log::info!("moved accounts to {} (secrets belong under config)", path.display());
        }
    }
    Store::load(path).map_err(|e| e.to_string())
}

fn print_account(a: &Account) {
    let state = if a.disabled {
        "disabled"
    } else if a.key_hash.is_empty() {
        "no key"
    } else {
        "active"
    };
    let last = a.last_used_at.map_or_else(
        || "never used".to_owned(),
        |t| {
            let ago = tab_atelier_proxy::users::now_secs().saturating_sub(t);
            match ago {
                0..=90 => "just now".to_owned(),
                91..=5400 => format!("{} min ago", ago / 60),
                5401..=172_800 => format!("{} h ago", ago / 3600),
                _ => format!("{} d ago", ago / 86400),
            }
        },
    );
    println!("{:<38}  {:<28}  {state:<8}  {last}", a.display_name(), a.email);
}

/// The one moment a key exists in readable form. Say so, rather than letting
/// someone discover it when they come back for it.
fn print_new_key(a: &Account, key: &str) {
    println!("{} <{}>", a.display_name(), a.email);
    println!("  key: {key}");
    println!("  This is the only time it is shown — the proxy stores a hash. Lost it? `rotate`.");
    println!("  On the machine that will use it:");
    println!("    tab-atelier remote add --label proxy --url https://<proxy-host> \\");
    println!("        --relay-token {key}");
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { listen } => {
            let addr: SocketAddr = listen.parse().map_err(|e| format!("bad --listen {listen}: {e}"))?;
            let state = state_dir()?;
            let token = admin_token(&config_dir()?)?;
            let store = store()?;
            let root = web_root();
            if root.is_none() {
                log::warn!("no web UI found — account management is CLI-only in this install");
            }
            let state = Arc::new(server::State {
                store: Mutex::new(store),
                usage: Mutex::new(tab_atelier_proxy::usage::Store::load(state.join("usage.json"))),
                sched: Mutex::new(tab_atelier_proxy::qos::Sched::new()),
                account: Mutex::new(tab_atelier_proxy::account::Monitor::load(&state)),
                wake: tokio::sync::Notify::new(),
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
        Command::AdminToken => {
            println!("{}", admin_token(&config_dir()?)?);
            Ok(())
        }
        Command::Add {
            first_name,
            last_name,
            email,
        } => {
            let mut s = store()?;
            let (a, key) = s.add(&first_name, &last_name, &email).map_err(|e| e.to_string())?;
            print_new_key(&a, &key);
            Ok(())
        }
        Command::List => {
            let s = store()?;
            if s.accounts().is_empty() {
                println!("no accounts yet — `tab-atelier-proxy add <first> <last> <email>`");
                return Ok(());
            }
            for a in s.accounts() {
                print_account(a);
            }
            Ok(())
        }
        Command::Rotate { who } => {
            let mut s = store()?;
            let (a, key) = s.rotate(&who).map_err(|e| e.to_string())?;
            print_new_key(&a, &key);
            Ok(())
        }
        Command::Disable { who } => {
            let mut s = store()?;
            let a = s.set_disabled(&who, true).map_err(|e| e.to_string())?;
            println!(
                "disabled {} <{}> — their key is refused from now on",
                a.display_name(),
                a.email
            );
            Ok(())
        }
        Command::Enable { who } => {
            let mut s = store()?;
            let a = s.set_disabled(&who, false).map_err(|e| e.to_string())?;
            println!(
                "enabled {} <{}> — their existing key works again",
                a.display_name(),
                a.email
            );
            Ok(())
        }
        Command::Remove { who } => {
            let mut s = store()?;
            let a = s.remove(&who).map_err(|e| e.to_string())?;
            println!("removed {} <{}>", a.display_name(), a.email);
            Ok(())
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(e) = run() {
        eprintln!("tab-atelier-proxy: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::now_rfc3339_at;

    /// Every timestamp this proxy writes carries a zone.
    ///
    /// The sample log is read by other tools (and by a browser that renders it
    /// in the viewer's zone), so a bare local-looking timestamp would be
    /// ambiguous by exactly the offset of whoever wrote it. `Z` says UTC.
    ///
    /// The conversion is hand-rolled — a date library for one format string is
    /// not worth the dependency — which is precisely why it is pinned here
    /// against known instants rather than trusted.
    #[test]
    fn timestamps_are_utc_and_say_so() {
        // Epoch itself, a leap day, and a date past 2038 (the proxy outlives
        // 32-bit time_t, and the arithmetic is u64 throughout).
        for (secs, expect) in [
            (0_u64, "1970-01-01T00:00:00Z"),
            (1_709_164_800, "2024-02-29T00:00:00Z"),
            (1_767_225_599, "2025-12-31T23:59:59Z"),
            (2_524_608_000, "2050-01-01T00:00:00Z"),
            (1_757_320_000, "2025-09-08T08:26:40Z"),
        ] {
            assert_eq!(now_rfc3339_at(secs), expect, "for {secs}");
        }
        assert!(now_rfc3339_at(0).ends_with('Z'), "the zone designator is not optional");
    }
}
