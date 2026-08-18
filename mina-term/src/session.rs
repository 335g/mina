//! agent 用のヘッドレス CLI: daemon に接続してコマンドを実行し、スナップショットを表示する。
//!
//! - `mina session get` — 現在の状態を取得する（JSON で出力）
//! - `mina session info` — daemon のビルド世代・累積メトリクスを取得する（issue #27）
//! - `mina session apply <path> <old> <new>` — 1コマンドで「Open→検証置換→Save」
//!   （issue #29 F1。minae ヘルパーの製品化。`--whole` / `--whole-stdin` /
//!   `--old-file` / `--new-file` で argv 制限やシェル引用を回避できる）
//! - `mina session exec <JSON>` — `Command` を1つ実行する（JSON は wire の [`Command`] そのまま）
//! - `mina session edit <JSON>` — [`DocumentEdit`]（位置指定編集）を1つ実行する
//! - `mina session wait <generation>` — 世代が `<generation>` を超えるまでブロックして状態を返す
//! - `mina session hints <path>` — 任意パスの inlay hint を全文テキストなしで取得する（ADR-0020）
//! - `mina session peek <path> <line>:<col>` — 指定位置（1-origin）の定義を全文なしで取得する（ADR-0025）
//!
//! 例: `mina session exec '{"Insert": {"text": "hello"}}'`
//! 例: `mina session edit '{"start": 0, "end": 0, "text": "hi", "checksum": <snapshot.checksum>}'`
//! 例: `mina session apply src/lib.rs "let x = 1" "let x = 2"`
//! 例: `mina session apply src/lib.rs --whole-stdin < new_content.rs`
//! 例: `mina session wait 42`
//! 例: `mina session hints src/main.rs`
//! 例: `mina session peek src/main.rs 12:5`
//!
//! daemon が動いていなければ自動起動される（TUI と同じ挙動）。終了コード:
//! 0 = 成功（適用・no-op 含む）、1 = トランスポート/JSON エラー、
//! 2 = `edit` が daemon に拒否された（checksum 不一致・範囲外。
//! 再読み込みして再試行可能）。拒否理由の詳細はスナップショットの
//! `status` フィールドに載る。

use std::io::Read;
use std::io;
use std::path::PathBuf;

use clap::Subcommand;
use mina_protocol::{ClientKind, Command, DocumentEdit, InlayHint, StateSnapshot};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::client;

