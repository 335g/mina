//! 移動: カーソルと選択を文書内で動かす操作。
//!
//! モデル（Helix の設計に基づく）:
//! - 移動は常に head（移動端）を動かす。anchor（固定端）は動かない。
//! - [`move_selection`] は選択を点（カーソル）に潰して移動する。
//! - [`extend_selection`] は anchor を保ったまま head を動かす（選択の拡張）。
//! - head が anchor を越えた場合は向きが反転し、anchor 側の文字は選択に残る。
//!
//! Helix との意図的な差異: Helix は前進選択のハンドルを「head の1つ手前」に
//! するため、カーソルからの最初の extend が2書記素を選択する癖がある。
//! minae では常に head を基準にし、カーソルでは anchor を反転させないことで
//! 常に1書記素ずつ拡張する。
//!
//! パフォーマンス注記: 移動ごとに文書全体を `String` にマテリアライズするため、
//! 巨大ファイルでは O(n) になる。
//! ponytail: チャンク対応の GraphemeCursor（Helix の graphemes.rs 相当）へ
//! 置き換えれば定数時間にできる。必要になるのはプロファイリング後でよい。

use crate::{Document, Range, Selection};
use unicode_segmentation::UnicodeSegmentation;

/// 移動方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// 次へ（右・下・次の単語）。
    Forward,
    /// 前へ（左・上・前の単語）。
    Backward,
}

/// 移動の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Movement {
    /// 1書記素（結合文字・絵文字を1単位として扱う）。
    Char,
    /// 1行。現在の列を保ち、行の長さでクランプする。
    Line,
    /// 単語の先頭（単語 = 英数字 + アンダースコアの連続。カテゴリ境界で判定）。
    Word,
    /// 単語の末尾（Helix の `e`）。空白・改行を越えて次の単語の末尾まで。
    /// Backward は前の単語先頭（`b`）と同じ。
    WordEnd,
    /// 行頭（列 0）。方向は無視する。
    LineStart,
    /// 行末（改行の直前）。方向は無視する。
    LineEnd,
    /// 行の最初の非空白文字（空白のみの行は列 0）。方向は無視する。
    FirstNonWhitespace,
}

/// 選択全体を移動する。各 Range は点（カーソル）に潰される。
pub fn move_selection(
    doc: &Document,
    selection: &Selection,
    movement: Movement,
    dir: Direction,
) -> Selection {
    transform(doc, selection, movement, dir, false)
}

/// 各 Range の head だけを移動する。anchor は動かない（選択の拡張・縮小）。
pub fn extend_selection(
    doc: &Document,
    selection: &Selection,
    movement: Movement,
    dir: Direction,
) -> Selection {
    transform(doc, selection, movement, dir, true)
}

fn transform(
    doc: &Document,
    selection: &Selection,
    movement: Movement,
    dir: Direction,
    extend: bool,
) -> Selection {
    let text = doc.text().to_string();
    let ranges = selection
        .ranges()
        .iter()
        .map(|r| move_range(&text, *r, movement, dir, extend))
        .collect();
    Selection::new(ranges, selection.primary_index())
}

/// 選択を `lines` 行分まとめて移動する（スクロール連動用。ADR-0023）。
///
/// [`Movement::Line`] の繰り返しと違い1回で済ませる（本モジュールのパフォーマンス
/// 注記どおり、繰り返すと毎回文書全体のマテリアライズが走る）。列は維持し、行の
/// 長さと文書端でクランプする。`extend` なら anchor を保って head だけ動かす
/// （Select モードのスクロール拡張用）、そうでなければ点に潰す。
pub fn move_selection_lines(
    doc: &Document,
    selection: &Selection,
    lines: isize,
    extend: bool,
) -> Selection {
    let text = doc.text().to_string();
    let ranges = selection
        .ranges()
        .iter()
        .map(|r| put_cursor(&text, *r, step_lines(&text, r.head(), lines), extend))
        .collect();
    Selection::new(ranges, selection.primary_index())
}

/// 各 Range を head の行の「最初の非空白文字」（空白のみの行は列 0）へ点に潰して
/// 移動する（Helix の `I` = insert_at_line_start と同じ位置。空行の自動インデント
/// は行わない — ADR-0023）。
pub fn move_selection_to_line_first_non_whitespace(
    doc: &Document,
    selection: &Selection,
) -> Selection {
    let text = doc.text().to_string();
    let ranges = selection
        .ranges()
        .iter()
        .map(|r| Range::point(step_line_first_non_whitespace(&text, r.head())))
        .collect();
    Selection::new(ranges, selection.primary_index())
}

/// 各 Range を head の行全体（末尾改行は含まない）へ広げる（行選択）。
/// anchor は行頭（列 0）、head は行末（改行の直前）に置く。空行は
/// カーソル（空範囲）になる。
pub fn select_line_selection(doc: &Document, selection: &Selection) -> Selection {
    let text = doc.text().to_string();
    let ranges = selection
        .ranges()
        .iter()
        .map(|r| {
            let head = r.head();
            let start = step_line_start(&text, head);
            let end = step_line_end(&text, head);
            Range::new(start, end)
        })
        .collect();
    Selection::new(ranges, selection.primary_index())
}

/// 各 Range を「選択の末尾（カーソルならその直後の1文字、行末でクランプ）」へ
/// 点に潰して返す（Helix の `a` = append の挿入位置）。選択がある場合は選択
/// の直後（`end`）、カーソルの場合は head の次の書記素境界（改行を跨がない）。
pub fn append_selection(doc: &Document, selection: &Selection) -> Selection {
    let text = doc.text().to_string();
    let ranges = selection
        .ranges()
        .iter()
        .map(|r| {
            let mut pos = r.end();
            if r.is_cursor() {
                let line_end = step_line_end(&text, pos);
                pos = next_grapheme_boundary(&text, pos).min(line_end);
            }
            Range::point(pos)
        })
        .collect();
    Selection::new(ranges, selection.primary_index())
}

/// 各 Range を「行全体（`X` 相当）+ その下の行」まで head を拡張する
/// （Helix の `x` = `extend_line_below`。連打で行が追加されていく）。
/// anchor は Range の先頭（top）側の行頭（列 0）、head は bottom 行の
/// 1つ下の行の末尾（末尾改行を含む。最終行は文書末尾）に置く。
pub fn extend_line_below(doc: &Document, selection: &Selection) -> Selection {
    let text = doc.text().to_string();
    let len = text.chars().count();
    let ranges = selection
        .ranges()
        .iter()
        .map(|r| {
            // 選択の末尾側の文字位置（カーソルは自身、選択範囲は end-1）
            let bottom = if r.is_cursor() {
                r.head()
            } else {
                r.end().saturating_sub(1)
            };
            let anchor = step_line_start(&text, r.start());
            // bottom 行の末尾 → 1つ下の行の末尾（末尾改行を含む）
            let mut end = step_line_end(&text, bottom);
            if end < len {
                end = step_line_end(&text, end + 1);
            }
            if end < len {
                end += 1;
            }
            Range::new(anchor, end)
        })
        .collect();
    Selection::new(ranges, selection.primary_index())
}

