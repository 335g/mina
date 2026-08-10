//! The [`Document`]: the unit of editable text.

use ropey::Rope;

/// The unit of editable text.
///
/// A thin newtype over [`Rope`]. It owns the text content and is the target
/// of edits; the active cursor state ([`Selection`](crate::Selection)) lives
/// outside it, so one document can later be shown in several views.
///
/// Properties beyond the text (syntax, diagnostics, edit history) will be
/// added here only when a step actually needs them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    text: Rope,
}

impl Document {
    /// Creates an empty document.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a document containing `text`.
    pub fn from_str(text: &str) -> Self {
        Self {
            text: Rope::from_str(text),
        }
    }

    /// The underlying text.
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// The length of the document in Unicode scalar values (chars).
    pub fn len_chars(&self) -> usize {
        self.text.len_chars()
    }

    /// Whether the document contains no text.
    pub fn is_empty(&self) -> bool {
        self.text.len_chars() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_has_zero_chars() {
        let doc = Document::new();
        assert!(doc.is_empty());
        assert_eq!(doc.len_chars(), 0);
    }

    #[test]
    fn len_chars_counts_unicode_scalars() {
        let doc = Document::from_str("aβ😀");
        assert_eq!(doc.len_chars(), 3);
    }

    #[test]
    fn from_str_keeps_text() {
        let doc = Document::from_str("hello");
        assert_eq!(doc.text().to_string(), "hello");
    }
}
