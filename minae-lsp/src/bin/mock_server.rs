//! テスト用のモック LSP サーバ（stdio 同期版）。
//!
//! - `initialize` には `positionEncoding` で応答する（`--cjk` なら `"utf-16"`、それ以外は `"utf-8"`）
//! - `textDocument/didOpen` / `didChange` を受けたら、テキスト内の "TODO" の
//!   位置に error 診断を publish する（テキストが変われば位置も変わる。
//!   `--cjk` 時は位置を UTF-16 単位に変換して publish する）
//! - `textDocument/diagnostic`（pull）には同じ TODO 診断を items で返す

use std::io::{BufRead, BufReader, Read, Write};

use serde_json::{Value, json};

fn main() {
    let utf16 = std::env::args().any(|a| a == "--cjk");
    // --bare: 機能を何も advertise しない「未検証サーバ」を模擬する
    // （Stage 3: 能力ゲートの負の経路をテストするため。--cjk 同様 spawn 引数）。
    let bare = std::env::args().any(|a| a == "--bare");
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
    let mut current: Option<(String, String)> = None; // (uri, text)
    loop {
        // Content-Length フレームを読む
        let mut content_length = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return; // EOF
            }
            let line = line.trim_end().to_string();
            if line.is_empty() {
                break;
            }
            if let Some(len) = line.strip_prefix("Content-Length:") {
                content_length = len.trim().parse::<usize>().ok();
            }
        }
        let len = content_length.expect("Content-Length");
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).expect("body を読む");
        let msg: Value = serde_json::from_slice(&buf).expect("JSON");

        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                "initialize" => {
                    let enc = if utf16 { "utf-16" } else { "utf-8" };
                    // 全機能を advertise（rename / references / definition は実装済み）。
                    // --bare なら positionEncoding と textDocumentSync のみ。
                    let mut caps = json!({
                        "positionEncoding": enc,
                        "textDocumentSync": 1, // full sync
                    });
                    if !bare {
                        caps["diagnosticProvider"] = json!({
                            "identifier": "mock",
                            "interFileDependencies": false,
                            "workspaceDiagnostics": false,
                        });
                        caps["inlayHintProvider"] = json!({});
                        caps["renameProvider"] = json!(true);
                        caps["referencesProvider"] = json!(true);
                        caps["definitionProvider"] = json!(true);
                        caps["documentSymbolProvider"] = json!(true);
                        caps["hoverProvider"] = json!(true);
                        caps["workspaceSymbolProvider"] = json!(true);
                    }
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "capabilities": caps,
                            "serverInfo": { "name": "mock-server" },
                        },
                    });
                    write_frame(&mut stdout, &resp);
                }
                "shutdown" => {
                    let resp = json!({ "jsonrpc": "2.0", "id": id, "result": null });
                    write_frame(&mut stdout, &resp);
                }
                "textDocument/diagnostic" => {
                    // pull 診断: 現在のテキストの TODO 位置を items で返す
                    let diag = current
                        .as_ref()
                        .map(|(_, text)| todo_diagnostic(text, utf16));
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "kind": "full",
                            "resultId": "mock",
                            "items": diag.map(|d| vec![d]).unwrap_or_default(),
                        },
                    });
                    write_frame(&mut stdout, &resp);
                }
                "textDocument/inlayHint" => {
                    // pull inlay hint: 固定パターンのヒント配列を返す
                    let hints = current
                        .as_ref()
                        .map(|(_, text)| inlay_hints(text, utf16))
                        .unwrap_or_default();
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": hints,
                    });
                    write_frame(&mut stdout, &resp);
                }
                "textDocument/definition" => {
                    // 定義: 現在の文書内の最初の "fn " の位置を Location で返す
                    // （PeekDefinition の確認用。カーソル位置は解析しない）。
                    let loc = current.as_ref().map(|(uri, text)| {
                        let start = text.find("fn ").unwrap_or(0);
                        let line = text[..start].matches('\n').count() as u32;
                        let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
                        let char = if utf16 {
                            text[line_start..start].encode_utf16().count() as u32
                        } else {
                            (start - line_start) as u32
                        };
                        json!({
                            "uri": uri,
                            "range": {
                                "start": { "line": line, "character": char },
                                "end": { "line": line, "character": char + 5 },
                            },
                        })
                    });
                    let resp = json!({ "jsonrpc": "2.0", "id": id, "result": loc });
                    write_frame(&mut stdout, &resp);
                }
                "textDocument/documentSymbol" => {
                    // 簡易アウトライン: 行ごとに `fn NAME` / `struct NAME` / `let NAME`
                    // を走査して DocumentSymbol 配列（kind・range・selectionRange）を
                    // 返す。階層（children）は持たない — 木構造の変換は lsp.rs の
                    // ユニットテストで検証する。範囲は実測が必要な輪郭だけ正確に
                    // （行全体が range・名前トークンが selectionRange）。
                    let symbols = current
                        .as_ref()
                        .map(|(_, text)| outline_symbols(text, utf16))
                        .unwrap_or_default();
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": symbols,
                    });
                    write_frame(&mut stdout, &resp);
                }
                "textDocument/rename" => {
                    // 意味リネームのモック: 要求位置の単語を特定し、現在文書内の
                    // 全出現を newName に置き換える WorkspaceEdit（documentChanges 形式
                    // — 実測で rust-analyzer が返す形式）を返す。位置に単語が
                    // 無ければ LSP error（解析待ち/シンボルなしの模擬）。
                    let Some((uri, text)) = current.as_ref() else {
                        let resp = json!({ "jsonrpc": "2.0", "id": id, "result": null });
                        write_frame(&mut stdout, &resp);
                        continue;
                    };
                    let pos = &msg["params"]["position"];
                    let line = pos["line"].as_u64().unwrap_or(0) as u32;
                    let character = pos["character"].as_u64().unwrap_or(0) as u32;
                    let new_name = msg["params"]["newName"].as_str().unwrap_or("");
                    if let Some(occ) = occurrences_at(text, line, character, utf16) {
                        let edits = occ
                            .iter()
                            .map(|(l, bs, be)| json!({
                                "range": {
                                    "start": { "line": l, "character": lsp_char_col_at(text, *l, *bs, utf16) },
                                    "end": { "line": l, "character": lsp_char_col_at(text, *l, *be, utf16) },
                                },
                                "newText": new_name,
                            }))
                            .collect::<Vec<_>>();
                        let resp = json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {
                                "documentChanges": [{
                                    "textDocument": { "uri": uri, "version": 1 },
                                    "edits": edits,
                                }],
                            },
                        });
                        write_frame(&mut stdout, &resp);
                    } else {
                        let resp = json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": -32602, "message": "No references found at position" },
                        });
                        write_frame(&mut stdout, &resp);
                    }
                }
                "textDocument/references" => {
                    let Some((uri, text)) = current.as_ref() else {
                        let resp = json!({ "jsonrpc": "2.0", "id": id, "result": [] });
                        write_frame(&mut stdout, &resp);
                        continue;
                    };
                    let pos = &msg["params"]["position"];
                    let line = pos["line"].as_u64().unwrap_or(0) as u32;
                    let character = pos["character"].as_u64().unwrap_or(0) as u32;
                    let locs = occurrences_at(text, line, character, utf16)
                        .unwrap_or_default()
                        .iter()
                        .map(|(l, bs, be)| json!({
                            "uri": uri,
                            "range": {
                                "start": { "line": l, "character": lsp_char_col_at(text, *l, *bs, utf16) },
                                "end": { "line": l, "character": lsp_char_col_at(text, *l, *be, utf16) },
                            },
                        }))
                        .collect::<Vec<_>>();
                    let resp = json!({ "jsonrpc": "2.0", "id": id, "result": locs });
                    write_frame(&mut stdout, &resp);
                }
                "textDocument/hover" => {
                    // hover のモック: 位置の単語を返す（無ければ null = hover なし）。
                    // rust-analyzer と同じ配列形（先頭 = 型シグネチャの MarkedString、
                    // 続いて doc の MarkupContent）で返す。
                    let hover = current.as_ref().and_then(|(_, text)| {
                        let pos = &msg["params"]["position"];
                        let line = pos["line"].as_u64().unwrap_or(0) as u32;
                        let character = pos["character"].as_u64().unwrap_or(0) as u32;
                        word_at(text, line, character, utf16).map(|w| json!({
                            "contents": [
                                { "language": "rust", "value": format!("fn {w}() -> i32") },
                                { "kind": "markdown", "value": format!("mock doc for {w}") },
                            ],
                        }))
                    });
                    let resp = json!({ "jsonrpc": "2.0", "id": id, "result": hover });
                    write_frame(&mut stdout, &resp);
                }
                "workspace/symbol" => {
                    // workspace/symbol のモック: 現在文書のアウトラインから名前が
                    // クエリを含むシンボルを SymbolInformation[] で返す。
                    let query = msg["params"]["query"].as_str().unwrap_or("");
                    let symbols = current
                        .as_ref()
                        .map(|(uri, text)| workspace_symbol_info(text, uri, query, utf16))
                        .unwrap_or_default();
                    let resp = json!({ "jsonrpc": "2.0", "id": id, "result": symbols });
                    write_frame(&mut stdout, &resp);
                }
                _ => {}
            }
        } else if let Some(method) = msg.get("method").and_then(Value::as_str) {
            if method == "textDocument/didOpen" || method == "textDocument/didChange" {
                let uri = msg["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let text = if method == "textDocument/didOpen" {
                    msg["params"]["textDocument"]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                } else {
                    msg["params"]["contentChanges"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                };
                current = Some((uri.clone(), text.clone()));
                let notif = json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": { "uri": uri, "diagnostics": vec![todo_diagnostic(&text, utf16)] },
                });
                write_frame(&mut stdout, &notif);
            }
        }
    }
}

