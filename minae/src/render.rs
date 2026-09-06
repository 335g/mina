//! 描画: StateSnapshot → ratatui（仕様書 §6-§8・§12-§14）。
//!
//! 見た目は P1 プロトタイプで確定（配色の実値は colors.rs の実パレット）。
//! フレーム毎に全画面再構築する（`Buffer::diff` が差分を吸収する）。

use mina_protocol::{Mode, Severity, StateSnapshot};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Overlay};
use crate::colors::{ColorCapability, UiRole};

/// テキストの各行の開始 char オフセット。
pub(crate) fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    let mut idx = 0usize;
    for c in text.chars() {
        if c == '\n' {
            starts.push(idx + 1);
        }
        idx += 1;
    }
    starts
}

/// char オフセット → (0-origin 行, 行内 char 列)。
pub(crate) fn offset_to_line_col(text: &str, offset: usize) -> (usize, usize) {
    let starts = line_starts(text);
    let line = starts.partition_point(|&s| s <= offset).saturating_sub(1);
    (line, offset.saturating_sub(starts[line]))
}

/// プライマリカーソルの (0-origin 行, 行内 char 列)。
pub(crate) fn cursor_line_col(snap: &StateSnapshot) -> (usize, usize) {
    let head = snap
        .selection
        .get(snap.primary_index)
        .map(|r| r.head)
        .unwrap_or(0);
    offset_to_line_col(&snap.text, head)
}

fn severity_rank(s: &Severity) -> u8 {
    match s {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
        Severity::Hint => 3,
    }
}

/// インライン診断マーカー（1 文字。Error > Warning > Info > Hint の最上位）。
fn severity_marker(s: &Severity) -> &'static str {
    match s {
        Severity::Error => "✗",
        Severity::Warning => "!",
        Severity::Info => "i",
        Severity::Hint => "i",
    }
}

fn severity_role(s: &Severity) -> UiRole {
    match s {
        Severity::Error => UiRole::DiagnosticError,
        Severity::Warning => UiRole::DiagnosticWarning,
        Severity::Info => UiRole::DiagnosticInfo,
        Severity::Hint => UiRole::DiagnosticHint,
    }
}

fn severity_name(s: &Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
        Severity::Hint => "hint",
    }
}

/// スピナーの 1 フレーム（braille）。
fn spinner(tick: u64) -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    FRAMES[(tick as usize) % FRAMES.len()]
}

/// スタイル解決のヘルパー（NO_COLOR では無装飾、能力に応じて近似）。
fn st(app: &App, style: Style) -> Style {
    if app.no_color {
        Style::default()
    } else {
        style
    }
}

/// UI ロールの ratatui スタイル（未掲載は無色）。
fn ui(app: &App, role: UiRole) -> Style {
    st(
        app,
        app.scheme
            .ui_style(role)
            .map(|s| s.to_ratatui(app.capability))
            .unwrap_or_default(),
    )
}

/// シンタックスグループの ratatui スタイル（未掲載は無色）。
fn syn(app: &App, group: mina_protocol::HighlightGroup) -> Style {
    st(
        app,
        app.scheme
            .syntax_style(group)
            .map(|s| s.to_ratatui(app.capability))
            .unwrap_or_default(),
    )
}

/// エディタのベース（背景 + 前景）。StatusLine の背景と明るい行番号色を使う。
fn base(app: &App) -> Style {
    let bg = app
        .scheme
        .ui_style(UiRole::StatusLine)
        .and_then(|s| s.bg)
        .map(|c| {
            crate::colors::adapt_color(c, app.capability).to_ratatui()
        });
    let fg = app
        .scheme
        .ui_style(UiRole::LineNumberActive)
        .and_then(|s| s.fg)
        .map(|c| {
            crate::colors::adapt_color(c, app.capability).to_ratatui()
        });
    let mut style = Style::default();
    if !app.no_color {
        if let Some(bg) = bg {
            style = style.bg(bg);
        }
        if let Some(fg) = fg {
            style = style.fg(fg);
        }
    }
    style
}

