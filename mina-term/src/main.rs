//! mina: daemon/client 分割のターミナルエディタ（bin クレート）。
//!
//! モード:
//! - `mina daemon serve` — 常駐デーモン（クライアントから自動起動されることもある）
//! - `mina [file]` — クライアント（S0 では GetState の表示のみ。TUI は S1）

mod client;
mod daemon;
mod keymap;
mod render;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("daemon") => daemon::run().await,
        Some("session") => {
            eprintln!("mina: session サブコマンドは v1 の S4 で追加予定");
            Ok(())
        }
        // ファイルパス → TUI で開く
        Some(path) => client::run(Some(path)).await,
        None => client::run(None).await,
    }
}
