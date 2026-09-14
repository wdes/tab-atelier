// SPDX-License-Identifier: MPL-2.0

//! CLI parsing for the proxy. Command execution lives in [`cli`].

use clap::{Parser, Subcommand};

mod cli;

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
    /// Time the round trip to the Anthropic API, stage by stage.
    Ping {
        /// Model to ask for the one-token completion.
        #[arg(long, default_value = "claude-haiku-4-5-20251001")]
        model: String,
        /// Repeat this many times and report the spread.
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Add an account and print its key. The key is shown once.
    Add {
        first_name: String,
        last_name: String,
        email: String,
    },
    /// List accounts.
    List,
    /// Add a named key to an account, printing it once.
    AddKey {
        who: String,
        /// What it is for — `laptop`, `ci`, `fleet`. Names are how keys get
        /// revoked later, so a vague one costs you at exactly the wrong time.
        name: String,
    },
    /// List an account's keys, with when and where each was last used.
    Keys { who: String },
    /// Delete one key by name or id. The account's other keys keep working.
    RemoveKey { who: String, key: String },
    /// Refuse an account's key without deleting the account.
    Disable { who: String },
    /// Let a disabled account back in.
    Enable { who: String },
    /// Delete an account.
    Remove { who: String },
    /// Route every request from this account through one provider.
    ///
    /// An empty provider clears the pin and returns the account to normal
    /// routing. Enforced, not preferred: an account pinned to a provider that
    /// is unavailable gets a 503 rather than a quiet fall back to the
    /// subscription the operator was keeping it off.
    SetProvider {
        /// Account email or id.
        who: String,
        /// Provider id, as listed in `providers.json`. Empty clears the pin.
        #[arg(default_value = "")]
        provider: String,
    },
    /// Install a Claude login copied from a machine that can run `claude`.
    ///
    /// The proxy talks to Anthropic with the host's own Claude OAuth
    /// credentials, and a headless server cannot complete that login. Copy
    /// `~/.claude/.credentials.json` from a machine that can:
    ///
    ///   ssh proxy-host tab-atelier-proxy import-credentials < ~/.claude/.credentials.json
    ///
    /// Re-run it whenever the proxy reports "OAuth access token has been
    /// revoked": a refresh rotates the refresh token, so logging in again
    /// anywhere else invalidates this copy.
    ImportCredentials {
        /// Read from this file instead of stdin.
        #[arg(long, value_name = "PATH")]
        from: Option<std::path::PathBuf>,
    },
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { listen } => cli::serve(&listen),
        Command::AdminToken => {
            println!("{}", cli::admin_token_migrated()?);
            Ok(())
        }
        Command::Ping { model, count } => cli::ping(&model, count),
        Command::Add {
            first_name,
            last_name,
            email,
        } => {
            let mut store = cli::store()?;
            let account = store
                .add(&first_name, &last_name, &email)
                .map_err(|error| error.to_string())?;
            println!("added {} <{}>", account.display_name(), account.email);
            println!("  next: tab-atelier-proxy add-key {} <laptop|ci|...>", account.email);
            Ok(())
        }
        Command::List => {
            let store = cli::store()?;
            if store.accounts().is_empty() {
                println!("no accounts yet — `tab-atelier-proxy add <first> <last> <email>`");
            } else {
                for account in store.accounts() {
                    cli::print_account(account);
                }
            }
            Ok(())
        }
        Command::AddKey { who, name } => {
            let mut store = cli::store()?;
            let (key, secret) = store.add_key(&who, &name).map_err(|error| error.to_string())?;
            let account = store.find(&who).ok_or("account vanished")?.clone();
            println!("{} <{}> — key {:?}", account.display_name(), account.email, key.name);
            println!("  key: {secret}");
            println!("  This is the only time it is shown — the proxy stores a hash.");
            println!("  On the machine that will use it:");
            println!("    tab-atelier remote add --label proxy --url https://<proxy-host> \\");
            println!("        --relay-token {secret}");
            Ok(())
        }
        Command::Keys { who } => cli::list_keys(&who),
        Command::RemoveKey { who, key } => cli::remove_key(&who, &key),
        Command::Disable { who } => cli::set_disabled(&who, true),
        Command::Enable { who } => cli::set_disabled(&who, false),
        Command::SetProvider { who, provider } => cli::set_provider(&who, &provider),
        Command::Remove { who } => cli::remove_account(&who),
        Command::ImportCredentials { from } => cli::import_credentials(from.as_deref()),
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
    use tab_atelier_proxy::now_rfc3339_at;

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
