//! Selections: the set of active cursor ranges.

use smallvec::SmallVec;

/// One element of a [`Selection`]: a span of text with an anchor (fixed end)
/// and a head (moving end).
///
/// Positions are char indices into the document's text. A range with
/// `anchor == head` is a cursor: a point, not a span.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    anchor: usize,
    head: usize,
}

impl Range {
    /// Creates a range spanning `anchor`..`head` (either direction).
    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    /// Creates a cursor at `pos`.
    pub fn point(pos: usize) -> Self {
        Self {
            anchor: pos,
            head: pos,
        }
    }

    /// The fixed end of the range.
    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// The moving end of the range; extended during selection operations.
    pub fn head(&self) -> usize {
        self.head
    }

    /// The lowest position covered by the range.
    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    /// The highest position covered by the range, exclusive.
    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    /// The number of chars covered.
    pub fn len(&self) -> usize {
        self.end() - self.start()
    }

    /// Whether this is a cursor (no text covered).
    pub fn is_cursor(&self) -> bool {
        self.anchor == self.head
    }
}

/// The set of active cursor ranges.
///
/// A single cursor is a `Selection` with one range. Invariants: at least one
/// range, `primary_index` in bounds, and ranges sorted by start without
/// overlap (checked by `debug_assert` in [`Selection::new`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    ranges: SmallVec<[Range; 1]>,
    primary_index: usize,
}

impl Selection {
    /// Creates a selection from `ranges`; `primary_index` names the primary
    /// range.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if `ranges` is empty, `primary_index` is out
    /// of bounds, or ranges overlap or are out of order.
    pub fn new(ranges: Vec<Range>, primary_index: usize) -> Self {
        debug_assert!(!ranges.is_empty(), "selection must contain at least one range");
        debug_assert!(
            primary_index < ranges.len(),
            "primary index out of bounds"
        );
        debug_assert!(
            ranges.windows(2).all(|w| w[0].end() <= w[1].start()),
            "ranges must be sorted by start and non-overlapping"
        );
        Self {
            ranges: ranges.into(),
            primary_index,
        }
    }

    /// Creates a single-cursor selection at `pos`.
    pub fn point(pos: usize) -> Self {
        Self::new(vec![Range::point(pos)], 0)
    }

    /// The number of ranges.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// Whether this is a single cursor covering no text.
    pub fn is_single_cursor(&self) -> bool {
        self.len() == 1 && self.ranges[0].is_cursor()
    }

    /// The primary range — the one operations such as extend act on.
    pub fn primary(&self) -> Range {
        self.ranges[self.primary_index]
    }

    /// The index of the primary range.
    pub fn primary_index(&self) -> usize {
        self.primary_index
    }

    /// All ranges, in order.
    pub fn ranges(&self) -> &[Range] {
        &self.ranges
    }
}

impl Default for Selection {
    /// A single cursor at position 0.
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
