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
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "capabilities": {
                                "positionEncoding": enc,
                                "textDocumentSync": 1, // full sync
                                "diagnosticProvider": {
                                    "identifier": "mock",
                                    "interFileDependencies": false,
                                    "workspaceDiagnostics": false,
                                },
                            },
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

fn write_frame(out: &mut impl Write, msg: &Value) {
    let data = serde_json::to_vec(msg).expect("JSON");
    out.write_all(format!("Content-Length: {}\r\n\r\n", data.len()).as_bytes())
        .expect("header");
    out.write_all(&data).expect("body");
    out.flush().expect("flush");
}
