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
async fn server_error_is_not_converted_to_an_empty_result() {
    // ドッグフーディング #2: サーバの JSON-RPC error を null に潰すと「サーバが
    // 拒否した」が「サーバが空を返した」に化け、呼び出し側は原因を失う（実測:
    // rust-analyzer の "Renaming aliases is currently unsupported" が捨てられ、
    // alias を anchor にした rename が 20 回リトライの末に
    // "symbol not found (rename produced no edits)" という実態と違う断定になった）。
    let (mut client, reader) = spawn_client(&[]).await;

    client.request("initialize", json!({})).await.expect("initialize");
    client.notify("initialized", json!({})).await.unwrap();
    client
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": "file:///mock.rs",
                    "languageId": "rust",
                    "version": 1,
                    "text": "let fee = 1;\n\n",
                },
            }),
        )
        .await
        .unwrap();

    // 空行（単語が無い位置）→ モックは -32602 を返す（LSP の意味論では
    // 「その位置にシンボルが無い」= 決定的な拒否。transport 障害ではない）。
    let err = client
        .request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": "file:///mock.rs" },
                "position": { "line": 1, "character": 0 },
                "newName": "x",
            }),
        )
        .await
        .expect_err("サーバのエラーは Err として返る（null に潰さない）");
    assert!(
        err.to_string().contains("No references found at position"),
        "サーバの文言を保持する: {err}"
    );

    client.kill().await;
    reader.abort();
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
async fn cjk_publishes_utf16_character_offsets() {
    // 欠陥の E2E 検証: --cjk の mock は位置を UTF-16 単位で publish する。
    // バイトオフセットのまま publish すると "あTODO" で 3 になるが、
    // 正しくは 1（あ は UTF-8 で3バイト / UTF-16 で1単位）。
    let (mut client, reader) = spawn_client(&["--cjk"]).await;
    let result = client
        .request("initialize", json!({}))
        .await
        .expect("initialize");
    assert_eq!(result["capabilities"]["positionEncoding"], "utf-16");

    client.notify("initialized", json!({})).await.unwrap();
    client
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": "file:///mock_cjk.rs",
                    "languageId": "rust",
                    "version": 1,
                    "text": "あTODO",
                },
            }),
        )
        .await
        .unwrap();

    match wait_notification(&mut client).await {
        Incoming::Notification { method, params } => {
            assert_eq!(method, "textDocument/publishDiagnostics");
            assert_eq!(
                params["diagnostics"][0]["range"]["start"]["character"], 1,
                "UTF-16 単位の位置（あ=1単位、バイトでは3）: {params}"
            );
            assert_eq!(
                params["diagnostics"][0]["range"]["end"]["character"], 5,
                "TODO は4単位: {params}"
            );
        }
    }

    // サロゲートペア（😀=2単位）も正しく数えることを確認
    client
        .notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": "file:///mock_cjk.rs", "version": 2 },
                "contentChanges": [{ "text": "あ😀TODO" }],
            }),
        )
        .await
        .unwrap();
    match wait_notification(&mut client).await {
        Incoming::Notification { params, .. } => {
            assert_eq!(
                params["diagnostics"][0]["range"]["start"]["character"], 3,
                "あ=1単位 + 😀=2単位 で UTF-16 は3（バイトでは7）: {params}"
            );
        }
    }

    client.kill().await;
    reader.abort();
}

#[tokio::test]
async fn notification_channel_is_bounded() {
    // M4: 通知チャネルは bounded。読まないまま容量を超える通知を生成しても
    // キューに残るのは容量ぶんだけ（超過分は破棄され、メモリが成長しない）。
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

    // 容量を超える didChange を送り、publishDiagnostics を flood させる
    // （一切読まないのでキューが満杯になり、以降の通知は破棄される）。
    let flood = mina_lsp::NOTIFICATION_CAPACITY * 2;
    for i in 0..flood {
        client
            .notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": "file:///mock.rs", "version": i + 2 },
                    "contentChanges": [{ "text": "TODO" }],
                }),
            )
            .await
            .unwrap();
    }

    // 後続リクエストの応答は先行する通知がすべて reader に処理された後に返る
    // （フレームは順序どおり処理される）ため、ここでの drain は最終状態を表す。
    client
        .request(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": "file:///mock.rs" } }),
        )
        .await
        .expect("flood 後もリクエストは処理される");

    let mut count = 0;
    while client.try_recv().is_ok() {
        count += 1;
    }
    assert_eq!(
        count,
        mina_lsp::NOTIFICATION_CAPACITY,
        "キューは容量を超えて蓄積しない（超過分は破棄される）: {count}"
    );

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

#[tokio::test]
async fn pull_diagnostics_returns_items() {
    // pull 診断（textDocument/diagnostic）: didOpen 後の状態を items で返し、
    // didChange で位置が追随する。identifier は initialize で advertise される。
    let (mut client, reader) = spawn_client(&[]).await;
    let result = client.request("initialize", json!({})).await.unwrap();
    assert_eq!(
        result["capabilities"]["diagnosticProvider"]["identifier"],
        "mock"
    );
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
    wait_notification(&mut client).await; // push も届く（無視してよい）

    let result = client
        .request(
            "textDocument/diagnostic",
            json!({
                "textDocument": { "uri": "file:///mock.rs" },
                "identifier": "mock",
            }),
        )
        .await
        .expect("pull 診断");
    assert_eq!(result["kind"], "full");
    assert_eq!(result["items"].as_array().unwrap().len(), 1);
    assert_eq!(result["items"][0]["range"]["start"]["character"], 9);
    assert_eq!(result["items"][0]["message"], "mock: TODO found");

    // didChange 後の pull は位置が追随する
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
    wait_notification(&mut client).await;
    let result = client
        .request(
            "textDocument/diagnostic",
            json!({
                "textDocument": { "uri": "file:///mock.rs" },
                "identifier": "mock",
            }),
        )
        .await
        .expect("pull 診断");
    assert_eq!(result["items"][0]["range"]["start"]["character"], 3);

    client.kill().await;
    reader.abort();
}
