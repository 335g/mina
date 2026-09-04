//! 編集: 文書と選択を新しい状態に変換する関数型プリミティブ。
//!
//! すべて `(document, selection) → (new_document, new_selection)` の形
//! （`docs/adr/0002-functional-core-selection.md` 参照）。内部で
//! [`Transaction`] を作って適用する。

use crate::movement::{
    next_grapheme_boundary, next_word_end, prev_grapheme_boundary, prev_word_start,
};
use crate::{Document, Range, Selection, Transaction};

/// char 位置 `start..end` を `text` で置き換える位置指定編集ファサード
/// （headless エージェントと daemon が共有する「1本の道」。issue #25）。
///
/// - `start == end`: 挿入
/// - `text` が空: 削除
/// - それ以外: 置換
///
/// 3 種すべてが**1つのトランザクション**で処理される（`Transaction::insert` は
/// Range を text で置き換える仕様。replace を delete→insert の2段階で書くと
/// undo が2段階になり原子性が壊れるため）。
///
/// 範囲外（`start > end`、`end > 文書長`）は文書長へクランプする — 呼び出し側で
/// 分岐せずコア側で一括処理する。位置は char インデックス。
///
/// 返り値は「適用後の文書 + 適用に使ったトランザクション」なので、呼び出し側は
/// `tx.invert()` で undo 履歴を自然に支えられる。
pub fn insert_at(doc: &Document, start: usize, end: usize, text: &str) -> (Document, Transaction) {
    let len = doc.len_chars();
    let start = start.min(len);
    let end = end.min(len);
    let selection = Selection::new(vec![Range::new(start, end)], 0);
    let tx = Transaction::insert(doc, &selection, text);
    let new_doc = tx.apply(doc);
    (new_doc, tx)
}

/// `selection` の各 Range を `text` で置き換える。
/// カーソル（空の Range）はその位置への挿入。挿入後、各カーソルは挿入された
/// テキストの直後に置かれる。
pub fn insert_text(doc: &Document, selection: &Selection, text: &str) -> (Document, Selection) {
    let tx = Transaction::insert(doc, selection, text);
    let new_doc = tx.apply(doc);
    let new_selection = tx.map_selection(selection, true);
    (new_doc, new_selection)
}

/// `selection` が覆う範囲を削除する。削除後、各カーソルは削除開始点に置かれる。
pub fn delete_range(doc: &Document, selection: &Selection) -> (Document, Selection) {
    let tx = Transaction::delete(doc, selection);
    let new_doc = tx.apply(doc);
    let new_selection = tx.map_selection(selection, false);
    (new_doc, new_selection)
}

/// 後方削除（Backspace 相当）。カーソルなら直前の1書記素を削除し、
/// 選択があれば選択全体を削除する。
pub fn delete_backward(doc: &Document, selection: &Selection) -> (Document, Selection) {
    let tx = delete_backward_transaction(doc, selection);
    let new_doc = tx.apply(doc);
    let new_selection = tx.map_selection(selection, false);
    (new_doc, new_selection)
}

/// 後方削除のトランザクション。undo 履歴に記録する呼び出し側（Editor 層）向け。
pub fn delete_backward_transaction(doc: &Document, selection: &Selection) -> Transaction {
    // カーソルのみ1書記素分後方へ広げ、選択はそのまま削除対象にする
    let text = doc.text().to_string();
    let ranges: Vec<Range> = selection
        .ranges()
        .iter()
        .map(|r| {
            if r.is_cursor() {
                let prev = prev_grapheme_boundary(&text, r.head());
                Range::new(r.head(), prev) // [prev, head) = 直前の1書記素
            } else {
                *r
            }
        })
        .collect();
    let expanded = Selection::new(ranges, selection.primary_index());
    Transaction::delete(doc, &expanded)
}

/// 後方単語削除（Alt-Backspace / Ctrl-w 相当）。カーソルなら直前の単語
/// （前の単語先頭からカーソルまで）を削除し、選択があれば選択全体を削除する。
pub fn delete_word_backward(doc: &Document, selection: &Selection) -> (Document, Selection) {
    let tx = delete_word_backward_transaction(doc, selection);
    let new_doc = tx.apply(doc);
    let new_selection = tx.map_selection(selection, false);
    (new_doc, new_selection)
}

