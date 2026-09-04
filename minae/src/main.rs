//! minae: daemon/client 分割のターミナルエディタ（bin クレート）。
//!
//! モード:
//! - `minae daemon serve` — 常駐デーモン（クライアントから自動起動されることもある）
//! - `minae session <get|exec|edit|apply|wait|hints|peek|rename|references|outline|at>` — agent 用ヘッドレス CLI
//! - `minae config <show|path|edit|set|get|init>` — ユーザー設定の確認・編集
//! - `minae open [file]` — ファイル編集 TUI（`tui` feature を付けてビルドした場合のみ）
//!
//! デフォルトビルド（`cargo install minae`）はエージェント用（daemon + session + skill）で
//! TUI を含まない。TUI を使うには `cargo install minae --features tui`（ADR-0033）。

#[cfg(feature = "tui")]
mod client;
#[cfg(feature = "tui")]
mod colorscheme;
mod config;
mod daemon;
#[cfg(feature = "tui")]
mod keymap;
mod languages;
mod lsp;
#[cfg(feature = "tui")]
mod render;
mod session;
mod skill;

#[cfg(feature = "tui")]
use std::io;
#[cfg(feature = "tui")]
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "minae", version, about = "Terminal editor with a daemon/client split")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Resident daemon
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Headless CLI for agents (connect to the daemon and run commands)
    Session {
        #[command(subcommand)]
        cmd: session::SessionCmd,
    },
    /// View and edit user settings (config.toml)
    Config {
        #[command(subcommand)]
        cmd: config::ConfigCmd,
    },
    /// Agent skill guides: `minae skill` lists topics, `minae skill <topic>` prints one
    Skill {
        /// Topic to print (omit for the index)
        topic: Option<String>,
    },
    /// Open the file-editing TUI (built with --features tui)
    #[cfg(feature = "tui")]
    Open {
        /// File to open (empty buffer when omitted)
        path: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start the resident daemon
    Serve,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        // エラーは「Error: <message>」で stderr に、終了コード 1（agent は $? で判定）
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon { cmd: DaemonCmd::Serve } => daemon::run().await,
        Command::Session { cmd } => session::run(cmd).await,
        Command::Config { cmd } => config::run(cmd).await,
        Command::Skill { topic } => skill::run(topic),
        #[cfg(feature = "tui")]
        Command::Open { path } => {
            let path = path
                .map(|p| p.into_os_string().into_string())
                .transpose()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "パスが UTF-8 ではありません"))?;
            client::run(path.as_deref()).await
        }
    }
}