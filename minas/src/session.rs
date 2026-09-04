//! agent 用のヘッドレス CLI: daemon に接続してコマンドを実行し、スナップショットを表示する。
//!
//! - `minas get` — 現在の状態を取得する（JSON で出力）
//! - `minas info` — daemon のビルド世代・累積メトリクスを取得する（issue #27）
//! - `minas apply <path> <old> <new>` — 1コマンドで「Open→検証置換→Save」
//!   （issue #29 F1。minae ヘルパーの製品化。`--whole` / `--whole-stdin` /
//!   `--old-file` / `--new-file` で argv 制限やシェル引用を回避できる。
//!   複数編集は `--hunks-stdin`（JSON 配列を stdin から、1 接続で Save は最後に一度））
//! - `minas exec <JSON>` — `Command` を1つ実行する（JSON は wire の [`Command`] そのまま）
//! - `minas edit <JSON>` — [`DocumentEdit`]（位置指定編集）を1つ実行する
//! - `minas wait <generation>` — 世代が `<generation>` を超えるまでブロックして状態を返す
//!   （90 秒で時間切れ: 現状を返し exit code 2 = 再試行可能）
//! - `minas hints <path>` — 任意パスの inlay hint を全文テキストなしで取得する（ADR-0020）
//! - `minas peek <path> <line>:<col>` — 指定位置（1-origin）の定義を全文なしで取得する（ADR-0025）
//! - `minas outline <path>` — シンボルの階層ツリー（名前・種別・範囲）を全文なしで取得する（ADR-0031）
//! - `minas at <path> <line>:<col>` — 指定位置を囲む記号とその正確な範囲を全文なしで取得する（ADR-0031）
//! - `minas hover <path> <line>:<col>` — 指定位置の hover（型・シグネチャ・doc）を全文なしで取得する（ADR-0032）
//! - `minas symbol <path> <query>` — ワークスペース内のシンボル検索を全文なしで取得する（ADR-0032）
//! - `minas check <path>` — 診断の settle を待って診断だけを返す（ADR-0032）
//!
//! 例: `minas exec '{"Insert": {"text": "hello"}}'`
//! 例: `minas edit '{"start": 0, "end": 0, "text": "hi", "checksum": <snapshot.checksum>}'`
//! 例: `minas apply src/lib.rs "let x = 1" "let x = 2"`
//! 例: `minas apply src/lib.rs --whole-stdin < new_content.rs`
//! 例: `minas wait 42`
//! 例: `minas hints src/main.rs`
//! 例: `minas peek src/main.rs 12:5`
//! 例: `minas outline src/main.rs`
//! 例: `minas at src/main.rs 12:5`
//!
//! daemon が動いていなければ自動起動される（TUI と同じ挙動）。終了コード:
//! 0 = 成功（適用・no-op 含む）、1 = トランスポート/JSON エラー、
//! 2 = 再試行可能な失敗 — `edit` が daemon に拒否された（checksum 不一致・
//! 範囲外）か、`wait` が時間内（90 秒）に世代超過を観測できなかった。
//! 拒否/待機失敗の詳細はスナップショットの `status` フィールドに載る。

use std::io::Read;
use std::io;
use std::path::PathBuf;

use clap::Subcommand;
use mina_protocol::{
    CheckDiagnostic, ClientKind, Command, DocumentEdit, InlayHint, OutlineSymbol, StateSnapshot,
    WorkspaceSymbol,
};
use mina_protocol::{ReferenceLocation, ServerMessage, Severity};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{timeout, Duration};

use mina_conn as conn;

/// ワンショット接続を開く（session CLI の各コマンドの共通冒頭）。
///
/// daemon が動いていなければ `minad` を探して起動してから（クライアント視点の
/// ライフサイクル）接続し、Hello（Headless 宣言）を送って (write_half, reader)
/// を返す。接続は 1コマンドごとに開く（永続ではない — ADR-0013）。
async fn open_one_shot() -> io::Result<(OwnedWriteHalf, BufReader<OwnedReadHalf>)> {
    let socket = mina_protocol::socket_path();
    if conn::connect(&socket).await.is_err() {
        let exe = conn::daemon_exe().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "minad が見つかりません（cargo install minad、または MINAD_EXE で指定）",
            )
        })?;
        conn::spawn_daemon(&exe, &["serve"])?;
        conn::wait_ready(&socket, 50).await?;
    }
    conn::open_session(&socket, ClientKind::Headless, true).await
}

