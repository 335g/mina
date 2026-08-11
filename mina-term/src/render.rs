//! 描画: [`StateSnapshot`] をエスケープシーケンス列に変換する純粋関数。
//!
//! ponytail: 毎フレーム全画面を再描画する（差分描画は巨大ファイルや flicker が
//! 問題になったら）。全角文字は表示幅 2 として unicode-width で処理する。

use std::io::Write;

use mina_protocol::{Diagnostic, Mode, Range, Severity, StateSnapshot};
use termina::event::{KeyCode, KeyEvent};
use unicode_width::UnicodeWidthChar;

/// 行の区切り情報（char 位置と byte 位置の両方を保持）。
///
/// 行 `i` の描画対象は `[byte_starts[i], byte_starts[i+1])` から行末の `\n` を
/// 除いた範囲。行の先頭 char 位置は `char_starts[i]`。
struct LineIndex {
    byte_starts: Vec<usize>,
    char_starts: Vec<usize>,
    text_len: usize,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut byte_starts = vec![0usize];
        let mut char_starts = vec![0usize];
        let mut char_count = 0usize;
        for (byte, ch) in text.char_indices() {
            if ch == '\n' {
                byte_starts.push(byte + 1);
                char_starts.push(char_count + 1);
            }
            char_count += 1;
        }
        Self {
            byte_starts,
            char_starts,
            text_len: text.len(),
        }
    }

    fn byte_range(&self, idx: usize) -> Option<(usize, usize)> {
        let start = *self.byte_starts.get(idx)?;
        // 次の行の先頭の直前 = この行の末尾（\n を除く）
        let end = self
            .byte_starts
            .get(idx + 1)
            .map(|b| b - 1)
            .unwrap_or(self.text_len);
        Some((start, end))
    }

    /// 行 `idx` の先頭 char 位置。
    fn char_start(&self, idx: usize) -> Option<usize> {
        self.char_starts.get(idx).copied()
    }
}

/// 画面全体の描画エスケープ列を生成する。`width`×`height` はターミナルのセル数。
pub fn render_text(state: &StateSnapshot, pending: &[KeyEvent], width: u16, height: u16) -> String {
    let width = width as usize;
    let height = height as usize;
    let lines = LineIndex::new(&state.text);
    let primary = state
        .selection
        .get(state.primary_index)
        .copied()
        .unwrap_or(Range { anchor: 0, head: 0 });
    let head = primary.head;

    let mut s = String::new();
    s.push_str("\x1b[?2026h"); // synchronized output ON（未対応端末では無視される）
    s.push_str("\x1b[H"); // カーソルをホームへ

    let body_rows = height.saturating_sub(1); // 最終行はステータス行
    for row in 0..body_rows {
        s.push_str(&format!("\x1b[{};1H", row + 1));
        let line_idx = state.first_line + row;
        match lines.byte_range(line_idx) {
            Some((bs, be)) => {
                let cs = lines
                    .char_start(line_idx)
                    .expect("byte_range があるなら char_start もある");
                draw_line(
                    &mut s,
                    &state.text[bs..be],
                    cs,
                    &state.selection,
                    &state.diagnostics,
                    width,
                );
            }
            None => s.push_str("\x1b[K"), // 行が無ければクリア
        }
    }

    // ステータス行
    s.push_str(&format!("\x1b[{};1H", height.max(1)));
    s.push_str("\x1b[K");
    draw_status(&mut s, state, pending, width);

    // ターミナルカーソルを primary head へ
    let (row, _, colw) = cursor_pos(&state.text, head);
    let term_row = row.saturating_sub(state.first_line) + 1;
    if term_row <= body_rows {
        s.push_str(&format!("\x1b[{};{}H", term_row, colw + 1));
    }
    s.push_str("\x1b[?2026l"); // synchronized output OFF
    s
}

/// 画面を描画する（`out` への書き込み）。テストではバッファへ書き出せる。
pub fn draw(
    out: &mut impl Write,
    state: &StateSnapshot,
    pending: &[KeyEvent],
    width: u16,
    height: u16,
) -> std::io::Result<()> {
    out.write_all(render_text(state, pending, width, height).as_bytes())
}

/// 1行分を描画する。選択範囲は反転、診断範囲は下線。幅は表示幅（全角2）で切り詰める。
fn draw_line(
    s: &mut String,
    line: &str,
    line_char_start: usize,
    selection: &[Range],
    diagnostics: &[Diagnostic],
    width: usize,
) {
    let mut out_width = 0usize;
    let mut style = (false, false); // (選択中, 診断中)
    for (i, ch) in line.chars().enumerate() {
        let char_global = line_char_start + i;
        let in_sel = selection
            .iter()
            .any(|r| char_global >= r.anchor.min(r.head) && char_global < r.anchor.max(r.head));
        let in_diag = diagnostics
            .iter()
            .any(|d| char_global >= d.start && char_global < d.end);
        let new_style = (in_sel, in_diag);
        if new_style != style {
            style = new_style;
            match style {
                (true, true) => s.push_str("\x1b[4;7m"), // 下線 + 反転
                (true, false) => s.push_str("\x1b[7m"),
                (false, true) => s.push_str("\x1b[4m"), // 下線
                (false, false) => s.push_str("\x1b[0m"),
            }
        }
        let w = ch.width().unwrap_or(0);
        if out_width + w > width {
            break; // 幅超過で切り詰め（行末の全角文字は途中で切れる — ponytail）
        }
        s.push(ch);
        out_width += w;
    }
    if style != (false, false) {
        s.push_str("\x1b[0m");
    }
    s.push_str("\x1b[K"); // 行末までクリア
}

