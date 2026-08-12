//! LSP セッション: rust-analyzer の spawn・initialize・全文同期・診断の取り込み。
//!
//! 診断は daemon のスナップショットに載る（ADR-0006 の「次のスナップショットに
//! 含める」方針）。LSP 通知はコマンド処理時に drain されるため、UI が待ち状態の
//! 間に届いた診断は次のキー入力で表示される。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mina_lsp::{Client, PositionEncoding, PublishDiagnostic};
use mina_protocol::{Diagnostic, Severity};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::daemon::Daemon;

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
#[cfg(not(test))]
const LSP_LOCK_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
const LSP_LOCK_TIMEOUT: Duration = Duration::from_millis(200);

/// LSP セッションの状態（1セッション = 1サーバ。S3 は rust-analyzer のみ）。
pub struct LspSession {
    client: Client,
    encoding: PositionEncoding,
    version: i64,
    current_uri: Option<String>,
    /// initialize 応答で advertise された pull 診断の identifier。
    diagnostic_identifier: Option<String>,
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

/// 開いたファイルを包含する最小の解析単位（WorkspaceRoot）を求める。
///
/// ファイルの親から上方探索し、最寄りの `Cargo.toml` を含むディレクトリを返す
/// （crates/ 配下の個別プロジェクト等、最小グループに閉じた LSP のため）。
/// なければ最寄りの `.git`（worktree 等のファイル形式も含む）、それもなければ
/// ファイルの親ディレクトリにフォールバックする（ADR-0010）。
pub fn workspace_root(path: &Path) -> PathBuf {
    let fallback = path.parent().unwrap_or_else(|| Path::new("."));
    let mut dir = fallback.to_path_buf();
    loop {
        if dir.join("Cargo.toml").exists() || dir.join(".git").exists() {
            return dir;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return fallback.to_path_buf(),
        }
    }
}

impl LspSession {
    /// サーバを spawn し、initialize まで完了させる。
    pub async fn new(command: &str, root: &Path) -> Result<Self, String> {
        Self::new_with_args(command, root, &[]).await
    }