/// `minas` のサブコマンド。引数・型は clap が検証する。
#[derive(Subcommand)]
pub enum SessionCmd {
    /// Fetch the current state (printed as JSON)
    Get {
        /// Restrict output to a line range `start:end` (1-origin, inclusive; `end`
        /// may be empty = last line). Prints only those numbered lines instead of
        /// the whole snapshot — cuts the token cost of reading a large file to
        /// the region needed (P1). Out-of-range `start` yields an explained zero
        /// result (Q3), `end` past EOF is clamped with a note.
        #[arg(long)]
        lines: Option<String>,
    },
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
        /// Apply multiple verified edits: stdin carries a JSON array
        /// [{"old": "...", "new": "..."}, ...] applied in order on one connection,
        /// saving once at the end (round-trip reduction). Exit 2 (nothing saved)
        /// if any old is not found or an edit is rejected.
        #[arg(long)]
        hunks_stdin: bool,
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
    /// Semantic rename of a symbol via LSP (ADR-0029). Content-addressed: `<old>`
    /// is resolved to the first identifier occurrence by the daemon; the language
    /// server rewrites all references (possibly across files) and the result is
    /// applied and saved. Response reports the impact: `N files, M edits` plus the
    /// changed file list. No positional math by the agent.
    Rename {
        /// File containing the symbol (relative to the agent's cwd)
        path: PathBuf,
        /// Symbol name to rename
        old: String,
        /// New name
        new: String,
    },
    /// List the reference locations of a symbol via LSP (ADR-0029, read-only).
    /// Resolves `<old>` like Rename and prints `path:line` (1-origin) locations
    /// without fetching full text — use with `--lines` to read.
    References {
        /// File containing the symbol (relative to the agent's cwd)
        path: PathBuf,
        /// Symbol name to look up
        old: String,
    },
    /// Fetch the hierarchical symbol outline of any path without full text
    /// (ADR-0031). Prints the symbol tree (name, kind, ranges) as JSON — the
    /// ranges double as the addresses for later reads and edits.
    Outline {
        /// Path
        path: PathBuf,
    },
    /// Report the symbol enclosing `<line>:<col>` (1-origin) with its exact
    /// range and name-token range (ADR-0031). Reads no full text — use the
    /// returned ranges with `--lines` / `apply`.
    At {
        /// Path
        path: PathBuf,
        /// Position as `line:col` (1-origin, col is a char count)
        pos: String,
    },
    /// Fetch the hover info (type/signature/doc) at `<line>:<col>` without full
    /// text (ADR-0032). Empty text = no hover at that position.
    Hover {
        /// Path
        path: PathBuf,
        /// Position as `line:col` (1-origin, col is a char count)
        pos: String,
    },
    /// Search symbols across the workspace via LSP `workspace/symbol`
    /// (ADR-0032). `<path>` anchors the workspace root; results can span the
    /// whole root (not just that file). Use instead of `rg` to find where a
    /// name is defined/declared.
    Symbol {
        /// Any file inside the workspace root (relative to the agent's cwd)
        path: PathBuf,
        /// Search query (fuzzy; empty is rejected with exit 1)
        query: String,
    },
    /// Wait for the LSP diagnostics of `<path>` to settle and return only the
    /// diagnostics — no full text (ADR-0032). Replaces `wait` + `get` + JSON
    /// parsing for the edit→verify loop. Exit 2 when at least one `error`
    /// diagnostic is present (warnings alone exit 0); exit 1 on not-supported /
    /// bad input; other failures exit 2.
    Check {
        /// Path
        path: PathBuf,
    },
}

/// `session wait` のタイムアウト。診断 settle の正常終了上限（~30–60 秒、
/// ADR-0028）にマージンを足した値。時間切れは exit code 2（再試行可能）で
/// 表す（e2e-01: settle の進まない wait が永久ブロックした欠陥の修正）。
const WAIT_TIMEOUT: Duration = Duration::from_secs(90);

/// [`wait_with_timeout`] の結果。タイムアウト時も現状スナップショットを返す。
#[derive(Debug, PartialEq)]
enum WaitOutcome {
    /// 時間内に世代が target を超えた時点のスナップショット。
    Completed(StateSnapshot),
    /// 時間内に世代が進まなかった。`fallback`（= 現状スナップショット）を返す。
    TimedOut(StateSnapshot),
}

/// `fut` を `timeout_dur` まで待つ。時間切れなら `fallback` の結果を
/// `TimedOut` として返す。待機中の接続は daemon 側でブロックされたまま
/// （per-connection 直列処理）なので、現状の取得は別接続で行う —
/// 呼び出し側は `GetState` を `fallback` に渡す。
async fn wait_with_timeout(
    timeout_dur: Duration,
    fut: impl std::future::Future<Output = io::Result<StateSnapshot>>,
    fallback: impl std::future::Future<Output = io::Result<StateSnapshot>>,
) -> io::Result<WaitOutcome> {
    match timeout(timeout_dur, fut).await {
        Ok(result) => Ok(WaitOutcome::Completed(result?)),
        Err(_elapsed) => Ok(WaitOutcome::TimedOut(fallback.await?)),
    }
}

