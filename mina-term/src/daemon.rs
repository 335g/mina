//! daemon: 編集状態を所有し、クライアントからのコマンドを処理する常駐プロセス。
//!
//! 状態は mina-view の [`Editor`] がすべて保持する（ADR-0005）。S2 では編集
//! （insert/delete/undo/redo）と保存を扱う。ファイル I/O（Open/Save）だけは
//! ロックを握らないよう接続ハンドラ側で async 実行する。socket は
//! `<temp_dir>/mina.sock`（単一ユーザ前提）。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mina_core::{Transaction, extend_selection, move_selection};
use mina_protocol::{Command, GotoTarget, Range, StateSnapshot};
use mina_view::Editor;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::lsp;
use crate::lsp::LspSession;

/// Open で受け入れる最大ファイルサイズ（ADR-0008）。
///
/// v1 は毎コマンド全文スナップショット + LSP 全文同期のため、これを超える
/// ファイルは実用外。非正規ファイル（/dev/zero 等の無限ストリーム・FIFO・
/// ディレクトリ）の無制限読み込みによる OOM もこの検証で防ぐ（SEC-1）。
const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;

/// NDJSON 1コマンド行の最大バイト数。
///
/// 改行のない無限ストリームで daemon が OOM しないよう、超過する行は
/// 接続を閉じる（SEC-1）。
const MAX_CMD_LINE: usize = 1024 * 1024;

/// SetViewport で受け入れる高さの上限。
///
/// 端末の行数はこれを超えないが、壊れた/悪意あるコマンド（usize::MAX 等）
/// で `first_line + height` の overflow panic を起こさないよう clamp する
/// （scroll_to_cursor の加算は debug ビルドで panic する）。
const MAX_VIEWPORT_HEIGHT: usize = 10_000;

/// daemon が保持する編集状態。
pub struct Daemon {
    pub(crate) editor: Editor,
    /// クライアントから通知されるターミナル表示高さ（カーソル追従スクロール用）。
    pub(crate) viewport_height: usize,
    /// LSP セッション（初回 .rs オープン時に生成。以後は温かいまま保持）。
    /// 独立した Mutex で保護し、LSP の await は daemon ロック外で行う（ADR-0009）。
    pub(crate) lsp: Option<Arc<Mutex<LspSession>>>,
    /// 現在の文書の診断（LSP の publishDiagnostics を反映）。
    pub(crate) diagnostics: Vec<mina_protocol::Diagnostic>,
}

impl Daemon {
    pub(crate) fn new() -> Self {
        Self {
            editor: Editor::new(),
            viewport_height: 24,
            lsp: None,
            diagnostics: Vec::new(),
        }
    }

    /// クライアント切断時の後始末: Insert モードで開いたままの undo グループを
    /// 閉じ、モードを Normal に戻す（ADR-0007）。閉じ忘れると、常駐 daemon の
    /// 履歴がセッションを跨いで編集を同一 undo グループに統合してしまう
    /// （H1: Insert のまま終了した次セッションの入力が undo 1回で消える）。
    fn on_client_disconnect(&mut self) {
        if self.editor.mode() == mina_view::Mode::Insert {
            self.editor.end_group();
            self.editor.set_mode(mina_view::Mode::Normal);
        }
    }
}

/// 常駐デーモンとして起動する（`mina daemon serve`）。
pub async fn run() -> std::io::Result<()> {
    serve(&socket_path()).await
}

/// 同時に処理する接続数の上限（6b: 接続の張り放題による fd/タスク枯渇対策）。
/// 上限を超えた接続はキューに残る（accept されない）。
///
/// ponytail: 実質1クライアント前提。アイドルタイムアウトは入れない — TUI は
/// 読書中もアイドルになるのが正常で、切断されると有害。
const MAX_CONNECTIONS: usize = 4;