/// 各 Range の head を絶対位置 `char_idx` へ移動し、anchor は保つ
/// （選択の拡張 — Select モードの `gg`/`G` など。向きは [`put_cursor`] と同様に
/// 反転する）。
pub fn extend_to(doc: &Document, selection: &Selection, char_idx: usize) -> Selection {
    let text = doc.text().to_string();
    let ranges = selection
        .ranges()
        .iter()
        .map(|r| put_cursor(&text, *r, char_idx, true))
        .collect();
    Selection::new(ranges, selection.primary_index())
}

/// `pos` を含む単語（英数字 + アンダースコアの連続）の char 範囲を返す。/// `pos` が空白・改行・記号の上なら `None`。`*`（全一致選択）と rename の
/// `pos` を含む単語（英数字 + アンダースコアの連続）の char 範囲を返す。
/// `pos` が空白・改行・記号の上なら `None`。`*`（全一致選択）と rename の
/// 「カーソル位置のシンボル」取り出しに使う。
///
/// ponytail: 単語境界の定義は [`categorize`] の単語文字をそのまま使う
/// （LSP の symbol 範囲と必ずしも一致しないが、実用上十分）。
pub fn word_at(doc: &Document, pos: usize) -> Option<(usize, usize)> {
    let text = doc.text().to_string();
    let ch = text.chars().nth(pos)?;
    if categorize(ch) != CharCategory::Word {
        return None;
    }
    let start = start_of_word(&text, pos);
    let end = end_of_word(&text, pos);
    Some((start, end))
}

/// `pos` から始まる単語の末尾（排他）。[`word_at`] の内部ヘルパー。
fn end_of_word(text: &str, pos: usize) -> usize {
    (pos..text.chars().count())
        .take_while(|&i| {
            matches!(
                categorize(text.chars().nth(i).unwrap_or(' ')),
                CharCategory::Word
            )
        })
        .last()
        .map(|i| i + 1)
        .unwrap_or(pos + 1)
}

/// `pos` から遡って単語の先頭（含む）。[`word_at`] の内部ヘルパー。
fn start_of_word(text: &str, pos: usize) -> usize {
    (0..=pos)
        .rev()
        .take_while(|&i| {
            matches!(
                categorize(text.chars().nth(i).unwrap_or(' ')),
                CharCategory::Word
            )
        })
        .last()
        .unwrap_or(pos)
}

/// `pos` の行頭（列 0）の char インデックス（[`Movement::LineStart`] と同じ位置）。
pub fn line_start_of(text: &str, pos: usize) -> usize {
    step_line_start(text, pos)
}

/// `pos` の行末（改行の直前。最終行は文書末尾）の char インデックス
/// （[`Movement::LineEnd`] と同じ位置。行の末尾改行そのものは含まない）。
pub fn line_end_of(text: &str, pos: usize) -> usize {
    step_line_end(text, pos)
}

/// `r`（[`Command::Replace`]）の置換対象: カーソルならその位置の1文字
/// （改行を跨がない — 行末は置換対象なし）、選択ならその範囲。
/// 空範囲（カーソルが行末・文書端）はリストに含めない（`replace_ranges` に
/// start==end を渡すと挿入扱いになり、no-op にならないため）。
pub fn replace_targets(doc: &Document, selection: &Selection) -> Vec<(usize, usize)> {
    let text = doc.text().to_string();
    selection
        .ranges()
        .iter()
        .filter_map(|r| {
            if r.is_cursor() {
                let p = r.head();
                let line_end = step_line_end(&text, p);
                let end = next_grapheme_boundary(&text, p).min(line_end);
                (end > p).then_some((p, end))
            } else {
                Some((r.start(), r.end()))
            }
        })
        .collect()
}

fn move_range(text: &str, range: Range, movement: Movement, dir: Direction, extend: bool) -> Range {
    let new_pos = match movement {
        Movement::Char => step_grapheme(text, range.head(), dir),
        Movement::Line => step_line(text, range.head(), dir),
        Movement::Word => step_word(text, range.head(), dir),
        Movement::WordEnd => step_word_end(text, range.head(), dir),
        Movement::LineStart => step_line_start(text, range.head()),
        Movement::LineEnd => step_line_end(text, range.head()),
        Movement::FirstNonWhitespace => step_line_first_non_whitespace(text, range.head()),
    };
    put_cursor(text, range, new_pos, extend)
}

/// 新位置 `char_idx` に head を置く。`extend` なら anchor を保ち、そうでなければ
/// 点に潰す。head が anchor を越えた場合のみ、anchor 側の文字を選択に残すため
/// anchor を1書記素分ずらす（カーソルでは反転させない — Helix の2書記素選択の
/// 癖を避ける意図的な差異）。
fn put_cursor(text: &str, range: Range, char_idx: usize, extend: bool) -> Range {
    if !extend {
        return Range::point(char_idx);
    }
    let anchor = if range.is_cursor() {
        range.anchor()
    } else if range.head() >= range.anchor() && char_idx < range.anchor() {
        next_grapheme_boundary(text, range.anchor())
    } else if range.head() < range.anchor() && char_idx >= range.anchor() {
        prev_grapheme_boundary(text, range.anchor())
    } else {
        range.anchor()
    };
    Range::new(anchor, char_idx)
}

// --- 書記素（grapheme）境界 ---

fn step_grapheme(text: &str, char_pos: usize, dir: Direction) -> usize {
    match dir {
        Direction::Forward => next_grapheme_boundary(text, char_pos),
        Direction::Backward => prev_grapheme_boundary(text, char_pos),
    }
}

/// `char_pos` の次にある書記素境界（char インデックス）。
pub(crate) fn next_grapheme_boundary(text: &str, char_pos: usize) -> usize {
    let byte = char_to_byte(text, char_pos);
    let next = text[byte..]
        .grapheme_indices(true)
        .nth(1)
        .map(|(i, _)| byte + i)
        .unwrap_or(text.len());
    byte_to_char(text, next)
}