/// `minas <subcommand>` を処理する。
pub async fn run(cmd: SessionCmd) -> io::Result<()> {
    match cmd {
        SessionCmd::Get { lines } => {
            let snapshot = execute(&Command::GetState).await?;
            match lines {
                // --lines: 全文スナップショットを渡す代わりに、対象行だけを番号付きで
                // 返す（トークン削減 — P1）。daemon へのソケット転送はローカルで無料
                // なので、節約は CLI 出力側（= LLM が読む量）で成立する。
                Some(range) => print_line_range(&snapshot, &range)?,
                None => println!("{}", serde_json::to_string_pretty(&snapshot)?),
            }
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
            // H1: `session edit` は保存しない（apply だけが保存まで行う）。daemon 上は
            // 変わったのにディスクが古いまま＝エージェントが旧ファイルを参照し続ける
            // 事故クラスを防ぐため、dirty のままなら永続化導線を stderr に1行だけ
            // 明示する（auto-save は不採用 — 非保存が正しいユースケースが存在する）。
            if snapshot.dirty {
                eprintln!("note: buffer is dirty (not saved); persist with: session exec '\"Save\"'");
            }
        }
        SessionCmd::Apply {
            path,
            old,
            new,
            whole,
            whole_stdin,
            hunks_stdin,
            old_file,
            new_file,
        } => {
            apply(
                &path,
                old,
                new,
                whole,
                whole_stdin,
                hunks_stdin,
                old_file,
                new_file,
            )
            .await?;
        }
        SessionCmd::Wait { generation } => {
            // WAIT_TIMEOUT 以内に世代が進まなければ、現状を返して再試行可能な
            // exit code 2 で終了する（settle が進まない編集等で永久ブロック
            // しないため — e2e-01 の中タスクで再現した欠陥）。
            match wait_with_timeout(
                WAIT_TIMEOUT,
                execute(&Command::WaitFor { generation }),
                execute(&Command::GetState),
            )
            .await?
            {
                WaitOutcome::Completed(snapshot) => {
                    println!("{}", serde_json::to_string_pretty(&snapshot)?);
                }
                WaitOutcome::TimedOut(snapshot) => {
                    eprintln!(
                        "wait timed out after {}s: generation {generation} が観測されなかった（再試行可能）",
                        WAIT_TIMEOUT.as_secs()
                    );
                    println!("{}", serde_json::to_string_pretty(&snapshot)?);
                    std::process::exit(2);
                }
            }
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
        SessionCmd::Rename { path, old, new } => {
            // 成功: `renamed: <old> -> <new> (N files, M edits)` と変更ファイル一覧
            // （トークン最小・モデルが影響範囲を確認できる形 — Q3）。
            // 失敗: stderr に理由、exit 1（入力エラー: 再試行不可）または
            // exit 2（再試行可能: シンボル未解決・LSP エラー・保存失敗）。
            let outcome = execute_rename(&path.to_string_lossy(), &old, &new).await?;
            if let Some(e) = &outcome.error {
                eprintln!("renamed: {e}");
                std::process::exit(rename_exit_code(e));
            }
            println!("renamed: {old} -> {new} ({} files, {} edits)", outcome.files, outcome.edits);
            for f in &outcome.changed {
                println!("changed: {f}");
            }
        }
        SessionCmd::References { path, old } => {
            // 成功: `N references in M files:` に続けて `path:line`（1-origin。
            // エージェントは位置から --lines で読む — T1 の原則でスニペットは返さない）。
            // 失敗: stderr に理由、exit 1/2（Rename と同じ分類）。
            let outcome = execute_references(&path.to_string_lossy(), &old).await?;
            if let Some(e) = &outcome.error {
                eprintln!("references: {e}");
                std::process::exit(references_exit_code(e));
            }
            let mut files = std::collections::BTreeSet::new();
            for loc in &outcome.locations {
                files.insert(loc.path.as_str());
            }
            println!("{} references in {} files:", outcome.total, files.len());
            for loc in &outcome.locations {
                println!("{}:{}", loc.path, loc.line + 1);
            }
        }
        SessionCmd::Outline { path } => {
            // 成功: シンボルの階層ツリーを JSON で出力する（全文なし — ADR-0031。
            // エージェントは得られた range を読み・編集の住所にする）。
            // 失敗: stderr に理由、exit 1（入力エラー: not supported / invalid）
            // または exit 2（再試行可能: LSP エラー等）。
            let outcome = execute_outline(&path.to_string_lossy()).await?;
            if let Some(e) = &outcome.error {
                eprintln!("outline: {e}");
                std::process::exit(outline_exit_code(e));
            }
            // compact JSON で出力する（トークン削減が目的の経路なので、pretty の
            // 空白を省く。262 記号で ~40% 削減 — ADR-0031 検証の実測）。
            println!("{}", serde_json::to_string(&outcome.symbols)?);
        }
        SessionCmd::At { path, pos } => {
            // 成功: 囲む記号（名前・種別・正確な範囲）を JSON で出力する —
            // エージェントは範囲をそのまま apply / --lines の住所にできる。
            // 記号なしは found: false で success（exit 0 — Peek の空定義と同じ流儀）。
            // 失敗: stderr に理由、exit 1/2（Rename / References と同じ分類）。
            let (line, col) = parse_position(&pos)?;
            let outcome = execute_enclosing(&path.to_string_lossy(), line, col).await?;
            if let Some(e) = outcome
                .get("error")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                eprintln!("at: {e}");
                std::process::exit(symbol_at_exit_code(e));
            }
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        }
        SessionCmd::Hover { path, pos } => {
            // 成功: 位置の hover（型・シグネチャ・doc）を JSON で出力する
            // （全文なし — ADR-0032）。hover の無い位置は text 空の成功応答
            // （exit 0 — Peek の空定義と同じ流儀）。失敗: stderr に理由、
            // exit 1/2（outline / at と同じ分類）。
            let (line, col) = parse_position(&pos)?;
            let outcome = execute_hover(&path.to_string_lossy(), line, col).await?;
            if let Some(e) = &outcome.error {
                eprintln!("hover: {e}");
                std::process::exit(hover_exit_code(e));
            }
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "path": outcome.path,
                    "text": outcome.text,
                }))?
            );
        }
        SessionCmd::Symbol { path, query } => {
            // 成功: ヒットしたシンボルを compact JSON で出力する（全文なし —
            // ADR-0032。エージェントは位置から --lines で読む）。失敗: stderr に
            // 理由、exit 1/2（outline / at と同じ分類）。
            let outcome = execute_symbol(&path.to_string_lossy(), &query).await?;
            if let Some(e) = &outcome.error {
                eprintln!("symbol: {e}");
                std::process::exit(symbol_exit_code(e));
            }
            println!("{}", serde_json::to_string(&outcome.symbols)?);
        }
        SessionCmd::Check { path } => {
            // 成功: 診断を compact JSON で出力する（全文なし — ADR-0032）。
            // クリーン（エラーなし）は exit 0、error 診断が1件でもあれば exit 2
            // （警告のみなら 0 — エージェントは $? だけで分岐できる）。失敗: stderr
            // に理由、exit 1/2（outline / at と同じ分類）。
            let outcome = execute_check(&path.to_string_lossy()).await?;
            if let Some(e) = &outcome.error {
                eprintln!("check: {e}");
                std::process::exit(check_exit_code(e));
            }
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "path": outcome.path,
                    "total": outcome.total,
                    "diagnostics": outcome.diagnostics,
                }))?
            );
            if outcome
                .diagnostics
                .iter()
                .any(|d| d.severity == Severity::Error)
            {
                std::process::exit(2);
            }
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

