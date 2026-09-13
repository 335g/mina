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
    WorkspaceSymbol, ReviewCommentView,
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
/// 接続冒頭の Hello に載せる自己申告ラベル（ADR-0038）。[`run`] で一度だけ
/// 解決される（--name > MINAE_CLIENT_NAME > "unknown"）。
static CLIENT_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

async fn open_one_shot() -> io::Result<(OwnedWriteHalf, BufReader<OwnedReadHalf>)> {
    let socket = mina_protocol::socket_path();
    if conn::connect(&socket).await.is_err() {
        let exe = conn::daemon_exe().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "minad not found (cargo install minad, or set MINAD_EXE)",
            )
        })?;
        conn::spawn_daemon(&exe, &["serve"])?;
        conn::wait_ready(&socket, 50).await?;
    }
    conn::open_session(
        &socket,
        ClientKind::Headless,
        true,
        CLIENT_NAME.get().map(String::as_str).unwrap_or("unknown"),
    )
    .await
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
        /// Strip the unbounded history fields (`events`, `activity`) from the
        /// output. The snapshot itself is small; these two are the token bomb
        /// (78-char buffer -> tens of KB; rust2 #5-3). Use when you only need
        /// text / checksum / dirty.
        #[arg(long)]
        brief: bool,
    },
    /// Read the text of any path without touching the current buffer
    /// (ADR-0048). Buffer-independent and LSP-free: never moves focus/generation,
    /// prefers the open document (unsaved edits included) and falls back to disk.
    /// The response is a lightweight text message, not a full snapshot.
    Read {
        /// File to read (relative to the agent's cwd, like `Open`)
        path: PathBuf,
        /// Restrict output to a line range `start:end` (1-origin, inclusive; `end`
        /// may be empty = last line). Prints only those numbered lines instead of
        /// the whole text — same contract as `get --lines`.
        #[arg(long)]
        lines: Option<String>,
    },
    /// Delete a file (ADR-0060) — the editing surface covers creation (`apply`)
    /// and modification but a module move needs the old file GONE; without this
    /// the agent falls back to the shell's `rm`. Refuses (never silently
    /// succeeds) when the path is under a read-only base root, is not a regular
    /// file, or has unsaved edits in the daemon's buffer. An already-absent path
    /// is a NO-OP (exit 0) so repeated scripts do not fail.
    Delete {
        /// File to delete (relative to the agent's cwd, like `Open`)
        path: PathBuf,
    },
    /// Find literal matches in any file without reading it in full (read-only).
    /// Prints `{path, generation, total, truncated, matches:[{line,col,len}]}`
    /// (1-origin line, 1-origin CHAR column, char length). Buffer-first: the open
    /// document's unsaved edits are searched, else the file on disk. Use it to
    /// locate occurrences cheaply, then read only the lines you need.
    Search {
        /// File to search (relative to the agent's cwd, like `Open`)
        path: PathBuf,
        /// Literal text to find (empty is rejected)
        query: String,
        /// Case-insensitive search
        #[arg(long, short = 'i')]
        ignore_case: bool,
        /// Match only at word boundaries (ASCII identifiers). Use it when
        /// sweeping a renamed OLD name that is a prefix of the NEW one
        /// (e.g. TOTAL_COUNT vs TOTAL_COUNT_HEADER).
        #[arg(long, short = 'w')]
        word: bool,
    },
    /// Fetch the daemon build generation and metrics (printed as JSON, issue #27).
    /// This is the DAEMON's build/metrics (the wire command is `GetServerInfo`),
    /// and it ALSO lists the configured language servers — `servers:
    /// [{id, languages, running_sessions}]` — plus `base_roots` (comparison
    /// roots registered via `RegisterBaseRoot`; empty is normal, language
    /// servers are resolved per file, not through base_roots). Use `servers`
    /// when `check` said "no LSP server configured" (rust2 #49).
    Info,
    /// Run one `Command` (JSON is the wire [`Command`] as-is)
    Exec {
        /// Command JSON
        json: String,
    },
    /// Run one [`DocumentEdit`] (position-addressed edit). `expected_text` is
    /// REQUIRED: without it a shifted range is not detected (document checksum
    /// is whole-document, not range-local) and the edit silently corrupts
    /// (rust2 #5-2). Prefer `apply` for content-addressed editing.
    Edit {
        /// DocumentEdit JSON
        json: String,
        /// Strip `events` / `activity` from the snapshot response (token
        /// reduction — same as `get --brief`; rust2 #5-3)
        #[arg(long)]
        brief: bool,
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
        /// Apply several verified edits from file pairs (repeatable):
        /// `--pair old.txt new.txt [--pair old2.txt new2.txt ...]`. Each pair is
        /// applied in order on one connection (same atomic contract as
        /// --hunks-stdin, but texts come from files — no JSON \n escaping).
        #[arg(long, num_args = 2, value_names = ["OLD_FILE", "NEW_FILE"])]
        pair: Vec<std::path::PathBuf>,
        /// Allow replacing a non-empty file with empty text (—whole /
        /// --whole-stdin). Without it, an empty whole-file replacement is
        /// refused: it almost always means stdin was not connected (rust2 #5-1).
        #[arg(long)]
        allow_empty: bool,
    },
    /// Block until the generation exceeds `<generation>` and return the state
    Wait {
        /// Generation
        generation: u64,
        /// Strip `events` / `activity` from the snapshot response (token
        /// reduction — same as `get --brief`; the wait loop pays this cost per
        /// wake, rust2 #15: 68KB per wake without it)
        #[arg(long)]
        brief: bool,
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
    /// ranges double as the addresses for later reads and edits. With
    /// `--recursive`, file-scoped modules are followed to their definition files
    /// and nested into the tree (ADR-0049).
    Outline {
        /// Path
        path: PathBuf,
        /// Follow file-scoped module declarations to their definition files and
        /// inline the nested tree (ADR-0049). Without it, only the one file's
        /// symbols are returned. Specifying --depth also enables recursion
        /// (depth then overrides the default of 3).
        #[arg(long, short)]
        recursive: bool,
        /// Maximum module depth for recursion (default 3, must be >= 1).
        /// 1 = direct child modules only. Implies --recursive. Counts MODULE
        /// hops (file to file), not symbol nesting: a direct child module is
        /// inlined whole at --depth 1.
        #[arg(long)]
        depth: Option<u32>,
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
    /// Wait for the LSP diagnostics of `<path>...` to settle and return only the
    /// diagnostics — no full text (ADR-0032). Replaces `wait` + `get` + JSON
    /// parsing for the edit→verify loop. Multiple paths are checked in order and
    /// aggregated (ADR-0046 — one round trip per path, client-side loop). Each
    /// entry carries a `verdict` (`errors` / `warnings` / `clean-unverified` /
    /// on failure `failed` with an `error` reason) so agents can branch without
    /// re-deriving the settled semantics (ADR-0045/0052). `--summary` prints
    /// `path: verdict` lines instead of the JSON array.
    /// Exit 2 when at least one `error` diagnostic is present (warnings alone
    /// exit 0); exit 1 on not-supported / bad input; other failures exit 2.
    Check {
        /// Paths to check (one or more)
        paths: Vec<PathBuf>,
        /// Print one verdict line per path (`path: errors|clean-unverified|failed
        /// — reason`) instead of the full JSON array. Exit codes are unchanged.
        #[arg(long)]
        summary: bool,
        /// Check the whole crate in one call: expand `<path>` (a .rs file inside
        /// the crate, e.g. src/lib.rs) to every .rs under its directory tree
        /// (src/) plus, with --include-tests, the sibling tests/ dir. Compile
        /// errors often span several files at once — this closes the
        /// edit→verify loop in one round trip (rust2 #7).
        #[arg(long, value_name = "PATH")]
        crate_root: Option<PathBuf>,
        /// With --crate-root: also check the sibling `tests/` directory.
        #[arg(long)]
        include_tests: bool,
    },
    /// List the daemon-held review comments for AI consumption (#50, read-only).
    /// Prints the full list (path, side, stored/resolved lines, stale flag,
    /// snippet, body, base) as compact JSON — the extraction path of the
    /// compare-review → AI loop. With `--clear`, deletes all comments instead
    /// (agent cleanup; TUI re-pin/Unregister also clears).
    Review {
        /// Delete all review comments instead of listing
        #[arg(long)]
        clear: bool,
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
pub async fn run(cmd: SessionCmd, name: Option<String>) -> io::Result<()> {
    // ADR-0038: 自己申告ラベルを一度だけ解決する（--name > MINAE_CLIENT_NAME > "unknown"）。
    let _ = CLIENT_NAME.set(
        name.unwrap_or_else(|| {
            std::env::var("MINAE_CLIENT_NAME")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".to_string())
        }),
    );
    match cmd {
        SessionCmd::Get { lines, brief } => {
            let snapshot = execute(&Command::GetState).await?;
            match lines {
                // --lines: 全文スナップショットを渡す代わりに、対象行だけを番号付きで
                // 返す（トークン削減 — P1）。daemon へのソケット転送はローカルで無料
                // なので、節約は CLI 出力側（= LLM が読む量）で成立する。
                Some(range) => print_line_range(&snapshot, &range)?,
                None => {
                    // --brief: events/activity（無制限近い履歴・数十KB）を落とす。
                    if brief {
                        print_brief_snapshot(&snapshot)?;
                    } else {
                        println!("{}", serde_json::to_string_pretty(&snapshot)?);
                    }
                }
            }
        }
        SessionCmd::Read { path, lines } => {
            // ADR-0048: パス指定の軽量テキスト読み。get（現在のバッファ）と違い
            // 任意パスをバッファを汚さず読む。失敗は stderr + exit 1（cannot open = 
            // 入力エラー、再試行不可）。
            let outcome = execute_read(&path.to_string_lossy()).await?;
            if let Some(e) = &outcome.error {
                eprintln!("read: {e}");
                std::process::exit(1);
            }
            match lines {
                // --lines: get と同じ番号付き行契約（read→apply の old にそのまま使える）。
                Some(range) => print_text_lines(
                    &outcome.path,
                    outcome.generation,
                    outcome.checksum,
                    &outcome.text,
                    &range,
                )?,
                None => {
                    // ADR-0063 / self-host #10: 生テキスト経路も revision を stderr に出す。
                    // stdout は `minas apply <new> --whole-stdin` にそのまま流せる形のまま
                    // （skill は小さいファイルでは --lines より素の read を勧めているので、
                    // そこだけ revision が無いと identity が途切れる）。
                    print!("{}", outcome.text);
                    eprintln!("read: revision {}", outcome.checksum);
                }
            }
        }
        SessionCmd::Delete { path } => {
            // ADR-0060: ファイル削除。成功は `deleted: <path>`、不在は NO-OP
            // （どちらも exit 0 — `rm -f` と同じで、繰り返し走るスクリプトを
            // 失敗させない）。拒否は理由ごとに分類する:
            //   cannot delete / read-only-base … 入力エラー（1、再試行不可）
            //   delete-rejected（未保存編集）… 呼び出し側が直せる（2）
            let status = execute_delete(&path.to_string_lossy()).await?;
            let status = status.unwrap_or_else(|| "no status".into());
            if let Some(rest) = status.strip_prefix("deleted: ") {
                println!("deleted: {rest}");
                return Ok(());
            }
            if let Some(rest) = status.strip_prefix("no-op: ") {
                eprintln!("NO-OP: {rest}");
                return Ok(());
            }
            eprintln!("delete failed: {status}");
            std::process::exit(delete_exit_code(&status));
        }
        SessionCmd::Search {
            path,
            query,
            ignore_case,
            word,
        } => {
            // 全文を読まずに一致位置だけを得る（rust2 要望）。失敗は stderr + exit 1
            // （cannot open / empty query = 入力エラー、再試行不可）。
            let outcome = execute_search(
                &path.to_string_lossy(),
                &query,
                !ignore_case,
                word,
            )
            .await?;
            if let Some(e) = &outcome.error {
                eprintln!("search: {e}");
                std::process::exit(1);
            }
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "path": outcome.path,
                    "generation": outcome.generation,
                    "checksum": outcome.checksum,
                    "total": outcome.total,
                    "truncated": outcome.truncated,
                    "matches": outcome.matches,
                }))?
            );
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
                .map_err(|e| invalid(format!("cannot parse the command JSON: {e}")))?;
            exec_raw(&command).await?;
        }
        SessionCmd::Edit { json, brief } => {
            let edit: DocumentEdit = serde_json::from_str(&json)
                .map_err(|e| invalid(format!("cannot parse the DocumentEdit JSON: {e}")))?;
            // 位置指定 edit の静かな破壊ガード（rust2 #5-2）: expected_text は
            // 「置換範囲が本当に想定テキストか」を検証する唯一の手段。文書
            // checksum は文書単位なので位置ズレを検出できず、exit 0 で壊す
            // （println! 2 箇所で +5 ずらす再現）。headless からは apply を
            // 使うのが正しく、raw edit は expected_text 必須とする。
            if edit.expected_text.is_none() {
                return Err(invalid(
                    "positional edit requires expected_text (the old text at start..end) — \
                     document checksum cannot detect a shifted range. Use `minas apply` \
                     for content-addressed editing (rust2 #5-2)",
                ));
            }
            let snapshot = execute_edit(&edit).await?;
            if brief {
                print_brief_snapshot(&snapshot)?;
            } else {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            }
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
            pair,
            allow_empty,
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
                pair,
                allow_empty,
            )
            .await?;
        }
        SessionCmd::Wait { generation, brief } => {
            // WAIT_TIMEOUT 以内に世代が進まなければ、現状を返して再試行可能な
            // exit code 2 で終了する（settle が進まない編集等で永久ブロック
            // しないため — e2e-01 の中タスクで再現した欠陥）。
            // 内容変化のみで起床する契約は daemon 側の WaitFor が担う
            // （rust2 #16: Open 段階での早起床を避ける）。
            let print = |snapshot: &StateSnapshot| -> io::Result<()> {
                if brief {
                    print_brief_snapshot(snapshot)
                } else {
                    println!("{}", serde_json::to_string_pretty(snapshot)?);
                    Ok(())
                }
            };
            match wait_with_timeout(
                WAIT_TIMEOUT,
                execute(&Command::WaitFor { generation }),
                execute(&Command::GetState),
            )
            .await?
            {
                WaitOutcome::Completed(snapshot) => {
                    print(&snapshot)?;
                }
                WaitOutcome::TimedOut(snapshot) => {
                    eprintln!(
                        "wait timed out after {}s: generation {generation} was not observed (retryable)",
                        WAIT_TIMEOUT.as_secs()
                    );
                    print(&snapshot)?;
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
                eprintln!("rename failed: {e}");
                std::process::exit(rename_exit_code(e));
            }
            println!("renamed: {old} -> {new} ({} files, {} edits)", outcome.files, outcome.edits);
            for f in &outcome.changed {
                println!("changed: {f}");
            }
            // ドッグフーディング #16: LSP の rename は**アクティブな cfg の範囲**だけを
            // 書き換える（`#[cfg(feature = "x")]` の非活性モジュールは RA の解析対象外）。
            // 「N files, M edits」を影響範囲として読むと、feature ビルドで E0425 になる。
            // テキストレベルの網をかけて、変更されなかったのに名前が残るファイルを警告する。
            // rename は改名後に走査するので、ヒットしたファイルに残る旧名は
            // すべて「LSP が変えなかった言及」（文字列・コメント）。差し引き不要。
            warn_leftover_mentions(
                &path,
                &old,
                &outcome.changed,
                &std::collections::HashMap::new(),
                "rename",
            );
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
            // #16 / self-host #4: 参照一覧も cfg 非活性のコードを見ない。同じ網を
            // かけて警告し、さらに**一覧に載ったファイルの中**の未列挙言及
            // （文字列・コメント）を件数で報告する。LSP が挙げた件数をファイル別に
            // 数えて差し引くので、二重計上しない。
            let mut touched: Vec<String> = Vec::new();
            let mut listed: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for loc in &outcome.locations {
                let abs = conn::absolutize(&loc.path);
                if !listed.contains_key(&abs) {
                    touched.push(abs.clone());
                }
                *listed.entry(abs).or_insert(0) += 1;
            }
            warn_leftover_mentions(&path, &old, &touched, &listed, "references");
        }
        SessionCmd::Outline { path, recursive, depth } => {
            // 成功: シンボルの階層ツリーを JSON で出力する（全文なし — ADR-0031。
            // エージェントは得られた range を読み・編集の住所にする）。
            // 失敗: stderr に理由、exit 1（入力エラー: not supported / invalid）
            // または exit 2（再試行可能: LSP エラー等）。
            // ADR-0049: --recursive か --depth 指定でモジュール横断（--depth は
            // 既定 3 を上書きし、再帰を兼ねる）。
            let recursive = recursive || depth.is_some();
            let depth = depth.unwrap_or(3);
            if recursive && depth == 0 {
                eprintln!("outline: invalid input: depth must be >= 1");
                std::process::exit(1);
            }
            let outcome =
                execute_outline(&path.to_string_lossy(), recursive, depth).await?;
            if let Some(e) = &outcome.error {
                eprintln!("outline: {e}");
                std::process::exit(outline_exit_code(e));
            }
            if outcome.truncated {
                // ADR-0049: シンボル総数上限で打ち切った旨を stderr に出す
                // （exit は変えない — 部分結果は成功扱い）。
                //
                // ドッグフーディング #9: 「--depth で制限しろ」は**直接の子
                // モジュールが巨大な場合には効かない**（--depth 1 でもその
                // モジュールは丸ごと inline されるので、同じ 500 で切れる）。
                // 実際に効く逃げ道だけを案内する。
                eprintln!(
                    "outline: truncated at {} symbols — one module expanded to more than the cap. \
                     --depth does NOT help when a direct child module is this large; \
                     use a per-file `minas outline <file>` (no --recursive) for the big module",
                    MAX_RECURSIVE_OUTLINE_SYMBOLS
                );
            }
            // compact JSON で出力する（トークン削減が目的の経路なので、pretty の
            // 空白を省く。262 記号で ~40% 削減 — ADR-0031 検証の実測）。
            println!("{}", serde_json::to_string(&outcome.symbols)?);
            // ADR-0063 / self-host #3: 単一ファイルの outline は「どの内容に対する
            // 範囲か」を revision で言える（`read --lines` の "checksum" と同じ値）。
            // --recursive はツリーが複数ファイルにまたがるので出さない（ノードごとの
            // `path` と、そのファイルの read の checksum で照合する）。
            if !recursive {
                eprintln!("outline: revision {}", outcome.checksum);
            }
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
                    "checksum": outcome.checksum,
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
            // self-host #2: 依存ソースなどプロジェクト外のヒットを注記する。
            note_out_of_project_hits(&path, &outcome.symbols);
        }
        SessionCmd::Check {
            paths,
            summary,
            crate_root,
            include_tests,
        } => {
            // --crate-root: クレートを構成する .rs を列挙して 1 コールで検証する
            // （rust2 #7: version 追加で src 4 ファイル + tests 2 ファイルが同時に
            // 壊れ、手で列挙しきれなかった）。Rust の crate 構成はディレクトリ木と
            // ほぼ同型（src/ の .rs + 任意で tests/）なので、LSP を使わず既存の
            // check ループに流す。target や .hidden は除外。
            let mut paths = paths;
            if let Some(crate_root) = &crate_root {
                let root_abs = conn::absolutize(&crate_root.to_string_lossy());
                if !root_abs.ends_with(".rs") {
                    return Err(invalid("--crate-root takes a .rs file (e.g. src/lib.rs)"));
                }
                let Some(crate_dir) = std::path::Path::new(&root_abs).parent() else {
                    return Err(invalid("cannot resolve the parent directory of --crate-root"));
                };
                let mut rs = Vec::new();
                collect_rs_files(crate_dir, &mut rs);
                // 警告は「ディレクトリに .rs が 1 つも無い」場合だけ。以前は
                // 「ルート以外に .rs が無い」で出していたため、**単一ファイル
                // クレート**（新規ライブラリの普通の形）で
                // 「no .rs files found」と言いながら直下でそのファイルを check
                // していた（dogfooding #14: stderr と stdout が矛盾する）。
                let dir_has_rs = !rs.is_empty();
                rs.retain(|p| conn::absolutize(&p.to_string_lossy()) != root_abs);
                rs.sort();
                paths = vec![crate_root.clone()];
                paths.extend(rs);
                if include_tests {
                    // tests/ は crate_dir の兄弟。`join("..")` で組み立てると
                    // 展開結果に `..` が残り、**失敗経路**（daemon が入力をそのまま
                    // 返す）と**成功経路**（文書を開いて正規化される）で同じファイルが
                    // 別の綴りになる（driver #7: `src/../tests/x.rs` vs
                    // `tests/x.rs`）。パス文字列で突き合わせる呼び出し側が壊れる。
                    let tests_dir = crate_dir
                        .parent()
                        .map(|p| p.join("tests"))
                        .unwrap_or_else(|| crate_dir.join("tests"));
                    let mut ts = Vec::new();
                    collect_rs_files(&tests_dir, &mut ts);
                    ts.sort();
                    paths.extend(ts);
                }
                if !dir_has_rs {
                    eprintln!("check: no .rs files found under {}", crate_dir.display());
                }
            }
            // 成功: 各パスの診断を順に取得し、集約した compact JSON を出力する
            // （全文なし — ADR-0032/0046）。複数ファイルの変更を 1 コマンドで
            // 一括検証できる（第1回検証: 1ファイルずつしか渡せなかった）。
            // クリーン（エラーなし）は exit 0、error 診断が1件でもあれば exit 2
            // （警告のみなら 0 — エージェントは $? だけで分岐できる）。失敗: stderr
            // に理由、exit 1/2（outline / at と同じ分類）。失敗したパスも
            // 配列の 1 要素（"error" 付き）として返す — stdout が `[]` になると
            // 「診断なし」と「取得失敗」が区別できない（ADR-0046 追記）。
            if paths.is_empty() {
                eprintln!("check: at least one path is required");
                std::process::exit(1);
            }
            let mut results = Vec::new();
            let mut any_error = false;
            let mut worst_failure = 0;
            for path in &paths {
                let outcome = execute_check(&path.to_string_lossy()).await?;
                if let Some(e) = &outcome.error {
                    // 失敗の分類（not supported = exit 1、再試行可能 = exit 2）を
                    // 保持しつつ、他パスの検証は続ける（ADR-0046）。失敗したパスも
                    // 配列の 1 要素（"error" 付き）として返す — stdout が `[]` に
                    // なると「診断なし」と「取得失敗」が区別できない（ADR-0046 追記）。
                    worst_failure = worst_failure.max(check_exit_code(e));
                    if summary {
                        println!("failed: {} — {e}", outcome.path);
                    } else {
                        results.push(serde_json::json!({
                            "path": outcome.path,
                            "total": 0,
                            "diagnostics": [],
                            "settled": false,
                            "verdict": "failed",
                            "error": e,
                        }));
                    }
                    continue;
                }
                // ADR-0045/0052: 空は常に settled:false（クリーンの根拠にしない）。
                // verdict は「確定クリーン」を決して言わない（pull は実在エラーを
                // 取りこぼすため — クリーン判定は cargo）。エージェントは verdict で
                // 分岐し、グリーン判定は実ビルドに落とす。
                // severity 別に数える（rust2 #9: RA の Hint — #[cfg] の非活性分岐
                // など — を「warnings」と表示すると存在しない rustc 警告を探す）。
                // Hint/Info を含むファイルはまとめて「hints (n)」とする（warnings に
                // 数えない — check が返すのは LSP 診断であって rustc lint ではない）。
                let unlinked = outcome.diagnostics.iter().any(is_unlinked_file);
                let (verdict, not_clean) = if unlinked {
                    // 「どのクレートにも属していない」= 解析対象になっていない。
                    // 新規作成の直後（cargo 再ロードの窓）ではこの Hint だけが返り、
                    // 本物のエラーはリンク後に現れる（ドッグフーディング #3）。
                    // `hints (n)` + exit 0 で「警告だけのクリーン」に見せると
                    // create → check → commit のループが偽グリーンで通る。
                    // 検証できていないので exit 2 に倒す（クリーンでもエラーでもない）。
                    ("unlinked-unverified".to_string(), true)
                } else if outcome.diagnostics.is_empty() {
                    // プロジェクト外（マーカーなし）なら LSP は解析対象を持たず、
                    // 空は「クリーン」でも「未 settle」でもなく「対象外」（rust2 #21）。
                    let v = if in_project(&outcome.path) {
                        "clean-unverified"
                    } else {
                        "unlinked-unverified"
                    };
                    // 対象外も「見ていない」ので exit 2（未検証を exit 0 にしない）。
                    let not_clean = !in_project(&outcome.path);
                    (v.to_string(), not_clean)
                } else {
                    let n_e = outcome
                        .diagnostics
                        .iter()
                        .filter(|d| d.severity == Severity::Error)
                        .count();
                    let n_w = outcome
                        .diagnostics
                        .iter()
                        .filter(|d| d.severity == Severity::Warning)
                        .count();
                    let n_h = outcome
                        .diagnostics
                        .iter()
                        .filter(|d| {
                            matches!(d.severity, Severity::Hint | Severity::Info)
                        })
                        .count();
                    let verdict = if n_e > 0 {
                        format!("errors ({n_e})")
                    } else if n_w > 0 {
                        format!("warnings ({n_w})")
                    } else {
                        format!("hints ({n_h})")
                    };
                    (verdict, n_e > 0)
                };
                any_error |= not_clean;
                if summary {
                    println!("{}: {}", outcome.path, verdict);
                } else {
                    results.push(serde_json::json!({
                        "path": outcome.path,
                        "total": outcome.total,
                        "diagnostics": outcome.diagnostics,
                        "settled": outcome.settled,
                        "verdict": verdict,
                    }));
                }
            }
            if !summary {
                println!("{}", serde_json::to_string(&results)?);
            }
            // 集約: error 診断が 1 件でもあれば 2（確定）。なければ各パスの失敗分類
            // （1 = 再試行不可 / 2 = 再試行可能）を最大値で返す。以前は
            // `2.max(worst_failure)` で常に 2 以上になり、1 分類が握り潰されていた
            // （rust2 #23: `check README.md` が exit 2 になっていた）。
            // self-host #7 の「別プロジェクトのファイルが同じ verdict 文言で返る」は
            // 注記を試して撤回した: cargo workspace の**メンバー**と、ツリー内の
            // 無関係なパッケージ（gitignore されたスクラッチ等）を、cargo の
            // メンバー判定なしには区別できない（`members` グロブ + path 依存の
            // 自動メンバー）。区別できない警告は、正しい使い方（メンバー crate を
            // ルートから check する）で誤爆するだけなので、skill の記述で扱う。
            let code = check_final_exit_code(any_error, worst_failure);
            if code != 0 {
                std::process::exit(code);
            }
        }
        SessionCmd::Review { clear } => {
            // #50: daemon 保持のレビューコメントを抽出する（AI への入力化経路）。
            // 成功: 全文付き一覧を compact JSON で出力する（全文なしの他経路と
            // 違い、コメント本文こそが成果物のため全文を出す — stale 付きで
            // ずれた先の判断は AI が行う）。空一覧は `[]` の成功（exit 0）。
            // --clear: 全消し（TUI の再ピン/Unregister と同じ効果）。空 Clear も
            // 成功（exit 0 — daemon の M1: 変化なしに合わせる）。
            if clear {
                let snapshot = execute(&Command::ClearReviewComments).await?;
                println!("{}", serde_json::to_string(&snapshot.review_comment_count)?);
                return Ok(());
            }
            let outcome = execute_reviews().await?;
            println!("{}", serde_json::to_string(&outcome)?);
        }
    }
    Ok(())
}

