//! [`Document`]: 編集対象のテキストの単位。

use ropey::Rope;

/// 編集対象のテキストの単位。
///
/// [`Rope`] に対する薄い NewType。テキストの内容を所有し、編集の対象となる。
/// アクティブなカーソル状態（[`Selection`](crate::Selection)）はこの外に置く —
/// 1つの文書を後で複数の View で表示できるようにするためである。
///
/// テキスト以外のプロパティ（構文・診断・編集履歴）は、実際に必要なステップが
/// 来た時点でここに追加する。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    text: Rope,
}

impl Document {
    /// 空の文書を作成する。
    pub fn new() -> Self {
        Self::default()
    }

    /// テキストの内容。
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// 文書の長さ（Unicode スカラー値 = char の数）。
    pub fn len_chars(&self) -> usize {
        self.text.len_chars()
    }

    /// 文書がテキストを一切含まないかどうか。
    pub fn is_empty(&self) -> bool {
        self.text.len_chars() == 0
    }
}

impl From<&str> for Document {
    fn from(text: &str) -> Self {
        Self {
            text: Rope::from_str(text),
        }
    }
}

impl From<Rope> for Document {
    fn from(text: Rope) -> Self {
        Self { text }
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
        let doc = Document::from("aβ😀");
        assert_eq!(doc.len_chars(), 3);
    }

    #[test]
    fn from_str_keeps_text() {
        let doc = Document::from("hello");
        assert_eq!(doc.text().to_string(), "hello");
    }
}
