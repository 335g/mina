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
//! mina では常に head を基準にし、カーソルでは anchor を反転させないことで
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

fn move_range(text: &str, range: Range, movement: Movement, dir: Direction, extend: bool) -> Range {
    let new_pos = match movement {
        Movement::Char => step_grapheme(text, range.head(), dir),
        Movement::Line => step_line(text, range.head(), dir),
        Movement::Word => step_word(text, range.head(), dir),
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
    fn move_line_over_multibyte_lines_does_not_panic() {
        // バグ修正: 全角文字を含む行を跨ぐ Line 移動で、相対バイトオフセットを
        // 絶対変換してしまい「byte index is not a char boundary」で panic していた。
        // 日本語行の上から 3 回下へ移動しても panic せず正しい位置へ着く。
        let doc = Document::from("あ\nい\nう\nえ");
        // 行0 の 'あ' から: 下 → 行1 の 'い'（char 2）→ 行2 の 'う'（char 4）→ 行3 の 'え'（char 6）
        let mut sel = Selection::point(0);
        for expected in [2, 4, 6] {
            sel = move_selection(
                &doc,
                &sel,
                Movement::Line,
                Direction::Forward,
            );
            assert_eq!(sel.primary().head(), expected, "移動後位置: {sel:?}");
        }
        // 上の行にも戻れる
        let mut sel = Selection::point(6);
        for expected in [4, 2, 0] {
            sel = move_selection(
                &doc,
                &sel,
                Movement::Line,
                Direction::Backward,
            );
            assert_eq!(sel.primary().head(), expected, "移動後位置: {sel:?}");
        }
        // 日本語 + ASCII 混在（報告ケースに近い形）
        let doc = Document::from("//! 検索: 文書内のパターン一致\n//! 主役は [`find_matches`]\nabc");
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
}
