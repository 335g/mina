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
        /// Print the paste-able wrapper (YAML frontmatter + topic body + the build stamp) for a
        /// user's agent, instead of the raw topic — drop it where your runner reads skills
        /// (pi: ~/.pi/agent/skills/minas/SKILL.md), or into AGENTS.md / CLAUDE.md
        #[arg(long)]
        md: bool,
    },
}

#[tokio::main]
async fn main() {
    restore_sigpipe_default();
    if let Err(e) = run().await {
        // エラーは「Error: <message>」で stderr に、終了コード 1（agent は $? で判定）
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

/// SIGPIPE を既定動作に戻す（ドッグフーディング #13）。
///
/// Rust は起動時に SIGPIPE を無視するため、読み手が先に死んだパイプへの
/// `println!` は EPIPE になり **panic（exit 101 + panic banner）** する。
/// `minas check … | head -1` のように応答がパイプバッファを超える普通の使い方で
/// 起き、エージェントには「ツールの内部障害」に見える（`if minas … | head -1` を
/// 使うスクリプトも壊れる）。既定動作に戻すと SIGPIPE で静かに終わる（exit 141 —
/// Unix のフィルタの慣習どおりで、stderr に何も出さない）。
fn restore_sigpipe_default() {
    #[cfg(unix)]
    // SAFETY: 起動直後に 1 回だけ、プロセス全体のシグナル設定を既定値に戻す。
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

async fn run() -> std::io::Result<()> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // clap は usage エラーを既定で exit 2 で終えるが、minas の契約は
            // 「bad input = exit 1（再試行するな）」（`minas skill errors`）。
            // 実測（ドッグフーディング #13）: `minas outline a b` が exit 2、
            // `minas hover <path> not-a-pos` が exit 1 で、同じ「引数のミス」が
            // 層によって retryable に見えていた。--help / --version は stdout へ。
            let _ = e.print();
            std::process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    };
    match cli.command {
        Command::Session(cmd) => session::run(cmd, cli.name).await,
        Command::Skill { topic, md } => skill::run(topic, md),
    }
}