/// `char_pos` の前にある書記素境界（char インデックス）。`char_pos` が境界なら
/// 直前の境界を、書記素の途中ならその書記素の開始位置を返す。
pub(crate) fn prev_grapheme_boundary(text: &str, char_pos: usize) -> usize {
    let byte = char_to_byte(text, char_pos);
    let prev = text[..byte]
        .grapheme_indices(true)
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0);
    byte_to_char(text, prev)
}

fn char_to_byte(text: &str, char_pos: usize) -> usize {
    text.char_indices()
        .nth(char_pos)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

fn byte_to_char(text: &str, byte_pos: usize) -> usize {
    text[..byte_pos].chars().count()
}

// --- 行移動 ---

/// 現在の列（行頭からの char 数）を保って隣の行へ移動する。行が短ければ
/// 行末にクランプする。先頭行・最終行では動かない。
fn step_line(text: &str, char_pos: usize, dir: Direction) -> usize {
    let len = text.chars().count();
    if len == 0 {
        return 0;
    }
    let byte_pos = char_to_byte(text, char_pos);
    let line_start = text[..byte_pos]
        .rfind('\n')
        .map(|b| byte_to_char(text, b) + 1)
        .unwrap_or(0);
    // 注意: `find('\n')` の戻り値 `b` は `text[byte_pos..]` からの相対バイト。
    // `byte_to_char(text, b)`（文書先頭からの絶対変換）に渡すと、前の行に
    // 多バイト文字があるとき char インデックスがずれ、その後のスライスが
    // char 境界を跨いで panic する（バグ修正前: 日本語行を跨ぐ j 移動で
    // 「byte index is not a char boundary」）。相対バイト → 相対 char 数に変換する。
    let line_end = text[byte_pos..]
        .find('\n')
        .map(|b| char_pos + text[byte_pos..byte_pos + b].chars().count())
        .unwrap_or(len);
    let col = char_pos - line_start;
    match dir {
        Direction::Forward => {
            if line_end >= len {
                return char_pos; // 最終行
            }
            let next_start = line_end + 1;
            let next_byte = char_to_byte(text, next_start);
            let next_end = text[next_byte..]
                .find('\n')
                .map(|b| next_start + text[next_byte..next_byte + b].chars().count())
                .unwrap_or(len);
            next_start + col.min(next_end - next_start)
        }
        Direction::Backward => {
            if line_start == 0 {
                return char_pos; // 先頭行
            }
            let prev_start = text[..char_to_byte(text, line_start - 1)]
                .rfind('\n')
                .map(|b| byte_to_char(text, b) + 1)
                .unwrap_or(0);
            let prev_end = line_start - 1;
            prev_start + col.min(prev_end - prev_start)
        }
    }
}

// --- 単語移動 ---

#[derive(Clone, Copy, PartialEq)]
enum CharCategory {
    Word,
    Whitespace,
    Eol,
    Other,
}

fn categorize(ch: char) -> CharCategory {
    if ch == '\n' || ch == '\r' {
        CharCategory::Eol
    } else if ch.is_whitespace() {
        CharCategory::Whitespace
    } else if ch.is_alphanumeric() || ch == '_' {
        CharCategory::Word
    } else {
        CharCategory::Other
    }
}

/// 単語の先頭とみなさない文字（空白・改行）。
fn is_word_skip(ch: char) -> bool {
    matches!(categorize(ch), CharCategory::Whitespace | CharCategory::Eol)
}

/// 前の単語先頭（`b` 相当）。`step_word` の Backward と同じ意味を明示する。
pub(crate) fn prev_word_start(text: &str, char_pos: usize) -> usize {
    step_word(text, char_pos, Direction::Backward)
}

/// 次の単語末尾（`e` 相当）。`step_word_end` の Forward と同じ意味を明示する。
pub(crate) fn next_word_end(text: &str, char_pos: usize) -> usize {
    step_word_end(text, char_pos, Direction::Forward)
}

// --- 単語移動（Helix 流の選択保持） ---

/// 単語移動の目標。Helix の `WordMotionTarget` のうちデフォルトキーで使う4種。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordMoveTarget {
    /// `w`: 次の単語の先頭。
    NextWordStart,
    /// `e`: 次の単語の末尾。
    NextWordEnd,
    /// `b`: 前の単語の先頭。
    PrevWordStart,
    /// 前の単語の末尾（デフォルトキーでは未使用）。
    PrevWordEnd,
}

/// Helix の単語移動（`word_move` + `range_to_target` の移植）。
///
/// [`move_selection`] が選択を点に潰すのに対し、こちらは anchor を保持して
/// 「移動した分」を選択状態にする。Helix のカーソルは常に1文字の選択（block
/// cursor）で、w/b/e はその anchor を残すため、単語の途中から `b` を押すと
/// 現在の単語が選択される。これにより `bw` で単語全体+空白を選択して `d` で
/// 削除する、という Helix 流の操作が可能になる。
///
/// 単語カテゴリは [`categorize`] の4分類（Word/Whitespace/Eol/Other）。改行は
/// 線結合的に越える（直後の改行を越える移動では anchor が行頭側に置かれる）。
///
/// 素の `w`/`b`/`e` は Helix と同様（`w` は単語+後続の空白を選択）。ただし
/// `b` 直後の `w`（反転選択での `w`）だけは現在の単語の末尾までを選択し、
/// `bw` で単語のみ（空白なし）が選択される（ユーザー要求）。
pub fn word_move_selection(
    doc: &Document,
    selection: &Selection,
    target: WordMoveTarget,
) -> Selection {
    let text = doc.text().to_string();
    let ranges = selection
        .ranges()
        .iter()
        .map(|r| word_move_range(&text, *r, target))
        .collect();
    Selection::new(ranges, selection.primary_index())
}

/// 1つの Range に対する単語移動（Helix の `word_move`）。
fn word_move_range(text: &str, range: Range, target: WordMoveTarget) -> Range {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    // b 直後の w の調整: 反転選択（head < anchor — b の結果）で w を押すと、
    // 「次の単語先頭」ではなく「現在の単語の末尾」までを選択する。これにより
    // bw で単語のみ（空白なし）が選択される（ユーザー要求。素の w は従来どおり
    // 単語+空白を選択する — この調整は b 由来の反転選択にしか効かない）。
    let target = if target == WordMoveTarget::NextWordStart && range.anchor() > range.head() {
        WordMoveTarget::NextWordEnd
    } else {
        target
    };
    let is_prev = matches!(
        target,
        WordMoveTarget::PrevWordStart | WordMoveTarget::PrevWordEnd
    );
    let head = range.head();
    // 文書端では動かない（Helix と同じ early-out）
    if (is_prev && head == 0) || (!is_prev && head == n) {
        return range;
    }
    // block-cursor セマンティクス: 空の選択（カーソル）は anchor を head の次の
    // 文字に置き、既存の選択は向きに応じて anchor を決める。以降の walk で
    // anchor は原則動かない（境界上から出発した場合のみ head に畳む）。
    let (anchor, start_head) = if is_prev {
        if range.anchor() < head {
            (head, prev_grapheme_boundary(text, head))
        } else {
            (next_grapheme_boundary(text, head), head)
        }
    } else if range.anchor() < head {
        (prev_grapheme_boundary(text, head), head)
    } else {
        (head, next_grapheme_boundary(text, head))
    };
    range_to_target(&chars, anchor, start_head, target)
}

