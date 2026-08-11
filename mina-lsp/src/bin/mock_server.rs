//! テスト用のモック LSP サーバ（stdio 同期版）。
//!
//! - `initialize` には `positionEncoding: "utf-8"` で応答する
//! - `textDocument/didOpen` / `didChange` を受けたら、テキスト内の "TODO" の
//!   位置に error 診断を publish する（テキストが変われば位置も変わる）
//! - 起動引数 `--cjk` で UTF-16 エンコーディングを選び、CJK 文字を診断対象にする

use std::io::{BufRead, BufReader, Read, Write};

use serde_json::{Value, json};

fn main() {
    let utf16 = std::env::args().any(|a| a == "--cjk");
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
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
                } else {
                    msg["params"]["contentChanges"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                };
                // "TODO" の位置に error を publish（テキストが変われば位置が動く）
                let needle = if utf16 { "TODO" } else { "TODO" };
                let start = text.find(needle).map(|i| i as u32).unwrap_or(0);
                let end = start + 4;
                let notif = json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": uri,
                        "diagnostics": [{
                            "range": {
                                "start": { "line": 0, "character": start },
                                "end": { "line": 0, "character": end },
                            },
                            "severity": 1,
                            "message": "mock: TODO found",
                        }],
                    },
                });
                write_frame(&mut stdout, &notif);
            }
        }
    }
}

fn write_frame(out: &mut impl Write, msg: &Value) {
    let data = serde_json::to_vec(msg).expect("JSON");
    out.write_all(format!("Content-Length: {}\r\n\r\n", data.len()).as_bytes())
        .expect("header");
    out.write_all(&data).expect("body");
    out.flush().expect("flush");
}
