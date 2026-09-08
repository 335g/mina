//! LSP クライアント: 言語サーバの spawn・JSON-RPC 送受信・通知の配信。
//!
//! 位置変換（UTF-8/UTF-16 ↔ char インデックス）は [`position`] モジュール。
//! 診断の格納と表示は daemon 側（minae-term の lsp モジュール）の責務。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command as TokioCommand};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// 1フレーム（LSP メッセージ）の最大バイト数（5f）。
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// 通知チャネルの容量（M4）。
///
/// 通知（publishDiagnostics 等）は production で誰も読まない（診断は pull で
/// 取り込む方針のため）。unbounded のままだと daemon 稼働中ずっと蓄積して
/// メモリが無制限に成長するため、bounded にして満杯の通知は破棄する。
/// サーバ死の検知（送信側 drop → is_closed）は破棄の影響を受けない。
pub const NOTIFICATION_CAPACITY: usize = 64;

pub mod position;

/// クライアントが受信した通知。
#[derive(Debug)]
pub enum Incoming {
    Notification { method: String, params: Value },
}

/// `textDocument/publishDiagnostics` のパラメータ（位置は LSP 座標のまま）。
#[derive(Debug, Clone, Deserialize)]
pub struct PublishParams {
    pub uri: String,
    #[serde(default)]
    pub diagnostics: Vec<PublishDiagnostic>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublishDiagnostic {
    pub range: LspRange,
    #[serde(default)]
    pub severity: Option<u32>,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

/// 位置エンコーディング（initialize でサーバが選ぶ。既定は UTF-16）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
}

/// 言語サーバへの接続。reader タスクがバックグラウンドで応答と通知を振り分ける。
pub struct Client {
    write: ChildStdin,
    child: Child,
    next_id: u64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    notifications: mpsc::Receiver<Incoming>,
}

impl Client {
    /// `command` を spawn して接続する。第2戻り値のタスクがサーバの stdout を
    /// 読み続ける（EOF で終了）。
    pub async fn spawn(command: &str, args: &[&str]) -> std::io::Result<(Self, JoinHandle<()>)> {
        let mut child = TokioCommand::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let write = child.stdin.take().expect("stdin は piped");
        let stdout = child.stdout.take().expect("stdout は piped");
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // M4: unbounded だと読まれない通知が蓄積し続けるため bounded にする
        let (tx, rx) = mpsc::channel(NOTIFICATION_CAPACITY);
        let reader_pending = pending.clone();
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let frame = match read_frame(&mut reader).await {
                    Ok(Some(f)) => f,
                    Ok(None) => break, // サーバ終了
                    Err(_) => break,
                };
                let msg: Value = match serde_json::from_slice(&frame) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                // レスポンス（id があり method がない）
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    if msg.get("method").is_none() {
                        if let Some(sender) = reader_pending.lock().await.remove(&id) {
                            let _ = sender.send(msg.get("result").cloned().unwrap_or(Value::Null));
                        }
                        continue;
                    }
                }
                // 通知
                let method = msg
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                // M4: bounded チャネル + try_send。満杯の通知は破棄する
                // （誰も読まないため。診断は pull で取り込む方針を崩さない）。
                match tx.try_send(Incoming::Notification { method, params }) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {} // 満杯: 破棄
                    Err(mpsc::error::TrySendError::Closed(_)) => break, // 受信側が消えた
                }
            }
        });
        Ok((
            Self {
                write,
                child,
                next_id: 1,
                pending,
                notifications: rx,
            },
            reader,
        ))
    }

    /// リクエストを送り、応答を待つ（10秒でタイムアウト）。
    pub async fn request(&mut self, method: &str, params: Value) -> std::io::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let data = serde_json::to_vec(&msg).expect("シリアライズ可能");
        if let Err(e) = write_frame(&mut self.write, &data).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        match timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "サーバが終了した",
            )),
            Err(_) => {
                // L2: タイムアウト時も pending から除去する（oneshot リーク防止）
                self.pending.lock().await.remove(&id);
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "LSP 応答タイムアウト",
                ))
            }
        }
    }

    /// 通知を送る（応答を待たない）。
    ///
    /// M2: サーバが stdin を読まないとパイプが詰まって永久ブロックするため、
    /// 書き込み全体に 2 秒のタイムアウトを付ける。
    pub async fn notify(&mut self, method: &str, params: Value) -> std::io::Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let data = serde_json::to_vec(&msg).expect("シリアライズ可能");
        timeout(Duration::from_secs(2), write_frame(&mut self.write, &data))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "LSP 書き込みタイムアウト")
            })?
    }

    /// 未処理の通知を1つ取り出す。
    pub fn try_recv(&mut self) -> Result<Incoming, mpsc::error::TryRecvError> {
        self.notifications.try_recv()
    }

    /// サーバが終了したか。
    ///
    /// reader タスクがサーバ stdout の EOF/エラーで終了すると通知チャネルの
    /// 送信側が drop されるため、`is_closed()` で検知できる（M3）。
    pub fn is_dead(&self) -> bool {
        self.notifications.is_closed()
    }

    /// サーバプロセスを終了させる。
    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// Content-Length フレームを書き出す（LSP 標準トランスポート）。
async fn write_frame(write: &mut ChildStdin, data: &[u8]) -> std::io::Result<()> {
    write
        .write_all(format!("Content-Length: {}\r\n\r\n", data.len()).as_bytes())
        .await?;
    write.write_all(data).await?;
    write.flush().await
}

/// Content-Length フレームを1つ読む。EOF なら None。
async fn read_frame(reader: &mut BufReader<ChildStdout>) -> std::io::Result<Option<Vec<u8>>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(len) = line.strip_prefix("Content-Length:") {
            content_length = len.trim().parse::<usize>().ok();
        }
    }
    let len = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Content-Length ヘッダがない",
        )
    })?;
    // 5f: 巨大な Content-Length で巨大アロケーションしないよう上限を設ける
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Content-Length が大きすぎる",
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(buf))
}
