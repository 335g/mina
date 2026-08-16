//! 描画: [`StateSnapshot`] をエスケープシーケンス列に変換する純粋関数。
//!
//! ponytail: 毎フレーム全画面を再描画する（差分描画は巨大ファイルや flicker が
//! 問題になったら）。全角文字は表示幅 2 として unicode-width で処理する。

use std::io::Write;

use crate::colorscheme::{adapt_color, ColorCapability, Colorscheme, Style, UiRole};
use mina_protocol::{Diagnostic, HighlightGroup, HighlightRange, InlayHint, Mode, Range, Severity, StateSnapshot};
use termina::event::{KeyCode, KeyEvent};
use unicode_width::UnicodeWidthChar;

/// 行の区切り情報（char 位置と byte 位置の両方を保持）。
///
/// 行 `i` の描画対象は `[byte_starts[i], byte_starts[i+1])` から行末の `\n` を
/// 除いた範囲。行の先頭 char 位置は `char_starts[i]`。
pub(crate) struct LineIndex {
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

/// フレーム間で再利用する [`LineIndex`] キャッシュ。`checksum` が同じテキスト
/// なら再構築しない（打鍵ごとの全文 char 走査を回避 — 1MB で ~1.2ms）。
///
/// 前提: `checksum` はスナップショットの `text` と同源（daemon の `snapshot()`
/// が同じ `text` から算出する）。partial snapshot（ADR-0006 延期）で checksum
/// だけ新しくなる運用にしたら、この前提は崩れるため再訪が必要。
pub(crate) struct LineIndexCache {
    checksum: Option<u64>,
    index: LineIndex,
}

impl LineIndexCache {
    pub(crate) fn get(&mut self, checksum: u64, text: &str) -> &LineIndex {
        if self.checksum != Some(checksum) {
            self.index = LineIndex::new(text);
            self.checksum = Some(checksum);
        }
        &self.index
    }
}

impl Default for LineIndexCache {
    fn default() -> Self {
        Self {
            checksum: None,
            index: LineIndex::new(""),
        }
    }
}

/// 画面全体の描画エスケープ列を生成する。`width`×`height` はターミナルのセル数。
///
/// テスト・単発描画用（毎フレーム新しい [`LineIndexCache`] を使う）。
/// TUI ループは [`render_text_with_cache`] で LineIndex をフレーム間再利用する。
#[cfg(test)]
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
    let mut cache = LineIndexCache::default();
    render_text_with_cache(
        scheme, capability, no_color, state, pending, command_line, flash, width, height, &mut cache,
    )
}

