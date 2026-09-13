//! minas: minae のヘッドレス CLI（エージェント用）。
//!
//! - `minas <get|exec|edit|apply|wait|hints|peek|rename|references|outline|at|…>` —
//!   minad に接続してコマンドを実行する（1コマンド1接続のワンショット）
//! - `minas skill [topic]` — エージェント向け判断・手順の参考書
//!
//! daemon が動いていなければ自動起動する（`minad` を探して spawn）。

mod session;
mod skill;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "minas", version, about = "Headless agent CLI for minae")]
struct Cli {
    /// Self-reported label (ADR-0038 — it lands in activity's actor). Defaults
    /// to the MINAE_CLIENT_NAME env var, then to "unknown".
    #[arg(long, global = true)]
    name: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Session(session::SessionCmd),
    /// Agent skill guides: `minas skill` lists topics, `minas skill <topic>` prints one
    Skill {
        /// Topic to print (omit for the index)
        topic: Option<String>,
    },
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
        Command::Session(cmd) => session::run(cmd, cli.name).await,
        Command::Skill { topic } => skill::run(topic),
    }
}
