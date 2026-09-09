//! 検索: 文書内のパターン一致を探す関数型プリミティブ。
//!
//! 主役は [`find_matches`] — 一致をすべて [`Selection`]（複数カーソル）として
//! 返す。これが「全一致を選択して一括編集する」操作（Kakoune/Helix の `*` 相当）
//! の土台になる。[`find_next`] はカーソル位置からの前方検索（ナビゲーション用）。
//!
//! 現状は部分文字列検索。正規表現対応は `regex` クレートを導入する後続ステップで、
//! API は変わらない（内部のスキャンだけ差し替える）。
//!
//! パフォーマンス注記: 移動と同様、検索ごとに文書全体を `String` に
//! マテリアライズする。ponytail: 必要になったら ropey のチャンク上で走査する。

use crate::{Document, Range, Selection};

/// 大文字小文字の扱い。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseSensitivity {
    /// クエリに大文字が含まれていれば区別し、なければ区別しない（Helix の
    /// smart-case と同じ挙動）。既定値。
    Smart,
    /// 常に区別する。
    Sensitive,
    /// 常に区別しない。
    Insensitive,
}

/// `query` に一致する範囲をすべて見つけ、[`Selection`] として返す。
///
/// 一致は左から走査し、**重複しない**（前の一致の直後から次を探す）。
/// primary は最初の一致。一致がなければ `None`。
pub fn find_matches(doc: &Document, query: &str, case: CaseSensitivity) -> Option<Selection> {
    let text = doc.text().to_string();
    let insensitive = is_insensitive(query, case);
    let mut ranges = Vec::new();
    let mut start = 0;
    while let Some((s, e)) = scan_from(&text, query, start, insensitive) {
        ranges.push(Range::new(s, e));
        start = e;
    }
    if ranges.is_empty() {
        None
    } else {
        Some(Selection::new(ranges, 0))
    }
}

/// `from`（含む）以降で最初に一致する範囲を返す。なければ `None`。
/// 文書末尾を越えた折り返し（wrap-around）は呼び出し側（ビュー層）の責務。
pub fn find_next(doc: &Document, query: &str, from: usize, case: CaseSensitivity) -> Option<Range> {
    let text = doc.text().to_string();
    let insensitive = is_insensitive(query, case);
    scan_from(&text, query, from, insensitive).map(|(s, e)| Range::new(s, e))
}

/// `from`（含む）以前で最後に一致する範囲を返す（`?` / `N` の後方検索）。
/// 一致の開始位置が `from` 以下のうち最も後ろのものを返す。なければ `None`。
/// 文書先頭を越えた折り返しは呼び出し側の責務。
pub fn find_prev(doc: &Document, query: &str, from: usize, case: CaseSensitivity) -> Option<Range> {
    let text = doc.text().to_string();
    let insensitive = is_insensitive(query, case);
    let text_chars: Vec<char> = text.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();
    if query_chars.is_empty() {
        return None;
    }
    // 前方走査で開始位置が from 以下の最後の一致を追う（後方走査の代わり。
    // 文書が短いので O(n) で十分）。
    let mut last = None;
    let mut i = 0;
    while i + query_chars.len() <= text_chars.len() {
        if matches_at(&text_chars, &query_chars, i, insensitive) && i <= from {
            last = Some((i, i + query_chars.len()));
        }
        i += 1;
    }
    last.map(|(s, e)| Range::new(s, e))
}

fn is_insensitive(query: &str, case: CaseSensitivity) -> bool {
    match case {
        CaseSensitivity::Sensitive => false,
        CaseSensitivity::Insensitive => true,
        // クエリに大文字が含まれる場合のみ区別する
        CaseSensitivity::Smart => query.chars().all(|c| !c.is_uppercase()),
    }
}

/// `start`（char インデックス、含む）から左端最長で走査し、最初の一致の
/// char 範囲を返す。空クエリは常に `None`。
fn scan_from(text: &str, query: &str, start: usize, insensitive: bool) -> Option<(usize, usize)> {
    let text_chars: Vec<char> = text.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();
    if query_chars.is_empty() {
        return None;
    }
    let mut i = start;
    while i + query_chars.len() <= text_chars.len() {
        if matches_at(&text_chars, &query_chars, i, insensitive) {
            return Some((i, i + query_chars.len()));
        }
        i += 1;
    }
    None
}

