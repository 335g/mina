//! minae-debug: `minae session edit` の試行用対話 CLI（開発用・使い捨て）。
//!
//! daemon に接続し、DocumentEdit をフォーム入力（cliclack）で適用する。
//! checksum（FNV-1a 64 全文）は毎回 `GetState` で取得した文書から自動計算する
//! ので、手計算の必要がない（Q7: 毎回取得）。
//!
//! 適用結果は status / 直近イベント / dirty / generation + 影響範囲のスニペットで
//! 表示する。拒否（checksum 不一致・範囲外）は daemon が状態を変えず status で
//! 報告するので、そのまま表示して観察できる。
//!
//! 使い方: `cargo run -p minae-debug`（daemon 未起動なら自動起動する）

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use minae_protocol::{fnv1a64, ClientKind, Command, DocumentEdit, Hello, ServerMessage, StateSnapshot};
use serde::Serialize;

/// daemon のソケットパス（minae-term の `daemon::socket_path()` と一致させること）。
fn socket_path() -> PathBuf {
    std::env::temp_dir().join("minae.sock")
}

/// メニューのアクション。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Insert,
    Delete,
    Replace,
    Get,
    Quit,
}

fn main() {
    if let Err(e) = run() {
        if e.kind() == ErrorKind::Interrupted {
            // ユーザーの Esc / Ctrl-C: エラーではない
            let _ = cliclack::outro_cancel("キャンセル");
        } else {
            let _ = cliclack::outro_cancel(format!("エラー: {e}"));
            std::process::exit(1);
        }
    } else {
        let _ = cliclack::outro("bye");
    }
}

fn run() -> std::io::Result<()> {
    cliclack::intro("minae-debug: DocumentEdit 試行ツール")?;

    let mut conn = Conn::connect()?;
    let state = conn.request(&Command::GetState)?;
    show_state_summary(&state)?;

    loop {
        let action = cliclack::select("アクション")
            .item(Action::Insert, "Insert", "start に text 挿入 (start==end)")
            .item(Action::Delete, "Delete", "[start, end) を削除 (text 空)")
            .item(Action::Replace, "Replace", "[start, end) を text で置換")
            .item(Action::Get, "GetState", "現在の状態を表示")
            .item(Action::Quit, "Quit", "終了")
            .initial_value(Action::Insert)
            .interact()?;

        match action {
            Action::Quit => break,
            Action::Get => {
                let s = conn.request(&Command::GetState)?;
                show_state_summary(&s)?;
            }
            Action::Insert => {
                let start: usize = cliclack::input("start").placeholder("0").interact()?;
                let text: String = text_input()?;
                apply(&mut conn, start, start, &text)?;
            }
            Action::Delete => {
                let start: usize = cliclack::input("start").placeholder("0").interact()?;
                let end: usize = cliclack::input("end").interact()?;
                apply(&mut conn, start, end, "")?;
            }
            Action::Replace => {
                let start: usize = cliclack::input("start").placeholder("0").interact()?;
                let end: usize = cliclack::input("end").interact()?;
                let text: String = text_input()?;
                apply(&mut conn, start, end, &text)?;
            }
        }
    }
    Ok(())
}

/// text 入力（複数行対応。Enter で改行、Esc→Enter で確定）。
fn text_input() -> std::io::Result<String> {
    cliclack::input("text").multiline().interact()
}

/// Q7: 毎回 `GetState` で文書を取り直して checksum を計算し、編集を適用する。
fn apply(conn: &mut Conn, start: usize, end: usize, text: &str) -> std::io::Result<()> {
    let before = conn.request(&Command::GetState)?;
    let edit = DocumentEdit {
        start,
        end,
        text: text.to_string(),
        checksum: fnv1a64(before.text.as_bytes()),
        expected_text: None,
    };
    let after = conn.request(&edit)?;
    report(&after, start, end, text)
}

