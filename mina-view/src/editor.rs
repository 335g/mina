//! エディタ状態: 文書の集合・アクティブな View・モード・履歴。

use std::collections::BTreeMap;

use mina_core::{Document, Selection, Transaction};

use crate::Mode;
use crate::history::History;

/// 文書を一意に識別する ID。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DocumentId(pub usize);

/// 1つの表示領域（将来のスプリット1つ分）を表す。
///
/// どの文書を表示しているかと、その文書でのアクティブな選択を保持する。
/// スクロール位置（viewport）はターミナル描画の段階で追加する。
#[derive(Clone, Debug)]
pub struct View {
    pub doc: DocumentId,
    pub selection: Selection,
}

/// エディタのグローバル状態。
///
/// 関数型コアの「現在の状態」を保持する imperative shell。コアの変換は
/// トランザクションとして [`Editor::apply`] に渡し、文書・選択・履歴を
/// 更新する。
#[derive(Clone, Debug)]
pub struct Editor {
    documents: BTreeMap<DocumentId, Document>,
    histories: BTreeMap<DocumentId, History>,
    next_document_id: usize,
    view: View,
    mode: Mode,
}

impl Editor {
    /// 空のスクラッチ文書を1つ持つエディタを作る。
    pub fn new() -> Self {
        let scratch = DocumentId(0);
        let mut editor = Self {
            documents: BTreeMap::new(),
            histories: BTreeMap::new(),
            next_document_id: 1,
            view: View {
                doc: scratch,
                selection: Selection::point(0),
            },
            mode: Mode::Normal,
        };
        editor.documents.insert(scratch, Document::new());
        editor.histories.insert(scratch, History::new());
        editor
    }

    /// `doc` を開き、新しい文書 ID を返す。
    ///
    /// 単一 View のため、View は開いた文書へ移動する（分割表示は Tree の
    /// 導入時に対応する）。履歴は文書ごとに独立して持つ。
    pub fn open(&mut self, doc: Document) -> DocumentId {
        let id = DocumentId(self.next_document_id);
        self.next_document_id += 1;
        self.documents.insert(id, doc);
        self.histories.insert(id, History::new());
        self.view.doc = id;
        self.view.selection = Selection::point(0);
        id
    }

    /// 文書を ID で取得する。
    ///
    /// # Panics
    ///
    /// 存在しない ID の場合に panic する。
    pub fn document(&self, id: DocumentId) -> &Document {
        &self.documents[&id]
    }

    /// アクティブな View が表示している文書。
    pub fn current_document(&self) -> &Document {
        self.document(self.view.doc)
    }

    /// アクティブな View。
    pub fn view(&self) -> &View {
        &self.view
    }

    /// 現在の選択。
    pub fn selection(&self) -> Selection {
        self.view.selection.clone()
    }

    /// 現在の選択を置き換える。
    pub fn set_selection(&mut self, selection: Selection) {
        self.view.selection = selection;
    }

    /// 現在のモード。
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// モードを切り替える。
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// トランザクションを現在の文書に適用し、履歴に記録する。
    ///
    /// `selection_after` は適用後の選択（挿入なら挿入テキストの後ろ、削除なら
    /// 削除開始点）。redo 時にここへ戻る。例:
    ///
    /// ```
    /// use mina_core::{Selection, Transaction};
    /// use mina_view::Editor;
    ///
    /// let mut editor = Editor::new();
    /// let selection = Selection::point(0);
    /// let tx = Transaction::insert(editor.current_document(), &selection, "Hello");
    /// let selection_after = tx.map_selection(&selection, true);
    /// editor.apply(tx, selection_after);
    /// assert_eq!(editor.current_document().text().to_string(), "Hello");
    /// editor.undo();
    /// assert!(editor.current_document().is_empty());
    /// ```
    pub fn apply(&mut self, transaction: Transaction, selection_after: Selection) {
        let doc_id = self.view.doc;
        let old_doc = self.documents[&doc_id].clone();
        let selection_before = self.view.selection.clone();
        let new_doc = transaction.apply(&old_doc);
        self.history_mut()
            .push(transaction, selection_before, selection_after.clone());
        self.documents.insert(doc_id, new_doc);
        self.view.selection = selection_after;
    }

