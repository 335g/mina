//! 履歴: undo / redo のスタック。

use mina_core::{Document, Selection, Transaction};

/// 1回の編集操作（1キー入力など）を表す変更。
///
/// undo 時は `selection_before` へ、redo 時は `selection_after` へ戻る。
#[derive(Clone, Debug)]
struct Change {
    transaction: Transaction,
    selection_before: Selection,
    selection_after: Selection,
}

/// undo / redo の履歴。
///
/// undo の単位は「グループ」。単独の変更は1要素のグループ、
/// [`History::begin_group`] から [`History::end_group`] の間に記録された変更は
/// 1つのグループにまとまる（Insert モードで "hello" と打っても undo 1回で
/// 消える、という挙動のため。トランザクションを合成せず、グループ内の変更を
/// 逆順に逆転して適用する）。
///
/// 関数型のコアに合わせ、undo / redo は新しい文書と選択を返す。
#[derive(Clone, Debug, Default)]
pub struct History {
    undo: Vec<Vec<Change>>,
    redo: Vec<Vec<Change>>,
    grouping: bool,
}

impl History {
    /// 空の履歴を作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// 変更を記録する。新しい変更は redo スタックを空にする。
    pub fn push(
        &mut self,
        transaction: Transaction,
        selection_before: Selection,
        selection_after: Selection,
    ) {
        let change = Change {
            transaction,
            selection_before,
            selection_after,
        };
        if self.grouping {
            match self.undo.last_mut() {
                Some(group) => group.push(change),
                None => self.undo.push(vec![change]),
            }
        } else {
            self.undo.push(vec![change]);
        }
        self.redo.clear();
    }

    /// 以降の [`History::push`] を同じ undo 単位にまとめる。
    pub fn begin_group(&mut self) {
        self.grouping = true;
    }

    /// グループ化を終了する。
    pub fn end_group(&mut self) {
        self.grouping = false;
    }

    /// undo できる変更があるか。
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// redo できる変更があるか。
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// 直近のグループを元に戻す。適用後の文書と選択を返す。
    ///
    /// グループ内の変更は逆順に逆転して適用される（逆転は作成時に
    /// 事前計算されているので、現在の文書だけから適用できる）。
    /// 選択はグループ最初の変更の適用前に戻る。戻すものがない場合は `None`。
    pub fn undo(&mut self, doc: &Document) -> Option<(Document, Selection)> {
        let group = self.undo.pop()?;
        let mut new_doc = doc.clone();
        for change in group.iter().rev() {
            new_doc = change.transaction.invert().apply(&new_doc);
        }
        let selection = group
            .first()
            .expect("グループは常に1つ以上の変更を含む")
            .selection_before
            .clone();
        self.redo.push(group);
        Some((new_doc, selection))
    }

