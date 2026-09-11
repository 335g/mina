//! LSP セッション: rust-analyzer の spawn・initialize・全文同期・診断の取り込み。
//!
//! 診断は daemon のスナップショットに載る（ADR-0006 の「次のスナップショットに
//! 含める」方針）。LSP 通知はコマンド処理時に drain されるため、UI が待ち状態の
//! 間に届いた診断は次のキー入力で表示される。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mina_lsp::{Client, LspRange, PositionEncoding, Progress, PublishDiagnostic, ReadyPolicy};
use mina_protocol::{Diagnostic, InlayHint, Severity};
use serde::Deserialize;use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::time::timeout;

/// 1回の publish で取り込む診断の上限（5c: 診断 flood 対策）。
const MAX_DIAGNOSTICS: usize = 500;

/// 編集後の pull 診断を打つまでの待ち時間。
///
/// rust-analyzer は解析完了まで pull に「解析前の空」を返すため、didChange の
/// 直後に pull すると誤ってクリーン扱いになる。インクリメンタル解析は概ね
/// 100ms 前後で完了するため、250ms 待ってから pull する。
const PULL_SETTLE: Duration = Duration::from_millis(250);

/// LSP セッションの Mutex 取得のタイムアウト（MEDIUM-4）。
///
/// サーバが生きているが応答しない（stdin を読まない）と `notify` が最大 2 秒
/// ロックを握り続け、待ち側の `open_document` / `sync` が無制限に待つ。ロックを
/// 取れないときは諦める — didOpen の欠落は次回 .rs Open、didChange の欠落は
/// 全文同期の次の編集で補われる（スキップしても整合が壊れない）。
/// daemon 側（serve_peek_definition）もセッションの encoding を読むため共用する。
#[cfg(not(test))]
pub(crate) const LSP_LOCK_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
pub(crate) const LSP_LOCK_TIMEOUT: Duration = Duration::from_millis(200);

/// 索引完走（`$/progress` が静まるまで）の待ち方。
///
/// 索引未完の LSP では `workspace/symbol` は空を返し、`rename` は開いていない
/// ファイルを取りこぼしたまま「成功」する（実測: L0 `--cold` 2026-09-12）。
/// 待つのはセッションごとに 1 回だけ（[`Progress`] が確認済みを記録する）。
/// 上限は「待ち続けて要求を止めない」ためのもの — 超えたら未確認のまま進める。
///
/// `arm` の根拠（実測 2026-09-12）: rust-analyzer の最初の進捗は `initialized` の
/// 約 0.26 秒後（Fetching）、typescript-language-server は進捗を 1 件も送らない
/// （= TS は 1 セッション 1 回だけ 1 秒払う。TS は読み込み自体が秒単位なので相対的に
/// 小さい）。初回進捗が 1 秒を超えるサーバを見つけたら `arm` を上げる。
#[cfg(not(test))]
pub(crate) const LSP_READY: ReadyPolicy = ReadyPolicy {
    arm: Duration::from_secs(1),
    quiesce: Duration::from_millis(300),
    cap: Duration::from_secs(10),
};
#[cfg(test)]
pub(crate) const LSP_READY: ReadyPolicy = ReadyPolicy {
    arm: Duration::from_millis(100),
    quiesce: Duration::from_millis(20),
    cap: Duration::from_millis(400),
};

/// LSP セッションの状態（1セッション = 1サーバ。言語テーブルは ADR-0030）。
pub struct LspSession {
    /// pub(crate): daemon 統合（ensure / drain / settle）が生死判定に読む。
    pub(crate) client: Client,
    /// initialize 応答から導出したサーバ能力（ADR-0030 Stage 3）。機能ゲートは
    /// daemon 側（rename / references / peek）と pull 側（診断 / inlay hints）が読む。
    pub(crate) caps: ServerCapabilities,
    encoding: PositionEncoding,
    version: i64,
    current_uri: Option<String>,
    /// initialize 応答で advertise された pull 診断の identifier。
    diagnostic_identifier: Option<String>,
    /// `textDocument.languageId`（spawn 元ファイルの言語。ADR-0030）。
    /// セッション生成後に変わらない（root+言語キーの拡張は Stage 4）。
    language_id: String,
}

/// initialize 応答の capabilities から導出したサーバ能力（ADR-0030 Stage 3）。
///
/// 未 advertise の機能は要求しない（要求すると MethodNotFound 等の往復・10 秒リトライ
/// を無駄にする — 検証済みサーバ以外は capability が自然なゲートになる）。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ServerCapabilities {
    /// `textDocument/diagnostic`（pull 診断）を提供する。
    pub(crate) pull_diagnostics: bool,
    /// `textDocument/inlayHint` を提供する。
    pub(crate) inlay_hints: bool,
    /// `textDocument/rename` を提供する。
    pub(crate) rename: bool,
    /// `textDocument/references` を提供する。
    pub(crate) references: bool,
    /// `textDocument/definition` を提供する。
    pub(crate) definition: bool,
    /// `textDocument/documentSymbol`（シンボルの階層ツリー）を提供する。
    pub(crate) document_symbols: bool,
    /// `textDocument/hover` を提供する。
    pub(crate) hover: bool,
    /// `workspace/symbol` を提供する。
    pub(crate) workspace_symbols: bool,
}

/// initialize 応答から能力を導出する。`renameProvider: false` 等の明示 false と
/// キー欠落は非対応扱い、`true` とオブジェクト形式（RenameOptions 等）は対応扱い。
fn capabilities_of(result: &Value) -> ServerCapabilities {
    // null / false / キー欠落は非対応扱い（null を advertise するサーバは spec 違反だが
    // 稀にいる）。true とオブジェクト形式（RenameOptions 等）は対応扱い。
    let cap = |path: &str| -> bool {
        result
            .pointer(&format!("/capabilities{path}"))
            .is_some_and(|v| !matches!(v, Value::Bool(false) | Value::Null))
    };
    ServerCapabilities {
        // diagnosticProvider はオブジェクトのはずだが、null / false も cap() で
        // 非対応扱いに揃える（他機能と同じ false/null 防御）。
        pull_diagnostics: cap("/diagnosticProvider"),
        inlay_hints: cap("/inlayHintProvider"),
        rename: cap("/renameProvider"),
        references: cap("/referencesProvider"),
        definition: cap("/definitionProvider"),
        document_symbols: cap("/documentSymbolProvider"),
        hover: cap("/hoverProvider"),
        workspace_symbols: cap("/workspaceSymbolProvider"),
    }
}

/// `file://` URI。
///
/// ponytail: パスの percent-encoding は未対応（空白を含むパスは壊れる）。
pub(crate) fn uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

impl LspSession {
    /// work-done progress の状態（索引完走の待ちに使う）。
    ///
    /// 待つ側はセッションロックを握らない — この `Arc` を clone してからロックを
    /// 離し、[`Progress::wait_ready`] を外側で待つ（編集中の didChange を締め出さない）。
    pub(crate) fn progress(&self) -> Arc<Progress> {
        self.client.progress()
    }

    /// サーバを spawn し、initialize まで完了させる（**テスト専用**: 言語は rust、
    /// init options なし。本番は [`new_with_config`] を使う）。
    #[cfg(test)]
    pub async fn new(command: &str, root: &Path) -> Result<Self, String> {
        Self::new_with_args(command, root, &[]).await
    }

    /// サーバを spawn し、initialize まで完了させる（**テスト専用**: 起動引数付き）。
    #[cfg(test)]
    pub async fn new_with_args(command: &str, root: &Path, args: &[&str]) -> Result<Self, String> {
        let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        Self::new_with_config(command, root, &args, "rust", None).await
    }