/// Helix の `range_to_target` の移植。head を目標位置まで歩かせ、開始位置が
/// ちょうど境界上なら anchor を head に畳む（それ以外は anchor を保持）。
fn range_to_target(
    chars: &[char],
    mut anchor: usize,
    mut head: usize,
    target: WordMoveTarget,
) -> Range {
    let is_prev = matches!(
        target,
        WordMoveTarget::PrevWordStart | WordMoveTarget::PrevWordEnd
    );
    // 先頭の peek（前進は head-1、後退は head の文字）
    let mut prev_ch = if is_prev {
        chars.get(head).copied()
    } else {
        head.checked_sub(1).and_then(|i| chars.get(i)).copied()
    };
    // 直後の改行を越える（Helix と同じ。anchor は越えた後の位置に畳む）
    loop {
        let next = if is_prev {
            head.checked_sub(1).map(|i| chars[i])
        } else {
            chars.get(head).copied()
        };
        match next {
            Some(ch) if is_line_ending(ch) => {
                prev_ch = Some(ch);
                if is_prev {
                    head = head.saturating_sub(1);
                } else {
                    head += 1;
                }
            }
            _ => break,
        }
    }
    if prev_ch.map(is_line_ending).unwrap_or(false) {
        anchor = head;
    }
    // 目標位置まで歩く
    let head_start = head;
    loop {
        let next_ch = if is_prev {
            head.checked_sub(1).map(|i| chars[i])
        } else {
            chars.get(head).copied()
        };
        let next_ch = match next_ch {
            Some(c) => c,
            None => break,
        };
        if prev_ch.is_none() || reached_target(target, prev_ch.unwrap(), next_ch) {
            if head == head_start {
                anchor = head;
            } else {
                break;
            }
        }
        prev_ch = Some(next_ch);
        if is_prev {
            head = head.saturating_sub(1);
        } else {
            head += 1;
        }
    }
    Range::new(anchor, head)
}

fn is_line_ending(c: char) -> bool {
    c == '\n' || c == '\r'
}

fn is_word_boundary(a: char, b: char) -> bool {
    categorize(a) != categorize(b)
}

fn reached_target(target: WordMoveTarget, prev_ch: char, next_ch: char) -> bool {
    match target {
        WordMoveTarget::NextWordStart | WordMoveTarget::PrevWordEnd => {
            is_word_boundary(prev_ch, next_ch)
                && (is_line_ending(next_ch) || !next_ch.is_whitespace())
        }
        WordMoveTarget::NextWordEnd | WordMoveTarget::PrevWordStart => {
            is_word_boundary(prev_ch, next_ch)
                && (!prev_ch.is_whitespace() || is_line_ending(next_ch))
        }
    }
}

/// 次/前の「単語の先頭」（カテゴリ境界の直後にある非空白文字）へ移動する。
/// 文書端ではクランプする。
fn step_word(text: &str, char_pos: usize, dir: Direction) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    match dir {
        Direction::Forward => {
            let mut i = char_pos;
            while i < n {
                if i > char_pos
                    && categorize(chars[i]) != categorize(chars[i - 1])
                    && !is_word_skip(chars[i])
                {
                    return i;
                }
                i += 1;
            }
            n
        }
        Direction::Backward => {
            let mut i = char_pos;
            while i > 0 {
                let cur = chars[i - 1];
                let boundary = if i == 1 {
                    true
                } else {
                    categorize(chars[i - 2]) != categorize(cur)
                };
                if boundary && !is_word_skip(cur) {
                    return i - 1;
                }
                i -= 1;
            }
            0
        }
    }
}

