//! 描画: [`StateSnapshot`] をエスケープシーケンス列に変換する純粋関数。
//!
//! ponytail: 毎フレーム全画面を再描画する（差分描画は巨大ファイルや flicker が
//! 問題になったら）。全角文字は表示幅 2 として unicode-width で処理する。

use std::io::Write;

use crate::colorscheme::{adapt_color, ColorCapability, Colorscheme, Style, UiRole};
use mina_protocol::{Diagnostic, HighlightGroup, HighlightRange, Mode, Range, Severity, StateSnapshot};
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
///
/// `command_line`: コマンドモードの入力バッファ（`Some` ならステータス行を `:` プロンプトに置き換える）。
/// `flash`: クライアント側の一時メッセージ（未知コマンド等。次のキーで消える）。
#[allow(clippy::too_many_arguments)] // 純粋関数: 全描画状態を引数で受ける（scheme 追加で上限超え）
pub fn render_text(
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    state: &StateSnapshot,
    pending: &[KeyEvent],
    command_line: Option<&str>,
    flash: Option<&str>,
    width: u16,
    height: u16,
) -> String {
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
    // highlights は行を跨いで昇順に進むため、ポインタを行ループの外に持つ
    // （行ごとに 0 から歩き直すと O(行数×範囲数) — 敵対的検証で発見）。
    let mut hl_idx = 0usize;
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
                    &state.highlights,
                    &mut hl_idx,
                    scheme,
                    capability,
                    no_color,
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
    draw_status(
        &mut s,
        scheme,
        capability,
        no_color,
        state,
        pending,
        command_line,
        flash,
        width,
    );

    // 外部削除ポップアップ（ADR-0015）: 中央にモーダル表示。入力をブロックする
    // のはクライアント側（任意キーで Close が送られる）。
    if let Some(deleted_path) = &state.deleted {
        let mut lines = vec![
            "file deleted on disk".to_string(),
            format!("  {}", sanitize_status_data(deleted_path)),
        ];
        if state.dirty {
            lines.push("unsaved changes will be lost".to_string());
        }
        lines.push("(press any key to close)".to_string());
        let line_width = |l: &str| l.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();
        let box_w = lines
            .iter()
            .map(|l| line_width(l))
            .max()
            .unwrap_or(0)
            .min(width.saturating_sub(2));
        let box_h = lines.len().min(body_rows);
        let top = body_rows.saturating_sub(box_h) / 2;
        let left = width.saturating_sub(box_w) / 2;
        for (i, line) in lines.iter().take(box_h).enumerate() {
            s.push_str(&format!("\x1b[{};{}H", top + i + 1, left + 1));
            s.push_str(&ui_sgr(scheme, capability, no_color, UiRole::Popup));
            let mut padded = line.clone();
            let mut w = line_width(&padded);
            while w < box_w {
                padded.push(' ');
                w += 1;
            }
            truncate_wide(&mut padded, box_w);
            s.push_str(&padded);
            s.push_str("\x1b[0m\x1b[K");
        }
    }

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
#[allow(clippy::too_many_arguments)]
pub fn draw(
    out: &mut impl Write,
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    state: &StateSnapshot,
    pending: &[KeyEvent],
    command_line: Option<&str>,
    flash: Option<&str>,
    width: u16,
    height: u16,
) -> std::io::Result<()> {
    out.write_all(
        render_text(scheme, capability, no_color, state, pending, command_line, flash, width, height)
            .as_bytes(),
    )
}

/// 1行分を描画する。選択範囲は反転、診断範囲は下線、ハイライトグループは前景色、
/// カーソルセルは青背景。
///
/// ターミナルカーソルは `\x1b[?25l` で隠しているため、カーソル位置はこの
/// セル描画でのみ可視化される（修正前はどこにも見えず、ステータス行の座標だけが
/// 手がかりだった）。
#[allow(clippy::too_many_arguments)] // 行描画に必要な状態をすべて引数で受ける
fn draw_line(
    s: &mut String,
    line: &str,
    line_char_start: usize,
    selection: &[Range],
    diagnostics: &[Diagnostic],
    highlights: &[HighlightRange],
    hl_idx: &mut usize, // highlights は start 昇順・非重複（mina-loader の不変条件）
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    cursor: Option<usize>,
    width: usize,
) {
    let mut out_width = 0usize;
    let mut style = (false, false, None, None); // (カーソル, 選択中, 診断ロール, グループ)
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
        let diag_role = diagnostics
            .iter()
            .find(|d| char_global >= d.start && char_global < d.end)
            .map(diag_role);
        // グループは昇順・非重複の範囲から、1 char ずつ進むポインタで引く
        let group = loop {
            match highlights.get(*hl_idx) {
                Some(r) if r.end <= char_global => *hl_idx += 1,
                Some(r) if r.start <= char_global => break Some(r.group),
                _ => break None,
            }
        };
        // 幅超過で切り詰め — 描画されない文字のスタイル遷移 (宙に浮く SGR) を
        // 出さないため、スタイル遷移より先に判定する（敵対的検証で発見）。
        if out_width + w > width {
            break;
        }
        let new_style = (is_cursor, in_sel, diag_role, group);
        if new_style != style {
            style = new_style;
            s.push_str(&style_sgr(scheme, capability, no_color, style));
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
            let diag_role = diagnostics
                .iter()
                .find(|d| d.start <= c && c < d.end)
                .map(diag_role);
            s.push_str(&style_sgr(scheme, capability, no_color, (true, false, diag_role, None)));
            s.push(' ');
            s.push_str("\x1b[0m");
        }
    }
    if style != (false, false, None, None) {
        s.push_str("\x1b[0m");
    }
    s.push_str("\x1b[K"); // 行末までクリア
}

