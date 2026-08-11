//! モック LSP サーバに対するクライアントの統合テスト。
//! （実サーバなしで、spawn→initialize→didOpen→診断受信のパイプラインを検証する）

use std::time::Duration;

use mina_lsp::{Client, Incoming};
use serde_json::json;

async fn spawn_client(args: &[&str]) -> (Client, tokio::task::JoinHandle<()>) {
    let bin = env!("CARGO_BIN_EXE_mock-server");
    Client::spawn(bin, args).await.expect("spawn")
}

async fn wait_notification(client: &mut Client) -> Incoming {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(msg) = client.try_recv() {
                return msg;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("診断通知が来る")
}

#[tokio::test]
async fn initialize_and_publish_diagnostics_round_trip() {
    let (mut client, reader) = spawn_client(&[]).await;

    let result = client
        .request("initialize", json!({}))
        .await
        .expect("initialize");
    assert_eq!(result["capabilities"]["positionEncoding"], "utf-8");

    client.notify("initialized", json!({})).await.unwrap();
    client
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": "file:///mock.rs",
                    "languageId": "rust",
                    "version": 1,
                    "text": "fn f() { TODO }",
                },
            }),
        )
        .await
        .unwrap();

    match wait_notification(&mut client).await {
        Incoming::Notification { method, params } => {
            assert_eq!(method, "textDocument/publishDiagnostics");
            assert_eq!(params["uri"], "file:///mock.rs");
            assert_eq!(params["diagnostics"][0]["severity"], 1);
            assert_eq!(
                params["diagnostics"][0]["range"]["start"]["character"], 9,
                "TODO は char 9 から（fn f() の前は9文字）"
            );
            assert_eq!(params["diagnostics"][0]["message"], "mock: TODO found");
        }
    }

    client.kill().await;
    reader.abort();
}

#[tokio::test]
async fn did_change_moves_diagnostic_position() {
    let (mut client, reader) = spawn_client(&[]).await;
    client.request("initialize", json!({})).await.unwrap();
    client.notify("initialized", json!({})).await.unwrap();
    client
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": "file:///mock.rs",
                    "languageId": "rust",
                    "version": 1,
                    "text": "TODO",
                },
            }),
        )
        .await
        .unwrap();
    wait_notification(&mut client).await;

    // 先頭に文字を足すと診断位置も動く
    client
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": "file:///mock.rs", "version": 2 },
                "contentChanges": [{ "text": "xx TODO" }],
            }),
        )
        .await
        .unwrap();
    match wait_notification(&mut client).await {
        Incoming::Notification { params, .. } => {
            assert_eq!(
                params["diagnostics"][0]["range"]["start"]["character"], 3,
                "位置がずれる: {params}"
            );
        }
    }

    client.kill().await;
    reader.abort();
}

#[tokio::test]
async fn utf16_encoding_is_negotiated() {
    let (mut client, reader) = spawn_client(&["--cjk"]).await;
    let result = client.request("initialize", json!({})).await.unwrap();
    assert_eq!(result["capabilities"]["positionEncoding"], "utf-16");
    client.kill().await;
    reader.abort();
}