/// 10 進の桁数。
fn digits(mut n: usize) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// メイン描画。
pub(crate) fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(base(app)), area);

    // レイアウト: [本文] / [ステータス行 1 行]
    let [body, status_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    app.body_h = body.height as usize;

    // 下側オーバーレイ（診断/activity/peek）は本文の下 ~30% を使う
    let (editor_area, overlay_area) = match app.overlay {
        Overlay::Diagnostics | Overlay::Activity | Overlay::Peek => {
            let h = (body.height * 30 / 100).max(5).min(body.height);
            let eh = body.height.saturating_sub(h);
            (
                Rect {
                    height: eh,
                    ..body
                },
                Some(Rect {
                    y: body.y + eh,
                    height: h,
                    ..body
                }),
            )
        }
        _ => (body, None),
    };

    draw_editor(f, app, editor_area);
    if let Some(area) = overlay_area {
        match app.overlay {
            Overlay::Diagnostics => draw_diag_view(f, app, area),
            Overlay::Activity => draw_activity_view(f, app, area),
            Overlay::Peek => draw_peek_view(f, app, area),
            _ => {}
        }
    }
    if app.overlay == Overlay::Tree {
        draw_tree(f, app, body);
    }
    draw_status(f, app, status_area);

    // カーソル（エディタ可視時のみ。ツリー全画面時は置かない）
    if app.overlay != Overlay::Tree {
        draw_cursor(f, app, editor_area);
    }
}

/// エディタ本文の描画（ガター + インライン診断マーカー + ハイライト）。
fn draw_editor(f: &mut Frame, app: &App, area: Rect) {
    let snap = &app.snapshot;
    if area.height == 0 {
        return;
    }
    let lines: Vec<&str> = snap.text.split('\n').collect();
    let total = lines.len();
    let num_w = digits(total).max(3);
    let highlights = &snap.highlights;
    let starts = line_starts(&snap.text);
    let (cur_line, _) = cursor_line_col(snap);

    let mut rendered = Vec::new();
    for r in 0..area.height as usize {
        let row = snap.first_line + r;
        if row >= total {
            rendered.push(Line::from(Span::styled("", base(app))));
            continue;
        }
        let line_no = row + 1;
        let line = lines[row];
        let line_chars: Vec<char> = line.chars().collect();
        let ls = starts[row];
        let le = ls + line_chars.len();

        // 診断インラインマーカーは行番号より左の固定列（1 文字幅）
        let mut spans = vec![if let Some(sev) = snap
            .diagnostics
            .iter()
            .filter(|d| {
                let s = d.start.min(d.end.max(d.start));
                let e = d.end.max(d.start);
                // char 範囲 → 行カバレッジ（空範囲は開始行）
                let first = offset_to_line_col(&snap.text, s.min(le.max(ls))).0;
                let last = if e > s {
                    offset_to_line_col(&snap.text, e.saturating_sub(1).max(s)).0
                } else {
                    first
                };
                row >= first && row <= last
            })
            .min_by_key(|d| severity_rank(&d.severity))
            .map(|d| &d.severity)
        {
            Span::styled(severity_marker(sev), ui(app, severity_role(sev)))
        } else {
            Span::raw(" ")
        }];
        spans.push(Span::raw("  "));

        // ガター: 行番号（1 始まり・右詰め固定幅）。カーソル行は明るく。
        let num_style = if row == cur_line {
            ui(app, UiRole::LineNumberActive)
        } else {
            ui(app, UiRole::LineNumber)
        };
        spans.push(Span::styled(
            format!("{line_no:>num_w$} "),
            num_style,
        ));

        // 本文: ハイライト範囲で区切ってスタイル付け
        let base_style = base(app);
        let mut pos = ls;
        // 当該行に掛かる範囲だけを走査する（highlights は昇順・非重複）
        for hl in highlights.iter().filter(|h| h.start < le && h.end > ls) {
            let seg_start = hl.start.max(ls);
            let seg_end = hl.end.min(le);
            if seg_start > pos {
                spans.push(Span::styled(
                    line_chars[pos - ls..seg_start - ls].iter().collect::<String>(),
                    base_style,
                ));
            }
            spans.push(Span::styled(
                line_chars[seg_start - ls..seg_end - ls]
                    .iter()
                    .collect::<String>(),
                base_style.patch(syn(app, hl.group)),
            ));
            pos = seg_end;
        }
        if pos < le {
            spans.push(Span::styled(
                line_chars[pos - ls..].iter().collect::<String>(),
                base_style,
            ));
        }
        rendered.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(rendered).style(base(app)), area);
}

