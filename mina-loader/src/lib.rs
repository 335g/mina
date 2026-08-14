//! mina-loader: 言語定義レジストリ。
//!
//! tree-sitter の grammar とハイライトクエリ (.scm) を言語ごとに束ねて提供する
//! 静的レジストリ (ADR-0017: tree-sitter 採用 / ADR-0018: フラット taxonomy)。
//! 2言語目以降は Cargo 依存の追加 + このファイルへの1エントリで済む。
//!
//! ハイライトクエリの capture 名は `HighlightGroup` (mina-protocol、M2 で導入) の
//! 小文字名に一致させる: comment keyword string number constant function type
//! parameter field operator punctuation attribute error。

use tree_sitter::Language;

/// 1言語分の定義。`grammar` はパース用、`highlights` はハイライトクエリ。
pub struct LanguageDef {
    pub name: &'static str,
    /// ドット付き拡張子 (例: `[".rs"]`)。
    pub extensions: &'static [&'static str],
    /// grammar を返す関数。`LanguageFn` → `Language` の変換が実行時のみのため
    /// const レジストリと両立する形として関数ポインタにしている (M1)。
    pub grammar: fn() -> Language,
    /// `include_str!` で同梱した .scm (capture 名は HighlightGroup の小文字名)。
    pub highlights: &'static str,
}

fn rust_grammar() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

/// Rust (マイルストーン1の最初の言語。lsp.rs の `.rs → rust-analyzer` 対応と揃える)。
pub const RUST: LanguageDef = LanguageDef {
    name: "rust",
    extensions: &[".rs"],
    grammar: rust_grammar,
    highlights: include_str!("../highlights/rust.scm"),
};

/// 静的レジストリ。言語追加はここに1エントリ。
pub static LANGUAGES: [&LanguageDef; 1] = [&RUST];

/// パスの拡張子から言語を引く。未登録の拡張子・拡張子なしは `None`。
pub fn language_for_path(path: &str) -> Option<&'static LanguageDef> {
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    LANGUAGES.iter().copied().find(|def| {
        def.extensions
            .iter()
            .any(|e| e.strip_prefix('.').unwrap_or(e) == ext)
    })
}

/// 言語名から引く。未登録なら `None`。
pub fn language_by_name(name: &str) -> Option<&'static LanguageDef> {
    LANGUAGES.iter().copied().find(|def| def.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

    /// ADR-0018 の正規グループ (capture 名の正規集合)。
    const CANONICAL: [&str; 13] = [
        "comment",
        "keyword",
        "string",
        "number",
        "constant",
        "function",
        "type",
        "parameter",
        "field",
        "operator",
        "punctuation",
        "attribute",
        "error",
    ];

    /// スニペットをパースし、クエリが付けた capture 名を返す。
    fn capture_names(def: &LanguageDef, src: &str) -> Vec<String> {
        let mut parser = Parser::new();
        parser.set_language(&(def.grammar)()).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let query = Query::new(&(def.grammar)(), def.highlights).unwrap();
        let mut cursor = QueryCursor::new();
        // tree-sitter 0.26 の QueryCaptures は StreamingIterator (next() = advance→get)。
        // 各 (match, idx) は1 capture。capture 名 id は capture 自身の `index` フィールド。
        let mut captures = cursor.captures(&query, tree.root_node(), src.as_bytes());
        let mut names = Vec::new();
        while let Some((m, idx)) = captures.next() {
            let capture = &m.captures[*idx];
            let name = query.capture_names()[capture.index as usize];
            names.push(name.strip_prefix('@').unwrap_or(name).to_string());
        }
        names
    }

    #[test]
    fn rust_snippet_highlights_expected_groups() {
        let def = language_by_name("rust").expect("rust が登録済み");
        let src = r#"
// comment text
#[allow(dead_code)]
fn add(a: i32, b: i32) -> i32 {
    let sum = a + b;
    println!("sum: {}", sum);
    const MAX: i32 = 10;
    let p = Point { x: sum };
    let dx = p.x;
    if sum > MAX { return sum; }
    (1.5, true)
}
"#;
        let names: std::collections::BTreeSet<_> = capture_names(def, src).into_iter().collect();
        for group in [
            "comment",
            "keyword",
            "string",
            "number",
            "constant",
            "function",
            "type",
            "parameter",
            "field",
            "operator",
            "punctuation",
            "attribute",
        ] {
            assert!(
                names.contains(group),
                "グループ {group} が得られない: {names:?}"
            );
        }
        // ADR-0018: capture 名は正規集合から外れない
        for name in &names {
            assert!(
                CANONICAL.contains(&name.as_str()),
                "非正規 capture 名: {name}"
            );
        }
    }

    #[test]
    fn unknown_extension_resolves_to_none() {
        assert!(language_for_path("readme.txt").is_none());
        assert!(language_for_path("Makefile").is_none());
        assert!(language_for_path("src/lib.rs").is_some());
        assert_eq!(language_for_path("src/lib.rs").unwrap().name, "rust");
        assert!(language_by_name("ruby").is_none());
    }

    #[test]
    fn broken_input_parses_without_panic() {
        // 構文エラーを含む入力でもパースとクエリが走る (error グループが現れ得る)
        let def = language_by_name("rust").unwrap();
        let _ = capture_names(def, "fn broken( {");
    }
}
