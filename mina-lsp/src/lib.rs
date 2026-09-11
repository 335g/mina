//! LSP クライアント: 言語サーバの spawn・JSON-RPC 送受信・通知の配信。
//!
//! 位置変換（UTF-8/UTF-16 ↔ char インデックス）は [`position`] モジュール。
//! 診断の格納と表示は daemon 側（minae-term の lsp モジュール）の責務。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

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
    progress: Arc<Progress>,
}

/// [`Progress::wait_ready`] の待ち方。
#[derive(Debug, Clone, Copy)]
pub struct ReadyPolicy {
    /// 進捗を 1 つも通知しないサーバは、この時間で「進捗を使わない」と判定して諦める。
    pub arm: Duration,
    /// 進捗が空になってから、もう一度空のままかを確認するまでの時間。
    /// 相次ぐ phase（Fetching → CrateGraph → Indexing）の合間の空を「完走」と
    /// 誤認しないための静止確認。
    pub quiesce: Duration,
    /// 待ちの上限。超えたら ready 扱いで進む（待ち続けて要求を止めない）。
    pub cap: Duration,
}

impl Default for ReadyPolicy {
    fn default() -> Self {
        Self {
            arm: Duration::from_secs(1),
            quiesce: Duration::from_millis(300),
            cap: Duration::from_secs(10),
        }
    }
}

/// サーバの work-done progress（`$/progress`）の集約。
///
/// 言語サーバは索引の進捗を `$/progress` で通知する。進捗の終わりを待たずに
/// 索引依存の要求（workspace/symbol・rename・pull 診断）を出すと、サーバは
/// 「索引未完の空・一部ファイルだけの rename」を**正常応答として**返す（実測:
/// rust-analyzer は client が `window.workDoneProgress` を advertise したときだけ
/// 進捗を送る。L0 計測 2026-09-12）。daemon は [`Progress::wait_ready`] で
/// 索引完走を待ってから要求を出す。
#[derive(Debug)]
pub struct Progress {
    inner: StdMutex<Inner>,
    started: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    /// 進行中のトークン（begin で追加・end で削除）。
    outstanding: HashSet<String>,
    /// `$/progress` を 1 つでも見たか（進捗を使わないサーバの判定）。
    seen: bool,
    /// 索引完走を確認済みか（一旦確認したら待たない — 待ちはセッションごとに 1 回）。
    ready: bool,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            inner: StdMutex::new(Inner::default()),
            started: Instant::now(),
        }
    }
}

impl Progress {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn begin(&self, token: &str) {
        let mut inner = self.lock();
        inner.seen = true;
        inner.outstanding.insert(token.to_string());
    }

    pub(crate) fn end(&self, token: &str) {
        let mut inner = self.lock();
        inner.seen = true;
        inner.outstanding.remove(token);
    }

    /// 索引完走を確認済みか（待たずに判定する）。
    pub fn is_ready(&self) -> bool {
        self.lock().ready
    }

    /// 索引完走を待つ。`true` = 完走と確認できた、`false` = 上限まで待って未確認。
    ///
    /// 判定は「進捗が 1 つも無く、静止確認 `quiesce` の後も無い」— 進捗を 1 つも
    /// 通知しないサーバは `arm` で諦める。一旦確認したら以後は即返る（飛行場に
    /// 毎回 300ms 払わない）。
    pub async fn wait_ready(&self, policy: ReadyPolicy) -> bool {
        if self.is_ready() {
            return true;
        }
        loop {
            let idle = {
                let inner = self.lock();
                (inner.seen, inner.outstanding.is_empty())
            };
            if idle.0 && idle.1 {
                tokio::time::sleep(policy.quiesce).await;
                if self.lock().outstanding.is_empty() {
                    self.lock().ready = true;
                    return true;
                }
            } else if !idle.0 && self.started.elapsed() >= policy.arm {
                // 進捗を通知しないサーバ（work-done progress 非対応）。
                self.lock().ready = true;
                return true;
            }
            if self.started.elapsed() >= policy.cap {
                return false; // 未確認のまま要求を出す（上限は daemon 側が決める）
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
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
        let progress = Arc::new(Progress::default());
        let reader_progress = progress.clone();
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
                // work-done progress は daemon が索引完走の判定に使う（Progress）。
                // 通知チャネルには流さない — 索引中は毎秒何十件も届き、容量 64 の
                // bounded チャネルから publishDiagnostics を押し出すため。
                if method == "$/progress" {
                    if let Some(token) = params.get("token").and_then(Value::as_str) {
                        match params.pointer("/value/kind").and_then(Value::as_str) {
                            Some("begin") => reader_progress.begin(token),
                            Some("end") => reader_progress.end(token),
                            _ => {} // report（進捗率）は使わない
                        }
                    }
                    continue;
                }
                // work-done progress の登録要求。応答不要（daemon の関心ではない）。
                if method == "window/workDoneProgress/create" {
                    continue;
                }
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
                progress,
            },
            reader,
        ))
    }

    /// work-done progress の状態（索引完走の待ちに使う）。
    pub fn progress(&self) -> Arc<Progress> {
        self.progress.clone()
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, timeout};

    /// 索引の進捗を待つ規則: outstanding があるうちは抜けない、end 後は静止確認を
    /// 通って完走、進捗を通知しないサーバは arm で諦める。早期に抜けると索引未完の
    /// 応答（空の workspace/symbol 等）を正常応答として返してしまう。
    #[tokio::test]
    async fn progress_waits_for_outstanding_and_settles() {
        let policy = ReadyPolicy {
            arm: Duration::from_millis(50),
            quiesce: Duration::from_millis(20),
            cap: Duration::from_millis(500),
        };

        // 進捗を 1 つも通知しないサーバ: arm の後に「使われていない」と判定して進む。
        let quiet = Progress::default();
        assert!(quiet.wait_ready(policy).await);
        assert!(quiet.is_ready());

        // outstanding があるうちは抜けない（早期脱出の回帰）。
        let p = Arc::new(Progress::default());
        p.begin("mock/Indexing");
        let released = p.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(80)).await;
            released.end("mock/Indexing");
        });
        assert!(timeout(Duration::from_millis(40), p.wait_ready(policy))
            .await
            .is_err());
        assert!(p.wait_ready(policy).await, "end の後は完走する");

        // 上限まで進捗が終わらないサーバ: 未確認（false）で要求を止めない。
        let stuck = Progress::default();
        stuck.begin("mock/Indexing");
        assert!(!stuck.wait_ready(policy).await);
    }
}