/// `mina session` のサブコマンド。引数・型は clap が検証する。
#[derive(Subcommand)]
pub enum SessionCmd {
    /// Fetch the current state (printed as JSON)
    Get,
    /// Fetch the daemon build generation and metrics (printed as JSON, issue #27)
    Info,
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
    /// One-command verified edit: Open → expected_text-checked replace → Save
    /// (issue #29 F1 — the minae helper flow, productized). Substring replace of
    /// the first occurrence by default; `--whole` / `--whole-stdin` replace the
    /// whole file; `--old-file` / `--new-file` read texts from files to avoid
    /// shell quoting and argv limits.
    Apply {
        /// File to edit (relative to the agent's cwd, like `Open`)
        path: PathBuf,
        /// Text to find (first occurrence). Omit with --whole / --whole-stdin / --old-file
        old: Option<String>,
        /// Replacement text. Omit with --new-file / --whole-stdin
        new: Option<String>,
        /// Replace the whole file with this text (no `old` lookup)
        #[arg(long)]
        whole: Option<String>,
        /// Replace the whole file with the text read from stdin (no argv limit)
        #[arg(long)]
        whole_stdin: bool,
        /// Read the sought text from a file (avoids shell quoting)
        #[arg(long)]
        old_file: Option<PathBuf>,
        /// Read the replacement from a file (avoids shell quoting)
        #[arg(long)]
        new_file: Option<PathBuf>,
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
    /// Peek the definition at `<line>:<col>` (1-origin) without fetching full text (ADR-0025)
    Peek {
        /// Path
        path: PathBuf,
        /// Position as `line:col` (1-origin, col is a char count)
        pos: String,
    },
}

/// `mina session <subcommand>` を処理する。
pub async fn run(cmd: SessionCmd) -> io::Result<()> {
    match cmd {
        SessionCmd::Get => {
            let snapshot = execute(&Command::GetState).await?;
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
        }
        SessionCmd::Info => {
            // #27: ワンショット CLI にも GetServerInfo 経路を用意する。daemon の
            // ビルド世代（Git hash）と累積メトリクスを JSON で出力。エージェントは
            // これを接続冒頭で叩き、自分の知る機能レベルと食い違えば再ビルドを
            // 促して silent ignore 事故（feedback 報告#1）を未然に検知できる。
            let info = execute_server_info().await?;
            println!("{}", serde_json::to_string_pretty(&info)?);
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
        SessionCmd::Apply {
            path,
            old,
            new,
            whole,
            whole_stdin,
            old_file,
            new_file,
        } => {
            apply(&path, old, new, whole, whole_stdin, old_file, new_file).await?;
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
        SessionCmd::Peek { path, pos } => {
            let (line, col) = parse_position(&pos)?;
            let peek = execute_peek(&path.to_string_lossy(), line, col).await?;
            // 応答は (path, line, text)。全文スナップショットではなく定義だけを
            // JSON で出力する（トークン削減 — ADR-0025）。
            println!("{}", serde_json::to_string_pretty(&peek)?);
        }
    }
    Ok(())
}

/// `<line>:<col>`（1-origin）を解釈する。不正ならエラー。
fn parse_position(pos: &str) -> io::Result<(u32, u32)> {
    let Some((line, col)) = pos.split_once(':') else {
        return Err(invalid(format!("位置は <行>:<列> 形式で指定してください: {pos:?}")));
    };
    let line = line
        .parse::<u32>()
        .map_err(|_| invalid(format!("行番号が不正です: {line:?}")))?;
    let col = col
        .parse::<u32>()
        .map_err(|_| invalid(format!("列番号が不正です: {col:?}")))?;
    if line == 0 || col == 0 {
        return Err(invalid("行・列は 1 始まりです（0 は指定できません）"));
    }
    Ok((line, col))
}

/// daemon に接続し、指定位置の定義を軽量応答（[`ServerMessage::Peek`]）で受け取る。
async fn execute_peek(path: &str, line: u32, col: u32) -> io::Result<mina_protocol::Peek> {
    let socket = crate::daemon::socket_path();
    client::ensure_daemon(&socket).await?;
    let stream = UnixStream::connect(&socket).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    // ADR-0012: 接続直後に Hello（ヘッドレス宣言）を送る
    client::send_hello(&mut write_half, ClientKind::Headless).await?;
    // CRITICAL C2: パスは agent の cwd 基準で絶対化してから送る
    client::request_peek(&mut write_half, &mut reader, &client::absolutize(path), line, col).await
}

/// daemon に接続し、`GetServerInfo` の応答（[`ServerMessage::ServerInfo`]）を取得する
/// （issue #27/D1）。ビルド世代（Git hash）と起動からの累積メトリクスを返す。
/// スナップショットを運ばず世代・push・イベントを進めない軽量応答なので専用経路。
async fn execute_server_info() -> io::Result<serde_json::Value> {
    let socket = crate::daemon::socket_path();
    client::ensure_daemon(&socket).await?;
    let stream = UnixStream::connect(&socket).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    // ADR-0012: 接続直後に Hello（ヘッドレス宣言）を送る
    client::send_hello(&mut write_half, ClientKind::Headless).await?;
    let mut line = serde_json::to_string(&Command::GetServerInfo).expect("コマンドはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<mina_protocol::ServerMessage>(&response) {
        Ok(mina_protocol::ServerMessage::ServerInfo {
            generation,
            daemon_build_ts,
            metrics,
        }) => Ok(serde_json::json!({ "generation": generation, "daemon_build_ts": daemon_build_ts, "metrics": metrics })),
        Ok(mina_protocol::ServerMessage::Response { .. })
        | Ok(mina_protocol::ServerMessage::Push { .. }) => {
            Err(invalid("GetServerInfo にスナップショット応答が返った（旧 daemon: 再ビルドしてください）"))
        }
        Ok(mina_protocol::ServerMessage::Hints { .. })
        | Ok(mina_protocol::ServerMessage::Peek { .. }) => {
            Err(invalid("GetServerInfo に想定外の軽量応答が返った"))
        }
        Err(e) => Err(invalid(format!("不正な応答: {e}"))),
    }
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

/// `session apply` の本体（issue #29 F1 — minae ヘルパーの製品化）。
///
/// 1回の実行で「Open → 探索(old) → checksum+expected_text 検証付き replace →
/// Save」を完了させる。心臓部の検証は daemon 側の apply_edit が行うため、
/// この CLI は位置計算（char インデックス）だけを担う — エージェント向け
/// 「1本の道」のプロトコル層としての役割（A1 の未達部分の現実解）。
///
/// - old あり: 先頭から最初の出現位置を置換（見つからなければ exit 2）
/// - `--whole` / `--whole-stdin`: 全文置換（old 不要）
/// - ファイルが存在しない場合: 空文書として扱い、Save で新規作成する
///   （report 3-1 の touch→open 2段階ハックの解消）
///
/// 終了コード: 0 = 成功、1 = トランスポート/引数エラー、2 = 探索失敗・
/// daemon 拒否（checksum/expected_text 不一致）・Save 失敗（再試行可能）。
async fn apply(
    path: &PathBuf,
    old: Option<String>,
    new: Option<String>,
    whole: Option<String>,
    whole_stdin: bool,
    old_file: Option<PathBuf>,
    new_file: Option<PathBuf>,
) -> io::Result<()> {
    let (old_text, new_text) = resolve_apply_args(old, new, whole, whole_stdin, old_file, new_file)?;

    let socket = crate::daemon::socket_path();
    client::ensure_daemon(&socket).await?;
    let stream = UnixStream::connect(&socket).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    // ADR-0012: 接続直後に Hello（ヘッドレス宣言）を送る
    client::send_hello(&mut write_half, ClientKind::Headless).await?;

    let abs = client::absolutize(&path.to_string_lossy());
    let mut snapshot = client::request(&mut write_half, &mut reader, &Command::Open { path: abs.clone() }).await?;
    // 未存在パス: 空ファイルを作成してから再 Open し、Save で新規作成する
    // （#28/#29: 新規ファイルを直接アドレスする現実解。report 3-1 の touch→open
    // 2段階ハックをコマンド側に内包。自動オープンの範囲外判断（#28）には触れない）。
    // ponytail: 編集失敗時も空ファイルが残る（touch と同じ挙動）。気になるなら
    // daemon 側の Open で未存在パスのドキュメント生成を検討する。
    if snapshot.status.is_some() && std::fs::metadata(&abs).is_err() {
        std::fs::write(&abs, "")
            .map_err(|e| invalid(format!("新規ファイル作成失敗: {abs}: {e}")))?;
        snapshot = client::request(&mut write_half, &mut reader, &Command::Open { path: abs.clone() }).await?;
    }
    if let Some(status) = &snapshot.status {
        return Err(invalid(format!("Open 失敗: {status}")));
    }
    let text = snapshot.text.clone();

    // char インデックスで位置を計算する（DocumentEdit は char 単位）:
    // byte インデックスを渡すとマルチバイト文書でずれる。
    let (start, end, expected) = match &old_text {
        Some(old) => match find_range(&text, old) {
            Some((start, end)) => (start, end, Some(old.clone())),
            // 探索失敗は再試行可能な失敗として exit 2（`session edit` の拒否と同格）
            None => {
                eprintln!("NOT FOUND: {:?}", short(old));
                std::process::exit(2);
            }
        },
        None => (0, text.chars().count(), None),
    };

    let edit = DocumentEdit {
        start,
        end,
        text: new_text.clone(),
        checksum: snapshot.checksum,
        expected_text: expected,
    };
    let snapshot = client::request(&mut write_half, &mut reader, &edit).await?;
    if let Some(status) = &snapshot.status {
        // 拒否（checksum/expected_text 不一致）: 再試行可能な失敗として exit 2
        eprintln!("EDIT REJECTED: {status}");
        std::process::exit(2);
    }

    let snapshot = client::request(&mut write_half, &mut reader, &Command::Save).await?;
    let saved = snapshot
        .status
        .as_deref()
        .is_some_and(|s| s.starts_with("saved"));
    if !saved {
        eprintln!("SAVE FAILED: {:?}", snapshot.status);
        std::process::exit(2);
    }
    match &old_text {
        Some(_) => println!("applied: {abs} (chars {start}..{end})"),
        None => println!("applied: {abs} (whole file, {} chars)", text.chars().count()),
    }
    Ok(())
}

/// `text` 中の `old` の最初の出現位置を char インデックスで返す（[`DocumentEdit`]
/// は char 単位のため、byte ではなく char で数える）。
fn find_range(text: &str, old: &str) -> Option<(usize, usize)> {
    let byte_idx = text.find(old)?;
    let start = text[..byte_idx].chars().count();
    Some((start, start + old.chars().count()))
}

/// `session apply` の入力モードを解決する。互いに排他な指定はエラーにする。
/// 戻り値は (探索テキスト, 置換テキスト)。探索テキストが None なら全文置換。
fn resolve_apply_args(
    old: Option<String>,
    new: Option<String>,
    whole: Option<String>,
    whole_stdin: bool,
    old_file: Option<PathBuf>,
    new_file: Option<PathBuf>,
) -> io::Result<(Option<String>, String)> {
    if whole_stdin {
        if old.is_some() || new.is_some() || whole.is_some() || old_file.is_some() || new_file.is_some() {
            return Err(invalid("--whole-stdin は他の入力指定と併用できません"));
        }
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        return Ok((None, buf));
    }
    if let Some(w) = whole {
        if old.is_some() || new.is_some() || old_file.is_some() || new_file.is_some() {
            return Err(invalid("--whole は他の入力指定と併用できません"));
        }
        return Ok((None, w));
    }
    let old = match old_file {
        Some(f) => {
            if old.is_some() {
                return Err(invalid("old と --old-file は併用できません"));
            }
            std::fs::read_to_string(&f).map_err(|e| invalid(format!("--old-file {f:?}: {e}")))?
        }
        None => old.ok_or_else(|| invalid("置換対象テキストが必要です (old, または --whole / --whole-stdin / --old-file)"))?,
    };
    let new = match new_file {
        Some(f) => {
            if new.is_some() {
                return Err(invalid("new と --new-file は併用できません"));
            }
            std::fs::read_to_string(&f).map_err(|e| invalid(format!("--new-file {f:?}: {e}")))?
        }
        None => new.ok_or_else(|| invalid("置換後のテキストが必要です (new, または --new-file)"))?,
    };
    Ok((Some(old), new))
}

/// エラーメッセージ用に文字列を先頭40文字に短縮する。
fn short(s: &str) -> String {
    let mut out: String = s.chars().take(40).collect();
    if s.chars().count() > 40 {
        out.push('…');
    }
    out
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
    fn parse_position_parses_line_col() {
        // ADR-0025: `<行>:<列>`（1-origin）。0 と不正形式は拒否。
        assert_eq!(parse_position("12:5").unwrap(), (12, 5));
        assert_eq!(parse_position("1:1").unwrap(), (1, 1));
        assert!(parse_position("12").is_err(), "コロン無しは拒否");
        assert!(parse_position("0:5").is_err(), "行 0 は拒否");
        assert!(parse_position("12:0").is_err(), "列 0 は拒否");
        assert!(parse_position("a:b").is_err(), "数値以外は拒否");
    }

    #[test]
    fn find_range_counts_chars_not_bytes() {
        // DocumentEdit の位置は char 単位。マルチバイト文字の後方での
        // 出現は byte インデックスと一致しないため、char で数える必要がある。
        assert_eq!(find_range("abc", "b"), Some((1, 2)));
        assert_eq!(find_range("あいうえお", "うえ"), Some((2, 4)));
        assert_eq!(find_range("あxい", "x"), Some((1, 2)));
        assert_eq!(find_range("a→bc", "bc"), Some((2, 4)));
        assert_eq!(find_range("a→b→c", "→"), Some((1, 2)), "最初の出現");
        assert_eq!(find_range("abc", "z"), None);
        assert_eq!(find_range("", "x"), None);
        // 前後の文字数（chars().count）と整合する
        let text = "a→bc";
        let (s, e) = find_range(text, "bc").unwrap();
        assert_eq!(&text.chars().skip(s).take(e - s).collect::<String>(), "bc");
    }

    #[test]
    fn resolve_apply_args_modes_are_exclusive() {
        // 既定: old/new 位置指定
        let (old, new) = resolve_apply_args(
            Some("x".into()),
            Some("y".into()),
            None,
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(old.as_deref(), Some("x"));
        assert_eq!(new, "y");

        // --whole: old 不要・全文置換
        let (old, new) = resolve_apply_args(None, None, Some("full".into()), false, None, None).unwrap();
        assert_eq!(old, None, "--whole は全文置換");
        assert_eq!(new, "full");

        // --whole と位置指定の併用は拒否
        assert!(resolve_apply_args(Some("x".into()), None, Some("f".into()), false, None, None).is_err());
        assert!(resolve_apply_args(None, None, None, true, Some(PathBuf::from("f")), None).is_err());

        // old なしはエラー
        assert!(resolve_apply_args(None, Some("y".into()), None, false, None, None).is_err());
        // new なしもエラー
        assert!(resolve_apply_args(Some("x".into()), None, None, false, None, None).is_err());

        // --old-file は一時ファイルを書き、中身を読む
        let dir = std::env::temp_dir();
        let oldf = dir.join(format!("mina-apply-old-{}.txt", std::process::id()));
        let newf = dir.join(format!("mina-apply-new-{}.txt", std::process::id()));
        std::fs::write(&oldf, "old text").unwrap();
        std::fs::write(&newf, "new text").unwrap();
        let (old, new) = resolve_apply_args(
            None,
            None,
            None,
            false,
            Some(oldf.clone()),
            Some(newf.clone()),
        )
        .unwrap();
        assert_eq!(old.as_deref(), Some("old text"));
        assert_eq!(new, "new text");
        let _ = std::fs::remove_file(&oldf);
        let _ = std::fs::remove_file(&newf);
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
