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
                    Some(head),
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

/// 1行分を描画する。選択範囲は反転、診断範囲は下線、カーソルセルは青背景。
///
/// ターミナルカーソルは `\x1b[?25l` で隠しているため、カーソル位置はこの
/// セル描画でのみ可視化される（修正前はどこにも見えず、ステータス行の座標だけが
/// 手がかりだった）。
fn draw_line(
    s: &mut String,
    line: &str,
    line_char_start: usize,
    selection: &[Range],
    diagnostics: &[Diagnostic],
    cursor: Option<usize>,
    width: usize,
) {
    let mut out_width = 0usize;
    let mut style = (false, false, false); // (カーソル, 選択中, 診断中)
    for (i, ch) in line.chars().enumerate() {
        // CRLF の \r: 非表示文字。生出力するとターミナルがカーソルを行頭へ戻し、
        // 直後の \x1b[K で行全体が消える（H2）。
        if ch == '\r' {
            continue;
        }
        let is_ctrl = ch.is_control() && ch != '\t';
        // 制御文字（ESC 等）は端末インジェクション対策で � に置換してから出力する
        let w = if is_ctrl { 1 } else { ch.width().unwrap_or(0) };
        let char_global = line_char_start + i;
        let is_cursor = cursor == Some(char_global);
        let in_sel = selection
            .iter()
            .any(|r| char_global >= r.anchor.min(r.head) && char_global < r.anchor.max(r.head));
        let in_diag = diagnostics
            .iter()
            .any(|d| char_global >= d.start && char_global < d.end);
        let new_style = (is_cursor, in_sel, in_diag);
        if new_style != style {
            style = new_style;
            match style {
                (true, _, true) => s.push_str("\x1b[4;44m"), // カーソル + 診断: 青背景 + 下線
                (true, _, false) => s.push_str("\x1b[44m"),   // カーソル: 青背景
                (false, true, true) => s.push_str("\x1b[4;7m"), // 下線 + 反転
                (false, true, false) => s.push_str("\x1b[7m"),
                (false, false, true) => s.push_str("\x1b[4m"), // 下線
                (false, false, false) => s.push_str("\x1b[0m"),
            }
        }
        if out_width + w > width {
            break; // 幅超過で切り詰め（行末の全角文字は途中で切れる — ponytail）
        }
        if is_ctrl {
            s.push('\u{FFFD}');
        } else {
            s.push(ch);
        }
        out_width += w;
    }
    // カーソルが行末（最後の文字の直後）にある場合: 青背景の空白で可視化する
    if let Some(c) = cursor {
        if c == line_char_start + line.chars().count() && out_width < width {
            let in_diag = diagnostics.iter().any(|d| d.start <= c && c < d.end);
            s.push_str(if in_diag { "\x1b[4;44m" } else { "\x1b[44m" });
            s.push(' ');
            s.push_str("\x1b[0m");
        }
    }
    if style != (false, false, false) {
        s.push_str("\x1b[0m");
    }
    s.push_str("\x1b[K"); // 行末までクリア
}