/// 適用結果を表示する（Q6: status / 直近イベント / dirty / generation + スニペット）。
fn report(after: &StateSnapshot, start: usize, end: usize, text: &str) -> std::io::Result<()> {
    if let Some(status) = &after.status {
        // 拒否: daemon は状態を変えていない
        cliclack::log::warning(format!("拒否: {status}"))?;
        return Ok(());
    }
    cliclack::log::success(format!("適用: [{start}..{end}) → {:?}", text))?;
    let event = after.events.last().map(|e| format!("{:?}/{:?}", e.kind, e.source));
    cliclack::log::info(format!(
        "gen={} dirty={} event={}{}",
        after.generation,
        after.dirty,
        event.as_deref().unwrap_or("-"),
        if after.deleted.is_some() { " deleted" } else { "" }
    ))?;
    cliclack::note("スニペット", snippet(&after.text, start, 1))?;
    Ok(())
}

/// 状態の概要を表示する（Get アクション・起動時）。
fn show_state_summary(s: &StateSnapshot) -> std::io::Result<()> {
    cliclack::log::info(format!(
        "path={:?} mode={:?} dirty={} gen={} diagnostics={}",
        s.path,
        s.mode,
        s.dirty,
        s.generation,
        s.diagnostics.len()
    ))?;
    let preview: String = s.text.lines().take(10).collect::<Vec<_>>().join("\n");
    let preview = if s.text.lines().count() > 10 {
        format!("{preview}\n...")
    } else {
        preview
    };
    cliclack::note("テキスト", preview)?;
    Ok(())
}

/// char 位置 `at` を含む行と前後 `context` 行を「行番号 + 内容」で返す。
/// 行は '\n' を除いた範囲。空テキスト・末尾（at == len）は最終行にフォールバック。
fn snippet(text: &str, at: usize, context: usize) -> String {
    let lines = line_ranges(text);
    let idx = lines
        .iter()
        .position(|(s, e)| at >= *s && at < *e)
        .unwrap_or(lines.len() - 1);
    let from = idx.saturating_sub(context);
    let to = (idx + context).min(lines.len() - 1);
    let chars: Vec<char> = text.chars().collect();
    lines[from..=to]
        .iter()
        .enumerate()
        .map(|(k, (s, e))| {
            let ln = from + k;
            let marker = if ln == idx { ">" } else { " " };
            format!("{marker} {:>3}: {}", ln + 1, chars[*s..*e].iter().collect::<String>())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// テキストを行に分割し、各行の (開始 char 位置, 終了 char 位置) を返す。
/// 末尾の改行の後の空行も含む（`"a\n"` → `[(0,1), (2,2)]`）。
fn line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, ch) in text.chars().enumerate() {
        if ch == '\n' {
            lines.push((start, i));
            start = i + 1;
        }
    }
    lines.push((start, text.chars().count()));
    lines
}

/// daemon への接続（Hello 送信済み）。リクエスト/レスポンスは NDJSON 1 行ずつ。
struct Conn {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Conn {
    /// daemon を確保（未起動なら自動起動）して接続し、Hello（Headless）を送る。
    fn connect() -> std::io::Result<Self> {
        let path = socket_path();
        ensure_daemon(&path)?;
        let stream = UnixStream::connect(&path)?;
        let reader = BufReader::new(stream.try_clone()?);
        let mut conn = Conn { stream, reader };
        let mut line = serde_json::to_string(&Hello {
            kind: ClientKind::Headless,
            reset_cursor_on_disconnect: true, // Headless には無意味（切断でリセットしない）
        })
        .expect("Hello はシリアライズ可能");
        line.push('\n');
        conn.stream.write_all(line.as_bytes())?;
        conn.stream.flush()?;
        Ok(conn)
    }

    /// メッセージを送り、応答スナップショットを 1 つ受け取る。
    /// ADR-0013: 応答はタグ付き ServerMessage エンベロープ。Headless は
    /// push を受け取らない（購読されない）が、形状に依存しないようどちらでも取り出す。
    fn request<T: Serialize>(&mut self, message: &T) -> std::io::Result<StateSnapshot> {
        let mut line = serde_json::to_string(message).map_err(io_err)?;
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        self.stream.flush()?;
        let mut response = String::new();
        self.reader.read_line(&mut response)?;
        match serde_json::from_str::<ServerMessage>(&response) {
            Ok(ServerMessage::Response { snapshot }) | Ok(ServerMessage::Push { snapshot }) => {
                Ok(snapshot)
            }
            // #23: GetInlayHints の応答はこの CLI ではまだ使わない
            Ok(ServerMessage::Hints { .. }) => Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "unexpected inlay hints response".to_string(),
            )),
            // ADR-0025: Peek 応答もこの CLI では使わない
            Ok(ServerMessage::Peek { .. }) => Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "unexpected peek response".to_string(),
            )),
            // GET サーバ情報応答を普通の状態要求として受け取った場合は応答不一致
            Ok(ServerMessage::ServerInfo { .. }) => Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "unexpected server info response".to_string(),
            )),
            // ADR-0029: Rename / References 応答もこの CLI では使わない
            Ok(ServerMessage::RenameResult { .. })
            | Ok(ServerMessage::ReferencesResult { .. })
            | Ok(ServerMessage::Outline { .. })
            | Ok(ServerMessage::EnclosingSymbol { .. })
            // ADR-0032: Hover / WorkspaceSymbols / Check 応答もこの CLI では使わない
            | Ok(ServerMessage::Hover { .. })
            | Ok(ServerMessage::WorkspaceSymbols { .. })
            | Ok(ServerMessage::Check { .. }) => {
                Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "unexpected semantic response".to_string(),
                ))
            }
            Err(e) => Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("不正な応答: {e}"),
            )),
        }
    }
}

