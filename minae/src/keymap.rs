//! キーマップ: モード別の prefix トライ。キー列 → [`Command`] を解決する。
//!
//! 子ノードは `Vec<(KeyEvent, Node)>` の線形探索（バインディング数が少ないので
//! ハッシュより速い）。
//! ponytail: バインディングが数十を超えたら `HashMap` 化を検討する（要 Hash key）。

use minae_protocol::{Command, Direction, GotoTarget, Mode, Movement};
use termina::event::{KeyCode, KeyEvent, Modifiers};

#[derive(Default)]
struct Node {
    command: Option<Command>,
    children: Vec<(KeyEvent, Node)>,
}

impl Node {
    fn insert(&mut self, keys: &[KeyEvent], command: Command) {
        match keys.split_first() {
            Some((first, rest)) => {
                let node = match self.children.iter_mut().find(|(k, _)| k == first) {
                    Some((_, node)) => node,
                    None => {
                        self.children.push((*first, Node::default()));
                        &mut self.children.last_mut().expect("push 直後").1
                    }
                };
                node.insert(rest, command);
            }
            None => self.command = Some(command),
        }
    }

    fn get(&self, key: &KeyEvent) -> Option<&Node> {
        self.children
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, node)| node)
    }
}

/// キー解決の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// コマンドが確定した。
    Command(Command),
    /// prefix の途中（次のキーを待つ）。
    Pending,
    /// どのバインディングにも一致しない（pending はリセットされる）。
    NoMatch,
}

/// モード別のキーマップ。
pub struct Keymaps {
    normal: Node,
    insert: Node,
    select: Node,
}

/// Shift 付きのアルファベットキーから SHIFT を落とし、大文字に統一する。
///
/// termina は大文字バイトを `Char('G')+SHIFT` として報告する（ターミナルによっては
/// `Char('g')+SHIFT` のこともある）。バインディングは「大文字 = 修飾子なし」に
/// 統一するため、どちらも `Char('G')`（修飾子なし）へ正規化する。
fn normalize(key: KeyEvent) -> KeyEvent {
    match key.code {
        KeyCode::Char(c)
            if c.is_ascii_alphabetic() && key.modifiers.contains(Modifiers::SHIFT) =>
        {
            KeyEvent::new(
                KeyCode::Char(c.to_ascii_uppercase()),
                key.modifiers & !Modifiers::SHIFT,
            )
        }
        _ => key,
    }
}