/// `<line>:<col>`（1-origin）を解釈する。不正ならエラー。
fn parse_position(pos: &str) -> io::Result<(u32, u32)> {
    let Some((line, col)) = pos.split_once(':') else {
        return Err(invalid(format!("position must be <line>:<col>: {pos:?}")));
    };
    let line = line
        .parse::<u32>()
        .map_err(|_| invalid(format!("invalid line number: {line:?}")))?;
    let col = col
        .parse::<u32>()
        .map_err(|_| invalid(format!("invalid column number: {col:?}")))?;
    if line == 0 || col == 0 {
        return Err(invalid("line and column are 1-origin (0 is not allowed)"));
    }
    Ok((line, col))
}

/// `session get --lines start:end` / `session read --lines start:end` の出力。
/// 全文の代わりに、対象行だけを番号付き JSON で返す（トークン削減 — P1）。
/// `start` が行数を超えれば説明付きゼロ結果（Q3/R1）、`end` が行数を超えれば
/// 最終行へクランプしてその旨を載せる。read/get の結果（番号付き行）がそのまま
/// `session apply` の `old` 指定に使える（read/edit 契約統一 — P2）。
fn print_line_range(snapshot: &StateSnapshot, range: &str) -> io::Result<()> {
    print_text_lines(
        &snapshot.path.clone().unwrap_or_default(),
        snapshot.generation,
        snapshot.checksum,
        &snapshot.text,
        range,
    )
}

