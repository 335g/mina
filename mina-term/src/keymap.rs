//! キーマップ: モード別の prefix トライ。キー列 → [`Command`] を解決する。
//!
//! 子ノードは `Vec<(KeyEvent, Node)>` の線形探索（バインディング数が少ないので
//! ハッシュより速い）。
//! ponytail: バインディングが数十を超えたら `HashMap` 化を検討する（要 Hash key）。

use mina_protocol::{Command, Direction, GotoTarget, Mode, Movement};
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

impl Keymaps {
    /// S1 のバインディングセットを構築する。
    pub fn new() -> Self {
        let mut normal = Node::default();
        let mv = |m: Movement, d: Direction| Command::Move {
            movement: m,
            direction: d,
        };
        let ex = |m: Movement, d: Direction| Command::Extend {
            movement: m,
            direction: d,
        };

        // Normal: 移動
        for (key, movement) in [
            (KeyCode::Char('h'), Movement::Char),
            (KeyCode::Char('l'), Movement::Char),
            (KeyCode::Char('j'), Movement::Line),
            (KeyCode::Char('k'), Movement::Line),
            (KeyCode::Char('w'), Movement::Word),
            (KeyCode::Char('b'), Movement::Word),
        ] {
            normal.insert(
                &[plain(key)],
                mv(movement, if matches!(key, KeyCode::Char('h') | KeyCode::Char('k') | KeyCode::Char('b')) { Direction::Backward } else { Direction::Forward }),
            );
        }
        // 矢印キー
        normal.insert(&[plain(KeyCode::Left)], mv(Movement::Char, Direction::Backward));
        normal.insert(&[plain(KeyCode::Right)], mv(Movement::Char, Direction::Forward));
        normal.insert(&[plain(KeyCode::Up)], mv(Movement::Line, Direction::Backward));
        normal.insert(&[plain(KeyCode::Down)], mv(Movement::Line, Direction::Forward));
        // prefix g: g g = 先頭, G = 末尾
        normal.insert(
            &[plain(KeyCode::Char('g')), plain(KeyCode::Char('g'))],
            Command::Goto {
                target: GotoTarget::DocumentStart,
            },
        );
        normal.insert(
            &[plain(KeyCode::Char('G'))],
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        );
        // スクロール（ページ単位、高さは daemon 側が把握）
        normal.insert(&[ctrl('d')], Command::Scroll { pages: 1 });
        normal.insert(&[ctrl('u')], Command::Scroll { pages: -1 });
        // モード遷移
        normal.insert(
            &[plain(KeyCode::Char('v'))],
            Command::SetMode { mode: Mode::Select },
        );
        normal.insert(
            &[plain(KeyCode::Char('i'))],
            Command::SetMode { mode: Mode::Insert },
        );
        // 編集
        normal.insert(&[plain(KeyCode::Char('x'))], Command::DeleteForward);
        normal.insert(&[plain(KeyCode::Backspace)], Command::DeleteBackward);
        normal.insert(&[plain(KeyCode::Char('u'))], Command::Undo);
        normal.insert(&[plain(KeyCode::Char('U'))], Command::Redo);
        // 保存は `:` コマンドモードの :w（Helix/vim 流）。s には割り当てない。

        // Select: 拡張移動 + 解除
        let mut select = Node::default();
        for (key, movement) in [
            (KeyCode::Char('h'), Movement::Char),
            (KeyCode::Char('l'), Movement::Char),
            (KeyCode::Char('j'), Movement::Line),
            (KeyCode::Char('k'), Movement::Line),
            (KeyCode::Char('w'), Movement::Word),
            (KeyCode::Char('b'), Movement::Word),
        ] {
            select.insert(
                &[plain(key)],
                ex(movement, if matches!(key, KeyCode::Char('h') | KeyCode::Char('k') | KeyCode::Char('b')) { Direction::Backward } else { Direction::Forward }),
            );
        }
        select.insert(&[plain(KeyCode::Left)], ex(Movement::Char, Direction::Backward));
        select.insert(&[plain(KeyCode::Right)], ex(Movement::Char, Direction::Forward));
        select.insert(&[plain(KeyCode::Up)], ex(Movement::Line, Direction::Backward));
        select.insert(&[plain(KeyCode::Down)], ex(Movement::Line, Direction::Forward));
        select.insert(
            &[plain(KeyCode::Char('g')), plain(KeyCode::Char('g'))],
            Command::Goto {
                target: GotoTarget::DocumentStart,
            },
        );
        select.insert(
            &[plain(KeyCode::Char('G'))],
            Command::Goto {
                target: GotoTarget::DocumentEnd,
            },
        );
        select.insert(
            &[plain(KeyCode::Char('v'))],
            Command::SetMode { mode: Mode::Normal },
        );
        select.insert(
            &[plain(KeyCode::Escape)],
            Command::SetMode { mode: Mode::Normal },
        );
        select.insert(&[plain(KeyCode::Char('x'))], Command::DeleteRange);
        select.insert(&[plain(KeyCode::Backspace)], Command::DeleteRange);
        select.insert(&[plain(KeyCode::Char('u'))], Command::Undo);
        select.insert(&[plain(KeyCode::Char('U'))], Command::Redo);

        // Insert: 文字入力はクライアント側のフォールバック。ここでは
        // Esc・Enter（改行）・Tab・Backspace・左右移動のみ
        let mut insert = Node::default();
        insert.insert(
            &[plain(KeyCode::Escape)],
            Command::SetMode { mode: Mode::Normal },
        );
        insert.insert(&[plain(KeyCode::Enter)], Command::Insert { text: "\n".into() });
        insert.insert(&[plain(KeyCode::Tab)], Command::Insert { text: "\t".into() });
        insert.insert(&[plain(KeyCode::Backspace)], Command::DeleteBackward);
        insert.insert(&[plain(KeyCode::Left)], Command::Move {
            movement: Movement::Char,
            direction: Direction::Backward,
        });
        insert.insert(&[plain(KeyCode::Right)], Command::Move {
            movement: Movement::Char,
            direction: Direction::Forward,
        });

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
    fn ctrl_d_scrolls() {
        let km = Keymaps::new();
        let mut pending = Vec::new();
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, ctrl('d')),
            Resolution::Command(Command::Scroll { pages: 1 })
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
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('x')),
            Resolution::Command(Command::DeleteForward)
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('u')),
            Resolution::Command(Command::Undo)
        ));
        assert!(matches!(
            km.resolve(Mode::Normal, &mut pending, k('U')),
            Resolution::Command(Command::Redo)
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
        // Select モードでは x が範囲削除
        assert!(matches!(
            km.resolve(Mode::Select, &mut pending, k('x')),
            Resolution::Command(Command::DeleteRange)
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
        // Normal モードではフォールバックしない
        assert!(matches!(
            km.resolve_with_insert_fallback(Mode::Normal, &mut pending, k('a')),
            Resolution::NoMatch
        ));
    }
}