    /// サーバを spawn し、initialize まで完了させる。
    ///
    /// 本番（daemon の `ensure`）は languages.toml のテーブル（ADR-0030）から
    /// command / args / init options / languageId を渡す。init options はサーバ固有の
    /// 不透明 JSON — 無ければ送らない（サーバ既定に任せる）。
    pub async fn new_with_config(
        command: &str,
        root: &Path,
        args: &[String],
        language_id: &str,
        init_options: Option<serde_json::Value>,
    ) -> Result<Self, String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (mut client, _reader) = Client::spawn(command, &args)
            .await
            .map_err(|e| format!("LSP サーバを起動できません: {e}"))?;
        let mut params = json!({
            "processId": null,
            "rootUri": uri(root),
            "capabilities": {
                // work-done progress を購読する（索引完走の判定。ADR-0051）。
                // advertise しないと rust-analyzer は `$/progress` を送らない（実測）。
                "window": { "workDoneProgress": true },
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": false },
                    // inlay hint は static 登録のみ（resolve は使わない。ADR-0020）。
                    "inlayHint": { "dynamicRegistration": false },
                    // documentSymbol は階層形（DocumentSymbol）を要求する — 広告しないと
                    // rust-analyzer はフラットな SymbolInformation[]（location のみ・
                    // selectionRange なし）を返し、名前トークン範囲が取れない（実測）。
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                }
            },
            "positionEncodings": ["utf-8", "utf-16"],
        });
        if let Some(opts) = init_options {
            params["initializationOptions"] = opts;
        }
        let result = client
            .request("initialize", params)
            .await
            .map_err(|e| format!("initialize に失敗しました: {e}"))?;
        let caps = capabilities_of(&result);
        let encoding = match result
            .pointer("/capabilities/positionEncoding")
            .and_then(|v| v.as_str())
        {
            Some("utf-8") => PositionEncoding::Utf8,
            _ => PositionEncoding::Utf16,
        };
        // pull 診断の identifier（rust-analyzer は "rust-analyzer" を advertise）。
        let diagnostic_identifier = result
            .pointer("/capabilities/diagnosticProvider/identifier")
            .and_then(Value::as_str)
            .map(str::to_string);
        client
            .notify("initialized", json!({}))
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            caps,
            encoding,
            version: 0,
            current_uri: None,
            diagnostic_identifier,
            language_id: language_id.to_string(),
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
                "languageId": self.language_id,
                "version": self.version,
                "text": text,
            }
        });
        let _ = self.client.notify("textDocument/didOpen", params).await;
        self.current_uri = Some(doc_uri);
    }

    /// 前の文書を閉じずに didOpen する（バッチ用）。
    ///
    /// セマンティック要求の前段（`open_workspace_files`）で workspace 内の同拡張子
    /// ファイルを**開いたまま保持**する — tsserver 等は閉じたファイルの参照/rename を
    /// 返さない（probe 実測）。rust-analyzer も開いているファイルの参照しか返さないため
    /// どちらの流儀にも適合する。`current_uri` は最後に開いた文書（復元・pull は
    /// 要求後にフォーカス文書を開き直して戻す）。
    pub async fn did_open_keep(&mut self, path: &Path, text: &str) {
        let doc_uri = uri(path);
        self.version += 1;
        let params = json!({
            "textDocument": {
                "uri": doc_uri,
                "languageId": self.language_id,
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

    /// ネゴシエート済みの位置 encoding（rename の WorkspaceEdit 座標変換など
    /// ロック外でも使えるようコピーで返す）。
    pub fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    /// 文字インデックス → LSP 座標（ネゴシエート済み encoding 込み）。
    /// カーソル基準の TUI 要求（[`Command::PeekDefinition`]）が、位置指定の
    /// [`definition_peek_at`] に渡す座標を作るために使う。
    pub fn char_to_lsp_pos(&self, text: &str, char_idx: usize) -> (u32, u32) {
        char_to_lsp_pos(text, char_idx, self.encoding)
    }

    /// pull 診断（`textDocument/diagnostic`）を取得し、char インデックスに変換して返す。
    ///
    /// 解析未完了（空）・エラー応答・URI 不一致の場合は `None`（呼び出し側は
    /// 現在の診断を維持する）。rust-analyzer はライブ（in-memory）の診断を
    /// push（publishDiagnostics）ではなく pull で返すため、編集後の診断更新は
    /// この経路で行う（上流フィードバック: クライアントは両方扱うべき）。
    /// サーバが `diagnosticProvider` を advertise していなければ `Some(空)` ——
    /// 診断を提供しないサーバとして正しく空にする（要求しない。Stage 3）。
    pub async fn pull_diagnostics(&mut self, path: &Path, text: &str) -> Option<Vec<Diagnostic>> {
        if !self.caps.pull_diagnostics {
            return Some(Vec::new());
        }
        let doc_uri = uri(path);
        if self.current_uri.as_deref() != Some(doc_uri.as_str()) {
            return None;
        }
        let mut params = json!({ "textDocument": { "uri": doc_uri } });
        if let Some(id) = &self.diagnostic_identifier {
            params["identifier"] = json!(id);
        }
        let result = self
            .client
            .request("textDocument/diagnostic", params)
            .await
            .ok()?;
        let items = result.get("items")?.as_array()?;
        let items: Vec<PublishDiagnostic> = serde_json::from_value(Value::Array(items.clone())).ok()?;
        Some(convert_diagnostics(text, self.encoding, items))
    }

    /// inlay hint（`textDocument/inlayHint`）を取得し、char インデックスに変換して返す。
    ///
    /// 診断の pull と同じ形状: 現在の文書以外には応えない（`None`）。応答は
    /// 全文範囲のヒント配列。座標はネゴシエート済み encoding で変換する。
    /// サーバが `inlayHintProvider` を advertise していなければ `Some(空)` ——
    /// 表示や配信のないサーバとして正しく空にする（要求しない。Stage 3）。
    pub async fn pull_inlay_hints(&mut self, path: &Path, text: &str) -> Option<Vec<InlayHint>> {
        if !self.caps.inlay_hints {
            return Some(Vec::new());
        }
        let doc_uri = uri(path);
        if self.current_uri.as_deref() != Some(doc_uri.as_str()) {
            return None;
        }
        let result = self
            .client
            .request(
                "textDocument/inlayHint",
                json!({
                    "textDocument": { "uri": doc_uri },
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": doc_end_position(text, self.encoding),
                    },
                }),
            )
            .await
            .ok()?;
        let items = result.as_array()?;
        let items: Vec<LspInlayHint> = serde_json::from_value(Value::Array(items.clone())).ok()?;
        Some(convert_inlay_hints(text, self.encoding, items))
    }
}

/// `textDocument/documentSymbol` によるシンボル階層ツリーの取得（ADR-0031）。
/// 応答は全文を運ばず、名前・種別・範囲（選択範囲含む）だけの軽いツリー。
///
/// 解析待ちのシグナル（null / 空配列）は rename / references と同じくリトライ
/// し、2 回連続で同一になるまで待つ（プロジェクトロード中の部分/空応答を
/// 取りこぼさない — ADR-0029 の規律を ADR-0031 で再利用。予算切れは最後の
/// 結果をそのまま返し、呼び出し側が「0 件」として正直に扱う）。
/// `Err` は恒久的な失敗（タイムアウト・サーバ死亡・変な応答形）。
pub async fn document_symbols_at(
    session: &Mutex<LspSession>,
    path: &Path,
    text: &str,
) -> Result<Vec<mina_protocol::OutlineSymbol>, String> {
    // 解析前の null / 空配列も解析待ちとしてリトライする（references と同じ）。
    // 空のファイルが本当に「記号なし」の場合も予算だけ余分に待つ — references と
    // 同じ許容（予算切れ後はそのまま空を返す）。
    let is_loading = |r: &Value| r.is_null() || r.as_array().map_or(true, |a| a.is_empty());
    let result = request_with_loading_retry(
        session,
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": uri(path) } }),
        is_loading,
    )
    .await?;
    let Some(items) = result.as_array() else {
        return Err("textDocument/documentSymbol の応答が配列ではありません".into());
    };
    let enc = {
        let Ok(s) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
            return Err("LSP セッションのロックを取得できませんでした".into());
        };
        s.encoding
    };
    let index = LineIndex::new(text);
    Ok(convert_symbol_list(&index, text, enc, items))
}

/// LSP の DocumentSymbol 配列 → 内部形（char インデックス）の再帰変換（ADR-0031）。
/// `range`（記号全体）と `selectionRange`（名前トークン）の両方を LSP 座標から
/// char インデックスへ変換する。未知の kind は [`SymbolKind::Other`] に潰す。
fn convert_symbol_list(
    index: &LineIndex,
    text: &str,
    enc: PositionEncoding,
    items: &[Value],
) -> Vec<mina_protocol::OutlineSymbol> {
    // 形状の判定: DocumentSymbol（階層・selectionRange あり）か SymbolInformation
    // （フラット・location のみ）か。一部サーバはクライアントの広告を無視して
    // SymbolInformation[] を返す — その場合も 0 件の静かな空にしないため両形状に
    // 対応する（実測: 広告しないと rust-analyzer もフラットを返す）。
    if items
        .first()
        .is_some_and(|i| i.get("selectionRange").is_some())
    {
        items
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?;
                let range = symbol_range(index, text, item.get("range")?, enc)?;
                let selection_range = symbol_range(index, text, item.get("selectionRange")?, enc)?;
                let children = item
                    .get("children")
                    .and_then(Value::as_array)
                    .map(|c| convert_symbol_list(index, text, enc, c))
                    .unwrap_or_default();
                Some(mina_protocol::OutlineSymbol {
                    name: name.to_string(),
                    kind: lsp_symbol_kind(item.get("kind")).unwrap_or_default(),
                    range,
                    selection_range,
                    children,
                })
            })
            .collect()
    } else {
        // SymbolInformation（フラット）: location.range を範囲とし、名前トークン
        // 範囲は遠慮なく同じ範囲で代用する（階層広告が効くサーバでは使われない）。
        items
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?;
                let range = symbol_range(index, text, item.get("location")?.get("range")?, enc)?;
                Some(mina_protocol::OutlineSymbol {
                    name: name.to_string(),
                    kind: lsp_symbol_kind(item.get("kind")).unwrap_or_default(),
                    range,
                    selection_range: range,
                    children: Vec::new(),
                })
            })
            .collect()
    }
}

/// DocumentSymbol の `range` / `selectionRange`（LSP 座標）→ char インデックス範囲。
fn symbol_range(
    index: &LineIndex,
    text: &str,
    range: &Value,
    enc: PositionEncoding,
) -> Option<mina_protocol::Range> {
    let start = range.pointer("/start")?;
    let end = range.pointer("/end")?;
    Some(mina_protocol::Range {
        anchor: lsp_pos_to_char_indexed(
            index,
            text,
            start.get("line")?.as_u64()? as u32,
            start.get("character")?.as_u64()? as u32,
            enc,
        ),
        head: lsp_pos_to_char_indexed(
            index,
            text,
            end.get("line")?.as_u64()? as u32,
            end.get("character")?.as_u64()? as u32,
            enc,
        ),
    })
}

/// LSP の SymbolKind（数値）を proto 側の小さな集合へ写像する（ADR-0031）。
/// 未知の値・欠落は [`SymbolKind::Other`]。
fn lsp_symbol_kind(kind: Option<&Value>) -> Option<mina_protocol::SymbolKind> {
    use mina_protocol::SymbolKind;
    Some(match kind?.as_u64()? {
        2 => SymbolKind::Module,                     // Module
        6 | 9 => SymbolKind::Method,                  // Method / Constructor
        12 => SymbolKind::Function,                   // Function
        5 | 11 | 23 | 3 | 4 | 26 => SymbolKind::Type, // Class / Interface / Struct / Namespace / Package / TypeParameter
        10 | 22 => SymbolKind::Enum,                  // Enum / EnumMember
        14 => SymbolKind::Constant,                   // Constant
        13 | 7 | 8 => SymbolKind::Variable,           // Variable / Property / Field
        _ => SymbolKind::Other,
    })
}

/// エージェント入力の 1-origin 行:列（文字数単位）を char インデックスへ変換する
/// （ADR-0031 の EnclosingSymbol 用）。行が範囲外なら最終行、列は行末へ
/// クランプする（`session get --lines` の端クランプと同じ流儀）。
pub fn line_col_to_char_idx(text: &str, line: u32, col: u32) -> usize {
    let index = LineIndex::new(text);
    let line_start = index.line_start(line.saturating_sub(1));
    let line_len = text
        .chars()
        .skip(line_start)
        .take_while(|&c| c != '\n')
        .count();
    line_start + (col.saturating_sub(1) as usize).min(line_len)
}

/// シンボルツリーから、char インデックスを**含む最も深い**記号を返す（ADR-0031）。
/// 候補の範囲（`anchor..head`)を踏み外した場合、奥の children は見ない。
/// 見つからなければ `None`（位置がどの記号にも含まれない）。
pub fn enclosing_symbol(
    symbols: &[mina_protocol::OutlineSymbol],
    char_idx: usize,
) -> Option<&mina_protocol::OutlineSymbol> {
    for sym in symbols {
        if char_idx >= sym.range.anchor && char_idx < sym.range.head {
            return Some(enclosing_symbol(&sym.children, char_idx).unwrap_or(sym));
        }
    }
    None
}