/// `minas get --brief` の出力。`events` / `activity`（履歴で数十 KB になりうる）
/// を取り除いたスナップショットを compact JSON で返す（トークン削減 — rust2
/// #5-3: 78 chars のバッファで 79KB の snapshot、内訳は events ~65KB + activity
/// ~12KB）。残るのは text / checksum / dirty 等の現状態 — `get --lines` と
/// 併用すれば「対象行 + checksum」を小刻みに取れる。
fn print_brief_snapshot(snapshot: &StateSnapshot) -> io::Result<()> {
    let mut value = serde_json::to_value(snapshot).map_err(|e| invalid(e.to_string()))?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("events");
        obj.remove("activity");
    }
    println!("{}", serde_json::to_string(&value).map_err(|e| invalid(e.to_string()))?);
    Ok(())
}

/// [`print_line_range`] のテキスト直接版（ADR-0048 — `read` の `--lines`）。
/// スナップショットを介さず `path`/`generation`/`text` から同じ番号付き
/// 出力を作る。
fn print_text_lines(
    path: &str,
    generation: u64,
    checksum: u64,
    text: &str,
    range: &str,
) -> io::Result<()> {
    let (start, end) = parse_line_range(range)?;
    let r = slice_lines(text, start, end);
    let end_str = end.map_or(String::new(), |e| e.to_string());
    let mut obj = serde_json::Map::new();
    obj.insert("path".into(), path.into());
    obj.insert("generation".into(), generation.into());
    obj.insert("line_count".into(), (r.line_count as u64).into());
    // ADR-0063: この窓がどの内容に対するものか。outline / at が返す revision と
    // 比べれば、位置が古いテキストを指していないか確認できる。
    obj.insert("checksum".into(), checksum.into());
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
        .ok_or_else(|| invalid("the line range must be start:end (1-origin)"))?;
    let start: usize = s
        .trim()
        .parse()
        .map_err(|_| invalid(format!("invalid line number: {s:?}")))?;
    if start == 0 {
        return Err(invalid("lines are 1-origin (0 is not allowed)"));
    }
    let end = if e.trim().is_empty() {
        None
    } else {
        let v: usize = e
            .trim()
            .parse()
            .map_err(|_| invalid(format!("invalid line number: {e:?}")))?;
        if v == 0 {
            return Err(invalid("lines are 1-origin (0 is not allowed)"));
        }
        if v < start {
            return Err(invalid("end must be greater than or equal to start"));
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
    /// ルートファイルの内容 revision（fnv1a64。ADR-0063）。--recursive では
    /// ツリーが複数ファイルにまたがるので、これはルートファイルの分だけ。
    checksum: u64,
    symbols: Vec<OutlineSymbol>,
    error: Option<String>,
    /// ADR-0049: 再帰横断時にシンボル総数上限で打ち切られたか。
    truncated: bool,
}

/// outline --recursive のシンボル総数上限（ADR-0049。daemon 側の
/// MAX_RECURSIVE_OUTLINE_SYMBOLS と同じ値 — truncated 警告の表示用）。
const MAX_RECURSIVE_OUTLINE_SYMBOLS: usize = 500;

/// rename の失敗を exit コードに分類する（ADR-0029）: 入力エラー（not supported /
/// invalid input）は再試行しても通らないので 1、それ以外（シンボル未解決・
/// LSP エラー・保存失敗）は再試行可能なので 2。
fn rename_exit_code(e: &str) -> i32 {
    if lsp_input_error(e) {
        1
    } else {
        2
    }
}

/// references の失敗の exit コード分類（rename と同型）。
fn references_exit_code(e: &str) -> i32 {
    if lsp_input_error(e) {
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
    let mut line = serde_json::to_string(&command).expect("the command must be serializable");
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
            "Rename returned a snapshot response (old daemon: rebuild)",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::ReadPath { .. })
        | Ok(ServerMessage::SearchMatches { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::ReviewComments { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {
            Err(invalid("Rename: unexpected lightweight response"))
        }
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
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
    let mut line = serde_json::to_string(&command).expect("the command must be serializable");
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
            "References returned a snapshot response (old daemon: rebuild)",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::ReadPath { .. })
        | Ok(ServerMessage::SearchMatches { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::ReviewComments { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {
            Err(invalid("References: unexpected lightweight response"))
        }
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
    }
}

/// outline の失敗を exit コードに分類する（ADR-0031）: 入力エラー（not supported /
/// invalid）は再試行しても通らないので 1、それ以外（LSP エラー等）は再試行可能
/// なので 2。
fn outline_exit_code(e: &str) -> i32 {
    if lsp_input_error(e) {
        1
    } else {
        2
    }
}

/// symbol range（`session at`）の失敗の exit コード分類（outline と同型）。
fn symbol_at_exit_code(e: &str) -> i32 {
    if lsp_input_error(e) {
        1
    } else {
        2
    }
}

/// hover の失敗の exit コード分類（ADR-0032。outline と同型）: 入力エラー
/// （not supported / invalid）は再試行しても通らないので 1、それ以外（LSP エラー）
/// は再試行可能なので 2。
fn hover_exit_code(e: &str) -> i32 {
    if lsp_input_error(e) {
        1
    } else {
        2
    }
}

/// symbol search（`session symbol`）の失敗の exit コード分類（hover と同型）。
fn symbol_exit_code(e: &str) -> i32 {
    if lsp_input_error(e) {
        1
    } else {
        2
    }
}

/// rust-analyzer が「このファイルはどのクレートにも属していない」と言っているか
/// （`unlinked-file` 診断）。
///
/// 重要: これは**新規作成直後の cargo 再ロード窓**にも出る。実測（ドッグ
/// フーディング #3、`tests/cold.rs` を書いて直後に `check`）:
///   1 回目: severity=Hint, "This file is not included in any crates … adding
///            \"unlinked-file\" to rust-analyzer.diagnostics.disabled" → hints (1), exit 0
///   3 秒後: severity=Error, "expected u64, found &'static str" → errors (1), exit 2
/// つまり本物のエラーが「リンク後に」現れる。メッセージ本文はバージョンで
/// 揺れるため、rust-analyzer 自身の設定キー名（unlinked-file）と既知の文言の
/// 両方で照合する。
fn is_unlinked_file(d: &mina_protocol::CheckDiagnostic) -> bool {
    d.message.contains("unlinked-file")
        || d.message.contains("not included in any crates")
        || d.message.contains("not included anywhere in the module tree")
}

/// LSP コマンド共通の「入力エラー（再試行しても通らない）」判定。
///
/// 再試行しても結果が変わらない失敗だけを 1 に落とす: 対象外（LSP 非対応）・
/// 不正入力・開けないパス・大きすぎるファイル。LSP の一時的な失敗（索引中・
/// ロック競合）は 2 のまま。
///
/// 「cannot open」を入力エラーに含めるのが要点 — 存在しないパス・ディレクトリ・
/// バイナリは何度投げても同じで、exit code だけで分岐するエージェントが無限
/// 再試行してしまう（rust2 #23 の check だけが例外だったのを全コマンドに揃える。
/// ドッグフーディング #2 と同根: 「見れなかった」と「壊れている」を混同しない）。
fn lsp_input_error(e: &str) -> bool {
    e.starts_with("invalid input")
        || e.starts_with("cannot open")
        || e.starts_with("cannot delete")
        || e.starts_with("read-only-base")
        || e.starts_with("file too large")
        || e.contains("not supported")
}

/// check の失敗の exit コード分類（ADR-0032。outline と同型）: 入力エラー
/// （not supported / invalid）は再試行しても通らないので 1、それ以外（LSP エラー）
/// は再試行可能なので 2。
fn check_exit_code(e: &str) -> i32 {
    // 再試行不可（usage / 対象外 / 開けない）は 1。LSP の再試行可能失敗は 2。
    // rust2 #23: outline と同型に揃える（exit code だけで分岐するエージェントが
    // `check README.md` を無限再試行しない）。
    if lsp_input_error(e) {
        1
    } else {
        2
    }
}

/// check の最終 exit code: error 診断が 1 件でもあれば 2（確定）。なければ各パスの
/// 失敗分類（1 = 再試行不可 / 2 = 再試行可能）を最大値で返す。
///
/// rust2 #23: 以前は `2.max(worst_failure)` で常に 2 以上になり、`check not
/// supported` / `cannot open` の 1 分類が握り潰されていた。
fn check_final_exit_code(any_error: bool, worst_failure: i32) -> i32 {
    if any_error { 2 } else { worst_failure }
}

/// LSP が見ないコードのテキスト網（ドッグフーディング #16）。
///
/// `rename` / `references` は言語サーバの解析結果なので、**アクティブな cargo
/// 構成の外**（`#[cfg(feature = "x")]` の非活性モジュール、クレートグラフに入って
/// いないファイル）は見えない。それでも「N files, M edits」は影響範囲として読まれる
/// ので、feature ビルドで初めて E0425 が出る（driver #16 の実測: `cargo build` は
/// green のまま `cargo build --features gated` が壊れた）。
///
/// 網は「対象ファイルと同じディレクトリ配下の .rs」を読み、LSP が触らなかった
/// ファイルに旧名が単語として残っていれば stderr に列挙するだけ（exit は変えない —
/// コメント/文字列の誤検出があり得るし、判断は読む側に残す）。
///
/// 上限: [`LEFTOVER_SCAN_MAX_FILES`] ファイル / 1 ファイル [`LEFTOVER_SCAN_MAX_BYTES`]。
/// ディレクトリ走査なので、別クレート（crates/*）からの参照までは見えない。
fn warn_leftover_mentions(
    path: &std::path::Path,
    old: &str,
    touched: &[String],
    // LSP がそのファイルで挙げた件数（`references` のみ。`rename` は空 — 改名後は
    // 旧名が「一覧に載っていない言及」だけになるため差し引き不要）。
    listed: &std::collections::HashMap<String, usize>,
    what: &str,
) {
    if old.is_empty() {
        return;
    }
    let Some(dir) = path.parent() else {
        return;
    };
    // 相対パスのままだと祖先の walk-up が空パスを踏んで走査範囲を失う（実測:
    // `crates/b/src/lib.rs` で scan_root が空になり警告が出なかった）。
    // まず絶対化してから祖先を辿る。
    let abs_path = std::path::PathBuf::from(conn::absolutize(&path.to_string_lossy()));
    let dir = abs_path.parent().unwrap_or(dir);
    // 走査範囲（ドッグフーディング #16 の追試）: シンボルの**利用側は別の
    // ディレクトリ（多くの場合別クレート）に居る**ので、対象ファイルの
    // ディレクトリだけを走査するのは助けが要らない側を走査していた。
    // 祖先を辿って**最も外側の Cargo.toml を持つディレクトリ**（workspace root）
    // を探し、そこを走査する（見つからなければファイルのディレクトリ）。
    let mut scan_root = dir.to_path_buf();
    let mut probe = dir.to_path_buf();
    while let Some(parent) = probe.parent() {
        if parent.join("Cargo.toml").is_file() {
            scan_root = parent.to_path_buf();
        }
        probe = parent.to_path_buf();
    }
    let mut files = Vec::new();
    collect_rs_files(&scan_root, &mut files);
    let touched: std::collections::HashSet<String> = touched
        .iter()
        .map(|p| conn::absolutize(p))
        .collect();
    let mut leftovers = Vec::new();
    // ヒットしたファイルの中にも、LSP が改名しない言及（文字列リテラル・コメント）が
    // 残る。ファイル単位の警告だけだと、その差分が「完全な影響範囲」に見える
    // （ドッグフーディング self-host #4: daemon.rs の in-file recall 87%、5 件の
    // assert メッセージ文字列が不可視）。件数だけを 1 行で報告する。
    let mut in_touched_mentions = 0usize;
    for f in files.into_iter().take(LEFTOVER_SCAN_MAX_FILES) {
        let abs = conn::absolutize(&f.to_string_lossy());
        let Ok(md) = std::fs::metadata(&f) else {
            continue;
        };
        if md.len() > LEFTOVER_SCAN_MAX_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let n = count_whole_word(&text, old);
        if n == 0 {
            continue;
        }
        if touched.contains(&abs) {
            let already = *listed.get(&abs).unwrap_or(&0);
            in_touched_mentions += n.saturating_sub(already);
        } else {
            leftovers.push(abs);
        }
    }
    if !leftovers.is_empty() {
        eprintln!(
            "warning: {} file(s) still mention `{old}` but were not touched by the LSP {what} \
             (inactive #[cfg] / not in the crate graph / comment or string) — verify manually:",
            leftovers.len()
        );
        for f in leftovers {
            eprintln!("  {f}");
        }
    }
    if in_touched_mentions > 0 {
        eprintln!(
            "note: {in_touched_mentions} more textual mention(s) of `{old}` inside the \
             file(s) the LSP {what} did touch (string literals / comments) — the LSP does \
             not rename those; `minas search <file> {old}` or rg to see them"
        );
    }
}

/// `text` に `word` が識別子として（前後が識別子文字でない）現れる回数。
fn count_whole_word(text: &str, word: &str) -> usize {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut count = 0;
    let mut from = 0;
    while let Some(idx) = text[from..].find(word) {
        let start = from + idx;
        let end = start + word.len();
        let before_ok = text[..start].chars().next_back().is_none_or(|c| !is_word(c));
        let after_ok = text[end..].chars().next().is_none_or(|c| !is_word(c));
        if before_ok && after_ok {
            count += 1;
        }
        from = start + word.chars().next().map_or(1, |c| c.len_utf8());
    }
    count
}

/// 旧名の残留スキャンの上限（ファイル数 / 1 ファイルのサイズ）。
const LEFTOVER_SCAN_MAX_FILES: usize = 500;
const LEFTOVER_SCAN_MAX_BYTES: u64 = 1024 * 1024;

/// daemon に接続し、シンボルの階層ツリーを軽量応答（[`ServerMessage::Outline`]）
/// で受け取る（ADR-0031）。`recursive` ならファイル分割モジュールを辿る
/// [`Command::OutlineRecursive`]（ADR-0049）。全文は運ばれない。
async fn execute_outline(path: &str, recursive: bool, depth: u32) -> io::Result<OutlineOutcome> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = if recursive {
        Command::OutlineRecursive {
            path: conn::absolutize(path),
            depth,
        }
    } else {
        Command::Outline {
            path: conn::absolutize(path),
        }
    };
    let mut line = serde_json::to_string(&command).expect("the command must be serializable");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::Outline {
            symbols,
            checksum,
            error,
            truncated,
            ..
        }) => Ok(OutlineOutcome {
            checksum,
            symbols,
            error,
            truncated,
        }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "Outline returned a snapshot response (old daemon: rebuild)",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::ReviewComments { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. })
        | Ok(ServerMessage::ReadPath { .. })
        | Ok(ServerMessage::SearchMatches { .. }) => {
            Err(invalid("Outline: unexpected lightweight response"))
        }
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
    }
}

/// `session read` の結果（[`ServerMessage::ReadPath`] の展開形。ADR-0048）。
struct ReadOutcome {
    path: String,
    generation: u64,
    /// このテキストの revision（fnv1a64。ADR-0063）。位置（`--lines`）は内容に
    /// 依存するので、別の呼び出しの位置をこの値で照合できる。
    checksum: u64,
    text: String,
    error: Option<String>,
}

/// daemon に `Command::DeletePath` を送り、応答の status を返す（ADR-0060）。
/// 成功・拒否・NO-OP はすべて status の 1 行で表される（応答は通常のスナップショット）。
async fn execute_delete(path: &str) -> io::Result<Option<String>> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = Command::DeletePath {
        path: conn::absolutize(path),
    };
    let snapshot = conn::request(&mut write_half, &mut reader, &command).await?;
    Ok(snapshot.status)
}

/// `delete` の失敗分類（ADR-0060）: 入力エラー（cannot delete / read-only-base /
/// not supported）は再試行しても通らないので 1、それ以外（未保存編集の拒否など、
/// 呼び出し側が直せるもの）は 2。
fn delete_exit_code(status: &str) -> i32 {
    if lsp_input_error(status) {
        1
    } else {
        2
    }
}

/// daemon に接続し、パス指定の軽量テキスト読み（[`ServerMessage::ReadPath`]）で
/// 受け取る（ADR-0048）。LSP 非依存・バッファ非依存 — フォーカス・世代を動かさず
/// 任意パスのテキストだけを返す（開文書優先・未保存編集込み・ディスク fallback）。
async fn execute_read(path: &str) -> io::Result<ReadOutcome> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = Command::ReadPath {
        path: conn::absolutize(path),
    };
    let mut line = serde_json::to_string(&command).expect("the command must be serializable");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::ReadPath {
            path,
            generation,
            checksum,
            text,
            error,
        }) => Ok(ReadOutcome {
            path,
            generation,
            checksum,
            text,
            error,
        }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "ReadPath returned a snapshot response (old daemon: rebuild)",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::ReviewComments { .. })
        | Ok(ServerMessage::SearchMatches { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {
            Err(invalid("ReadPath: unexpected lightweight response"))
        }
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
    }
}

/// `minas search` の結果（[`ServerMessage::SearchMatches`] の展開形）。
struct SearchOutcome {
    path: String,
    generation: u64,
    /// 検索対象テキストの revision（fnv1a64。ADR-0063）。
    checksum: u64,
    total: usize,
    truncated: bool,
    matches: Vec<mina_protocol::SearchMatch>,
    error: Option<String>,
}

/// daemon に接続し、パス指定のリテラル一致検索（[`ServerMessage::SearchMatches`]）
/// を受け取る。テキスト解決は read と同じ（開文書優先・未保存編集込み・ディスク
/// fallback）— 全文は運ばれない。
async fn execute_search(
    path: &str,
    query: &str,
    case_sensitive: bool,
    word: bool,
) -> io::Result<SearchOutcome> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = Command::SearchMatches {
        path: conn::absolutize(path),
        query: query.to_string(),
        case_sensitive,
        word,
    };
    let mut line = serde_json::to_string(&command).expect("the command must be serializable");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::SearchMatches {
            path,
            generation,
            checksum,
            total,
            truncated,
            matches,
            error,
        }) => Ok(SearchOutcome {
            path,
            generation,
            checksum,
            total,
            truncated,
            matches,
            error,
        }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "SearchMatches returned a snapshot response (old daemon: rebuild)",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::ReviewComments { .. })
        | Ok(ServerMessage::ReadPath { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. }) => {
            Err(invalid("SearchMatches: unexpected lightweight response"))
        }
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
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
    let mut line = serde_json::to_string(&command).expect("the command must be serializable");
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
            checksum,
            error,
        }) => Ok(serde_json::json!({
            "path": path,
            "name": name,
            "kind": kind,
            "range": range,
            "selection_range": selection_range,
            "found": found,
            "checksum": checksum,
            "error": error,
        })),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "EnclosingSymbol returned a snapshot response (old daemon: rebuild)",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::ReadPath { .. })
        | Ok(ServerMessage::SearchMatches { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::ReviewComments { .. }) => {
            Err(invalid("EnclosingSymbol: unexpected lightweight response"))
        }
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
    }
}