    /// 現在の文書について undo できる変更があるか。
    pub fn can_undo(&self) -> bool {
        self.history().can_undo()
    }

    /// 現在の文書について redo できる変更があるか。
    pub fn can_redo(&self) -> bool {
        self.history().can_redo()
    }

    /// 現在の文書の直近の変更グループを元に戻す。
    pub fn undo(&mut self) {
        let doc_id = self.view.doc;
        let old_doc = self.documents[&doc_id].clone();
        if let Some((new_doc, selection)) = self.history_mut().undo(&old_doc) {
            self.documents.insert(doc_id, new_doc);
            self.view.selection = selection;
        }
    }

    /// 直近に undo された変更グループをやり直す。
    pub fn redo(&mut self) {
        let doc_id = self.view.doc;
        let old_doc = self.documents[&doc_id].clone();
        if let Some((new_doc, selection)) = self.history_mut().redo(&old_doc) {
            self.documents.insert(doc_id, new_doc);
            self.view.selection = selection;
        }
    }

    /// 以降の [`Editor::apply`] を1つの undo 単位にまとめる。
    /// 例: Insert モードに入ったとき呼び、抜けるときに終了する。
    pub fn begin_group(&mut self) {
        self.history_mut().begin_group();
    }

    /// グループ化を終了する。
    pub fn end_group(&mut self) {
        self.history_mut().end_group();
    }

    fn history(&self) -> &History {
        &self.histories[&self.view.doc]
    }

    fn history_mut(&mut self) -> &mut History {
        self.histories
            .get_mut(&self.view.doc)
            .expect("ビューが指す文書の履歴が存在しない")
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mina_core::{CaseSensitivity, Range, find_matches};

    #[test]
    fn new_editor_has_scratch_document_and_normal_mode() {
        let editor = Editor::new();
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.view().doc, DocumentId(0));
        assert!(editor.current_document().is_empty());
        assert_eq!(editor.selection(), Selection::point(0));
        assert!(!editor.can_undo());
        assert!(!editor.can_redo());
    }

    #[test]
    fn open_switches_view_to_new_document() {
        let mut editor = Editor::new();
        let id = editor.open(Document::from("hello"));
        assert_eq!(editor.view().doc, id);
        assert_eq!(editor.current_document().text().to_string(), "hello");
        // 元のスクラッチ文書は残っている
        assert!(editor.document(DocumentId(0)).is_empty());
    }

    #[test]
    fn apply_writes_document_selection_and_history() {
        let mut editor = Editor::new();
        editor.open(Document::from("hello"));
        let selection = Selection::point(2);
        editor.set_selection(selection.clone());
        let tx = Transaction::insert(editor.current_document(), &selection, "XY");
        let selection_after = tx.map_selection(&selection, true);
        editor.apply(tx, selection_after);

        assert_eq!(editor.current_document().text().to_string(), "heXYllo");
        assert_eq!(editor.selection(), Selection::point(4));
        assert!(editor.can_undo());

        editor.undo();
        assert_eq!(editor.current_document().text().to_string(), "hello");
        assert_eq!(editor.selection(), selection);
        assert!(!editor.can_undo());
        assert!(editor.can_redo());

        editor.redo();
        assert_eq!(editor.current_document().text().to_string(), "heXYllo");
        assert_eq!(editor.selection(), Selection::point(4));
    }

    #[test]
    fn apply_only_touches_the_view_document() {
        let mut editor = Editor::new();
        editor.open(Document::from("aaa"));
        editor.open(Document::from("bbb"));
        let selection = Selection::point(0);
        let tx = Transaction::insert(editor.current_document(), &selection, "!");
        let selection_after = tx.map_selection(&selection, true);
        editor.apply(tx, selection_after);

        // 現在の文書だけが変わり、以前に開いた文書は無傷
        assert_eq!(editor.current_document().text().to_string(), "!bbb");
        assert_eq!(editor.document(DocumentId(1)).text().to_string(), "aaa");
    }

