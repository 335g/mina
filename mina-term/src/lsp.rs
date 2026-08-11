//! LSP セッション: rust-analyzer の spawn・initialize・全文同期・診断の取り込み。
//!
//! 診断は daemon のスナップショットに載る（ADR-0006 の「次のスナップショットに
//! 含める」方針）。LSP 通知はコマンド処理時に drain されるため、UI が待ち状態の
//! 間に届いた診断は次のキー入力で表示される。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mina_lsp::{Client, Incoming, PositionEncoding, PublishParams};
use mina_protocol::{Diagnostic, Severity};
use serde_json::json;
use tokio::sync::Mutex;

use crate::daemon::Daemon;

/// 1回の publish で取り込む診断の上限（5c: 診断 flood 対策）。
const MAX_DIAGNOSTICS: usize = 500;

/// LSP セッションの状態（1セッション = 1サーバ。S3 は rust-analyzer のみ）。
pub struct LspSession {
    client: Client,
    encoding: PositionEncoding,
    version: i64,
    current_uri: Option<String>,
}

/// 拡張子 → サーバコマンド（組み込みテーブル。設定ファイル化はサーバが増えてから）。
pub fn server_for(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("rust-analyzer"),
        _ => None,
    }
}

/// `file://` URI。
///
/// ponytail: パスの percent-encoding は未対応（空白を含むパスは壊れる）。
fn uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

impl LspSession {
    /// サーバを spawn し、initialize まで完了させる。
    pub async fn new(command: &str, root: &Path) -> Result<Self, String> {
        let (mut client, _reader) = Client::spawn(command, &[])
            .await
            .map_err(|e| format!("LSP サーバを起動できません: {e}"))?;
        let result = client
            .request(
                "initialize",
                json!({
                    "processId": null,
                    "rootUri": uri(root),
                    "capabilities": {
                        "textDocument": { "publishDiagnostics": { "relatedInformation": false } }
                    },
                    "positionEncodings": ["utf-8", "utf-16"],
                }),
            )
            .await
            .map_err(|e| format!("initialize に失敗しました: {e}"))?;
        let encoding = match result
            .pointer("/capabilities/positionEncoding")
            .and_then(|v| v.as_str())
        {
            Some("utf-8") => PositionEncoding::Utf8,
            _ => PositionEncoding::Utf16,
        };
        client
            .notify("initialized", json!({}))
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            encoding,
            version: 0,
            current_uri: None,
        })
    }

    /// 文書を開いたことを通知する（別の文書が開いていたら閉じる）。
    pub async fn did_open(&mut self, path: &Path, text: &str) {
        let doc_uri = uri(path);
        self.version += 1;
        if let Some(prev) = self.current_uri.take() {
            if prev != doc_uri {
                let _ = self
                    .client
                    .notify("textDocument/didClose", json!({ "textDocument": { "uri": prev } }))
                    .await;
            }
        }
        let params = json!({
            "textDocument": {
                "uri": doc_uri,
                "languageId": "rust",
                "version": self.version,
                "text": text,
            }
        });
        let _ = self.client.notify("textDocument/didOpen", params).await;
        self.current_uri = Some(doc_uri);
    }

    /// 全文同期の didChange を送る。
    pub async fn did_change(&mut self, path: &Path, text: &str) {
        self.version += 1;
        let params = json!({
            "textDocument": { "uri": uri(path), "version": self.version },
            "contentChanges": [{ "text": text }],
        });
        let _ = self.client.notify("textDocument/didChange", params).await;
    }

    /// 現在の文書の URI。
    pub fn current_uri(&self) -> Option<&str> {
        self.current_uri.as_deref()
    }

    /// 未処理の通知を処理し、現在の文書向けの最新診断を返す。
    /// 診断が publish されていなければ None。
    fn drain_diagnostics(&mut self, text: &str, doc_uri: &str) -> Option<Vec<Diagnostic>> {
        let mut out = None;
        while let Ok(msg) = self.client.try_recv() {
            let Incoming::Notification { method, params } = msg;
            if method != "textDocument/publishDiagnostics" {
                continue;
            }
            let Ok(p) = serde_json::from_value::<PublishParams>(params) else {
                continue;
            };
            if p.uri != doc_uri {
                continue;
            }
            // 5c: 行インデックスを1回だけ構築し、各診断の座標変換を O(行長) に抑える
            // （毎回 O(文書長) を診断数ぶん繰り返さない）。
            let index = LineIndex::new(text);
            out = Some(
                p.diagnostics
                    .into_iter()
                    .take(MAX_DIAGNOSTICS)
                    .filter_map(|d| {
                        let start = lsp_pos_to_char_indexed(
                            &index,
                            text,
                            d.range.start.line,
                            d.range.start.character,
                            self.encoding,
                        );
                        let end = lsp_pos_to_char_indexed(
                            &index,
                            text,
                            d.range.end.line,
                            d.range.end.character,
                            self.encoding,
                        );
                        Some(Diagnostic {
                            start,
                            end,
                            severity: match d.severity {
                                Some(1) => Severity::Error,
                                Some(2) => Severity::Warning,
                                Some(3) => Severity::Info,
                                _ => Severity::Hint,
                            },
                            message: d.message,
                        })
                    })
                    .collect(),
            );
        }
        out
    }
}