/// `session hover` の結果（[`ServerMessage::Hover`] の展開形。ADR-0032）。
struct HoverOutcome {
    path: String,
    /// このテキストの revision（fnv1a64。ADR-0063）。
    checksum: u64,
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
    /// クリーン（空）が解析完了の確認済みか（ADR-0045）。false = クリーン未確認。
    settled: bool,
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
    let mut line = serde_json::to_string(&command).expect("the command must be serializable");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::Hover {
            path,
            checksum,
            text,
            error,
            ..
        }) => Ok(HoverOutcome {
            path,
            checksum,
            text,
            error,
        }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "Hover returned a snapshot response (old daemon: rebuild)",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::ReadPath { .. })
        | Ok(ServerMessage::SearchMatches { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::ReviewComments { .. }) => Err(invalid("Hover: unexpected lightweight response")),
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
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
    let mut line = serde_json::to_string(&command).expect("the command must be serializable");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::WorkspaceSymbols { symbols, error, .. }) => Ok(SymbolOutcome { symbols, error }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "WorkspaceSymbol returned a snapshot response (old daemon: rebuild)",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::ReadPath { .. })
        | Ok(ServerMessage::SearchMatches { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::Check { .. })
        | Ok(ServerMessage::ReviewComments { .. }) => {
            Err(invalid("WorkspaceSymbol: unexpected lightweight response"))
        }
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
    }
}