fn key(code: KeyCode, modifiers: Modifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn plain(code: KeyCode) -> KeyEvent {
    key(code, Modifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    key(KeyCode::Char(c), Modifiers::CONTROL)
}

fn alt(code: KeyCode) -> KeyEvent {
    key(code, Modifiers::ALT)
}

impl Keymaps {
    /// Normal/Select 共通の移動バインディング（キー列 → コマンド）。
    /// Normal では Move、Select では Extend になる（Goto/Scroll は両モード共通）。
    fn movement_bindings(extend: bool) -> Vec<(Vec<KeyEvent>, Command)> {
        let move_or_extend = |m: Movement, d: Direction| {
            if extend {
                Command::Extend {
                    movement: m,
                    direction: d,
                }
            } else {
                Command::Move {
                    movement: m,
                    direction: d,
                }
            }
        };
        let mut out = vec![
            // 1文字・1行・単語（hjkl + 矢印。e は単語末尾）
            (
                vec![plain(KeyCode::Char('h'))],
                move_or_extend(Movement::Char, Direction::Backward),
            ),
            (
                vec![plain(KeyCode::Char('l'))],
                move_or_extend(Movement::Char, Direction::Forward),
            ),
            (
                vec![plain(KeyCode::Char('j'))],
                move_or_extend(Movement::Line, Direction::Forward),
            ),
            (
                vec![plain(KeyCode::Char('k'))],
                move_or_extend(Movement::Line, Direction::Backward),
            ),
            (
                vec![plain(KeyCode::Char('w'))],
                move_or_extend(Movement::Word, Direction::Forward),
            ),
            (
                vec![plain(KeyCode::Char('b'))],
                move_or_extend(Movement::Word, Direction::Backward),
            ),
            (
                vec![plain(KeyCode::Char('e'))],
                move_or_extend(Movement::WordEnd, Direction::Forward),
            ),
            (
                vec![plain(KeyCode::Left)],
                move_or_extend(Movement::Char, Direction::Backward),
            ),
            (
                vec![plain(KeyCode::Right)],
                move_or_extend(Movement::Char, Direction::Forward),
            ),
            (
                vec![plain(KeyCode::Up)],
                move_or_extend(Movement::Line, Direction::Backward),
            ),
            (
                vec![plain(KeyCode::Down)],
                move_or_extend(Movement::Line, Direction::Forward),
            ),
            // Home/End: 行頭/行末（方向は core 側で無視される）
            (
                vec![plain(KeyCode::Home)],
                move_or_extend(Movement::LineStart, Direction::Forward),
            ),
            (
                vec![plain(KeyCode::End)],
                move_or_extend(Movement::LineEnd, Direction::Forward),
            ),
        ];
        // 文書先頭/末尾（prefix g: g g = 先頭, g e = 末尾 = Helix の last_line、
        // G = 末尾 = vim 流の文末）。Select では daemon 側が Extend 扱いする。
        out.push((
            vec![plain(KeyCode::Char('g')), plain(KeyCode::Char('g'))],
            Command::Goto {
                target: GotoTarget::DocumentStart,
            },
        ));
        out.push((
            vec![plain(KeyCode::Char('g')), plain(KeyCode::Char('e'))],
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        ));
        out.push((
            vec![plain(KeyCode::Char('G'))],
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        ));
        // g リーダーの行内 goto（Helix の g h / g l / g s）: Normal は Move、
        // Select は Extend（Select の移動系は全て Extend になる方針）
        for (key, movement) in [
            (KeyCode::Char('h'), Movement::LineStart),
            (KeyCode::Char('l'), Movement::LineEnd),
            (KeyCode::Char('s'), Movement::FirstNonWhitespace),
        ] {
            out.push((
                vec![plain(KeyCode::Char('g')), plain(key)],
                move_or_extend(movement, Direction::Forward),
            ));
        }
        // ページスクロール（ページ単位、高さは daemon 側が把握）。C-f/C-b/
        // PgDn/PgUp は1ページ。C-d/C-u は半ページ（Helix と同じ — ScrollHalf）。
        for (key, pages) in [
            (ctrl('f'), 1),
            (ctrl('b'), -1),
            (plain(KeyCode::PageDown), 1),
            (plain(KeyCode::PageUp), -1),
        ] {
            out.push((vec![key], Command::Scroll { pages }));
        }
        out.push((
            vec![ctrl('d')],
            Command::ScrollHalf {
                direction: Direction::Forward,
            },
        ));
        out.push((
            vec![ctrl('u')],
            Command::ScrollHalf {
                direction: Direction::Backward,
            },
        ));
        // 検索ナビゲーション（Helix の n/N/*）。`/`/`?` はクライアント側で
        // プロンプトを開くためキーマップにはない。
        out.push((
            vec![plain(KeyCode::Char('n'))],
            Command::SearchNext {
                direction: Direction::Forward,
            },
        ));
        out.push((
            vec![plain(KeyCode::Char('N'))],
            Command::SearchNext {
                direction: Direction::Backward,
            },
        ));
        out.push((
            vec![plain(KeyCode::Char('*'))],
            Command::SearchSelection,
        ));
        out
    }

    /// 現在のバインディングセットを構築する（Helix default を基準に整理）。
    pub fn new() -> Self {
        let mut normal = Node::default();
        let mut select = Node::default();

        // 移動テーブルは Normal/Select で共有（Normal = Move、Select = Extend）
        for (keys, command) in Self::movement_bindings(false) {
            normal.insert(&keys, command);
        }
        for (keys, command) in Self::movement_bindings(true) {
            select.insert(&keys, command);
        }

        // Normal: モード遷移
        normal.insert(
            &[plain(KeyCode::Char('v'))],
            Command::SetMode { mode: Mode::Select },
        );
        normal.insert(
            &[plain(KeyCode::Char('i'))],
            Command::SetMode { mode: Mode::Insert },
        );
        // Normal: 編集。x は選択を1行下へ拡張（Helix の x）、X はカーソル行全体を
        // 選択（Helix の X = extend_to_line_bounds）。d は選択削除・c は削除+Insert。
        // Backspace は1文字削除（vim 流）。
        // 保存は `:` コマンドモードの :w（Helix/vim 流）。s には割り当てない。
        normal.insert(&[plain(KeyCode::Char('x'))], Command::ExtendLineBelow);
        normal.insert(&[plain(KeyCode::Char('X'))], Command::SelectLine);
        normal.insert(&[plain(KeyCode::Backspace)], Command::DeleteBackward);
        normal.insert(&[plain(KeyCode::Char('d'))], Command::DeleteRange);
        normal.insert(&[plain(KeyCode::Char('c'))], Command::Change);
        // a/o/O: append・行の下/上に空行を開いて Insert（Helix の a/o/O）。
        // r はクライアント側で単一文字の置換プロンプトを開く（キーマップにはない）。
        normal.insert(&[plain(KeyCode::Char('a'))], Command::Append);
        normal.insert(&[plain(KeyCode::Char('o'))], Command::OpenBelow);
        normal.insert(&[plain(KeyCode::Char('O'))], Command::OpenAbove);
        // %: 文書全体を選択（Helix の select_all）
        normal.insert(&[plain(KeyCode::Char('%'))], Command::SelectAll);
        // A/I: 行末/行頭（最初の非空白）へ移動して Insert（Helix の A/I。ADR-0023）
        normal.insert(&[plain(KeyCode::Char('A'))], Command::InsertAtLineEnd);
        normal.insert(&[plain(KeyCode::Char('I'))], Command::InsertAtLineStart);
        normal.insert(&[plain(KeyCode::Char('u'))], Command::Undo);
        normal.insert(&[plain(KeyCode::Char('U'))], Command::Redo);
        // Space リーダー + k: カーソル位置のシンボル定義をポップアップ表示する
        // （ジャンプしない簡易確認。Helix の space リーダーに合わせた配置）。
        normal.insert(
            &[plain(KeyCode::Char(' ')), plain(KeyCode::Char('k'))],
            Command::PeekDefinition,
        );

        // Select: モード解除 + 編集
        select.insert(
            &[plain(KeyCode::Char('v'))],
            Command::SetMode { mode: Mode::Normal },
        );
        select.insert(
            &[plain(KeyCode::Escape)],
            Command::SetMode { mode: Mode::Normal },
        );
        // Select の x/X は Normal と同じく拡張（Helix の select モードも同様）。
        // d/c は選択の削除・変更。Backspace は DeleteRange の速記（minae 独自）。
        select.insert(&[plain(KeyCode::Char('x'))], Command::ExtendLineBelow);
        select.insert(&[plain(KeyCode::Char('X'))], Command::SelectLine);
        select.insert(&[plain(KeyCode::Backspace)], Command::DeleteRange);
        select.insert(&[plain(KeyCode::Char('d'))], Command::DeleteRange);
        select.insert(&[plain(KeyCode::Char('c'))], Command::Change);
        select.insert(&[plain(KeyCode::Char('a'))], Command::Append);
        select.insert(&[plain(KeyCode::Char('o'))], Command::OpenBelow);
        select.insert(&[plain(KeyCode::Char('O'))], Command::OpenAbove);
        select.insert(&[plain(KeyCode::Char('%'))], Command::SelectAll);
        // A/I: Select でも折りたたんで行末/行頭で Insert（ADR-0023）
        select.insert(&[plain(KeyCode::Char('A'))], Command::InsertAtLineEnd);
        select.insert(&[plain(KeyCode::Char('I'))], Command::InsertAtLineStart);
        select.insert(&[plain(KeyCode::Char('u'))], Command::Undo);
        select.insert(&[plain(KeyCode::Char('U'))], Command::Redo);
        select.insert(
            &[plain(KeyCode::Char(' ')), plain(KeyCode::Char('k'))],
            Command::PeekDefinition,
        );

        // Insert: 文字入力はクライアント側のフォールバック。ここでは
        // Esc・Enter（改行）・Tab・削除（文字/単語）・左右/行頭/行末移動のみ
        let mut insert = Node::default();
        insert.insert(
            &[plain(KeyCode::Escape)],
            Command::SetMode { mode: Mode::Normal },
        );
        insert.insert(&[plain(KeyCode::Enter)], Command::Insert { text: "\n".into() });
        insert.insert(&[plain(KeyCode::Tab)], Command::Insert { text: "\t".into() });
        insert.insert(&[plain(KeyCode::Backspace)], Command::DeleteBackward);
        insert.insert(&[ctrl('h')], Command::DeleteBackward);
        insert.insert(&[plain(KeyCode::Delete)], Command::DeleteForward);
        insert.insert(&[ctrl('d')], Command::DeleteForward);
        insert.insert(&[ctrl('w')], Command::DeleteWordBackward);
        insert.insert(&[alt(KeyCode::Backspace)], Command::DeleteWordBackward);
        insert.insert(&[alt(KeyCode::Char('d'))], Command::DeleteWordForward);
        insert.insert(&[plain(KeyCode::Left)], Command::Move {
            movement: Movement::Char,
            direction: Direction::Backward,
        });
        insert.insert(&[plain(KeyCode::Right)], Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });
        insert.insert(&[plain(KeyCode::Home)], Command::Move {
            movement: Movement::LineStart,
            direction: Direction::Forward,
        });
        insert.insert(&[plain(KeyCode::End)], Command::Move {
            movement: Movement::LineEnd,
            direction: Direction::Forward,
        });
        // 上下・ページスクロールも Insert で使えるようにする（Helix と同じ）。
        // 上下は移動（モードは変わらない）、PgUp/PgDn はスクロール。
        insert.insert(&[plain(KeyCode::Up)], Command::Move {
            movement: Movement::Line,
            direction: Direction::Backward,
        });
        insert.insert(&[plain(KeyCode::Down)], Command::Move {
            movement: Movement::Line,
            direction: Direction::Forward,
        });
        insert.insert(&[plain(KeyCode::PageUp)], Command::Scroll { pages: -1 });
        insert.insert(&[plain(KeyCode::PageDown)], Command::Scroll { pages: 1 });
        // C-u / C-k: 行頭/行末まで削除（Helix の Insert モード）
        insert.insert(&[ctrl('u')], Command::KillToLineStart);
        insert.insert(&[ctrl('k')], Command::KillToLineEnd);

        Self {
            normal,
            insert,
            select,
        }
    }

    fn root(&self, mode: Mode) -> &Node {
        match mode {
            Mode::Normal => &self.normal,
            Mode::Insert => &self.insert,
            Mode::Select => &self.select,
        }
    }

    /// `pending`（確定前の prefix キー列）を保持しつつ `key` を解決する。
    pub fn resolve(&self, mode: Mode, pending: &mut Vec<KeyEvent>, key: KeyEvent) -> Resolution {
        let key = normalize(key);
        let root = self.root(mode);
        let mut node = root;
        for k in pending.iter() {
            match node.get(k) {
                Some(next) => node = next,
                None => {
                    // 壊れた pending（起こらないはず）: リセットして単一キーで再解決
                    pending.clear();
                    return self.resolve(mode, pending, key);
                }
            }
        }
        match node.get(&key) {
            Some(child) => {
                if let Some(command) = &child.command {
                    pending.clear();
                    Resolution::Command(command.clone())
                } else {
                    pending.push(key);
                    Resolution::Pending
                }
            }
            None => {
                pending.clear();
                Resolution::NoMatch
            }
        }
    }

    /// キーイベントをコマンドに解決する。Insert モードでは未バインドの
    /// 文字キー（修飾キーなし/Shift のみ）をテキスト挿入にフォールバックする。
    pub fn resolve_with_insert_fallback(
        &self,
        mode: Mode,
        pending: &mut Vec<KeyEvent>,
        key: KeyEvent,
    ) -> Resolution {
        let key = normalize(key);
        match self.resolve(mode, pending, key) {
            Resolution::NoMatch if mode == Mode::Insert => match key.code {
                KeyCode::Char(c)
                    if key.modifiers.is_empty() || key.modifiers == Modifiers::SHIFT =>
                {
                    Resolution::Command(Command::Insert { text: c.to_string() })
                }
                _ => Resolution::NoMatch,
            },
            resolution => resolution,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(c: char) -> KeyEvent {
        plain(KeyCode::Char(c))
    }

    #[test]
    fn prefix_g_resolves_after_second_key() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        assert!(matches!(km.resolve(Mode::Normal, &mut pending, k('g')), Resolution::Pending));
        assert_eq!(pending, vec![k('g')]);
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('g')),
            Resolution::Command(Command::Goto { target: GotoTarget::DocumentStart })
        ));
        assert!(pending.is_empty());
    }

    #[test]
    fn simple_key_resolves_immediately() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('h')),
            Resolution::Command(Command::Move { .. })
        ));
        assert!(pending.is_empty());
    }

    #[test]
    fn unmatched_resets_pending() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        km.resolve(Mode::Normal, &mut pending, k('g'));
        assert!(matches!(km.resolve(Mode::Normal, &mut pending, k('x')), Resolution::NoMatch));
        assert!(pending.is_empty());
    }

    #[test]
    fn insert_mode_enter_and_tab_insert_newline_and_tab() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        assert_eq!(
            km.resolve_with_insert_fallback(Mode::Insert, &mut pending, KeyCode::Enter.into()),
            Resolution::Command(Command::Insert { text: "\n".into() })
        );
        assert_eq!(
            km.resolve_with_insert_fallback(Mode::Insert, &mut pending, KeyCode::Tab.into()),
            Resolution::Command(Command::Insert { text: "\t".into() })
        );
        // Normal では Enter/Tab は未定義のまま
        assert!(matches!(
            km.resolve_with_insert_fallback(Mode::Normal, &mut pending, KeyCode::Enter.into()),
            Resolution::NoMatch
        ));
    }

    #[test]
    fn modes_have_separate_keymaps() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        // Insert では h は未定義（S2 で入力になる）
        assert!(matches!(km.resolve(Mode::Insert, &mut pending, k('h')), Resolution::NoMatch));
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, KeyCode::Escape.into()),
            Resolution::Command(Command::SetMode { mode: Mode::Normal })
        ));
    }

    #[test]
    fn uppercase_keys_normalize_shift() {
        // termina は大文字バイトを Char('G')+SHIFT で報告する → 正規化して一致
        let km = Keymaps::new();
        let mut pending = Vec::new();
        // 実際の termina の報告形（大文字 + SHIFT）
        let shift_g = KeyEvent::new(KeyCode::Char('G'), Modifiers::SHIFT);
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, shift_g),
            Resolution::Command(Command::Goto {
                target: GotoTarget::DocumentEnd
            })
        ));
        let shift_u = KeyEvent::new(KeyCode::Char('U'), Modifiers::SHIFT);
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, shift_u),
            Resolution::Command(Command::Redo)
        ));
        // 一部ターミナルの報告形（小文字 + SHIFT）も同じく解決できる
        let shift_u2 = KeyEvent::new(KeyCode::Char('u'), Modifiers::SHIFT);
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, shift_u2),
            Resolution::Command(Command::Redo)
        ));
    }

    #[test]
    fn ctrl_d_scrolls_half_page() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        // Helix と同じ: C-d は半ページ下、C-u は半ページ上
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, ctrl('d')),
            Resolution::Command(Command::ScrollHalf {
                direction: Direction::Forward
            })
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, ctrl('u')),
            Resolution::Command(Command::ScrollHalf {
                direction: Direction::Backward
            })
        ));
    }

    #[test]
    fn shift_a_and_i_resolve_to_line_insert() {
        // ADR-0023: Shift+a → Char('A')（修飾子なし）→ InsertAtLineEnd。
        // Normal/Select の両方で、Insert では文字 'A' として入力される。
        let km = Keymaps::new();
        let mut pending = Vec::new();
        let shift_a = KeyEvent::new(KeyCode::Char('a'), Modifiers::SHIFT);
        for mode in [Mode::Normal, Mode::Select] {
            assert!(matches!(
                km.resolve(mode, &mut pending, shift_a),
                Resolution::Command(Command::InsertAtLineEnd)
            ));
            assert!(matches!(
                km.resolve(mode, &mut pending, k('I')),
                Resolution::Command(Command::InsertAtLineStart)
            ));
        }
        // Insert モードでは fallback で 'A' が入力される
        assert!(matches!(
            km.resolve_with_insert_fallback(Mode::Insert, &mut pending, shift_a),
            Resolution::Command(Command::Insert { text }) if text == "A"
        ));
    }

    #[test]
    fn hjkl_directions_match_vim() {
        // j/k の向きはよく間違えるので明示的に固定する
        let km = Keymaps::new();
        let mut pending = Vec::new();
        let mut move_of = |key| match km.resolve(Mode::Normal, &mut pending, key) {
            Resolution::Command(Command::Move {
                movement,
                direction,
            }) => Some((movement, direction)),
            _ => None,
        };
        assert_eq!(
            move_of(k('j')),
            Some((Movement::Line, Direction::Forward)),
            "j は下（Line Forward）"
        );
        assert_eq!(
            move_of(k('k')),
            Some((Movement::Line, Direction::Backward)),
            "k は上（Line Backward）"
        );
        assert_eq!(
            move_of(k('h')),
            Some((Movement::Char, Direction::Backward))
        );
        assert_eq!(
            move_of(k('l')),
            Some((Movement::Char, Direction::Forward))
        );
    }

    #[test]
    fn edit_bindings() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        // Helix 流: Normal の x は行下へ選択拡張、X は行全体選択
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('x')),
            Resolution::Command(Command::ExtendLineBelow)
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('X')),
            Resolution::Command(Command::SelectLine)
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('u')),
            Resolution::Command(Command::Undo)
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('U')),
            Resolution::Command(Command::Redo)
        ));
        // Helix 流: Normal の d は選択削除（カーソル上では no-op — daemon 側）、
        // c は削除+Insert、a は append、o/O は行を開く
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('d')),
            Resolution::Command(Command::DeleteRange)
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('c')),
            Resolution::Command(Command::Change)
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('a')),
            Resolution::Command(Command::Append)
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('o')),
            Resolution::Command(Command::OpenBelow)
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('O')),
            Resolution::Command(Command::OpenAbove)
        ));
        // % は全選択、n/N は検索ナビ、* は一致を全選択
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('%')),
            Resolution::Command(Command::SelectAll)
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('n')),
            Resolution::Command(Command::SearchNext {
                direction: Direction::Forward
            })
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('N')),
            Resolution::Command(Command::SearchNext {
                direction: Direction::Backward
            })
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('*')),
            Resolution::Command(Command::SearchSelection)
        ));
        // 保存は `:` コマンド（:w）に移行したので s は未定義
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('s')),
            Resolution::NoMatch
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, KeyCode::Backspace.into()),
            Resolution::Command(Command::DeleteBackward)
        ));
        // Select モードでは x / X も拡張系、d / Backspace が範囲削除
        assert!(matches!(
            km.resolve(Mode::Select, &mut pending, k('x')),
            Resolution::Command(Command::ExtendLineBelow)
        ));
        assert!(matches!(
            km.resolve(Mode::Select, &mut pending, k('X')),
            Resolution::Command(Command::SelectLine)
        ));
        assert!(matches!(
            km.resolve(Mode::Select, &mut pending, k('d')),
            Resolution::Command(Command::DeleteRange)
        ));
        assert!(matches!(
            km.resolve(Mode::Select, &mut pending, k('c')),
            Resolution::Command(Command::Change)
        ));
    }

    #[test]
    fn helix_movement_bindings() {
        // Helix との差分で追加した移動キー: e（単語末尾）・Home/End（行頭/行末）
        // ・PageUp/PageDown・C-b/C-f（ページスクロール）
        let km = Keymaps::new();
        let mut pending = Vec::new();
        let mut move_of = |key| match km.resolve(Mode::Normal, &mut pending, key) {
            Resolution::Command(Command::Move {
                movement,
                direction,
            }) => Some((movement, direction)),
            _ => None,
        };
        assert_eq!(
            move_of(k('e')),
            Some((Movement::WordEnd, Direction::Forward))
        );
        assert_eq!(
            move_of(KeyCode::Home.into()),
            Some((Movement::LineStart, Direction::Forward))
        );
        assert_eq!(
            move_of(KeyCode::End.into()),
            Some((Movement::LineEnd, Direction::Forward))
        );
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, KeyCode::PageDown.into()),
            Resolution::Command(Command::Scroll { pages: 1 })
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, KeyCode::PageUp.into()),
            Resolution::Command(Command::Scroll { pages: -1 })
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, ctrl('f')),
            Resolution::Command(Command::Scroll { pages: 1 })
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, ctrl('b')),
            Resolution::Command(Command::Scroll { pages: -1 })
        ));
    }

    #[test]
    fn select_mode_extends_with_new_movements() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        assert!(matches!(
            km.resolve(Mode::Select, &mut pending, k('e')),
            Resolution::Command(Command::Extend {
                movement: Movement::WordEnd,
                direction: Direction::Forward
            })
        ));
        assert!(matches!(
            km.resolve(Mode::Select, &mut pending, KeyCode::Home.into()),
            Resolution::Command(Command::Extend {
                movement: Movement::LineStart,
                ..
            })
        ));
    }

    #[test]
    fn insert_mode_word_delete_bindings() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, ctrl('w')),
            Resolution::Command(Command::DeleteWordBackward)
        ));
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, alt(KeyCode::Backspace)),
            Resolution::Command(Command::DeleteWordBackward)
        ));
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, alt(KeyCode::Char('d'))),
            Resolution::Command(Command::DeleteWordForward)
        ));
        // 文字削除: C-h / C-d も Helix と同じ
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, ctrl('h')),
            Resolution::Command(Command::DeleteBackward)
        ));
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, ctrl('d')),
            Resolution::Command(Command::DeleteForward)
        ));
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, KeyCode::Delete.into()),
            Resolution::Command(Command::DeleteForward)
        ));
    }

    #[test]
    fn insert_mode_home_end_move_to_line_bounds() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, KeyCode::Home.into()),
            Resolution::Command(Command::Move {
                movement: Movement::LineStart,
                ..
            })
        ));
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, KeyCode::End.into()),
            Resolution::Command(Command::Move {
                movement: Movement::LineEnd,
                ..
            })
        ));
    }

    #[test]
    fn g_leader_line_and_document_gotos() {
        // Helix の g リーダー: g e = 文末（last_line）、g h/l/s = 行頭/行末/非空白
        let km = Keymaps::new();
        let mut pending = Vec::new();
        let mut resolve_two = |first: char, second: char| {
            km.resolve(Mode::Normal, &mut pending, k(first));
            km.resolve(Mode::Normal, &mut pending, k(second))
        };
        assert!(matches!(
            resolve_two('g', 'e'),
            Resolution::Command(Command::Goto {
                target: GotoTarget::DocumentEnd
            })
        ));
        assert!(matches!(
            resolve_two('g', 'h'),
            Resolution::Command(Command::Move {
                movement: Movement::LineStart,
                ..
            })
        ));
        assert!(matches!(
            resolve_two('g', 'l'),
            Resolution::Command(Command::Move {
                movement: Movement::LineEnd,
                ..
            })
        ));
        assert!(matches!(
            resolve_two('g', 's'),
            Resolution::Command(Command::Move {
                movement: Movement::FirstNonWhitespace,
                ..
            })
        ));
        // Select では Extend になる（移動系は全て Extend の方針）
        km.resolve(Mode::Select, &mut pending, k('g'));
        assert!(matches!(
            km.resolve(Mode::Select, &mut pending, k('s')),
            Resolution::Command(Command::Extend {
                movement: Movement::FirstNonWhitespace,
                ..
            })
        ));
    }

    #[test]
    fn insert_mode_arrows_and_paging() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, KeyCode::Up.into()),
            Resolution::Command(Command::Move {
                movement: Movement::Line,
                direction: Direction::Backward
            })
        ));
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, KeyCode::Down.into()),
            Resolution::Command(Command::Move {
                movement: Movement::Line,
                direction: Direction::Forward
            })
        ));
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, KeyCode::PageUp.into()),
            Resolution::Command(Command::Scroll { pages: -1 })
        ));
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, KeyCode::PageDown.into()),
            Resolution::Command(Command::Scroll { pages: 1 })
        ));
        // C-u / C-k は行頭/行末まで削除
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, ctrl('u')),
            Resolution::Command(Command::KillToLineStart)
        ));
        assert!(matches!(
            km.resolve(Mode::Insert, &mut pending, ctrl('k')),
            Resolution::Command(Command::KillToLineEnd)
        ));
    }

    #[test]
    fn insert_mode_character_fallback() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        // 未バインドの文字キーは挿入になる
        assert!(matches!(
            km.resolve_with_insert_fallback(Mode::Insert, &mut pending, k('a')),
            Resolution::Command(Command::Insert { text }) if text == "a"
        ));
        // Shift 付き（大文字）も挿入
        let upper = KeyEvent::new(KeyCode::Char('A'), Modifiers::SHIFT);
        assert!(matches!(
            km.resolve_with_insert_fallback(Mode::Insert, &mut pending, upper),
            Resolution::Command(Command::Insert { text }) if text == "A"
        ));
        // Ctrl 付きは挿入しない
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), Modifiers::CONTROL);
        assert!(matches!(
            km.resolve_with_insert_fallback(Mode::Insert, &mut pending, ctrl_a),
            Resolution::NoMatch
        ));
        // Normal モードではフォールバックしない（a は Append に割り当て済み —
        // 未割り当てキーで確認）
        assert!(matches!(
            km.resolve_with_insert_fallback(Mode::Normal, &mut pending, k('z')),
            Resolution::NoMatch
        ));
    }

    #[test]
    fn space_k_peeks_definition_in_normal_and_select() {
        // Space はリーダー（prefix）で、k で定義ポップアップを要求する。
        let km = Keymaps::new();
        let mut pending = Vec::new();
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k(' ')),
            Resolution::Pending
        ));
        assert_eq!(pending, vec![k(' ')]);
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('k')),
            Resolution::Command(Command::PeekDefinition)
        ));
        assert!(pending.is_empty(), "確定後に pending が残らない");
        // Select でも同じ
        assert!(matches!(
            km.resolve(Mode::Select, &mut pending, k(' ')),
            Resolution::Pending
        ));
        assert!(matches!(
            km.resolve(Mode::Select, &mut pending, k('k')),
            Resolution::Command(Command::PeekDefinition)
        ));
        // Insert では Space は入力（fallback）になる
        assert!(matches!(
            km.resolve_with_insert_fallback(Mode::Insert, &mut pending, k(' ')),
            Resolution::Command(Command::Insert { text }) if text == " "
        ));
    }
}
