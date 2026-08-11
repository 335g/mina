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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::lsp;
use crate::lsp::LspSession;

/// daemon が保持する編集状態。
pub struct Daemon {
    pub(crate) editor: Editor,
    /// クライアントから通知されるターミナル表示高さ（カーソル追従スクロール用）。
    pub(crate) viewport_height: usize,
    /// LSP セッション（初回 .rs オープン時に生成。以後は温かいまま保持）。
    pub(crate) lsp: Option<LspSession>,
    /// 現在の文書の診断（LSP の publishDiagnostics を反映）。
    pub(crate) diagnostics: Vec<mina_protocol::Diagnostic>,
}

impl Daemon {
    fn new() -> Self {
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

/// `path` で待ち受ける。前回の異常終了で残った stale socket は除去してから bind する。
pub async fn serve(path: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let daemon = Arc::new(Mutex::new(Daemon::new()));
    accept_loop(listener, daemon).await
}

/// 接続を受け付け、接続ごとにコマンド処理タスクを立てる。
async fn accept_loop(listener: UnixListener, daemon: Arc<Mutex<Daemon>>) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let daemon = daemon.clone();
        tokio::spawn(handle_connection(stream, daemon));
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
        match reader.read_line(&mut line).await {
            Ok(0) => break, // クライアントの切断
            Ok(_) => {}
            Err(_) => break,
        }
        let command: Command = match serde_json::from_str(line.trim()) {
            Ok(c) => c,
            Err(_) => continue, // 壊れた行は無視
        };
        let snapshot = match command {
            // I/O コマンドはロックを握ったままブロックしないよう、接続ハンドラで処理する
            Command::Open { path } => {
                let contents = tokio::fs::read_to_string(&path).await.ok();
                let mut d = daemon.lock().await;
                let status = match &contents {
                    Some(contents) => {
                        d.editor.open_with_path(PathBuf::from(&path), contents);
                        let height = d.viewport_height;
                        d.editor.scroll_to_cursor(height);
                        d.diagnostics.clear();
                        // LSP 対応ファイルならセッションを確保して didOpen する
                        let path_buf = PathBuf::from(&path);
                        if lsp::server_for(&path_buf).is_some() {
                            match lsp::ensure(&mut d, &path_buf).await {
                                Ok(()) => {
                                    lsp::open_document(&mut d, &path_buf, contents).await;
                                    None
                                }
                                Err(msg) => Some(msg),
                            }
                        } else {
                            None
                        }
                    }
                    None => Some(format!("cannot open {path}")),
                };
                lsp::drain_into(&mut d);
                snapshot(&d, status)
            }
            Command::Save => {
                // 保存対象（テキストとパス）を取り出してから、ロック外で書き込む
                let (text, path) = {
                    let d = daemon.lock().await;
                    let text = d.editor.current_document().text().to_string();
                    (text, d.editor.focused_path().map(Path::to_path_buf))
                };
                let write_result = match &path {
                    Some(p) => tokio::fs::write(p, text.as_bytes()).await,
                    None => Err(io::Error::new(io::ErrorKind::NotFound, "no file name")),
                };
                let mut d = daemon.lock().await;
                match write_result {
                    Ok(()) => {
                        d.editor.mark_saved();
                        let shown = path
                            .as_ref()
                            .expect("書き込み成功ならパスはある")
                            .display();
                        snapshot(&d, Some(format!("saved: {shown}")))
                    }
                    Err(e) => snapshot(&d, Some(format!("save failed: {e}"))),
                }
            }
            command => {
                let mut d = daemon.lock().await;
                let is_edit = is_edit(&command);
                apply(&mut d, command);
                if is_edit {
                    // 編集後は LSP へ全文同期し、診断を取り込む
                    lsp::sync(&mut d).await;
                }
                lsp::drain_into(&mut d);
                snapshot(&d, None)
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
            daemon.viewport_height = height;
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
        let (text, p) = {
            let text = d.editor.current_document().text().to_string();
            (text, d.editor.focused_path().map(Path::to_path_buf))
        };
        let p = p.expect("パスがある");
        std::fs::write(&p, text.as_bytes()).unwrap();
        d.editor.mark_saved();
        let s = snapshot(&d, Some("saved".into()));

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
        assert!(!s.dirty);
        let _ = std::fs::remove_file(&path);
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