/// daemon に接続し、対象パスの診断を軽量応答（[`ServerMessage::Check`]）で受け取る
/// （ADR-0032）。全文は運ばれない。
async fn execute_check(path: &str) -> io::Result<CheckOutcome> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = Command::CheckDiagnostics {
        path: conn::absolutize(path),
    };
    let mut line = serde_json::to_string(&command).expect("the command must be serializable");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::Check {
            path,
            total,
            diagnostics,
            settled,
            error,
            ..
        }) => Ok(CheckOutcome {
            path,
            total,
            diagnostics,
            settled,
            error,
        }),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "Check returned a snapshot response (old daemon: rebuild)",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::ReadPath { .. })
        | Ok(ServerMessage::SearchMatches { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::ReviewComments { .. }) => {
            Err(invalid("Check: unexpected lightweight response"))
        }
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
    }
}

/// daemon に接続し、レビューコメントの全文付き一覧を軽量応答
/// （[`ServerMessage::ReviewComments`]）で受け取る（#50）。全文は運ばれない。
async fn execute_reviews() -> io::Result<Vec<ReviewCommentView>> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let mut line = serde_json::to_string(&Command::ListReviewComments).expect("the command must be serializable");
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    match serde_json::from_str::<ServerMessage>(&response) {
        Ok(ServerMessage::ReviewComments { comments, .. }) => Ok(comments),
        Ok(ServerMessage::Response { .. }) | Ok(ServerMessage::Push { .. }) => Err(invalid(
            "Review returned a snapshot response (old daemon: rebuild)",
        )),
        Ok(ServerMessage::Hints { .. })
        | Ok(ServerMessage::Peek { .. })
        | Ok(ServerMessage::ServerInfo { .. })
        | Ok(ServerMessage::RenameResult { .. })
        | Ok(ServerMessage::ReferencesResult { .. })
        | Ok(ServerMessage::Outline { .. })
        | Ok(ServerMessage::ReadPath { .. })
        | Ok(ServerMessage::SearchMatches { .. })
        | Ok(ServerMessage::EnclosingSymbol { .. })
        | Ok(ServerMessage::Hover { .. })
        | Ok(ServerMessage::WorkspaceSymbols { .. })
        | Ok(ServerMessage::Check { .. }) => {
            Err(invalid("Review: unexpected lightweight response"))
        }
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
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
    let mut line = serde_json::to_string(&Command::GetServerInfo).expect("the command must be serializable");
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
            base_roots,
            servers,
        }) => Ok(serde_json::json!({
            "generation": generation,
            "daemon_build_ts": daemon_build_ts,
            // #49: 登録中の基準 root（表示用）。
            "base_roots": base_roots,
            // I4: CLI 側のビルド世代も開示する（D1 のクライアント側バリアント —
            // 再ビルド後に release バイナリが古いまま、という事故の検知）。build.rs が
            // 注入した MINA_GIT_HASH / MINA_BUILD_TS を読む。
            "cli_generation": option_env!("MINA_GIT_HASH").unwrap_or("unknown"),
            "cli_build_ts": option_env!("MINA_BUILD_TS")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0),
            "metrics": metrics,
            // rust2 要望: 設定済み LSP サーバと稼働状況（id / 言語 / セッション数）。
            // `check` が "no LSP server configured" と言った理由を自己診断できる。
            "servers": servers,
        })),
        Ok(mina_protocol::ServerMessage::Response { .. })
        | Ok(mina_protocol::ServerMessage::Push { .. }) => {
            Err(invalid("GetServerInfo returned a snapshot response (old daemon: rebuild)"))
        }
        Ok(mina_protocol::ServerMessage::Hints { .. })
        | Ok(mina_protocol::ServerMessage::Peek { .. })
        | Ok(mina_protocol::ServerMessage::Outline { .. })
        | Ok(mina_protocol::ServerMessage::ReadPath { .. })
        | Ok(mina_protocol::ServerMessage::Hover { .. })
        | Ok(mina_protocol::ServerMessage::WorkspaceSymbols { .. })
        | Ok(mina_protocol::ServerMessage::Check { .. })
        | Ok(mina_protocol::ServerMessage::EnclosingSymbol { .. })
        | Ok(mina_protocol::ServerMessage::SearchMatches { .. })
        | Ok(mina_protocol::ServerMessage::ReviewComments { .. }) => {
            Err(invalid("GetServerInfo: unexpected lightweight response"))
        }
        Ok(mina_protocol::ServerMessage::RenameResult { .. })
        | Ok(mina_protocol::ServerMessage::ReferencesResult { .. }) => {
            Err(invalid("GetServerInfo: unexpected lightweight response"))
        }
        Err(e) => Err(invalid(format!("invalid response: {e}"))),
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