    /// サーバを spawn し、initialize まで完了させる（テスト用: 起動引数付き）。
    pub async fn new_with_args(command: &str, root: &Path, args: &[&str]) -> Result<Self, String> {
        let (mut client, _reader) = Client::spawn(command, args)
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
            encoding,
            version: 0,
            current_uri: None,
            diagnostic_identifier,
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

    /// pull 診断（`textDocument/diagnostic`）を取得し、char インデックスに変換して返す。
    ///
    /// 解析未完了（空）・エラー応答・URI 不一致の場合は `None`（呼び出し側は
    /// 現在の診断を維持する）。rust-analyzer はライブ（in-memory）の診断を
    /// push（publishDiagnostics）ではなく pull で返すため、編集後の診断更新は
    /// この経路で行う（上流フィードバック: クライアントは両方扱うべき）。
    pub async fn pull_diagnostics(&mut self, path: &Path, text: &str) -> Option<Vec<Diagnostic>> {
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

// ---- daemon 統合 ----

/// 必要なら LSP セッションを spawn + initialize する（初回 .rs オープン時）。
///
/// セッションは WorkspaceRoot 毎に持つ（ADR-0010）: 異なるプロジェクトの
/// ファイルを開いても、それぞれの root で spawn されたサーバに解析させる。
/// M1: spawn + initialize（最大10秒）は daemon ロック外で行うため、この関数は
/// `&Mutex<Daemon>` を受け取り、daemon ロックは短時間だけ掴む。
/// M3: 既存セッションが死んでいたら新しいセッションで置き換える。
pub async fn ensure(
    daemon: &Mutex<Daemon>,
    path: &Path,
) -> Result<Arc<Mutex<LspSession>>, String> {
    let root = workspace_root(path);
    // 既存セッション（root に生きていれば）を再利用する
    {
        let d = daemon.lock().await;
        if let Some(session) = d.lsp_sessions.get(&root) {
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
    let session = LspSession::new(command, &root).await?;
    let arc = Arc::new(Mutex::new(session));
    // 保存（短いロック・await なし）。
    let mut d = daemon.lock().await;
    // 同時 ensure レース: 既存が生きていればそちらを優先する
    if let Some(existing) = d.lsp_sessions.get(&root) {
        let alive = match existing.try_lock() {
            Ok(s) => !s.client.is_dead(),
            Err(_) => true, // 同期中: 生きているとみなす
        };
        if alive {
            // ponytail: 同時 ensure は実質1クライアント前提で起きない。起きた場合は
            // 既存を優先し、自分が spawn したセッションは破棄（プロセスは orphan）。
            return Ok(existing.clone());
        }
    }
    // M3/ADR-0009: 既存が死亡済みなら新セッションで置き換える
    // （修正前は無条件に既存を返し、再 spawn が毎回破棄されていた）
    d.lsp_sessions.insert(root, arc.clone());
    Ok(arc)
}

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
/// 診断の実体は pull（[`pull_after_edit`] / [`settle_open_diagnostics`]）で更新する。
/// ここでは MEDIUM-3（サーバ死亡時のクリア）だけを行う。push（publishDiagnostics）
/// は flycheck（cargo check・ディスク基準）由来で、編集内容と食い違う stale な
/// 診断を publish することがあり、pull の結果を上書きしないよう適用しない。
pub fn drain_into(daemon: &mut Daemon) {
    let Some(path) = daemon.editor.focused_path() else {
        daemon.diagnostics.clear();
        return;
    };
    let Some(session) = daemon.lsp_sessions.get(&workspace_root(path)) else {
        // フォーカス文書に LSP セッションがない（.rs 以外）: 前の文書の診断を
        // 残さない（単一セッション時代の current_uri 不一致クリアと同じ意図）。
        daemon.diagnostics.clear();
        return;
    };
    let doc_uri = uri(path);
    let Ok(session) = session.try_lock() else {
        return;
    };
    // MEDIUM-3: サーバが死んだら古い診断を残さない（下線・カウントが文書と
    // 不整合のまま表示され続ける）。再 spawn は次回 .rs Open 時（ADR-0009）。
    if session.client.is_dead() {
        daemon.diagnostics.clear();
    } else if session.current_uri() != Some(doc_uri.as_str()) {
        // フォーカスが LSP 対象外の文書に移ったら診断は残さない
        daemon.diagnostics.clear();
    }
}

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
) -> Option<Vec<Diagnostic>> {
    tokio::time::sleep(PULL_SETTLE).await;
    let Ok(mut session) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
        return None;
    };
    if session.client.is_dead() {
        return None;
    }
    session.pull_diagnostics(path, text).await
}

/// Open 直後の診断追跡タスク: 初期解析が完了するまで pull を繰り返し、
/// 診断を daemon に反映する。
///
/// rust-analyzer の初期解析（crate ロード）は数秒かかり、その間の pull は空を
/// 返す。非空が 2 回連続で返ったら解析完了とみなして終了する（空の連続は
/// 「解析未完」と区別できないため安定判定しない。30 秒間空ならクリーン
/// ファイルとみなして停止）。フォーカスが別文書に移ったら中断する。
/// 上限 120 回 = 60 秒。
pub async fn settle_open_diagnostics(
    daemon: &Mutex<Daemon>,
    session: Arc<Mutex<LspSession>>,
    path: PathBuf,
) {
    let mut prev: Option<usize> = None;
    for i in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // 現在のテキストを掴んでから pull（フォーカス移動・編集の最中は中断）
        let text = {
            let d = daemon.lock().await;
            if d.editor.focused_path().map(Path::to_path_buf).as_deref() != Some(path.as_path()) {
                return;
            }
            d.editor.current_document().text().to_string()
        };
        let pulled = {
            let Ok(mut s) = timeout(LSP_LOCK_TIMEOUT, session.lock()).await else {
                continue;
            };
            if s.client.is_dead() {
                return;
            }
            s.pull_diagnostics(&path, &text).await
        };
        let Some(diags) = pulled else {
            continue; // 解析中のキャンセル等: 次回に持ち越し
        };
        let n = diags.len();
        daemon.lock().await.diagnostics = diags;
        if n > 0 {
            // 非空が返った = 解析完了の確証。2回連続同じ件数なら安定とみなす
            // （誤検出: 解析未完の空（0,0,0...）を安定と誤認しないため、
            //  空の場合は安定判定しない）。
            if prev == Some(n) {
                return;
            }
        } else if i >= 60 {
            // 30秒間空のまま: クリーンファイルとみなして停止（解析が遅くても
            // 次の編集の pull で自己修復する）。
            return;
        }
        prev = Some(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_root_prefers_nearest_manifest_over_outer_git() {
        let dir = std::env::temp_dir().join(format!("mina-lsp-root1-{}", std::process::id()));
        let proj = dir.join("crates").join("foo");
        std::fs::create_dir_all(proj.join("src")).expect("tmp dirs");
        std::fs::write(proj.join("Cargo.toml"), "").expect("manifest");
        std::fs::create_dir_all(dir.join(".git")).expect("git dir");
        let file = proj.join("src").join("main.rs");
        std::fs::write(&file, "").expect("file");
        assert_eq!(workspace_root(&file), proj, "Cargo.toml が .git より優先");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_root_prefers_nearest_git_over_outer_manifest() {
        let dir = std::env::temp_dir().join(format!("mina-lsp-root2-{}", std::process::id()));
        let proj = dir.join("repo");
        std::fs::create_dir_all(proj.join("src")).expect("tmp dirs");
        std::fs::create_dir_all(proj.join(".git")).expect("git dir");
        std::fs::write(dir.join("Cargo.toml"), "").expect("outer manifest");
        let file = proj.join("src").join("main.rs");
        std::fs::write(&file, "").expect("file");
        assert_eq!(workspace_root(&file), proj, "内側の .git が外側の Cargo.toml より優先");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_root_accepts_git_file_worktree() {
        let dir = std::env::temp_dir().join(format!("mina-lsp-root3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        std::fs::write(dir.join(".git"), "gitdir: ../main/.git/worktrees/x").expect("gitfile");
        let file = dir.join("lib.rs");
        std::fs::write(&file, "").expect("file");
        assert_eq!(workspace_root(&file), dir, ".git はファイル形式（worktree）でも目印になる");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_root_falls_back_to_file_parent() {
        let dir = std::env::temp_dir().join(format!("mina-lsp-root4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let file = dir.join("notes.txt");
        std::fs::write(&file, "").expect("file");
        assert_eq!(workspace_root(&file), dir, "目印がなければファイルの親");
        let _ = std::fs::remove_dir_all(&dir);
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
    fn lsp_pos_to_char_utf16_cjk() {
        let text = "aあ😀b\nnext";
        // 1行目: a(1) あ(1) 😀(2) → utf-16 4 は 'b' の位置 = char 3
        assert_eq!(lsp_pos_to_char(text, 0, 4, PositionEncoding::Utf16), 3);
        // 2行目先頭は char 5（\n の直後）
        assert_eq!(lsp_pos_to_char(text, 1, 0, PositionEncoding::Utf16), 5);
    }

    #[tokio::test]
    async fn ensure_replaces_dead_session() {
        // M3/ADR-0009: サーバが死んだら次の .rs Open で再 spawn される。
        // 死亡セッションを返し続けず、新しいセッションに置き換わることを検証する。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let daemon = Arc::new(Mutex::new(Daemon::new()));
        let session = LspSession::new(bin, Path::new("/tmp"))
            .await
            .expect("initialize");
        let dead = Arc::new(Mutex::new(session));
        daemon
            .lock()
            .await
            .lsp_sessions
            .insert(workspace_root(Path::new("/tmp/x.rs")), dead.clone());

        // サーバを殺して is_dead になるまで待つ（reader タスクが EOF を拾う）
        dead.lock().await.client.kill().await;
        let mut is_dead = false;
        for _ in 0..100 {
            if dead.lock().await.client.is_dead() {
                is_dead = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(is_dead, "サーバを殺すと is_dead になる");

        let replaced = ensure(&daemon, Path::new("/tmp/x.rs"))
            .await
            .expect("再 spawn できる");
        assert!(
            !Arc::ptr_eq(&dead, &replaced),
            "新しいセッションで置き換わる"
        );
        assert!(
            !replaced.lock().await.client.is_dead(),
            "返るセッションは生きている"
        );
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
    async fn drain_into_clears_stale_diagnostics_when_server_dies() {
        // MEDIUM-3: サーバが死んだ後も古い診断（下線・カウント）が残り続けない。
        // 死んだ時点でクリアし、再 spawn 後の Open で新しく載る。
        let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/mock-server");
        if !std::path::Path::new(bin).exists() {
            eprintln!("mock-server が未ビルドのためスキップ（cargo test --workspace で実行）");
            return;
        }
        let mut daemon = Daemon::new();
        let path = PathBuf::from("/tmp/x.rs");
        daemon
            .editor
            .open_with_path(path.clone(), "fn main() { TODO }");
        let session = Arc::new(Mutex::new(
            LspSession::new(bin, Path::new("/tmp")).await.expect("initialize"),
        ));
        daemon
            .lsp_sessions
            .insert(workspace_root(&path), session.clone());

        // pull で TODO 診断を取り込んでから殺す（MEDIUM-3 の前提: 診断がある状態）
        session
            .lock()
            .await
            .did_open(&path, "fn main() { TODO }")
            .await;
        let diags = session
            .lock()
            .await
            .pull_diagnostics(&path, "fn main() { TODO }")
            .await
            .expect("pull 診断が返る");
        daemon.diagnostics = diags;
        assert!(!daemon.diagnostics.is_empty(), "診断が入っている");

        // サーバを殺す → drain で古い診断がクリアされる
        session.lock().await.client.kill().await;
        let mut is_dead = false;
        for _ in 0..100 {
            if session.lock().await.client.is_dead() {
                is_dead = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(is_dead, "サーバを殺すと is_dead になる");
        drain_into(&mut daemon);
        assert!(
            daemon.diagnostics.is_empty(),
            "サーバ死亡後の古い診断は残らない"
        );
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
}