fn io_err(e: serde_json::Error) -> std::io::Error {
    std::io::Error::new(ErrorKind::InvalidData, e)
}

/// daemon が起動していなければ `minae daemon serve` を起動して待つ。
/// `minae` バイナリは minae-debug の隣（target/debug 配下）→ PATH の順で探す。
fn ensure_daemon(path: &Path) -> std::io::Result<()> {
    if UnixStream::connect(path).is_ok() {
        return Ok(());
    }
    let exe = find_mina().ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::NotFound,
            "`minae` バイナリが見つかりません（`cargo build -p minae-term` で生成、または PATH に追加）",
        )
    })?;
    let mut cmd = ProcessCommand::new(exe);
    cmd.arg("daemon").arg("serve");
    // 端末を閉じても daemon が死なないよう、新しいセッションへ離脱させる
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    for _ in 0..50 {
        if UnixStream::connect(path).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(std::io::Error::new(ErrorKind::NotFound, "daemon が起動しなかった"))
}

fn find_mina() -> Option<PathBuf> {
    let sibling = std::env::current_exe().ok()?.parent()?.join("minae");
    if sibling.exists() {
        return Some(sibling);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("minae"))
            .find(|p| p.exists())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ranges_splits_on_newlines() {
        assert_eq!(line_ranges(""), vec![(0, 0)]);
        assert_eq!(line_ranges("abc\ndef\n"), vec![(0, 3), (4, 7), (8, 8)]);
        // マルチバイトも char 位置で数える
        assert_eq!(line_ranges("あい\nう"), vec![(0, 2), (3, 4)]);
    }

    #[test]
    fn snippet_marks_line_and_shows_context() {
        let text = "a\nbb\nccc\ndddd";
        // 'bb' の 'b'（at=2）を含む行が 2 行目。前後 1 行ずつ
        assert_eq!(
            snippet(text, 2, 1),
            "    1: a\n>   2: bb\n    3: ccc"
        );
        // 末尾（at=len）は最終行にフォールバック
        assert_eq!(snippet(text, text.chars().count(), 1), "    3: ccc\n>   4: dddd");
        // 空テキスト
        assert_eq!(snippet("", 0, 0), ">   1: ");
    }
}