/// `session get --lines start:end` の出力。全文スナップショットの代わりに、対象行
/// だけを番号付き JSON で返す（トークン削減 — P1）。`start` が行数を超えれば
/// 説明付きゼロ結果（Q3/R1）、`end` が行数を超えれば最終行へクランプしてその旨を
/// 載せる。read の結果（番号付き行）がそのまま `session apply` の `old` 指定に
/// 使える（read/edit 契約統一 — P2）。
fn print_line_range(snapshot: &StateSnapshot, range: &str) -> io::Result<()> {
    let (start, end) = parse_line_range(range)?;
    let r = slice_lines(&snapshot.text, start, end);
    let end_str = end.map_or(String::new(), |e| e.to_string());
    let mut obj = serde_json::Map::new();
    obj.insert("path".into(), snapshot.path.clone().unwrap_or_default().into());
    obj.insert("generation".into(), snapshot.generation.into());
    obj.insert("line_count".into(), (r.line_count as u64).into());
    if r.out_of_range {
        // Q3: 「空の成功」ではなく理由付きのゼロ結果。エージェントは次の範囲指定を
        // 根拠を持って決められる（無駄な再試行をしない）。
        obj.insert(
            "note".into(),
            format!("no lines in {start}..{end_str}: file has {} lines", r.line_count).into(),
        );
        obj.insert("lines".into(), serde_json::Value::Array(vec![]));
    } else {
        obj.insert(
            "lines".into(),
            r.lines
                .iter()
                .map(|(n, t)| serde_json::json!({ "n": n, "text": t }))
                .collect::<Vec<_>>()
                .into(),
        );
        if r.clamped {
            obj.insert(
                "note".into(),
                format!("clamped to last line {} (requested end {end_str})", r.line_count).into(),
            );
        }
    }
    println!("{}", serde_json::Value::Object(obj));
    Ok(())
}

/// `start:end`（1-origin・両端含む）を解釈する。`end` 省略可（最終行まで）。
fn parse_line_range(range: &str) -> io::Result<(usize, Option<usize>)> {
    let (s, e) = range
        .split_once(':')
        .ok_or_else(|| invalid("行範囲は start:end 形式です (1-origin)"))?;
    let start: usize = s
        .trim()
        .parse()
        .map_err(|_| invalid(format!("行番号が不正です: {s:?}")))?;
    if start == 0 {
        return Err(invalid("行は 1 始まりです（0 は指定できません）"));
    }
    let end = if e.trim().is_empty() {
        None
    } else {
        let v: usize = e
            .trim()
            .parse()
            .map_err(|_| invalid(format!("行番号が不正です: {e:?}")))?;
        if v == 0 {
            return Err(invalid("行は 1 始まりです（0 は指定できません）"));
        }
        if v < start {
            return Err(invalid("end は start 以上にしてください"));
        }
        Some(v)
    };
    Ok((start, end))
}

struct LineSlice {
    lines: Vec<(usize, String)>,
    line_count: usize,
    out_of_range: bool,
    clamped: bool,
}

/// `text` から 1-origin・両端含む `start..=end` 行を取り出す。trailing newline は
/// 行として数えない。`end` が `None`（開いた範囲）は最終行まで。
fn slice_lines(text: &str, start: usize, end: Option<usize>) -> LineSlice {
    let line_count = if text.is_empty() {
        0
    } else {
        let n = text.split('\n').count();
        if text.ends_with('\n') { n - 1 } else { n }
    };
    let out_of_range = start > line_count;
    let concrete_end = if out_of_range {
        0
    } else {
        end.unwrap_or(line_count).min(line_count)
    };
    let clamped = end.is_some() && end.unwrap() > line_count;
    let mut lines = Vec::new();
    if !out_of_range {
        for (i, line) in text.split('\n').enumerate() {
            let no = i + 1;
            if no < start {
                continue;
            }
            if no > concrete_end {
                break;
            }
            lines.push((no, line.to_string()));
        }
    }
    LineSlice {
        lines,
        line_count,
        out_of_range,
        clamped,
    }
}

/// `session rename` の結果（[`ServerMessage::RenameResult`] の展開形）。
struct RenameOutcome {
    files: usize,
    edits: usize,
    changed: Vec<String>,
    error: Option<String>,
}

/// `session references` の結果（[`ServerMessage::ReferencesResult`] の展開形）。
struct ReferencesOutcome {
    locations: Vec<ReferenceLocation>,
    total: usize,
    error: Option<String>,
}

/// `session outline` の結果（[`ServerMessage::Outline`] の展開形）。
struct OutlineOutcome {
    symbols: Vec<OutlineSymbol>,
    error: Option<String>,
}

/// rename の失敗を exit コードに分類する（ADR-0029）: 入力エラー（not supported /
/// invalid input）は再試行しても通らないので 1、それ以外（シンボル未解決・
/// LSP エラー・保存失敗）は再試行可能なので 2。
fn rename_exit_code(e: &str) -> i32 {
    if e.starts_with("rename not supported") || e.starts_with("invalid input") {
        1
    } else {
        2
    }
}

/// references の失敗の exit コード分類（rename と同型）。
fn references_exit_code(e: &str) -> i32 {
    if e.starts_with("references not supported") || e.starts_with("invalid input") {
        1
    } else {
        2
    }
}

/// daemon に接続し、内容指定の意味リネーム（[`Command::Rename`]）を実行する
/// （ADR-0029）。応答は全文を運ばない [`ServerMessage::RenameResult`]。
async fn execute_rename(path: &str, old: &str, new: &str) -> io::Result<RenameOutcome> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    // CRITICAL C2: パスは agent の cwd 基準で絶対化してから送る
    let command = Command::Rename {
        path: conn::absolutize(path),
        old: old.to_string(),
        new: new.to_string(),
    };
    let mut line = serde_json::to_string(&command).expect("コマンドはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::RenameResult {
            files,
            edits,
            changed,
            error,
            ..
        }) => Ok(RenameOutcome {
            files,
            edits,
            changed,
            error,
        }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "Rename にスナップショット応答が返った（旧 daemon: 再ビルドしてください）",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {
            Err(invalid("Rename に想定外の軽量応答が返った"))
        }
        Err(e) => Err(invalid(format!("不正な応答: {e}"))),
    }
}