/// LSP 診断アイテム（LSP 座標・severity）を char インデックスに変換する。
///
/// 5c: 行インデックスを1回だけ構築し、各診断の座標変換を O(行長) に抑える
/// （毎回 O(文書長) を診断数ぶん繰り返さない）。
fn convert_diagnostics(
    text: &str,
    enc: PositionEncoding,
    items: Vec<PublishDiagnostic>,
) -> Vec<Diagnostic> {
    let index = LineIndex::new(text);
    items
        .into_iter()
        .take(MAX_DIAGNOSTICS)
        .filter_map(|d| {
            let start = lsp_pos_to_char_indexed(
                &index,
                text,
                d.range.start.line,
                d.range.start.character,
                enc,
            );
            let end = lsp_pos_to_char_indexed(
                &index,
                text,
                d.range.end.line,
                d.range.end.character,
                enc,
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
        .collect()
}

/// `textDocument/inlayHint` の1項目（LSP 座標のまま。表示に使わない
/// kind / tooltip / textEdits / data は読まない）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LspInlayHint {
    position: mina_lsp::LspPosition,
    /// `string | InlayHintLabelPart[]`。
    #[serde(default)]
    label: InlayLabel,
    #[serde(default)]
    padding_left: bool,
    #[serde(default)]
    padding_right: bool,
}

/// `label` の untagged union（プレーン文字列 or parts 配列）。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum InlayLabel {
    Plain(String),
    Parts(Vec<InlayLabelPart>),
}

#[derive(Debug, Clone, Deserialize)]
struct InlayLabelPart {
    value: String,
}

impl InlayLabel {
    /// 表示テキスト: parts 配列は各 part の `value` を連結する
    /// （tooltip / location / command は表示に使わないため無視）。
    fn into_text(self) -> String {
        match self {
            InlayLabel::Plain(s) => s,
            InlayLabel::Parts(parts) => parts.into_iter().map(|p| p.value).collect(),
        }
    }
}

// label 欠落時（`#[serde(default)]`）の既定: 空ヒントは convert 側で落とされる。
impl Default for InlayLabel {
    fn default() -> Self {
        InlayLabel::Plain(String::new())
    }
}

/// LSP の inlay hint 項目を char インデックスに変換する。
///
/// 空 label のヒントは表示・配信の価値がないため落とす（#21 残リスク対策）。
/// 描画側（#24）は position 昇順のポインタ走査を前提とする（highlights と同じ
/// 不変条件）ため、サーバが昇順を保証しない場合に備えて stable sort で整える
/// （同 position は応答順を保持）。
fn convert_inlay_hints(
    text: &str,
    enc: PositionEncoding,
    items: Vec<LspInlayHint>,
) -> Vec<InlayHint> {
    let index = LineIndex::new(text);
    let mut hints: Vec<InlayHint> = items
        .into_iter()
        .filter_map(|h| {
            let position = lsp_pos_to_char_indexed(
                &index,
                text,
                h.position.line,
                h.position.character,
                enc,
            );
            let text = h.label.into_text();
            if text.is_empty() {
                return None;
            }
            Some(InlayHint {
                position,
                text,
                padding_left: h.padding_left,
                padding_right: h.padding_right,
            })
        })
        .collect();
    hints.sort_by_key(|h| h.position);
    hints
}

/// 文書末尾の LSP 座標（全文範囲要求用）。末尾改行は空行として数える。
///
/// utf-8 では `character` がバイト列、utf-16 では UTF-16 単位になる
/// （position.rs の変換と同じ単位系）。
fn doc_end_position(text: &str, enc: PositionEncoding) -> Value {
    let last_line = text.rsplit('\n').next().unwrap_or("");
    let character = match enc {
        PositionEncoding::Utf8 => last_line.len() as u32,
        PositionEncoding::Utf16 => {
            mina_lsp::position::char_to_utf16_col(last_line, last_line.chars().count())
        }
    };
    json!({ "line": text.matches('\n').count() as u32, "character": character })
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
#[cfg(test)]
fn lsp_pos_to_char(text: &str, line: u32, col: u32, enc: PositionEncoding) -> usize {
    lsp_pos_to_char_indexed(&LineIndex::new(text), text, line, col, enc)
}

/// char インデックス → LSP 座標（行・列）。encoding に応じて列の単位が変わる
/// （utf-8 = バイト列、utf-16 = UTF-16 単位）。
fn char_to_lsp_pos(text: &str, char_idx: usize, enc: PositionEncoding) -> (u32, u32) {
    // char 数 → バイト位置（マルチバイト対応。char 境界で必ず切れる）
    let byte_idx = text
        .char_indices()
        .nth(char_idx)
        .map_or(text.len(), |(b, _)| b);
    let line = text[..byte_idx].chars().filter(|&c| c == '\n').count() as u32;
    let line_start = text[..byte_idx].rfind('\n').map_or(0, |i| i + 1);
    let col = match enc {
        PositionEncoding::Utf8 => (byte_idx - line_start) as u32,
        PositionEncoding::Utf16 => {
            let line_col = text[line_start..byte_idx].chars().count();
            mina_lsp::position::char_to_utf16_col(&text[line_start..byte_idx], line_col)
        }
    };
    (line, col)
}

/// `textDocument/definition` の応答から最初の定義位置を取り出す。
/// 応答形状は `Location | Location[] | LocationLink[] | null` を扱う
/// （LocationLink は `targetUri` / `targetRange`）。
fn first_definition_target(value: &Value) -> Option<(String, LspRange)> {
    match value {
        Value::Null => None,
        Value::Array(items) => items.iter().find_map(first_definition_target),
        Value::Object(_) => {
            let uri = value
                .get("uri")
                .or_else(|| value.get("targetUri"))?
                .as_str()?;
            let range = value
                .get("range")
                .or_else(|| value.get("targetRange"))?;
            let range: LspRange = serde_json::from_value(range.clone()).ok()?;
            Some((uri.to_string(), range))
        }
        _ => None,
    }
}

/// 定義スニペットの上限（行数・1行の文字数）。ポップアップ表示のため大きくない。
const MAX_PEEK_LINES: usize = 12;
const MAX_PEEK_LINE_LEN: usize = 200;

/// 定義範囲を包む行 + 続きを数行（本体の入口まで見えるように）切り出す。
/// 戻り値は (開始行番号 1 始まり, スニペット)。
fn definition_snippet(text: &str, range: &LspRange) -> Option<(u32, String)> {
    // 行ごとのバイト範囲を1回の走査で集める（LineIndex は char 位置なので
    // スライスには使えない — ここのみバイト列で持つ）。
    let mut byte_starts = vec![0usize];
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            byte_starts.push(i + 1);
        }
    }
    let last_line = byte_starts.len() - 1;
    let line_end = |line: usize| -> usize {
        byte_starts.get(line + 1).map_or(text.len(), |&s| s - 1)
    };
    let first = (range.start.line as usize).min(last_line);
    let last = (range.end.line as usize + 2)
        .min(first + MAX_PEEK_LINES - 1)
        .min(last_line);
    let lines = (first..=last)
        .map(|line| {
            text[byte_starts[line]..line_end(line)]
                .chars()
                .take(MAX_PEEK_LINE_LEN)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some((first as u32 + 1, lines))
}

/// 対象ファイルを読む（サイズ上限付き）。ディレクトリ等は None。
/// ponytail: 開文書の未保存編集は反映されない（フォーカス文書内の定義は
/// `text` 引数が使われる）。必要になったら editor の文書からも引けるようにする。
async fn read_peek_target(path: &Path) -> Option<String> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let meta = file.metadata().await.ok()?;
    if !meta.is_file() || meta.len() > MAX_PEEK_FILE {
        return None;
    }
    let mut s = String::new();
    file.take(MAX_PEEK_FILE + 1)
        .read_to_string(&mut s)
        .await
        .ok()?;
    if s.len() as u64 > MAX_PEEK_FILE {
        return None;
    }
    Some(s)
}

/// 定義対象ファイルの読み取り上限。
/// ponytail: 固定 4MiB。巨大ファイル内の定義は読めず peek なしになる。
const MAX_PEEK_FILE: u64 = 4 * 1024 * 1024;

/// カーソル位置のシンボル定義を確認用スニペットとして返す（読み取り専用）。
///
/// `textDocument/definition` の応答（Location | Location[] | LocationLink[] | null）
/// の最初の定義を対象ファイルから数行抜き出す。サーバ死亡・ロック待ち・
/// エラー応答・定義なし・対象が読めない場合は `None`。
/// 行内の文字位置（0-origin）→ LSP の `character`（encoding の単位）。
/// 行末を超える文字位置は行末に clamp。TUI（文字インデックス基準）と
/// エージェント（1-origin 行:列基準）の両方が行内の文字位置を LSP 座標へ
/// 直すために使う。
fn char_col_to_lsp_character(line: &str, char_col: usize, enc: PositionEncoding) -> u32 {
    let char_col = char_col.min(line.chars().count());
    let byte_end = line.char_indices().nth(char_col).map_or(line.len(), |(b, _)| b);
    match enc {
        PositionEncoding::Utf8 => byte_end as u32,
        PositionEncoding::Utf16 => {
            let prefix = &line[..byte_end];
            mina_lsp::position::char_to_utf16_col(prefix, prefix.chars().count())
        }
    }
}

/// LSP 座標（0-origin 行・列）のシンボル定義を確認用スニペットとして返す
/// （読み取り専用。ADR-0025）。
///
/// `textDocument/definition` の応答（Location | Location[] | LocationLink[] | null）
/// の最初の定義を対象ファイルから数行抜き出す。サーバ死亡・ロック待ち・
/// エラー応答・定義なし・対象が読めない場合は `None`。
/// TUI（カーソル基準）もエージェント（指定位置基準）も、ここに LSP 座標を
/// 渡して呼ぶ。
pub async fn definition_peek_at(
    session: &Mutex<LspSession>,
    path: &Path,
    text: &str,
    line: u32,
    character: u32,
) -> Option<mina_protocol::Peek> {
    let (target_uri, range) = {
        let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
            return None;
        };
        if session.client.is_dead() {
            return None;
        }
        let result = session
            .client
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": uri(path) },
                    "position": { "line": line, "character": character },
                }),
            )
            .await
            .ok()?;
        first_definition_target(&result)?
    };
    let target_path = PathBuf::from(target_uri.strip_prefix("file://").unwrap_or(&target_uri));
    // フォーカス文書なら渡されたテキスト（未保存編集込み）、それ以外はディスクから読む
    let target_text = if target_path == path {
        text.to_string()
    } else {
        read_peek_target(&target_path).await?
    };
    let (line, text) = definition_snippet(&target_text, &range)?;
    Some(mina_protocol::Peek {
        path: target_path.to_string_lossy().into_owned(),
        line,
        text,
    })
}

/// カーソル基準の TUI 要求用: 文字インデックス → LSP 座標に変換してから
/// [`definition_peek_at`] を呼ぶ（ロックは座標変換の瞬間だけ別途握る）。
pub async fn definition_peek_at_char(
    session: &Mutex<LspSession>,
    path: &Path,
    text: &str,
    head: usize,
) -> Option<mina_protocol::Peek> {
    let (line, character) = {
        let Ok(s) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
            return None;
        };
        s.char_to_lsp_pos(text, head)
    };
    definition_peek_at(session, path, text, line, character).await
}

/// [`definition_peek_at`] の位置解決のみの軽量版（ADR-0049）: 指定位置の
/// `textDocument/definition` で最初の定義先の URI だけを返す（スニペット
/// 取得なし — モジュール横断 outline が「モジュール名 → 定義ファイル」を
/// 解決するための経路）。定義なし・サーバエラー・ロック待ちは `None`。
pub async fn definition_target_uri(
    session: &Mutex<LspSession>,
    path: &Path,
    line: u32,
    character: u32,
) -> Option<String> {
    let (target_uri, _range) = {
        let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
            return None;
        };
        if session.client.is_dead() {
            return None;
        }
        let result = session
            .client
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": uri(path) },
                    "position": { "line": line, "character": character },
                }),
            )
            .await
            .ok()?;
        first_definition_target(&result)?
    };
    Some(target_uri)
}

/// エージェント向け（ADR-0025）: 1-origin 行:列を指定して定義を引く。
/// `col` は文字数単位。行・列が範囲外なら定義なし（`None`）になる。
pub async fn definition_peek_at_line_col(
    session: &Mutex<LspSession>,
    path: &Path,
    text: &str,
    line: u32,
    col: u32,
) -> Option<mina_protocol::Peek> {
    let (lsp_line, lsp_character) = {
        let Ok(s) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
            return None;
        };
        let line_idx = line.saturating_sub(1); // 1-origin → 0-origin
        let Some(line_text) = text.lines().nth(line_idx as usize) else {
            return None; // 行が範囲外: 定義なし
        };
        let char_col = col.saturating_sub(1) as usize;
        (
            line_idx,
            char_col_to_lsp_character(line_text, char_col, s.encoding),
        )
    };
    definition_peek_at(session, path, text, lsp_line, lsp_character).await
}

// ---- hover・ワークスペースシンボル検索・診断 check（ADR-0032） ----

/// hover 応答のテキスト上限（ADR-0032）。rust-analyzer は型シグネチャ + doc
/// コメントを返す — doc は長くなり得るため、エージェントが読む量を抑える
/// （F8 の断片化と同じ方針）。超えたら末尾を `…` で切る。
const MAX_HOVER_CHARS: usize = 2000;

/// エージェント向け（ADR-0032）: 1-origin 行:列を指定して hover（型・シグネチャ・
/// doc）を引く。`col` は文字数単位。行・列が範囲外や hover の無い位置（空白・
/// コメント等）は `None`。LSP エラー・セッション停止も `None`（能力ゲートは
/// daemon 側が行う）。
pub async fn hover_at_line_col(
    session: &Mutex<LspSession>,
    path: &Path,
    text: &str,
    line: u32,
    col: u32,
) -> Option<String> {
    let (lsp_line, lsp_character) = {
        let Ok(s) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
            return None;
        };
        let line_idx = line.saturating_sub(1); // 1-origin → 0-origin
        let Some(line_text) = text.lines().nth(line_idx as usize) else {
            return None; // 行が範囲外: hover なし
        };
        let char_col = col.saturating_sub(1) as usize;
        (
            line_idx,
            char_col_to_lsp_character(line_text, char_col, s.encoding),
        )
    };
    let result = {
        let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
            return None;
        };
        if session.client.is_dead() {
            return None;
        }
        session
            .client
            .request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri(path) },
                    "position": { "line": lsp_line, "character": lsp_character },
                }),
            )
            .await
            .ok()?
    };
    hover_text(&result)
}

