//! daemon: 編集状態を所有し、クライアントからのコマンドを処理する常駐プロセス。
//!
//! S0 では編集状態を持たない（mina-view の接続は S1）。GetState には空の
//! スナップショットを返す。socket は `<temp_dir>/mina.sock`（単一ユーザ前提）。

use std::path::{Path, PathBuf};

use mina_protocol::{Command, StateSnapshot};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// 常駐デーモンとして起動する（`mina daemon serve`）。
pub async fn run() -> std::io::Result<()> {
    serve(&socket_path()).await
}

/// `path` で待ち受ける。前回の異常終了で残った stale socket は除去してから bind する。
pub async fn serve(path: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    accept_loop(listener).await
}

/// 接続を受け付け、接続ごとにコマンド処理タスクを立てる。
async fn accept_loop(listener: UnixListener) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle_connection(stream));
    }
}

/// 1接続分: NDJSON でコマンドを読み、応答スナップショットを返す。
///
/// ponytail: 接続ごとに全状態スナップショットを返す（O(n)/コマンド）。
/// 巨大ファイルで問題になったら差分送信に差し替える。
async fn handle_connection(stream: UnixStream) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => return, // クライアントの切断
            Ok(_) => {}
            Err(_) => return,
        }
        let command: Command = match serde_json::from_str(line.trim()) {
            Ok(c) => c,
            Err(_) => continue, // 壊れた行は無視
        };
        let snapshot = dispatch(command);
        let mut out = serde_json::to_string(&snapshot).expect("snapshot はシリアライズ可能");
        out.push('\n');
        if write_half.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn dispatch(command: Command) -> StateSnapshot {
    match command {
        Command::GetState => StateSnapshot::default(),
    }
}

/// socket パス。
///
/// ponytail: uid を入れていない（単一ユーザ前提）。複数ユーザ対応が必要に
/// なったら `<dir>/mina-<uid>.sock` にする。
pub fn socket_path() -> PathBuf {
    std::env::temp_dir().join("mina.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_state_round_trip() {
        let path = std::env::temp_dir().join(format!("mina-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        let serve_task = tokio::spawn(async move { accept_loop(listener).await });

        let mut stream = UnixStream::connect(&path).await.expect("connect");
        let mut line = serde_json::to_string(&Command::GetState).expect("serialize");
        line.push('\n');
        stream.write_all(line.as_bytes()).await.expect("write");
        stream.flush().await.ok();

        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).await.expect("read");
        let snapshot: StateSnapshot = serde_json::from_str(&response).expect("deserialize");
        assert_eq!(snapshot, StateSnapshot::default());

        serve_task.abort();
        let _ = std::fs::remove_file(&path);
    }
}
