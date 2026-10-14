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
            tab_atelier_kiosk::run(cfg).await.map_err(|err| err.to_string())
        }
    }
}
