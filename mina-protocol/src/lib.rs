//! mina の daemon/client IPC の wire 型。
//!
//! 依存を持たない（serde のみ）。daemon とクライアントの両方が参照する。
//! フレーミングは NDJSON: 1メッセージ = JSON 1行（`docs/adr/0006-state-snapshot-ipc.md`）。

use serde::{Deserialize, Serialize};

/// 編集モード（wire 型。mina-view の Mode とは別に持つ — protocol は依存を持たない）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Select,
}

/// クライアントが daemon へ送るコマンド。
///
/// 追加は後方互換（serde の外部タグ付け enum）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    /// 現在の状態スナップショットを要求する。
    GetState,
    /// ファイルを読み込んで開く（読み込み失敗は StateSnapshot.status に報告）。
    Open { path: String },
    /// 選択を点に潰して移動する。
    Move { movement: Movement, direction: Direction },
    /// anchor を保ったまま head を移動する（選択の拡張・縮小）。
    Extend { movement: Movement, direction: Direction },
    /// 文書の先頭/末尾へ絶対移動する。
    Goto { target: GotoTarget },
    /// 表示範囲をページ単位でスクロールする（正で下）。高さは daemon 側が知っている。
    Scroll { pages: isize },
    /// モードを切り替える。
    SetMode { mode: Mode },
    /// ターミナルの表示高さを通知する（カーソル追従スクロールに使う）。
    SetViewport { height: usize },
    /// 選択（またはカーソル位置）にテキストを挿入する。
    Insert { text: String },
    /// 後方削除（Backspace 相当）。
    DeleteBackward,
    /// 前方削除（Delete キー相当）。
    DeleteForward,
    /// 選択範囲を削除する。
    DeleteRange,
    /// 直近の変更グループを元に戻す。
    Undo,
    /// 直近に undo された変更グループをやり直す。
    Redo,
    /// 現在の文書をファイルに書き込む（結果は status に報告）。
    Save,
}

/// 移動の種類（wire 型）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Movement {
    Char,
    Line,
    Word,
}

/// 移動方向（wire 型）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Forward,
    Backward,
}

/// 絶対移動の目標。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GotoTarget {
    DocumentStart,
    DocumentEnd,
}

/// 選択範囲（wire 型。anchor/head は char インデックス）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub anchor: usize,
    pub head: usize,
}

/// 位置指定の文書編集（ADR-0011）。選択を読まない・変えない。
///
/// フォーカス文書に対して明示 char range で作用する:
/// - insert: `start == end`
/// - delete: `text` が空
/// `checksum` はクライアントが最後に読んだ文書全文（UTF-8 バイト列）の
/// FNV-1a 64。不一致（読み取り後に文書が変化）なら daemon は状態を変えず
/// status で拒否する。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentEdit {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub checksum: u64,
}

/// FNV-1a 64 ハッシュ（[`DocumentEdit`] のチェックサム検証用）。
///
/// 安定性のため固定実装（`std::collections::DefaultHasher` は Rust バージョン
/// 間で非安定）。検証用なので暗号学的強度は不要。
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// 診断の深刻度。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

/// 言語サーバが報告する問題（S3 で利用。StateSnapshot に含まれる）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub start: usize,
    pub end: usize,
    pub severity: Severity,
    pub message: String,
}

/// daemon が返す編集状態の全体像（ADR-0006: 毎コマンドに全量を返す）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub text: String,
    pub selection: Vec<Range>,
    pub primary_index: usize,
    pub mode: Mode,
    pub first_line: usize,
    pub diagnostics: Vec<Diagnostic>,
    /// 開いているファイルのパス（未開なら None）。
    pub path: Option<String>,
    /// 保存済み状態から編集されているか。
    pub dirty: bool,
    /// 一時的なメッセージ（Open の失敗など）。ステータス行に表示される。
    pub status: Option<String>,
}

impl Default for StateSnapshot {
    /// 空文書・位置0の単一カーソル・Normal モード。
    fn default() -> Self {
        Self {
            text: String::new(),
            selection: vec![Range { anchor: 0, head: 0 }],
            primary_index: 0,
            mode: Mode::Normal,
            first_line: 0,
            diagnostics: Vec::new(),
            path: None,
            dirty: false,
            status: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_snapshot_round_trip() {
        let snapshot = StateSnapshot {
            text: "hello\nworld".to_string(),
            selection: vec![Range { anchor: 2, head: 5 }],
            primary_index: 0,
            mode: Mode::Insert,
            first_line: 1,
            diagnostics: vec![Diagnostic {
                start: 0,
                end: 5,
                severity: Severity::Warning,
                message: "unused".to_string(),
            }],
            path: Some("test.rs".to_string()),
            dirty: true,
            status: Some("ok".to_string()),
        };
        let json = serde_json::to_string(&snapshot).expect("serialize");
        let back: StateSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, snapshot);
    }

    #[test]
    fn command_round_trip() {
        let json = serde_json::to_string(&Command::GetState).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, Command::GetState);

        let cmd = Command::Move {
            movement: Movement::Line,
            direction: Direction::Backward,
        };
        let json = serde_json::to_string(&cmd).expect("serialize");
        let back: Command = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, cmd);
    }

    #[test]
    fn document_edit_round_trip() {
        let edit = DocumentEdit {
            start: 2,
            end: 5,
            text: "x".into(),
            checksum: 42,
        };
        let json = serde_json::to_string(&edit).expect("serialize");
        let back: DocumentEdit = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, edit);
    }

    #[test]
    fn fnv1a64_is_stable_and_byte_based() {
        // 実装がバージョン間で変わらないこと（クライアント/daemon 両側で一致が前提）
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"hi\n"), fnv1a64("hi\n".as_bytes()));
    }
}