/// daemon に接続し、シンボルの参照位置列挙（[`Command::References`]）を実行する
/// （ADR-0029）。応答は全文を運ばない [`ServerMessage::ReferencesResult`]。
async fn execute_references(path: &str, old: &str) -> io::Result<ReferencesOutcome> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = Command::References {
        path: conn::absolutize(path),
        old: old.to_string(),
    };
    let mut line = serde_json::to_string(&command).expect("コマンドはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::ReferencesResult {
            locations,
            total,
            error,
            ..
        }) => Ok(ReferencesOutcome {
            locations,
            total,
            error,
        }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "References にスナップショット応答が返った（旧 daemon: 再ビルドしてください）",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {
            Err(invalid("References に想定外の軽量応答が返った"))
        }
        Err(e) => Err(invalid(format!("不正な応答: {e}"))),
    }
}

/// outline の失敗を exit コードに分類する（ADR-0031）: 入力エラー（not supported /
/// invalid）は再試行しても通らないので 1、それ以外（LSP エラー等）は再試行可能
/// なので 2。
fn outline_exit_code(e: &str) -> i32 {
    if e.starts_with("outline not supported") || e.starts_with("invalid input") {
        1
    } else {
        2
    }
}

/// symbol range（`session at`）の失敗の exit コード分類（outline と同型）。
fn symbol_at_exit_code(e: &str) -> i32 {
    if e.starts_with("symbol range not supported") || e.starts_with("invalid input") {
        1
    } else {
        2
    }
}

/// hover の失敗の exit コード分類（ADR-0032。outline と同型）: 入力エラー
/// （not supported / invalid）は再試行しても通らないので 1、それ以外（LSP エラー）
/// は再試行可能なので 2。
fn hover_exit_code(e: &str) -> i32 {
    if e.starts_with("hover not supported") || e.starts_with("invalid input") {
        1
    } else {
        2
    }
}

/// symbol search（`session symbol`）の失敗の exit コード分類（hover と同型）。
fn symbol_exit_code(e: &str) -> i32 {
    if e.starts_with("symbol search not supported") || e.starts_with("invalid input") {
        1
    } else {
        2
    }
}

/// check の失敗の exit コード分類（ADR-0032。outline と同型）: 入力エラー
/// （not supported / invalid）は再試行しても通らないので 1、それ以外（LSP エラー）
/// は再試行可能なので 2。
fn check_exit_code(e: &str) -> i32 {
    if e.starts_with("check not supported") || e.starts_with("invalid input") {
        1
    } else {
        2
    }
}

/// daemon に接続し、シンボルの階層ツリーを軽量応答（[`ServerMessage::Outline`]）
/// で受け取る（ADR-0031）。全文は運ばれない。
async fn execute_outline(path: &str) -> io::Result<OutlineOutcome> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = Command::Outline {
        path: conn::absolutize(path),
    };
    let mut line = serde_json::to_string(&command).expect("コマンドはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::Outline {
            symbols, error, ..
        }) => Ok(OutlineOutcome { symbols, error }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "Outline にスナップショット応答が返った（旧 daemon: 再ビルドしてください）",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {
            Err(invalid("Outline に想定外の軽量応答が返った"))
        }
        Err(e) => Err(invalid(format!("不正な応答: {e}"))),
    }
}

/// daemon に接続し、指定位置を囲む記号の範囲を軽量応答
/// （[`ServerMessage::EnclosingSymbol`]）で受け取る（ADR-0031）。全文は運ばれない。
async fn execute_enclosing(
    path: &str,
    line: u32,
    col: u32,
) -> io::Result<serde_json::Value> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = Command::EnclosingSymbol {
        path: conn::absolutize(path),
        line,
        col,
    };
    let mut line = serde_json::to_string(&command).expect("コマンドはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::EnclosingSymbol {
            path,
            name,
            kind,
            range,
            selection_range,
            found,
            error,
        }) => Ok(serde_json::json!({
            "path": path,
            "name": name,
            "kind": kind,
            "range": range,
            "selection_range": selection_range,
            "found": found,
            "error": error,
        })),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "EnclosingSymbol にスナップショット応答が返った（旧 daemon: 再ビルドしてください）",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. }) => {
            Err(invalid("EnclosingSymbol に想定外の軽量応答が返った"))
        }
        Err(e) => Err(invalid(format!("不正な応答: {e}"))),
    }
}

/// `session hover` の結果（[`ServerMessage::Hover`] の展開形。ADR-0032）。
struct HoverOutcome {
    path: String,
    text: String,
    error: Option<String>,
}

/// `session symbol` の結果（[`ServerMessage::WorkspaceSymbols`] の展開形。ADR-0032）。
struct SymbolOutcome {
    symbols: Vec<WorkspaceSymbol>,
    error: Option<String>,
}

/// `session check` の結果（[`ServerMessage::Check`] の展開形。ADR-0032）。
struct CheckOutcome {
    path: String,
    total: usize,
    diagnostics: Vec<CheckDiagnostic>,
    error: Option<String>,
}

/// daemon に接続し、指定位置の hover を軽量応答（[`ServerMessage::Hover`]）で受け取る
/// （ADR-0032）。全文は運ばれない。
async fn execute_hover(path: &str, line: u32, col: u32) -> io::Result<HoverOutcome> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = Command::HoverAt {
        path: conn::absolutize(path),
        line,
        col,
    };
    let mut line = serde_json::to_string(&command).expect("コマンドはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::Hover { path, text, error, .. }) => Ok(HoverOutcome { path, text, error }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "Hover にスナップショット応答が返った（旧 daemon: 再ビルドしてください）",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. }) => Err(invalid("Hover に想定外の軽量応答が返った")),
        Err(e) => Err(invalid(format!("不正な応答: {e}"))),
    }
}

