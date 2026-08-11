//! LSP 位置（行・列）と char インデックスの変換。列は行内のオフセット。

/// UTF-8（バイト）列 → 行内 char 数。
pub fn utf8_col_to_char(line: &str, byte_col: u32) -> usize {
    let byte_col = (byte_col as usize).min(line.len());
    line[..byte_col].chars().count()
}

/// UTF-16 単位の列 → 行内 char 数（サロゲートペアを2単位として数える）。
pub fn utf16_col_to_char(line: &str, utf16_col: u32) -> usize {
    let mut units = 0u32;
    for (i, ch) in line.chars().enumerate() {
        if units >= utf16_col {
            return i;
        }
        units += ch.len_utf16() as u32;
    }
    line.chars().count()
}

/// 行内 char 数 → UTF-16 単位の列。
pub fn char_to_utf16_col(line: &str, char_col: usize) -> u32 {
    line.chars()
        .take(char_col)
        .map(|c| c.len_utf16() as u32)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_col_counts_chars_not_bytes() {
        assert_eq!(utf8_col_to_char("あいう", 6), 2); // バイト6 = "あい"
        assert_eq!(utf8_col_to_char("あいう", 9), 3); // バイト9 = "あいう"
        assert_eq!(utf8_col_to_char("ab", 1), 1);
    }

    #[test]
    fn utf16_col_handles_surrogates() {
        // 😀 は UTF-16 で2単位
        assert_eq!(utf16_col_to_char("a😀b", 1), 1); // 'a' の直後
        assert_eq!(utf16_col_to_char("a😀b", 3), 2); // 'b' の直前（a=1 + 😀=2）
        assert_eq!(char_to_utf16_col("a😀b", 2), 3);
    }

    #[test]
    fn utf16_col_clamps_to_line_end() {
        assert_eq!(utf16_col_to_char("ab", 99), 2);
        assert_eq!(utf16_col_to_char("", 0), 0);
    }
}