/// カーソル描画（プライマリカーソルを自色で — 端末カーソルを使う）。
fn draw_cursor(f: &mut Frame, app: &App, area: Rect) {
    let snap = &app.snapshot;
    let (line, col) = cursor_line_col(snap);
    if line < snap.first_line {
        return;
    }
    let rel = line - snap.first_line;
    if rel >= area.height as usize {
        return;
    }
    let gutter_w = gutter_width(snap);
    let line_text: String = snap
        .text
        .split('\n')
        .nth(line)
        .unwrap_or("")
        .chars()
        .take(col)
        .collect();
    let disp_col = UnicodeWidthStr::width(line_text.as_str()) as u16;
    let x = area
        .x
        .saturating_add(gutter_w + disp_col)
        .min(area.x + area.width.saturating_sub(1));
    f.set_cursor_position((x, area.y + rel as u16));
}

/// ガター幅（マーカー 1 + 空白 2 + 行番号 + 空白 1）。
fn gutter_width(snap: &StateSnapshot) -> u16 {
    let total = snap.text.split('\n').count();
    (1 + 2 + digits(total).max(3) + 1) as u16
}

/// ステータス行（左: モード + パス / 右: 報知 + キーヒント）。プロンプト中は入力行。
fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    if let Some(prompt) = &app.prompt {
        let line = Line::from(vec![Span::styled(
            format!("{}{}", prompt.prefix(), prompt.buf()),
            ui(app, UiRole::CommandLine),
        )]);
        f.render_widget(Paragraph::new(line).style(base(app)), area);
        // プロンプト末尾にカーソル
        let x = area
            .x
            .saturating_add(1 + prompt.buf().chars().count() as u16)
            .min(area.x + area.width.saturating_sub(1));
        f.set_cursor_position((x, area.y));
        return;
    }
    let snap = &app.snapshot;
    let (mode_txt, mode_role) = match snap.mode {
        Mode::Normal => ("NORMAL", UiRole::ModeNormal),
        Mode::Insert => ("INSERT", UiRole::ModeInsert),
        Mode::Select => ("SELECT", UiRole::ModeSelect),
    };
    let path = snap.path.as_deref().unwrap_or("[No Name]");
    let dirty = if snap.dirty { " [+]" } else { "" };
    let left = format!("{mode_txt}  {path}{dirty}");

    // 右: 報知エリア（スピナー + 今の活動）+ キーヒント
    let mut right = String::new();
    if let Some(flash) = &app.flash {
        right.push_str(flash);
        right.push_str("   ");
    } else if !snap.activities.is_empty() {
        let labels: Vec<_> = snap.activities.iter().map(|a| a.label.as_str()).collect();
        right.push_str(&format!("{} {}   ", spinner(app.tick), labels.join(" / ")));
    } else if let Some(rec) = snap.activity.last() {
        right.push_str(&format!(
            "{} {:?} {}{}   ",
            rec.actor,
            rec.kind,
            rec.detail,
            if rec.ok { "" } else { " FAILED" }
        ));
    } else if let Some(ev) = snap.events.last() {
        right.push_str(&format!("{:?}   ", ev.kind));
    }
    if app.conn.is_none() {
        right.push_str("[未接続]   ");
    }
    right.push_str("T:ツリー G:診断 A:履歴 C:配色");

    let status_style = ui(app, UiRole::StatusLine);
    let left_w = UnicodeWidthStr::width(left.as_str());
    let right_w = UnicodeWidthStr::width(right.as_str());
    let pad = (area.width as usize).saturating_sub(left_w + right_w);
    let line = Line::from(vec![
        Span::styled(format!(" {left} "), ui(app, mode_role)),
        Span::styled(" ".repeat(pad), status_style),
        Span::styled(format!(" {right} "), status_style),
    ]);
    f.render_widget(Paragraph::new(line).style(status_style), area);
}

/// ファイルツリー（ほぼ全画面ピッカー型オーバーレイ）。
fn draw_tree(f: &mut Frame, app: &App, body: Rect) {
    let overlay = Rect {
        x: body.x + 2,
        y: body.y + 1,
        width: body.width.saturating_sub(4),
        height: body.height.saturating_sub(3),
    };
    if overlay.width < 10 || overlay.height < 5 {
        return;
    }
    let open = app.snapshot.path.as_deref();
    let items: Vec<ListItem> = app
        .tree
        .entries
        .iter()
        .enumerate()
        .map(|(idx, e)| {
            let mark = if !e.is_dir && Some(e.path.to_string_lossy().as_ref()) == open {
                "●"
            } else {
                " "
            };
            let icon = if e.is_dir {
                if app.tree.expanded.contains(&e.path) {
                    "▾"
                } else {
                    "▸"
                }
            } else {
                " "
            };
            let indent = "  ".repeat(e.depth);
            let style = if idx == app.tree.selected {
                base(app).patch(ui(app, UiRole::Selection).bg(ratatui::style::Color::Reset))
            } else {
                base(app)
            };
            // 選択行は反転で強調する
            let style = if idx == app.tree.selected {
                style.patch(Style::default().add_modifier(ratatui::style::Modifier::REVERSED))
            } else {
                style
            };
            ListItem::new(Line::from(vec![Span::styled(
                format!("{mark} {indent}{icon} {}", e.name),
                style,
            )]))
        })
        .collect();
    let block = Block::bordered()
        .title(format!(" ファイルツリー ({}) ", app.tree.root.display()))
        .border_style(ui(app, UiRole::PopupBorder))
        .style(base(app));
    f.render_widget(Clear, overlay);
    f.render_widget(List::new(items).block(block), overlay);
}

