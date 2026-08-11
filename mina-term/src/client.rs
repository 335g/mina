//! クライアント: daemon へ接続してコマンドを送る。
//!
//! S0 では GetState を送り、返ってきたスナップショットを表示するだけ
//! （TUI は S1）。daemon が動いていなければ自動起動する（ADR-0005）。

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use mina_protocol::{Command, StateSnapshot};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn run() -> std::io::Result<()> {
    let path = crate::daemon::socket_path();
    ensure_daemon(&path).await?;
    let mut stream = UnixStream::connect(&path).await?;

    let mut line = serde_json::to_string(&Command::GetState).expect("serialize");
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    let snapshot: StateSnapshot = serde_json::from_str(&response)?;
    println!("{snapshot:#?}");
    Ok(())
}

/// daemon が動いていなければ自動起動し、socket が現れるまで待つ。
///
/// 競合: 同時に2つのクライアントが spawn した場合、片方の daemon が bind に
/// 失敗して終了する（v1 の割り切り）。
///
/// ponytail: 起動確認は50×50ms のポーリング。遅い環境で起動が遅れたら
/// 回数を増やすか、daemon 側の ready 通知を導入する。
async fn ensure_daemon(path: &Path) -> std::io::Result<()> {
    if UnixStream::connect(path).await.is_ok() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let mut cmd = ProcessCommand::new(exe);
    cmd.arg("daemon").arg("serve");
    // 端末を閉じても daemon が死なないよう、新しいセッションへ離脱させる
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    for _ in 0..50 {
        if UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "daemon が起動しなかった",
    ))
}
