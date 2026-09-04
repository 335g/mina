//! 選択: アクティブなカーソル範囲の集合。

use smallvec::SmallVec;

/// [`Selection`] の1要素: anchor（固定端）と head（移動端）を持つテキストの区間。
///
/// 位置は文書テキストへの char インデックス。`anchor == head` の Range は
/// カーソル、つまり区間ではなく点を表す。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    anchor: usize,
    head: usize,
}

impl Range {
    /// `anchor`..`head` を張る Range を作成する（向きはどちらでもよい）。
    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    /// `pos` にカーソルを作成する。
    pub fn point(pos: usize) -> Self {
        Self {
            anchor: pos,
            head: pos,
        }
    }

    /// Range の固定端。
    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// Range の移動端。選択操作（extend など）ではこちらが伸びる。
    pub fn head(&self) -> usize {
        self.head
    }

    /// Range が覆う最小の位置。
    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    /// Range が覆う最大の位置（排他的）。
    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    /// 覆われる char 数。
    //
    // `is_empty` は意図的に置かない: テキストを覆わない Range はすなわち
    // カーソルであり、専用のアクセサ（is_cursor）がある。
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.end() - self.start()
    }

    /// カーソル（テキストを覆わない点）かどうか。
    pub fn is_cursor(&self) -> bool {
        self.anchor == self.head
    }
}

/// アクティブなカーソル範囲の集合。
///
/// 単一カーソルは Range を1つ持つ `Selection` である。不変条件: Range が
/// 1つ以上あること、`primary_index` が範囲内であること、Range が開始位置の
/// 昇順でソートされ重複しないこと（[`Selection::new`] の `debug_assert` で検査）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    ranges: SmallVec<[Range; 1]>,
    primary_index: usize,
}

impl Selection {
    /// `ranges` から選択を作成する。`primary_index` が primary Range を指す。
    ///
    /// # Panics
    ///
    /// debug ビルドで、`ranges` が空・`primary_index` が範囲外・Range の
    /// 重複または順序違反がある場合に panic する。
    pub fn new(ranges: Vec<Range>, primary_index: usize) -> Self {
        debug_assert!(!ranges.is_empty(), "選択には少なくとも1つの Range が必要");
        debug_assert!(primary_index < ranges.len(), "primary インデックスが範囲外");
        debug_assert!(
            ranges.windows(2).all(|w| w[0].end() <= w[1].start()),
            "Range は開始位置の昇順かつ非重複である必要がある"
        );
        Self {
            ranges: ranges.into(),
            primary_index,
        }
    }

    /// `pos` に単一カーソルの選択を作成する。
    pub fn point(pos: usize) -> Self {
        Self::new(vec![Range::point(pos)], 0)
    }

    /// Range の個数。
    //
    // `is_empty` は意図的に置かない: 不変条件により選択は常に1つ以上の
    // Range を持ち、空になり得ない。
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// テキストを覆わない単一カーソルかどうか。
    pub fn is_single_cursor(&self) -> bool {
        self.len() == 1 && self.ranges[0].is_cursor()
    }

    /// primary Range — extend などの操作が作用する対象。
    pub fn primary(&self) -> Range {
        self.ranges[self.primary_index]
    }

    /// primary Range のインデックス。
    pub fn primary_index(&self) -> usize {
        self.primary_index
    }

    /// すべての Range（順序どおり）。
    pub fn ranges(&self) -> &[Range] {
        &self.ranges
    }
}

impl Default for Selection {
    /// 位置0の単一カーソル。
    fn default() -> Self {
        Self::point(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_is_a_cursor() {
        let sel = Selection::point(4);
        assert_eq!(sel.len(), 1);
        assert!(sel.is_single_cursor());
        assert_eq!(sel.primary(), Range::point(4));
    }

    #[test]
    fn range_orders_ends() {
        let r = Range::new(5, 2);
        assert_eq!(r.anchor(), 5);
        assert_eq!(r.head(), 2);
        assert_eq!(r.start(), 2);
        assert_eq!(r.end(), 5);
        assert_eq!(r.len(), 3);
        assert!(!r.is_cursor());
    }

    #[test]
    fn multi_range_selection_keeps_primary() {
        let sel = Selection::new(vec![Range::point(1), Range::point(5)], 1);
        assert_eq!(sel.len(), 2);
        assert_eq!(sel.primary_index(), 1);
        assert_eq!(sel.primary(), Range::point(5));
        assert_eq!(sel.ranges(), &[Range::point(1), Range::point(5)]);
    }

    #[test]
    #[should_panic]
    fn empty_ranges_panic() {
        Selection::new(vec![], 0);
    }

    #[test]
    #[should_panic]
    fn primary_out_of_bounds_panics() {
        Selection::new(vec![Range::point(0)], 1);
    }
}