/// 次/前の「単語の末尾」へ移動する。
///
/// Forward: 空白・改行を越えて次の単語に着いたら、その単語の末尾（カテゴリが
/// 変わる直前、または文書端）まで進む。単語の途中なら現在の単語の末尾へ。
/// Backward: `step_word` の Backward（前の単語先頭）と同じ。
fn step_word_end(text: &str, char_pos: usize, dir: Direction) -> usize {
    if dir == Direction::Backward {
        return step_word(text, char_pos, dir);
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = char_pos;
    // 空白・改行を越えて次の単語の先頭へ
    while i < n && is_word_skip(chars[i]) {
        i += 1;
    }
    if i >= n {
        return n;
    }
    // 現在の単語（同じカテゴリの連続）の末尾へ
    let cat = categorize(chars[i]);
    i += 1;
    while i < n && categorize(chars[i]) == cat {
        i += 1;
    }
    i
}

/// 現在の行頭（列 0）へ。
fn step_line_start(text: &str, char_pos: usize) -> usize {
    let byte = char_to_byte(text, char_pos);
    text[..byte]
        .rfind('\n')
        .map(|b| byte_to_char(text, b) + 1)
        .unwrap_or(0)
}

/// 現在の行末（改行の直前。最終行は文書末尾）へ。
fn step_line_end(text: &str, char_pos: usize) -> usize {
    let len = text.chars().count();
    let byte = char_to_byte(text, char_pos);
    text[byte..]
        .find('\n')
        .map(|b| char_pos + text[byte..byte + b].chars().count())
        .unwrap_or(len)
}

/// `char_pos` から `lines` 行分（正で下、負で上）移動した位置。現在の列を維持し、
/// 行の長さと文書端でクランプする。端に達していれば動かない。
fn step_lines(text: &str, char_pos: usize, lines: isize) -> usize {
    let len = text.chars().count();
    if len == 0 || lines == 0 {
        return char_pos;
    }
    // 行開始位置（char インデックス）の一覧。行 k は [starts[k], starts[k+1]) で、
    // 末尾行（改行なし）の終端は文書末尾。
    let mut starts = vec![0usize];
    let mut chars = 0usize;
    for ch in text.chars() {
        chars += 1;
        if ch == '\n' {
            starts.push(chars);
        }
    }
    let cur_line = starts.partition_point(|&s| s <= char_pos) - 1;
    let last_line = starts.len() - 1;
    let target = (cur_line as isize + lines).clamp(0, last_line as isize) as usize;
    if target == cur_line {
        return char_pos;
    }
    let line_end = starts
        .get(target + 1)
        .map(|&s| s - 1) // 改行の直前
        .unwrap_or(len);
    let col = char_pos - starts[cur_line];
    starts[target] + col.min(line_end - starts[target])
}

/// 現在の行の最初の非空白文字の位置。空白のみの行は行頭（列 0）へ（Helix の
/// insert_at_line_start と同じ位置。`\r\n` 行末の `\r` も空白として飛ばす）。
fn step_line_first_non_whitespace(text: &str, char_pos: usize) -> usize {
    let start = step_line_start(text, char_pos);
    let end = step_line_end(text, start);
    let start_byte = char_to_byte(text, start);
    let end_byte = char_to_byte(text, end);
    match text[start_byte..end_byte].find(|c: char| !c.is_whitespace()) {
        Some(rel) => start + text[start_byte..start_byte + rel].chars().count(),
        None => start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Selection;

    fn sel(ranges: Vec<(usize, usize)>, primary: usize) -> Selection {
        Selection::new(
            ranges.into_iter().map(|(a, h)| Range::new(a, h)).collect(),
            primary,
        )
    }

    #[test]
    fn move_char_right_and_left() {
        let doc = Document::from("hello");
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(0),
                Movement::Char,
                Direction::Forward
            ),
            Selection::point(1)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(4),
                Movement::Char,
                Direction::Forward
            ),
            Selection::point(5)
        );
        // 文書端では動かない
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(5),
                Movement::Char,
                Direction::Forward
            ),
            Selection::point(5)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(1),
                Movement::Char,
                Direction::Backward
            ),
            Selection::point(0)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(0),
                Movement::Char,
                Direction::Backward
            ),
            Selection::point(0)
        );
    }

    #[test]
    fn move_char_collapses_selection_to_point() {
        let doc = Document::from("hello world");
        let selection = sel(vec![(1, 4)], 0); // "ell"
        // 右へ: head の次（選択の直後）に潰れる
        assert_eq!(
            move_selection(&doc, &selection, Movement::Char, Direction::Forward),
            Selection::point(5)
        );
        // 左へ: head の直前の境界に潰れる
        assert_eq!(
            move_selection(&doc, &selection, Movement::Char, Direction::Backward),
            Selection::point(3)
        );
    }

    #[test]
    fn move_char_moves_by_grapheme() {
        // "が"（か + 結合濁点）は1書記素。cursor 1 から右へは 2 を飛ばして 3 に着く。
        let doc = Document::from("aがb");
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(1),
                Movement::Char,
                Direction::Forward
            ),
            Selection::point(3)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(3),
                Movement::Char,
                Direction::Backward
            ),
            Selection::point(1)
        );
    }

    #[test]
    fn extend_char_selects_exactly_one_grapheme() {
        let doc = Document::from("hello");
        assert_eq!(
            extend_selection(
                &doc,
                &Selection::point(3),
                Movement::Char,
                Direction::Forward
            ),
            sel(vec![(3, 4)], 0)
        );
        assert_eq!(
            extend_selection(
                &doc,
                &Selection::point(3),
                Movement::Char,
                Direction::Backward
            ),
            sel(vec![(3, 2)], 0) // [2, 3) の1文字
        );
    }

    #[test]
    fn extend_char_grows_then_shrinks() {
        let doc = Document::from("hello");
        let mut selection = Selection::point(0);
        selection = extend_selection(&doc, &selection, Movement::Char, Direction::Forward);
        assert_eq!(selection, sel(vec![(0, 1)], 0));
        selection = extend_selection(&doc, &selection, Movement::Char, Direction::Forward);
        assert_eq!(selection, sel(vec![(0, 2)], 0));
        // head 側から縮む
        selection = extend_selection(&doc, &selection, Movement::Char, Direction::Backward);
        assert_eq!(selection, sel(vec![(0, 1)], 0));
    }

    #[test]
    fn extend_flips_anchor_when_crossing() {
        // 逆方向の選択 (6,3) を右へ拡張し続けると anchor を越え、向きが反転する。
        let doc = Document::from("abcdefg");
        let mut selection = sel(vec![(6, 3)], 0); // [3, 6) = "def"
        selection = extend_selection(&doc, &selection, Movement::Char, Direction::Forward);
        assert_eq!(selection, sel(vec![(6, 4)], 0)); // "ef"
        selection = extend_selection(&doc, &selection, Movement::Char, Direction::Forward);
        assert_eq!(selection, sel(vec![(6, 5)], 0)); // "f"
        // anchor(6) に達した時点で anchor は 5 に移り、以後は前向きに伸びる
        selection = extend_selection(&doc, &selection, Movement::Char, Direction::Forward);
        assert_eq!(selection, sel(vec![(5, 6)], 0));
        selection = extend_selection(&doc, &selection, Movement::Char, Direction::Forward);
        assert_eq!(selection, sel(vec![(5, 7)], 0)); // "fg"
    }

    #[test]
    fn move_line_keeps_column_and_clamps() {
        let doc = Document::from("abc\ndef\nghi");
        // 1列目で下へ → 2行目の1列目
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(1),
                Movement::Line,
                Direction::Forward
            ),
            Selection::point(5)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(5),
                Movement::Line,
                Direction::Backward
            ),
            Selection::point(1)
        );
        // 短い行へは列がクランプされる
        let short = Document::from("abc\nde\nfgh");
        assert_eq!(
            move_selection(
                &short,
                &Selection::point(2),
                Movement::Line,
                Direction::Forward
            ),
            Selection::point(6) // 2行目 "de" の行末（改行位置）へ
        );
        // 先頭行・最終行では動かない
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(0),
                Movement::Line,
                Direction::Backward
            ),
            Selection::point(0)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(8),
                Movement::Line,
                Direction::Forward
            ),
            Selection::point(8)
        );
    }

    #[test]
    fn move_selection_lines_moves_by_delta_keeping_column() {
        // "abc\ndef\nghi\njkl\nmno": 行0[0,3] 1[4,6] 2[8,10] 3[12,14] 4[16,18]
        let doc = Document::from("abc\ndef\nghi\njkl\nmno");
        // 行1（"def" の列1 = 'e'）から3行下 → 行4（"mno"）の列1
        assert_eq!(
            move_selection_lines(&doc, &Selection::point(5), 3, false),
            Selection::point(17)
        );
        // 戻る
        assert_eq!(
            move_selection_lines(&doc, &Selection::point(17), -3, false),
            Selection::point(5)
        );
        // 文書端ではクランプして動かない
        assert_eq!(
            move_selection_lines(&doc, &Selection::point(17), 3, false),
            Selection::point(17)
        );
        // 短い行へは列がクランプされる（"de" の列2 → "g" の列1）
        let short = Document::from("abc\nde\ng");
        assert_eq!(
            move_selection_lines(&short, &Selection::point(6), 1, false),
            Selection::point(8)
        );
        // 末尾に改行がある文書の最終行（空行）へ
        let trailing = Document::from("a\nb\n");
        assert_eq!(
            move_selection_lines(&trailing, &Selection::point(0), 2, false),
            Selection::point(4)
        );
        // 空文書では動かない
        let empty = Document::from("");
        assert_eq!(
            move_selection_lines(&empty, &Selection::point(0), 5, false),
            Selection::point(0)
        );
    }

    #[test]
    fn move_selection_lines_extend_keeps_anchor() {
        // Select モードのスクロール拡張: anchor を保って head だけ動かす
        let doc = Document::from("abc\ndef\nghi");
        let selection = sel(vec![(2, 5)], 0); // "cde"
        assert_eq!(
            move_selection_lines(&doc, &selection, 1, true),
            sel(vec![(2, 9)], 0) // head は行2の列1へ、anchor 2 のまま
        );
        // extend で head が anchor を越えたら、anchor 側の文字を選択に残すため
        // anchor が1書記素ずれる（put_cursor の規則）
        let up = sel(vec![(2, 5)], 0); // [2,5) = "cde"
        assert_eq!(
            move_selection_lines(&doc, &up, -2, true),
            sel(vec![(3, 1)], 0) // head は行0の列1へ、anchor は 2→3（'c' を残す）
        );
    }

    #[test]
    fn line_first_non_whitespace_moves_and_falls_back() {
        let doc = Document::from("  ab\ncd");
        // 空白の上から → 行の最初の非空白へ
        assert_eq!(
            move_selection_to_line_first_non_whitespace(&doc, &Selection::point(1)),
            Selection::point(2)
        );
        // 既に非空白の上なら動かない
        assert_eq!(
            move_selection_to_line_first_non_whitespace(&doc, &Selection::point(2)),
            Selection::point(2)
        );
        // 空白のみの行は列 0 へ
        let ws = Document::from("   \ncd");
        assert_eq!(
            move_selection_to_line_first_non_whitespace(&ws, &Selection::point(1)),
            Selection::point(0)
        );
        // マルチカーソル: 各 Range は head の行の位置へ
        assert_eq!(
            move_selection_to_line_first_non_whitespace(&doc, &sel(vec![(1, 1), (5, 5)], 0)),
            sel(vec![(2, 2), (5, 5)], 0)
        );
    }

    #[test]
    fn move_line_over_multibyte_lines_does_not_panic() {
        // バグ修正: 全角文字を含む行を跨ぐ Line 移動で、相対バイトオフセットを
        // 絶対変換してしまい「byte index is not a char boundary」で panic していた。
        // 日本語行の上から 3 回下へ移動しても panic せず正しい位置へ着く。
        let doc = Document::from("あ\nい\nう\nえ");
        // 行0 の 'あ' から: 下 → 行1 の 'い'（char 2）→ 行2 の 'う'（char 4）→ 行3 の 'え'（char 6）
        let mut sel = Selection::point(0);
        for expected in [2, 4, 6] {
            sel = move_selection(&doc, &sel, Movement::Line, Direction::Forward);
            assert_eq!(sel.primary().head(), expected, "移動後位置: {sel:?}");
        }
        // 上の行にも戻れる
        let mut sel = Selection::point(6);
        for expected in [4, 2, 0] {
            sel = move_selection(&doc, &sel, Movement::Line, Direction::Backward);
            assert_eq!(sel.primary().head(), expected, "移動後位置: {sel:?}");
        }
        // 日本語 + ASCII 混在（報告ケースに近い形）
        let doc =
            Document::from("//! 検索: 文書内のパターン一致\n//! 主役は [`find_matches`]\nabc");
        let mut sel = Selection::point(0);
        sel = move_selection(&doc, &sel, Movement::Line, Direction::Forward);
        sel = move_selection(&doc, &sel, Movement::Line, Direction::Forward);
        sel = move_selection(&doc, &sel, Movement::Line, Direction::Forward);
        // 最終行（abc）の先頭に着く（panic しないこと・位置は行先頭）
        assert_eq!(sel.primary().head(), 0 + doc.text().chars().count() - 3);
    }

    #[test]
    fn move_word_basic() {
        let doc = Document::from("hello world foo");
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(0),
                Movement::Word,
                Direction::Forward
            ),
            Selection::point(6)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(6),
                Movement::Word,
                Direction::Forward
            ),
            Selection::point(12)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(12),
                Movement::Word,
                Direction::Backward
            ),
            Selection::point(6)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(6),
                Movement::Word,
                Direction::Backward
            ),
            Selection::point(0)
        );
    }

    #[test]
    fn move_word_unicode() {
        // 日本語は is_alphanumeric なので1語として扱われる
        let doc = Document::from("こんにちは世界");
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(0),
                Movement::Word,
                Direction::Forward
            ),
            Selection::point(7)
        );
        let mixed = Document::from("hello 世界");
        assert_eq!(
            move_selection(
                &mixed,
                &Selection::point(0),
                Movement::Word,
                Direction::Forward
            ),
            Selection::point(6)
        );
    }

    #[test]
    fn move_word_at_boundaries() {
        let single = Document::from("hello");
        assert_eq!(
            move_selection(
                &single,
                &Selection::point(0),
                Movement::Word,
                Direction::Forward
            ),
            Selection::point(5)
        );
        let empty = Document::new();
        assert_eq!(
            move_selection(
                &empty,
                &Selection::point(0),
                Movement::Word,
                Direction::Forward
            ),
            Selection::point(0)
        );
    }

    #[test]
    fn extend_word_keeps_anchor() {
        let doc = Document::from("hello world");
        // 先頭から次の単語先頭（前の空白を含む）まで拡張される
        assert_eq!(
            extend_selection(
                &doc,
                &Selection::point(0),
                Movement::Word,
                Direction::Forward
            ),
            sel(vec![(0, 6)], 0)
        );
    }

    #[test]
    fn move_line_empty_document() {
        let doc = Document::new();
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(0),
                Movement::Line,
                Direction::Forward
            ),
            Selection::point(0)
        );
    }

    #[test]
    fn move_word_end_forward() {
        let doc = Document::from("hello world foo");
        // 単語途中から現在の単語の末尾へ
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(2),
                Movement::WordEnd,
                Direction::Forward
            ),
            Selection::point(5)
        );
        // 単語先頭からも同じ末尾へ
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(0),
                Movement::WordEnd,
                Direction::Forward
            ),
            Selection::point(5)
        );
        // 空白からは次の単語の末尾へ
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(5),
                Movement::WordEnd,
                Direction::Forward
            ),
            Selection::point(11)
        );
        // 文書端では動かない
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(15),
                Movement::WordEnd,
                Direction::Forward
            ),
            Selection::point(15)
        );
    }

    #[test]
    fn move_word_end_punctuation_run() {
        // 記号の連続（foo-bar）は独立した「語」として末尾を持つ
        let doc = Document::from("foo-bar");
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(3),
                Movement::WordEnd,
                Direction::Forward
            ),
            Selection::point(4)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(0),
                Movement::WordEnd,
                Direction::Forward
            ),
            Selection::point(3)
        );
    }

    #[test]
    fn move_word_end_backward_falls_back_to_word_start() {
        let doc = Document::from("hello world");
        // Backward は前の単語先頭（b と同じ）
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(11),
                Movement::WordEnd,
                Direction::Backward
            ),
            Selection::point(6)
        );
    }

    #[test]
    fn extend_word_end_keeps_anchor() {
        let doc = Document::from("hello world");
        // カーソルから単語末尾まで拡張 = 現在の単語を選択
        assert_eq!(
            extend_selection(
                &doc,
                &Selection::point(2),
                Movement::WordEnd,
                Direction::Forward
            ),
            sel(vec![(2, 5)], 0)
        );
    }

    #[test]
    fn move_line_start_and_end() {
        let doc = Document::from("abc\ndef\nghi");
        // 行頭: 3 行目の途中からその行頭へ
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(9),
                Movement::LineStart,
                Direction::Forward
            ),
            Selection::point(8)
        );
        // 行末: 1 行目の途中からその行末（改行の直前）へ
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(1),
                Movement::LineEnd,
                Direction::Forward
            ),
            Selection::point(3)
        );
        // 最終行の行末は文書末尾
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(9),
                Movement::LineEnd,
                Direction::Forward
            ),
            Selection::point(11)
        );
    }

    #[test]
    fn move_line_start_end_multibyte() {
        // 行頭・行末もバイト/char 変換を正しく行う（panic しないこと）
        let doc = Document::from("あい\nうえ");
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(1),
                Movement::LineEnd,
                Direction::Forward
            ),
            Selection::point(2)
        );
        assert_eq!(
            move_selection(
                &doc,
                &Selection::point(3),
                Movement::LineStart,
                Direction::Forward
            ),
            Selection::point(3)
        );
    }

    // --- Helix 流の単語移動（選択保持） ---

    fn word_sel(text: &str, pos: usize, target: WordMoveTarget) -> Selection {
        let doc = Document::from(text);
        word_move_selection(&doc, &Selection::point(pos), target)
    }

    #[test]
    fn word_b_from_mid_word_selects_current_word() {
        // 単語の途中から b → [単語先頭, カーソル+1)。カーソルの文字も含む
        // （Helix の block-cursor セマンティクス。Helix のテストからの移植）
        assert_eq!(
            word_sel(
                "Basic backward motion from the middle of a word",
                3,
                WordMoveTarget::PrevWordStart
            ),
            sel(vec![(4, 0)], 0),
            "Basic の途中 → Basi が選択"
        );
        // 単語の直後（空白）から b → 単語全体（Helix のテストと同じ入力 {6,7}）
        let doc = Document::from("Jumping to start of word from the end selects the word");
        assert_eq!(
            word_move_selection(&doc, &sel(vec![(6, 7)], 0), WordMoveTarget::PrevWordStart),
            sel(vec![(7, 0)], 0),
            "Jumping の直後 → Jumping が選択"
        );
        // 文末のカーソルから b → 現在の単語全体
        assert_eq!(
            word_sel("hello", 5, WordMoveTarget::PrevWordStart),
            sel(vec![(5, 0)], 0),
            "hello の文末 → hello が選択"
        );
    }

    #[test]
    fn word_b_from_word_start_selects_preceding_whitespace() {
        // 単語先頭のカーソルから b → 直前の空白ランが選択される（Helix と同じ）
        assert_eq!(
            word_sel(
                "    Jump to start of line from start of word preceded by whitespace",
                4,
                WordMoveTarget::PrevWordStart
            ),
            sel(vec![(4, 0)], 0),
            "J の先頭 → 前の空白が選択"
        );
        // 単語途中で空白の直後 → 単語先頭まで
        assert_eq!(
            word_sel(
                "    Jump to start of a word preceded by whitespace",
                5,
                WordMoveTarget::PrevWordStart
            ),
            sel(vec![(6, 4)], 0),
            "u の途中 → Ju が選択"
        );
    }

    #[test]
    fn word_b_crosses_newline_selecting_whitespace_run() {
        // 改行をまたぐ b は、直前の空白ランを選択する（Helix のテストからの移植）
        assert_eq!(
            word_sel("Jumping\n    \nback", 13, WordMoveTarget::PrevWordStart),
            sel(vec![(12, 8)], 0),
            "back の先頭 → 前の空白 4 文字が選択"
        );
        // 行頭の改行直後から b → 前の行の単語を選択（anchor は行頭側）
        assert_eq!(
            word_sel("foo\nbar", 4, WordMoveTarget::PrevWordStart),
            sel(vec![(3, 0)], 0),
            "bar の先頭 → foo が選択"
        );
    }

    #[test]
    fn word_w_from_mid_word_selects_rest_of_word() {
        assert_eq!(
            word_sel(
                "Starting from mid-word leaves anchor at start position and moves head",
                3,
                WordMoveTarget::NextWordStart
            ),
            sel(vec![(3, 9)], 0),
            "Starting の途中 → rting が選択（Helix のテストからの移植）"
        );
        // 単語先頭から w → 単語+空白
        assert_eq!(
            word_sel("hello world foo", 0, WordMoveTarget::NextWordStart),
            sel(vec![(0, 6)], 0),
            "hello が選択"
        );
        // 続けて w → 次の単語（anchor が畳まれて1語ずつ進む）
        let doc = Document::from("hello world foo");
        let mut s2 = word_move_selection(&doc, &Selection::point(0), WordMoveTarget::NextWordStart);
        s2 = word_move_selection(&doc, &s2, WordMoveTarget::NextWordStart);
        assert_eq!(s2, sel(vec![(6, 12)], 0), "world が選択");
    }

    #[test]
    fn word_bw_selects_whole_word_then_delete() {
        // ユーザーが求めるフロー: 単語途中で b → 現在の単語、続けて w →
        // 単語全体（空白なし）が選択され、d で削除できる
        let doc = Document::from("hello world foo");
        let mut s2 = word_move_selection(&doc, &Selection::point(3), WordMoveTarget::PrevWordStart);
        assert_eq!(s2, sel(vec![(4, 0)], 0), "b: hell が選択");
        s2 = word_move_selection(&doc, &s2, WordMoveTarget::NextWordStart);
        assert_eq!(s2, sel(vec![(0, 5)], 0), "bw: hello のみ選択（空白なし）");
        // 削除すると単語だけが消える（空白は残る）
        let tx = crate::Transaction::delete(&doc, &s2);
        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), " world foo");
    }

    #[test]
    fn word_e_selects_word_without_trailing_space() {
        // e は末尾まで（空白を含まない）
        assert_eq!(
            word_sel("hello world", 0, WordMoveTarget::NextWordEnd),
            sel(vec![(0, 5)], 0),
            "hello が選択（空白なし）"
        );
        assert_eq!(
            word_sel("hello world", 2, WordMoveTarget::NextWordEnd),
            sel(vec![(2, 5)], 0),
            "llo が選択"
        );
        // 空白からは次の単語の末尾まで（空白を含む）
        assert_eq!(
            word_sel("hello world", 5, WordMoveTarget::NextWordEnd),
            sel(vec![(5, 11)], 0),
            " world が選択"
        );
    }

    #[test]
    fn word_move_at_document_edges_is_noop() {
        // 文書端では動かない（Helix と同じ early-out）
        assert_eq!(
            word_sel("hello", 0, WordMoveTarget::PrevWordStart),
            sel(vec![(0, 0)], 0)
        );
        assert_eq!(
            word_sel("hello", 5, WordMoveTarget::NextWordStart),
            sel(vec![(5, 5)], 0)
        );
    }

    #[test]
    fn select_line_selects_line_without_newline() {
        let doc = Document::from("ab\ncd\nefg");
        // 2行目（'c'）から: 行内容のみ = "cd"（head は行末）
        assert_eq!(
            select_line_selection(&doc, &Selection::point(3)),
            sel(vec![(3, 5)], 0)
        );
        // 選択中でも head の行を選び直す
        assert_eq!(
            select_line_selection(&doc, &sel(vec![(0, 4)], 0)),
            sel(vec![(3, 5)], 0)
        );
        // 空行はカーソル（空範囲）になる
        let blank = Document::from("a\n\nb");
        assert_eq!(
            select_line_selection(&blank, &Selection::point(2)),
            sel(vec![(2, 2)], 0)
        );
        // 末尾改行なしの最終行は行末（文書末尾）まで
        assert_eq!(
            select_line_selection(&doc, &Selection::point(7)),
            sel(vec![(6, 9)], 0)
        );
        // 末尾に改行がある最終行も改行は含めない（"efg"）
        let trailing = Document::from("ab\ncd\nefg\n");
        assert_eq!(
            select_line_selection(&trailing, &Selection::point(7)),
            sel(vec![(6, 9)], 0)
        );
    }

    #[test]
    fn append_selection_moves_past_cursor_and_selection_end() {
        // カーソル: 直後の1文字後ろ（行末でクランプ）
        let doc = Document::from("abcd\nef");
        assert_eq!(
            append_selection(&doc, &Selection::point(1)),
            sel(vec![(2, 2)], 0),
            "カーソルは1文字後ろ"
        );
        // 行末ではクランプ（改行を跨がない）
        assert_eq!(
            append_selection(&doc, &Selection::point(4)),
            sel(vec![(4, 4)], 0),
            "行末では動かない"
        );
        // 選択: 選択の直後（`end`）
        assert_eq!(
            append_selection(&doc, &sel(vec![(0, 3)], 0)),
            sel(vec![(3, 3)], 0),
            "選択の末尾直後"
        );
        // 空文書は0のまま
        assert_eq!(
            append_selection(&Document::from(""), &Selection::point(0)),
            sel(vec![(0, 0)], 0)
        );
    }

    #[test]
    fn extend_to_keeps_anchor() {
        let doc = Document::from("hello world");
        // 選択 (2..5) の head を文書末尾へ拡張
        let extended = extend_to(&doc, &sel(vec![(2, 5)], 0), 11);
        assert_eq!(extended, sel(vec![(2, 11)], 0));
        // cursor では拡張される（anchor 固定ではないが点が残る）
        let cur = extend_to(&doc, &Selection::point(3), 8);
        assert_eq!(cur, sel(vec![(3, 8)], 0));
    }

    #[test]
    fn extend_line_below_adds_line_on_repeat() {
        // カーソル → 現在の行 + 下の行（末尾改行含む）
        let doc = Document::from("l0\nl1\nl2\nl3"); // 11 chars
        // 'l1' の先頭（char 3）
        let once = extend_line_below(&doc, &Selection::point(3));
        assert_eq!(once, sel(vec![(3, 9)], 0), "現在行+1つ下 = \"l1\\nl2\\n\"");
        // 連打で1行ずつ追加
        let twice = extend_line_below(&doc, &once);
        assert_eq!(twice, sel(vec![(3, 11)], 0), "さらに l3 が加わる");
        // 最終行ではそれ以上伸びない（l3 の中盤 char 9）
        let last = extend_line_below(&doc, &Selection::point(9));
        assert_eq!(last, sel(vec![(9, 11)], 0));
    }

    #[test]
    fn word_at_covers_identifiers_only() {
        let doc = Document::from("foo_bar baz\nqux = 12");
        // foo_bar(0-6) 空白(7) baz(8-10) \n(11) qux(12-14) 空白(15) =(16) 空白(17) 12(18-19)
        assert_eq!(word_at(&doc, 0), Some((0, 7)), "foo_bar");
        assert_eq!(word_at(&doc, 3), Some((0, 7)));
        assert_eq!(word_at(&doc, 10), Some((8, 11)), "baz");
        assert_eq!(word_at(&doc, 14), Some((12, 15)), "qux");
        assert_eq!(word_at(&doc, 18), Some((18, 20)), "数字も単語");
        // 空白・記号の上は None
        assert_eq!(word_at(&doc, 7), None, "空白");
        assert_eq!(word_at(&doc, 15), None, "qux の後の空白");
        assert_eq!(word_at(&doc, 16), None, "= の上");
        assert_eq!(word_at(&doc, 24), None, "範囲外");
        // 範囲外は None
        assert_eq!(word_at(&doc, 100), None);
        let empty = Document::from("");
        assert_eq!(word_at(&empty, 0), None);
    }
}