/// `textDocument/hover` の応答から表示テキストを取り出す（ADR-0032）。
///
/// `contents` は `string | MarkedString | MarkupContent | MarkedString[]` の
/// どれでもよい（LSP 3.17）。rust-analyzer は配列（先頭 = 型シグネチャ、続いて
/// doc の MarkupContent）、tsserver は MarkupContent を返す。全ての断片を連結し、
/// 上限（[`MAX_HOVER_CHARS`]）で切り詰める。`null` / 形状不正・空は `None`。
fn hover_text(result: &Value) -> Option<String> {
    let contents = result.get("contents")?;
    let mut parts = Vec::new();
    collect_hover_parts(contents, &mut parts);
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join("\n");
    let mut out: String = joined.chars().take(MAX_HOVER_CHARS).collect();
    if out.chars().count() < joined.chars().count() {
        out.push('…');
    }
    Some(out)
}

/// hover の `contents`（string / MarkedString / MarkupContent / 配列）から
/// テキスト断片を集める。MarkedString も MarkupContent も `value` を持つため、
/// オブジェクトは value を採用する（language / kind は表示に使わない）。
fn collect_hover_parts(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_hover_parts(item, out);
            }
        }
        Value::Object(_) => {
            if let Some(v) = value.get("value").and_then(Value::as_str) {
                out.push(v.to_string());
            }
        }
        _ => {}
    }
}

/// `workspace/symbol` によるシンボル検索（ADR-0032）。`query` は空不可
/// （daemon 側が検証済み）。応答は `(uri, kind, name, 0-origin 行)` のリスト。
/// 解析待ち（null）はリトライし、予算切れは最後の結果を返す（references と
/// 同じ規律 — クエリに対する空配列は「該当なし」の正常応答なので null のみ
/// 待ち対象にする）。`Err` は恒久的な失敗（タイムアウト・サーバ死亡）。
pub async fn workspace_symbols(
    session: &Mutex<LspSession>,
    query: &str,
) -> Result<Vec<(String, mina_protocol::SymbolKind, String, u32)>, String> {
    let is_loading = |r: &Value| r.is_null();
    let result = request_with_loading_retry(
        session,
        "workspace/symbol",
        json!({ "query": query }),
        is_loading,
    )
    .await?;
    let Some(items) = result.as_array() else {
        return Err("workspace/symbol の応答が配列ではありません".into());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .ok_or("workspace/symbol の SymbolInformation に name がありません")?;
        let kind = lsp_symbol_kind(item.get("kind")).unwrap_or_default();
        let u = item
            .get("location")
            .and_then(|l| l.get("uri"))
            .and_then(Value::as_str)
            .ok_or("workspace/symbol の SymbolInformation に location.uri がありません")?;
        let line = item
            .pointer("/location/range/start/line")
            .and_then(Value::as_u64)
            .ok_or("workspace/symbol の SymbolInformation に location.range.start.line がありません")?;
        out.push((u.to_string(), kind, name.to_string(), line as u32));
    }
    Ok(out)
}

/// `session check` 用（ADR-0032）: 診断が安定するまで pull を繰り返し、最後の
/// 結果を返す。
///
/// 安定判定は settle_open_diagnostics と同じ: 非空が 2 回連続で同数 = 安定
/// （`settled` を true にして返す）、空のまま予算（[`SEMANTIC_RETRIES`] ×
/// [`SEMANTIC_RETRY_WAIT`] ≈ 10 秒）を使い切ったら「クリーン**未確認**」として
/// 最後の空と `settled: false` を返す（ADR-0045 — 解析未完・クロスシンボル
/// 破壊の可能性を隠さない）。非空が 1 回だけのまま予算切れも `settled: false`。
/// `None` は恒久的な失敗（セッションロック待ち・サーバ死亡・対象が current でない）。
pub async fn pull_diagnostics_settled(
    session: &Mutex<LspSession>,
    path: &Path,
    text: &str,
) -> Option<(Vec<Diagnostic>, bool)> {
    let mut prev: Option<usize> = None;
    let mut last: Vec<Diagnostic> = Vec::new();
    for _ in 0..SEMANTIC_RETRIES {
        let pulled = {
            let Ok(mut s) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
                return None;
            };
            if s.client.is_dead() {
                return None;
            }
            s.pull_diagnostics(path, text).await
        };
        let Some(diags) = pulled else {
            return None;
        };
        let n = diags.len();
        if n > 0 && prev == Some(n) {
            return Some((diags, true)); // 非空が2回連続で同数 = 安定
        }
        prev = Some(n);
        last = diags;
        tokio::time::sleep(SEMANTIC_RETRY_WAIT).await;
    }
    Some((last, false)) // 予算切れ: 未確認（空ならクリーン扱いにしない — ADR-0045）
}

// ---- 意味リネーム・参照（ADR-0029） ----

/// 1ファイル分の rename 編集（LSP 座標を char インデックスへ変換済み）。
pub struct RenameFile {
    pub path: PathBuf,
    /// 適用前テキストに対する char 範囲の置換。同一ファイル内で重複しない
    /// （[`lsp_edits_to_char`] が保証）。
    pub edits: Vec<RenameEdit>,
}

/// char インデックス範囲のテキスト置換。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameEdit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// WorkspaceEdit の要素（まだ LSP 座標のまま。文字への変換は対象ファイルの
/// テキストが必要なため、daemon 側で [`lsp_edits_to_char`] を呼ぶ）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawFileEdits {
    pub uri: String,
    pub edits: Vec<RawLspEdit>,
}

/// WorkspaceEdit 内の 1 編集（LSP 座標の range + 新テキスト）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawLspEdit {
    pub range: LspRange,
    pub new_text: String,
}

/// rename / references が「解析待ち」と判定する条件。
///
/// rust-analyzer はワークスペースロード完了前に rename / references を要求
/// されると `error`（ContentModified「-32801」や「No references found」）を返す
/// （M0 実測: 1.98.0）。minae-lsp の [`Client`] は LSP の error 応答を `Null` に
/// 潰して返すため（minae-lsp/src/lib.rs reader）、ここでは「結果が Null」を
/// リトライ条件とする。Null か結果かを区別できないため、シンボルが本当に
/// rename 不能な場合もリトライ予算（約 10 秒）だけ余分に待ってから
/// 「not found」扱いになる。
const SEMANTIC_RETRIES: usize = 20;
const SEMANTIC_RETRY_WAIT: Duration = Duration::from_millis(500);

/// `textDocument/rename` を実行し、WorkspaceEdit を内部形へ変換して返す。
///
/// - `Ok(Some(files))`: 適用すべき編集（`changes` / `documentChanges` の両形式に
///   対応。resource 変更（create/delete/rename file）は未対応としてエラー）。
/// - `Ok(None)`: リトライ予算を使い切っても結果が得られなかった = 解析未完
///   または対象位置に rename 可能なシンボルがない。
/// - `Err(msg)`: 恒久的エラー（タイムアウト・サーバ死亡・未対応の WorkspaceEdit）。
/// `textDocument/rename` を実行し、WorkspaceEdit を内部形へ変換して返す。
///
/// - `Ok(files)`: 適用すべき編集（`changes` / `documentChanges` の両形式に
///   対応。resource 変更（create/delete/rename file）は未対応としてエラー）。
///   空リストは「rename できるものが無い」（シンボル未解決・解析未完の可能性）。
/// - `Err(msg)`: 恒久的エラー（タイムアウト・サーバ死亡・未対応の WorkspaceEdit）。
pub async fn rename_at(
    session: &Mutex<LspSession>,
    path: &Path,
    line: u32,
    character: u32,
    new_name: &str,
) -> Result<Vec<RawFileEdits>, String> {
    // 解析前は null または空の WorkspaceEdit が返る — どちらも解析待ちとして
    // リトライする（予算切れ後の空は「rename 結果なし」として daemon 側が扱う）。
    let is_loading = |r: &Value| r.is_null() || workspace_edit_is_empty(r);
    let result = request_with_loading_retry(
        session,
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri(path) },
            "position": { "line": line, "character": character },
            "newName": new_name,
        }),
        is_loading,
    )
    .await?;
    parse_workspace_edit(&result)
}

/// `textDocument/references` を実行し、参照位置（uri, 0-origin 行番号）を返す。
/// `includeDeclaration` で定義も含めた全参照を要求する。戻り値は `None` が
/// 解析未完（リトライ予算切れ）。
pub async fn references_at(
    session: &Mutex<LspSession>,
    path: &Path,
    line: u32,
    character: u32,
) -> Result<Vec<(String, u32)>, String> {
    // 解析前の "空配列"（rust-analyzer がロード中に返す）も解析待ちとして
    // リトライし、2回連続で同一になるまで待つ（インクリメンタルに増える参照を
    // 取りこぼさない — T5 の教訓）。予算切れ後の空は「参照なし」として返す。
    let is_loading = |r: &Value| r.is_null() || r.as_array().map_or(true, |a| a.is_empty());
    let result = request_with_loading_retry(
        session,
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri(path) },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": true },
        }),
        is_loading,
    )
    .await?;
    let Some(items) = result.as_array() else {
        return Err("textDocument/references の応答が配列ではありません".into());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let u = item
            .get("uri")
            .and_then(Value::as_str)
            .ok_or("references の Location に uri がありません")?;
        let line = item
            .pointer("/range/start/line")
            .and_then(Value::as_u64)
            .ok_or("references の Location に line がありません")?;
        out.push((u.to_string(), line as u32));
    }
    Ok(out)
}

/// WorkspaceEdit が編集を 1 件も含まないか（`changes`/`documentChanges` の両方を
/// 見る）。解析前の空応答と、確定した「何も編集がない」は区別できないため、
/// リトライ側はこれを解析待ちとして扱う（予算切れ後はそのまま空として返す）。
fn workspace_edit_is_empty(value: &Value) -> bool {
    let changes_empty = value
        .get("changes")
        .map_or(true, |c| c.as_object().map_or(true, |o| o.is_empty()));
    let dc_empty = value
        .get("documentChanges")
        .map_or(true, |d| d.as_array().map_or(true, |a| a.is_empty()));
    changes_empty && dc_empty
}

/// LSP リクエストを投げ、結果が「解析待ち」の形（`is_loading` が真）の間、
/// または結果がまだ安定しない間（2回連続で同一にならない）リトライする。
/// `Err` は恒久的な失敗（タイムアウト・サーバ死亡）のみ。
///
/// リトライ予算（[`SEMANTIC_RETRIES`] × [`SEMANTIC_RETRY_WAIT`] ≈ 10 秒）を
/// 使い切ったら「最後の結果」を返す（解析が不完全かもしれないが、保持できる
/// 最良の情報を返す — 呼び出し側が空/不完全をそのまま扱う）。
async fn request_with_loading_retry(
    session: &Mutex<LspSession>,
    method: &str,
    params: Value,
    is_loading: fn(&Value) -> bool,
) -> Result<Value, String> {
    let mut prev: Option<Value> = None;
    let mut retried = 0;
    loop {
        let result = {
            let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
                return Err("LSP セッションのロックを取得できませんでした".into());
            };
            if session.client.is_dead() {
                return Err("LSP サーバが停止しています（再起動を待つか再実行してください）".into());
            }
            session
                .client
                .request(method, params.clone())
                .await
                .map_err(|e| format!("LSP エラー: {e}"))?
        };
        let loading = is_loading(&result);
        if !loading && prev.as_ref() == Some(&result) {
            return Ok(result); // 2回連続で同一 = 解析が安定
        }
        if retried >= SEMANTIC_RETRIES {
            return Ok(result); // 予算切れ: 最後の結果（空/不完全の可能性）を返す
        }
        prev = Some(result);
        retried += 1;
        tokio::time::sleep(SEMANTIC_RETRY_WAIT).await;
    }
}