/// 診断の severity → UI ロール。Info/Hint も Warning 色を当てる（M4 以前の
/// 全 severity 下線表示を維持するため。ステータスの [nE nW] には Error/Warning
/// のみが数えられる — カウントとの不整合は仕様として許容）。
fn diag_role(d: &Diagnostic) -> UiRole {
    match d.severity {
        Severity::Error => UiRole::DiagnosticError,
        _ => UiRole::DiagnosticWarning,
    }
}

/// スタイル状態 → SGR シーケンス。優先順位: カーソル > 選択 > 診断 > グループ。
///
/// カーソル・選択はグループ色を置換し、診断の下線はカーソル/選択と合成する
/// （現行 4;44 / 4;7 を維持）。診断範囲内では「下線 + 診断色」がグループ色を
/// 置換する（M4 で M3 の共存ルールを置き換え — 仕様書 M3 の優先順位と整合）。
fn style_sgr(
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    style: (bool, bool, Option<UiRole>, Option<HighlightGroup>),
) -> String {
    let (cursor, in_sel, diag_role, group) = style;
    // 勝利ロールの Style を全フィールド合成する（bg のみ・reverse のみ等の
    // 部分抽出はスキーム作者が落とし穴になる — 敵対的検証で発見・修正）。
    let mut st = Style::new();
    if cursor {
        if let Some(s) = scheme.ui_style(UiRole::Cursor) {
            st = st.merged(s);
        }
        if diag_role.is_some() {
            st.underline = true; // カーソル + 診断 = 青背景 + 下線
        }
    } else if in_sel {
        if let Some(s) = scheme.ui_style(UiRole::Selection) {
            st = st.merged(s);
        }
        if diag_role.is_some() {
            st.underline = true; // 選択 + 診断 = 反転 + 下線
        }
    } else if let Some(role) = diag_role {
        // 診断 > グループ: 診断の Style（下線 + 診断色）がグループを置換
        if let Some(s) = scheme.ui_style(role) {
            st = st.merged(s);
        }
    } else if let Some(group) = group {
        if let Some(s) = scheme.syntax_style(group) {
            st = st.merged(s);
        }
    }
    emit_sgr(st, capability, no_color)
}

/// UI ロールの SGR（未定義ならリセット）。ステータス行・プロンプト・ポップアップ用。
fn ui_sgr(
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    role: UiRole,
) -> String {
    match scheme.ui_style(role) {
        Some(s) => emit_sgr(s, capability, no_color),
        None => "\x1b[0m".to_string(),
    }
}

