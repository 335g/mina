# Inlay hints: daemon 所有・TUI 表示とエージェント API

LSP の inlay hint（type / parameter の 2 種）を TUI に表示し、headless エージェントが全文テキストを読まずに型構造を参照できるようにする（LLM コンテキスト削減）。

- ヒントは daemon 所有（診断・構文ハイライトと同じ経路）。編集後 250ms settle（診断と同経路）と Open 時の settle ループで全文 pull し、`StateSnapshot.inlay_hints` に載せて TUI へ届ける。パスキーの checksum キャッシュ（上限付き evict）を持ち、stale ヒントは新ヒント到着まで表示を保持する
- 新しい読み取り専用 `Command::GetInlayHints { path }`（headless 許可リスト追加）が、任意パスのヒントをテキストなしで返す。Editor に開いていなければディスク読み（SEC-1 検証）して LSP に didOpen する
- **LSP セッションは同時に 1 文書しか開けない**（didOpen が前の文書を閉じる）。エージェント要求時は「対象文書へ切り替え → pull → フォーカス文書を現在テキストで didOpen し直し、診断も再 pull」で対応する。切り替えウィンドウ中のフォーカス文書のライブ更新停止は、復元と診断再 pull で自己修復する（Q10 の (a) 切り替えのみ / (b) キャッシュ温存のみ を reject: (a) は復元まで古い診断が残り、(b) はエージェントが先に TUI で開く必要があり使いにくい）
- 表示: ヒントは仮想テキストとして位置に挟み込む（Document の一部にしない — CONTEXT.md の InlayHint 定義）。専用 `UiRole::InlayHint`、`paddingLeft/Right` はサーバ指定を尊重、kind 別の色分けはしない。端末カーソル列はヒント幅を加算する

## Consequences

- wire 変更（新 Command / 新 ServerMessage::Hints / `StateSnapshot.inlay_hints` / 新 `InlayHint` 型）→ PROTOCOL_VERSION 2 → 3
- ツールチップ（`inlayHint/resolve`）・kind 別表示・トグルは対象外（必要になったら足す）