/// `path` で待ち受ける。
///
/// M6: 既存の socket を無条件に remove しない。bind が AddrInUse で失敗したら
/// connect プローブで判定し、生きている daemon の socket なら終了（スプリット
/// ブレイン防止）、前回の異常終了の残骸（stale）なら除去して再試行する。
pub async fn serve(path: &Path) -> std::io::Result<()> {
    let daemon = Arc::new(Mutex::new(Daemon::new()));
    match UnixListener::bind(path) {
        Ok(listener) => accept_loop(listener, daemon).await,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another daemon is running",
                ));
            }
            // stale socket: 除去して再試行
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path)?;
            accept_loop(listener, daemon).await
        }
        Err(e) => Err(e),
    }
}

/// 接続を受け付け、接続ごとにコマンド処理タスクを立てる。
async fn accept_loop(listener: UnixListener, daemon: Arc<Mutex<Daemon>>) -> std::io::Result<()> {
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _) = listener.accept().await?;
        let permit = match connections.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return Ok(()), // セマフォが閉じられた（起きない）
        };
        let daemon = daemon.clone();
        tokio::spawn(async move {
            let _permit = permit; // 接続処理中は許可を保持
            handle_connection(stream, daemon).await;
        });
    }
}

/// 1接続分: NDJSON でコマンドを読み、応答スナップショットを返す。
///
/// ponytail: 接続ごとに全状態スナップショットを返す（O(n)/コマンド）。
/// 巨大ファイルで問題になったら差分送信に差し替える。
async fn handle_connection(stream: UnixStream, daemon: Arc<Mutex<Daemon>>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    loop {
        let mut line = String::new();
        // SEC-1: 改行のない無限ストリームで行バッファが無制限に育たないよう、
        // 上限を超える行は接続を閉じる
        let read = (&mut reader)
            .take(MAX_CMD_LINE as u64 + 1)
            .read_line(&mut line)
            .await;
        match read {
            Ok(0) => break, // クライアントの切断
            Ok(_) => {}
            Err(_) => break,
        }
        if line.len() > MAX_CMD_LINE {
            break; // 過大なコマンド行: クライアントが壊れているか悪意がある
        }
        let snapshot = match serde_json::from_str(line.trim()) {
            // I/O コマンドはロックを握ったままブロックしないよう、接続ハンドラで処理する
            Ok(Command::Open { path }) => {
                // SEC-1: 非正規ファイル・過大ファイルを metadata で検証してから読む
                // （ADR-0008）。失敗時は状態を変えず status で報告する。
                let (contents, mut open_status) = read_open_target(&path).await;
                let path_buf = PathBuf::from(&path);
                // M1/ADR-0009: LSP セッションの spawn + initialize（最大10秒）は
                // daemon ロック外で行う。失敗時は status に載せる。
                let session = if contents.is_some() && lsp::server_for(&path_buf).is_some() {
                    match lsp::ensure(&daemon, &path_buf).await {
                        Ok(s) => Some(s),
                        Err(msg) => {
                            open_status = Some(msg);
                            None
                        }
                    }
                } else {
                    None
                };
                // ロック内: 文書状態の変更のみ（await なし）
                let mut d = daemon.lock().await;
                let (text, notify) = match &contents {
                    Some(contents) => {
                        d.editor.open_with_path(path_buf.clone(), contents);
                        let height = d.viewport_height;
                        d.editor.scroll_to_cursor(height);
                        d.diagnostics.clear();
                        (contents.clone(), session.is_some())
                    }
                    None => (String::new(), false),
                };
                drop(d);
                // M1: didOpen 通知は daemon ロック外（lsp mutex のみ・タイムアウト付き）
                if notify {
                    if let Some(session) = &session {
                        lsp::open_document(session, &path_buf, &text).await;
                    }
                }
                let mut d = daemon.lock().await;
                lsp::drain_into(&mut d);
                snapshot(&d, open_status)
            }
            Ok(Command::Save) => {
                // 保存対象（テキスト・パス・文書 ID）を取り出してから、ロック外で書き込む
                let (text, path, doc_id) = {
                    let d = daemon.lock().await;
                    let text = d.editor.current_document().text().to_string();
                    let doc_id = d.editor.focused_doc_id();
                    (text, d.editor.focused_path().map(Path::to_path_buf), doc_id)
                };
                let write_result = match &path {
                    Some(p) => tokio::fs::write(p, text.as_bytes()).await,
                    None => Err(io::Error::new(io::ErrorKind::NotFound, "no file name")),
                };
                let mut d = daemon.lock().await;
                match write_result {
                    Ok(()) => {
                        // 保存した文書そのものの dirty を消す（H3: 書き込み中に他接続が
                        // Open してフォーカスが変わっても、保存対象の文書を正しく扱う）。
                        d.editor.mark_saved_doc(doc_id);
                        let shown = path
                            .as_ref()
                            .expect("書き込み成功ならパスはある")
                            .display();
                        snapshot(&d, Some(format!("saved: {shown}")))
                    }
                    Err(e) => snapshot(&d, Some(format!("save failed: {e}"))),
                }
            }
            Ok(command) => {
                let mut d = daemon.lock().await;
                let is_edit = is_edit(&command);
                apply(&mut d, command);
                // M1/ADR-0009: 同期対象（セッション・パス・テキスト）をロック内で
                // 取り出し、didChange はロック外で await する（サーバ遅延で全
                // クライアントがブロックしない）。
                let sync_target = if is_edit {
                    d.lsp.clone().map(|session| {
                        let path = d.editor.focused_path().map(Path::to_path_buf);
                        let text = d.editor.current_document().text().to_string();
                        (session, path, text)
                    })
                } else {
                    None
                };
                drop(d);
                if let Some((session, Some(path), text)) = sync_target {
                    lsp::sync(&session, &path, &text).await;
                }
                let mut d = daemon.lock().await;
                lsp::drain_into(&mut d);
                snapshot(&d, None)
            }
            Err(_) => {
                // M7: 壊れたコマンド行にも status 付きスナップショットを返す
                // （応答なしだと送信元が永久待ちになる）
                let d = daemon.lock().await;
                snapshot(&d, Some("invalid command".into()))
            }
        };
        let mut out = serde_json::to_string(&snapshot).expect("snapshot はシリアライズ可能");
        out.push('\n');
        if write_half.write_all(out.as_bytes()).await.is_err() {
            break; // クライアントが応答を読めない（切断された）
        }
    }
    // 切断の後始末: Insert モードで開いたままの undo グループを閉じ、モードを
    // Normal に戻す（ADR-0007）。
    daemon.lock().await.on_client_disconnect();
}

