//! エディタ状態: 文書の集合・View の分割ツリー・モード・履歴。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use mina_text::{Document, Selection, Transaction};

use crate::Mode;
use crate::history::History;
use crate::tree::{SplitDirection, Tree, ViewId};

/// 文書を一意に識別する ID。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentId(pub usize);

/// 1つの表示領域（スプリット1つ分）を表す。
///
/// どの文書を表示しているか、その文書でのアクティブな選択、そして表示範囲の
/// 先頭行（viewport）を保持する。viewport の高さ（行数）はターミナルが描画時に
/// 渡すので、ここには「先頭行」だけを持つ。
///
/// 1つの文書を複数の View で表示できる。選択は文書ではなく View ごとに持つ
/// （`docs/adr/0002-functional-core-selection.md` 参照）。
#[derive(Clone, Debug)]
pub struct View {
    pub doc: DocumentId,
    pub selection: Selection,
    pub first_line: usize,
}

/// 保持する文書の上限（M4）。
///
/// Open の繰り返しで文書・履歴が無制限に蓄積しないよう、上限超過時は
/// 「どの View も表示しておらず・dirty でもない」最も古い文書を破棄する。
/// dirty な文書は破棄しない（未保存の編集を失わない）。
///
/// ponytail: 上限は実質1文書閲覧の v1 に対する安全弁。タブ UI 等で複数文書を
/// 並行保持する必要が出たら LRU/参照カウントに置き換える。
const MAX_DOCUMENTS: usize = 8;

/// エディタのグローバル状態。
///
/// 関数型コアの「現在の状態」を保持する imperative shell。文書の集合・View の
/// 分割ツリー・フォーカス・モード・履歴を持つ。コアの変換はトランザクション
/// として [`Editor::apply`] に渡し、フォーカス中の View の文書と選択を更新する。
#[derive(Clone, Debug)]
pub struct Editor {
    documents: BTreeMap<DocumentId, Document>,
    histories: BTreeMap<DocumentId, History>,
    /// 文書がファイルに紐づいている場合のパス（未保存のスクラッチ文書は含まれない）。
    paths: BTreeMap<DocumentId, PathBuf>,
    /// 保存済み状態から編集が進んでいる文書（保存でクリアされる）。
    dirty: BTreeSet<DocumentId>,
    next_document_id: usize,
    views: Vec<Option<View>>,
    next_view_id: usize,
    tree: Tree,
    mode: Mode,
}

impl Editor {
    /// 空のスクラッチ文書と1つの View を持つエディタを作る。
    pub fn new() -> Self {
        let scratch = DocumentId(0);
        let mut editor = Self {
            documents: BTreeMap::new(),
            histories: BTreeMap::new(),
            paths: BTreeMap::new(),
            dirty: BTreeSet::new(),
            next_document_id: 1,
            views: Vec::new(),
            next_view_id: 1,
            tree: Tree::new(ViewId(0)),
            mode: Mode::Normal,
        };
        editor.documents.insert(scratch, Document::new());
        editor.histories.insert(scratch, History::new());
        editor.views.push(Some(View {
            doc: scratch,
            selection: Selection::point(0),
            first_line: 0,
        }));
        editor
    }

    /// `doc` を開き、新しい文書 ID を返す。
    ///
    /// フォーカス中の View が開いた文書へ移動する。履歴は文書ごとに独立して
    /// 持つ。上限を超えた場合は不要な文書を破棄する（[`Self::evict_oldest_if_over_cap`]）。
    pub fn open(&mut self, doc: Document) -> DocumentId {
        let id = DocumentId(self.next_document_id);
        self.next_document_id += 1;
        self.documents.insert(id, doc);
        self.histories.insert(id, History::new());
        let view = self.view_mut();
        view.doc = id;
        view.selection = Selection::point(0);
        view.first_line = 0;
        self.evict_oldest_if_over_cap();
        id
    }

    /// 文書数が上限を超えたら、どの View も表示していない・dirty でもない
    /// 最も古い文書を上限に収まるまで破棄する（M4: Open の繰り返しによる
    /// メモリ蓄積の防止）。
    fn evict_oldest_if_over_cap(&mut self) {
        while self.documents.len() > MAX_DOCUMENTS {
            let viewed: BTreeSet<DocumentId> =
                self.views.iter().flatten().map(|v| v.doc).collect();
            let oldest = self
                .documents
                .keys()
                .copied()
                .find(|&id| !viewed.contains(&id) && !self.dirty.contains(&id));
            let Some(id) = oldest else { break }; // 残りは全て表示中 or dirty
            self.documents.remove(&id);
            self.histories.remove(&id);
            self.paths.remove(&id);
        }
    }

    /// ファイルから文書を開き、新しい文書 ID を返す。
    ///
    /// [`Editor::open`] と同じくフォーカス中の View が新しい文書へ移動する。
    /// パスを登録し、保存済み（clean）の状態で始まる。
    pub fn open_with_path(&mut self, path: PathBuf, text: &str) -> DocumentId {
        let id = self.open(Document::from(text));
        self.paths.insert(id, path);
        id
    }

