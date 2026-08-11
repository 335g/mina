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
}