/// 編集系コマンドか（LSP 全文同期の対象）。
fn is_edit(command: &Command) -> bool {
    matches!(
        command,
        Command::Insert { .. }
            | Command::DeleteBackward
            | Command::DeleteForward
            | Command::DeleteRange
            | Command::Undo
            | Command::Redo
    )
}

/// Open 対象を検証して読み込む。
///
/// 非正規ファイルは `cannot open`、[`MAX_FILE_SIZE`] 超は `file too large`、
/// 非 UTF-8 等の読み込み失敗は `cannot read` を status で報告する（SEC-1 /
/// ADR-0008）。
///
/// TOCTOU 対策: サイズ検証は open した fd の fstat で行い、その fd から
/// バイト上限付きで読む（metadata と read が別々の path を辿らない）。
///
/// ponytail: UTF-8 のみの I/O（read_to_string）。非 UTF-8 対応は v1 対象外。
/// 検証→open の間にパスが FIFO に差し替えられた場合は open でブロックし得る
/// （単一ユーザ前提。O_NONBLOCK 化は必要になってから）。
async fn read_open_target(path: &str) -> (Option<String>, Option<String>) {
    // 高速パス: 非正規ファイル（FIFO・ディレクトリ等）は open 前に弾く
    match tokio::fs::metadata(path).await {
        Ok(m) if m.len() > MAX_FILE_SIZE => {
            return (None, Some(format!("file too large: {path}")))
        }
        Ok(m) if !m.is_file() => return (None, Some(format!("cannot open {path}"))),
        _ => {}
    }
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return (None, Some(format!("cannot open {path}"))),
    };
    // 同一 fd の fstat で再検証（検証後に巨大化したファイルを丸読みしない）
    match file.metadata().await {
        Ok(m) if m.is_file() && m.len() <= MAX_FILE_SIZE => {
            let mut contents = String::new();
            // バイト上限付きで同一 fd から読む（fstat 後に伸びた分も cap）
            match file
                .take(MAX_FILE_SIZE + 1)
                .read_to_string(&mut contents)
                .await
            {
                Ok(_) if contents.len() as u64 > MAX_FILE_SIZE => {
                    (None, Some(format!("file too large: {path}")))
                }
                Ok(_) => (Some(contents), None),
                // 非 UTF-8 など decode 失敗も status で報告する（修正前は
                // 握り潰して「空文書が開けた」ように見えていた）
                Err(e) => (None, Some(format!("cannot read {path}: {e}"))),
            }
        }
        Ok(m) if m.len() > MAX_FILE_SIZE => (None, Some(format!("file too large: {path}"))),
        Ok(_) => (None, Some(format!("cannot open {path}"))),
        Err(_) => (None, Some(format!("cannot open {path}"))),
    }
}