/// 行先頭の char インデックス（5c: 診断座標変換を O(N×文書長) にしないための索引）。
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut starts = vec![0usize];
        for (i, ch) in text.chars().enumerate() {
            if ch == '\n' {
                starts.push(i + 1);
            }
        }
        Self { starts }
    }

    /// 行 `line` の先頭 char インデックス。範囲外なら最終行の先頭（既存挙動の踏襲）。
    fn line_start(&self, line: u32) -> usize {
        self.starts
            .get(line as usize)
            .copied()
            .unwrap_or_else(|| *self.starts.last().unwrap_or(&0))
    }
}

/// LSP 座標（行・列）を文書内の char インデックスへ変換する。O(行長)（索引付き）。
fn lsp_pos_to_char_indexed(
    index: &LineIndex,
    text: &str,
    line: u32,
    col: u32,
    enc: PositionEncoding,
) -> usize {
    let line_start = index.line_start(line);
    let line_text: String = text
        .chars()
        .skip(line_start)
        .take_while(|&c| c != '\n')
        .collect();
    let col_chars = match enc {
        PositionEncoding::Utf8 => mina_lsp::position::utf8_col_to_char(&line_text, col),
        PositionEncoding::Utf16 => mina_lsp::position::utf16_col_to_char(&line_text, col),
    };
    line_start + col_chars
}

/// LSP 座標（行・列）を文書内の char インデックスへ変換する。O(文書長)。
/// テストと単発変換用。診断の一括変換は [`lsp_pos_to_char_indexed`] を使う。
fn lsp_pos_to_char(text: &str, line: u32, col: u32, enc: PositionEncoding) -> usize {
    lsp_pos_to_char_indexed(&LineIndex::new(text), text, line, col, enc)
}

// ---- daemon 統合 ----