/// 後方単語削除のトランザクション。undo 履歴に記録する呼び出し側（Editor 層）向け。
pub fn delete_word_backward_transaction(doc: &Document, selection: &Selection) -> Transaction {
    let text = doc.text().to_string();
    let ranges: Vec<Range> = selection
        .ranges()
        .iter()
        .map(|r| {
            if r.is_cursor() {
                let start = prev_word_start(&text, r.head());
                Range::new(r.head(), start) // [start, head) = 直前の単語
            } else {
                *r
            }
        })
        .collect();
    let expanded = Selection::new(ranges, selection.primary_index());
    Transaction::delete(doc, &expanded)
}

/// 前方削除（Delete キー相当）。カーソルなら次の1書記素を削除し、
/// 選択があれば選択全体を削除する。
pub fn delete_forward(doc: &Document, selection: &Selection) -> (Document, Selection) {
    let tx = delete_forward_transaction(doc, selection);
    let new_doc = tx.apply(doc);
    let new_selection = tx.map_selection(selection, false);
    (new_doc, new_selection)
}

/// 前方削除のトランザクション。undo 履歴に記録する呼び出し側（Editor 層）向け。
pub fn delete_forward_transaction(doc: &Document, selection: &Selection) -> Transaction {
    let text = doc.text().to_string();
    let ranges: Vec<Range> = selection
        .ranges()
        .iter()
        .map(|r| {
            if r.is_cursor() {
                let next = next_grapheme_boundary(&text, r.head());
                Range::new(r.head(), next) // [head, next) = 次の1書記素
            } else {
                *r
            }
        })
        .collect();
    let expanded = Selection::new(ranges, selection.primary_index());
    Transaction::delete(doc, &expanded)
}

/// 前方単語削除（Alt-d 相当）。カーソルなら現在/次の単語の末尾までを削除し、
/// 選択があれば選択全体を削除する。
pub fn delete_word_forward(doc: &Document, selection: &Selection) -> (Document, Selection) {
    let tx = delete_word_forward_transaction(doc, selection);
    let new_doc = tx.apply(doc);
    let new_selection = tx.map_selection(selection, false);
    (new_doc, new_selection)
}

