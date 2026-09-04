//! 分割ツリー: View をスプリット（分割表示）として配置する。

use std::mem;

/// View を一意に識別する ID。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ViewId(pub usize);

/// スプリットの方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDirection {
    /// 上下に分割（横長のペインを重ねる）。
    Horizontal,
    /// 左右に分割（縦長のペインを並べる）。
    Vertical,
}

/// ツリーのノード: 1つの View（葉）か、分割コンテナ。
#[derive(Clone, Debug)]
enum Node {
    View(ViewId),
    Split(Split),
}

/// 分割コンテナ: 同じ方向に並ぶ子ノードの列。
#[derive(Clone, Debug)]
struct Split {
    direction: SplitDirection,
    children: Vec<Node>,
}

/// `mem::replace` 用のプレースホルダ（ルートを一時退避するときだけ使う）。
const PLACEHOLDER: Node = Node::View(ViewId(usize::MAX));

/// View の配置ツリー。
///
/// 葉（[`Node::View`]）が実際の View、内部ノード（[`Node::Split`]）が分割方向。
/// フォーカス（入力を受け取る View）は [`Tree::focused`] が保持する。
#[derive(Clone, Debug)]
pub struct Tree {
    root: Node,
    focused: ViewId,
}

impl Tree {
    /// 単一の View からなるツリーを作る。
    pub fn new(initial: ViewId) -> Self {
        Self {
            root: Node::View(initial),
            focused: initial,
        }
    }

    /// フォーカス中の View。
    pub fn focused(&self) -> ViewId {
        self.focused
    }

    /// フォーカスを設定する。
    pub fn set_focused(&mut self, view_id: ViewId) {
        self.focused = view_id;
    }

    /// 葉（View）を表示順（左上→右下）に列挙する。
    pub fn views_in_order(&self) -> Vec<ViewId> {
        let mut out = Vec::new();
        collect_views(&self.root, &mut out);
        out
    }

    /// フォーカスを次の View へ（末尾から先頭へ循環）。
    pub fn focus_next(&mut self) {
        let order = self.views_in_order();
        let idx = order
            .iter()
            .position(|v| *v == self.focused)
            .expect("focused がツリーにある");
        self.focused = order[(idx + 1) % order.len()];
    }

    /// フォーカスを前の View へ（先頭から末尾へ循環）。
    pub fn focus_prev(&mut self) {
        let order = self.views_in_order();
        let idx = order
            .iter()
            .position(|v| *v == self.focused)
            .expect("focused がツリーにある");
        self.focused = order[(idx + order.len() - 1) % order.len()];
    }

    /// `target` を `direction` に分割する。新しい View は `new_view_id`。
    ///
    /// 親の分割が同じ向きなら兄弟として追加してネストさせず、そうでなければ
    /// `target` を新しい分割で包む。フォーカスは新 View へ移る。
    ///
    /// # Panics
    ///
    /// `target` がツリーに存在しない場合に panic する。
    pub fn split(&mut self, target: ViewId, direction: SplitDirection, new_view_id: ViewId) {
        let mut path = Vec::new();
        assert!(
            find_path(&self.root, target, &mut path),
            "対象の View がツリーにない"
        );

        let same_direction = !path.is_empty()
            && matches!(
                node_at(&self.root, &path[..path.len() - 1]),
                Node::Split(s) if s.direction == direction
            );

        let root = mem::replace(&mut self.root, PLACEHOLDER);
        self.root = if same_direction {
            let parent_path = &path[..path.len() - 1];
            apply_at(root, parent_path, |node| match node {
                Node::Split(mut s) => {
                    s.children.push(Node::View(new_view_id));
                    Node::Split(s)
                }
                _ => unreachable!("同じ向きの親は Split のはず"),
            })
        } else {
            apply_at(root, &path, |node| {
                Node::Split(Split {
                    direction,
                    children: vec![node, Node::View(new_view_id)],
                })
            })
        };
        self.focused = new_view_id;
    }

    /// `view_id` の View を削除する。単一の View しかない（または見つからない）
    /// 場合は何もせず `false` を返す。
    ///
    /// 子が1つになった分割は折りたたまれ、ルートまで波及する。
    pub fn remove(&mut self, view_id: ViewId) -> bool {
        let mut path = Vec::new();
        if !find_path(&self.root, view_id, &mut path) || path.is_empty() {
            return false;
        }
        let root = mem::replace(&mut self.root, PLACEHOLDER);
        self.root = collapse(remove_node(root, &path));
        true
    }
}

/// ルートから `target` までの子インデックス列を `path` に積む。
/// ルート自身が `target` なら `path` は空になる。
fn find_path(node: &Node, target: ViewId, path: &mut Vec<usize>) -> bool {
    match node {
        Node::View(id) => *id == target,
        Node::Split(s) => {
            for (i, child) in s.children.iter().enumerate() {
                if find_path(child, target, path) {
                    path.push(i);
                    return true;
                }
            }
            false
        }
    }
}

/// `path` が指すノードを返す。
fn node_at<'a>(node: &'a Node, path: &[usize]) -> &'a Node {
    let mut node = node;
    for &i in path {
        node = match node {
            Node::Split(s) => &s.children[i],
            Node::View(_) => unreachable!("path が View を指している"),
        };
    }
    node
}

fn collect_views(node: &Node, out: &mut Vec<ViewId>) {
    match node {
        Node::View(id) => out.push(*id),
        Node::Split(s) => {
            for child in &s.children {
                collect_views(child, out);
            }
        }
    }
}

/// `path` が指すノードに `f` を適用したツリーを返す。
fn apply_at(mut node: Node, path: &[usize], f: impl FnOnce(Node) -> Node) -> Node {
    if path.is_empty() {
        return f(node);
    }
    if let Node::Split(s) = &mut node {
        let child = mem::replace(&mut s.children[path[0]], PLACEHOLDER);
        s.children[path[0]] = apply_at(child, &path[1..], f);
    } else {
        unreachable!("path が Split 以外のノードを指している");
    }
    node
}

/// `path` が指す子ノードを親から取り除いたツリーを返す。
/// `path` は少なくとも1要素（ルート自身の削除は呼び出し側で除外済み）。
fn remove_node(mut node: Node, path: &[usize]) -> Node {
    if let Node::Split(s) = &mut node {
        if path.len() == 1 {
            s.children.remove(path[0]);
        } else {
            let child = mem::replace(&mut s.children[path[0]], PLACEHOLDER);
            s.children[path[0]] = remove_node(child, &path[1..]);
        }
    } else {
        unreachable!("path が Split 以外のノードを指している");
    }
    node
}

/// 子が1つになった Split を折りたたむ（ルートまで波及）。
fn collapse(node: Node) -> Node {
    match node {
        Node::Split(s) => {
            let children: Vec<Node> = s.children.into_iter().map(collapse).collect();
            match children.len() {
                1 => children.into_iter().next().expect("要素数1"),
                0 => unreachable!("空の Split は発生しない"),
                _ => Node::Split(Split {
                    direction: s.direction,
                    children,
                }),
            }
        }
        node => node,
    }
}