    #[test]
    fn select_all_matches_edit_and_undo_through_editor() {
        // Kakoune/Helix の `*` の流れを Editor 経由で（undo 付き）
        let mut editor = Editor::new();
        editor.open(Document::from("foo bar foo"));
        let matches = find_matches(editor.current_document(), "foo", CaseSensitivity::Smart)
            .expect("一致がある");
        editor.set_selection(matches.clone());
        let tx = Transaction::insert(editor.current_document(), &matches, "X");
        let selection_after = tx.map_selection(&matches, true);
        editor.apply(tx, selection_after);

        assert_eq!(editor.current_document().text().to_string(), "X bar X");
        editor.undo();
        assert_eq!(editor.current_document().text().to_string(), "foo bar foo");
    }

    #[test]
    fn delete_through_editor_with_undo() {
        let mut editor = Editor::new();
        editor.open(Document::from("hello"));
        let selection = Selection::new(vec![Range::new(2, 5)], 0);
        editor.set_selection(selection.clone());
        let tx = Transaction::delete(editor.current_document(), &selection);
        let selection_after = tx.map_selection(&selection, false);
        editor.apply(tx, selection_after);

        assert_eq!(editor.current_document().text().to_string(), "he");
        assert_eq!(editor.selection(), Selection::point(2));

        editor.undo();
        assert_eq!(editor.current_document().text().to_string(), "hello");
        assert_eq!(editor.selection(), selection);
    }

    #[test]
    fn grouped_typing_undoes_in_one_step() {
        // Insert モード相当: "hello" と打っても undo 1回で消える
        let mut editor = Editor::new();
        editor.open(Document::from(""));
        editor.begin_group();
        for ch in "hello".chars() {
            let selection = editor.selection();
            let tx = Transaction::insert(editor.current_document(), &selection, &ch.to_string());
            let selection_after = tx.map_selection(&selection, true);
            editor.apply(tx, selection_after);
        }
        editor.end_group();
        assert_eq!(editor.current_document().text().to_string(), "hello");

        editor.undo();
        assert!(editor.current_document().is_empty());
        assert_eq!(editor.selection(), Selection::point(0));
        assert!(!editor.can_undo());

        editor.redo();
        assert_eq!(editor.current_document().text().to_string(), "hello");
        assert_eq!(editor.selection(), Selection::point(5));
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut editor = Editor::new();
        let selection = Selection::point(0);
        let tx = Transaction::insert(editor.current_document(), &selection, "!");
        let selection_after = tx.map_selection(&selection, true);
        editor.apply(tx, selection_after);
        editor.undo();
        assert!(editor.can_redo());

        let tx2 = Transaction::insert(editor.current_document(), &Selection::point(0), "?");
        let selection_after2 = tx2.map_selection(&Selection::point(0), true);
        editor.apply(tx2, selection_after2);
        assert!(!editor.can_redo());
    }

    #[test]
    fn history_is_per_document() {
        let mut editor = Editor::new();
        editor.open(Document::from("aaa"));
        let selection = Selection::point(0);
        let tx = Transaction::insert(editor.current_document(), &selection, "!");
        let selection_after = tx.map_selection(&selection, true);
        editor.apply(tx, selection_after);
        assert!(editor.can_undo());

        // 別の文書へ切り替えると、その文書の履歴は空
        editor.open(Document::from("bbb"));
        assert!(!editor.can_undo());

        let tx2 = Transaction::insert(editor.current_document(), &Selection::point(0), "?");
        let selection_after2 = tx2.map_selection(&Selection::point(0), true);
        editor.apply(tx2, selection_after2);
        assert!(editor.can_undo());
        editor.undo();
        assert_eq!(editor.current_document().text().to_string(), "bbb");
        assert!(!editor.can_undo());
    }
}