/// ステータス行: `[モード] パス 行:列 [pending] [status]`
fn draw_status(s: &mut String, state: &StateSnapshot, pending: &[KeyEvent], width: usize) {
    // ステータス文字列は別バッファで組み立ててから切り詰める
    // （出力全体の `s` を truncate すると画面が途中で消える）
    let mut status = String::new();
    let mode = match state.mode {
        Mode::Normal => "NORMAL",
        Mode::Insert => "INSERT",
        Mode::Select => "SELECT",
    };
    status.push_str("\x1b[7m");
    status.push_str(mode);
    status.push_str("\x1b[0m");
    let path = state.path.as_deref().unwrap_or("[no name]");
    let dirty = if state.dirty { "*" } else { "" };
    let primary = state
        .selection
        .get(state.primary_index)
        .copied()
        .unwrap_or(Range { anchor: 0, head: 0 });
    let (row, col, _) = cursor_pos(&state.text, primary.head);
    status.push_str(&format!(" {path}{dirty}  {row}:{col}"));
    if !pending.is_empty() {
        let keys: String = pending
            .iter()
            .filter_map(|k| match k.code {
                KeyCode::Char(c) => Some(c),
                _ => None,
            })
            .collect();
        status.push_str(&format!("  <{keys}>"));
    }
    if let Some(msg) = &state.status {
        status.push_str(&format!("  {msg}"));
    }
    // 診断カウント（ステータス行に載せる）
    let errors = state
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = state
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    if errors > 0 || warnings > 0 {
        status.push_str(&format!("  [{errors}E {warnings}W]"));
    }
    truncate_wide(&mut status, width);
    s.push_str(&status);
    s.push_str("\x1b[K");
}

/// 表示幅で `s` を切り詰める（全角2・結合文字0）。
fn truncate_wide(s: &mut String, width: usize) {
    let mut w = 0usize;
    let mut cut = None;
    for (i, ch) in s.chars().enumerate() {
        w += ch.width().unwrap_or(0);
        if w > width {
            cut = Some(i);
            break;
        }
    }
    if let Some(i) = cut {
        let byte = s.char_indices().nth(i).map(|(b, _)| b).unwrap_or(s.len());
        s.truncate(byte);
    }
}

/// primary head の (行, 列[char数], 列[表示幅])。O(head)。
fn cursor_pos(text: &str, head: usize) -> (usize, usize, usize) {
    let mut row = 0usize;
    let mut col = 0usize;
    let mut colw = 0usize;
    for (i, ch) in text.chars().enumerate() {
        if i >= head {
            break;
        }
        if ch == '\n' {
            row += 1;
            col = 0;
            colw = 0;
        } else {
            col += 1;
            colw += ch.width().unwrap_or(0);
        }
    }
    (row, col, colw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(text: &str, selection: Vec<Range>, primary: usize) -> StateSnapshot {
        StateSnapshot {
            text: text.into(),
            selection,
            primary_index: primary,
            mode: Mode::Normal,
            first_line: 0,
            diagnostics: Vec::new(),
            path: None,
            dirty: false,
            status: None,
        }
    }

    #[test]
    fn renders_text_status_and_cursor() {
        let state = state_with("hello", vec![Range { anchor: 2, head: 3 }], 0);
        let out = render_text(&state, &[], 40, 10);
        // 選択ハイライトで "hello" は分割されるので部分で検証
        assert!(out.contains("he") && out.contains("lo"), "{out:?}");
        assert!(out.contains("NORMAL"), "{out:?}");
        assert!(out.contains("\x1b[1;4H"), "カーソル: {out:?}");
        assert!(out.contains("\x1b[?2026l"), "同期出力 OFF で閉じる");
    }

    #[test]
    fn selection_is_highlighted() {
        let state = state_with("hello", vec![Range { anchor: 2, head: 3 }], 0);
        let out = render_text(&state, &[], 40, 10);
        assert!(out.contains("\x1b[7ml\x1b[0m"), "{out:?}");
    }

    #[test]
    fn status_shows_pending_keys() {
        let state = state_with("hello", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&state, &[plain(KeyCode::Char('g'))], 40, 10);
        assert!(out.contains("<g>"), "{out:?}");
    }

    #[test]
    fn wide_char_truncation_respects_display_width() {
        // "あ" は表示幅2。幅3なら "あ" で切れ、"あい" にはならない
        let state = state_with("あいうえお", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&state, &[], 3, 10);
        assert!(out.contains("あ") && !out.contains("あい"), "{out:?}");
    }

    #[test]
    fn empty_document_renders_blank() {
        let state = state_with("", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&state, &[], 40, 10);
        assert!(out.contains("\x1b[K"), "{out:?}");
    }

    #[test]
    fn multi_line_renders_all_rows() {
        let state = state_with("a\nb\nc", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&state, &[], 40, 10);
        assert!(!out.contains("a\n"), "行内に生の改行を出さない: {out:?}");
        assert!(out.contains("\x1b[2;1H"), "2行目へ移動: {out:?}");
    }

    #[test]
    fn diagnostics_get_underlined_and_counted() {
        let state = StateSnapshot {
            text: "hello world".into(),
            selection: vec![Range { anchor: 0, head: 0 }],
            primary_index: 0,
            mode: Mode::Normal,
            first_line: 0,
            diagnostics: vec![mina_protocol::Diagnostic {
                start: 6,
                end: 11,
                severity: mina_protocol::Severity::Error,
                message: "oops".into(),
            }],
            path: None,
            dirty: false,
            status: None,
        };
        let out = render_text(&state, &[], 40, 10);
        assert!(out.contains("\x1b[4mworld\x1b[0m"), "診断範囲に下線: {out:?}");
        assert!(out.contains("[1E 0W]"), "ステータスにカウント: {out:?}");
    }

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, termina::event::Modifiers::NONE)
    }
}
