//! LSP セッション: rust-analyzer の spawn・initialize・全文同期・診断の取り込み。
//!
//! 診断は daemon のスナップショットに載る（ADR-0006 の「次のスナップショットに
//! 含める」方針）。LSP 通知はコマンド処理時に drain されるため、UI が待ち状態の
//! 間に届いた診断は次のキー入力で表示される。

use std::path::{Path, PathBuf};

use mina_lsp::{Client, Incoming, PositionEncoding, PublishParams};
use mina_protocol::{Diagnostic, Severity};
use serde_json::json;

use crate::daemon::Daemon;

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
            out = Some(
                p.diagnostics
                    .into_iter()
                    .filter_map(|d| {
                        let start = lsp_pos_to_char(
                            text,
                            d.range.start.line,
                            d.range.start.character,
                            self.encoding,
                        );
                        let end = lsp_pos_to_char(
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

/// LSP 座標（行・列）を文書内の char インデックスへ変換する。O(文書長)。
fn lsp_pos_to_char(text: &str, line: u32, col: u32, enc: PositionEncoding) -> usize {
    let mut current_line = 0u32;
    let mut line_start = 0usize; // 現在行の先頭 char インデックス
    for (i, ch) in text.chars().enumerate() {
        if current_line >= line {
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = i + 1;
        }
    }
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

// ---- daemon 統合 ----

/// 必要なら LSP セッションを spawn + initialize する（初回 .rs オープン時）。
pub async fn ensure(daemon: &mut Daemon, path: &Path) -> Result<(), String> {
    if daemon.lsp.is_some() {
        return Ok(());
    }
    let command = server_for(path).expect("ensure は LSP 対応ファイルでのみ呼ばれる");
    let root = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let session = LspSession::new(command, &root).await?;
    daemon.lsp = Some(session);
    Ok(())
}

/// 文書を開いたことを LSP に通知する。
pub async fn open_document(daemon: &mut Daemon, path: &Path, text: &str) {
    if let Some(session) = &mut daemon.lsp {
        session.did_open(path, text).await;
    }
}

/// 編集後に全文同期する（現在の文書が LSP の監視対象のときだけ）。
pub async fn sync(daemon: &mut Daemon) {
    let Some(session) = &mut daemon.lsp else {
        return;
    };
    let Some(path) = daemon.editor.focused_path() else {
        return;
    };
    let doc_uri = uri(path);
    if session.current_uri() != Some(doc_uri.as_str()) {
        return; // 現在の文書は LSP 非対象（.rs 以外を編集中）
    }
    let text = daemon.editor.current_document().text().to_string();
    session.did_change(path, &text).await;
}

/// 未処理の LSP 通知を取り込み、診断を daemon に反映する。
pub fn drain_into(daemon: &mut Daemon) {
    let Some(session) = &mut daemon.lsp else {
        return;
    };
    let Some(path) = daemon.editor.focused_path() else {
        daemon.diagnostics.clear();
        return;
    };
    let doc_uri = uri(path);
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