/// 前方単語削除のトランザクション。undo 履歴に記録する呼び出し側（Editor 層）向け。
pub fn delete_word_forward_transaction(doc: &Document, selection: &Selection) -> Transaction {
    let text = doc.text().to_string();
    let ranges: Vec<Range> = selection
        .ranges()
        .iter()
        .map(|r| {
            if r.is_cursor() {
                let end = next_word_end(&text, r.head());
                Range::new(r.head(), end) // [head, end) = 次の単語（末尾まで）
            } else {
                *r
            }
        })
        .collect();
    let expanded = Selection::new(ranges, selection.primary_index());
    Transaction::delete(doc, &expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(ranges: Vec<(usize, usize)>, primary: usize) -> Selection {
        Selection::new(
            ranges.into_iter().map(|(a, h)| Range::new(a, h)).collect(),
            primary,
        )
    }

    #[test]
    fn insert_at_inserts_when_start_eq_end() {
        let doc = Document::from("hello");
        let (new_doc, _) = insert_at(&doc, 2, 2, "XY");
        assert_eq!(new_doc.text().to_string(), "heXYllo");
    }

    #[test]
    fn insert_at_deletes_when_text_empty() {
        let doc = Document::from("hello");
        let (new_doc, _) = insert_at(&doc, 1, 4, "");
        assert_eq!(new_doc.text().to_string(), "ho");
    }

    #[test]
    fn insert_at_replaces_range() {
        let doc = Document::from("hello world");
        let (new_doc, _) = insert_at(&doc, 6, 11, "minae");
        assert_eq!(new_doc.text().to_string(), "hello minae");
    }

    #[test]
    fn insert_at_clamps_out_of_range_to_doc_len() {
        let doc = Document::from("abc");
        // end は文書末尾にクランプされ start==end=3 なので挿入になる
        let (new_doc, _) = insert_at(&doc, 10, 20, "!");
        assert_eq!(new_doc.text().to_string(), "abc!");
        // start > end もクランプされ、min..max が削除対象になる
        let doc = Document::from("abcd");
        let (new_doc, _) = insert_at(&doc, 10, 1, "");
        assert_eq!(new_doc.text().to_string(), "a");
    }

    #[test]
    fn insert_at_undo_restores_original() {
        // 挿入・削除・置換のすべてが1トランザクションなので、invert 1回で元に戻る
        for (start, end, text, expected) in [
            (2, 2, "XY", "heXYllo"),
            (1, 4, "", "ho"),
            (1, 4, "minae", "hminaeo"),
        ] {
            let doc = Document::from("hello");
            let (mid, tx) = insert_at(&doc, start, end, text);
            assert_eq!(mid.text().to_string(), expected, "適用結果");
            let restored = tx.invert().apply(&mid);
            assert_eq!(restored.text().to_string(), "hello", "undo で原文に戻る");
        }
    }

    #[test]
    fn insert_at_uses_char_indices() {
        let doc = Document::from("日本語");
        let (new_doc, _) = insert_at(&doc, 1, 1, "X");
        assert_eq!(new_doc.text().to_string(), "日X本語");
        let (new_doc, _) = insert_at(&doc, 0, 1, "");
        assert_eq!(new_doc.text().to_string(), "本語");
    }

    #[test]
    fn insert_at_multi_line_replace() {
        let doc = Document::from("a\nb\nc");
        let (new_doc, _) = insert_at(&doc, 2, 3, "X");
        assert_eq!(new_doc.text().to_string(), "a\nX\nc");
    }

    #[test]
    fn insert_at_empty_text_at_cursor_is_noop() {
        let doc = Document::from("hello");
        let (new_doc, tx) = insert_at(&doc, 2, 2, "");
        assert_eq!(new_doc.text().to_string(), "hello");
        assert!(tx.is_noop());
    }

    #[test]
    fn insert_text_moves_cursor_after_text() {
        let doc = Document::from("hello");
        let (new_doc, selection) = insert_text(&doc, &Selection::point(2), "XY");
        assert_eq!(new_doc.text().to_string(), "heXYllo");
        assert_eq!(selection, Selection::point(4));
    }

    #[test]
    fn insert_text_replaces_selection() {
        let doc = Document::from("hello");
        let (new_doc, selection) = insert_text(&doc, &sel(vec![(2, 5)], 0), "Z");
        assert_eq!(new_doc.text().to_string(), "heZ");
        assert_eq!(selection, Selection::point(3));
    }

    #[test]
    fn insert_text_multi_cursor() {
        let doc = Document::from("hello");
        let selection = sel(vec![(1, 1), (3, 3)], 0);
        let (new_doc, mapped) = insert_text(&doc, &selection, "!");
        assert_eq!(new_doc.text().to_string(), "h!el!lo");
        assert_eq!(mapped, sel(vec![(2, 2), (5, 5)], 0));
    }

    #[test]
    fn delete_range_deletes_selection() {
        let doc = Document::from("hello world");
        let (new_doc, selection) = delete_range(&doc, &sel(vec![(6, 11)], 0));
        assert_eq!(new_doc.text().to_string(), "hello ");
        assert_eq!(selection, Selection::point(6));
    }

    #[test]
    fn delete_range_on_cursor_is_noop() {
        let doc = Document::from("hello");
        let (new_doc, selection) = delete_range(&doc, &Selection::point(3));
        assert_eq!(new_doc.text().to_string(), "hello");
        assert_eq!(selection, Selection::point(3));
    }

    #[test]
    fn delete_backward_deletes_previous_char() {
        let doc = Document::from("hello");
        let (new_doc, selection) = delete_backward(&doc, &Selection::point(3));
        assert_eq!(new_doc.text().to_string(), "helo");
        assert_eq!(selection, Selection::point(2));
    }

    #[test]
    fn delete_backward_with_selection_deletes_selection() {
        let doc = Document::from("hello");
        let (new_doc, selection) = delete_backward(&doc, &sel(vec![(1, 4)], 0));
        assert_eq!(new_doc.text().to_string(), "ho");
        assert_eq!(selection, Selection::point(1));
    }

    #[test]
    fn delete_backward_at_start_is_noop() {
        let doc = Document::from("hello");
        let (new_doc, selection) = delete_backward(&doc, &Selection::point(0));
        assert_eq!(new_doc.text().to_string(), "hello");
        assert_eq!(selection, Selection::point(0));
    }

    #[test]
    fn delete_backward_deletes_one_grapheme() {
        // "が"（か + 結合濁点）は1書記素としてまとめて削除される
        let doc = Document::from("がい");
        let (new_doc, selection) = delete_backward(&doc, &Selection::point(2));
        assert_eq!(new_doc.text().to_string(), "い");
        assert_eq!(selection, Selection::point(0));
    }

    #[test]
    fn delete_word_backward_deletes_previous_word() {
        let doc = Document::from("hello world");
        // カーソルは単語の直後。直前の単語だけが消え、空白は残る（Helix と同じ）
        let (new_doc, selection) = delete_word_backward(&doc, &Selection::point(11));
        assert_eq!(new_doc.text().to_string(), "hello ");
        assert_eq!(selection, Selection::point(6));
    }

    #[test]
    fn delete_word_backward_mid_word_deletes_whole_word() {
        let doc = Document::from("hello world");
        // 単語の途中なら単語先頭まで遡って削除（hel|lo → lo）
        let (new_doc, selection) = delete_word_backward(&doc, &Selection::point(3));
        assert_eq!(new_doc.text().to_string(), "lo world");
        assert_eq!(selection, Selection::point(0));
    }

    #[test]
    fn delete_word_backward_at_start_is_noop() {
        let doc = Document::from("hello");
        let (new_doc, selection) = delete_word_backward(&doc, &Selection::point(0));
        assert_eq!(new_doc.text().to_string(), "hello");
        assert_eq!(selection, Selection::point(0));
    }

    #[test]
    fn delete_word_backward_with_selection_deletes_selection() {
        let doc = Document::from("hello world");
        let (new_doc, selection) = delete_word_backward(&doc, &sel(vec![(1, 4)], 0));
        assert_eq!(new_doc.text().to_string(), "ho world");
        assert_eq!(selection, Selection::point(1));
    }

    #[test]
    fn delete_word_forward_deletes_next_word() {
        let doc = Document::from("hello world");
        // 単語の途中なら現在の単語の残りを削除（he|llo → he world）
        let (new_doc, selection) = delete_word_forward(&doc, &Selection::point(2));
        assert_eq!(new_doc.text().to_string(), "he world");
        assert_eq!(selection, Selection::point(2));
        // 単語先頭ならその単語全体
        let (new_doc, selection) = delete_word_forward(&doc, &Selection::point(6));
        assert_eq!(new_doc.text().to_string(), "hello ");
        assert_eq!(selection, Selection::point(6));
    }

    #[test]
    fn delete_word_forward_at_end_is_noop() {
        let doc = Document::from("hello");
        let (new_doc, selection) = delete_word_forward(&doc, &Selection::point(5));
        assert_eq!(new_doc.text().to_string(), "hello");
        assert_eq!(selection, Selection::point(5));
    }

    #[test]
    fn delete_word_forward_with_selection_deletes_selection() {
        let doc = Document::from("hello world");
        let (new_doc, selection) = delete_word_forward(&doc, &sel(vec![(1, 4)], 0));
        assert_eq!(new_doc.text().to_string(), "ho world");
        assert_eq!(selection, Selection::point(1));
    }

    #[test]
    fn delete_then_insert_restores_text() {
        // 手動 undo: 削除 → 同じ文字を挿入で元に戻る
        let doc = Document::from("hello");
        let (mid, selection) = delete_backward(&doc, &Selection::point(3));
        assert_eq!(mid.text().to_string(), "helo");
        let (restored, _) = insert_text(&mid, &selection, "l");
        assert_eq!(restored.text().to_string(), "hello");
    }

    #[test]
    fn delete_forward_deletes_next_char() {
        let doc = Document::from("hello");
        let (new_doc, selection) = delete_forward(&doc, &Selection::point(1));
        assert_eq!(new_doc.text().to_string(), "hllo");
        assert_eq!(selection, Selection::point(1));
    }

    #[test]
    fn delete_forward_with_selection_deletes_selection() {
        let doc = Document::from("hello");
        let (new_doc, selection) = delete_forward(&doc, &sel(vec![(1, 4)], 0));
        assert_eq!(new_doc.text().to_string(), "ho");
        assert_eq!(selection, Selection::point(1));
    }

    #[test]
    fn delete_forward_at_end_is_noop() {
        let doc = Document::from("hello");
        let (new_doc, selection) = delete_forward(&doc, &Selection::point(5));
        assert_eq!(new_doc.text().to_string(), "hello");
        assert_eq!(selection, Selection::point(5));
    }

    #[test]
    fn delete_backward_transaction_inverts() {
        // トランザクション版: invert で undo できる
        let doc = Document::from("hello");
        let tx = delete_backward_transaction(&doc, &Selection::point(3));
        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), "helo");
        let restored = tx.invert().apply(&new_doc);
        assert_eq!(restored.text().to_string(), "hello");
    }
}