/// daemon に接続し、ワークスペース内のシンボル検索を軽量応答
/// （[`ServerMessage::WorkspaceSymbols`]）で受け取る（ADR-0032）。全文は運ばれない。
async fn execute_symbol(path: &str, query: &str) -> io::Result<SymbolOutcome> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = Command::WorkspaceSymbol {
        path: conn::absolutize(path),
        query: query.to_string(),
    };
    let mut line = serde_json::to_string(&command).expect("コマンドはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::WorkspaceSymbols { symbols, error, .. }) => Ok(SymbolOutcome { symbols, error }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "WorkspaceSymbol にスナップショット応答が返った（旧 daemon: 再ビルドしてください）",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::Check { .. }) => {
            Err(invalid("WorkspaceSymbol に想定外の軽量応答が返った"))
        }
        Err(e) => Err(invalid(format!("不正な応答: {e}"))),
    }
}

/// daemon に接続し、対象パスの診断を軽量応答（[`ServerMessage::Check`]）で受け取る
/// （ADR-0032）。全文は運ばれない。
async fn execute_check(path: &str) -> io::Result<CheckOutcome> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = Command::CheckDiagnostics {
        path: conn::absolutize(path),
    };
    let mut line = serde_json::to_string(&command).expect("コマンドはシリアライズ可能");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::Check { path, total, diagnostics, error, .. }) => Ok(CheckOutcome {
            path,
            total,
            diagnostics,
            error,
        }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "Check にスナップショット応答が返った（旧 daemon: 再ビルドしてください）",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. }) => {
            Err(invalid("Check に想定外の軽量応答が返った"))
        }
        Err(e) => Err(invalid(format!("不正な応答: {e}"))),
    }
}

/// daemon に接続し、指定位置の定義を軽量応答（[`ServerMessage::Peek`]）で受け取る。
async fn execute_peek(path: &str, line: u32, col: u32) -> io::Result<mina_protocol::Peek> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    // CRITICAL C2: パスは agent の cwd 基準で絶対化してから送る
    conn::request_peek(&mut write_half, &mut reader, &conn::absolutize(path), line, col).await
}

/// daemon に接続し、`GetServerInfo` の応答（[`ServerMessage::ServerInfo`]）を取得する
/// （issue #27/D1）。ビルド世代（Git hash）と起動からの累積メトリクスを返す。
/// スナップショットを運ばず世代・push・イベントを進めない軽量応答なので専用経路。
async fn execute_server_info() -> io::Result<serde_json::Value> {
    let (mut write_half, mut reader) = open_one_shot().await?;
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
        }) => Ok(serde_json::json!({
            "generation": generation,
            "daemon_build_ts": daemon_build_ts,
            // I4: CLI 側のビルド世代も開示する（D1 のクライアント側バリアント —
            // 再ビルド後に release バイナリが古いまま、という事故の検知）。build.rs が
            // 注入した MINA_GIT_HASH / MINA_BUILD_TS を読む。
            "cli_generation": option_env!("MINA_GIT_HASH").unwrap_or("unknown"),
            "cli_build_ts": option_env!("MINA_BUILD_TS")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0),
            "metrics": metrics,
        })),
        Ok(mina_protocol::ServerMessage::Response { .. })
        | Ok(mina_protocol::ServerMessage::Push { .. }) => {
            Err(invalid("GetServerInfo にスナップショット応答が返った（旧 daemon: 再ビルドしてください）"))
        }
        Ok(mina_protocol::ServerMessage::Hints { .. })
        | Ok(mina_protocol::ServerMessage::Peek { .. })
        | Ok(mina_protocol::ServerMessage::Outline { .. })
        | Ok(mina_protocol::ServerMessage::Hover { .. })
        | Ok(mina_protocol::ServerMessage::WorkspaceSymbols { .. })
        | Ok(mina_protocol::ServerMessage::Check { .. })
        | Ok(mina_protocol::ServerMessage::EnclosingSymbol { .. }) => {
            Err(invalid("GetServerInfo に想定外の軽量応答が返った"))
        }
        Ok(mina_protocol::ServerMessage::RenameResult { .. })
        | Ok(mina_protocol::ServerMessage::ReferencesResult { .. }) => {
            Err(invalid("GetServerInfo に想定外の軽量応答が返った"))
        }
        Err(e) => Err(invalid(format!("不正な応答: {e}"))),
    }
}

/// daemon に接続し、コマンドを実行してスナップショットを受け取る。
async fn execute(command: &Command) -> io::Result<StateSnapshot> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    // CRITICAL C2: Open のパスは agent の cwd 基準で絶対化してから送る（TUI と
    // 同一の契約）。そのまま送ると daemon の spawn cwd 基準で解決され、意図しない
    // ファイルを開く恐れがある。
    let command = absolutize_paths(command.clone());
    conn::request(&mut write_half, &mut reader, &command).await
}

/// パスを含むコマンドのパスを絶対化する（Open / GetInlayHints。他はそのまま）。
///
/// [`mina_conn::absolutize`] を共通化して使う — daemon 側の cwd は spawn 時に
/// 固定されるため、解決は送信側（agent の cwd）で行う。
fn absolutize_paths(command: Command) -> Command {
    match command {
        Command::Open { path } => Command::Open {
            path: conn::absolutize(&path),
        },
        Command::GetInlayHints { path } => Command::GetInlayHints {
            path: conn::absolutize(&path),
        },
        other => other,
    }
}

