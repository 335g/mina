//! 囲み文字の操作（Helix の surround: `ms` / `mr` / `md`）。
//!
//! Helix の `helix-core/src/surround.rs` + `match_brackets::get_pair` の平文版。
//! 構文木（tree-sitter）によるペア検出はしない — mina の daemon は surround 用の
//! 構文を持たないため、テキストの走査だけで対応するペアを探す。
//!
//! ponytail: コマンド1回につき文書全体を `Vec<char>` に写して O(n) 走査する。
//! 対話操作の規模では問題にならない。count（外側のペアを数える）・複数文字ペア・
//! `mr m` / `md m`（最寄りペアの自動検出）は未対応 — 必要になったら Helix の
//! `find_nth_*` を持ち込む。

use crate::movement::prev_grapheme_boundary;
use crate::{Document, Range, Selection, Transaction};

/// 開き/閉じの対応表（Helix の `BRACKETS` と引用符ペア）。
const PAIRS: [(char, char); 13] = [
    ('(', ')'),
    ('{', '}'),
    ('[', ']'),
    ('<', '>'),
    ('‘', '’'),
    ('“', '”'),
    ('«', '»'),
    ('「', '」'),
    ('（', '）'),
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
    ('|', '|'),
];

/// `ch` に対応する `(開き, 閉じ)`。表に無い文字は `(ch, ch)`（Helix と同じ）。
fn get_pair(ch: char) -> (char, char) {
    PAIRS
        .iter()
        .find(|(open, close)| *open == ch || *close == ch)
        .copied()
        .unwrap_or((ch, ch))
}

/// Range の「カーソル」位置（Helix の `Range::cursor` と同じ）。前方選択は最後に
/// 含まれる文字、後方選択（anchor > head）は head そのもの。
fn cursor_pos(text: &str, range: Range) -> usize {
    if range.head() > range.anchor() {
        prev_grapheme_boundary(text, range.head())
    } else {
        range.head()
    }
}

/// `pos` 以前で、対応の取れた開き文字を探す（間に挟まる閉じ/開きは読み飛ばす）。
fn open_pos(chars: &[char], open: char, close: char, pos: usize) -> Option<usize> {
    if chars.get(pos) == Some(&open) {
        return Some(pos);
    }
    let mut step_over = 0usize;
    let mut i = pos;
    while i > 0 {
        i -= 1;
        let c = chars[i];
        if c == close {
            step_over += 1;
        } else if c == open {
            if step_over == 0 {
                return Some(i);
            }
            step_over -= 1;
        }
    }
    None
}

/// `pos` 以降で、対応の取れた閉じ文字を探す。
fn close_pos(chars: &[char], open: char, close: char, pos: usize) -> Option<usize> {
    if chars.get(pos) == Some(&close) {
        return Some(pos);
    }
    let mut step_over = 0usize;
    let mut i = pos + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == open {
            step_over += 1;
        } else if c == close {
            if step_over == 0 {
                return Some(i);
            }
            step_over -= 1;
        }
        i += 1;
    }
    None
}

/// `pos` のカーソルを囲む `ch` のペア位置 `(開き, 閉じ)`。無ければ None。
///
/// 開き == 閉じ（引用符）のとき、カーソルがその文字の上にあるのはどちら側を
/// 探すべきか決まらないため None（Helix の `CursorOnAmbiguousPair`）。
fn find_pair_pos(chars: &[char], pos: usize, ch: char) -> Option<(usize, usize)> {
    let (open, close) = get_pair(ch);
    if open == close {
        if chars.get(pos) == Some(&open) {
            return None;
        }
        let forward = (pos..chars.len()).find(|&i| chars[i] == open)?;
        let backward = (0..pos).rev().find(|&i| chars[i] == open)?;
        return Some((backward, forward));
    }
    Some((
        open_pos(chars, open, close, pos)?,
        close_pos(chars, open, close, pos)?,
    ))
}

/// `ch` のペア位置を Selection の各 Range について求め、位置昇順の
/// `(位置, 開き側か)` で返す。1つでも見つからなければ None（Helix と同じく、
/// 全 Range 揃わないと操作しない）。位置の重複（同じペアを2つの Range が
/// 指した）も None。
fn pair_positions(
    doc: &Document,
    selection: &Selection,
    ch: char,
) -> Option<Vec<(usize, bool)>> {
    let text = doc.text().to_string();
    let chars: Vec<char> = text.chars().collect();
    let mut positions = Vec::with_capacity(selection.len() * 2);
    for range in selection.ranges() {
        let (open, close) = find_pair_pos(&chars, cursor_pos(&text, *range), ch)?;
        positions.push((open, true));
        positions.push((close, false));
    }
    positions.sort_unstable();
    if positions.windows(2).any(|w| w[0].0 == w[1].0) {
        return None;
    }
    Some(positions)
}

