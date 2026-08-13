//! トランザクション: 文書への変更の単位。invert で undo の土台になる。

use crate::{Document, Range, Selection};
use ropey::RopeBuilder;

/// トランザクションを構成する操作の列の1要素。
///
/// 位置・長さはすべて char インデックス。`Retain` は元の文書の文字を保持、
/// `Delete` は元の文書の文字を削除、`Insert` は新たなテキストを挿入する。
/// 操作列は文書全体を覆う（末尾に `Retain` が残る）ことが不変条件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    /// 元の文書の文字を `usize` 文字分そのまま残す。
    Retain(usize),
    /// 元の文書の文字を `usize` 文字分削除する。
    Delete(usize),
    /// テキストを挿入する。
    Insert(String),
}

/// 文書への変更の単位: [`Operation`] の列。
///
/// 変更は適用（[`Transaction::apply`]）・逆転（[`Transaction::invert`]・
/// 位置の写像（[`Transaction::map_pos`] / [`Transaction::map_selection`]）を
/// 持つ。コアは関数型なので、適用は新しい [`Document`] を返し、元の文書は
/// 変更しない（`docs/adr/0002-functional-core-selection.md` 参照）。
///
/// **逆変換は作成時に事前計算して保持する**。コンストラクタは元の文書を
/// 受け取るため、削除された文字列を逆変換に埋め込める。これにより undo は
/// 「変更後の文書」だけを手にしていても逆転を適用できる（Helix も同様に
/// 逆変換を保持する）。`invert(&old_doc)` のように後から元の文書を要求する
/// 方式は、undo 時には変更前の文書が手元にないため使えない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    operations: Vec<Operation>,
    inverse: Vec<Operation>,
}

impl Transaction {
    /// `selection` の各 Range を `text` で置き換えるトランザクションを作る。
    ///
    /// カーソル（空の Range）は「その位置への挿入」として扱われる。複数
    /// カーソルの場合はすべての位置に同じ `text` が挿入・置換される。
    ///
    /// 操作列では `Insert` を `Delete` の**前**に置く。これにより、置換範囲の
    /// 開始位置にいたカーソルが `map_pos(is_insert = true)` で挿入テキストの
    /// 後ろに写像される（逆順だと削除点に詰められ、挿入テキストの前に来る）。
    pub fn insert(doc: &Document, selection: &Selection, text: &str) -> Self {
        let mut operations = Vec::new();
        let mut inverse = Vec::new();
        let mut pos = 0;
        for range in selection.ranges() {
            let start = range.start();
            let end = range.end();
            operations.push(Operation::Retain(start - pos));
            inverse.push(Operation::Retain(start - pos));
            operations.push(Operation::Insert(text.to_string()));
            inverse.push(Operation::Delete(text.chars().count()));
            if end > start {
                operations.push(Operation::Delete(end - start));
                // 削除された文字列は逆変換に埋め込む（undo 時に元の文書を
                // 必要としないため）
                let deleted = doc.text().slice(start..end).to_string();
                inverse.push(Operation::Insert(deleted));
            }
            pos = end;
        }
        let trailing = doc.len_chars() - pos;
        operations.push(Operation::Retain(trailing));
        inverse.push(Operation::Retain(trailing));
        Self {
            operations,
            inverse,
        }
    }

    /// `selection` が覆う範囲を削除するトランザクションを作る。
    pub fn delete(doc: &Document, selection: &Selection) -> Self {
        let mut operations = Vec::new();
        let mut inverse = Vec::new();
        let mut pos = 0;
        for range in selection.ranges() {
            let start = range.start();
            let end = range.end();
            operations.push(Operation::Retain(start - pos));
            inverse.push(Operation::Retain(start - pos));
            if end > start {
                operations.push(Operation::Delete(end - start));
                let deleted = doc.text().slice(start..end).to_string();
                inverse.push(Operation::Insert(deleted));
            }
            pos = end;
        }
        let trailing = doc.len_chars() - pos;
        operations.push(Operation::Retain(trailing));
        inverse.push(Operation::Retain(trailing));
        Self {
            operations,
            inverse,
        }
    }

    /// このトランザクションを `doc` に適用した新しい文書を返す。
    ///
    /// # Panics
    ///
    /// debug ビルドで、操作列が文書全体を覆っていない場合に panic する。
    pub fn apply(&self, doc: &Document) -> Document {
        let mut builder = RopeBuilder::new();
        let mut old = 0;
        for op in &self.operations {
            match op {
                Operation::Retain(n) => {
                    for chunk in doc.text().slice(old..old + n).chunks() {
                        builder.append(chunk);
                    }
                    old += n;
                }
                Operation::Delete(n) => old += n,
                Operation::Insert(s) => builder.append(s.as_str()),
            }
        }
        debug_assert_eq!(old, doc.len_chars(), "操作列が文書全体を覆っていない");
        Document::from(builder.finish())
    }