/// `minas exec` の生経路: 任意の [`Command`] を送り、返った [`ServerMessage`] を
/// そのまま出力する。
///
/// - `Response`（スナップショット）は従来どおり snapshot 本体を pretty JSON で
///   出力する（後方互換）
/// - 軽量応答（Hints / Peek / ReviewComments / ServerInfo / Outline …）は応答
///   JSON を出力する — rust2 #26: 許可リストにある `ListReviewComments` /
///   `GetServerInfo` が汎用 `execute`（スナップショット前提）で
///   "unexpected semantic response" になっていた
/// - headless ゲート拒否は status にだけ載って exit 0 になっていたので、定数
///   接頭辞を検出して exit 1（stderr に理由）にする
async fn exec_raw(command: &Command) -> io::Result<()> {
    let (mut write_half, mut reader) = open_one_shot().await?;
    let command = absolutize_paths(command.clone());
    let mut line = serde_json::to_string(&command).map_err(|e| invalid(e.to_string()))?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    let message: ServerMessage = serde_json::from_str(&response)
        .map_err(|e| invalid(format!("invalid response: {e}")))?;
    match message {
        ServerMessage::Response { snapshot } => {
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
            if let Some(status) = snapshot.status.as_deref() {
                if headless_gate_rejected(status) {
                    eprintln!("exec: {status}");
                    std::process::exit(1);
                }
            }
        }
        ServerMessage::Push { .. } => return Err(invalid("unexpected push response")),
        other => println!("{}", serde_json::to_string_pretty(&other)?),
    }
    Ok(())
}

/// headless ゲート拒否の status か（[`mina_protocol::HEADLESS_GATE_STATUS_PREFIX`]）。
fn headless_gate_rejected(status: &str) -> bool {
    status.starts_with(mina_protocol::HEADLESS_GATE_STATUS_PREFIX)
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
    pair: Vec<PathBuf>,
    allow_empty: bool,
) -> io::Result<()> {
    // --hunks-stdin: 複数編集を 1 プロセス・1 接続で適用（ラウンドトリップ削減）。
    // 他入力指定と排他。
    if hunks_stdin {
        if old.is_some() || new.is_some() || whole.is_some() || old_file.is_some() || new_file.is_some() || !pair.is_empty() {
            return Err(invalid("--hunks-stdin cannot be combined with other input options"));
        }
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        let hunks = parse_hunks(&buf)?;
        return apply_hunks(path, &hunks).await;
    }

    // --pair OLD_FILE NEW_FILE [x N]: テキストを JSON エスケープせずファイルで
    // 渡す --hunks-stdin 相当（--old-file/--new-file の複数版。原子性は同じ）。
    if !pair.is_empty() {
        if old.is_some() || new.is_some() || whole.is_some() || old_file.is_some() || new_file.is_some() {
            return Err(invalid("--pair cannot be combined with other input options"));
        }
        let hunks = pairs_to_hunks(&pair)?;
        if hunks.is_empty() {
            return Err(invalid("--pair takes OLD_FILE NEW_FILE pairs"));
        }
        return apply_hunks(path, &hunks).await;
    }

    let (old_text, new_text) = resolve_apply_args(old, new, whole, whole_stdin, old_file, new_file)?;

    let (mut write_half, mut reader) = open_one_shot().await?;

    let abs = conn::absolutize(&path.to_string_lossy());
    let mut snapshot = conn::request(&mut write_half, &mut reader, &Command::Open { path: abs.clone() }).await?;
    // 未存在パス: 空ファイルを作成してから再 Open し、Save で新規作成する
    // （#28/#29: 新規ファイルを直接アドレスする現実解。report 3-1 の touch→open
    // 2段階ハックをコマンド側に内包。自動オープンの範囲外判断（#28）には触れない）。
    // 失敗経路（NOT FOUND / EDIT REJECTED / SAVE FAILED / 再 Open 失敗）では、ここで
    // 作った空ファイルを rollback_created で消し、作成前（未存在）に戻す。失敗した
    // apply が 0 バイトのファイルを残すと、次の Open が「在るが空」を掴んで NOT FOUND の
    // 原因が見えなくなる。
    let mut created = false;
    if snapshot.status.is_some() && std::fs::metadata(&abs).is_err() {
        // 新規作成: 親ディレクトリが無ければ作る（`apply src/domain/mod.rs
        // --whole-stdin` で domain/ も無いケース — rust2 フィードバック #1-2）。
        if let Some(parent) = std::path::Path::new(&abs).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| invalid(format!("failed to create the parent directory: {}: {e}", parent.display())))?;
        }
        std::fs::write(&abs, "")
            .map_err(|e| invalid(format!("failed to create the file: {abs}: {e}")))?;
        created = true;
        snapshot = conn::request(&mut write_half, &mut reader, &Command::Open { path: abs.clone() }).await?;
    }
    if let Some(status) = &snapshot.status {
        rollback_created(&abs, created);
        return Err(invalid(format!("open failed: {status}")));
    }
    let text = snapshot.text.clone();

    // 空全文置換のガード（rust2 #5-1）: `--whole-stdin` の stdin を繋ぎ忘れると
    // 非空ファイルが 0 バイトに切り詰められる（exit 0 だった）— 破壊的。
    // 空 text による truncate は `--allow-empty` で明示的にのみ許可する。
    // （置換は必ず非空の old を持つのでこのガードは全文置換のみに効く。）
    if !allow_empty && old_text.is_none() && new_text.is_empty() && !text.is_empty() {
        rollback_created(&abs, created);
        eprintln!(
            "REFUSED: whole-file replacement with empty text would truncate {abs} \
             ({} chars). If that is intended, pass --allow-empty.",
            text.chars().count()
        );
        std::process::exit(2);
    }

    // char インデックスで位置を計算する（DocumentEdit は char 単位）:
    // byte インデックスを渡すとマルチバイト文書でずれる。
    let (start, end, expected) = match &old_text {
        Some(old) => match find_range(&text, old) {
            Some((start, end)) => (start, end, Some(old.clone())),
            // 探索失敗は再試行可能な失敗として exit 2（`session edit` の拒否と同格）
            None => {
                eprintln!("NOT FOUND: {:?}", short(old));
                rollback_created(&abs, created);
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
        rollback_created(&abs, created);
        std::process::exit(2);
    }

    let snapshot = conn::request(&mut write_half, &mut reader, &Command::Save).await?;
    let saved = snapshot
        .status
        .as_deref()
        .is_some_and(|s| s.starts_with("saved"));
    if !saved {
        // 理由は daemon の status にそのまま入っている（コード付き: `save-rejected:`
        // など）。`{:?}` で Some("…") に包むと、エージェントが読む行に Rust の
        // デバッグ表現が混ざる（ドッグフーディング #15）。
        match snapshot.status.as_deref() {
            Some(s) => eprintln!("SAVE FAILED: {s}"),
            None => eprintln!("SAVE FAILED: no status"),
        }
        rollback_created(&abs, created);
        std::process::exit(2);
    }
    // no-op（old == new / 同一内容の全文置換）は daemon が status に載せない（ADR-0012）。
    // 「成功」と読めてしまうので stderr に明示する（exit は 0 のまま — 冪等な編集）。
    if snapshot.text == text {
        eprintln!("NO-OP: the replacement left the buffer unchanged");
    }
    match &old_text {
        Some(_) => println!("applied: {abs} (chars {start}..{end})"),
        None => println!("applied: {abs} (whole file, {} chars)", new_text.chars().count()),
    }
    Ok(())
}

/// 失敗経路の後始末: apply が新規作成した空ファイルを消し、作成前（未存在）に戻す。
/// 作成していなければ何もしない。成功経路では呼ばない。
fn rollback_created(path: &str, created: bool) {
    if created {
        let _ = std::fs::remove_file(path);
    }
}

/// `text` 中の `old` の最初の出現位置を char インデックスで返す（[`DocumentEdit`]
/// は char 単位のため、byte ではなく char で数える）。
fn find_range(text: &str, old: &str) -> Option<(usize, usize)> {
    let byte_idx = text.find(old)?;
    let start = text[..byte_idx].chars().count();
    Some((start, start + old.chars().count()))
}

/// パスが言語サーバの解析対象プロジェクトに属するか（祖先ディレクトリに root
/// マーカーがあるか）。
///
/// minad の `GENERIC_ROOT_MARKERS`（minad/src/languages.rs）を鏡写しにした
/// ヒューリスティック。false のときは「プロジェクト外なので LSP に解析対象が
/// 無く、pull が空を返す」— 診断なし＝クリーンではない（rust2 #21: /tmp の
/// スクラッチファイルは構文エラーがあっても total 0 / settled false になる）。
/// languages.toml で独自 root-markers を設定した場合は偽陰性になり得るが、
/// verdict は「未検証」で安全側。
fn in_project(path: &str) -> bool {
    project_root(path).is_some()
}

/// そのパスが属するプロジェクトの root（最も近いマーカーのディレクトリ）を返す。
/// マーカーが無ければ `None`（プロジェクト外）。
///
/// `in_project` の真偽だけでなく**どのプロジェクトか**が要る場面のために分けた
/// （self-host #2/#7: ワークスペース外の依存ソースや別プロジェクトのファイルを
/// 「自分のプロジェクトの答え」として返さないため）。
fn project_root(path: &str) -> Option<String> {
    project_root_of_dir(std::path::Path::new(path).parent()?)
}

/// ディレクトリ**自身**から上方探索してプロジェクト root を返す（`project_root` の
/// ファイル版と、cwd のような「ディレクトリを渡したい」場面の両方に使う）。
fn project_root_of_dir(dir: &std::path::Path) -> Option<String> {
    const MARKERS: &[&str] = &[
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        ".git",
    ];
    let mut d = Some(dir.to_path_buf());
    while let Some(cur) = d {
        if MARKERS.iter().any(|m| cur.join(m).exists()) {
            return Some(conn::absolutize(&cur.to_string_lossy()));
        }
        d = cur.parent().map(std::path::Path::to_path_buf);
    }
    None
}

/// `symbol` のヒットがアンカーファイルのプロジェクト外なら stderr で注記する
/// （self-host #2: `minas symbol <daemon.rs> save` が `~/.cargo/registry/…/smallvec`
/// を返し、docs は「workspace root 内」と読めた。パスが唯一の手掛かりなので、
/// 黙って返さない）。
fn note_out_of_project_hits(anchor: &std::path::Path, symbols: &[mina_protocol::WorkspaceSymbol]) {
    let Some(root) = project_root(&anchor.to_string_lossy()) else {
        return;
    };
    let outside: Vec<&str> = symbols
        .iter()
        .map(|s| s.path.as_str())
        .filter(|p| !conn::absolutize(p).starts_with(&root))
        .collect();
    if outside.is_empty() {
        return;
    }
    eprintln!(
        "note: {} of {} hit(s) are outside the project root {root} (dependency sources?) — \
         check the path before trusting them:",
        outside.len(),
        symbols.len()
    );
    for p in outside.iter().take(5) {
        eprintln!("  {p}");
    }
}

/// `dir` 配下の `.rs` ファイルを再帰収集する（check --crate-root 用）。
/// `target` / `.git` / ドットディレクトリは除外（生成物・メタを混ぜない）。
/// 並び順は探索順（read_dir の順） — 呼び出し側で必要ならソートする。
fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target"
                && name != ".git"
                && !name.to_string_lossy().starts_with('.')
            {
                collect_rs_files(&path, out);
            }
        } else if !name.to_string_lossy().starts_with('.')
            && path.extension().is_some_and(|e| e == "rs")
        {
            out.push(path);
        }
    }
}