/// 診断専用 view（全幅・高さ約 30%・下側固定）。アクティブな診断 1 件のみ表示。
fn draw_diag_view(f: &mut Frame, app: &App, area: Rect) {
    // 重要度マーカーエリア（左上）: 存在する重要度にマーカー、アクティブのみ強調
    let mut markers = String::from(" ");
    for sev in [
        Severity::Error,
        Severity::Warning,
        Severity::Info,
        Severity::Hint,
    ] {
        let count = app
            .snapshot
            .diagnostics
            .iter()
            .filter(|d| d.severity == sev)
            .count();
        let letter = match sev {
            Severity::Error => "E",
            Severity::Warning => "W",
            Severity::Info => "I",
            Severity::Hint => "H",
        };
        if count > 0 {
            markers.push_str(&format!("{letter}:{count} "));
        }
    }
    let active = app.active_diag();
    let mut lines = vec![Line::from(vec![
        Span::styled(markers, ui(app, UiRole::PopupBorder)),
        Span::styled(
            format!(
                "[{}][{}] j:次 k:前 e/w/i/h:絞込 Tab:切替 G:閉じる",
                severity_name(&app.diag_filter),
                match app.diag_focus {
                    crate::app::DiagFocus::View => "view",
                    crate::app::DiagFocus::Editor => "editor",
                }
            ),
            base(app),
        ),
    ])];
    match active {
        Some((start, msg)) => {
            let (line, col) = offset_to_line_col(&app.snapshot.text, start);
            lines.push(Line::from(vec![Span::styled(
                format!(
                    "{} {}:{} {}",
                    severity_name(&app.diag_filter),
                    line + 1,
                    col + 1,
                    app.snapshot
                        .path
                        .as_deref()
                        .unwrap_or("[No Name]")
                ),
                ui(app, severity_role(&app.diag_filter)),
            )]));
            for mline in msg.lines() {
                lines.push(Line::from(vec![Span::styled(
                    format!("  {mline}"),
                    base(app),
                )]));
            }
        }
        None => lines.push(Line::from(vec![Span::styled(
            "  （診断なし）",
            base(app),
        )])),
    }
    let block = Block::bordered()
        .title(" 診断 ")
        .border_style(ui(app, UiRole::PopupBorder))
        .style(base(app));
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// 活動履歴 view（全幅・高さ約 30%・下側固定。診断 view と同型）。
fn draw_activity_view(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<_> = app
        .snapshot
        .activity
        .iter()
        .filter(|r| app.activity_filter.keeps(r))
        .collect();
    let h = area.height.saturating_sub(2) as usize; // 枠分
    let start = if app.activity_scroll == usize::MAX || app.activity_scroll + 1 >= rows.len() {
        rows.len().saturating_sub(h)
    } else {
        app.activity_scroll.min(rows.len().saturating_sub(1))
    };
    let mut lines = vec![Line::from(vec![Span::styled(
        format!(
            " フィルタ: {} (f:切替 j/k:移動 A:閉じる) ",
            app.activity_filter.label()
        ),
        base(app),
    )])];
    for rec in rows.iter().skip(start).take(h.saturating_sub(1)) {
        let mark = if rec.ok { "✓" } else { "✗" };
        let style = if rec.ok {
            base(app)
        } else {
            base(app).patch(ui(app, UiRole::DiagnosticError))
        };
        lines.push(Line::from(vec![Span::styled(
            format!("{} {} {:?} {}", mark, rec.actor, rec.kind, rec.detail),
            style,
        )]));
    }
    if rows.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  （活動なし）",
            base(app),
        )]));
    }
    let block = Block::bordered()
        .title(" 活動履歴 ")
        .border_style(ui(app, UiRole::PopupBorder))
        .style(base(app));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Peek パネル（MVP 簡易表示 — 定義の確認用スニペット）。