/// `pos` の位置にクエリが一致するか。大文字小文字を区別しない場合は
/// 文字ごとに `to_lowercase` で比較する（'İ' のような文字数が変わる
/// フォールディングでも位置ずれしない。ただし完全な Unicode case folding
/// ではない — ponytail: 必要なのは実用上の言語で顕在化してからでよい）。
fn matches_at(text: &[char], query: &[char], pos: usize, insensitive: bool) -> bool {
    text[pos..pos + query.len()]
        .iter()
        .zip(query)
        .all(|(t, q)| {
            if insensitive {
                t.to_lowercase().eq(q.to_lowercase())
            } else {
                t == q
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(ranges: Vec<(usize, usize)>) -> Selection {
        Selection::new(
            ranges.into_iter().map(|(a, h)| Range::new(a, h)).collect(),
            0,
        )
    }

    #[test]
    fn find_matches_basic() {
        let doc = Document::from("hello world, hello again");
        let matches = find_matches(&doc, "hello", CaseSensitivity::Smart).unwrap();
        assert_eq!(matches, sel(vec![(0, 5), (13, 18)]));
        assert_eq!(matches.primary_index(), 0);
    }

    #[test]
    fn find_matches_no_match_returns_none() {
        let doc = Document::from("hello");
        assert_eq!(find_matches(&doc, "xyz", CaseSensitivity::Smart), None);
        assert_eq!(find_matches(&doc, "", CaseSensitivity::Smart), None);
    }

    #[test]
    fn find_matches_are_non_overlapping() {
        let doc = Document::from("aaa");
        let matches = find_matches(&doc, "aa", CaseSensitivity::Smart).unwrap();
        assert_eq!(matches, sel(vec![(0, 2)]));
    }

    #[test]
    fn find_matches_case_sensitivity() {
        let doc = Document::from("Hello hello");
        let sensitive = find_matches(&doc, "hello", CaseSensitivity::Sensitive).unwrap();
        assert_eq!(sensitive, sel(vec![(6, 11)]));
        let insensitive = find_matches(&doc, "hello", CaseSensitivity::Insensitive).unwrap();
        assert_eq!(insensitive, sel(vec![(0, 5), (6, 11)]));
        // Smart: クエリが小文字のみなら区別しない
        assert_eq!(
            find_matches(&doc, "hello", CaseSensitivity::Smart),
            Some(insensitive)
        );
        // Smart: クエリに大文字があれば区別する
        let smart = find_matches(&doc, "Hello", CaseSensitivity::Smart).unwrap();
        assert_eq!(smart, sel(vec![(0, 5)]));
    }

    #[test]
    fn find_matches_unicode() {
        let doc = Document::from("こんにちは世界 こんばんは");
        let matches = find_matches(&doc, "こん", CaseSensitivity::Smart).unwrap();
        assert_eq!(matches, sel(vec![(0, 2), (8, 10)]));
        // 大文字小文字のない文字は常に一致
        let doc2 = Document::from("ABC abc");
        let matches2 = find_matches(&doc2, "abc", CaseSensitivity::Insensitive).unwrap();
        assert_eq!(matches2, sel(vec![(0, 3), (4, 7)]));
    }

    #[test]
    fn find_next_from_position() {
        let doc = Document::from("hello world hello");
        assert_eq!(
            find_next(&doc, "hello", 0, CaseSensitivity::Smart),
            Some(Range::new(0, 5))
        );
        assert_eq!(
            find_next(&doc, "hello", 5, CaseSensitivity::Smart),
            Some(Range::new(12, 17))
        );
        // それ以降に一致なし
        assert_eq!(find_next(&doc, "hello", 13, CaseSensitivity::Smart), None);
        // 一致が `from` の手前から始まっていても、位置が重なっていれば見つかる
        let doc2 = Document::from("aabc");
        assert_eq!(
            find_next(&doc2, "abc", 1, CaseSensitivity::Smart),
            Some(Range::new(1, 4))
        );
    }

    #[test]
    fn find_prev_returns_last_match_at_or_before_from() {
        let doc = Document::from("foo bar foo baz");
        assert_eq!(
            find_prev(&doc, "foo", 16, CaseSensitivity::Smart),
            Some(Range::new(8, 11)),
            "文末から → 最後の一致"
        );
        assert_eq!(
            find_prev(&doc, "foo", 8, CaseSensitivity::Smart),
            Some(Range::new(8, 11)),
            "一致の開始位置 == from も含む"
        );
        assert_eq!(
            find_prev(&doc, "foo", 7, CaseSensitivity::Smart),
            Some(Range::new(0, 3)),
            "from より前の最後の一致"
        );
        assert_eq!(
            find_prev(&doc, "foo", 0, CaseSensitivity::Smart),
            Some(Range::new(0, 3))
        );
        assert_eq!(find_prev(&doc, "xyz", 16, CaseSensitivity::Smart), None);
        assert_eq!(
            find_prev(&doc, "", 16, CaseSensitivity::Smart),
            None,
            "空クエリ"
        );
        // 大文字小文字を区別する場合
        let mixed = Document::from("Foo foo");
        assert_eq!(
            find_prev(&mixed, "Foo", 7, CaseSensitivity::Sensitive),
            Some(Range::new(0, 3))
        );
        assert_eq!(
            find_prev(&mixed, "foo", 7, CaseSensitivity::Sensitive),
            Some(Range::new(4, 7))
        );
    }

    #[test]
    fn select_all_matches_then_edit_all() {
        // Kakoune/Helix の `*` の流れ: 全一致を複数カーソル化して一括置換する
        let doc = Document::from("foo bar foo baz foo");
        let matches = find_matches(&doc, "foo", CaseSensitivity::Smart).unwrap();
        let (new_doc, selection) = crate::insert_text(&doc, &matches, "X");
        assert_eq!(new_doc.text().to_string(), "X bar X baz X");
        assert_eq!(
            selection,
            sel(vec![(1, 1), (7, 7), (13, 13)]) // 各 X の直後にカーソル
        );
    }
}
