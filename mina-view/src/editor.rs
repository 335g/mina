//! エディタ状態: 文書の集合・アクティブな View・モード。

use std::collections::BTreeMap;

use mina_core::{Document, Selection};

use crate::Mode;

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
/// 関数型コアの「現在の状態」を保持する imperative shell。コアの変換
/// （`(document, selection) → (document, selection)`）を
/// [`Editor::apply_edit`] 経由で適用して状態を更新する。
#[derive(Clone, Debug)]
pub struct Editor {
    documents: BTreeMap<DocumentId, Document>,
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
            next_document_id: 1,
            view: View {
                doc: scratch,
                selection: Selection::point(0),
            },
            mode: Mode::Normal,
        };
        editor.documents.insert(scratch, Document::new());
        editor
    }

    /// `doc` を開き、新しい文書 ID を返す。
    ///
    /// 単一 View のため、View は開いた文書へ移動する（分割表示は Tree の
    /// 導入時に対応する）。
    pub fn open(&mut self, doc: Document) -> DocumentId {
        let id = DocumentId(self.next_document_id);
        self.next_document_id += 1;
        self.documents.insert(id, doc);
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

    /// 関数型の編集操作を現在の文書と選択に適用し、結果を状態へ書き戻す。
    ///
    /// コアの関数（`mina_core::insert_text` など）は `(Document, Selection) →
    /// (Document, Selection)` の形なので、そのまま渡せる。
    ///
    /// ```
    /// use mina_core::insert_text;
    /// use mina_view::Editor;
    ///
    /// let mut editor = Editor::new();
    /// editor.apply_edit(|doc, sel| insert_text(doc, sel, "Hello"));
    /// assert_eq!(editor.current_document().text().to_string(), "Hello");
    /// ```
    pub fn apply_edit(&mut self, f: impl FnOnce(&Document, &Selection) -> (Document, Selection)) {
        let doc_id = self.view.doc;
        let doc = self.documents[&doc_id].clone();
        let (new_doc, new_selection) = f(&doc, &self.view.selection);
        self.documents.insert(doc_id, new_doc);
        self.view.selection = new_selection;
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
    use mina_core::{CaseSensitivity, delete_backward, find_matches, insert_text};

    #[test]
    fn new_editor_has_scratch_document_and_normal_mode() {
        let editor = Editor::new();
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.view().doc, DocumentId(0));
        assert!(editor.current_document().is_empty());
        assert_eq!(editor.selection(), Selection::point(0));
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
    fn apply_edit_writes_document_and_selection_back() {
        let mut editor = Editor::new();
        editor.open(Document::from("hello"));
        editor.set_selection(Selection::point(2));
        editor.apply_edit(|doc, sel| insert_text(doc, sel, "XY"));
        assert_eq!(editor.current_document().text().to_string(), "heXYllo");
        assert_eq!(editor.selection(), Selection::point(4));
    }

    #[test]
    fn apply_edit_only_touches_the_view_document() {
        let mut editor = Editor::new();
        editor.open(Document::from("aaa"));
        editor.open(Document::from("bbb"));
        editor.apply_edit(|doc, sel| insert_text(doc, sel, "!"));
        // 現在の文書だけが変わり、以前に開いた文書は無傷
        assert_eq!(editor.current_document().text().to_string(), "!bbb");
        assert_eq!(editor.document(DocumentId(1)).text().to_string(), "aaa");
    }

    #[test]
    fn select_all_matches_and_edit_through_editor() {
        // Kakoune/Helix の `*` の流れを Editor 経由で
        let mut editor = Editor::new();
        editor.open(Document::from("foo bar foo"));
        let matches = find_matches(editor.current_document(), "foo", CaseSensitivity::Smart)
            .expect("一致がある");
        editor.set_selection(matches);
        editor.apply_edit(|doc, sel| insert_text(doc, sel, "X"));
        assert_eq!(editor.current_document().text().to_string(), "X bar X");
    }

    #[test]
    fn delete_backward_through_editor() {
        let mut editor = Editor::new();
        editor.open(Document::from("hello"));
        editor.set_selection(Selection::point(3));
        editor.apply_edit(delete_backward);
        assert_eq!(editor.current_document().text().to_string(), "helo");
        assert_eq!(editor.selection(), Selection::point(2));
    }

    #[test]
    fn mode_transitions() {
        let mut editor = Editor::new();
        assert_eq!(editor.mode(), Mode::Normal);
        editor.set_mode(Mode::Insert);
        assert_eq!(editor.mode(), Mode::Insert);
        editor.set_mode(Mode::Select);
        assert_eq!(editor.mode(), Mode::Select);
        editor.set_mode(Mode::Normal);
        assert_eq!(editor.mode(), Mode::Normal);
    }
}