/// 複数 hunk を順に適用した結果をバッファ内で事前再現し、全 old が検出できるか
/// を検証する。失敗したら (hunk 番号, 見つからなかった old) を返し、成功なら
/// `None`。
///
/// daemon 側の適用（最初の出現を置換・char 単位）と同一の規則で再現するため、
/// これが通れば実適用も通る（`expected_text` 検証は同じ位置で一致する）。
/// [`apply_hunks`] はこれを edit 送信前に回し、失敗なら 1 件も送らない —
/// 「先行 hunk はバッファに適用済み・ディスクは無傷」の phantom 不整合を作らない。
fn simulate_hunks(text: &str, hunks: &[Hunk]) -> Option<(usize, String)> {
    let mut text = text.to_string();
    for (i, hunk) in hunks.iter().enumerate() {
        // byte 境界は find が保証（old は UTF-8 境界で一致する）。
        let Some(start) = text.find(&hunk.old) else {
            return Some((i, hunk.old.clone()));
        };
        text.replace_range(start..start + hunk.old.len(), &hunk.new);
    }
    None
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
            return Err(invalid("--whole-stdin cannot be combined with other input options"));
        }
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        return Ok((None, buf));
    }
    if let Some(w) = whole {
        if old.is_some() || new.is_some() || old_file.is_some() || new_file.is_some() {
            return Err(invalid("--whole cannot be combined with other input options"));
        }
        return Ok((None, w));
    }
    let old = match old_file {
        Some(f) => {
            if old.is_some() {
                return Err(invalid("old and --old-file cannot be combined"));
            }
            std::fs::read_to_string(&f).map_err(|e| invalid(format!("--old-file {f:?}: {e}")))?
        }
        None => old.ok_or_else(|| invalid("the text to replace is required (old, or --whole / --whole-stdin / --old-file)"))?,
    };
    let new = match new_file {
        Some(f) => {
            if new.is_some() {
                return Err(invalid("new and --new-file cannot be combined"));
            }
            std::fs::read_to_string(&f).map_err(|e| invalid(format!("--new-file {f:?}: {e}")))?
        }
        None => new.ok_or_else(|| invalid("the replacement text is required (new, or --new-file)"))?,
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
        .map_err(|e| invalid(format!("cannot parse the --hunks-stdin JSON: {e}")))?;
    if hunks.is_empty() {
        return Err(invalid("--hunks-stdin requires at least one hunk"));
    }
    if hunks.iter().any(|h| h.old.is_empty()) {
        return Err(invalid("hunk.old cannot be empty (the text to replace is required)"));
    }
    Ok(hunks)
}

/// `--pair` のファイル群を hunk 列に変換する（偶数個の保証・ファイルの存在
/// 検証は呼び出し側で行う — この関数は入力の整形だけ担う）。
fn pairs_to_hunks(pair: &[PathBuf]) -> io::Result<Vec<Hunk>> {
    if pair.len() % 2 != 0 {
        return Err(invalid("--pair takes OLD_FILE NEW_FILE pairs"));
    }
    let mut hunks = Vec::with_capacity(pair.len() / 2);
    for chunk in pair.chunks_exact(2) {
        let o = std::fs::read_to_string(&chunk[0])
            .map_err(|e| invalid(format!("--pair old-file {:?}: {e}", chunk[0])))?;
        let n = std::fs::read_to_string(&chunk[1])
            .map_err(|e| invalid(format!("--pair new-file {:?}: {e}", chunk[1])))?;
        if o.is_empty() {
            return Err(invalid("the --pair old file is empty (an empty old cannot be distinguished from a whole-file replacement)"));
        }
        hunks.push(Hunk { old: o, new: n });
    }
    Ok(hunks)
}

