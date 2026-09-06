//! minae: ターミナル UI（ratatui/crossterm でゼロベース再構築 — 仕様書
//! `docs/spec/minae-tui-rebuild.md`）。
//!
//! daemon 操作は `minad`、agent CLI は `minas` を使う。minae 自体は TUI 専用。
//!
//! 起動: `minae [file ...]` — 指定ファイルを開いて開始する。

mod app;
mod colors;
mod config;
mod keymap;
mod render;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1).peekable();
    let mut files = Vec::new();
    for arg in args.by_ref() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("minae: terminal UI for the minae editor daemon\n\nUsage: minae [file ...]");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("minae {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => files.push(arg),
        }
    }
    app::run(files).await
}