/// ファイルを読み込んで Editor に開く（clean 状態で始まる）。テスト用。
#[cfg(test)]
fn open_in_editor(daemon: &mut Daemon, path: &str, contents: Option<String>) -> StateSnapshot {
    match contents {
        Some(contents) => {
            daemon.editor.open_with_path(PathBuf::from(path), &contents);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        None => snapshot(daemon, Some(format!("cannot open {path}"))),
    }
}

/// Open/Save 以外のコマンドを状態に適用し、新しいスナップショットを返す。
fn apply(daemon: &mut Daemon, command: Command) -> StateSnapshot {
    match command {
        Command::Insert { text } => {
            let selection = daemon.editor.selection();
            let tx = Transaction::insert(daemon.editor.current_document(), &selection, &text);
            let selection_after = tx.map_selection(&selection, true);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        Command::DeleteBackward => {
            let selection = daemon.editor.selection();
            let tx = mina_core::delete_backward_transaction(
                daemon.editor.current_document(),
                &selection,
            );
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        Command::DeleteForward => {
            let selection = daemon.editor.selection();
            let tx = mina_core::delete_forward_transaction(
                daemon.editor.current_document(),
                &selection,
            );
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        Command::DeleteRange => {
            let selection = daemon.editor.selection();
            let tx = Transaction::delete(daemon.editor.current_document(), &selection);
            let selection_after = tx.map_selection(&selection, false);
            daemon.editor.apply(tx, selection_after);
            // 選択を消したら Select モードを抜ける（vim の d と同様）
            if daemon.editor.mode() == mina_view::Mode::Select {
                daemon.editor.set_mode(mina_view::Mode::Normal);
            }
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        Command::Undo => {
            daemon.editor.undo();
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        Command::Redo => {
            daemon.editor.redo();
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        Command::Move {
            movement,
            direction,
        } => {
            let moved = {
                let doc = daemon.editor.current_document();
                let selection = daemon.editor.selection();
                move_selection(doc, &selection, convert_movement(movement), convert_direction(direction))
            };
            daemon.editor.set_selection(moved);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        Command::Extend {
            movement,
            direction,
        } => {
            let extended = {
                let doc = daemon.editor.current_document();
                let selection = daemon.editor.selection();
                extend_selection(doc, &selection, convert_movement(movement), convert_direction(direction))
            };
            daemon.editor.set_selection(extended);
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        Command::Goto { target } => {
            let pos = match target {
                GotoTarget::DocumentStart => 0,
                GotoTarget::DocumentEnd => daemon.editor.current_document().len_chars(),
            };
            daemon.editor.set_selection(mina_core::Selection::point(pos));
            daemon.editor.scroll_to_cursor(daemon.viewport_height);
            snapshot(daemon, None)
        }
        Command::Scroll { pages } => {
            daemon
                .editor
                .scroll_pages(pages, daemon.viewport_height);
            snapshot(daemon, None)
        }
        Command::SetMode { mode } => {
            let new_mode = convert_mode(mode);
            let current = daemon.editor.mode();
            // Insert モードの入力を1つの undo グループにまとめる（グループは
            // daemon 側で開閉する — 状態は daemon が持つため）。境界は「Insert
            // セッション」: Insert への進入で開き、Insert からの離脱（→Normal・
            // →Select のいずれでも）で閉じる。切断時は接続ハンドラ側で閉じる
            // （ADR-0007）。
            // ponytail: ネストしたグループは考慮しない（v1 にその経路はない）。
            if new_mode == mina_view::Mode::Insert && current != mina_view::Mode::Insert {
                daemon.editor.begin_group();
            } else if new_mode != mina_view::Mode::Insert && current == mina_view::Mode::Insert {
                daemon.editor.end_group();
            }
            daemon.editor.set_mode(new_mode);
            snapshot(daemon, None)
        }
        Command::SetViewport { height } => {
            // 壊れた/悪意ある高さでスクロール計算が overflow しないよう clamp
            daemon.viewport_height = height.min(MAX_VIEWPORT_HEIGHT);
            snapshot(daemon, None)
        }
        Command::GetState => snapshot(daemon, None),
        Command::Open { .. } | Command::Save => {
            unreachable!("I/O コマンドは接続ハンドラで処理される")
        }
    }
}

fn snapshot(daemon: &Daemon, status: Option<String>) -> StateSnapshot {
    let editor = &daemon.editor;
    let selection = editor.selection();
    StateSnapshot {
        text: editor.current_document().text().to_string(),
        selection: selection
            .ranges()
            .iter()
            .map(|r| Range {
                anchor: r.anchor(),
                head: r.head(),
            })
            .collect(),
        primary_index: selection.primary_index(),
        mode: convert_mode_back(editor.mode()),
        first_line: editor.first_line(),
        diagnostics: daemon.diagnostics.clone(),
        path: editor.focused_path().map(|p| p.to_string_lossy().into_owned()),
        dirty: editor.is_dirty(),
        status,
    }
}

fn convert_movement(m: mina_protocol::Movement) -> mina_core::Movement {
    match m {
        mina_protocol::Movement::Char => mina_core::Movement::Char,
        mina_protocol::Movement::Line => mina_core::Movement::Line,
        mina_protocol::Movement::Word => mina_core::Movement::Word,
    }
}

fn convert_direction(d: mina_protocol::Direction) -> mina_core::Direction {
    match d {
        mina_protocol::Direction::Forward => mina_core::Direction::Forward,
        mina_protocol::Direction::Backward => mina_core::Direction::Backward,
    }
}

fn convert_mode(m: mina_protocol::Mode) -> mina_view::Mode {
    match m {
        mina_protocol::Mode::Normal => mina_view::Mode::Normal,
        mina_protocol::Mode::Insert => mina_view::Mode::Insert,
        mina_protocol::Mode::Select => mina_view::Mode::Select,
    }
}

fn convert_mode_back(m: mina_view::Mode) -> mina_protocol::Mode {
    match m {
        mina_view::Mode::Normal => mina_protocol::Mode::Normal,
        mina_view::Mode::Insert => mina_protocol::Mode::Insert,
        mina_view::Mode::Select => mina_protocol::Mode::Select,
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
    use mina_protocol::{Direction, GotoTarget, Mode, Movement};

    fn daemon() -> Daemon {
        Daemon::new()
    }

    fn open(d: &mut Daemon, text: &str) -> StateSnapshot {
        open_in_editor(d, "test.txt", Some(text.into()))
    }

    fn open_path(d: &mut Daemon, path: &str, text: &str) -> StateSnapshot {
        open_in_editor(d, path, Some(text.into()))
    }

    #[test]
    fn open_loads_file_into_snapshot() {
        let mut d = daemon();
        let s = open(&mut d, "hello\nworld");
        assert_eq!(s.text, "hello\nworld");
        assert_eq!(s.path.as_deref(), Some("test.txt"));
        assert!(!s.dirty);
        assert_eq!(s.status, None);
        assert_eq!(s.selection[0], Range { anchor: 0, head: 0 });
    }

    #[test]
    fn open_failure_reports_status() {
        let mut d = daemon();
        let s = open_in_editor(&mut d, "missing", None);
        assert!(s.status.is_some());
        assert!(s.path.is_none());
    }

    #[tokio::test]
    async fn open_rejects_oversized_file() {
        // SEC-1 / ADR-0008: MAX_FILE_SIZE 超のファイルは状態を変えずに拒否する
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mina-sec-oversize-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // スパースファイル: 実データなしでサイズだけ上限を超える
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_FILE_SIZE + 1).unwrap();
        drop(f);

        let path_str = path.to_string_lossy().into_owned();
        let (contents, status) = read_open_target(&path_str).await;
        assert!(contents.is_none());
        let msg = status.expect("拒否メッセージが出る");
        assert!(
            msg.starts_with("file too large"),
            "専用メッセージ: {msg}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn open_rejects_non_regular_file() {
        // SEC-1: ディレクトリ（非正規ファイル）は拒否し、既存のメッセージで報告
        let dir = std::env::temp_dir().join(format!("mina-sec-nonfile-{}", std::process::id()));
        let _ = std::fs::remove_dir(&dir);
        std::fs::create_dir(&dir).unwrap();

        let path_str = dir.to_string_lossy().into_owned();
        let (contents, status) = read_open_target(&path_str).await;
        assert!(contents.is_none());
        let msg = status.expect("拒否メッセージが出る");
        assert!(msg.starts_with("cannot open"), "{msg}");
        let _ = std::fs::remove_dir(&dir);
    }

    #[tokio::test]
    async fn open_reports_non_utf8_as_cannot_read() {
        // 非 UTF-8 ファイルを無言で空文書にせず、status で報告する
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mina-sec-nonutf8-{}.txt", std::process::id()));
        // 無効な UTF-8 バイト列（UTF-16 BOM に使われる 0xFF 0xFE を含む）
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x41]).unwrap();

        let path_str = path.to_string_lossy().into_owned();
        let (contents, status) = read_open_target(&path_str).await;
        assert!(contents.is_none());
        let msg = status.expect("拒否メッセージが出る");
        assert!(msg.starts_with("cannot read"), "{msg}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn oversized_command_line_closes_connection() {
        // SEC-1: 改行のない過大な行を送ると接続が閉じられ、応答が返らない
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("mina-sec-sock-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let sock_serve = sock.clone();
        tokio::spawn(async move {
            let _ = serve(&sock_serve).await;
        });
        // socket が現れるまで待つ
        for _ in 0..100 {
            if UnixStream::connect(&sock).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        use tokio::io::AsyncReadExt;
        let mut stream = UnixStream::connect(&sock).await.expect("接続できる");
        let big = "x".repeat(MAX_CMD_LINE + 1);
        let _ = stream.write_all(big.as_bytes()).await; // 途中で EPIPE になる場合もある
        let mut buf = vec![0u8; 16];
        let n = stream.read(&mut buf).await;
        // 応答は来ず、接続は閉じられる（EOF かエラー）
        assert!(
            matches!(n, Ok(0) | Err(_)),
            "過大行に対して応答しない: {n:?}"
        );
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn set_viewport_huge_height_is_clamped() {
        // usize::MAX の高さでも overflow panic せず、上限に clamp される
        let mut d = daemon();
        open(&mut d, "a\nb\nc\nd\ne");
        apply(&mut d, Command::SetViewport { height: usize::MAX });
        assert_eq!(d.viewport_height, MAX_VIEWPORT_HEIGHT);
        // clamp 後はスクロール計算（first_line + height）が overflow しない
        let _ = apply(
            &mut d,
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        );
        let _ = apply(
            &mut d,
            Command::Move {
                movement: Movement::Char,
                direction: Direction::Forward,
            },
        );
    }

    #[test]
    fn insert_marks_dirty_and_moves_cursor() {
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply(&mut d, Command::Insert { text: "X".into() });
        assert_eq!(s.text, "Xhello");
        assert!(s.dirty, "編集で dirty になる");
        assert_eq!(s.selection[0].head, 1);
    }

    #[test]
    fn delete_backward_and_undo() {
        let mut d = daemon();
        open(&mut d, "hello");
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        apply(&mut d, Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        let s = apply(&mut d, Command::DeleteBackward);
        assert_eq!(s.text, "hllo");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "hello");
        let s = apply(&mut d, Command::Redo);
        assert_eq!(s.text, "hllo");
    }

    #[test]
    fn delete_forward_deletes_next_char() {
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply(&mut d, Command::DeleteForward);
        assert_eq!(s.text, "ello");
    }

    #[test]
    fn insert_mode_typing_undoes_as_one_group() {
        // i → "abc" 入力 → Esc → undo 1回で元に戻る
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        for ch in ["a", "b", "c"] {
            apply(&mut d, Command::Insert { text: ch.into() });
        }
        let s = apply(&mut d, Command::SetMode { mode: Mode::Normal });
        assert_eq!(s.text, "abc");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "");
    }

    #[test]
    fn leaving_insert_for_select_closes_undo_group() {
        // H1 変種: Insert→Select でもグループを閉じる（ADR-0007）。
        // 2回の Insert セッションが別々の undo 単位になる。
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "a".into() });
        apply(&mut d, Command::SetMode { mode: Mode::Select });
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "b".into() });
        let s = apply(&mut d, Command::SetMode { mode: Mode::Normal });
        assert_eq!(s.text, "ab");

        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "a", "undo 1回目は2回目の Insert セッションだけを戻す");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "", "undo 2回目で1回目のセッションも戻る");
    }

    #[test]
    fn disconnect_in_insert_mode_closes_group_and_resets_mode() {
        // H1: Insert のまま接続が切れても、次のクライアントの編集は別グループになる
        let mut d = daemon();
        open(&mut d, "");
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "a".into() });
        d.on_client_disconnect(); // 接続切断（修正前はグループが開いたまま漏れた）
        assert_eq!(d.editor.mode(), mina_view::Mode::Normal, "切断で Normal に戻る");

        // 次のクライアント: 再び Insert で入力しても別グループになる
        apply(&mut d, Command::SetMode { mode: Mode::Insert });
        apply(&mut d, Command::Insert { text: "b".into() });
        let s = apply(&mut d, Command::SetMode { mode: Mode::Normal });
        assert_eq!(s.text, "ab");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "a", "undo は切断後のセッションだけを戻す");
        let s = apply(&mut d, Command::Undo);
        assert_eq!(s.text, "", "切断前のセッションも別グループとして戻せる");
    }

    #[test]
    fn disconnect_in_normal_mode_is_noop() {
        let mut d = daemon();
        open(&mut d, "hello");
        d.on_client_disconnect();
        assert_eq!(d.editor.mode(), mina_view::Mode::Normal);
        assert_eq!(d.editor.current_document().text().to_string(), "hello");
    }

    #[test]
    fn delete_selection_exits_select_mode() {
        let mut d = daemon();
        open(&mut d, "hello");
        apply(&mut d, Command::SetMode { mode: Mode::Select });
        for _ in 0..3 {
            apply(&mut d, Command::Extend {
                movement: Movement::Char,
                direction: Direction::Forward,
            });
        }
        let s = apply(&mut d, Command::DeleteRange);
        assert_eq!(s.text, "lo");
        assert_eq!(s.mode, Mode::Normal, "選択削除後は Normal に戻る");
    }

    #[test]
    fn save_writes_file_and_clears_dirty() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mina-save-test-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_str = path.to_string_lossy().into_owned();

        let mut d = daemon();
        open_path(&mut d, &path_str, "hello");
        apply(
            &mut d,
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        );
        apply(&mut d, Command::Insert { text: " world".into() });

        // Save は接続ハンドラ相当のロジック（テストでは直接実行）
        let (text, p, doc_id) = {
            let text = d.editor.current_document().text().to_string();
            (text, d.editor.focused_path().map(Path::to_path_buf), d.editor.focused_doc_id())
        };
        let p = p.expect("パスがある");
        std::fs::write(&p, text.as_bytes()).unwrap();
        d.editor.mark_saved_doc(doc_id);
        let s = snapshot(&d, Some("saved".into()));

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
        assert!(!s.dirty);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_marks_the_saved_doc_not_the_current_focus() {
        // H3: Save の書き込み中に他接続が Open するとフォーカスが変わる。
        // 保存した文書（保存開始時点の ID）の dirty を消し、新フォーカスの
        // dirty（未保存編集）は残す — 修正前は新フォーカス文書の dirty まで
        // 消えて「保存済み」誤認（データ損失）につながった。
        let dir = std::env::temp_dir();
        let path_a = dir.join(format!("mina-h3-a-{}.txt", std::process::id()));
        let path_b = dir.join(format!("mina-h3-b-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
        let path_a_str = path_a.to_string_lossy().into_owned();
        let path_b_str = path_b.to_string_lossy().into_owned();

        let mut d = daemon();
        // A を開いて編集（dirty）
        open_path(&mut d, &path_a_str, "hello");
        apply(&mut d, Command::Insert { text: " X".into() });
        // Save の保存対象を捕捉（接続ハンドラのロック解放前の処理に相当）
        let doc_id_a = d.editor.focused_doc_id();
        let text_a = d.editor.current_document().text().to_string();
        // 書き込み中に他接続が B を開いて編集（フォーカスが B に移動）
        open_path(&mut d, &path_b_str, "world");
        apply(&mut d, Command::Insert { text: " Y".into() });
        // 書き込み完了 → 保存した文書 A の dirty を消す
        std::fs::write(&path_a, text_a.as_bytes()).unwrap();
        d.editor.mark_saved_doc(doc_id_a);

        // スナップショットはフォーカス（B）の状態: B の未保存編集は dirty のまま
        let s = snapshot(&d, Some("saved".into()));
        assert!(s.dirty, "B の未保存編集が保存済み扱いにならない: {}", s.dirty);
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    #[test]
    fn move_advances_cursor() {
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply(
            &mut d,
            Command::Move {
                movement: Movement::Char,
                direction: Direction::Forward,
            },
        );
        assert_eq!(s.selection[0].head, 1);
    }

    #[test]
    fn goto_end_scrolls_viewport() {
        let mut d = daemon();
        let text = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\nu\nv\nw\nx\ny\nz\n";
        open(&mut d, text);
        apply(&mut d, Command::SetViewport { height: 5 }, );
        let s = apply(
            &mut d,
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        );
        assert_eq!(s.selection[0].head, text.chars().count());
        assert!(s.first_line > 0, "カーソルに追従してスクロールする: {}", s.first_line);
    }
}