/// テキスト内の "TODO" 位置の error 診断（LSP 座標は UTF-8 バイト or UTF-16 単位）。
fn todo_diagnostic(text: &str, utf16: bool) -> Value {
    let needle = "TODO";
    let start_byte = text.find(needle).unwrap_or(0);
    // LSP の character は行頭からの UTF-16 単位（--cjk 時は utf-16 を advertise）。
    let start = if utf16 {
        text[..start_byte].encode_utf16().count() as u32
    } else {
        start_byte as u32
    };
    let end = start + 4; // "TODO" は ASCII なので UTF-16 でも 4 単位
    json!({
        "range": {
            "start": { "line": 0, "character": start },
            "end": { "line": 0, "character": end },
        },
        "severity": 1,
        "source": "mock",
        "message": "mock: TODO found",
    })
}

/// テキスト内の固定パターン inlay hint（LSP 座標）:
/// - `let NAME ...` の NAME の直後に type ヒント `: i32`（padding なし —
///   rust-analyzer の bind_pat と同じ `pad_left = !render_colons`, `pad_right = false`）
/// - `foo(` の直後の引数の直前に parameter ヒント `arg:`（右 padding —
///   rust-analyzer の param_name と同じ）
///
/// 位置は advertise した encoding の単位（`--cjk` なら UTF-16）。
fn inlay_hints(text: &str, utf16: bool) -> Vec<Value> {
    let mut hints = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        // type ヒント: `let NAME` の NAME の直後
        if let Some(let_pos) = line.find("let ") {
            let rest = &line[let_pos + 4..];
            let name_end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if name_end > 0 {
                hints.push(hint_value(
                    line_idx as u32,
                    lsp_char_col(line, let_pos + 4 + name_end, utf16),
                    ": i32",
                    1, // Type
                    false,
                    false,
                ));
            }
        }
        // parameter ヒント: `foo(` の直後の引数の直前に `arg:`
        if let Some(open) = line.find("foo(") {
            hints.push(hint_value(
                line_idx as u32,
                lsp_char_col(line, open + 4, utf16),
                "arg:",
                2, // Parameter
                false,
                true,
            ));
        }
    }
    hints
}

