//! daemon: 編集状態を所有し、クライアントからのコマンドを処理する常駐プロセス。
//!
//! 状態は mina-view の [`Editor`] がすべて保持する（ADR-0005）。S1 では
//! ファイルを開く・移動・スクロール・モード切替を扱い、編集（S2）と
//! LSP（S3）は後のスライス。socket は `<temp_dir>/mina.sock`（単一ユーザ前提）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mina_core::{extend_selection, move_selection};
use mina_protocol::{Command, GotoTarget, Mode, Range, StateSnapshot};
use mina_view::Editor;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// daemon が保持する編集状態。
struct Daemon {
    editor: Editor,
    /// クライアントから通知されるターミナル表示高さ（カーソル追従スクロール用）。
    viewport_height: usize,
    /// 開いているファイルのパス（S2 の save で使う）。
    path: Option<PathBuf>,
}

impl Daemon {
    fn new() -> Self {
        Self {
            editor: Editor::new(),
            viewport_height: 24,
            path: None,
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
            Ok(0) => return, // クライアントの切断
            Ok(_) => {}
            Err(_) => return,
        }
        let command: Command = match serde_json::from_str(line.trim()) {
            Ok(c) => c,
            Err(_) => continue, // 壊れた行は無視
        };
        // Open はファイル読み込みが先行（ロックを握ったまま I/O ブロックしない）
        let file_contents = match &command {
            Command::Open { path } => Some(tokio::fs::read_to_string(path).await.ok()),
            _ => None,
        };
        let snapshot = {
            let mut daemon = daemon.lock().await;
            apply(&mut daemon, command, file_contents.flatten())
        };
        let mut out = serde_json::to_string(&snapshot).expect("snapshot はシリアライズ可能");
        out.push('\n');
        if write_half.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// コマンドを状態に適用し、新しいスナップショットを返す。
fn apply(daemon: &mut Daemon, command: Command, file_contents: Option<String>) -> StateSnapshot {
    match command {
        Command::Open { path } => match file_contents {
            Some(contents) => {
                daemon.editor.open(contents.as_str().into());
                daemon.path = Some(PathBuf::from(&path));
                daemon
                    .editor
                    .scroll_to_cursor(daemon.viewport_height);
                snapshot(daemon, None)
            }
            None => snapshot(daemon, Some(format!("cannot open {path}"))),
        },
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
            daemon.editor.set_mode(convert_mode(mode));
            snapshot(daemon, None)
        }
        Command::SetViewport { height } => {
            daemon.viewport_height = height;
            snapshot(daemon, None)
        }
        Command::GetState => snapshot(daemon, None),
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
        diagnostics: Vec::new(),
        path: daemon.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
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

fn convert_mode(m: Mode) -> mina_view::Mode {
    match m {
        Mode::Normal => mina_view::Mode::Normal,
        Mode::Insert => mina_view::Mode::Insert,
        Mode::Select => mina_view::Mode::Select,
    }
}

fn convert_mode_back(m: mina_view::Mode) -> Mode {
    match m {
        mina_view::Mode::Normal => Mode::Normal,
        mina_view::Mode::Insert => Mode::Insert,
        mina_view::Mode::Select => Mode::Select,
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
        apply(
            d,
            Command::Open {
                path: "test.txt".into(),
            },
            Some(text.into()),
        )
    }

    #[test]
    fn open_loads_file_into_snapshot() {
        let mut d = daemon();
        let s = open(&mut d, "hello\nworld");
        assert_eq!(s.text, "hello\nworld");
        assert_eq!(s.path.as_deref(), Some("test.txt"));
        assert_eq!(s.status, None);
        assert_eq!(s.selection[0], Range { anchor: 0, head: 0 });
    }

    #[test]
    fn open_failure_reports_status() {
        let mut d = daemon();
        let s = apply(&mut d, Command::Open { path: "missing".into() }, None);
        assert!(s.status.is_some());
        assert!(s.path.is_none());
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
            None,
        );
        assert_eq!(s.selection[0].head, 1);
    }

    #[test]
    fn extend_grows_selection() {
        let mut d = daemon();
        open(&mut d, "hello");
        let s = apply(
            &mut d,
            Command::Extend {
                movement: Movement::Char,
                direction: Direction::Forward,
            },
            None,
        );
        assert_eq!(s.selection[0], Range { anchor: 0, head: 1 });
    }

    #[test]
    fn goto_end_scrolls_viewport() {
        let mut d = daemon();
        let text = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\nu\nv\nw\nx\ny\nz\n";
        open(&mut d, text);
        apply(&mut d, Command::SetViewport { height: 5 }, None);
        let s = apply(
            &mut d,
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
            None,
        );
        assert_eq!(s.selection[0].head, text.chars().count());
        assert!(s.first_line > 0, "カーソルに追従してスクロールする: {}", s.first_line);
        let s2 = apply(&mut d, Command::Scroll { pages: -1 }, None);
        assert!(s2.first_line < s.first_line);
    }

    #[test]
    fn mode_switch_is_recorded() {
        let mut d = daemon();
        let s = apply(&mut d, Command::SetMode { mode: Mode::Insert }, None);
        assert_eq!(s.mode, Mode::Insert);
        let s2 = apply(&mut d, Command::SetMode { mode: Mode::Normal }, None);
        assert_eq!(s2.mode, Mode::Normal);
    }
}
