// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI for the proxy: run it, and manage the people allowed to use it.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use tab_atelier_proxy::users::{Account, Store};
use tab_atelier_proxy::{admin_token, server, state_dir, web_root};

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

fn store() -> Result<Store, String> {
    let dir = state_dir()?;
    Store::load(dir.join("users.json")).map_err(|e| e.to_string())
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
    println!("    tab-atelier remote add proxy --url https://<proxy-host> --relay-token {key}");
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { listen } => {
            let addr: SocketAddr = listen.parse().map_err(|e| format!("bad --listen {listen}: {e}"))?;
            let dir = state_dir()?;
            let token = admin_token(&dir)?;
            let store = store()?;
            let root = web_root();
            if root.is_none() {
                log::warn!("no web UI found — account management is CLI-only in this install");
            }
            let state = Arc::new(server::State {
                store: Mutex::new(store),
                usage: Mutex::new(tab_atelier_proxy::usage::Store::load(dir.join("usage.json"))),
                admin_token: token,
                web_root: root,
            });
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("runtime: {e}"))?;
            rt.block_on(async {
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
            let dir = state_dir()?;
            println!("{}", admin_token(&dir)?);
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
