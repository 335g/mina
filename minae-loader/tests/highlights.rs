//! ハイライト計算の回帰テスト（敵対的検証から昇格）。
//!
//! compute_highlights の不変条件（仕様書）と、クエリの coverage を検証する。

use minae_loader::{compute_highlights, language_by_name};
use minae_protocol::{HighlightGroup, HighlightRange};

fn ranges(src: &str) -> Vec<HighlightRange> {
    compute_highlights(language_by_name("rust").unwrap(), src)
}
fn text_of(src: &str, r: &HighlightRange) -> String {
    src.chars().skip(r.start).take(r.end - r.start).collect()
}
fn assert_valid(text: &str, r: &[HighlightRange]) {
    let n = text.chars().count();
    let mut prev = 0usize;
    for x in r {
        assert!(x.start < x.end && x.end <= n, "範囲不正: {x:?}");
        assert!(x.start >= prev, "重複/非昇順: {x:?}");
        prev = x.end;
    }
}

#[test]
fn macro_invocation() {
    // (macro_invocation macro: ...) が grammar の field 名 (path?) と一致するか
    let src = "fn main() { println!(\"hi\"); }";
    let r = ranges(src);
    let hits: Vec<_> = r.iter().filter(|r| text_of(src, r) == "println").collect();
    println!("macro probe: {:?}", hits);
    assert!(!hits.is_empty(), "println がハイライトされる (現状 {r:?})");
    assert_eq!(hits[0].group, HighlightGroup::Function);
}

#[test]
fn scoped_path_call_is_highlighted_as_function() {
    // HashMap::new() の new（scoped_identifier の name）が function になる
    let src = "fn main() { let m = std::collections::HashMap::new(); let n = foo::bar(); }";
    let r = ranges(src);
    assert_valid(src, &r);
    for name in ["new", "bar"] {
        let hits: Vec<_> = r.iter().filter(|x| text_of(src, x) == name).collect();
        assert_eq!(hits.len(), 1, "{name}: {r:?}");
        assert_eq!(hits[0].group, HighlightGroup::Function, "{name}: {r:?}");
    }
    // パス部分 (HashMap, foo) は function にならない
    assert!(
        r.iter().filter(|x| x.group == HighlightGroup::Function)
            .all(|x| text_of(src, x) != "HashMap"),
        "パス部分は function にしない: {r:?}"
    );
}

#[test]
fn deep_method_chain() {
    let src = "fn f() { let a = x.y.z.method(); let b = p.x; }";
    let r = ranges(src);
    assert_valid(src, &r);
    for (name, expect) in [
        ("method", HighlightGroup::Function),
        ("y", HighlightGroup::Field),
        ("z", HighlightGroup::Field),
    ] {
        let hits: Vec<_> = r.iter().filter(|x| text_of(src, x) == name).collect();
        assert_eq!(hits.len(), 1, "{name}: {r:?}");
        assert_eq!(hits[0].group, expect, "{name}: {r:?}");
    }
}

#[test]
fn crlf_comments() {
    let src = "// comment\r\nfn f() {}\r\n";
    let r = ranges(src);
    assert_valid(src, &r);
    let c = r
        .iter()
        .find(|x| x.group == HighlightGroup::Comment)
        .expect("comment がある");
    println!("crlf comment: {}..{} = {:?}", c.start, c.end, text_of(src, c));
}

#[test]
fn empty_and_whitespace() {
    assert!(ranges("").is_empty());
    assert!(ranges("   \n\t\n").is_empty());
}

#[test]
fn error_nesting_is_partitioned() {
    // ERROR 内部の capture は外側の error が勝ち、重複しない
    let src = "fn broken( { let x = \"unterminated";
    let r = ranges(src);
    println!("error nesting: {r:?}");
    assert_valid(src, &r);
}

#[test]
fn multibyte_string_range() {
    let src = "fn main() { let s = \"あいうえお\"; }";
    let r = ranges(src);
    assert_valid(src, &r);
    let s_range = r
        .iter()
        .find(|x| x.group == HighlightGroup::String)
        .expect("string がある");
    assert_eq!(text_of(src, s_range), "\"あいうえお\"");
}

#[test]
fn tabs_inside_string() {
    let src = "fn main() { let s = \"a\tb\"; }";
    let r = ranges(src);
    assert_valid(src, &r);
    let s_range = r
        .iter()
        .find(|x| x.group == HighlightGroup::String)
        .expect("string がある");
    assert_eq!(text_of(src, s_range), "\"a\tb\"");
}

#[test]
fn raw_string_and_escape() {
    let src = r##"fn main() { let s = r#"raw \n string"#; let c = 'x'; }"##;
    let r = ranges(src);
    assert_valid(src, &r);
    assert!(
        r.iter().any(|x| x.group == HighlightGroup::String),
        "raw string / char literal が string: {r:?}"
    );
}