/// WorkspaceEdit をパースする。`changes`（uri → edits マップ）と
/// `documentChanges`（TextDocumentEdit 配列）の両形式に対応し、両方があれば
/// 併合する。resource 変更（CreateFile / RenameFile / DeleteFile）を含む
/// `documentChanges` は未対応としてエラーを返す（シンボル rename では発生しない）。
fn parse_workspace_edit(value: &Value) -> Result<Vec<RawFileEdits>, String> {
    let mut out = Vec::new();
    if let Some(changes) = value.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            let edits: Vec<RawLspEdit> = serde_json::from_value(edits.clone())
                .map_err(|e| format!("WorkspaceEdit.changes を解釈できません: {e}"))?;
            out.push(RawFileEdits {
                uri: uri.clone(),
                edits,
            });
        }
    }
    if let Some(doc_changes) = value.get("documentChanges").and_then(Value::as_array) {
        for item in doc_changes {
            let Some(edits) = item.get("edits") else {
                return Err(
                    "未対応の WorkspaceEdit です（resource 変更 = ファイル作成/削除/移動が含まれる）"
                        .into(),
                );
            };
            let u = item
                .pointer("/textDocument/uri")
                .and_then(Value::as_str)
                .ok_or("WorkspaceEdit.documentChanges に textDocument.uri がありません")?;
            let edits: Vec<RawLspEdit> = serde_json::from_value(edits.clone())
                .map_err(|e| format!("WorkspaceEdit.documentChanges.edits を解釈できません: {e}"))?;
            out.push(RawFileEdits {
                uri: u.to_string(),
                edits,
            });
        }
    }
    Ok(out)
}

/// LSP 座標の編集を、対象テキストに対する char インデックスの置換に変換する。
/// 範囲がファイルを超える・範囲が逆転・同一ファイル内で編集範囲が重複する場合は
/// エラー（適用前に検出してディスクを汚さない）。
pub fn lsp_edits_to_char(
    text: &str,
    enc: PositionEncoding,
    edits: &[RawLspEdit],
) -> Result<Vec<RenameEdit>, String> {
    let index = LineIndex::new(text);
    let len = text.chars().count();
    let line_count = text.lines().count() as u32;
    let mut out = Vec::with_capacity(edits.len());
    for e in edits {
        // 行が文書を超える場合、lsp_pos_to_char_indexed は最終行へクランプする
        // （診断向けの既定挙動）。rename は陳腐化した range の適用が破壊的
        // なので、ここでは明示的に範囲外として拒否する（適用前に全失敗を検出）。
        if e.range.start.line >= line_count || e.range.end.line >= line_count {
            return Err(format!(
                "WorkspaceEdit の行がファイルを超えています (行 {}/{})",
                e.range.start.line.max(e.range.end.line),
                line_count
            ));
        }
        let start = lsp_pos_to_char_indexed(&index, text, e.range.start.line, e.range.start.character, enc);
        let end = lsp_pos_to_char_indexed(&index, text, e.range.end.line, e.range.end.character, enc);
        if start > end || end > len {
            return Err(format!(
                "WorkspaceEdit の範囲がファイルを超えています [{start}, {end}) / 長さ {len}"
            ));
        }
        out.push(RenameEdit {
            start,
            end,
            text: e.new_text.clone(),
        });
    }
    // 重複検査（LSP が保証するが、防御: 重複適用は破壊的）。
    let mut sorted: Vec<_> = out.iter().collect();
    sorted.sort_by_key(|e| e.start);
    for w in sorted.windows(2) {
        if w[1].start < w[0].end {
            return Err("WorkspaceEdit 内に重複する編集範囲があります".into());
        }
    }
    Ok(out)
}

/// 同一ファイル内の複数置換を、range 降順（bottom-up）で適用した新しいテキストを返す。
/// 編集は char インデックス基準（適用前テキストに対するもの）。
pub fn apply_char_edits(text: &str, edits: &[RenameEdit]) -> String {
    let mut out = text.to_string();
    let mut edits: Vec<_> = edits.iter().collect();
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    for e in edits {
        let start_byte = char_byte_idx(&out, e.start);
        let end_byte = char_byte_idx(&out, e.end);
        out.replace_range(start_byte..end_byte, &e.text);
    }
    out
}

/// char インデックス → バイトインデックス（末尾 clamp）。`Rope` の char/byte 変換が
/// できない場面で使う（この関数は小さい編集リストに対してのみ呼ばれる）。
fn char_byte_idx(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map_or(text.len(), |(b, _)| b)
}

/// `old` の最初の「識別子としての」出現位置（char インデックス）を返す。
///
/// tree-sitter でリーフの識別子ノード（kind に "identifier" を含む）に限定して
/// マッチするため、コメント・文字列リテラル内の出現には解決しない（T3 の
/// 「誤位置への静かな適用」の事故クラスを予防）。grammar が無い言語・パース
/// 失敗時は単語境界検索にフォールバックする。
pub fn find_symbol_char_idx(
    grammar: Option<&'static mina_loader::LanguageDef>,
    text: &str,
    old: &str,
) -> Option<usize> {
    if old.is_empty() || text.is_empty() {
        return None;
    }
    // grammar は languages.toml の `[[language]].grammar` 経由で渡される
    // （ADR-0030 Stage 4）。None なら tree-sitter による識別子限定は諦めて fallback。
    if let Some(def) = grammar {
        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&(def.grammar)()).is_ok() {
            if let Some(tree) = parser.parse(text, None) {
                if let Some(found) = first_identifier_leaf(text, &tree, old) {
                    return Some(found);
                }
            }
        }
    }
    // fallback: 単語境界による最初の出現（コメント除外は効かない — 言語不明時）。
    first_word_occurrence(text, old)
}

/// ツリーを pre-order に走査し、テキストが `old` と一致する最初のリーフ識別子
/// ノードの char インデックスを返す。
fn first_identifier_leaf(text: &str, tree: &tree_sitter::Tree, old: &str) -> Option<usize> {
    let mut cursor = tree.root_node().walk();
    let mut done = false;
    while !done {
        let node = cursor.node();
        if node.child_count() == 0
            && node.is_named()
            && node.kind().contains("identifier")
            && text.as_bytes().get(node.start_byte()..node.end_byte()) == Some(old.as_bytes())
        {
            return Some(text[..node.start_byte()].chars().count());
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                done = true;
                break;
            }
        }
    }
    None
}

/// 単語境界（先頭/末尾が英数字・`_` 以外）での最初の出現位置。
fn first_word_occurrence(text: &str, old: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + old.len() <= bytes.len() {
        if &bytes[i..i + old.len()] == old.as_bytes() {
            let before_ok = i == 0 || !b_alnum(bytes[i - 1]);
            let after = i + old.len();
            let after_ok = after == bytes.len() || !b_alnum(bytes[after]);
            if before_ok && after_ok {
                return Some(text[..i].chars().count());
            }
        }
        i += 1;
    }
    None
}

fn b_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// ---- daemon 統合 ----

/// 文書を開いたことを LSP に通知する（daemon ロック外・lsp mutex のみ）。
///
/// MEDIUM-4: ロック取得にもタイムアウトを付け、他タスクが hung サーバの
/// notify でロックを握り続けていても Open コマンドをブロックしない。
pub async fn open_document(session: &Mutex<LspSession>, path: &Path, text: &str) {
    let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
        return; // サーバが忙しい: didOpen は次回の .rs Open で送られる
    };
    session.did_open(path, text).await;
}

/// 文書を「閉じずに」開いたことを LSP に通知する（`open_document` の keep 版。
/// [`LspSession::did_open_keep`] 参照）。
pub async fn open_document_keep(session: &Mutex<LspSession>, path: &Path, text: &str) {
    let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
        return; // サーバが忙しい: 欠落は次の Open で補われる
    };
    session.did_open_keep(path, text).await;
}

/// 編集後に全文同期する（現在の文書が LSP の監視対象のときだけ）。
///
/// M1: daemon ロック外で呼ばれる（lsp mutex のみ）。
/// M3: サーバが死んでいたら何もしない（無駄な 2s タイムアウト待ちを避ける。
/// 再 spawn は次回 .rs Open 時に `ensure` が行う）。
/// MEDIUM-4: ロック取得もタイムアウト付き（他タスクがロックを握っていても
/// 編集の応答を待たせない — 全文同期なので次の編集の sync が最新テキストを運ぶ）。
pub async fn sync(session: &Mutex<LspSession>, path: &Path, text: &str) {
    let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
        return;
    };
    if session.client.is_dead() {
        return;
    }
    let doc_uri = uri(path);
    if session.current_uri() != Some(doc_uri.as_str()) {
        return; // 現在の文書は LSP 非対象（.rs 以外を編集中）
    }
    session.did_change(path, text).await;
}

/// 未処理の LSP 通知を取り込み、daemon の診断を維持する。
///
/// 編集後の診断を pull で取り込む（daemon ロック外・lsp mutex のみ）。
///
/// didChange の直後は解析未完了で pull が空を返すため、[`PULL_SETTLE`] だけ
/// 待ってから打つ。`None`（サーバ死亡・ロック待ち・エラー応答）なら呼び出し側は
/// 現状維持する。
///
/// ponytail: 固定待ち 250ms。解析が遅い環境では「解析前の空」が返り、次の編集
/// まで診断が消えることがある。気になるなら push 通知を解析完了シグナルとして
/// 使ってから pull する方式に差し替える。
pub async fn pull_after_edit(
    session: &Mutex<LspSession>,
    path: &Path,
    text: &str,
) -> (Option<Vec<Diagnostic>>, Option<Vec<InlayHint>>) {
    tokio::time::sleep(PULL_SETTLE).await;
    let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
        return (None, None);
    };
    if session.client.is_dead() {
        return (None, None);
    }
    // 診断とヒントを続けて pull する（同じテキスト・同じ解析状態に対して）。
    let diags = session.pull_diagnostics(path, text).await;
    let hints = session.pull_inlay_hints(path, text).await;
    (diags, hints)
}

/// セッションを掴んで inlay hint を pull する（ロック取得はタイムアウト付き）。
///
/// エージェントの GetInlayHints 処理（daemon 側の serve_inlay_hints）用。
/// サーバ死亡・ロック待ち・エラー応答は `None`（呼び出し側は現状維持）。
pub async fn pull_hints_timeout(
    session: &Mutex<LspSession>,
    path: &Path,
    text: &str,
) -> Option<Vec<InlayHint>> {
    let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
        return None;
    };
    if session.client.is_dead() {
        return None;
    }
    session.pull_inlay_hints(path, text).await
}

/// フォーカス文書へ LSP セッションを復元する（Q10-(c)）: 現在テキストで
/// didOpen し直し、診断を即時 pull して返す（解析は温かいため settle 待ちなし）。
///
/// エージェントの GetInlayHints がセッションを借りた後の自己修復。サーバ死亡・
/// ロック待ちは `None`（呼び出し側は現状維持）。
pub async fn restore_focus_with_diagnostics(
    session: &Mutex<LspSession>,
    path: &Path,
    text: &str,
) -> Option<Vec<Diagnostic>> {
    open_document(session, path, text).await;
    let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
        return None;
    };
    if session.client.is_dead() {
        return None;
    }
    session.pull_diagnostics(path, text).await
}

#[cfg(test)]
mod tests {
    use super::*;
#[cfg(test)]
use std::sync::Arc;