/// 各 Range を `ch` のペアで囲む。適用後の選択は囲んだ全体（開き〜閉じ）を
/// 覆う（Helix の `surround_add` と同じ）。向き（anchor/head）は保たれる。
pub fn surround_add(doc: &Document, selection: &Selection, ch: char) -> (Transaction, Selection) {
    let (open, close) = get_pair(ch);
    // 開き1文字 + 閉じ1文字（複数文字ペアは未対応）。
    let surround_len = 2;
    let mut edits: Vec<(usize, usize, String)> = Vec::with_capacity(selection.len() * 2);
    let mut ranges = Vec::with_capacity(selection.len());
    let mut offs = 0;
    for range in selection.ranges() {
        edits.push((range.start(), range.start(), open.to_string()));
        edits.push((range.end(), range.end(), close.to_string()));
        let from = offs + range.start();
        let to = offs + range.end() + surround_len;
        ranges.push(if range.anchor() <= range.head() {
            Range::new(from, to)
        } else {
            Range::new(to, from)
        });
        offs += surround_len;
    }
    (
        Transaction::replace_ranges(doc, &edits),
        Selection::new(ranges, selection.primary_index()),
    )
}

/// 各 Range のカーソルを囲む `ch` のペアを両側とも削除する。
pub fn surround_delete(
    doc: &Document,
    selection: &Selection,
    ch: char,
) -> Option<(Transaction, Selection)> {
    let positions = pair_positions(doc, selection, ch)?;
    let edits: Vec<(usize, usize, String)> = positions
        .iter()
        .map(|&(p, _)| (p, p + 1, String::new()))
        .collect();
    let tx = Transaction::replace_ranges(doc, &edits);
    let selection_after = tx.map_selection(selection, false);
    Some((tx, selection_after))
}