/// SGR シーケンスを組み立てる。属性 → 色の順（既存の "4;44" / "4;7" 等を維持）。
///
/// 遷移時は必ず `\x1b[0m` を前置する — 旧属性（下線・反転・背景色）はリセット
/// されるまで滲むため（例: 診断→グループで下線が残る。敵対的検証で発見）。
/// 色は能力に合わせて変換し（adapt_color）、`no_color` では色を落とす
/// （属性は維持 — ADR-0019）。
fn emit_sgr(st: Style, capability: ColorCapability, no_color: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if st.underline {
        parts.push("4".to_string()); // 下線
    }
    if st.reverse {
        parts.push("7".to_string()); // 反転
    }
    if !no_color {
        if let Some(c) = st.fg {
            parts.push(adapt_color(c, capability).fg_sgr());
        }
        if let Some(b) = st.bg {
            parts.push(adapt_color(b, capability).bg_sgr());
        }
    }
    if parts.is_empty() {
        "\x1b[0m".to_string()
    } else {
        format!("\x1b[0m\x1b[{}m", parts.join(";"))
    }
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

/// ステータス行: コマンドモード中は `:` + 入力バッファ（Helix 流）、
/// それ以外は `[モード] パス 行:列 [pending] [status|flash]`
#[allow(clippy::too_many_arguments)]
fn draw_status(
    s: &mut String,
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    state: &StateSnapshot,
    pending: &[KeyEvent],
    command_line: Option<&str>,
    flash: Option<&str>,
    width: usize,
) {
    // ステータス文字列は別バッファで組み立ててから切り詰める
    // （出力全体の `s` を truncate すると画面が途中で消える）
    let mut status = String::new();
    if let Some(buf) = command_line {
        // コマンドモード: 行全体を反転し `:` + バッファ + カーソルを表示する
        status.push_str(&ui_sgr(scheme, capability, no_color, UiRole::CommandLine));
        let mut line = String::from(":");
        line.push_str(&sanitize_status_data(buf));
        // カーソルは常に見えるよう 1 セル空けてから `_` を付ける
        truncate_wide(&mut line, width.saturating_sub(1));
        status.push_str(&line);
        status.push('_');
        status.push_str("\x1b[0m");
    } else {
        // テキストを SGR なしで組み立ててから幅で切り詰め、最後に mode を反転で
        // 包む — エスケープを幅に数えると truncate がエスケープ途中で切れて
        // 壊れた CSI を出力する（敵対的検証で発見）。
        let mode = match state.mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Select => "SELECT",
        };
        let mut text = String::new();
        text.push_str(mode);
        // 診断カウントは mode の直後（パスより前）に置く — 長いパスで truncate
        // されてもカウントが消えないようにする（修正前は末尾だったため、実用の
        // 長いパスで [nE nW] が画面外に切れていた）。
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
            text.push_str(&format!("  [{errors}E {warnings}W]"));
        }
        let path = sanitize_status_data(state.path.as_deref().unwrap_or("[no name]"));
        let dirty = if state.dirty { "*" } else { "" };
        let primary = state
            .selection
            .get(state.primary_index)
            .copied()
            .unwrap_or(Range { anchor: 0, head: 0 });
        let (row, col, _) = cursor_pos(&state.text, primary.head);
        text.push_str(&format!(" {path}{dirty}  {row}:{col}"));
        if !pending.is_empty() {
            let keys: String = pending
                .iter()
                .filter_map(|k| match k.code {
                    KeyCode::Char(c) => Some(c),
                    _ => None,
                })
                .collect();
            text.push_str(&format!("  <{keys}>"));
        }
        // クライアント側の一時メッセージ（flash）は daemon の status より優先する
        // （flash が立つのは未知コマンド等で、status は古い情報のまま残っているため）
        let msg = flash.or(state.status.as_deref());
        if let Some(msg) = msg {
            text.push_str(&format!("  {}", sanitize_status_data(msg)));
        }
        truncate_wide(&mut text, width);
        status.push_str(&ui_sgr(scheme, capability, no_color, UiRole::StatusLine));
        if let Some(rest) = text.strip_prefix(mode) {
            // mode が丸ごと残った: mode だけ反転で包み、残りは無地
            status.push_str(mode);
            status.push_str("\x1b[0m");
            status.push_str(rest);
        } else {
            // 切り詰めが mode の途中に入った: 残り全部を反転で包む
            status.push_str(&text);
            status.push_str("\x1b[0m");
        }
    }
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
            checksum: 0, // 描画テスト用 fixture: checksum は検証対象外
            selection,
            primary_index: primary,
            mode: Mode::Normal,
            first_line: 0,
            diagnostics: Vec::new(),
            inlay_hints: Vec::new(), // #24: 描画テストで設定する
            highlights: Vec::new(),
            path: None,
            dirty: false,
            status: None,
            generation: 0,
            events: Vec::new(),
            deleted: None,
        }
    }

    #[test]
    fn renders_text_status_and_cursor() {
        let state = state_with("hello", vec![Range { anchor: 2, head: 3 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
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
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        // 選択（char 2）は反転、カーソル（head=3）は青背景で区別される
        assert!(out.contains("\x1b[7ml\x1b[0m\x1b[44ml"), "{out:?}");
    }

    #[test]
    fn highlight_groups_get_colored() {
        // グループ付きの行: string は緑 (32)、comment は灰 (90)
        let mut state = state_with("let s = \"hi\"; // x", vec![], 0);
        state.highlights = vec![
            HighlightRange { start: 8, end: 12, group: HighlightGroup::String },
            HighlightRange { start: 14, end: 16, group: HighlightGroup::Comment },
        ];
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[32m\"hi\""), "string が緑で描画される: {out:?}");
        assert!(out.contains("\x1b[90m//"), "comment が灰で描画される: {out:?}");
        assert!(!out.contains("\x1b[32m;"), "範囲外にグループ色を出さない: {out:?}");
    }

    #[test]
    fn cursor_overrides_group_color() {
        let mut state = state_with("abc", vec![Range { anchor: 2, head: 2 }], 0);
        state.highlights =
            vec![HighlightRange { start: 0, end: 3, group: HighlightGroup::Keyword }];
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        // グループ全体は cyan (36) だが、カーソル (char 2) は青背景 (44) で置換される
        assert!(out.contains("\x1b[36mab"), "グループ色: {out:?}");
        assert!(out.contains("\x1b[44mc"), "カーソルセルは青背景: {out:?}");
        assert!(!out.contains("\x1b[36mc"), "カーソル下でグループ色を出さない: {out:?}");
    }

    #[test]
    fn selection_overrides_group_color() {
        let mut state = state_with("abc", vec![Range { anchor: 1, head: 2 }], 0);
        state.highlights =
            vec![HighlightRange { start: 0, end: 3, group: HighlightGroup::Keyword }];
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        // 選択 (char 1) は反転 (7) で、グループ色は出ない
        assert!(out.contains("\x1b[36ma"), "グループ色: {out:?}");
        assert!(out.contains("\x1b[7mb"), "選択は反転: {out:?}");
        assert!(!out.contains("\x1b[36mb"), "選択下でグループ色を出さない: {out:?}");
    }

    #[test]
    fn diagnostic_replaces_group_color() {
        // M4: 診断 > グループ の優先順位。診断範囲内は「下線 + 診断色」がグループ色を置換する
        let mut state = state_with("abc", vec![Range { anchor: 0, head: 0 }], 0);
        state.diagnostics = vec![Diagnostic {
            start: 1,
            end: 2,
            severity: Severity::Error,
            message: "oops".into(),
        }];
        state.highlights =
            vec![HighlightRange { start: 1, end: 2, group: HighlightGroup::String }];
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        // 下線 (4) + エラー色 (91)。グループ色 (32) は出ない
        assert!(out.contains("\x1b[4;91mb"), "下線 + 診断色: {out:?}");
        assert!(!out.contains("\x1b[32mb"), "グループ色を出さない: {out:?}");
    }

    #[test]
    fn diagnostic_severity_colors_differ() {
        // Error は赤 (91)、Warning は黄 (93)。カーソル (char 0) と重ならないよう char 1 に置く
        let mut state = state_with("abc", vec![Range { anchor: 0, head: 0 }], 0);
        state.diagnostics = vec![Diagnostic {
            start: 1,
            end: 2,
            severity: Severity::Error,
            message: "e".into(),
        }];
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[4;91mb"), "Error: {out:?}");

        let mut state = state_with("abc", vec![Range { anchor: 0, head: 0 }], 0);
        state.diagnostics = vec![Diagnostic {
            start: 1,
            end: 2,
            severity: Severity::Warning,
            message: "w".into(),
        }];
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[4;93mb"), "Warning: {out:?}");
    }

    #[test]
    fn triple_overlap_precedence() {
        // カーソル > 選択 > 診断 > グループ: 全ロールが同じ行で重なる
        let mut state = state_with("abcd", vec![Range { anchor: 1, head: 3 }], 0);
        state.diagnostics = vec![Diagnostic {
            start: 2,
            end: 3,
            severity: Severity::Error,
            message: "e".into(),
        }];
        state.highlights =
            vec![HighlightRange { start: 0, end: 4, group: HighlightGroup::String }];
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        // char 0: グループ (32) / char 1: 選択 (7) / char 2: 選択+診断 (4;7) / char 3: カーソル (44)
        assert!(out.contains("\x1b[32ma"), "グループ: {out:?}");
        assert!(out.contains("\x1b[7mb"), "選択: {out:?}");
        assert!(out.contains("\x1b[4;7mc"), "選択+診断: {out:?}");
        assert!(out.contains("\x1b[44md"), "カーソル: {out:?}");
        assert!(!out.contains("\x1b[32mb"), "選択下でグループ色を出さない");
        assert!(!out.contains("\x1b[32mc"), "選択+診断下でグループ色を出さない");
        assert!(!out.contains("\x1b[32md"), "カーソル下でグループ色を出さない");
    }

    #[test]
    fn group_spanning_lines() {
        // 範囲が改行を跨ぐ（複数行文字列等）: 次の行の先頭も同グループ
        let mut state = state_with("ab\ncd", vec![Range { anchor: 5, head: 5 }], 0);
        state.highlights =
            vec![HighlightRange { start: 0, end: 4, group: HighlightGroup::String }];
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[32mab"), "1行目: {out:?}");
        assert!(out.contains("\x1b[32mc"), "2行目の先頭も同グループ: {out:?}");
        assert!(!out.contains("\x1b[32mcd"), "範囲外 (char 4) に色を出さない: {out:?}");
    }

    #[test]
    fn wide_char_truncation_inside_group() {
        // 幅3: "あ" (2) まで描画。切り詰め時に宙に浮く SGR を出さない
        let mut state = state_with("あいう", vec![Range { anchor: 3, head: 3 }], 0);
        state.highlights =
            vec![HighlightRange { start: 0, end: 3, group: HighlightGroup::String }];
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 3, 10);
        assert!(out.contains("\x1b[32mあ"), "グループ色で全角1文字: {out:?}");
        assert!(!out.contains("い"), "幅超過で切り詰め: {out:?}");
        assert!(!out.contains("\x1b[32m\x1b[0m"), "宙に浮く SGR を出さない: {out:?}");
    }

    #[test]
    fn eof_cursor_after_trailing_newline_is_visible() {
        // レビュー指摘の検証: 末尾 \n 直後の EOF カーソルが空白セルで見えるか
        // （末尾空行の char_start は「\n の直後」なので既存の行末判定で一致する）
        let state = state_with("ab\n", vec![Range { anchor: 3, head: 3 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[44m "), "EOF カーソルが空白セルで見える: {out:?}");

        let state = state_with("ab\ncd\n", vec![Range { anchor: 6, head: 6 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[44m "), "複数行末尾の EOF カーソル: {out:?}");
    }

    #[test]
    fn narrow_status_line_does_not_emit_broken_csi() {
        // レビュー指摘: エスケープを幅に数えて truncate すると中途半端な CSI を
        // 出力する。テキスト先行切り詰めに変えたので mode のラップとリセットが
        // 壊れない（幅 10 < NORMAL 6 + 残りの幅）
        let state = state_with("x", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 10, 10);
        assert!(
            out.contains("\x1b[7mNORMAL\x1b[0m"),
            "mode の反転ラップとリセットが壊れない: {out:?}"
        );
        assert!(
            !out.contains("\x1b[7mNORMAL\x1b[K"),
            "エスケープが \x1b[K に飲み込まれない: {out:?}"
        );
    }

    #[test]
    fn vivid_scheme_renders_different_group_colors() {
        // スキーム切替が描画に反映される（DEFAULT: keyword 36 → VIVID: 35 マゼンタ）
        let mut state = state_with("abc", vec![Range { anchor: 3, head: 3 }], 0);
        state.highlights =
            vec![HighlightRange { start: 0, end: 3, group: HighlightGroup::Keyword }];
        let out = render_text(&crate::colorscheme::VIVID, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[35mabc"), "VIVID の keyword はマゼンタ: {out:?}");
        assert!(!out.contains("\x1b[36mabc"), "DEFAULT の cyan を出さない: {out:?}");
    }

    #[test]
    fn truecolor_capability_emits_rgb() {
        // VIVID の keyword は Rgb(205,0,205)。TrueColor では 38;2 で出力される
        let mut state = state_with("abc", vec![Range { anchor: 3, head: 3 }], 0);
        state.highlights =
            vec![HighlightRange { start: 0, end: 3, group: HighlightGroup::Keyword }];
        let out = render_text(
            &crate::colorscheme::VIVID,
            ColorCapability::TrueColor,
            false,
            &state,
            &[],
            None,
            None,
            40,
            10,
        );
        assert!(
            out.contains("\x1b[38;2;205;0;205mabc"),
            "truecolor 出力: {out:?}"
        );
    }

    #[test]
    fn no_color_drops_colors_keeps_attributes() {
        // NO_COLOR: 色は出ないが下線・反転は残る (ADR-0019)
        let mut state = state_with("abc", vec![Range { anchor: 0, head: 2 }], 0);
        state.diagnostics = vec![Diagnostic {
            start: 0,
            end: 2,
            severity: Severity::Error,
            message: "e".into(),
        }];
        let out = render_text(
            &crate::colorscheme::DEFAULT,
            ColorCapability::TrueColor,
            true,
            &state,
            &[],
            None,
            None,
            40,
            10,
        );
        assert!(!out.contains("38;2"), "truecolor を出さない: {out:?}");
        assert!(!out.contains("\x1b[44m"), "カーソル背景を出さない: {out:?}");
        assert!(out.contains("\x1b[4;7m"), "下線 + 反転は残る: {out:?}");
    }

    #[test]
    fn scheme_styles_are_merged_fully() {
        // 勝利ロールの Style は全フィールドが尊重される（bg だけ・reverse だけの
        // 部分抽出でないこと — 敵対的検証で発見したバグの回帰テスト）
        let scheme = Colorscheme {
            name: "probe",
            syntax: &[],
            ui: &[(UiRole::Selection, Style {
                fg: None,
                bg: Some(crate::colorscheme::Color::Ansi(0)),
                underline: false,
                reverse: true,
            })],
        };
        let state = state_with("ab", vec![Range { anchor: 0, head: 1 }], 0);
        let out = render_text(&scheme, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        // 選択 (char 0): reverse (7) + bg (40) が両方出る。カーソル (char 1) は未定義 → リセット
        assert!(out.contains("\x1b[7;40ma"), "Selection の全フィールド: {out:?}");
        assert!(!out.contains("\x1b[44m"), "未定義 Cursor に既定色を出さない: {out:?}");
    }

    #[test]
    fn cursor_at_line_end_is_visible() {
        // カーソルが行末（最後の文字の直後）にあっても青背景の空白で見える
        let state = state_with("hi", vec![Range { anchor: 2, head: 2 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[44m "), "行末カーソルの青背景空白: {out:?}");
    }

    #[test]
    fn status_shows_pending_keys() {
        let state = state_with("hello", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[plain(KeyCode::Char('g'))], None, None, 40, 10);
        assert!(out.contains("<g>"), "{out:?}");
    }

    #[test]
    fn wide_char_truncation_respects_display_width() {
        // "あ" は表示幅2。幅3なら "あ" で切れ、"あい" にはならない
        let state = state_with("あいうえお", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 3, 10);
        assert!(out.contains("あ") && !out.contains("あい"), "{out:?}");
    }

    #[test]
    fn empty_document_renders_blank() {
        let state = state_with("", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[K"), "{out:?}");
    }

    #[test]
    fn multi_line_renders_all_rows() {
        let state = state_with("a\nb\nc", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(!out.contains("a\n"), "行内に生の改行を出さない: {out:?}");
        assert!(out.contains("\x1b[2;1H"), "2行目へ移動: {out:?}");
    }

    #[test]
    fn crlf_line_is_rendered_not_erased() {
        // H2: CRLF の \r を生出力すると直後の \x1b[K で行全体が消える。
        // \r は非表示文字としてスキップし、両行とも描画される。
        let state = state_with("a\r\nb\r\n", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(!out.contains('\r'), "生の \\r を出力しない: {out:?}");
        assert!(out.contains("a"), "1行目が描画される: {out:?}");
        assert!(out.contains("b"), "2行目が描画される: {out:?}");
    }

    #[test]
    fn control_chars_are_replaced_not_emitted() {
        // 制御文字（ESC 等）は � に置換（端末インジェクション対策）。
        // 文書内の \x1b[31m がそのまま端末へ流れないことを確認する。
        let state = state_with("\x1b[31mred", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
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
            checksum: 0, // fixture
            selection: vec![Range { anchor: 0, head: 0 }],
            primary_index: 0,
            mode: Mode::Normal,
            first_line: 0,
            diagnostics: Vec::new(),
            inlay_hints: Vec::new(), // #24: 描画テストで設定する
            highlights: Vec::new(),
            // OSC シーケンス（ターミナルタイトル変更）と SGR（色変更）の注入を試みる
            path: Some("\x1b]0;evil\x07".into()),
            dirty: false,
            status: Some("\x1b[31m".into()),
            generation: 0,
            events: Vec::new(),
            deleted: None,
        };
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
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
            checksum: 0, // fixture
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
            inlay_hints: Vec::new(), // #24: 描画テストで設定する
            highlights: Vec::new(),
            path: None,
            dirty: false,
            status: None,
            generation: 0,
            events: Vec::new(),
            deleted: None,
        };
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[4;91mworld\x1b[0m"), "診断範囲に下線 + エラー色: {out:?}");
        assert!(out.contains("[1E 0W]"), "ステータスにカウント: {out:?}");
    }

    #[test]
    fn deleted_state_renders_central_popup() {
        // ADR-0015: 外部削除ポップアップが中央に描画される。dirty なら警告が付く。
        let mut state = state_with("hello", vec![Range { anchor: 0, head: 0 }], 0);
        state.deleted = Some("/tmp/x.txt".into());
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("file deleted on disk"), "{out:?}");
        assert!(out.contains("x.txt"), "パスが表示される: {out:?}");
        assert!(out.contains("press any key to close"), "{out:?}");
        assert!(!out.contains("unsaved changes will be lost"), "{out:?}");

        state.dirty = true;
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("unsaved changes will be lost"), "{out:?}");
    }

    #[test]
    fn command_line_replaces_status_with_prompt() {
        // コマンドモード中はステータス行全体が `:` プロンプトに置き換わる
        let state = state_with("hello", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], Some("w"), None, 40, 10);
        assert!(out.contains(":w_"), "`:` + バッファ + カーソル: {out:?}");
        assert!(!out.contains("NORMAL"), "モード表示はプロンプトに置き換わる: {out:?}");
    }

    #[test]
    fn flash_shows_in_status_slot_and_yields_to_status() {
        // クライアント側メッセージ（flash）は status スロットに表示される
        let state = state_with("hello", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, Some("unknown command: foo"), 120, 10);
        assert!(out.contains("unknown command: foo"), "{out:?}");
        // コマンドモード中は flash ではなくプロンプトが優先される
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], Some("q"), Some("unknown command: foo"), 40, 10);
        assert!(out.contains(":q_") && !out.contains("unknown"), "{out:?}");
    }

    #[test]
    fn command_line_is_sanitized_and_truncated_with_cursor() {
        // SEC-2: コマンドラインの制御文字は � に置換され、生の ESC が流れない
        let state = state_with("hello", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], Some("\x1b[31m"), None, 40, 10);
        assert!(!out.contains("\x1b[31m"), "ESC を生出力しない: {out:?}");
        // 幅5のプロンプト: カーソル `_` は常に 1 セル確保される
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], Some("abcd"), None, 5, 10);
        assert!(out.contains(":abc_"), "カーソルが残る: {out:?}");
    }

    fn plain(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, termina::event::Modifiers::NONE)
    }
}