/// [`render_text`] のキャッシュ付き版。`cache` はフレーム間で使い回す
/// （checksum が同じテキストなら LineIndex の再構築を省く）。
#[allow(clippy::too_many_arguments)] // 純粋関数: 全描画状態を引数で受ける
pub(crate) fn render_text_with_cache(
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    state: &StateSnapshot,
    pending: &[KeyEvent],
    command_line: Option<&str>,
    flash: Option<&str>,
    width: u16,
    height: u16,
    cache: &mut LineIndexCache,
) -> String {
    let width = width as usize;
    let height = height as usize;
    let lines = cache.get(state.checksum, &state.text);
    let primary = state
        .selection
        .get(state.primary_index)
        .copied()
        .unwrap_or(Range { anchor: 0, head: 0 });
    let head = primary.head;
    // カーソル行（ガターの LineNumberActive 判定と端末カーソル列で使う。
    // LineIndex ベースで O(log n + 行長)）。
    let (cursor_row, _, cursor_colw) = cursor_pos_at(lines, &state.text, head, &state.inlay_hints);
    // ステータス行の "row:col" はヒント幅を含めない（従来の cursor_pos(&[])
    // と同じ意味論）。
    let (_, cursor_col, _) = cursor_pos_at(lines, &state.text, head, &[]);

    let mut s = String::new();
    s.push_str("\x1b[?2026h"); // synchronized output ON（未対応端末では無視される）
    s.push_str("\x1b[H"); // カーソルをホームへ

    let body_rows = height.saturating_sub(1); // 最終行はステータス行
    // ガター幅: 左余白 1 + 4 桁（右詰め）+ 区切りの空白 1 = 6（Helix に寄せた固定幅）
    let gutter_shift = 1 + 4 + 1;
    let text_width = width.saturating_sub(gutter_shift);
    // highlights は行を跨いで昇順に進むため、ポインタを行ループの外に持つ
    // （行ごとに 0 から歩き直すと O(行数×範囲数) — 敵対的検証で発見）。
    // inlay_hints も同じ流儀（ADR-0020。position 昇順の不変条件）。
    let mut hl_idx = 0usize;
    let mut hint_idx = 0usize;
    for row in 0..body_rows {
        s.push_str(&format!("\x1b[{};1H", row + 1));
        let line_idx = state.first_line + row;
        match lines.byte_range(line_idx) {
            Some((bs, be)) => {
                if width < gutter_shift {
                    s.push_str("\x1b[K"); // 幅がガター未満: 行全体をクリア
                    continue;
                }
                // ガター: 1 始まりの絶対番号。カーソル行だけ LineNumberActive（白）
                let role = if line_idx == cursor_row {
                    UiRole::LineNumberActive
                } else {
                    UiRole::LineNumber
                };
                s.push_str(&ui_sgr(scheme, capability, no_color, role));
                s.push_str(&format!(
                    "{:>width$}",
                    line_idx + 1,
                    width = gutter_shift - 1
                ));
                s.push(' ');
                s.push_str("\x1b[0m");
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
                    &state.inlay_hints,
                    &mut hint_idx,
                    scheme,
                    capability,
                    no_color,
                    Some(head),
                    text_width,
                );
            }
            None => s.push_str("\x1b[K"), // 行が無ければクリア（ガターも空白のまま）
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
        cursor_row,
        cursor_col,
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

    // ターミナルカーソルを primary head へ（Q4: ヒントは仮想テキストなので
    // head より前のヒント幅を表示列に加算する。視覚カーソルは実キャラの
    // スタイル描画のため影響を受けない）。ガター幅分も右へずらす。
    let term_row = cursor_row.saturating_sub(state.first_line) + 1;
    if term_row <= body_rows {
        s.push_str(&format!(
            "\x1b[{};{}H",
            term_row,
            cursor_colw + gutter_shift + 1
        ));
    }
    s.push_str("\x1b[?2026l"); // synchronized output OFF
    s
}

/// 画面を描画する（`out` への書き込み）。テストではバッファへ書き出せる。
///
/// [`draw_with_cache`] の単発版（キャッシュは毎回新規 — テスト・1回描画用）。
/// TUI ループでは [`draw_with_cache`] で LineIndex をフレーム間再利用する。
pub(crate) fn draw_with_cache(
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
    cache: &mut LineIndexCache,
) -> std::io::Result<()> {
    out.write_all(
        render_text_with_cache(
            scheme,
            capability,
            no_color,
            state,
            pending,
            command_line,
            flash,
            width,
            height,
            cache,
        )
        .as_bytes(),
    )
}

/// 1行分を描画する。選択範囲は反転、診断範囲は下線、ハイライトグループは前景色、
/// カーソルセルは青背景。inlay hint は仮想テキストとして位置に挟み込む（ADR-0020）。
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
    inlay_hints: &[InlayHint],
    hint_idx: &mut usize, // inlay_hints は position 昇順（#22 の stable sort の不変条件）
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    cursor: Option<usize>,
    width: usize,
) {
    let mut out_width = 0usize;
    let mut style = (false, false, None, None, false); // (カーソル, 選択中, 診断ロール, グループ, ヒント)
    // ヒントは行を跨いで昇順に進む。前行の終端を超えたヒントを読み飛ばす
    // （行ごとに 0 から歩き直すと O(行数×ヒント数) — highlights と同じ流儀）。
    while inlay_hints
        .get(*hint_idx)
        .is_some_and(|h| h.position < line_char_start)
    {
        *hint_idx += 1;
    }
    'line: for (i, ch) in line.chars().enumerate() {
        let char_global = line_char_start + i;
        // 位置 `char_global` のキャラの直前に、position 昇順のヒントを挟み込む
        while let Some(h) = inlay_hints.get(*hint_idx).filter(|h| h.position == char_global) {
            if !emit_hint(s, h, scheme, capability, no_color, &mut style, &mut out_width, width) {
                break 'line; // ヒントが幅超過: 行末まで打ち切り（実キャラと同じ扱い）
            }
            *hint_idx += 1;
        }
        // CRLF の \r: 非表示文字。生出力するとターミナルがカーソルを行頭へ戻し、
        // 直後の \x1b[K で行全体が消える（H2）。
        if ch == '\r' {
            continue;
        }
        let is_ctrl = ch.is_control() && ch != '\t';
        // 制御文字（ESC 等）は端末インジェクション対策で � に置換してから出力する
        let w = if is_ctrl { 1 } else { ch.width().unwrap_or(0) };
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
        let new_style = (is_cursor, in_sel, diag_role, group, false);
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
    // 行末（最後の文字の直後）のヒントを挟み込む
    let line_end = line_char_start + line.chars().count();
    while let Some(h) = inlay_hints.get(*hint_idx).filter(|h| h.position == line_end) {
        if !emit_hint(s, h, scheme, capability, no_color, &mut style, &mut out_width, width) {
            break;
        }
        *hint_idx += 1;
    }
    // カーソルが行末（最後の文字の直後）にある場合: 青背景の空白で可視化する
    if let Some(c) = cursor {
        if c == line_end && out_width < width {
            let diag_role = diagnostics
                .iter()
                .find(|d| d.start <= c && c < d.end)
                .map(diag_role);
            s.push_str(&style_sgr(scheme, capability, no_color, (true, false, diag_role, None, false)));
            s.push(' ');
            s.push_str("\x1b[0m");
        }
    }
    if style != (false, false, None, None, false) {
        s.push_str("\x1b[0m");
    }
    s.push_str("\x1b[K"); // 行末までクリア
}

