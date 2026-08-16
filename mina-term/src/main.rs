//! mina: daemon/client 分割のターミナルエディタ（bin クレート）。
//!
//! モード:
//! - `mina daemon serve` — 常駐デーモン（クライアントから自動起動されることもある）
//! - `mina session <get|exec|edit|wait|hints>` — agent 用ヘッドレス CLI
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
#[command(name = "mina", version, about = "daemon/client 分割のターミナルエディタ")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 常駐デーモン
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// agent 用ヘッドレス CLI（daemon に接続してコマンドを実行）
    Session {
        #[command(subcommand)]
        cmd: session::SessionCmd,
    },
    /// ファイル編集 TUI を開く
    Open {
        /// 開くファイル（省略時は新規バッファ）
        path: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// 常駐デーモンを起動する
    Serve,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon { cmd: DaemonCmd::Serve } => daemon::run().await,
        Command::Session { cmd } => session::run(cmd).await,
        Command::Open { path } => {
            let path = path
                .map(|p| p.into_os_string().into_string())
                .transpose()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "パスが UTF-8 ではありません"))?;
            client::run(path.as_deref()).await
        }
    }
}