    #[test]
    

fn capabilities_of_parses_initialize_response() {
        // 全機能 advertise（rust-analyzer / mock 相当）: 全部対応
        let caps = capabilities_of(&json!({
            "capabilities": {
                "positionEncoding": "utf-8",
                "diagnosticProvider": { "identifier": "x" },
                "inlayHintProvider": {},
                "renameProvider": true,
                "referencesProvider": true,
            }
        }));
        assert!(caps.pull_diagnostics);
        assert!(caps.inlay_hints);
        assert!(caps.rename);
        assert!(caps.references);
        assert!(!caps.definition, "未 advertise は非対応");
        // 何も advertise しないサーバ（未検証サーバの自然なゲート）
        let none = capabilities_of(&json!({ "capabilities": {} }));
        assert!(!none.pull_diagnostics && !none.inlay_hints && !none.rename && !none.references);
        assert!(!none.definition);
        // 明示 false は非対応扱い。オブジェクト形式（RenameOptions 等）は対応扱い
        let mixed = capabilities_of(&json!({
            "capabilities": {
                "renameProvider": false,
                "definitionProvider": true,
            }
        }));
        assert!(!mixed.rename);
        assert!(mixed.definition);
        // null（spec 違反だが稀に advertise される）も非対応扱い
        let null_cap = capabilities_of(&json!({
            "capabilities": { "renameProvider": null }
        }));
        assert!(!null_cap.rename, "null は非対応扱い");
        let obj = capabilities_of(&json!({
            "capabilities": { "renameProvider": { "prepareProvider": true } }
        }));
        assert!(obj.rename, "オブジェクト形式の renameProvider は対応");
    }

    #[test]
    fn lsp_pos_to_char_utf8_multi_line() {
        let text = "ab\ncd\nこんにちは";
        // 2行目（cd）の char 1 = 'd'（"ab\n" の3 + 1）
        assert_eq!(lsp_pos_to_char(text, 1, 1, PositionEncoding::Utf8), 4);
        // 3行目のバイト6 = こんにちは の 2文字目（ん）。行開始は "ab\ncd\n" の6
        assert_eq!(lsp_pos_to_char(text, 2, 6, PositionEncoding::Utf8), 8);
    }

    #[test]
    fn char_to_lsp_pos_converts_with_encoding() {
        // UTF-8: 列はバイト列
        assert_eq!(char_to_lsp_pos("ab\ncd", 4, PositionEncoding::Utf8), (1, 1));
        // 先頭
        assert_eq!(char_to_lsp_pos("ab\ncd", 0, PositionEncoding::Utf8), (0, 0));
        // 末尾（改行直後の空行）
        assert_eq!(char_to_lsp_pos("ab\n", 3, PositionEncoding::Utf8), (1, 0));
        // UTF-16: サロゲートペアは2単位
        let text = "a😀b\ncd";
        assert_eq!(char_to_lsp_pos(text, 3, PositionEncoding::Utf16), (0, 4)); // 改行（😀 の2単位込み）
        assert_eq!(char_to_lsp_pos(text, 5, PositionEncoding::Utf16), (1, 1)); // 'd'
        // 範囲外は clamp
        assert_eq!(char_to_lsp_pos("ab", 99, PositionEncoding::Utf8), (0, 2));
    }

    #[test]
    fn char_col_to_lsp_character_converts_with_encoding() {
        // UTF-8: 列は行先頭からのバイト列
        assert_eq!(char_col_to_lsp_character("abc", 1, PositionEncoding::Utf8), 1);
        // マルチバイトはバイト数で数える
        assert_eq!(char_col_to_lsp_character("あいう", 1, PositionEncoding::Utf8), 3);
        // UTF-16: サロゲートペアは2単位
        assert_eq!(char_col_to_lsp_character("a😀b", 2, PositionEncoding::Utf16), 3);
        // 行末を超える列は行末に clamp
        assert_eq!(char_col_to_lsp_character("abc", 99, PositionEncoding::Utf8), 3);
        assert_eq!(char_col_to_lsp_character("", 0, PositionEncoding::Utf8), 0);
    }

    #[test]
    fn first_definition_target_handles_all_shapes() {
        use serde_json::json;
        let pos = |l: u32, c: u32| json!({ "line": l, "character": c });
        // Location 単体
        let loc = json!({"uri": "file:///a.rs", "range": {"start": pos(1, 2), "end": pos(1, 5)}});
        let (uri, range) = first_definition_target(&loc).unwrap();
        assert_eq!(uri, "file:///a.rs");
        assert_eq!(range.start.line, 1);
        assert_eq!(range.start.character, 2);
        // Location[] — 最初の要素を取る
        let arr = json!([null, loc]);
        let (uri, _) = first_definition_target(&arr).unwrap();
        assert_eq!(uri, "file:///a.rs");
        // LocationLink（targetUri / targetRange）
        let link = json!({"originSelectionRange": {"start": pos(0, 0), "end": pos(0, 1)}, "targetUri": "file:///b.rs", "targetRange": {"start": pos(3, 0), "end": pos(3, 4)}, "targetSelectionRange": {"start": pos(3, 0), "end": pos(3, 4)}});
        let (uri, range) = first_definition_target(&link).unwrap();
        assert_eq!(uri, "file:///b.rs");
        assert_eq!(range.start.line, 3);
        // LocationLink[]
        let (uri, _) = first_definition_target(&json!([link])).unwrap();
        assert_eq!(uri, "file:///b.rs");
        // null / 空配列 / 無関係なオブジェクト
        assert!(first_definition_target(&Value::Null).is_none());
        assert!(first_definition_target(&json!([])).is_none());
        assert!(first_definition_target(&json!([null, null])).is_none());
        assert!(first_definition_target(&json!({ "foo": 1 })).is_none());
    }

