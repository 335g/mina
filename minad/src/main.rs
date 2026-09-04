//! minad: minae の常駐デーモン。
//!
//! エディタ状態（documents / histories / selections / LSP sessions）を所有し、
//! `minae` (TUI)・`minas` (agent CLI) からの接続を受け付ける。
//! 起動: `minad serve`（不在時はクライアントが自動起動する）。

mod daemon;
mod languages;
mod lsp;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "minad", version, about = "Resident daemon for minae")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the resident daemon
    Serve,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => daemon::run().await,
    }
}