/// 各 Range のカーソルを囲む `from` のペアを `to` のペアに置き換える。
pub fn surround_replace(
    doc: &Document,
    selection: &Selection,
    from: char,
    to: char,
) -> Option<(Transaction, Selection)> {
    let positions = pair_positions(doc, selection, from)?;
    let (open, close) = get_pair(to);
    let edits: Vec<(usize, usize, String)> = positions
        .iter()
        .map(|&(p, is_open)| {
            let ch = if is_open { open } else { close };
            (p, p + 1, ch.to_string())
        })
        .collect();
    let tx = Transaction::replace_ranges(doc, &edits);
    let selection_after = tx.map_selection(selection, true);
    Some((tx, selection_after))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;

    fn sel(ranges: Vec<(usize, usize)>, primary: usize) -> Selection {
        Selection::new(
            ranges.into_iter().map(|(a, h)| Range::new(a, h)).collect(),
            primary,
        )
    }

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn get_pair_resolves_both_sides_and_falls_back() {
        assert_eq!(get_pair('('), ('(', ')'));
        assert_eq!(get_pair(')'), ('(', ')'));
        assert_eq!(get_pair('['), ('[', ']'));
        assert_eq!(get_pair('"'), ('"', '"'));
        assert_eq!(get_pair('x'), ('x', 'x'));
    }

    #[test]
    fn find_pair_pos_handles_nesting_and_cursor_on_closing() {
        // カーソルを囲む最も内側のペア
        let t = chars("(a(b))");
        assert_eq!(find_pair_pos(&t, 3, '('), Some((2, 4))); // 'b' の上
        assert_eq!(find_pair_pos(&t, 1, ')'), Some((0, 5))); // 'a' の上
        assert_eq!(find_pair_pos(&t, 0, '('), Some((0, 5))); // 開き文字の上
        assert_eq!(find_pair_pos(&t, 5, '('), Some((0, 5))); // 閉じ文字の上
        assert_eq!(find_pair_pos(&t, 1, 'x'), None); // 開きの無い文字
        assert_eq!(find_pair_pos(&chars("(ab"), 2, '('), None); // 閉じが無い
    }

    #[test]
    fn find_pair_pos_ambiguous_quote_under_cursor() {
        let t = chars("\"ab\"");
        assert_eq!(find_pair_pos(&t, 1, '"'), Some((0, 3)));
        assert_eq!(find_pair_pos(&t, 2, '"'), Some((0, 3)));
        assert_eq!(find_pair_pos(&t, 0, '"'), None); // 開き引用符の上は曖昧
        assert_eq!(find_pair_pos(&t, 3, '"'), None); // 閉じ引用符の上も曖昧
    }

    #[test]
    fn surround_add_wraps_selection_and_cursor() {
        let doc = Document::from("hello world");
        // 選択を囲む: "hello" を (hello) に
        let selection = sel(vec![(0, 5)], 0);
        let (tx, after) = surround_add(&doc, &selection, '(');
        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), "(hello) world");
        // 選択は囲んだ全体を覆う（開き〜閉じ）
        assert_eq!(after.ranges(), &[Range::new(0, 7)]);
        // カーソル（空 Range）は ( と ) の間に
        let doc2 = Document::from("abc");
        let (tx2, after2) = surround_add(&doc2, &Selection::point(1), '(');
        let new_doc2 = tx2.apply(&doc2);
        assert_eq!(new_doc2.text().to_string(), "a()bc");
        assert_eq!(after2.ranges(), &[Range::new(1, 3)]);
    }

    #[test]
    fn surround_add_multiple_ranges_and_undo() {
        let doc = Document::from("ab cd");
        let selection = sel(vec![(0, 2), (3, 5)], 1);
        let (tx, after) = surround_add(&doc, &selection, '[');
        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), "[ab] [cd]");
        assert_eq!(after.len(), 2);
        assert_eq!(after.ranges()[0], Range::new(0, 4));
        assert_eq!(after.ranges()[1], Range::new(5, 9));
        assert_eq!(after.primary_index(), 1);
        // undo で元に戻る
        let restored = tx.invert().apply(&new_doc);
        assert_eq!(restored.text().to_string(), "ab cd");
    }

    #[test]
    fn surround_add_preserves_backward_direction() {
        let doc = Document::from("hello");
        // 後方選択（anchor > head）: 向きが保たれる
        let selection = Selection::new(vec![Range::new(5, 0)], 0);
        let (tx, after) = surround_add(&doc, &selection, '(');
        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), "(hello)");
        let r = after.ranges()[0];
        assert_eq!((r.anchor(), r.head()), (7, 0));
    }

    #[test]
    fn surround_delete_removes_pair_around_cursor() {
        let doc = Document::from("(abc) def");
        // 'c' の上のカーソル（index 3）
        let selection = Selection::point(3);
        let (tx, after) = surround_delete(&doc, &selection, '(').unwrap();
        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), "abc def");
        // カーソルはそのまま 'c' の上に残る（index 2 にずれる）
        assert_eq!(after, Selection::point(2));
        // undo で戻る
        let restored = tx.invert().apply(&new_doc);
        assert_eq!(restored.text().to_string(), "(abc) def");
    }

    #[test]
    fn surround_delete_quote_and_failure() {
        // 引用符のペア削除（カーソルは文字の上）
        let doc = Document::from("\"hi\" there");
        let (tx, _) = surround_delete(&doc, &Selection::point(2), '"').unwrap();
        assert_eq!(tx.apply(&doc).text().to_string(), "hi there");
        // ペアが無ければ None
        let doc2 = Document::from("no pair here");
        assert!(surround_delete(&doc2, &Selection::point(3), '(').is_none());
    }

    #[test]
    fn surround_delete_fails_if_any_range_has_no_pair() {
        // 片方のカーソルにペアが無い → 全体を no-op（Helix と同じ）
        let doc = Document::from("(a) b");
        let selection = sel(vec![(1, 1), (5, 5)], 0);
        assert!(surround_delete(&doc, &selection, '(').is_none());
    }

    #[test]
    fn surround_replace_swaps_pairs() {
        let doc = Document::from("(abc) (def)");
        let selection = sel(vec![(1, 1), (7, 7)], 0);
        let (tx, _) = surround_replace(&doc, &selection, '(', '[').unwrap();
        assert_eq!(tx.apply(&doc).text().to_string(), "[abc] [def]");
        // 閉じ文字を指定しても同じペアを探す
        let doc2 = Document::from("(abc)");
        let (tx2, _) = surround_replace(&doc2, &Selection::point(2), ')', '{').unwrap();
        assert_eq!(tx2.apply(&doc2).text().to_string(), "{abc}");
        // 引用符 → 括弧
        let doc3 = Document::from("\"x\"");
        let (tx3, _) = surround_replace(&doc3, &Selection::point(1), '"', '(').unwrap();
        assert_eq!(tx3.apply(&doc3).text().to_string(), "(x)");
    }
}