    /// 既に開かれているパスに対応する文書へフォーカスを戻す（#7）。
    ///
    /// 該当する文書が無ければ `None` を返し、状態は一切変えない。文書のテキスト・
    /// dirty フラグ・undo/redo ヒストリーは保持される（ディスクからの再読込はしない）。
    /// 選択・先頭行は View の現在値をそのまま使う（再利用 = 「何も変えない」）。
    pub fn focus_open_path(&mut self, path: &Path) -> Option<DocumentId> {
        let id = self
            .paths
            .iter()
            .find(|(_, p)| p.as_path() == path)
            .map(|(id, _)| *id)?;
        self.view_mut().doc = id;
        Some(id)
    }

    /// フォーカス中の View が表示する文書のファイルパス（未保存なら None）。
    pub fn focused_path(&self) -> Option<&Path> {
        self.paths.get(&self.view().doc).map(|p| p.as_path())
    }

    /// フォーカス中の View が表示する文書の ID。
    pub fn focused_doc_id(&self) -> DocumentId {
        self.view().doc
    }

    /// ファイルに紐づいている全文書のパス（監視対象 — ADR-0015）。
    ///
    /// スクラッチ（未保存）文書は含まれない。
    pub fn open_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.paths.values()
    }

    /// 保持している全文書の ID（Syntax キャッシュの掃除などで使う）。
    pub fn document_ids(&self) -> impl Iterator<Item = DocumentId> + '_ {
        self.documents.keys().copied()
    }

    /// パスに対応する文書 ID（未オープンなら None）。
    pub fn doc_id_for_path(&self, path: &Path) -> Option<DocumentId> {
        self.paths
            .iter()
            .find(|(_, p)| p.as_path() == path)
            .map(|(id, _)| *id)
    }

    /// 指定文書のテキストをディスクの内容で置き換える（Reload — ADR-0015）。
    ///
    /// 全文置換の Transaction としてその文書の履歴に記録されるので undo で
    /// 外部変更前の状態に戻れる。置換後、テキストはディスクと一致するため
    /// dirty はクリアされる。文書を表示している View の選択は新しい長さへ
    /// クランプされる。テキストが既に一致していれば何もせず `false`。
    pub fn reload_doc(&mut self, doc_id: DocumentId, new_text: &str) -> bool {
        let old_text = self.documents[&doc_id].text().to_string();
        if old_text == new_text {
            return false;
        }
        let old_doc = self.documents[&doc_id].clone();
        let tx = Transaction::replace_all(&old_doc, new_text);
        let new_doc = tx.apply(&old_doc);
        let new_len = new_doc.len_chars();
        let clamp = |sel: &Selection| {
            Selection::new(
                sel.ranges()
                    .iter()
                    .map(|r| {
                        mina_text::Range::new(r.anchor().min(new_len), r.head().min(new_len))
                    })
                    .collect(),
                sel.primary_index(),
            )
        };
        // 文書を表示している View の選択を新しい長さへクランプする。
        // 表示していない文書は履歴記録用に点選択を使う。
        let selection_after = self
            .views
            .iter()
            .flatten()
            .find(|v| v.doc == doc_id)
            .map(|v| clamp(&v.selection))
            .unwrap_or_else(|| Selection::point(0));
        self.histories
            .get_mut(&doc_id)
            .expect("文書の履歴は存在する")
            .push(tx, selection_after.clone(), selection_after.clone());
        self.documents.insert(doc_id, new_doc);
        for view in self.views.iter_mut().flatten() {
            if view.doc == doc_id {
                view.selection = clamp(&view.selection);
            }
        }
        // テキストがディスクと一致するので dirty は解消する（ADR-0015）
        self.dirty.remove(&doc_id);
        true
    }

    /// フォーカス中の文書を閉じる（Close — ADR-0015）。
    ///
    /// 文書・履歴・パス・dirty を破棄し、フォーカスを残りの開いている文書
    /// （パス順で最初）へ移す。残りが無ければスクラッチ（空画面）へ戻す。
    /// ファイルに紐づいていない文書（スクラッチ）は閉じず `false`。
    pub fn close_focused_document(&mut self) -> bool {
        let doc_id = self.view().doc;
        if !self.paths.contains_key(&doc_id) {
            return false;
        }
        self.documents.remove(&doc_id);
        self.histories.remove(&doc_id);
        self.paths.remove(&doc_id);
        self.dirty.remove(&doc_id);
        // この文書を表示していた View を次へ移す
        let next = self.paths.iter().next().map(|(id, _)| *id);
        // 残りが無ければ空状態用のスクラッチを用意する（既存スクラッチ id 0 が
        // 残っていればそれを使う）
        let scratch = if next.is_none() && !self.documents.contains_key(&DocumentId(0)) {
            Some(self.open(Document::new()))
        } else {
            None
        };
        for view in self.views.iter_mut().flatten() {
            if view.doc == doc_id {
                match next {
                    Some(id) => {
                        view.doc = id;
                        view.selection = Selection::point(0);
                        view.first_line = 0;
                    }
                    None => {
                        // 空状態: スクラッチへ戻る
                        view.doc = scratch.unwrap_or(DocumentId(0));
                        view.selection = Selection::point(0);
                        view.first_line = 0;
                    }
                }
            }
        }
        true
    }

    /// 全 View のカーソルとビューポートを初期位置（文書先頭・1行目）へ戻す。
    /// ADR-0027: 最後の Interactive クライアント切断時の後始末。
    pub fn reset_views_to_start(&mut self) {
        for view in self.views.iter_mut().flatten() {
            view.selection = Selection::point(0);
            view.first_line = 0;
        }
    }

    /// フォーカス中の View が表示する文書が保存済み状態から編集されているか。
    ///
    /// ponytail: undo で保存時点まで戻っても dirty は残る（履歴に保存時点の
    /// マーカーを持たない）。正確な追跡が必要になったら履歴にマーカーを入れる。
    pub fn is_dirty(&self) -> bool {
        self.dirty.contains(&self.view().doc)
    }

    /// 指定した文書への保存完了を記録する。
    ///
    /// 保存対象は「保存開始時にフォーカスしていた文書」であり、保存完了時点の
    /// フォーカスではない（H3: 書き込み中に別接続が Open するとフォーカスが
    /// 変わり得る。dirty は View ではなく Document 単位）。
    ///
    /// 書き込み対象のテキストが現在のテキストと一致する場合のみ dirty をクリア
    /// して `true` を返す。一致しない場合（HIGH-1: 書き込み中に他接続が編集）
    /// は dirty を残して `false` を返す — 古いテキストを保存しても未保存の
    /// 編集が残っているので、「保存済み」と誤認させて編集を失わせない。文書が
    /// 既に破棄されている場合（破棄は clean な文書のみ）はクリア不要なので
    /// `true`。
    pub fn mark_saved_doc(&mut self, doc_id: DocumentId, written_text: &str) -> bool {
        match self.documents.get(&doc_id) {
            Some(doc) if doc.text().to_string() == written_text => {
                self.dirty.remove(&doc_id);
                true
            }
            Some(_) => false,
            None => true,
        }
    }

    /// 文書を ID で取得する。
    ///
    /// # Panics
    ///
    /// 存在しない ID の場合に panic する。
    pub fn document(&self, id: DocumentId) -> &Document {
        &self.documents[&id]
    }

    /// フォーカス中の View が表示している文書。
    pub fn current_document(&self) -> &Document {
        let doc_id = self.view().doc;
        self.document(doc_id)
    }

    /// フォーカス中の View。
    pub fn view(&self) -> &View {
        self.views[self.tree.focused().0]
            .as_ref()
            .expect("フォーカス中の View が存在しない")
    }

    /// View を ID で取得する。
    ///
    /// # Panics
    ///
    /// 存在しない ID の場合に panic する。
    pub fn view_by_id(&self, id: ViewId) -> &View {
        self.views[id.0].as_ref().expect("View が存在しない")
    }

    /// フォーカス中の View の ID。
    pub fn focused_view_id(&self) -> ViewId {
        self.tree.focused()
    }

    /// ツリー上の View の数。
    pub fn view_count(&self) -> usize {
        self.tree.views_in_order().len()
    }

    /// 現在の選択（フォーカス中の View）。
    pub fn selection(&self) -> Selection {
        self.view().selection.clone()
    }

    /// 現在の選択を置き換える（フォーカス中の View）。
    pub fn set_selection(&mut self, selection: Selection) {
        self.view_mut().selection = selection;
    }

    /// 現在のモード。
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// モードを切り替える。
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// フォーカス中の View を `direction` に分割する。
    ///
    /// 新しい View は現在の View の文書・選択・先頭行を引き継ぎ（選択は
    /// 独立した値としてコピー）、フォーカスは新 View へ移る。新しい View の
    /// ID を返す。
    pub fn split(&mut self, direction: SplitDirection) -> ViewId {
        let focused = self.tree.focused();
        let new_view = {
            let view = self.view();
            View {
                doc: view.doc,
                selection: view.selection.clone(),
                first_line: view.first_line,
            }
        };
        let id = ViewId(self.next_view_id);
        self.next_view_id += 1;
        self.views.push(Some(new_view));
        self.tree.split(focused, direction, id);
        id
    }

    /// フォーカスを次の View へ（表示順に循環）。
    pub fn focus_next(&mut self) {
        self.tree.focus_next();
    }

    /// フォーカスを前の View へ（表示順に循環）。
    pub fn focus_prev(&mut self) {
        self.tree.focus_prev();
    }

    /// フォーカス中の View を閉じる。最後の1つの場合は何もしない。
    ///
    /// 閉じた View のあった位置に近い View（表示順で次）へフォーカスを移す。
    pub fn close_view(&mut self) {
        let focused = self.tree.focused();
        let order = self.tree.views_in_order();
        if order.len() <= 1 {
            return;
        }
        let idx = order
            .iter()
            .position(|v| *v == focused)
            .expect("フォーカス中の View がツリーにある");
        if self.tree.remove(focused) {
            self.views[focused.0] = None;
        }
        let new_order = self.tree.views_in_order();
        self.tree.set_focused(new_order[idx % new_order.len()]);
    }

    /// トランザクションをフォーカス中の View の文書に適用し、履歴に記録する。
    ///
    /// `selection_after` は適用後の選択（挿入なら挿入テキストの後ろ、削除なら
    /// 削除開始点）。redo 時にここへ戻る。例:
    ///
    /// ```
    /// use mina_text::{Selection, Transaction};
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
        // ADR-0012: 文書を変えないトランザクション（空範囲の削除・空文字挿入）は
        // 適用も undo 履歴への記録も行わない。適用すると空グループが履歴に積まれ、
        // undo 1回で何も起きない上、呼び出し側が誤ってイベント/世代を進める
        // 原因になる。no-op の写像は恒等なので選択を変える必要もない。
        if transaction.is_noop() {
            return;
        }
        let doc_id = self.view().doc;
        let old_doc = self.documents[&doc_id].clone();
        let selection_before = self.view().selection.clone();
        let new_doc = transaction.apply(&old_doc);
        self.history_mut()
            .push(transaction, selection_before, selection_after.clone());
        self.documents.insert(doc_id, new_doc);
        self.view_mut().selection = selection_after;
        self.dirty.insert(doc_id);
    }

    /// 指定文書にトランザクションを適用し、その文書の履歴に記録する。
    ///
    /// [`Editor::apply`] のフォーカス非依存版（意味リネーム等のマルチ文書適用 —
    /// ADR-0029）。undo は文書ごと独立（Q5: リネーム全体は undo 対象外）だが、
    /// 履歴に記録しないと undo が「変更前のテキスト前提」で再適用されて壊れる
    /// ため、必ず履歴の invariant（「履歴 = 現在テキストへの逆適用列」）を保つ。
    /// 文書を表示している全 View の選択も新文書長へクランプする（CRITICAL C1 と
    /// 同趣旨: 文書が選択位置より短くなった後の範囲外アクセスによる panic 防止）。
    pub fn apply_document(
        &mut self,
        doc_id: DocumentId,
        transaction: Transaction,
        selection_after: Selection,
    ) {
        if transaction.is_noop() {
            return;
        }
        let selection_before = self
            .views
            .iter()
            .flatten()
            .find(|v| v.doc == doc_id)
            .map(|v| v.selection.clone())
            .unwrap_or_else(|| Selection::point(0));
        let new_doc = transaction.apply(&self.documents[&doc_id]);
        self.histories
            .get_mut(&doc_id)
            .expect("文書の履歴が存在しない")
            .push(transaction, selection_before, selection_after);
        self.documents.insert(doc_id, new_doc.clone());
        let new_len = new_doc.len_chars();
        for view in self.views.iter_mut().flatten() {
            if view.doc == doc_id {
                let clamp = |pos: usize| pos.min(new_len);
                view.selection = Selection::new(
                    view.selection
                        .ranges()
                        .iter()
                        .map(|r| mina_text::Range::new(clamp(r.anchor()), clamp(r.head())))
                        .collect(),
                    view.selection.primary_index(),
                );
            }
        }
        self.dirty.insert(doc_id);
    }

    /// フォーカス中の View の文書について undo できる変更があるか。
    pub fn can_undo(&self) -> bool {
        self.history().can_undo()
    }

    /// フォーカス中の View の文書について redo できる変更があるか。
    pub fn can_redo(&self) -> bool {
        self.history().can_redo()
    }

    /// フォーカス中の View の文書の直近の変更グループを元に戻す。
    pub fn undo(&mut self) {
        let doc_id = self.view().doc;
        let old_doc = self.documents[&doc_id].clone();
        if let Some((new_doc, selection)) = self.history_mut().undo(&old_doc) {
            self.documents.insert(doc_id, new_doc);
            self.view_mut().selection = selection;
            // undo は文書を変えるので dirty を立てる。履歴に保存時点のマーカー
            // を持たないため、保存時点まで戻った場合も保守的に dirty を残す
            // （is_dirty の ponytail コメントと整合）。
            self.dirty.insert(doc_id);
        }
    }

    /// 直近に undo された変更グループをやり直す。
    pub fn redo(&mut self) {
        let doc_id = self.view().doc;
        let old_doc = self.documents[&doc_id].clone();
        if let Some((new_doc, selection)) = self.history_mut().redo(&old_doc) {
            self.documents.insert(doc_id, new_doc);
            self.view_mut().selection = selection;
            // redo も undo と同じく文書を変えるので dirty を立てる。
            self.dirty.insert(doc_id);
        }
    }

    /// 以降の [`Editor::apply`] を1つの undo 単位にまとめる。
    /// 例: Insert モードに入ったとき呼び、抜けるときに終了する。
    pub fn begin_group(&mut self) {
        self.history_mut().begin_group();
    }

    /// グループ化を終了する。
    ///
    /// グループは文書ごとの履歴に属するが、開くのはフォーカス中の文書に対する
    /// [`Editor::begin_group`] だけ。フォーカスが別文書へ移った後（Open・View
    /// 切替等）も、開いたグループは閉じる（ADR-0007）: フォーカス中の文書だけを
    /// 閉じると、元の文書のグループが孤児化して開きっぱなしになり、そこへの
    /// 後続の書き込みが別クライアントの編集と同一 undo グループに混入する（M3）。
    ///
    /// 同時に開けるグループは高々1つ（モード・所有者は daemon グローバル）なので
    /// 全履歴を走査して閉じるのは安全。開いていない履歴への end_group は no-op
    /// なので、対象が無くても影響はない。
    pub fn end_group(&mut self) {
        for history in self.histories.values_mut() {
            history.end_group();
        }
    }

    /// 表示範囲の先頭行（viewport の上端）。
    pub fn first_line(&self) -> usize {
        self.view().first_line
    }

    /// カーソルが見えるように viewport をスクロールする。
    ///
    /// カーソルが viewport の上に出ていれば上端をカーソル行に合わせ、
    /// 下に出ていれば下端（`first_line + height - 1`）がカーソル行になるように
    /// 合わせる。`height` はターミナルの表示行数。カーソル移動・編集の後に
    /// 呼ぶこと（自動ではスクロールしない）。カーソル位置は primary の head。
    pub fn scroll_to_cursor(&mut self, height: usize) {
        // CRITICAL C1: 文書がカーソル位置より短くなる編集（DocumentEdit の
        // 短縮等）で選択が範囲外に残ると、ropey の char_to_line が範囲外
        // char index で panic する。保存されている選択は変更せず、計算用に
        // だけ文書末尾へクランプする。
        let doc = self.current_document();
        let head = self
            .view()
            .selection
            .primary()
            .head()
            .min(doc.len_chars());
        let cursor_line = doc.text().char_to_line(head);
        let first = self.view().first_line;
        if cursor_line < first {
            self.view_mut().first_line = cursor_line;
        } else if height > 0 && cursor_line >= first + height {
            self.view_mut().first_line = cursor_line - height + 1;
        }
    }

    /// 表示範囲を `amount` 行分スクロールする（正で下、負で上）。
    ///
    /// 先頭行は文書の範囲内 `[0, 最終行]` にクランプされる。
    pub fn scroll_lines(&mut self, amount: isize) {
        let max = self.current_document().text().len_lines().saturating_sub(1);
        let first = self.view().first_line as isize + amount;
        self.view_mut().first_line = first.clamp(0, max as isize) as usize;
    }

    /// 表示範囲を `pages` ページ分スクロールする（1ページ = `height` 行）。
    pub fn scroll_pages(&mut self, pages: isize, height: usize) {
        self.scroll_lines(pages.saturating_mul(height as isize));
    }

    fn view_mut(&mut self) -> &mut View {
        self.views[self.tree.focused().0]
            .as_mut()
            .expect("フォーカス中の View が存在しない")
    }

    fn history(&self) -> &History {
        &self.histories[&self.view().doc]
    }

    fn history_mut(&mut self) -> &mut History {
        let doc_id = self.view().doc;
        self.histories
            .get_mut(&doc_id)
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
    use mina_text::{CaseSensitivity, Range, find_matches};

    #[test]
    fn new_editor_has_scratch_document_and_normal_mode() {
        let editor = Editor::new();
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.view().doc, DocumentId(0));
        assert_eq!(editor.view_count(), 1);
        assert!(editor.current_document().is_empty());
        assert_eq!(editor.selection(), Selection::point(0));
        assert!(!editor.can_undo());
        assert!(!editor.can_redo());
    }

    #[test]
    fn open_with_path_tracks_path_and_dirty() {
        let mut editor = Editor::new();
        let id = editor.open_with_path(PathBuf::from("/tmp/foo.txt"), "hello");
        assert_eq!(
            editor.focused_path(),
            Some(Path::new("/tmp/foo.txt")),
            "パスが登録される"
        );
        assert!(!editor.is_dirty(), "開いた直後は clean");

        // 編集で dirty になる
        let selection = Selection::point(0);
        let tx = Transaction::insert(editor.current_document(), &selection, "X");
        let selection_after = tx.map_selection(&selection, true);
        editor.apply(tx, selection_after);
        assert!(editor.is_dirty());

        // 保存でクリアされる（書き込んだテキスト == 現在のテキスト）
        let text = editor.document(id).text().to_string();
        assert!(editor.mark_saved_doc(id, &text));
        assert!(!editor.is_dirty());
    }

    #[test]
    fn focus_open_path_reuses_document_keeping_state_and_history() {
        // #7: 開き済みパスの再 Open は既存ドキュメントを再利用し、
        // テキスト・dirty・undo ヒストリーを保持して重複生成しない。
        let mut editor = Editor::new();
        let a = editor.open_with_path(PathBuf::from("/tmp/a.txt"), "hello");
        // 編集して dirty + undo 可能にする
        let selection = editor.selection();
        let tx = Transaction::insert(editor.current_document(), &selection, "X");
        let selection_after = tx.map_selection(&selection, true);
        editor.apply(tx, selection_after);
        assert!(editor.is_dirty());

        // 別文書を開いてから同じパスへ戻す
        let b = editor.open_with_path(PathBuf::from("/tmp/b.txt"), "world");
        assert_ne!(a, b);
        let id = editor
            .focus_open_path(Path::new("/tmp/a.txt"))
            .expect("開き済みパスは再利用される");
        assert_eq!(id, a, "同じ DocumentId が返る（重複生成しない）");
        assert_eq!(editor.focused_path(), Some(Path::new("/tmp/a.txt")));
        assert_eq!(editor.current_document().text(), "Xhello", "テキスト保持");
        assert!(editor.is_dirty(), "dirty 保持");
        assert!(editor.can_undo(), "undo ヒストリー保持");

        // 未登録パスは None で状態不変
        assert!(editor.focus_open_path(Path::new("/tmp/none.txt")).is_none());
        assert_eq!(editor.focused_path(), Some(Path::new("/tmp/a.txt")));
    }

    #[test]
    fn mark_saved_doc_marks_only_the_named_document() {
        // H3: mark_saved_doc は指定した文書の dirty だけを消す。フォーカスが
        // 別の文書に移っていても（Save 中の Open 割り込み）、その文書には触れない。
        let mut editor = Editor::new();
        let id_a = editor.open_with_path(PathBuf::from("/tmp/a.txt"), "hello");
        let selection = Selection::point(0);
        let tx = Transaction::insert(editor.current_document(), &selection, "X");
        let selection_after = tx.map_selection(&selection, true);
        editor.apply(tx, selection_after);
        assert!(editor.is_dirty(), "A は dirty");

        // 保存の書き込み中に別文書 B を開いて編集した状況
        let id_b = editor.open_with_path(PathBuf::from("/tmp/b.txt"), "world");
        let tx_b = Transaction::insert(editor.current_document(), &Selection::point(0), "Y");
        let selection_after_b = tx_b.map_selection(&Selection::point(0), true);
        editor.apply(tx_b, selection_after_b);
        assert!(editor.is_dirty(), "B も dirty（フォーカスは B）");

        // A の保存完了を記録しても、B の dirty は残る
        let text_a = editor.document(id_a).text().to_string();
        assert!(editor.mark_saved_doc(id_a, &text_a));
        assert!(editor.is_dirty(), "B の dirty は消えない");

        // 指定した文書（B）に対してはフォーカスに関係なくクリアできる
        let text_b = editor.document(id_b).text().to_string();
        assert!(editor.mark_saved_doc(id_b, &text_b));
        assert!(!editor.is_dirty());
    }

    #[test]
    fn mark_saved_doc_keeps_dirty_if_edited_during_save() {
        // HIGH-1: Save はテキストを捕捉→ロック外で書き込み→完了後に dirty を
        // 消す。書き込み中に他接続が編集すると、保存したのは古いテキストなの
        // で dirty を消してはならない（消すと「保存済み」誤認で編集を失う）。
        let mut editor = Editor::new();
        let id = editor.open_with_path(PathBuf::from("/tmp/a.txt"), "hello");
        // 保存対象として捕捉したテキスト（書き込み開始時点）
        let written = editor.document(id).text().to_string();

        // 書き込み中に他接続が編集（dirty になる）
        let tx = Transaction::insert(editor.current_document(), &Selection::point(0), "X");
        let selection_after = tx.map_selection(&Selection::point(0), true);
        editor.apply(tx, selection_after);
        assert!(editor.is_dirty(), "書き込み中の編集で dirty");

        // 書き込み完了 → 古いテキストではクリアされない
        assert!(!editor.mark_saved_doc(id, &written));
        assert!(editor.is_dirty(), "未保存の編集が保存済み扱いにならない");

        // 書き込みと同時点のテキストで保存した場合はクリアされる
        let current = editor.document(id).text().to_string();
        assert!(editor.mark_saved_doc(id, &current));
        assert!(!editor.is_dirty());
    }

    #[test]
    fn open_switches_focused_view_to_new_document() {
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
    fn end_group_closes_group_on_non_focused_document() {
        // M3: グループは文書ごとの履歴に開くが、フォーカスが別文書へ移った後も
        // end_group は開いたグループを閉じる。フォーカス文書だけ閉じると元の
        // 文書のグループが孤児化し、そこへの後続の書き込みが同一 undo グループに
        // 混入する（ADR-0007 違反）。
        let mut editor = Editor::new();
        let x = editor.open_with_path(PathBuf::from("/tmp/x.txt"), "");
        editor.begin_group();
        let sel = Selection::point(0);
        let tx = Transaction::insert(editor.current_document(), &sel, "a");
        editor.apply(tx.clone(), tx.map_selection(&sel, true));

        // 別文書 Y へフォーカスが移る → X のグループは開いたまま
        let y = editor.open_with_path(PathBuf::from("/tmp/y.txt"), "");
        assert_ne!(x, y);

        // フォーカスが Y にある状態での end_group が X のグループも閉じる
        editor.end_group();

        // X に戻って書き込み → 別グループ（undo 1回で追記分のみ戻る）
        editor.focus_open_path(Path::new("/tmp/x.txt"));
        let sel2 = Selection::point(1);
        editor.set_selection(sel2.clone());
        let tx2 = Transaction::insert(editor.current_document(), &sel2, "Z");
        editor.apply(tx2.clone(), tx2.map_selection(&sel2, true));
        assert_eq!(editor.current_document().text().to_string(), "aZ");
        editor.undo();
        assert_eq!(
            editor.current_document().text().to_string(),
            "a",
            "undo は閉じたグループへの追記分だけを戻す"
        );
        editor.undo();
        assert!(
            editor.current_document().is_empty(),
            "2回目の undo で元のセッションも別グループとして戻る"
        );
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

    /// 10行の文書（行 i は char 位置 3i から始まる）。
    fn ten_line_document() -> Document {
        Document::from("l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9")
    }

    #[test]
    fn scroll_to_cursor_keeps_cursor_visible() {
        let mut editor = Editor::new();
        editor.open(ten_line_document());

        // カーソルが viewport 内なら動かない
        editor.set_selection(Selection::point(6)); // 2行目
        editor.scroll_to_cursor(5);
        assert_eq!(editor.first_line(), 0);

        // カーソルが viewport の下に出ている → 下端がカーソル行になる
        editor.set_selection(Selection::point(21)); // 7行目
        editor.scroll_to_cursor(5);
        assert_eq!(editor.first_line(), 3);

        // カーソルが viewport の上に出ている → 上端がカーソル行になる
        editor.set_selection(Selection::point(21)); // 7行目
        editor.scroll_to_cursor(5); // 下端基準で first_line = 3
        editor.set_selection(Selection::point(3)); // 1行目
        editor.scroll_to_cursor(5);
        assert_eq!(editor.first_line(), 1);

        // 最終行でスクロールするとカーソルが下端に来る
        editor.set_selection(Selection::point(27)); // 9行目
        editor.scroll_to_cursor(5);
        assert_eq!(editor.first_line(), 5);
    }

    #[test]
    fn scroll_to_cursor_does_not_panic_on_out_of_range_selection() {
        // CRITICAL C1: 文書がカーソル位置より短くなる編集（DocumentEdit の
        // 短縮等）で選択が範囲外に残っても、scroll_to_cursor は計算用に
        // 文書末尾へクランプして panic しない（保存された選択は変更しない）。
        let mut editor = Editor::new();
        editor.open(Document::from("hi"));
        // 11 は 2文字の文書では範囲外
        editor.set_selection(Selection::point(11));
        editor.scroll_to_cursor(5);
        assert_eq!(editor.first_line(), 0);
        assert_eq!(editor.selection().primary().head(), 11, "保存選択は変更しない");

        // 空文書で範囲外でも panic しない
        let mut editor = Editor::new();
        editor.set_selection(Selection::point(3));
        editor.scroll_to_cursor(5);
        assert_eq!(editor.first_line(), 0);
    }

    #[test]
    fn scroll_lines_clamps_to_document() {
        let mut editor = Editor::new();
        editor.open(ten_line_document());

        editor.scroll_lines(3);
        assert_eq!(editor.first_line(), 3);
        editor.scroll_lines(-5);
        assert_eq!(editor.first_line(), 0); // 上端でクランプ
        editor.scroll_lines(100);
        assert_eq!(editor.first_line(), 9); // 最終行でクランプ
    }

    #[test]
    fn scroll_pages_moves_by_height() {
        let mut editor = Editor::new();
        editor.open(ten_line_document());

        editor.scroll_pages(1, 5);
        assert_eq!(editor.first_line(), 5);
        editor.scroll_pages(1, 5);
        assert_eq!(editor.first_line(), 9); // クランプ
        editor.scroll_pages(-2, 5);
        assert_eq!(editor.first_line(), 0);
    }

    #[test]
    fn scroll_on_empty_document_is_noop() {
        let mut editor = Editor::new();
        editor.scroll_lines(5);
        assert_eq!(editor.first_line(), 0);
        editor.scroll_to_cursor(5);
        assert_eq!(editor.first_line(), 0);
    }

    #[test]
    fn cursor_movement_followed_by_scroll_to_cursor() {
        use mina_text::{Direction, Movement, move_selection};

        // 移動のたびに scroll_to_cursor を呼ぶのがターミナル層の役割
        let mut editor = Editor::new();
        editor.open(ten_line_document());
        for _ in 0..9 {
            let doc = editor.current_document();
            let selection = editor.selection();
            let moved = move_selection(doc, &selection, Movement::Line, Direction::Forward);
            editor.set_selection(moved);
            editor.scroll_to_cursor(3);
        }
        // 9回下がって最終行。高さ3なので first_line は 7
        assert_eq!(editor.first_line(), 7);
        assert_eq!(editor.selection(), Selection::point(27));
    }

    // --- 分割表示 ---

    #[test]
    fn split_creates_second_view_sharing_document() {
        let mut editor = Editor::new();
        let doc_id = editor.open(Document::from("hello"));
        let first = editor.focused_view_id();
        let second = editor.split(SplitDirection::Vertical);

        assert_eq!(editor.view_count(), 2);
        assert_eq!(editor.focused_view_id(), second);
        // 同じ文書・同じ選択を引き継ぐ
        assert_eq!(editor.view_by_id(first).doc, doc_id);
        assert_eq!(editor.view_by_id(second).doc, doc_id);
        assert_eq!(
            editor.view_by_id(first).selection,
            editor.view_by_id(second).selection
        );
    }

    #[test]
    fn split_same_direction_stays_flat_and_focus_cycles() {
        let mut editor = Editor::new();
        editor.open(Document::from("hello"));
        let first = editor.focused_view_id();
        let second = editor.split(SplitDirection::Vertical);
        let third = editor.split(SplitDirection::Vertical);
        assert_eq!(editor.view_count(), 3);

        // フォーカスは新 View → 前へ戻る → 循環
        assert_eq!(editor.focused_view_id(), third);
        editor.focus_prev();
        assert_eq!(editor.focused_view_id(), second);
        editor.focus_prev();
        assert_eq!(editor.focused_view_id(), first);
        editor.focus_prev();
        assert_eq!(editor.focused_view_id(), third); // 先頭から末尾へ循環
        editor.focus_next();
        assert_eq!(editor.focused_view_id(), first); // 末尾から先頭へ循環
    }

    #[test]
    fn views_keep_independent_selections() {
        // ADR-0002 の本領: 同じ文書を表示していても選択は View ごとに独立
        let mut editor = Editor::new();
        editor.open(Document::from("hello"));
        let first = editor.focused_view_id();
        let second = editor.split(SplitDirection::Vertical);

        editor.set_selection(Selection::point(2)); // 2番目の View で移動
        assert_eq!(editor.view_by_id(second).selection, Selection::point(2));

        editor.focus_prev();
        assert_eq!(editor.focused_view_id(), first);
        assert_eq!(editor.selection(), Selection::point(0)); // 1番目は無傷
    }

    #[test]
    fn edit_in_one_view_affects_shared_document_only_via_focused_view() {
        let mut editor = Editor::new();
        editor.open(Document::from("hello"));
        let first = editor.focused_view_id();
        let second = editor.split(SplitDirection::Vertical);

        // 2番目の View で挿入
        editor.set_selection(Selection::point(2));
        let selection = editor.selection();
        let tx = Transaction::insert(editor.current_document(), &selection, "X");
        let selection_after = tx.map_selection(&selection, true);
        editor.apply(tx, selection_after);
        assert_eq!(editor.current_document().text().to_string(), "heXllo");
        assert_eq!(editor.view_by_id(second).selection, Selection::point(3));

        // 1番目の View の選択は無傷（文書は共有なのでテキストは変わる）
        editor.focus_prev();
        assert_eq!(editor.view_by_id(first).selection, Selection::point(0));
        assert_eq!(editor.current_document().text().to_string(), "heXllo");
    }

    #[test]
    fn close_view_collapses_and_moves_focus() {
        let mut editor = Editor::new();
        editor.open(Document::from("hello"));
        let first = editor.focused_view_id();
        let _second = editor.split(SplitDirection::Vertical);
        let third = editor.split(SplitDirection::Vertical);
        assert_eq!(editor.view_count(), 3);

        // 3番目を閉じる → 2つになり、フォーカスは次の位置へ
        editor.close_view();
        assert_eq!(editor.view_count(), 2);
        assert!(editor.focused_view_id() != third);
        assert_eq!(editor.focused_view_id(), first); // 表示順の次（循環）

        // 2つを閉じて1つに
        editor.close_view();
        assert_eq!(editor.view_count(), 1);
        editor.close_view(); // 最後の1つは閉じない
        assert_eq!(editor.view_count(), 1);

        // 折りたたまれて View 1つになっても、再分割できる
        let again = editor.split(SplitDirection::Horizontal);
        assert_eq!(editor.view_count(), 2);
        assert_eq!(editor.focused_view_id(), again);
        assert_ne!(editor.focused_view_id(), first);
    }

    #[test]
    fn open_evicts_oldest_docs_but_keeps_focused() {
        // M4: 上限を超えて Open しても、フォーカス中の文書は残り、
        // 文書数は上限に収まる
        let mut editor = Editor::new();
        for i in 0..(MAX_DOCUMENTS + 4) {
            editor.open_with_path(PathBuf::from(format!("/tmp/f{i}")), "x");
        }
        assert!(
            editor.documents.len() <= MAX_DOCUMENTS,
            "上限を超えない: {}",
            editor.documents.len()
        );
        assert!(
            editor.documents.contains_key(&editor.focused_doc_id()),
            "フォーカス中の文書は残る"
        );
        // 最も古い文書（scratch を除く最初の Open）は破棄されている
        assert!(!editor.documents.contains_key(&DocumentId(1)));
    }

    #[test]
    fn open_does_not_evict_dirty_docs() {
        // M4: dirty な文書は破棄されない（未保存の編集を失わない）
        let mut editor = Editor::new();
        let a = editor.open_with_path(PathBuf::from("/tmp/a.txt"), "x");
        editor.dirty.insert(a);
        for i in 0..(MAX_DOCUMENTS + 2) {
            editor.open_with_path(PathBuf::from(format!("/tmp/f{i}")), "x");
        }
        assert!(
            editor.documents.contains_key(&a),
            "dirty な文書は破棄されない"
        );
    }
}
