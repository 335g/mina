//! mina: daemon/client 分割のターミナルエディタ（bin クレート）。
//!
//! モード:
//! - `mina daemon serve` — 常駐デーモン（クライアントから自動起動されることもある）
//! - `mina session <get|exec|edit|wait|hints>` — agent 用ヘッドレス CLI
//! - `mina [file]` — クライアント（S0 では GetState の表示のみ。TUI は S1）

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
    /// ファイルパス（TUI で開く。サブコマンド名と重なる場合はサブコマンドが優先）
    path: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
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
        Some(Command::Daemon { cmd: DaemonCmd::Serve }) => daemon::run().await,
        Some(Command::Session { cmd }) => session::run(cmd).await,
        None => {
            let path = cli
                .path
                .map(|p| p.into_os_string().into_string())
                .transpose()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "パスが UTF-8 ではありません"))?;
            client::run(path.as_deref()).await
        }
    }
}