/// 制御文字（ESC 等）を � に置換する（端末インジェクション対策 — SEC-2）。
///
/// `\t` はそのまま。ステータス行のパス・メッセージ（クライアント/ファイル由来の
/// データ）に使う。draw_line と同一の方針。
fn sanitize_status_data(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\t' { '\u{FFFD}' } else { c })
        .collect()
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
    // 診断カウントは mode の直後（パスより前）に置く — 長いパスで truncate され
    // てもカウントが消えないようにする（修正前は末尾だったため、実用の長い
    // パスで [nE nW] が画面外に切れていた）。
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
    let path = sanitize_status_data(state.path.as_deref().unwrap_or("[no name]"));
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
        status.push_str(&format!("  {}", sanitize_status_data(msg)));
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
        } else if ch != '\r' {
            // \r は非表示（CRLF）なので行・列に数えない（H2）
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
            generation: 0,
            events: Vec::new(),
            disk_changed: false,
        }
    }

    #[test]
    fn renders_text_status_and_cursor() {
        let state = state_with("hello", vec![Range { anchor: 2, head: 3 }], 0);
        let out = render_text(&state, &[], 40, 10);
        // 選択ハイライトとカーソルセルで "hello" は分割されるので部分で検証
        assert!(out.contains("he"), "{out:?}");
        assert!(out.contains("NORMAL"), "{out:?}");
        assert!(out.contains("\x1b[44ml"), "カーソルセルが青背景で見える: {out:?}");
        assert!(out.contains("\x1b[1;4H"), "カーソル位置エスケープ: {out:?}");
        assert!(out.contains("\x1b[?2026l"), "同期出力 OFF で閉じる");
    }

    #[test]
    fn selection_is_highlighted() {
        let state = state_with("hello", vec![Range { anchor: 2, head: 3 }], 0);
        let out = render_text(&state, &[], 40, 10);
        // 選択（char 2）は反転、カーソル（head=3）は青背景で区別される
        assert!(out.contains("\x1b[7ml\x1b[44ml"), "{out:?}");
    }

    #[test]
    fn cursor_at_line_end_is_visible() {
        // カーソルが行末（最後の文字の直後）にあっても青背景の空白で見える
        let state = state_with("hi", vec![Range { anchor: 2, head: 2 }], 0);
        let out = render_text(&state, &[], 40, 10);
        assert!(out.contains("\x1b[44m "), "行末カーソルの青背景空白: {out:?}");
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
    fn crlf_line_is_rendered_not_erased() {
        // H2: CRLF の \r を生出力すると直後の \x1b[K で行全体が消える。
        // \r は非表示文字としてスキップし、両行とも描画される。
        let state = state_with("a\r\nb\r\n", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&state, &[], 40, 10);
        assert!(!out.contains('\r'), "生の \\r を出力しない: {out:?}");
        assert!(out.contains("a"), "1行目が描画される: {out:?}");
        assert!(out.contains("b"), "2行目が描画される: {out:?}");
    }

    #[test]
    fn control_chars_are_replaced_not_emitted() {
        // 制御文字（ESC 等）は � に置換（端末インジェクション対策）。
        // 文書内の \x1b[31m がそのまま端末へ流れないことを確認する。
        let state = state_with("\x1b[31mred", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&state, &[], 40, 10);
        assert!(!out.contains("\x1b[31m"), "ESC シーケンスを生出力しない: {out:?}");
        assert!(out.contains('\u{FFFD}'), "制御文字は � に置換される: {out:?}");
    }

    #[test]
    fn cursor_pos_ignores_carriage_return() {
        // \r は非表示なのでカーソル列（表示幅・表示列）に数えない
        assert_eq!(cursor_pos("ab\r", 3), (0, 2, 2));
        assert_eq!(cursor_pos("a\r\nb", 3), (1, 0, 0));
    }

    #[test]
    fn status_line_sanitizes_path_and_message() {
        // SEC-2: パス・status メッセージ（クライアント/ファイル由来のデータ）の
        // 制御文字は � に置換され、生の ESC が端末に流れない。
        let state = StateSnapshot {
            text: "hello".into(),
            selection: vec![Range { anchor: 0, head: 0 }],
            primary_index: 0,
            mode: Mode::Normal,
            first_line: 0,
            diagnostics: Vec::new(),
            // OSC シーケンス（ターミナルタイトル変更）と SGR（色変更）の注入を試みる
            path: Some("\x1b]0;evil\x07".into()),
            dirty: false,
            status: Some("\x1b[31m".into()),
            generation: 0,
            events: Vec::new(),
            disk_changed: false,
        };
        let out = render_text(&state, &[], 40, 10);
        assert!(!out.contains("\x1b]0;evil"), "OSC を生出力しない: {out:?}");
        assert!(!out.contains("\x1b[31m"), "status の ESC を生出力しない: {out:?}");
    }

    #[test]
    fn sanitize_status_data_replaces_control_chars() {
        assert_eq!(sanitize_status_data("a\x1bb"), "a\u{FFFD}b");
        assert_eq!(sanitize_status_data("tab\tok"), "tab\tok", "\t は維持");
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
            generation: 0,
            events: Vec::new(),
            disk_changed: false,
        };
        let out = render_text(&state, &[], 40, 10);
        assert!(out.contains("\x1b[4mworld\x1b[0m"), "診断範囲に下線: {out:?}");
        assert!(out.contains("[1E 0W]"), "ステータスにカウント: {out:?}");
    }

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, termina::event::Modifiers::NONE)
    }
}