/// 必要なら LSP セッションを spawn + initialize する（初回 .rs オープン時）。
///
/// M1: spawn + initialize（最大10秒）は daemon ロック外で行うため、この関数は
/// `&Mutex<Daemon>` を受け取り、daemon ロックは短時間だけ掴む。
/// M3: 既存セッションが死んでいたら新しいセッションで置き換える。
pub async fn ensure(
    daemon: &Mutex<Daemon>,
    path: &Path,
) -> Result<Arc<Mutex<LspSession>>, String> {
    // 既存セッション（生きていれば）を再利用する
    {
        let d = daemon.lock().await;
        if let Some(session) = &d.lsp {
            let reuse = match session.try_lock() {
                Ok(s) => !s.client.is_dead(),
                Err(_) => true, // 同期中: 生きているとみなして再利用
            };
            if reuse {
                return Ok(session.clone());
            }
        }
    }
    // 未作成 or 死亡: ロックを離して spawn + initialize（M1）
    let command = server_for(path).expect("ensure は LSP 対応ファイルでのみ呼ばれる");
    let root = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let session = LspSession::new(command, &root).await?;
    let arc = Arc::new(Mutex::new(session));
    // 保存（短いロック・await なし）。
    let mut d = daemon.lock().await;
    if let Some(existing) = &d.lsp {
        // ponytail: 同時 ensure は実質1クライアント前提で起きない。起きた場合は
        // 既存を優先し、自分が spawn したセッションは破棄（プロセスは orphan）。
        return Ok(existing.clone());
    }
    d.lsp = Some(arc.clone());
    Ok(arc)
}

/// 文書を開いたことを LSP に通知する（daemon ロック外・lsp mutex のみ）。
pub async fn open_document(session: &Mutex<LspSession>, path: &Path, text: &str) {
    let mut session = session.lock().await;
    session.did_open(path, text).await;
}

/// 編集後に全文同期する（現在の文書が LSP の監視対象のときだけ）。
///
/// M1: daemon ロック外で呼ばれる（lsp mutex のみ）。
/// M3: サーバが死んでいたら何もしない（無駄な 2s タイムアウト待ちを避ける。
/// 再 spawn は次回 .rs Open 時に `ensure` が行う）。
pub async fn sync(session: &Mutex<LspSession>, path: &Path, text: &str) {
    let mut session = session.lock().await;
    if session.client.is_dead() {
        return;
    }
    let doc_uri = uri(path);
    if session.current_uri() != Some(doc_uri.as_str()) {
        return; // 現在の文書は LSP 非対象（.rs 以外を編集中）
    }
    session.did_change(path, text).await;
}

/// 未処理の LSP 通知を取り込み、診断を daemon に反映する。
///
/// M1: セッションが同期中（ロック中）ならスキップする — 診断は「次の
/// スナップショットに載る」設計のため、daemon ロック内で LSP を待たない。
pub fn drain_into(daemon: &mut Daemon) {
    let Some(session) = &daemon.lsp else {
        return;
    };
    let Some(path) = daemon.editor.focused_path() else {
        daemon.diagnostics.clear();
        return;
    };
    let doc_uri = uri(path);
    let Ok(mut session) = session.try_lock() else {
        return;
    };
    if session.current_uri() != Some(doc_uri.as_str()) {
        return;
    }
    let text = daemon.editor.current_document().text().to_string();
    if let Some(diagnostics) = session.drain_diagnostics(&text, &doc_uri) {
        daemon.diagnostics = diagnostics;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_pos_to_char_utf8_multi_line() {
        let text = "ab\ncd\nこんにちは";
        // 2行目（cd）の char 1 = 'd'（"ab\n" の3 + 1）
        assert_eq!(lsp_pos_to_char(text, 1, 1, PositionEncoding::Utf8), 4);
        // 3行目のバイト6 = こんにちは の 2文字目（ん）。行開始は "ab\ncd\n" の6
        assert_eq!(lsp_pos_to_char(text, 2, 6, PositionEncoding::Utf8), 8);
    }

    #[test]
    fn lsp_pos_to_char_utf16_cjk() {
        let text = "aあ😀b\nnext";
        // 1行目: a(1) あ(1) 😀(2) → utf-16 4 は 'b' の位置 = char 3
        assert_eq!(lsp_pos_to_char(text, 0, 4, PositionEncoding::Utf16), 3);
        // 2行目先頭は char 5（\n の直後）
        assert_eq!(lsp_pos_to_char(text, 1, 0, PositionEncoding::Utf16), 5);
    }
}
