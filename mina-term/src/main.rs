//! mina: daemon/client 分割のターミナルエディタ（bin クレート）。
//!
//! モード:
//! - `mina daemon serve` — 常駐デーモン（クライアントから自動起動されることもある）
//! - `mina session <get|exec|edit|wait|hints>` — agent 用ヘッドレス CLI
//! - `mina config <show|path|edit|set|get|init>` — ユーザー設定の確認・編集
//! - `mina open [file]` — ファイル編集 TUI（省略時は新規バッファ）

mod client;
mod colorscheme;
mod config;
mod daemon;
mod keymap;
mod lsp;
mod render;
mod session;

use std::io;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mina", version, about = "Terminal editor with a daemon/client split")]
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
    /// Open the file-editing TUI
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
        Command::Open { path } => {
            let path = path
                .map(|p| p.into_os_string().into_string())
                .transpose()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "パスが UTF-8 ではありません"))?;
            client::run(path.as_deref()).await
        }
    }
}