fn draw_peek_view(f: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if let Some(peek) = &app.peek {
        lines.push(Line::from(vec![Span::styled(
            format!("{}:{}", peek.path, peek.line),
            ui(app, UiRole::PopupBorder),
        )]));
        for pline in peek.text.lines() {
            lines.push(Line::from(vec![Span::styled(
                format!("  {pline}"),
                base(app),
            )]));
        }
    }
    let block = Block::bordered()
        .title(" 定義 (Esc:閉じる) ")
        .border_style(ui(app, UiRole::PopupBorder))
        .style(base(app));
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// capability が 256/16 のときは [`ColorCapability`] に合わせて近似する
/// （Style::to_ratatui 内で adapt 済み — ここは互換のための再掲）。
#[allow(dead_code)]
fn _capability_note(_cap: ColorCapability) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use crate::colors::default_scheme;
    use mina_protocol::{Diagnostic, Range};

    fn test_app() -> App {
        let mut app = App::new(
            default_scheme(),
            PathBuf::from("/tmp/minae-test-schemes"),
            ColorCapability::TrueColor,
            false,
        );
        app.snapshot = StateSnapshot {
            text: "fn main() {\n    let x = 1;\n}\n".into(),
            checksum: 0,
            selection: vec![Range { anchor: 0, head: 0 }],
            primary_index: 0,
            mode: Mode::Normal,
            first_line: 0,
            diagnostics: vec![Diagnostic {
                start: 14,
                end: 15,
                severity: Severity::Warning,
                message: "unused".into(),
            }],
            inlay_hints: vec![],
            highlights: vec![mina_protocol::HighlightRange {
                start: 0,
                end: 2,
                group: mina_protocol::HighlightGroup::Keyword,
            }],
            path: Some("src/main.rs".into()),
            dirty: false,
            status: None,
            activities: vec![],
            activity: vec![],
            generation: 1,
            events: vec![],
            deleted: None,
            peek: None,
        };
        app.width = 80;
        app.height = 24;
        app.body_h = 23;
        app
    }

    fn buffer_text(backend: &ratatui::backend::TestBackend) -> String {
        let buf = backend.buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            // 全角文字の継続セル（空白）は詰める — 実端末では 1 文字に重なる
            let mut skip_next_space = false;
            for x in 0..buf.area.width {
                let s = buf[(x, y)].symbol();
                if skip_next_space && s == " " {
                    skip_next_space = false;
                    continue;
                }
                skip_next_space = UnicodeWidthStr::width(s) == 2;
                out.push_str(s);
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn editor_renders_gutter_markers_path_and_mode() {
        let mut app = test_app();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("src/main.rs"), "パスがステータスに出る");
        assert!(text.contains("NORMAL"), "モードが出る");
        assert!(text.contains('!'), "warning マーカーが出る");
        assert!(text.contains("T:ツリー"), "キーヒントが出る");
    }

    #[test]
    fn overlays_render_without_panic() {
        for overlay in [
            Overlay::Tree,
            Overlay::Diagnostics,
            Overlay::Activity,
            Overlay::Peek,
        ] {
            let mut app = test_app();
            app.overlay = overlay;
            if overlay == Overlay::Peek {
                app.peek = Some(mina_protocol::Peek {
                    path: "src/main.rs".into(),
                    line: 2,
                    text: "let x = 1;".into(),
                });
            }
            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| render(f, &mut app)).unwrap();
            let text = buffer_text(terminal.backend());
            match overlay {
                Overlay::Tree => assert!(text.contains("ファイルツリー")),
                Overlay::Diagnostics => assert!(text.contains("診断")),
                Overlay::Activity => assert!(text.contains("活動履歴")),
                Overlay::Peek => assert!(text.contains("定義")),
                Overlay::None => {}
            }
        }
    }

    #[test]
    fn status_shows_activity_record() {
        let mut app = test_app();
        app.snapshot.activity = vec![mina_protocol::ActivityRecord {
            actor: "agent-1".into(),
            kind: mina_protocol::EventKind::Open,
            ok: false,
            detail: "no such file".into(),
        }];
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("agent-1"), "今の活動が出る");
        assert!(text.contains("FAILED"), "失敗が分かる");
    }
}