    #[test]
    fn parse_workspace_edit_handles_changes_and_document_changes() {
        use serde_json::json;
        let pos = |l: u32, c: u32| json!({ "line": l, "character": c });
        let edit = |l: u32, c: u32, new: &str| json!({
            "range": { "start": pos(l, c), "end": pos(l, c + 3) },
            "newText": new,
        });
        // changes 形式（uri → 編集リスト）
        let v = json!({
            "changes": { "file:///a.rs": [edit(0, 1, "AAA")], "file:///b.rs": [edit(2, 0, "BBB")] }
        });
        let files = parse_workspace_edit(&v).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].uri, "file:///a.rs");
        assert_eq!(files[0].edits[0].new_text, "AAA");
        // documentChanges 形式（TextDocumentEdit）
        let v = json!({
            "documentChanges": [{
                "textDocument": { "uri": "file:///c.rs", "version": 1 },
                "edits": [edit(1, 5, "CCC")],
            }]
        });
        let files = parse_workspace_edit(&v).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].uri, "file:///c.rs");
        assert_eq!(files[0].edits[0].new_text, "CCC");
        // 両方があれば併合
        let v = json!({
            "changes": { "file:///a.rs": [edit(0, 1, "AAA")] },
            "documentChanges": [{
                "textDocument": { "uri": "file:///d.rs", "version": 1 },
                "edits": [edit(0, 0, "DDD")],
            }]
        });
        let files = parse_workspace_edit(&v).unwrap();
        assert_eq!(files.len(), 2);
        // resource 変更（CreateFile 等）は未対応としてエラー
        let v = json!({
            "documentChanges": [{ "kind": "create", "uri": "file:///new.rs" }]
        });
        assert!(parse_workspace_edit(&v).is_err());
        // 空 WorkspaceEdit
        assert_eq!(parse_workspace_edit(&json!({})).unwrap().len(), 0);
    }

    #[test]
    fn lsp_edits_to_char_converts_and_validates() {
        let text = "ab cd\nef gh";
        let raw = |l: u32, s: u32, e: u32, new: &str| RawLspEdit {
            range: LspRange {
                start: mina_lsp::LspPosition { line: l, character: s },
                end: mina_lsp::LspPosition { line: l, character: e },
            },
            new_text: new.into(),
        };
        // utf-8: 列 = バイト（ASCII は char と一致）
        let edits = lsp_edits_to_char(text, PositionEncoding::Utf8, &[raw(0, 0, 2, "XX")]).unwrap();
        assert_eq!(edits, vec![RenameEdit { start: 0, end: 2, text: "XX".into() }]);
        // 範囲が文書を超える → エラー
        assert!(lsp_edits_to_char(text, PositionEncoding::Utf8, &[raw(5, 0, 1, "X")]).is_err());
        // 範囲の逆転 → エラー
        assert!(lsp_edits_to_char(text, PositionEncoding::Utf8, &[raw(0, 3, 1, "X")]).is_err());
        // 重複範囲 → エラー（適用順で破壊するため）
        let dup = [raw(0, 0, 3, "A"), raw(0, 2, 4, "B")];
        assert!(lsp_edits_to_char(text, PositionEncoding::Utf8, &dup).is_err());
        // 同一位置への複数編集（0-len insert）は重複とみなさない
        let ins = [raw(0, 2, 2, "A"), raw(0, 4, 4, "B")];
        assert_eq!(lsp_edits_to_char(text, PositionEncoding::Utf8, &ins).unwrap().len(), 2);
    }

    #[test]
    fn apply_char_edits_applies_bottom_up() {
        // 後方の編集が前方の編集の位置を崩さない（降順適用）
        let text = "aaaa bbbb cccc";
        let edits = [
            RenameEdit { start: 0, end: 4, text: "X".into() },
            RenameEdit { start: 5, end: 9, text: "Y".into() },
            RenameEdit { start: 10, end: 14, text: "Z".into() },
        ];
        assert_eq!(apply_char_edits(text, &edits), "X Y Z");
        // 複数行に跨る
        let text = "foo\nbar\nbaz";
        let edits = [
            RenameEdit { start: 0, end: 3, text: "F".into() },
            RenameEdit { start: 4, end: 7, text: "B".into() },
            RenameEdit { start: 8, end: 11, text: "C".into() },
        ];
        assert_eq!(apply_char_edits(text, &edits), "F\nB\nC");
        // 全文長が変わる編集（挿入）の後に来る編集も正しい
        let text = "ab";
        let edits = [
            RenameEdit { start: 0, end: 0, text: "<>".into() },
            RenameEdit { start: 2, end: 2, text: "[]".into() },
        ];
        assert_eq!(apply_char_edits(text, &edits), "<>ab[]");
    }

    #[test]
    fn find_symbol_char_idx_skips_comments_and_strings() {
        let dir = std::env::temp_dir();
        let path = dir.join("sym-test.rs");
        // コメントと文字列内の出現は無視し、最初の識別子（定義）に解決する。
        // grammar は languages.toml 経由で渡される（ADR-0030 Stage 4）:
        // tree-sitter が使える場合と使えない場合（None = 単語境界 fallback）を両方検証する。
        let grammar = mina_loader::language_by_name("rust");
        let text = "// rate = 1\nlet rate = 2;\nlet s = \"rate\";\nlet t = rate * 3;\n";
        let idx = find_symbol_char_idx(grammar, text, "rate").unwrap();
        // "let rate" — 2行目の 'rate' の開始位置: "// rate = 1\n" (12 chars) + "let " (4) = 16
        assert_eq!(&text[idx..idx + 4], "rate");
        assert_eq!(idx, 16);
        // 無い名前は None
        assert!(find_symbol_char_idx(grammar, text, "nope").is_none());
        // 空・空文字列
        assert!(find_symbol_char_idx(grammar, "", "rate").is_none());
        assert!(find_symbol_char_idx(grammar, text, "").is_none());
        // grammar なし（言語未登録相当）は単語境界 fallback でヒットする
        let idx = find_symbol_char_idx(None, text, "rate").unwrap();
        assert_eq!(&text[idx..idx + 4], "rate");
    }

    #[test]
    fn find_symbol_char_idx_works_for_typescript() {
        // Stage 4: TS も tree-sitter で識別子に限定して解決する
        let grammar = mina_loader::language_by_name("typescript");
        let text =
            "// foo = 1\nconst foo = 2;\nexport function bar() { return foo; }\n";
        let idx = find_symbol_char_idx(grammar, text, "foo").unwrap();
        assert_eq!(&text[idx..idx + 3], "foo", "コメント内の foo は無視して const foo に解決");
        assert_eq!(idx, "// foo = 1\nconst ".len());
    }

    #[test]
    fn first_word_occurrence_is_boundary_aware() {
        let text = "let rate2 = rate * rate_2; // rate";
        // rate2 や rate_2 には一致せず、境界付きの rate に一致する
        let idx = first_word_occurrence(text, "rate").unwrap();
        assert_eq!(&text[idx..idx + 4], "rate");
        assert_eq!(idx, 12, "'rate2' を飛ばして 2 つ目の rate に一致: {text:?}");
        assert!(first_word_occurrence("rate2", "rate").is_none());
        assert!(first_word_occurrence(text, "absent").is_none());
    }

    #[test]
    fn definition_snippet_covers_range_plus_following_lines() {
        let text = "a\npub fn f(x: i32) -> i32 {\n    x * 2\n}\nnext";
        let range = LspRange {
            start: mina_lsp::LspPosition { line: 1, character: 0 },
            end: mina_lsp::LspPosition { line: 1, character: 9 },
        };
        let (line, snippet) = definition_snippet(text, &range).unwrap();
        assert_eq!(line, 2, "1 始まり");
        assert_eq!(snippet, "pub fn f(x: i32) -> i32 {\n    x * 2\n}", "定義行 + 本体 2 行");
        // 範囲が複数行に跨る場合はその行まで
        let range = LspRange {
            start: mina_lsp::LspPosition { line: 1, character: 0 },
            end: mina_lsp::LspPosition { line: 2, character: 4 },
        };
        let (_, snippet) = definition_snippet(text, &range).unwrap();
        assert!(snippet.starts_with("pub fn f"));
        // 範囲外行は最後の行に clamp
        let range = LspRange {
            start: mina_lsp::LspPosition { line: 99, character: 0 },
            end: mina_lsp::LspPosition { line: 99, character: 1 },
        };
        assert_eq!(definition_snippet(text, &range).unwrap().0, 5);
        // 1行の長い行は切り詰める
        let long = "x".repeat(500);
        let range = LspRange {
            start: mina_lsp::LspPosition { line: 0, character: 0 },
            end: mina_lsp::LspPosition { line: 0, character: 1 },
        };
        let (_, snippet) = definition_snippet(&long, &range).unwrap();
        assert_eq!(snippet.chars().count(), MAX_PEEK_LINE_LEN);
    }

    #[test]
    fn definition_snippet_empty_text() {
        let range = LspRange {
            start: mina_lsp::LspPosition { line: 0, character: 0 },
            end: mina_lsp::LspPosition { line: 0, character: 0 },
        };
        assert_eq!(definition_snippet("", &range).unwrap().0, 1);
    }

    #[test]
    fn lsp_pos_to_char_utf16_cjk() {
        let text = "aあ😀b\nnext";
        // 1行目: a(1) あ(1) 😀(2) → utf-16 4 は 'b' の位置 = char 3
        assert_eq!(lsp_pos_to_char(text, 0, 4, PositionEncoding::Utf16), 3);
        // 2行目先頭は char 5（\n の直後）
        assert_eq!(lsp_pos_to_char(text, 1, 0, PositionEncoding::Utf16), 5);
    }

    #[test]
    fn hover_text_extracts_all_contents_shapes() {
        // ADR-0032: string / MarkedString / MarkupContent / 配列のどれも拾う
        // 文字列
        assert_eq!(hover_text(&json!({ "contents": "plain" })).unwrap(), "plain");
        // MarkupContent オブジェクト
        assert_eq!(
            hover_text(&json!({ "contents": { "kind": "markdown", "value": "doc" } })).unwrap(),
            "doc"
        );
        // 配列（rust-analyzer 形 = 型シグネチャ + doc）
        assert_eq!(
            hover_text(&json!({
                "contents": [
                    { "language": "rust", "value": "fn f() -> i32" },
                    { "kind": "markdown", "value": "docs here" },
                ]
            }))
            .unwrap(),
            "fn f() -> i32\ndocs here"
        );
        // 切り詰め: 上限を超えたら末尾に …
        let long = "x".repeat(MAX_HOVER_CHARS + 50);
        let out = hover_text(&json!({ "contents": long })).unwrap();
        assert!(out.chars().count() <= MAX_HOVER_CHARS + 1, "+1 は … 分");
        assert!(out.ends_with('…'));
        // null（hover なし）と空
        assert!(hover_text(&json!(null)).is_none());
        assert!(hover_text(&json!({ "contents": [] })).is_none());
    }

    #[tokio::test]
    async fn hover_at_line_col_returns_type_and_doc() {
        // ADR-0032 E2E: mock は位置の単語の hover を配列形で返す
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let path = PathBuf::from("/tmp/hover.rs");
        let session = Arc::new(Mutex::new(
            LspSession::new(bin, Path::new("/tmp")).await.expect("initialize"),
        ));
        let text = "fn frobnicate() {}\n";
        session.lock().await.did_open(&path, text).await;
        // 1行目 5文字目（fn の後ろ）→ 単語 frobnicate
        let h = hover_at_line_col(&session, &path, text, 1, 5).await.expect("hover が返る");
        assert!(h.contains("fn frobnicate() -> i32"), "型シグネチャ: {h}");
        assert!(h.contains("mock doc for frobnicate"), "doc: {h}");
        // 行が範囲外 → None
        assert!(hover_at_line_col(&session, &path, text, 99, 1).await.is_none());
    }

    #[tokio::test]
    async fn workspace_symbols_returns_matching_locations() {
        // ADR-0032 E2E: mock はアウトラインから名前にクエリを含む SymbolInformation
        // を返す。パスは daemon 側で file:// を剥ぐため、lsp 層は uri をそのまま返す。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let path = PathBuf::from("/tmp/ws.rs");
        let session = Arc::new(Mutex::new(
            LspSession::new(bin, Path::new("/tmp")).await.expect("initialize"),
        ));
        let text = "fn run() {}\nstruct Thing;\nfn go() {}\n";
        session.lock().await.did_open(&path, text).await;
        let hit = workspace_symbols(&session, "run")
            .await
            .expect("検索が返る");
        assert_eq!(hit.len(), 1, "run に一致: {hit:?}");
        assert_eq!(hit[0].2, "run");
        assert_eq!(hit[0].3, 0, "0-origin 行");
        let all = workspace_symbols(&session, "").await.expect("検索が返る");
        assert_eq!(all.len(), 3, "[空クエリ] は mock で全シンボルを返す（daemon は空を拒否する）");
    }

    #[tokio::test]
    async fn pull_diagnostics_settled_waits_for_stability() {
        // ADR-0032: TODO を持つ文書は診断が安定（非空 ×2）するまで待って返す。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let path = PathBuf::from("/tmp/check.rs");
        let session = Arc::new(Mutex::new(
            LspSession::new(bin, Path::new("/tmp")).await.expect("initialize"),
        ));
        let text = "fn ok() { TODO }\n";
        session.lock().await.did_open(&path, text).await;
        let (diags, settled) = pull_diagnostics_settled(&session, &path, text)
            .await
            .expect("診断が返る");
        assert!(settled, "非空が2回連続で安定 = settled:true");
        assert_eq!(diags.len(), 1, "TODO 診断が1件: {diags:?}");
        assert_eq!(diags[0].message, "mock: TODO found");
    }

    #[tokio::test]
    async fn pull_converts_cjk_utf16_positions() {
        // 欠陥の E2E 検証: --cjk の mock が返す pull 診断の UTF-16 単位の位置が、
        // pull_diagnostics（convert_diagnostics → position.rs 変換）を経て
        // char インデックスとして正しく返る。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let path = PathBuf::from("/tmp/x.rs");
        // "あ😀TODO": UTF-16 では あ=1単位 + 😀=2単位 で TODO は offset 3。
        // バイトでは offset 7 なので、バイトのままだと誤って 7 文字目扱いになる。
        let session = Arc::new(Mutex::new(
            LspSession::new_with_args(bin, Path::new("/tmp"), &["--cjk"])
                .await
                .expect("initialize"),
        ));
        session.lock().await.did_open(&path, "あ😀TODO").await;
        let diags = session
            .lock()
            .await
            .pull_diagnostics(&path, "あ😀TODO")
            .await
            .expect("pull 診断が返る");
        assert_eq!(diags.len(), 1, "TODO 診断が1件: {diags:?}");
        let d = &diags[0];
        // UTF-16 offset 3 → char 2（TODO の 'T'。あ=char0、😀=char1）
        assert_eq!(d.start, 2, "あ(1単位)+😀(2単位) の後: {d:?}");
        assert_eq!(d.end, 6, "TODO は4文字: {d:?}");
        assert_eq!(d.message, "mock: TODO found");
    }

    #[tokio::test]
    async fn open_document_gives_up_when_session_lock_is_held() {
        // MEDIUM-4: セッションロックが他タスクに握られていても open_document は
        // タイムアウトで諦め、Open コマンドをブロックしない。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let session = Arc::new(Mutex::new(
            LspSession::new(bin, Path::new("/tmp")).await.expect("initialize"),
        ));
        let guard = session.lock().await; // ロックを握りっぱなしにする
        let start = std::time::Instant::now();
        open_document(&session, Path::new("/tmp/x.rs"), "fn main() {}").await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "ロック解放を無限に待たない（タイムアウトで諦める）: {:?}",
            start.elapsed()
        );
        drop(guard);
    }

    #[tokio::test]
    async fn sync_gives_up_when_session_lock_is_held() {
        // MEDIUM-4: ロックが握られていても sync はタイムアウトで諦め、編集の
        // 応答をブロックしない（全文同期なので次回の sync が最新テキストを運ぶ）。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let session = Arc::new(Mutex::new(
            LspSession::new(bin, Path::new("/tmp")).await.expect("initialize"),
        ));
        let guard = session.lock().await;
        let start = std::time::Instant::now();
        sync(&session, Path::new("/tmp/x.rs"), "fn main() {}").await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "ロック解放を無限に待たない（タイムアウトで諦める）: {:?}",
            start.elapsed()
        );
        drop(guard);
    }

    // ---- inlay hint ----

    /// テスト用の LSP ヒント項目（LSP 座標）。
    fn hint(
        line: u32,
        character: u32,
        label: &str,
        padding_left: bool,
        padding_right: bool,
    ) -> LspInlayHint {
        LspInlayHint {
            position: mina_lsp::LspPosition { line, character },
            label: InlayLabel::Plain(label.into()),
            padding_left,
            padding_right,
        }
    }

    #[test]
    fn inlay_hint_conversion_utf8_multi_line() {
        // "let x = 5\nfoo(1)": type ヒントは x の直後（char 5）、param ヒントは
        // ( の直後（"let x = 5\n"=10 + "foo("=4 → char 14）。
        let text = "let x = 5\nfoo(1)";
        let items = vec![hint(0, 5, ": i32", false, true), hint(1, 4, "arg: i32", false, false)];
        let hints = convert_inlay_hints(text, PositionEncoding::Utf8, items);
        assert_eq!(hints.len(), 2, "{hints:?}");
        assert_eq!(hints[0].position, 5);
        assert_eq!(hints[0].text, ": i32");
        assert!(hints[0].padding_right);
        assert!(!hints[0].padding_left);
        assert_eq!(hints[1].position, 14);
        assert_eq!(hints[1].text, "arg: i32");
    }

    #[test]
    fn inlay_hint_conversion_utf16_cjk() {
        // "あ😀let x = 5": x の直後は UTF-16 で 8（あ=1 + 😀=2 + "let "=4 + x=1）。
        // バイトなら 12 なので、バイトのまま char に直すと誤って 12 文字目扱いになる。
        let text = "あ😀let x = 5";
        let items = vec![hint(0, 8, ": i32", false, true)];
        let hints = convert_inlay_hints(text, PositionEncoding::Utf16, items);
        assert_eq!(hints.len(), 1, "{hints:?}");
        assert_eq!(hints[0].position, 7, "x の直後の空白（あ=0,😀=1,let=2-5,x=6）: {hints:?}");
        assert_eq!(hints[0].text, ": i32");
    }

    #[test]
    fn inlay_hint_same_position_keeps_order_and_sorts() {
        // 同 position の複数ヒントは応答順を保ち、全体は position 昇順に整う
        // （描画側 #24 の昇順ポインタ走査の前提）。入力は順不同で与える。
        let text = "ab\nfoo(x, y)";
        let items = vec![
            hint(1, 4, "second", false, false), // "foo(" の直後 = char 7
            hint(0, 1, "first", false, false),  // char 1
            hint(1, 4, "third", false, false),  // 同位置 7（応答順を保つ）
        ];
        let hints = convert_inlay_hints(text, PositionEncoding::Utf8, items);
        let pos: Vec<usize> = hints.iter().map(|h| h.position).collect();
        assert_eq!(pos, vec![1, 7, 7], "昇順・同位置は応答順: {hints:?}");
        assert_eq!(hints[1].text, "second");
        assert_eq!(hints[2].text, "third");
    }

    #[test]
    fn inlay_hint_empty_label_is_dropped() {
        let text = "let x = 5";
        let items = vec![hint(0, 5, "", false, false), hint(0, 5, ": i32", false, true)];
        let hints = convert_inlay_hints(text, PositionEncoding::Utf8, items);
        assert_eq!(hints.len(), 1, "空 label は落とす: {hints:?}");
        assert_eq!(hints[0].text, ": i32");
    }

    #[test]
    fn inlay_hint_label_parts_concatenate() {
        let text = "let x = 5";
        let items = vec![LspInlayHint {
            position: mina_lsp::LspPosition { line: 0, character: 5 },
            label: InlayLabel::Parts(vec![
                InlayLabelPart { value: ": ".into() },
                InlayLabelPart { value: "i32".into() },
            ]),
            padding_left: false,
            padding_right: true,
        }];
        let hints = convert_inlay_hints(text, PositionEncoding::Utf8, items);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].text, ": i32", "parts の value が連結される");
    }

    #[tokio::test]
    async fn pull_inlay_hints_converts_cjk_utf16_positions() {
        // 欠陥の E2E 検証: --cjk の mock が返す inlay hint の UTF-16 単位の位置が、
        // pull_inlay_hints（convert_inlay_hints → position.rs 変換）を経て
        // char インデックスとして正しく返る。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let path = PathBuf::from("/tmp/x.rs");
        let text = "あ😀let x = 5";
        let session = Arc::new(Mutex::new(
            LspSession::new_with_args(bin, Path::new("/tmp"), &["--cjk"])
                .await
                .expect("initialize"),
        ));
        session.lock().await.did_open(&path, text).await;
        let hints = session
            .lock()
            .await
            .pull_inlay_hints(&path, text)
            .await
            .expect("pull ヒントが返る");
        assert_eq!(hints.len(), 1, "{hints:?}");
        let h = &hints[0];
        assert_eq!(h.position, 7, "x の直後: {h:?}");
        assert_eq!(h.text, ": i32");
        assert!(
            !h.padding_left && !h.padding_right,
            "type ヒントは padding なし（rust-analyzer の bind_pat と同値）: {h:?}"
        );
    }

    #[tokio::test]
    async fn pull_inlay_hints_type_and_parameter() {
        // mock の固定パターン: `let NAME` に type、`foo(` に parameter ヒント。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let path = PathBuf::from("/tmp/x.rs");
        let text = "let x = 5\nfoo(1)";
        let session = Arc::new(Mutex::new(
            LspSession::new(bin, Path::new("/tmp")).await.expect("initialize"),
        ));
        session.lock().await.did_open(&path, text).await;
        let hints = session
            .lock()
            .await
            .pull_inlay_hints(&path, text)
            .await
            .expect("pull ヒントが返る");
        assert_eq!(hints.len(), 2, "{hints:?}");
        assert_eq!(hints[0].position, 5);
        assert_eq!(hints[0].text, ": i32");
        assert_eq!(hints[1].position, 14, "2行目の ( の直後: {hints:?}");
        assert_eq!(hints[1].text, "arg:");
        assert!(hints[1].padding_right, "param ヒントは右 padding: {hints:?}");
    }

    #[test]
    fn document_symbols_converts_kinds_and_ranges() {
        // LSP の DocumentSymbol 配列（rust-analyzer 相当の形状）→ OutlineSymbol。
        // kind 写像: 12=Function, 6=Method, 23=Struct, 10=Enum, 14=Constant, 13=Variable、
        // 未知（999）は Other。range と selectionRange は両方 char インデックスに。
        let items = json!([
            {
                "name": "Widget",
                "kind": 23,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 9, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 13 } },
                "children": [
                    {
                        "name": "new",
                        "kind": 6,
                        "range": { "start": { "line": 1, "character": 4 }, "end": { "line": 3, "character": 5 } },
                        "selectionRange": { "start": { "line": 1, "character": 7 }, "end": { "line": 1, "character": 10 } }
                    }
                ]
            },
            { "name": "MAX", "kind": 14, "range": { "start": { "line": 9, "character": 0 }, "end": { "line": 9, "character": 5 } },
              "selectionRange": { "start": { "line": 9, "character": 0 }, "end": { "line": 9, "character": 3 } } },
            { "name": "weird", "kind": 999, "range": { "start": { "line": 10, "character": 0 }, "end": { "line": 10, "character": 1 } },
              "selectionRange": { "start": { "line": 10, "character": 0 }, "end": { "line": 10, "character": 1 } } }
        ]);
        let text = "struct Widget {}\n    fn new() {}\n\n\n\n\n\n\nconst MAX: i32 = 1;\nx";
        let out = convert_symbol_list(
            &LineIndex::new(text),
            text,
            PositionEncoding::Utf8,
            items.as_array().unwrap(),
        );
        assert_eq!(out.len(), 3);
        let widget = &out[0];
        assert_eq!(widget.name, "Widget");
        assert_eq!(widget.kind, mina_protocol::SymbolKind::Type);
        assert_eq!(widget.range.anchor, 0);
        assert_eq!(widget.range.head, text.len(), "struct 全体の範囲");
        assert_eq!(widget.selection_range.anchor, 7, "名前トークン Widget の先頭");
        assert_eq!(widget.selection_range.head, 13);
        assert_eq!(widget.children.len(), 1);
        assert_eq!(widget.children[0].name, "new");
        assert_eq!(widget.children[0].kind, mina_protocol::SymbolKind::Method);
        assert_eq!(widget.children[0].selection_range.anchor, 24, "fn 名 new の char 位置（17 + 4sp + fn + space + 0）");
        assert_eq!(widget.children[0].selection_range.head, 27);
        assert_eq!(out[1].name, "MAX");
        assert_eq!(out[1].kind, mina_protocol::SymbolKind::Constant);
        assert_eq!(out[2].kind, mina_protocol::SymbolKind::Other, "未知 kind は Other に潰す");
    }

    #[test]
    fn document_symbols_converts_flat_symbol_information() {
        // 階層広告を無視して SymbolInformation[]（location のみ）を返すサーバ向け
        // フォールバック: 0 件の静かな空にせず、location.range で変換する。
        let items = json!([{
            "name": "RUNTIME",
            "kind": 14,
            "location": {
                "uri": "file:///x.rs",
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 7 } },
            },
        }]);
        let text = "const RUNTIME: u32 = 1;";
        let out = convert_symbol_list(
            &LineIndex::new(text),
            text,
            PositionEncoding::Utf8,
            items.as_array().unwrap(),
        );
        assert_eq!(out.len(), 1, "フラット形状でも変換する");
        assert_eq!(out[0].name, "RUNTIME");
        assert_eq!(out[0].kind, mina_protocol::SymbolKind::Constant);
        assert_eq!(out[0].range.anchor, 0);
        assert_eq!(out[0].range.head, 7);
        assert_eq!(
            out[0].selection_range, out[0].range,
            "名前トークン範囲は代用（= 全体）"
        );
        assert!(out[0].children.is_empty());
    }

    #[test]
    fn document_symbols_converts_utf16_cjk_ranges() {
        // utf-16 列で応答した場合（CJK 混在テキスト）も char インデックスへ一致する。
        // 「あ」は1 char = UTF-16 1 単位なので 2 行目「あStruct」の列 1 = char 6。
        let items = json!([{
            "name": "S",
            "kind": 23,
            "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 7 } },
            "selectionRange": { "start": { "line": 1, "character": 1 }, "end": { "line": 1, "character": 2 } }
        }]);
        let text = "ああ\nあStruct";
        let out = convert_symbol_list(
            &LineIndex::new(text),
            text,
            PositionEncoding::Utf16,
            items.as_array().unwrap(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].range.anchor, 3, "2行目先頭（ああ\n の3 char）");
        assert_eq!(out[0].range.head, 10, "2行目の 7 char 分（文末）");
        assert_eq!(out[0].selection_range.anchor, 4, "2行目1列目 = S");
        assert_eq!(out[0].selection_range.head, 5);
    }

    #[test]
    fn line_col_to_char_idx_clamps_to_line_end() {
        let text = "ab\ncdefg";
        assert_eq!(line_col_to_char_idx(text, 1, 1), 0);
        assert_eq!(line_col_to_char_idx(text, 2, 3), 5, "2行目の列3 → c の次の d");
        // 列が行末を超える → 行末にクランプ（行の終端 = 最後の文字の直後）
        assert_eq!(line_col_to_char_idx(text, 2, 99), 8, "2行目の終端");
        // 行が範囲外 → 最終行の先頭
        assert_eq!(line_col_to_char_idx(text, 99, 1), 3, "最終行（2行目）の先頭");
    }

    #[test]
    fn enclosing_symbol_finds_deepest_containing() {
        use mina_protocol::{OutlineSymbol, Range, SymbolKind};
        let sym = |name: &str, anchor: usize, head: usize, children: Vec<OutlineSymbol>| {
            OutlineSymbol {
                name: name.into(),
                kind: SymbolKind::Function,
                range: Range { anchor, head },
                selection_range: Range { anchor, head },
                children,
            }
        };
        let tree = vec![sym(
            "outer",
            0,
            40,
            vec![sym("mid", 5, 25, vec![sym("inner", 10, 20, vec![])])],
        )];
        // 最深の記号が勝つ
        assert_eq!(enclosing_symbol(&tree, 15).unwrap().name, "inner");
        assert_eq!(enclosing_symbol(&tree, 6).unwrap().name, "mid");
        assert_eq!(enclosing_symbol(&tree, 30).unwrap().name, "outer");
        // 範囲外（head は排他）・ツリー外は None
        assert!(enclosing_symbol(&tree, 40).is_none());
        assert!(enclosing_symbol(&tree, 41).is_none());
        // 子の範囲を踏み外した位置は、その子の兄弟を調べない（最も深い親で止まる）
        let sibling = vec![sym("a", 0, 10, vec![]), sym("b", 12, 20, vec![])];
        assert!(enclosing_symbol(&sibling, 11).is_none());
        assert_eq!(enclosing_symbol(&sibling, 15).unwrap().name, "b");
    }
}
