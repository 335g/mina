//! agent 用のヘッドレス CLI: daemon に接続してコマンドを実行し、スナップショットを表示する。
//!
//! - `mina session get` — 現在の状態を取得する（JSON で出力）
//! - `mina session exec <JSON>` — `Command` を1つ実行する（JSON は wire の [`Command`] そのまま）
//! - `mina session edit <JSON>` — [`DocumentEdit`]（位置指定編集）を1つ実行する
//! - `mina session wait <generation>` — 世代が `<generation>` を超えるまでブロックして状態を返す
//! - `mina session hints <path>` — 任意パスの inlay hint を全文テキストなしで取得する（ADR-0020）
//!
//! 例: `mina session exec '{"Insert": {"text": "hello"}}'`
//! 例: `mina session edit '{"start": 0, "end": 0, "text": "hi", "checksum": <snapshot.checksum>}'`
//! 例: `mina session wait 42`
//! 例: `mina session hints src/main.rs`
//!
//! daemon が動いていなければ自動起動される（TUI と同じ挙動）。終了コード:
//! 0 = 成功（適用・no-op 含む）、1 = トランスポート/JSON エラー、
//! 2 = `edit` が daemon に拒否された（checksum 不一致・範囲外。
//! 再読み込みして再試行可能）。拒否理由の詳細はスナップショットの
//! `status` フィールドに載る。

use std::io;
use std::path::PathBuf;

use clap::Subcommand;
use mina_protocol::{ClientKind, Command, DocumentEdit, InlayHint, StateSnapshot};
use tokio::io::BufReader;
use tokio::net::UnixStream;

use crate::client;

/// `mina session` のサブコマンド。引数・型は clap が検証する。
#[derive(Subcommand)]
pub enum SessionCmd {
    /// Fetch the current state (printed as JSON)
    Get,
    /// Run one `Command` (JSON is the wire [`Command`] as-is)
    Exec {
        /// Command JSON
        json: String,
    },
    /// Run one [`DocumentEdit`] (position-addressed edit)
    Edit {
        /// DocumentEdit JSON
        json: String,
    },
    /// Block until the generation exceeds `<generation>` and return the state
    Wait {
        /// Generation
        generation: u64,
    },
    /// Fetch inlay hints for any path without full text (ADR-0020)
    Hints {
        /// Path
        path: PathBuf,
    },
}

/// `mina session <subcommand>` を処理する。
pub async fn run(cmd: SessionCmd) -> io::Result<()> {
    match cmd {
        SessionCmd::Get => {
            let snapshot = execute(&Command::GetState).await?;
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        }
        SessionCmd::Exec { json } => {
            let command: Command = serde_json::from_str(&json)
                .map_err(|e| invalid(format!("コマンド JSON を解釈できません: {e}")))?;
            let snapshot = execute(&command).await?;
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        }
        SessionCmd::Edit { json } => {
            let edit: DocumentEdit = serde_json::from_str(&json)
                .map_err(|e| invalid(format!("DocumentEdit JSON を解釈できません: {e}")))?;
            let snapshot = execute_edit(&edit).await?;
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
            // #11: daemon の拒否（status 付き応答）を exit code 2 で明示する。
            // エージェントは $? だけで失敗を検知し、再読み込み→再試行できる。
            let code = edit_exit_code(&snapshot);
            if code != 0 {
                std::process::exit(code);
            }
        }
        SessionCmd::Wait { generation } => {
            let snapshot = execute(&Command::WaitFor { generation }).await?;
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        }
        SessionCmd::Hints { path } => {
            let hints = execute_hints(&path.to_string_lossy()).await?;
            // 応答は (path, generation, hints)。hints だけを JSON で出力する
            // （エージェントがそのまま読める形）。
            println!("{}", serde_json::to_string_pretty(&hints.2)?);
        }
    }
    Ok(())
}

/// daemon に接続し、コマンドを実行してスナップショットを受け取る。
async fn execute(command: &Command) -> io::Result<StateSnapshot> {
    let path = crate::daemon::socket_path();
    client::ensure_daemon(&path).await?;
    let stream = UnixStream::connect(&path).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    // ADR-0012: 接続直後に Hello（ヘッドレス宣言）を送る
    client::send_hello(&mut write_half, ClientKind::Headless).await?;
    // CRITICAL C2: Open のパスは agent の cwd 基準で絶対化してから送る（TUI と
    // 同一の契約）。そのまま送ると daemon の spawn cwd 基準で解決され、意図しない
    // ファイルを開く恐れがある。
    let command = absolutize_paths(command.clone());
    client::request(&mut write_half, &mut reader, &command).await
}