/// `--hunks-stdin` / `--pair` の実体: 複数編集を 1 接続で順次適用し、最後に
/// 一度だけ Save する。途中で `old` 未発見や daemon 拒否なら exit 2（Save 前な
/// のでディスクは無変更 — 単発 apply の失敗契約を複数に一般化したもの）。
///
/// 原子性はディスクだけでなくバッファにも及ぶ: 適用前に [`simulate_hunks`] で
/// 全 hunk を検証し、1 つでも old が見つからなければ edit を 1 件も送らない。
/// さもないと「先行 hunk はバッファに適用済み・ディスクは無傷」の不整合が残り、
/// `minas read` が phantom テキストを返す（rust2 バグ報告 — read は ADR-0048 で
/// 開文書優先・Open は #7 で既存文書を再読込しないため Close でも直らない）。
async fn apply_hunks(path: &PathBuf, hunks: &[Hunk]) -> io::Result<()> {
    let (mut write_half, mut reader) = open_one_shot().await?;

    let abs = conn::absolutize(&path.to_string_lossy());
    let mut snapshot = conn::request(
        &mut write_half,
        &mut reader,
        &Command::Open { path: abs.clone() },
    )
    .await?;
    // 既存 apply と同じ: 未存在パスは空ファイルを touch して再 Open（Save で新規作成）。
    // 失敗経路では rollback_created で作成前（未存在）に戻す。
    let mut created = false;
    if snapshot.status.is_some() && std::fs::metadata(&abs).is_err() {
        // 新規作成: 親ディレクトリが無ければ作る（apply 単発と同じ扱い）。
        if let Some(parent) = std::path::Path::new(&abs).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| invalid(format!("failed to create the parent directory: {}: {e}", parent.display())))?;
        }
        std::fs::write(&abs, "")
            .map_err(|e| invalid(format!("failed to create the file: {abs}: {e}")))?;
        created = true;
        snapshot = conn::request(
            &mut write_half,
            &mut reader,
            &Command::Open { path: abs.clone() },
        )
        .await?;
    }
    if let Some(status) = &snapshot.status {
        rollback_created(&abs, created);
        return Err(invalid(format!("open failed: {status}")));
    }

    // バッファ原子性（rust2 バグ報告）: edit を 1 件も送る前に、全 hunk の old が
    // 順に検出できるかを検証する。途中で見つからない hunk があれば exit 2 で
    // 何も送らない — 先行 hunk がバッファに適用済み・ディスクは無傷という
    // phantom 状態を作らない（read/Open は開文書を返すため、Close でも直らない）。
    if let Some((idx, missing)) = simulate_hunks(&snapshot.text, hunks) {
        eprintln!("NOT FOUND: {:?} (hunk #{})", short(&missing), idx + 1);
        rollback_created(&abs, created);
        std::process::exit(2);
    }

    let mut applied = 0usize;
    let mut noops = 0usize;
    for (i, hunk) in hunks.iter().enumerate() {
        // 恒等 hunk（old == new）はエージェントのミス（同じものを送った）— daemon
        // に送る前にはっきり区別する（rust2 #10: 「unchanged」は「old==new で何も
        // しない」と「置換して結果が同じ」の 2 意味に読めた。SKIPPED なら自明）。
        // カウントは noops に含める（edits_noop > 0 で「何かが no-op だった」と
        // 気づける — 適用漏れとの区別は SKIPPED 行が担う）。
        if hunk.old == hunk.new {
            noops += 1;
            eprintln!("HUNK SKIPPED: #{} (old == new)", i + 1);
            continue;
        }
        // 直前の編集適用後の現在テキストに対し位置を再計算する（行揺れを踏む。char 単位）
        let text = snapshot.text.clone();
        let Some((start, end)) = find_range(&text, &hunk.old) else {
            eprintln!("NOT FOUND: {:?}", short(&hunk.old));
            rollback_created(&abs, created); // Save 前なのでディスクは作成前の状態に戻す
            std::process::exit(2);
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
            rollback_created(&abs, created);
            std::process::exit(2);
        }
        // 置換後に変わらなかった（old != new なのに同一 = 理論上ほぼ起きない
        // 防御。恒等 hunk は上の SKIPPED で捕捉済み — ADR-0012: no-op は世代を
        // 進めない）。一括適用では 1 件の静かな no-op が「N 件適用」に埋もれる
        // ので、件数と該当 hunk を明示する。
        if snapshot.text == text {
            noops += 1;
            eprintln!("HUNK NO-OP: #{} (changed nothing) {:?}", i + 1, short(&hunk.old));
        }
        applied += 1;
    }

    let snapshot = conn::request(&mut write_half, &mut reader, &Command::Save).await?;
    let saved = snapshot
        .status
        .as_deref()
        .is_some_and(|s| s.starts_with("saved"));
    if !saved {
        // 理由は daemon の status にそのまま入っている（コード付き: `save-rejected:`
        // など）。`{:?}` で Some("…") に包むと、エージェントが読む行に Rust の
        // デバッグ表現が混ざる（ドッグフーディング #15）。
        match snapshot.status.as_deref() {
            Some(s) => eprintln!("SAVE FAILED: {s}"),
            None => eprintln!("SAVE FAILED: no status"),
        }
        rollback_created(&abs, created);
        std::process::exit(2);
    }
    // Q3: 成功時に generation を返す（エージェントは wait <generation> を直接呼べる）
    println!(
        "{}",
        serde_json::json!({
            "applied": abs,
            "edits": applied,
            "edits_noop": noops,
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
    fn headless_gate_rejection_is_detected() {
        // rust2 #26: 拒否は status にだけ載っていた — exec は接頭辞を検出して exit 1。
        assert!(headless_gate_rejected(&format!(
            "{} GetState, Save",
            mina_protocol::HEADLESS_GATE_STATUS_PREFIX
        )));
        assert!(!headless_gate_rejected("reloaded from disk"));
        assert!(!headless_gate_rejected(""));
    }

    #[test]
    fn check_exit_codes_distinguish_retryable() {
        // rust2 #23: not supported / cannot open は再試行不可（1）、LSP 失敗は 2。
        assert_eq!(check_exit_code("check not supported for /x.md (no LSP server configured)"), 1);
        assert_eq!(check_exit_code("cannot open /x.rs"), 1);
        assert_eq!(check_exit_code("invalid input"), 1);
        assert_eq!(
            check_exit_code("diagnostics pull failed: the session lock is held"),
            2,
            "再試行可能な LSP 失敗は 2"
        );
        // 集約: error 診断がなければ各パスの分類をそのまま返す（以前は常に 2 以上に
        // なって 1 が握り潰されていた）。
        assert_eq!(check_final_exit_code(false, 0), 0);
        assert_eq!(check_final_exit_code(false, 1), 1, "not supported は 1 のまま");
        assert_eq!(check_final_exit_code(false, 2), 2);
        assert_eq!(check_final_exit_code(true, 1), 2, "error 診断があれば 2");
    }

    #[test]
    fn every_lsp_command_treats_unopenable_path_as_input_error() {
        // ドッグフーディング #2: 存在しない / ディレクトリ / バイナリは「開けない」で
        // 何度投げても同じ。check だけが 1 で他が 2 だった（エージェントは
        // 存在しないパスを無限再試行する）。全コマンドで 1 に揃える。
        let classifiers: [(&str, fn(&str) -> i32); 7] = [
            ("rename", rename_exit_code),
            ("references", references_exit_code),
            ("outline", outline_exit_code),
            ("at", symbol_at_exit_code),
            ("hover", hover_exit_code),
            ("symbol", symbol_exit_code),
            ("check", check_exit_code),
        ];
        for (name, f) in classifiers {
            for msg in [
                "cannot open /x/missing.rs",
                "cannot open /x/dir: not a file, is a directory",
                "cannot open /x/bin.dat: not valid UTF-8 (binary file?)",
                "file too large: /x/big.rs",
                "invalid input: depth must be >= 1",
            ] {
                assert_eq!(f(msg), 1, "{name} は入力エラーを 1 にする: {msg}");
            }
            // LSP の一時的な失敗は再試行可能な 2 のまま。
            assert_eq!(f("LSP error: request timed out"), 2, "{name}: LSP 失敗は 2");
        }
    }

    #[test]
    fn unlinked_file_hint_is_detected_in_both_wordings() {
        // ドッグフーディング #3: 新規作成直後の窓では rust-analyzer は
        // 「どのクレートにも属していない」Hint だけを返す。verdict を hints (n) に
        // すると exit 0 = 偽グリーンになるので、unlinked として検出する。
        let d = |msg: &str| mina_protocol::CheckDiagnostic {
            severity: Severity::Hint,
            line: 1,
            col: 1,
            start: 0,
            end: 1,
            message: msg.to_string(),
        };
        assert!(is_unlinked_file(&d(
            "This file is not included in any crates, so rust-analyzer can't offer IDE \
             services.\n\nIf you're intentionally working on unowned files, you can silence \
             this warning by adding \"unlinked-file\" to rust-analyzer.diagnostics.disabled"
        )));
        assert!(is_unlinked_file(&d(
            "This file is not included anywhere in the module tree, so rust-analyzer can't \
             offer IDE services."
        )));
        assert!(!is_unlinked_file(&d("expected u64, found &'static str")));
        assert!(!is_unlinked_file(&d("unused variable: `x`")));
    }

    #[test]
    fn delete_exit_codes_distinguish_input_errors() {
        // ADR-0060: cannot delete / read-only-base は再試行不可（1）、
        // delete-rejected（未保存編集）は呼び出し側が直せる（2）。
        assert_eq!(delete_exit_code("cannot delete /x.rs: not a regular file"), 1);
        assert_eq!(
            delete_exit_code("read-only-base: /x.rs is under read-only base abc1234 …"),
            1
        );
        assert_eq!(
            delete_exit_code("delete-rejected: /x.rs has unsaved edits in the daemon's buffer …"),
            2,
            "未保存編集の拒否は、閉じれば通るので再試行可能"
        );
        assert_eq!(delete_exit_code("no such thing"), 2);
    }

    #[test]
    fn in_project_detects_root_markers() {
        // rust2 #21: プロジェクト外（マーカーなし）は unlinked-unverified にする。
        let base = std::env::temp_dir().join(format!("inproj-{}", std::process::id()));
        let proj = base.join("proj");
        std::fs::create_dir_all(proj.join("src")).unwrap();
        std::fs::write(proj.join("Cargo.toml"), "").unwrap();
        std::fs::write(proj.join("src/lib.rs"), "").unwrap();
        let scratch = base.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("x.rs"), "").unwrap();

        assert!(
            in_project(proj.join("src/lib.rs").to_str().unwrap()),
            "Cargo.toml を祖先に持つファイルはプロジェクト内"
        );
        assert!(
            !in_project(scratch.join("x.rs").to_str().unwrap()),
            "マーカーなしはプロジェクト外"
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn collect_rs_files_walks_tree_and_skips_target() {
        let dir = std::env::temp_dir().join(format!("collect-rs-{}", std::process::id()));
        let sub = dir.join("src");
        std::fs::create_dir_all(sub.join("domain")).unwrap();
        std::fs::create_dir_all(sub.join("target")).unwrap();
        std::fs::write(sub.join("lib.rs"), "").unwrap();
        std::fs::write(sub.join("domain/mod.rs"), "").unwrap();
        std::fs::write(sub.join("domain/todo.rs"), "").unwrap();
        std::fs::write(sub.join("target/ignored.rs"), "").unwrap();
        std::fs::write(sub.join(".hidden.rs"), "").unwrap();
        std::fs::write(sub.join("main.txt"), "").unwrap();

        let mut out = Vec::new();
        collect_rs_files(&sub, &mut out);
        out.sort();
        let names: Vec<String> = out
            .iter()
            .map(|p| p.strip_prefix(&sub).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["domain/mod.rs", "domain/todo.rs", "lib.rs"],
            "target / .dot は除外、非 .rs も除外"
        );

        // 空 / 存在しないディレクトリは空結果（クラッシュしない）
        let mut none = Vec::new();
        collect_rs_files(&dir.join("nope"), &mut none);
        assert!(none.is_empty());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn simulate_hunks_verifies_all_hunks_before_apply() {
        // 全 hunk の old が順に検出できるなら None。途中で見つからない hunk が
        // あれば (番号, old) を返す — edit を送る前の事前検証（phantom 対応）。
        let h = |old: &str, new: &str| Hunk { old: old.into(), new: new.into() };

        let ok = simulate_hunks(
            "fn a() {}\nfn b() {}\n",
            &[h("fn a() {}", "fn a() { /* t */ }"), h("fn b() {}", "fn b() { /* t */ }")],
        );
        assert!(ok.is_none(), "全 hunk 検出可能なら成功");

        // 同一 old の反復（rust2 #4-2: 5 箇所の同一リテラルを 5 hunk で置換）:
        // 置換後テキストに対し順に検出されるので、出現数と同じ個数なら成功。
        let dup = simulate_hunks(
            "fn t() { x; x; x; }",
            &[h("x", "y"), h("x", "y"), h("x", "y")],
        );
        assert!(dup.is_none(), "同一 old の反復は出現順に当たる");

        // 2 番目の hunk が検出不可 → (1, old) を返す。
        let miss = simulate_hunks(
            "fn a() {}\nfn b() {}\n",
            &[h("fn a() {}", "A"), h("NOT_PRESENT", "x")],
        );
        let (idx, old) = miss.expect("検出不可の hunk は失敗を返す");
        assert_eq!(idx, 1);
        assert_eq!(old, "NOT_PRESENT");

        // 1 hunk 目から見つからない場合も同様。
        let miss_first = simulate_hunks("abc", &[h("zzz", "x")]);
        assert_eq!(miss_first.unwrap().0, 0);

        // 置換で先行 hunk の old が消えた後の後続 hunk も検出される（適用順の再現）。
        let chained = simulate_hunks("a", &[h("a", "b"), h("b", "c")]);
        assert!(chained.is_none(), "置換結果に対する後続 hunk も検出");
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
    fn pairs_to_hunks_reads_files_in_pairs() {
        // --pair: 偶数個はファイル読み → 順序保持で hunk 列に。奇数個・空 old は拒否。
        let dir = std::env::temp_dir();
        let o1 = dir.join(format!("pairtest-o1-{}", std::process::id()));
        let n1 = dir.join(format!("pairtest-n1-{}", std::process::id()));
        let o2 = dir.join(format!("pairtest-o2-{}", std::process::id()));
        let n2 = dir.join(format!("pairtest-n2-{}", std::process::id()));
        std::fs::write(&o1, "old one").unwrap();
        std::fs::write(&n1, "new one").unwrap();
        std::fs::write(&o2, "old two").unwrap();
        std::fs::write(&n2, "new two").unwrap();

        let hunks =
            pairs_to_hunks(&[o1.clone(), n1.clone(), o2.clone(), n2.clone()]).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old, "old one");
        assert_eq!(hunks[0].new, "new one");
        assert_eq!(hunks[1].old, "old two");
        assert_eq!(hunks[1].new, "new two");

        let odd = pairs_to_hunks(&[o1.clone(), n1.clone(), o2.clone()]);
        assert!(odd.is_err(), "奇数個は拒否");

        let empty = std::env::temp_dir().join(format!("pairtest-empty-{}", std::process::id()));
        std::fs::write(&empty, "").unwrap();
        let empty_old = pairs_to_hunks(&[empty.clone(), n1.clone()]);
        assert!(empty_old.is_err(), "空 old は拒否");

        let missing = std::env::temp_dir().join(format!("pairtest-missing-{}", std::process::id()));
        assert!(pairs_to_hunks(&[missing, n1.clone()]).is_err(), "存在しないファイルは拒否");

        for p in [o1, n1, o2, n2, empty] {
            let _ = std::fs::remove_file(p);
        }
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
