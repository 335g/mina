//! 編集: 文書と選択を新しい状態に変換する関数型プリミティブ。
//!
//! すべて `(document, selection) → (new_document, new_selection)` の形
//! （`docs/adr/0002-functional-core-selection.md` 参照）。内部で
//! [`Transaction`] を作って適用する。

use crate::movement::prev_grapheme_boundary;
use crate::{Document, Range, Selection, Transaction};

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
    delete_range(doc, &expanded)
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
    fn delete_then_insert_restores_text() {
        // 手動 undo: 削除 → 同じ文字を挿入で元に戻る
        let doc = Document::from("hello");
        let (mid, selection) = delete_backward(&doc, &Selection::point(3));
        assert_eq!(mid.text().to_string(), "helo");
        let (restored, _) = insert_text(&mid, &selection, "l");
        assert_eq!(restored.text().to_string(), "hello");
    }
}
