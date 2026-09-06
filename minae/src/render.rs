//! 描画: StateSnapshot → ratatui（仕様書 §6-§8・§12-§14）。
//!
//! 見た目は P1 プロトタイプで確定（配色の実値は colors.rs の実パレット）。
//! フレーム毎に全画面再構築する（`Buffer::diff` が差分を吸収する）。

use mina_protocol::{HighlightGroup, Mode, ReviewSide, Severity, StateSnapshot};
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
use crate::git;

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

    // 比較差分（表示用キャッシュ付き。checksum キーなので変化時のみ git 呼び出し）。
    let cmp_diff = app.compare_diff_for_render();
    let cmp_ref = cmp_diff.as_ref();
    let gap_cur = app.gap_review.map(|g| (g.gap_idx, g.line_idx, g.col));
    draw_editor(f, app, editor_area, cmp_ref, gap_cur);
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
        draw_cursor(f, app, editor_area, cmp_ref, gap_cur);
    }
}

/// 仮想表示行（比較の削除 gap 挿入後）。
enum VRow<'a> {
    Real(usize),
    Gap {
        gap_idx: usize,
        gap: &'a git::Gap,
        line_idx: usize,
    },
}

/// スタイルの fg だけ差し替える（None なら据え置き）。
fn tint(base: Style, fg: Option<ratatui::style::Color>) -> Style {
    match fg {
        Some(c) => base.patch(Style::default().fg(c)),
        None => base,
    }
}

/// 削除 gap の旧番号の最大値（ガター幅用）。
fn max_old_no(diff: Option<&git::FileDiff>) -> usize {
    diff.map(|d| {
        d.gaps
            .iter()
            .map(|g| g.old_start + g.lines.len())
            .max()
            .unwrap_or(0)
    })
    .unwrap_or(0)
}

/// 変更種別の表示色（ツリーの M/A/D・注釈マーカー用）。
fn status_style(app: &App, st: git::ChangeStatus) -> Style {
    let g = match st {
        git::ChangeStatus::Added => HighlightGroup::String,
        git::ChangeStatus::Modified => HighlightGroup::Number,
        git::ChangeStatus::Deleted => HighlightGroup::Error,
    };
    syn(app, g)
}

