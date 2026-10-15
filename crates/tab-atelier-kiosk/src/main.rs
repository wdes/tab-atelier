// SPDX-License-Identifier: MPL-2.0

use clap::{Parser, Subcommand};

use tab_atelier_kiosk::{Config, DEFAULT_BIND, DEFAULT_UPSTREAM};

#[derive(Parser)]
#[command(
    name = "tab-atelier-kiosk",
    version,
    about = "Web Kiosk: serves the interface and relays the rest to the daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the kiosk server.
    Serve {
        /// Address to listen on.
        #[arg(long, default_value = DEFAULT_BIND)]
        listen: String,
        /// Daemon base URL to relay every other path to.
        #[arg(long, default_value = DEFAULT_UPSTREAM)]
        upstream: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), String> {
    match Cli::parse().command {
        Command::Serve { listen, upstream } => {
            let cfg = Config::new(&upstream, &listen)?;
            println!("tab-atelier-kiosk on http://{}{}", cfg.bind, uri_tail(&cfg));
            match (&cfg.token, &cfg.token_file) {
                // The path, never the token: console output ends up in logs and
                // scrollback, and the file is where an operator goes to read it.
                (Some(_), Some(path)) => println!("  board token file: {}", path.display()),
                (Some(_), None) => println!("  board token: from TAB_ATELIER_KIOSK_TOKEN"),
                (None, _) => println!("  board token: none — the panes answer 401"),
            }
            tab_atelier_kiosk::run(cfg).await.map_err(|err| err.to_string())
        }
    }
}

/// The `?token=` tail for the startup line, so the printed URL is
/// copy-pasteable.
///
/// This one line DOES carry the token, on purpose: it is this process's own
/// console, the same place a one-time password is shown, and making an operator
/// dig the secret out of a file is exactly why the first-start token went
/// unnoticed. The line above it still names the file, for anyone reading a log.
fn uri_tail(cfg: &Config) -> String {
    cfg.token
        .as_ref()
        .map_or_else(String::new, |token| format!("/?token={token}"))
}
