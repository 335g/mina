//! mina-loader: 言語定義レジストリ。
//!
//! tree-sitter の grammar とハイライトクエリ (.scm) を言語ごとに束ねて提供する
//! 静的レジストリ (ADR-0017: tree-sitter 採用 / ADR-0018: フラット taxonomy)。
//! 2言語目以降は Cargo 依存の追加 + このファイルへの1エントリで済む。
//!
//! ハイライトクエリの capture 名は `HighlightGroup` (mina-protocol、M2 で導入) の
//! 小文字名に一致させる: comment keyword string number constant function type
//! parameter field operator punctuation attribute error。

use mina_protocol::{HighlightGroup, HighlightRange};
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

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

/// テキストのハイライト範囲を計算する（byte 範囲 → char インデックスに変換）。
///
/// 同じノードに複数の capture がマッチする場合（例: `x.method()` の `method` が
/// `@function` と `@field` の両方 — M2 引き継ぎ #1）は優先順位で1つに確定する。
/// tree-sitter のクエリには「親ノードが X でない」という除外述語がなく、
/// クエリ側で除外できないため、計算側で確定する（`priority` 参照）。
///
/// 戻り値は start 昇順・重複しない範囲（仕様書の不変条件: 同じ char は1つの
/// グループに属する）。
///
/// ponytail: 全文パース（インクリメンタルは編集範囲の追跡が各編集源に必要に
/// なるため、巨大ファイルのプロファイル後に導入する）。
pub fn compute_highlights(def: &LanguageDef, text: &str) -> Vec<HighlightRange> {
    let mut parser = Parser::new();
    if parser.set_language(&(def.grammar)()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let Ok(query) = Query::new(&(def.grammar)(), def.highlights) else {
        return Vec::new();
    };

    let mut items: Vec<(usize, usize, HighlightGroup)> = Vec::new();
    {
        let mut cursor = QueryCursor::new();
        let mut captures = cursor.captures(&query, tree.root_node(), text.as_bytes());
        while let Some((m, idx)) = captures.next() {
            let capture = &m.captures[*idx];
            let name = &query.capture_names()[capture.index as usize];
            let Some(group) = group_for_capture(name) else {
                continue; // 正規集合 (ADR-0018) 外の capture 名は無視
            };
            let node = capture.node;
            let (start, end) = (node.start_byte(), node.end_byte());
            if end > start {
                items.push((start, end, group));
            }
        }
    }

    // 重複の確定: start 昇順 → 優先順位降順 → 終了 byte 降順でソートし、
    // 直前の範囲と重なるものを落とす。同一ノードの複数 capture は同範囲なの
    // で、優先順位の高い先頭だけが残る（ネストした ERROR 内部の capture も
    // 同様に外側が勝つ）。
    items.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| priority(b.2).cmp(&priority(a.2)))
            .then_with(|| b.1.cmp(&a.1))
    });
    let mut deduped: Vec<(usize, usize, HighlightGroup)> = Vec::new();
    for item in items {
        if deduped
            .last()
            .is_some_and(|last| item.0 < last.1)
        {
            continue; // 直前の範囲と重なる（同一ノードの別 capture・ネスト）
        }
        deduped.push(item);
    }

    // byte → char インデックス変換（コードベースの慣習: char インデックス）。
    // char_starts[c] = 文字 c の開始 byte（c = n_chars は text.len()）。
    let mut char_starts: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    char_starts.push(text.len());
    let char_at = |byte: usize| char_starts.partition_point(|&b| b <= byte) - 1;
    deduped
        .into_iter()
        .map(|(start, end, group)| HighlightRange {
            start: char_at(start),
            end: char_at(end),
            group,
        })
        .collect()
}

/// capture 名（`@` 付きでも可）を [`HighlightGroup`] に写像する。正規集合外は `None`。
fn group_for_capture(name: &str) -> Option<HighlightGroup> {
    Some(match name.strip_prefix('@').unwrap_or(name) {
        "comment" => HighlightGroup::Comment,
        "keyword" => HighlightGroup::Keyword,
        "string" => HighlightGroup::String,
        "number" => HighlightGroup::Number,
        "constant" => HighlightGroup::Constant,
        "function" => HighlightGroup::Function,
        "type" => HighlightGroup::Type,
        "parameter" => HighlightGroup::Parameter,
        "field" => HighlightGroup::Field,
        "operator" => HighlightGroup::Operator,
        "punctuation" => HighlightGroup::Punctuation,
        "attribute" => HighlightGroup::Attribute,
        "error" => HighlightGroup::Error,
        _ => return None,
    })
}