    /// 直近に undo されたグループをやり直す。適用後の文書と選択を返す。
    ///
    /// 選択はグループ最後の変更の適用後へ進む。やり直すものがない場合は `None`。
    pub fn redo(&mut self, doc: &Document) -> Option<(Document, Selection)> {
        let group = self.redo.pop()?;
        let mut new_doc = doc.clone();
        for change in &group {
            new_doc = change.transaction.apply(&new_doc);
        }
        let selection = group
            .last()
            .expect("グループは常に1つ以上の変更を含む")
            .selection_after
            .clone();
        self.undo.push(group);
        Some((new_doc, selection))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 挿入の変更を作って返す（適用後選択は挿入テキストの後ろ）。
    fn insert_change(doc: &Document, sel: &Selection, text: &str) -> (Transaction, Selection) {
        let tx = Transaction::insert(doc, sel, text);
        let after = tx.map_selection(sel, true);
        (tx, after)
    }

    #[test]
    fn push_then_undo_redo_roundtrip() {
        let doc = Document::from("hello");
        let sel = Selection::point(2);
        let (tx, after) = insert_change(&doc, &sel, "X");
        let new_doc = tx.apply(&doc);

        let mut history = History::new();
        history.push(tx, sel.clone(), after.clone());

        let (restored, restored_sel) = history.undo(&new_doc).expect("undo");
        assert_eq!(restored.text().to_string(), "hello");
        assert_eq!(restored_sel, sel);
        assert!(history.can_redo());

        let (redone, redone_sel) = history.redo(&restored).expect("redo");
        assert_eq!(redone.text().to_string(), "heXllo");
        assert_eq!(redone_sel, after);
        assert!(history.can_undo());
    }

    #[test]
    fn undo_restores_intermediate_states() {
        let doc = Document::from("hello");
        let mut history = History::new();

        // 2回の編集: "hello" → "hXello" → "hXYello"
        let sel1 = Selection::point(1);
        let (tx1, after1) = insert_change(&doc, &sel1, "X");
        let doc1 = tx1.apply(&doc);
        history.push(tx1, sel1.clone(), after1.clone());

        let (tx2, after2) = insert_change(&doc1, &after1, "Y");
        let doc2 = tx2.apply(&doc1);
        history.push(tx2, after1.clone(), after2.clone());

        let (back1, back1_sel) = history.undo(&doc2).expect("undo 1");
        assert_eq!(back1.text().to_string(), "hXello");
        assert_eq!(back1_sel, after1);

        let (back2, back2_sel) = history.undo(&back1).expect("undo 2");
        assert_eq!(back2.text().to_string(), "hello");
        assert_eq!(back2_sel, sel1);

        let (fwd2, _) = history.redo(&back2).expect("redo 1");
        assert_eq!(fwd2.text().to_string(), "hXello");
        let (fwd1, fwd1_sel) = history.redo(&fwd2).expect("redo 2");
        assert_eq!(fwd1.text().to_string(), "hXYello");
        assert_eq!(fwd1_sel, after2);
    }

    #[test]
    fn new_push_clears_redo() {
        let doc = Document::from("hello");
        let sel = Selection::point(1);
        let mut history = History::new();

        let (tx, after) = insert_change(&doc, &sel, "X");
        let doc1 = tx.apply(&doc);
        history.push(tx, sel.clone(), after.clone());
        history.undo(&doc1).expect("undo");
        assert!(history.can_redo());

        // 新しい編集は redo を無効化する
        let (tx2, after2) = insert_change(&doc, &Selection::point(4), "Z");
        history.push(tx2, Selection::point(4), after2);
        assert!(!history.can_redo());
    }

    #[test]
    fn group_undoes_all_changes_at_once() {
        let doc = Document::from("hello");
        let mut history = History::new();

        history.begin_group();
        let mut cur = doc.clone();
        let mut sel = Selection::point(2);
        for ch in ["X", "Y"] {
            let (tx, after) = insert_change(&cur, &sel, ch);
            cur = tx.apply(&cur);
            history.push(tx, sel.clone(), after.clone());
            sel = after;
        }
        history.end_group();
        assert_eq!(cur.text().to_string(), "heXYllo");

        // 1回の undo でグループ全体（2変更）が戻る
        let (restored, restored_sel) = history.undo(&cur).expect("undo");
        assert_eq!(restored.text().to_string(), "hello");
        assert_eq!(restored_sel, Selection::point(2));
        assert!(!history.can_undo());

        // redo で全体が復元される
        let (redone, redone_sel) = history.redo(&restored).expect("redo");
        assert_eq!(redone.text().to_string(), "heXYllo");
        assert_eq!(redone_sel, Selection::point(4));
    }

    #[test]
    fn undo_redo_empty_returns_none() {
        let mut history = History::new();
        let doc = Document::from("hello");
        assert_eq!(history.undo(&doc), None);
        assert_eq!(history.redo(&doc), None);
        assert!(!history.can_undo());
        assert!(!history.can_redo());
    }
}