/// 簡易アウトライン（[`textDocument/documentSymbol`] のモック応答）:
/// 行ごとに `fn NAME` / `struct NAME` / `let NAME` を走査し、DocumentSymbol 配列を
/// 返す。range は行全体、selectionRange は名前トークン。kind: 12=Function /
/// 23=Struct / 13=Variable。階層（children）は持たない — 入れ子変換は
/// minae-term の lsp.rs ユニットテストで検証する。
fn outline_symbols(text: &str, utf16: bool) -> Vec<Value> {
    let mut out = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        for (kw, kind) in [("fn ", 12u64), ("struct ", 23u64), ("let ", 13u64)] {
            let Some(kw_pos) = line.find(kw) else {
                continue;
            };
            let name_start = kw_pos + kw.len();
            let name_end = line[name_start..]
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .map_or(line.len(), |e| name_start + e);
            if name_start == name_end {
                continue;
            }
            out.push(json!({
                "name": &line[name_start..name_end],
                "kind": kind,
                "range": {
                    "start": { "line": line_idx as u32, "character": 0 },
                    "end": { "line": line_idx as u32, "character": lsp_char_col(line, line.len(), utf16) },
                },
                "selectionRange": {
                    "start": { "line": line_idx as u32, "character": lsp_char_col(line, name_start, utf16) },
                    "end": { "line": line_idx as u32, "character": lsp_char_col(line, name_end, utf16) },
                },
            }));
        }
    }
    out
}