/// 同一ノードに複数 capture が付いたときの優先順位（高い方が残る）。
///
/// `x.method()` の `method` は `@field` と `@function` の両方にマッチするが、
/// 呼び出し対象の field は関数呼び出しなので `function` を優先する（M2 #1）。
fn priority(group: HighlightGroup) -> u8 {
    match group {
        HighlightGroup::Function => 1,
        _ => 0,
    }
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
        // 構文エラーを含む入力でもパースとクエリが走り、error グループが現れる
        // （M2 引き継ぎ #2: パニックしないことだけでなく error 発火も検証する）
        let def = language_by_name("rust").unwrap();
        let names = capture_names(def, "fn broken( {");
        assert!(names.contains(&"error".to_string()), "error が発火する: {names:?}");
        // エラー入力でも正規集合外の capture は出ない
        for name in &names {
            assert!(
                CANONICAL.contains(&name.as_str()),
                "非正規 capture 名: {name}"
            );
        }
    }

    /// 範囲の不変条件を検証する: start 昇順・重複なし・テキスト内・char 境界。
    fn assert_valid_ranges(text: &str, ranges: &[HighlightRange]) {
        let n_chars = text.chars().count();
        let mut prev_end = 0usize;
        for r in ranges {
            assert!(r.start < r.end, "空でない範囲: {r:?}");
            assert!(r.end <= n_chars, "テキスト内: {r:?}");
            assert!(
                r.start >= prev_end,
                "重複しない・昇順: {r:?} (prev_end={prev_end})"
            );
            prev_end = r.end;
        }
    }

    /// 範囲のテキストを char インデックスで取り出す（byte スライスは使わない）。
    fn text_of(src: &str, r: &HighlightRange) -> String {
        src.chars().skip(r.start).take(r.end - r.start).collect()
    }

    #[test]
    fn method_call_field_is_function_not_field() {
        // M2 引き継ぎ #1: x.method() の method は @field と @function の両方に
        // マッチするが、範囲計算で function に確定し、field は残らない。
        let def = language_by_name("rust").unwrap();
        let src = "let p = Point { x: 1 }; let a = p.x; let b = p.x.max(a);";
        let ranges = compute_highlights(def, src);
        assert_valid_ranges(src, &ranges);

        let method = ranges
            .iter()
            .find(|r| text_of(src, r) == "max")
            .expect("max がハイライトされる");
        assert_eq!(method.group, HighlightGroup::Function, "{ranges:?}");
        assert!(
            ranges
                .iter()
                .filter(|r| r.group == HighlightGroup::Field)
                .all(|r| text_of(src, r) != "max"),
            "max に field は残らない: {ranges:?}"
        );
        // 単純なフィールドアクセス p.x は field のまま
        let x = ranges
            .iter()
            .find(|r| text_of(src, r) == "x")
            .expect("x がハイライトされる");
        assert_eq!(x.group, HighlightGroup::Field, "{ranges:?}");
    }

    #[test]
    fn compute_highlights_maps_captures_to_groups_and_char_indices() {
        // グループ写像・char インデックス・範囲の不変条件を一括検証。
        // 全角文字を含むコメントの前後でも char 境界が正しいこと。
        let def = language_by_name("rust").unwrap();
        let src = "// 注釈\nfn add(a: i32) -> i32 { a + 1 }\n";
        let ranges = compute_highlights(def, src);
        assert_valid_ranges(src, &ranges);

        let keyword = ranges
            .iter()
            .find(|r| text_of(src, r) == "fn")
            .expect("fn が keyword");
        assert_eq!(keyword.group, HighlightGroup::Keyword);
        let comment = ranges
            .iter()
            .find(|r| r.group == HighlightGroup::Comment)
            .expect("コメントがある");
        assert_eq!(text_of(src, comment), "// 注釈");
        let number = ranges
            .iter()
            .find(|r| text_of(src, r) == "1")
            .expect("1 が number");
        assert_eq!(number.group, HighlightGroup::Number);
    }
}