    /// このトランザクションの逆転（作成時に事前計算したもの）。
    ///
    /// 逆転を適用結果に適用すると元の文書に戻る。undo に使う。
    pub fn invert(&self) -> Transaction {
        Transaction {
            operations: self.inverse.clone(),
            inverse: self.operations.clone(),
        }
    }

    /// 元の文書の位置 `pos` を、このトランザクション適用後の位置に写像する。
    ///
    /// `is_insert` は挿入点にちょうど位置する場合の挙動を決める: `true` なら
    /// 挿入されたテキストの後ろ、`false` なら前になる。削除された範囲の
    /// 内側の位置は削除開始点に詰められる。
    pub fn map_pos(&self, pos: usize, is_insert: bool) -> usize {
        let mut old = 0;
        let mut new = 0;
        for op in &self.operations {
            match op {
                Operation::Retain(n) => {
                    if pos < old + n {
                        return new + (pos - old);
                    }
                    old += n;
                    new += n;
                }
                Operation::Delete(n) => {
                    if pos <= old + n {
                        return new;
                    }
                    old += n;
                }
                Operation::Insert(s) => {
                    let len = s.chars().count();
                    if pos == old {
                        return new + if is_insert { len } else { 0 };
                    }
                    new += len;
                }
            }
        }
        debug_assert!(pos <= old, "位置が文書の末尾を超えている");
        new
    }

    /// [`Selection`] 全体を、このトランザクション適用後の位置に写像する。
    ///
    /// 各 Range の anchor と head を [`Transaction::map_pos`] で写す。
    /// 代表的な使い方: 挿入後は `is_insert = true` でカーソルを挿入テキストの
    /// 後ろへ、削除後は `is_insert = false` で削除開始点へ移動させる。
    pub fn map_selection(&self, selection: &Selection, is_insert: bool) -> Selection {
        let ranges = selection
            .ranges()
            .iter()
            .map(|r| {
                Range::new(
                    self.map_pos(r.anchor(), is_insert),
                    self.map_pos(r.head(), is_insert),
                )
            })
            .collect();
        Selection::new(ranges, selection.primary_index())
    }