/// daemon に接続し、位置指定編集（DocumentEdit）を実行してスナップショットを受け取る。
async fn execute_edit(edit: &DocumentEdit) -> io::Result<StateSnapshot> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    conn::request(&mut write_half, &mut reader, edit).await
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
/// - `--hunks-stdin`: 複数編集を 1 接続で適用（Open → Edit×N → Save×1）
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
    hunks_stdin: bool,
    old_file: Option<PathBuf>,
    new_file: Option<PathBuf>,
) -> io::Result<()> {
    // --hunks-stdin: 複数編集を 1 プロセス・1 接続で適用（ラウンドトリップ削減）。
    // 他入力指定と排他。
    if hunks_stdin {
        if old.is_some() || new.is_some() || whole.is_some() || old_file.is_some() || new_file.is_some() {
            return Err(invalid("--hunks-stdin は他の入力指定と併用できません"));
        }
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        let hunks = parse_hunks(&buf)?;
        return apply_hunks(path, &hunks).await;
    }

    let (old_text, new_text) = resolve_apply_args(old, new, whole, whole_stdin, old_file, new_file)?;

    let (mut write_half, mut reader) = open_one_shot().await?;

    let abs = conn::absolutize(&path.to_string_lossy());
    let mut snapshot = conn::request(&mut write_half, &mut reader, &Command::Open { path: abs.clone() }).await?;
    // 未存在パス: 空ファイルを作成してから再 Open し、Save で新規作成する
    // （#28/#29: 新規ファイルを直接アドレスする現実解。report 3-1 の touch→open
    // 2段階ハックをコマンド側に内包。自動オープンの範囲外判断（#28）には触れない）。
    // ponytail: 編集失敗時も空ファイルが残る（touch と同じ挙動）。気になるなら
    // daemon 側の Open で未存在パスのドキュメント生成を検討する。
    if snapshot.status.is_some() && std::fs::metadata(&abs).is_err() {
        std::fs::write(&abs, "")
            .map_err(|e| invalid(format!("新規ファイル作成失敗: {abs}: {e}")))?;
        snapshot = conn::request(&mut write_half, &mut reader, &Command::Open { path: abs.clone() }).await?;
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
    let snapshot = conn::request(&mut write_half, &mut reader, &edit).await?;
    if let Some(status) = &snapshot.status {
        // 拒否（checksum/expected_text 不一致）: 再試行可能な失敗として exit 2
        eprintln!("EDIT REJECTED: {status}");
        std::process::exit(2);
    }

    let snapshot = conn::request(&mut write_half, &mut reader, &Command::Save).await?;
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

/// `--hunks-stdin` の編集 1 つ分。`old` を現在テキストに検索し `new` で置換する。
#[derive(Debug, Deserialize)]
struct Hunk {
    #[serde(default)]
    old: String,
    #[serde(default)]
    new: String,
}

/// `--hunks-stdin` の入力を解釈する。少なくとも 1 つ、かつ `old` が空でないこと。
fn parse_hunks(input: &str) -> io::Result<Vec<Hunk>> {
    let hunks: Vec<Hunk> = serde_json::from_str(input)
        .map_err(|e| invalid(format!("--hunks-stdin の JSON を解釈できません: {e}")))?;
    if hunks.is_empty() {
        return Err(invalid("--hunks-stdin は少なくとも 1 つの hunk が必要です"));
    }
    if let Some(h) = hunks.iter().find(|h| h.old.is_empty()) {
        return Err(invalid("hunk.old は空にできません (置換対象が必要)"));
    }
    Ok(hunks)
}

/// `--hunks-stdin` の実体: 複数編集を 1 接続で順次適用し、最後に一度だけ Save する。
/// 途中で `old` 未発見や daemon 拒否なら exit 2（Save 前なのでディスクは無変更
/// — 単発 apply の失敗契約を複数に一般化したもの。エージェントは state で再確認
/// →修正→再試行）。成功時は generation を JSON で返し、続く `wait <generation>`
/// を別プロセスなしで直接呼べるようにする（ラウンドトリップ削減 — e2e-01）。
async fn apply_hunks(path: &PathBuf, hunks: &[Hunk]) -> io::Result<()> {
    let (mut write_half, mut reader) = open_one_shot().await?;

    let abs = conn::absolutize(&path.to_string_lossy());
    let mut snapshot = conn::request(
        &mut write_half,
        &mut reader,
        &Command::Open { path: abs.clone() },
    )
    .await?;
    // 既存 apply と同じ: 未存在パスは空ファイルを touch して再 Open（Save で新規作成）
    if snapshot.status.is_some() && std::fs::metadata(&abs).is_err() {
        std::fs::write(&abs, "")
            .map_err(|e| invalid(format!("新規ファイル作成失敗: {abs}: {e}")))?;
        snapshot = conn::request(
            &mut write_half,
            &mut reader,
            &Command::Open { path: abs.clone() },
        )
        .await?;
    }
    if let Some(status) = &snapshot.status {
        return Err(invalid(format!("Open 失敗: {status}")));
    }

    let mut applied = 0usize;
    for hunk in hunks {
        // 直前の編集適用後の現在テキストに対し位置を再計算する（行揺れを踏む。char 単位）
        let text = snapshot.text.clone();
        let Some((start, end)) = find_range(&text, &hunk.old) else {
            eprintln!("NOT FOUND: {:?}", short(&hunk.old));
            std::process::exit(2); // Save 前なのでディスク無変更
        };
        let edit = DocumentEdit {
            start,
            end,
            text: hunk.new.clone(),
            checksum: snapshot.checksum,
            expected_text: Some(hunk.old.clone()),
        };
        snapshot = conn::request(&mut write_half, &mut reader, &edit).await?;
        if let Some(status) = &snapshot.status {
            eprintln!("EDIT REJECTED: {status}");
            std::process::exit(2);
        }
        applied += 1;
    }

    let snapshot = conn::request(&mut write_half, &mut reader, &Command::Save).await?;
    let saved = snapshot
        .status
        .as_deref()
        .is_some_and(|s| s.starts_with("saved"));
    if !saved {
        eprintln!("SAVE FAILED: {:?}", snapshot.status);
        std::process::exit(2);
    }
    // Q3: 成功時に generation を返す（エージェントは wait <generation> を直接呼べる）
    println!(
        "{}",
        serde_json::json!({
            "applied": abs,
            "edits": applied,
            "generation": snapshot.generation,
            "checksum": snapshot.checksum,
        })
    );
    Ok(())
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
    let (mut write_half, mut reader) = open_one_shot().await?;
    // CRITICAL C2: パスは agent の cwd 基準で絶対化してから送る（Open と同一の契約）
    conn::request_hints(&mut write_half, &mut reader, &conn::absolutize(path)).await
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

    #[tokio::test]
    async fn wait_with_timeout_returns_current_state_on_timeout() {
        // e2e-01 で再現した欠陥（世代が進まない wait の永久ブロック）の回帰
        // テスト。ヘルパーは分離済みなので短時間の timeout で検証でき、実 CLI
        // の 90 秒定数に依存しない。完了パスは daemon 側の
        // `wait_for_generation_blocks_until_change` が実接続で検証済み。
        let current = StateSnapshot {
            text: "current".into(),
            generation: 5,
            ..Default::default()
        };

        // 完了パス: 未来が瞬時に完了すればその結果を返し、fallback は呼ばれない
        let done = wait_with_timeout(
            Duration::from_millis(200),
            async { Ok(current.clone()) },
            async { unreachable!("完了時は fallback を呼ばない") },
        )
        .await
        .unwrap();
        assert!(matches!(done, WaitOutcome::Completed(s) if s.generation == 5));

        // タイムアウトパス: 完了しない未来は time out し、fallback（現状取得）
        // の結果を TimedOut として返す
        let pending = std::future::pending::<io::Result<StateSnapshot>>();
        let timed_out = wait_with_timeout(
            Duration::from_millis(50),
            pending,
            async { Ok(current.clone()) },
        )
        .await
        .unwrap();
        assert!(
            matches!(&timed_out, WaitOutcome::TimedOut(s) if s.generation == 5),
            "タイムアウト時は現状スナップショットを返す: {timed_out:?}"
        );
    }

    #[test]
    fn parse_hunks_rejects_bad_input() {
        // 不正 JSON・空・空 old は拒否。有効な配列はそのまま解釈する。
        assert!(parse_hunks("not json").is_err());
        assert!(parse_hunks("[]").is_err(), "空配列は拒否");
        assert!(parse_hunks("[{\"old\":\"\",\"new\":\"b\"}]").is_err(), "空 old は拒否");

        let hunks = parse_hunks("[{\"old\":\"a\",\"new\":\"b\"},{\"old\":\"c\",\"new\":\"\"}]").unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old, "a");
        assert_eq!(hunks[0].new, "b");
        assert_eq!(hunks[1].old, "c");
        assert_eq!(hunks[1].new, "", "空 new は削除を意味し許容");
    }

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
    fn parse_line_range_parses_start_end() {
        // start:end（1-origin・両端含む）。end 省略可。0・逆順・不正は拒否。
        assert_eq!(parse_line_range("10:20").unwrap(), (10, Some(20)));
        assert_eq!(parse_line_range("1:1").unwrap(), (1, Some(1)));
        assert_eq!(parse_line_range("5:").unwrap(), (5, None), "end 省略=最終行");
        assert!(parse_line_range("0:5").is_err(), "行 0 は拒否");
        assert!(parse_line_range("5:0").is_err(), "end=0 は拒否");
        assert!(parse_line_range("20:10").is_err(), "end < start は拒否");
        assert!(parse_line_range("a:b").is_err(), "数値以外は拒否");
        assert!(parse_line_range("5").is_err(), "コロンなしは拒否");
    }

    #[test]
    fn slice_lines_selects_range_and_detects_edges() {
        // 通常範囲（trailing newline を 1 行に数えない）
        let r = slice_lines("a\nb\nc\nd\n", 2, Some(3));
        assert_eq!(r.line_count, 4);
        assert!(!r.out_of_range);
        assert!(!r.clamped);
        assert_eq!(r.lines, vec![(2, "b".into()), (3, "c".into())]);

        // end 省略 = 最終行まで
        let r = slice_lines("a\nb\nc", 2, None);
        assert_eq!(r.line_count, 3);
        assert_eq!(r.lines, vec![(2, "b".into()), (3, "c".into())]);

        // end 超過は最終行へクランプされ clamped フラグが立つ
        let r = slice_lines("a\nb", 1, Some(99));
        assert_eq!(r.line_count, 2);
        assert!(r.clamped);
        assert!(!r.out_of_range);
        assert_eq!(r.lines, vec![(1, "a".into()), (2, "b".into())]);

        // start 超過は説明付きゼロ結果（Q3）
        let r = slice_lines("a\nb\nc", 50, Some(60));
        assert!(r.out_of_range);
        assert_eq!(r.line_count, 3);
        assert!(r.lines.is_empty());

        // 空文字列は 0 行
        let r = slice_lines("", 1, None);
        assert_eq!(r.line_count, 0);
        assert!(r.out_of_range);
    }

    #[test]
    fn slice_lines_handles_empty_interior_lines() {
        // 文書内部の空行は正しく数え・返す
        let r = slice_lines("a\n\nb\n", 1, Some(3));
        assert_eq!(r.line_count, 3);
        assert_eq!(r.lines, vec![(1, "a".into()), (2, "".into()), (3, "b".into())]);
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
        let oldf = dir.join(format!("minae-apply-old-{}.txt", std::process::id()));
        let newf = dir.join(format!("minae-apply-new-{}.txt", std::process::id()));
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