/// エディタ本文の描画（ガター + インライン診断マーカー + ハイライト + 比較注釈）。
fn draw_editor(
    f: &mut Frame,
    app: &App,
    area: Rect,
    diff: Option<&git::FileDiff>,
    gap_active: Option<(usize, usize, usize)>,
) {
    let snap = &app.snapshot;
    if area.height == 0 {
        return;
    }
    let lines: Vec<&str> = snap.text.split('\n').collect();
    let total = lines.len();
    // ガター幅: 実テキスト行数と削除行の旧番号の大きい方に合わせる。
    let num_w = digits(total.max(max_old_no(diff))).max(3);
    let highlights = &snap.highlights;
    let starts = line_starts(&snap.text);
    let (cur_line, _) = cursor_line_col(snap);

    // 可視の仮想行列（実テキスト行 + 削除 gap 行）。first_line は実テキスト行基準。
    let h = area.height as usize;
    let mut vrows: Vec<VRow> = Vec::new();
    let mut r = snap.first_line;
    loop {
        if vrows.len() >= h {
            break;
        }
        if let Some(d) = diff {
            for (gi, g) in d.gaps.iter().enumerate() {
                if g.at == r {
                    for (li, _) in g.lines.iter().enumerate() {
                        if vrows.len() >= h {
                            break;
                        }
                        vrows.push(VRow::Gap {
                            gap_idx: gi,
                            gap: g,
                            line_idx: li,
                        });
                    }
                }
            }
            if vrows.len() >= h {
                break;
            }
        }
        if r >= total {
            break;
        }
        vrows.push(VRow::Real(r));
        r += 1;
    }

    let mut rendered = Vec::new();
    for vrow in vrows {
        match vrow {
            VRow::Gap {
                gap_idx,
                gap,
                line_idx,
            } => {
                // 旧側のみの行: 旧行番号つきで控えめ色。カーソル対象外。
                // gapレビュー中は対象行を反転強調する。
                let old_no = gap.old_start + line_idx;
                let comment_fg = syn(app, HighlightGroup::Comment).fg;
                let dim = tint(base(app), comment_fg);
                let active =
                    matches!(gap_active, Some((gi, li, _)) if gi == gap_idx && li == line_idx);
                // #50: 基準側コメントのマーカー（`"`）。
                let commented = app.review_list.iter().any(|e| {
                    e.side == ReviewSide::Base && e.line == old_no as u32 && (|| {
                        let cmp = app.compare.as_ref()?;
                        let snap_path = app.snapshot.path.as_deref()?;
                        Some(e.path == cmp.base_path_for(snap_path)?)
                    })()
                    .unwrap_or(false)
                });
                let mut spans = vec![
                    Span::styled("-", dim),
                    Span::raw("  "),
                    Span::styled(
                        format!("{old_no:>num_w$} "),
                        tint(ui(app, UiRole::LineNumber), comment_fg),
                    ),
                    Span::styled(gap.lines[line_idx].clone(), dim),
                ];
                if commented {
                    spans.push(Span::styled(" \"", syn(app, HighlightGroup::String)));
                }
                let spans = if active {
                    spans
                        .into_iter()
                        .map(|s| {
                            Span::styled(
                                s.content,
                                s.style.patch(
                                    Style::default()
                                        .add_modifier(ratatui::style::Modifier::REVERSED),
                                ),
                            )
                        })
                        .collect()
                } else {
                    spans
                };
                rendered.push(Line::from(spans));
            }
            VRow::Real(row) => {
        let line_no = row + 1;
        let line = lines[row];
        let line_chars: Vec<char> = line.chars().collect();
        let ls = starts[row];
        let le = ls + line_chars.len();
        let kind = diff.and_then(|d| d.kinds.get(row).copied());
        let delta_fg = match kind {
            Some(git::RowKind::Modified) => syn(app, HighlightGroup::Number).fg,
            Some(git::RowKind::Added) => syn(app, HighlightGroup::String).fg,
            _ => None,
        };

        // 診断インラインマーカーは行番号より左の固定列（1 文字幅）。
        // 比較中は差分マーカー（+/~/−）が優先する。
        let mut spans = vec![match kind.and_then(|k| k.marker()) {
            Some(m) => {
                let mg = match kind {
                    Some(git::RowKind::Modified) => HighlightGroup::Number,
                    _ => HighlightGroup::String,
                };
                Span::styled(m.to_string(), syn(app, mg))
            }
            None => if let Some(sev) = snap
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
        }}];
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
        let base_style = tint(base(app), delta_fg);
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
                tint(base_style.patch(syn(app, hl.group)), delta_fg),
            ));
            pos = seg_end;
        }
        if pos < le {
            spans.push(Span::styled(
                line_chars[pos - ls..].iter().collect::<String>(),
                base_style,
            ));
        }
        // #50: 現在側コメントのマーカー（`"`。解決行で照合）。
        if app.review_list.iter().any(|e| {
            e.side == ReviewSide::Current
                && Some(e.path.as_str()) == snap.path.as_deref()
                && e.resolved_line == line_no as u32
        }) {
            spans.push(Span::styled(" \"", syn(app, HighlightGroup::String)));
        }
        rendered.push(Line::from(spans));
            }
        }
    }
    f.render_widget(Paragraph::new(rendered).style(base(app)), area);
}