    /// このトランザクションを構成する操作の列。
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }

    /// このトランザクションが文書を実際に変えるか。
    ///
    /// 空文字の `Insert`（カーソル位置への空文字挿入）や `Retain` のみの列
    /// （空範囲の削除・文書先頭の Backspace 等）は適用しても文書が変わらない。
    /// 呼び出し側はこれを元に適用・undo 履歴・イベント記録をスキップできる
    /// （ADR-0012: 状態を変えない操作は世代を進めない）。
    pub fn is_noop(&self) -> bool {
        self.operations.iter().all(|op| match op {
            Operation::Retain(_) => true,
            Operation::Delete(n) => *n == 0,
            Operation::Insert(s) => s.is_empty(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Range;

    fn sel(ranges: Vec<(usize, usize)>, primary: usize) -> Selection {
        Selection::new(
            ranges.into_iter().map(|(a, h)| Range::new(a, h)).collect(),
            primary,
        )
    }

    #[test]
    fn insert_at_cursor_and_invert() {
        let doc = Document::from("hello");
        let selection = Selection::point(1);
        let tx = Transaction::insert(&doc, &selection, "X");

        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), "hXello");

        let restored = tx.invert().apply(&new_doc);
        assert_eq!(restored.text().to_string(), "hello");
    }

    #[test]
    fn insert_replaces_selection() {
        let doc = Document::from("hello world");
        let selection = sel(vec![(6, 11)], 0);
        let tx = Transaction::insert(&doc, &selection, "Rust");

        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), "hello Rust");

        let restored = tx.invert().apply(&new_doc);
        assert_eq!(restored.text().to_string(), "hello world");
    }

    #[test]
    fn delete_selection_and_invert() {
        let doc = Document::from("hello world");
        let selection = sel(vec![(6, 11)], 0);
        let tx = Transaction::delete(&doc, &selection);

        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), "hello ");

        let restored = tx.invert().apply(&new_doc);
        assert_eq!(restored.text().to_string(), "hello world");
    }

    #[test]
    fn multi_cursor_insert() {
        let doc = Document::from("hello");
        let selection = sel(vec![(1, 1), (3, 3)], 0);
        let tx = Transaction::insert(&doc, &selection, "!");

        let new_doc = tx.apply(&doc);
        // カーソル1は h と e の間、カーソル3（元文書の位置）は l と l の間。
        assert_eq!(new_doc.text().to_string(), "h!el!lo");

        let restored = tx.invert().apply(&new_doc);
        assert_eq!(restored.text().to_string(), "hello");
    }

    #[test]
    fn multi_cursor_delete() {
        let doc = Document::from("hello world");
        let selection = sel(vec![(2, 5), (6, 9)], 0);
        let tx = Transaction::delete(&doc, &selection);

        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), "he ld");
    }

    #[test]
    fn insert_into_empty_document() {
        let doc = Document::new();
        let tx = Transaction::insert(&doc, &Selection::point(0), "a");

        let new_doc = tx.apply(&doc);
        assert_eq!(new_doc.text().to_string(), "a");

        let restored = tx.invert().apply(&new_doc);
        assert_eq!(restored.text().to_string(), "");
    }

    #[test]
    fn unicode_edits_use_char_indices() {
        let doc = Document::from("こんにちは世界");
        let tx = Transaction::delete(&doc, &sel(vec![(1, 3)], 0));
        assert_eq!(tx.apply(&doc).text().to_string(), "こちは世界");

        let doc2 = Document::from("こんにちは世界");
        let tx2 = Transaction::insert(&doc2, &Selection::point(5), "!");
        assert_eq!(tx2.apply(&doc2).text().to_string(), "こんにちは!世界");
    }

    #[test]
    fn map_pos_after_insert_with_affinity() {
        let doc = Document::from("hello");
        let tx = Transaction::insert(&doc, &Selection::point(1), "X");

        // 挿入点にいたカーソルは、is_insert=true なら挿入後、false なら挿入前。
        assert_eq!(tx.map_pos(1, true), 2);
        assert_eq!(tx.map_pos(1, false), 1);
        // 挿入点より後ろは shift される。
        assert_eq!(tx.map_pos(3, false), 4);
        // 挿入点より前は変わらない。
        assert_eq!(tx.map_pos(0, false), 0);
    }

    #[test]
    fn map_pos_after_delete_clamps_into_deleted_region() {
        let doc = Document::from("hello world");
        let tx = Transaction::delete(&doc, &sel(vec![(6, 11)], 0));

        assert_eq!(tx.map_pos(6, false), 6);
        // 削除範囲の内側は削除開始点に詰められる。
        assert_eq!(tx.map_pos(8, false), 6);
        assert_eq!(tx.map_pos(11, false), 6);
        // 文書末尾（削除範囲の直後）も削除開始点へ。
    }

    #[test]
    fn map_selection_after_insert_lands_after_text() {
        let doc = Document::from("hello");
        let selection = sel(vec![(1, 1), (3, 3)], 0);
        let tx = Transaction::insert(&doc, &selection, "!");

        let mapped = tx.map_selection(&selection, true);
        assert_eq!(mapped.ranges(), &[Range::point(2), Range::point(5)]);
        assert_eq!(mapped.primary_index(), 0);
    }

    #[test]
    fn map_selection_after_delete_lands_at_deletion_start() {
        let doc = Document::from("hello world");
        let selection = sel(vec![(6, 11)], 0);
        let tx = Transaction::delete(&doc, &selection);

        let mapped = tx.map_selection(&selection, false);
        assert_eq!(mapped, Selection::point(6));
    }

    #[test]
    fn operations_are_exposed_for_inspection() {
        let doc = Document::from("hello");
        let tx = Transaction::insert(&doc, &Selection::point(0), "X");
        assert_eq!(
            tx.operations(),
            &[
                Operation::Retain(0),
                Operation::Insert("X".to_string()),
                Operation::Retain(5),
            ]
        );
    }

    #[test]
    fn is_noop_detects_retain_only_and_empty_insert() {
        let doc = Document::from("hello");
        // 空範囲の削除（カーソル上の DeleteRange 等）は Retain のみ
        assert!(Transaction::delete(&doc, &Selection::point(3)).is_noop());
        // 空文字の挿入も状態を変えない
        assert!(Transaction::insert(&doc, &Selection::point(3), "").is_noop());
        // 実変更のあるトランザクションは no-op ではない
        assert!(!Transaction::insert(&doc, &Selection::point(3), "X").is_noop());
        assert!(!Transaction::delete(&doc, &sel(vec![(1, 4)], 0)).is_noop());
        // 空文字挿入でも選択範囲があれば置換として実変更
        assert!(!Transaction::insert(&doc, &sel(vec![(1, 4)], 0), "").is_noop());
    }
}