/// パスを含むコマンドのパスを絶対化する（Open / GetInlayHints。他はそのまま）。
///
/// [`crate::client::absolutize`] を共通化して使う — daemon 側の cwd は spawn 時に
/// 固定されるため、解決は送信側（agent の cwd）で行う。
fn absolutize_paths(command: Command) -> Command {
    match command {
        Command::Open { path } => Command::Open {
            path: client::absolutize(&path),
        },
        Command::GetInlayHints { path } => Command::GetInlayHints {
            path: client::absolutize(&path),
        },
        other => other,
    }
}

/// daemon に接続し、位置指定編集（DocumentEdit）を実行してスナップショットを受け取る。
async fn execute_edit(edit: &DocumentEdit) -> io::Result<StateSnapshot> {
    let path = crate::daemon::socket_path();
    client::ensure_daemon(&path).await?;
    let stream = UnixStream::connect(&path).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    // ADR-0012: 接続直後に Hello（ヘッドレス宣言）を送る
    client::send_hello(&mut write_half, ClientKind::Headless).await?;
    client::request(&mut write_half, &mut reader, edit).await
}

/// daemon に接続し、任意パスの inlay hint を取得する（ADR-0020）。
/// 応答はスナップショットでなく Hints（path, generation, hints）なので専用経路。
async fn execute_hints(path: &str) -> io::Result<(String, u64, Vec<InlayHint>)> {
    let sock = crate::daemon::socket_path();
    client::ensure_daemon(&sock).await?;
    let stream = UnixStream::connect(&sock).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    // ADR-0012: 接続直後に Hello（ヘッドレス宣言）を送る
    client::send_hello(&mut write_half, ClientKind::Headless).await?;
    // CRITICAL C2: パスは agent の cwd 基準で絶対化してから送る（Open と同一の契約）
    client::request_hints(&mut write_half, &mut reader, &client::absolutize(path)).await
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

/// `session edit` の終了コード: daemon が拒否（status 付き応答）なら 2、
/// それ以外（適用・no-op）は 0。
fn edit_exit_code(snapshot: &StateSnapshot) -> i32 {
    if snapshot.status.is_some() {
        2
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_edit_status_yields_exit_code_2() {
        // #11: 拒否（status 付き応答）は exit 2、成功・no-op（status なし）は 0。
        let rejected = StateSnapshot {
            status: Some("document changed since read".into()),
            ..Default::default()
        };
        assert_eq!(edit_exit_code(&rejected), 2, "拒否は再試行可能な失敗");
        assert_eq!(edit_exit_code(&StateSnapshot::default()), 0);
    }

    #[test]
    fn bad_json_is_rejected() {
        // サブコマンドの JSON 解釈（clap は引数構造のみ検証し、JSON はここで検証する）
        let err = serde_json::from_str::<Command>("not json")
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("expected"));
    }

    #[test]
    fn absolutize_paths_absolutizes_path_commands_only() {
        // CRITICAL C2: パスを含むコマンド（Open / GetInlayHints）だけが agent の
        // cwd 基準で絶対化される（daemon の spawn cwd で解決されないように）。
        // 他コマンドは素通し。
        let cmd = absolutize_paths(Command::Open { path: "rel/file.rs".into() });
        match cmd {
            Command::Open { path } => {
                assert!(
                    std::path::Path::new(&path).is_absolute(),
                    "絶対化される: {path}"
                );
                assert!(path.ends_with("rel/file.rs"));
            }
            other => panic!("Open 以外が返った: {other:?}"),
        }
        let cmd = absolutize_paths(Command::GetInlayHints {
            path: "rel/lib.rs".into(),
        });
        match cmd {
            Command::GetInlayHints { path } => {
                assert!(std::path::Path::new(&path).is_absolute(), "絶対化される: {path}");
                assert!(path.ends_with("rel/lib.rs"));
            }
            other => panic!("GetInlayHints 以外が返った: {other:?}"),
        }
        assert_eq!(absolutize_paths(Command::GetState), Command::GetState);
        assert_eq!(
            absolutize_paths(Command::Insert { text: "x".into() }),
            Command::Insert { text: "x".into() }
        );
    }
}
