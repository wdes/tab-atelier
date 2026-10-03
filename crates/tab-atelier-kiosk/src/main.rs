// SPDX-License-Identifier: MPL-2.0

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tab-atelier-kiosk", version, about = "Web kiosk for tab-atelier")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the kiosk server.
    Serve {
        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:8282")]
        listen: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { listen } => tab_atelier_kiosk::serve(&listen).await,
    }
}