/// ヒント1件を仮想テキストとして描画する（padding 含む）。スタイルは常に
/// `UiRole::InlayHint`（カーソル・選択・診断・グループの対象外 — 仮想テキスト）。
///
/// 幅予算は実キャラと同じ扱いで消費し、超過したら途中で打ち切って false を返す
/// （呼び出し側は行描画を止める）。スタイル遷移より先に幅を判定する（宙に浮く
/// SGR を出さない — 実キャラと同じ原則）。
fn emit_hint(
    s: &mut String,
    h: &InlayHint,
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    style: &mut (bool, bool, Option<UiRole>, Option<HighlightGroup>, bool),
    out_width: &mut usize,
    width: usize,
) -> bool {
    if h.padding_left && !emit_hint_char(s, ' ', scheme, capability, no_color, style, out_width, width) {
        return false;
    }
    for c in h.text.chars() {
        if !emit_hint_char(s, c, scheme, capability, no_color, style, out_width, width) {
            return false;
        }
    }
    if h.padding_right && !emit_hint_char(s, ' ', scheme, capability, no_color, style, out_width, width) {
        return false;
    }
    true
}

/// ヒントの1キャラを描画する。幅超過で打ち切ったら false。
fn emit_hint_char(
    s: &mut String,
    ch: char,
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    style: &mut (bool, bool, Option<UiRole>, Option<HighlightGroup>, bool),
    out_width: &mut usize,
    width: usize,
) -> bool {
    let w = ch.width().unwrap_or(0);
    if *out_width + w > width {
        return false;
    }
    let hint_style = (false, false, None, None, true);
    if hint_style != *style {
        *style = hint_style;
        s.push_str(&style_sgr(scheme, capability, no_color, hint_style));
    }
    s.push(ch);
    *out_width += w;
    true
}