/// workspace/symbol のモック応答（SymbolInformation[]）: 現在文書のアウトライン
/// から、名前が `query` を部分一致で含むシンボルを `location` 付きで返す。
fn workspace_symbol_info(text: &str, uri: &str, query: &str, utf16: bool) -> Vec<Value> {
    outline_symbols(text, utf16)
        .into_iter()
        .filter(|s| {
            let name = s.get("name").and_then(Value::as_str).unwrap_or("");
            query.is_empty() || name.contains(query)
        })
        .map(|s| {
            let name = s.get("name").cloned().unwrap_or(Value::Null);
            let kind = s.get("kind").cloned().unwrap_or(Value::Null);
            let line = s.pointer("/range/start/line").and_then(Value::as_u64).unwrap_or(0);
            let end = s.pointer("/range/end/character").and_then(Value::as_u64).unwrap_or(0);
            json!({
                "name": name,
                "kind": kind,
                "location": {
                    "uri": uri,
                    "range": {
                        "start": { "line": line, "character": 0 },
                        "end": { "line": line, "character": end },
                    },
                },
            })
        })
        .collect()
}

/// 行内バイト列 → LSP の character（advertise した encoding の単位）。
fn lsp_char_col(line: &str, byte_off: usize, utf16: bool) -> u32 {
    let byte_off = byte_off.min(line.len());
    if utf16 {
        line[..byte_off].encode_utf16().count() as u32
    } else {
        byte_off as u32
    }
}

fn hint_value(
    line: u32,
    character: u32,
    label: &str,
    kind: u32,
    padding_left: bool,
    padding_right: bool,
) -> Value {
    json!({
        "position": { "line": line, "character": character },
        "label": label,
        "kind": kind,
        "paddingLeft": padding_left,
        "paddingRight": padding_right,
    })
}

fn write_frame(out: &mut impl Write, msg: &Value) {
    let data = serde_json::to_vec(msg).expect("JSON");
    out.write_all(format!("Content-Length: {}\r\n\r\n", data.len()).as_bytes())
        .expect("header");
    out.write_all(&data).expect("body");
    out.flush().expect("flush");
}

/// LSP 座標（行・encoding 単位の列）を char 列に変換して行内の位置を特定し、
/// その位置にある識別子（単語）を返す。無ければ None。
fn word_at(text: &str, line: u32, character: u32, utf16: bool) -> Option<String> {
    let line_text = text.lines().nth(line as usize)?;
    let char_col = if utf16 {
        // UTF-16 単位 → char 列（ASCII 中心のテスト fixture なので 1:1 だが
        // 一般化しておく: サロゲートペアを数える）
        let mut units = 0u32;
        let mut col = 0usize;
        for ch in line_text.chars() {
            let w = ch.len_utf16() as u32;
            if units + w > character {
                break;
            }
            units += w;
            col += 1;
        }
        col
    } else {
        character as usize
    };
    let mut start = char_col.min(line_text.len());
    let mut end = start;
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    while start > 0 && is_word(line_text.as_bytes()[start - 1]) {
        start -= 1;
    }
    while end < line_text.len() && is_word(line_text.as_bytes()[end]) {
        end += 1;
    }
    if start == end {
        None
    } else {
        Some(line_text[start..end].to_string())
    }
}

/// テキスト内の単語の全出現（行・開始 byte・終了 byte）。指定位置に単語が
/// 無ければ None。
fn occurrences_at(
    text: &str,
    line: u32,
    character: u32,
    utf16: bool,
) -> Option<Vec<(u32, usize, usize)>> {
    let word = word_at(text, line, character, utf16)?;
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find(&word) {
        let i = search_from + rel;
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_';
        let after = i + word.len();
        let after_ok = after == bytes.len()
            || !bytes[after].is_ascii_alphanumeric() && bytes[after] != b'_';
        if before_ok && after_ok {
            let l = text[..i].matches('\n').count() as u32;
            out.push((l, i, after));
        }
        search_from = after;
    }
    Some(out)
}

/// 行・byte 位置 → LSP の character（advertise した encoding の単位）。
fn lsp_char_col_at(text: &str, line: u32, byte_off: usize, utf16: bool) -> u32 {
    let line_text = text.lines().nth(line as usize).unwrap_or("");
    let byte_in_line = byte_off.saturating_sub(
        text.split('\n')
            .take(line as usize)
            .map(|l| l.len() + 1)
            .sum::<usize>(),
    );
    let byte_in_line = byte_in_line.min(line_text.len());
    if utf16 {
        line_text[..byte_in_line].encode_utf16().count() as u32
    } else {
        byte_in_line as u32
    }
}