/// カーソル描画（プライマリカーソルを自色で — 端末カーソルを使う）。
fn draw_cursor(
    f: &mut Frame,
    app: &App,
    area: Rect,
    diff: Option<&git::FileDiff>,
    gap_cur: Option<(usize, usize, usize)>,
) {
    let snap = &app.snapshot;
    // gapレビュー中: gap 行に端末カーソルを置く（デーモン選択は動かさない）。
    if let Some((gi, li, col)) = gap_cur {
        let pos: Option<(usize, u16)> = (|| {
            let d = diff?;
            let g = d.gaps.get(gi)?;
            if g.at < snap.first_line {
                return None;
            }
            let text = g.lines.get(li)?;
            let mut rel = g.at - snap.first_line;
            for (oi, og) in d.gaps.iter().enumerate() {
                if og.at < snap.first_line {
                    continue;
                }
                if og.at < g.at || (og.at == g.at && oi < gi) {
                    rel += og.lines.len();
                }
            }
            rel += li;
            if rel >= area.height as usize {
                return None;
            }
            let gutter_w = gutter_width(snap, diff);
            let disp_col = UnicodeWidthStr::width(
                text.chars().take(col).collect::<String>().as_str(),
            ) as u16;
            let x = area
                .x
                .saturating_add(gutter_w + disp_col)
                .min(area.x + area.width.saturating_sub(1));
            Some((rel, x))
        })();
        if let Some((rel, x)) = pos {
            f.set_cursor_position((x, area.y + rel as u16));
        }
        return;
    }
    let (line, col) = cursor_line_col(snap);
    if line < snap.first_line {
        return;
    }
    // 削除 gap 行の挿入分だけ表示位置が下がる。
    let extra: usize = diff
        .map(|d| {
            d.gaps
                .iter()
                .filter(|g| g.at >= snap.first_line && g.at <= line)
                .map(|g| g.lines.len())
                .sum()
        })
        .unwrap_or(0);
    let rel = line - snap.first_line + extra;
    if rel >= area.height as usize {
        return;
    }
    let gutter_w = gutter_width(snap, diff);
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

/// ガター幅（マーカー 1 + 空白 2 + 行番号 + 空白 1）。削除行の旧番号も収める。
fn gutter_width(snap: &StateSnapshot, diff: Option<&git::FileDiff>) -> u16 {
    let total = snap.text.split('\n').count();
    (1 + 2 + digits(total.max(max_old_no(diff))).max(3) + 1) as u16
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
    let path = snap
        .path
        .as_deref()
        .map(|p| app.base_display_path(p))
        .unwrap_or_else(|| "[No Name]".to_string());
    let dirty = if snap.dirty { " [+]" } else { "" };
    let gap_mark = if app.gap_review.is_some() { " GAP" } else { "" };
    let cmp_mark = app
        .compare
        .as_ref()
        .filter(|c| c.is_showing())
        .map(|c| {
            // #50: レビューコメント件数（有るときのみ）。
            let n = app
                .review_list
                .iter()
                .filter(|e| Some(e.path.as_str()) == app.snapshot.path.as_deref())
                .count();
            if n > 0 {
                format!(" ◈{} \"{}", c.short(), n)
            } else {
                format!(" ◈{}", c.short())
            }
        })
        .unwrap_or_default();
    let left = format!("{mode_txt}  {path}{dirty}{cmp_mark}{gap_mark}");

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
    if app.gap_review.is_some() {
        right.push_str("j/k:移動 h/l:列 Enter:定義 K:コメント Esc:戻る");
    } else if app.compare.as_ref().is_some_and(|c| c.is_showing()) {
        right.push_str("T:ツリー G:診断 A:履歴 C:配色 D:比較 K:コメント");
    } else {
        right.push_str("T:ツリー G:診断 A:履歴 C:配色 D:比較");
    }

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
            // 比較マーカー: ファイルは M/A/D（+/~/−）、畳みディレクトリは配下件数。
            let (dmark, dstyle) = match e.status {
                Some(st) => (format!("{} ", st.marker()), status_style(app, st)),
                None => (String::new(), base(app)),
            };
            let aggr = if e.is_dir && e.subtree_changes > 0 && !app.tree.expanded.contains(&e.path)
            {
                format!(" +{}", e.subtree_changes)
            } else {
                String::new()
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
            let dstyle = if idx == app.tree.selected {
                dstyle.patch(Style::default().add_modifier(ratatui::style::Modifier::REVERSED))
            } else {
                dstyle
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{mark} {indent}{icon} "), style),
                Span::styled(dmark, dstyle),
                Span::styled(e.name.clone(), style),
                Span::styled(aggr, style),
            ]))
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
            format!("{}:{}", app.base_display_path(&peek.path), peek.line),
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
            review_comment_count: 0,
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