/// ヒント1件の表示幅（padding のスペース + ラベル幅）。端末カーソル列の加算用。
fn hint_display_width(h: &InlayHint) -> usize {
    (h.padding_left as usize)
        + h.text.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>()
        + (h.padding_right as usize)
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
/// ヒントは仮想テキストの専用スタイル（カーソル・選択・診断の対象外）。
///
/// カーソル・選択はグループ色を置換し、診断の下線はカーソル/選択と合成する
/// （現行 4;44 / 4;7 を維持）。診断範囲内では「下線 + 診断色」がグループ色を
/// 置換する（M4 で M3 の共存ルールを置き換え — 仕様書 M3 の優先順位と整合）。
fn style_sgr(
    scheme: &Colorscheme,
    capability: ColorCapability,
    no_color: bool,
    style: (bool, bool, Option<UiRole>, Option<HighlightGroup>, bool), // (カーソル, 選択, 診断, グループ, ヒント)
) -> String {
    let (cursor, in_sel, diag_role, group, hint) = style;
    if hint {
        return ui_sgr(scheme, capability, no_color, UiRole::InlayHint);
    }
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
    if st.dim {
        parts.push("2".to_string()); // ディム
    }
    if st.italic {
        parts.push("3".to_string()); // 斜体
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
    cursor_row: usize,
    cursor_col: usize,
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
        // テキストを SGR なしで組み立ててから幅で切り詰め、最後に mode をマーカー色で
        // 包む — エスケープを幅に数えると truncate がエスケープ途中で切れて
        // 壊れた CSI を出力する（敵対的検証で発見）。
        // mode はマーカーチップ（前後に余白込み）として色分けし、残りは無地で描く。
        let (mode, mode_role) = match state.mode {
            Mode::Normal => (" NORMAL ", UiRole::ModeNormal),
            Mode::Insert => (" INSERT ", UiRole::ModeInsert),
            Mode::Select => (" SELECT ", UiRole::ModeSelect),
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
        let (row, col) = (cursor_row, cursor_col); // 呼び出し側で LineIndex から算出済み
        // 行・列は 1 始まり（ガターの行番号と揃える — vim/helix と同じ表示規約）
        text.push_str(&format!(" {path}{dirty}  {}:{}", row + 1, col + 1));
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
        if let Some(rest) = text.strip_prefix(mode) {
            // mode が丸ごと残った: mode だけマーカー色で包み、残りは無地
            status.push_str(&ui_sgr(scheme, capability, no_color, mode_role));
            status.push_str(mode);
            status.push_str("\x1b[0m");
            status.push_str(rest);
        } else {
            // 切り詰めが mode の途中に入った: 残り全部をマーカー色で包む
            status.push_str(&ui_sgr(scheme, capability, no_color, mode_role));
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
///
/// 表示幅には、head と同じ行内で head より前に挟み込まれる inlay hint の幅が
/// 含まれる（Q4: 端末カーソル列は仮想テキスト分も右へずれる）。`hints` は
/// position 昇順（#22 の不変条件）。
/// カーソル位置（行・列・表示列幅）を計算する。`head` は char インデックス。
///
/// [`LineIndex`] ベースの [`cursor_pos_at`] への委譲（テスト用。内部で
/// LineIndex を構築する）。TUI は [`cursor_pos_at`] をキャッシュ済み
/// LineIndex で呼ぶ。
#[cfg(test)]
fn cursor_pos(text: &str, head: usize, hints: &[InlayHint]) -> (usize, usize, usize) {
    cursor_pos_at(&LineIndex::new(text), text, head, hints)
}

/// カーソル位置（行・列・表示列幅）を [`LineIndex`] から計算する。
/// O(log n + 行長) — 全文を head まで歩かない（1MB の末尾で ~1ms の走査を回避）。
///
/// 意味論は従来の全文走査版と同一:
/// - `\r` は非表示（CRLF）なので行・列に数えない（H2）
/// - 行頭より前のヒントは列に影響しない（ADR-0020。1行目のヒントが2行目の列に漏れない）
/// - `position == head` のヒント（行末カーソル直前）も表示列に加算する
/// - 行末の `\n` の位置にカーソルがあるときは前行末扱い（列 0 の次行ではない）
fn cursor_pos_at(
    lines: &LineIndex,
    text: &str,
    head: usize,
    hints: &[InlayHint],
) -> (usize, usize, usize) {
    // 行 = head が属する行（char_starts は行先頭 char 位置の昇順）。
    // head が行末の `\n` の位置ならその行のまま（前行末 = 列 0 の次行ではない）。
    let row = lines
        .char_starts
        .partition_point(|&cs| cs <= head)
        .saturating_sub(1);
    let line_char_start = lines.char_starts[row];
    let (bs, be) = lines.byte_range(row).expect("row は常に有効");
    // 行内の head までの文字を走査（`\r` は列・幅に数えない）。
    let target = head - line_char_start;
    let mut col = 0usize;
    let mut colw = 0usize;
    for (_, ch) in text[bs..be].char_indices().take(target) {
        if ch != '\r' {
            col += 1;
            colw += ch.width().unwrap_or(0);
        }
    }
    // この行の head までの位置に挟まるヒント（position ∈ [行頭, head]）が
    // 表示列を右へ押す。前行のヒントは行頭より前なので除外される。
    // position 昇順（#22 の stable sort の不変条件）なので二分探索で先頭を引く。
    let mut hint_i = hints.partition_point(|h| h.position < line_char_start);
    while let Some(h) = hints.get(hint_i) {
        if h.position > head {
            break;
        }
        colw += hint_display_width(h);
        hint_i += 1;
    }
    (row, col, colw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

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
        // ガター（左余白 1 + 4 桁 + 空白 1 = 6）分右へ: 表示列 3 → 端末列 10
        assert!(out.contains("\x1b[1;10H"), "カーソル位置エスケープ: {out:?}");
        assert!(out.contains("\x1b[?2026l"), "同期出力 OFF で閉じる");
    }

    #[test]
    fn gutter_shows_one_based_numbers_with_active_highlight() {
        // ガター: 左余白 1 + 4 桁右詰め + 空白 1。カーソル行（2 行目）は白 (97)、他は灰 (90)
        let state = state_with("a\nb\nc", vec![Range { anchor: 3, head: 3 }], 0); // head = 2 行目末
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[90m    1 \x1b[0ma"), "1行目: 灰の 1: {out:?}");
        assert!(out.contains("\x1b[97m    2 \x1b[0mb"), "2行目: 白の 2（カーソル行）: {out:?}");
        assert!(out.contains("\x1b[90m    3 \x1b[0mc"), "3行目: 灰の 3: {out:?}");
    }

    #[test]
    fn gutter_pads_and_follows_viewport_offset() {
        // ガターは 4 桁固定（右詰め）。first_line=5 → 6..10 を表示、EOF 以降は番号なし
        let text = (0..10).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
        let mut state = state_with(&text, vec![Range { anchor: 0, head: 0 }], 0);
        state.first_line = 5;
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains("\x1b[90m    6 \x1b[0m"), "1 桁の番号が 4 桁枠で右詰め: {out:?}");
        assert!(out.contains("\x1b[90m   10 \x1b[0m"), "2 桁の番号も 4 桁枠内: {out:?}");
        assert!(!out.contains("\x1b[90m5 "), "first_line より前の行番号を出さない: {out:?}");
    }

    #[test]
    fn status_line_is_one_based() {
        // 行番号は 1 始まり（ガターと揃える）。head が 2 行目の 2 文字目 → "2:2"
        let state = state_with("ab\ncd", vec![Range { anchor: 4, head: 4 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(out.contains(" 2:2"), "ステータスの行:列は 1 始まり: {out:?}");
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
        // 幅9（ガター 6 を除きテキスト幅 3）: "あ" (2) まで描画。切り詰め時に宙に浮く SGR を出さない
        let mut state = state_with("あいう", vec![Range { anchor: 3, head: 3 }], 0);
        state.highlights =
            vec![HighlightRange { start: 0, end: 3, group: HighlightGroup::String }];
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 9, 10);
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
        // 壊れない（幅 10 < " NORMAL " 8 + 残りの幅）
        let state = state_with("x", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 10, 10);
        assert!(
            out.contains("\x1b[30;104m NORMAL \x1b[0m"),
            "mode のマーカー色ラップとリセットが壊れない: {out:?}"
        );
        assert!(
            !out.contains("\x1b[30;104m NORMAL \x1b[K"),
            "エスケープが \x1b[K に飲み込まれない: {out:?}"
        );
    }

    #[test]
    fn mode_marker_has_per_mode_color_and_padding() {
        // モードごとにマーカーチップの色が変わり、前後に余白（空白 1 セル）が入る
        let cases = [
            (Mode::Normal, "\x1b[30;104m NORMAL "),
            (Mode::Insert, "\x1b[30;102m INSERT "),
            (Mode::Select, "\x1b[30;105m SELECT "),
        ];
        for (mode, chip) in cases {
            let mut state = state_with("x", vec![Range { anchor: 0, head: 0 }], 0);
            state.mode = mode;
            let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
            assert!(out.contains(chip), "{:?}: {out:?}", mode);
            assert!(out.contains("\x1b[0m"), "{:?} の後にリセット: {out:?}", mode);
        }
        // マーカーは無地の残り（パス等）と区別される — 反転の StatusLine は使わない
        let mut state = state_with("x", vec![Range { anchor: 0, head: 0 }], 0);
        state.mode = Mode::Insert;
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 40, 10);
        assert!(!out.contains("\x1b[7mINSERT"), "反転チップは使わない: {out:?}");
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
            name: Cow::Borrowed("probe"),
            syntax: Cow::Borrowed(&[]),
            ui: Cow::Borrowed(&[(UiRole::Selection, Style {
                fg: None,
                bg: Some(crate::colorscheme::Color::Ansi(0)),
                underline: false,
                reverse: true,
                dim: false,
                italic: false,
            })]),
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
        // "あ" は表示幅2。幅9（ガター 6 を除きテキスト幅 3）なら "あ" で切れ、"あい" にはならない
        let state = state_with("あいうえお", vec![Range { anchor: 0, head: 0 }], 0);
        let out = render_text(&crate::colorscheme::DEFAULT, ColorCapability::Ansi16, false, &state, &[], None, None, 9, 10);
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
        assert_eq!(cursor_pos("ab\r", 3, &[]), (0, 2, 2));
        assert_eq!(cursor_pos("a\r\nb", 3, &[]), (1, 0, 0));
    }

    #[test]
    fn cursor_pos_includes_hint_widths_before_head() {
        // Q4: ヒントは仮想テキスト。head と同じ行内で head より前に挟まる
        // ヒントの幅は端末カーソル列に加算される。
        let hints = |text: &str| -> Vec<InlayHint> {
            vec![InlayHint {
                position: text.find("x").unwrap() + 1, // x の直後
                text: ": i32".into(),
                padding_left: false,
                padding_right: false,
            }]
        };
        // "let x = 5": x は char 4。ヒント位置 5（x の直後の空白）→ ラベル幅 5（": i32"）
        let text = "let x = 5";
        assert_eq!(cursor_pos(text, 6, &hints(text)), (0, 6, 11), "= の表示列にヒント幅が加算");
        // head がヒント位置（x の直後の空白）でも同様: ヒントはそのキャラの直前に挟まる
        assert_eq!(cursor_pos(text, 5, &hints(text)), (0, 5, 10));
        // head がヒントより前なら加算しない
        assert_eq!(cursor_pos(text, 3, &hints(text)), (0, 3, 3));
        // 別の行のヒントは現在行のカーソル列に影響しない
        let text2 = "let x = 5\nlet y = 1";
        let h2 = vec![InlayHint {
            position: 5,
            text: ": i32".into(),
            padding_left: false,
            padding_right: false,
        }];
        assert_eq!(cursor_pos(text2, 12, &h2), (1, 2, 2), "1行目のヒントは2行目の列に影響しない");
    }

    // ---- inlay hint 描画（ADR-0020 / #24） ----

    /// draw_line の出力を返す（テスト用。SGR 込み）。
    fn draw_line_out(
        line: &str,
        char_start: usize,
        hints: &[InlayHint],
        cursor: Option<usize>,
        width: usize,
    ) -> String {
        let mut s = String::new();
        let mut hl = 0usize;
        let mut hi = 0usize;
        draw_line(
            &mut s,
            line,
            char_start,
            &[],
            &[],
            &[],
            &mut hl,
            hints,
            &mut hi,
            &crate::colorscheme::DEFAULT,
            ColorCapability::Ansi16,
            false,
            cursor,
            width,
        );
        s
    }

    /// SGR・行クリア（\x1b[..m / \x1b[K）を除去してプレーン文字列にする。
    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.next(); // [
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn inlay_hint_renders_between_chars() {
        // ヒントは位置のキャラの直前に仮想テキストとして挟まる（"let x" + ": i32" + " = 5"）
        let hints = vec![InlayHint {
            position: 5,
            text: ": i32".into(),
            padding_left: false,
            padding_right: false,
        }];
        let out = draw_line_out("let x = 5", 0, &hints, None, 40);
        assert_eq!(strip_sgr(&out), "let x: i32 = 5", "x と = の間に挟まる: {out:?}");
        assert!(out.contains("\x1b[2;3m"), "既定スタイルはディム+斜体 (2;3): {out:?}");
    }

    #[test]
    fn inlay_hint_padding_reflects_flags() {
        // paddingLeft / paddingRight の空白がラベル前後に付く
        let hints = vec![InlayHint {
            position: 1,
            text: "i32".into(),
            padding_left: true,
            padding_right: true,
        }];
        let out = draw_line_out("x = 5", 0, &hints, None, 40);
        assert_eq!(strip_sgr(&out), "x i32  = 5", "padding の空白が反映される: {out:?}");
    }

    #[test]
    fn inlay_hint_visible_without_color() {
        // NO_COLOR でもディム+斜体は属性なので視認できる（色だけが落ちる）
        let hints = vec![InlayHint {
            position: 5,
            text: ": i32".into(),
            padding_left: false,
            padding_right: false,
        }];
        let mut s = String::new();
        let mut hl = 0usize;
        let mut hi = 0usize;
        draw_line(
            &mut s,
            "let x = 5",
            0,
            &[],
            &[],
            &[],
            &mut hl,
            &hints,
            &mut hi,
            &crate::colorscheme::DEFAULT,
            ColorCapability::Ansi16,
            true, // NO_COLOR
            None,
            40,
        );
        assert!(s.contains("\x1b[2;3m"), "NO_COLOR でも属性で視認できる: {s:?}");
    }

    #[test]
    fn inlay_hints_at_same_position_keep_order() {
        // 同 position の複数ヒントは応答順に挟まる（#22 の stable sort の不変条件）
        let hints = vec![
            InlayHint {
                position: 1,
                text: "a".into(),
                padding_left: false,
                padding_right: false,
            },
            InlayHint {
                position: 1,
                text: "b".into(),
                padding_left: false,
                padding_right: false,
            },
        ];
        let out = draw_line_out("xy", 0, &hints, None, 40);
        assert_eq!(strip_sgr(&out), "xaby", "同 position は応答順: {out:?}");
    }

    #[test]
    fn inlay_hint_never_takes_cursor_or_selection_style() {
        // カーソル・選択は実キャラにのみ適用され、ヒントは常に専用スタイル。
        // "let x = 5": x=char4（選択）、ヒント位置 5、=は char6（カーソル）。
        let hints = vec![InlayHint {
            position: 5,
            text: ": i32".into(),
            padding_left: false,
            padding_right: false,
        }];
        let mut s = String::new();
        let mut hl = 0usize;
        let mut hi = 0usize;
        let sel = vec![Range { anchor: 4, head: 5 }];
        draw_line(
            &mut s,
            "let x = 5",
            0,
            &sel,
            &[],
            &[],
            &mut hl,
            &hints,
            &mut hi,
            &crate::colorscheme::DEFAULT,
            ColorCapability::Ansi16,
            false,
            Some(6),
            40,
        );
        assert!(s.contains("\x1b[2;3m: i32"), "ヒントは専用スタイル: {s:?}");
        assert!(s.contains("\x1b[7mx"), "選択は x にのみ: {s:?}");
        assert!(s.contains("\x1b[44m="), "カーソルは = にのみ: {s:?}");
        assert!(!s.contains("\x1b[44m: i32"), "ヒントにカーソルスタイルを出さない: {s:?}");
    }

    #[test]
    fn inlay_hint_truncates_mid_label() {
        // 幅予算はヒント分も消費し、ヒントは実キャラと同じ扱いで途中で切れる。
        // "let x"=5 + ": i"=3 → 8 ちょうど。次の '3' で幅超過（ラベル ": i32" の空白込み）。
        let hints = vec![InlayHint {
            position: 5,
            text: ": i32".into(),
            padding_left: false,
            padding_right: false,
        }];
        let out = draw_line_out("let x = 5", 0, &hints, None, 8);
        assert_eq!(strip_sgr(&out), "let x: i", "幅超過の途中で切れる: {out:?}");
        assert!(out.ends_with("\x1b[0m\x1b[K"), "宙に浮くスタイルを残さない: {out:?}");
    }

    #[test]
    fn inlay_hint_at_line_end_renders_after_last_char() {
        // 行末（最後の文字の直後）のヒントも描画される
        let hints = vec![InlayHint {
            position: 5,
            text: "// eol".into(),
            padding_left: true,
            padding_right: false,
        }];
        let out = draw_line_out("let x", 0, &hints, None, 40);
        assert_eq!(strip_sgr(&out), "let x // eol", "行末ヒント: {out:?}");
    }

    #[test]
    fn terminal_cursor_column_includes_hints() {
        // Q4: 端末カーソル列はヒント幅を加算する（\x1b[{row};{col}H）
        let mut state = state_with("let x = 5", vec![Range { anchor: 6, head: 6 }], 0);
        state.inlay_hints = vec![InlayHint {
            position: 5,
            text: ": i32".into(),
            padding_left: false,
            padding_right: false,
        }];
        let out = render_text(
            &crate::colorscheme::DEFAULT,
            ColorCapability::Ansi16,
            false,
            &state,
            &[],
            None,
            None,
            40,
            10,
        );
        // head=6（= の位置）: 表示列 = char 6 + ヒント幅 5 = 11。ガター（6）を足し → 1行目 col 18
        assert!(out.contains("\x1b[1;18H"), "カーソル列にヒント幅が加算: {out:?}");
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
