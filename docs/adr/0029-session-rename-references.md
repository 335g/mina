# session rename / references: 内容指定の意味リネームと参照列挙（ADR-0029）

エージェント向けに、シンボルの意味リネーム（`textDocument/rename`）と参照列挙（`textDocument/references`）を LSP 経由で提供する。

- **Rename**（`Command::Rename { path, old, new }` / `ServerMessage::RenameResult { generation, files, edits, changed, error }`）: 内容指定。daemon が `old` の最初の「識別子としての」出現を解決し（コメント・文字列内は除外 — tree-sitter）、その位置で LSP rename を実行し、WorkspaceEdit を適用・保存して**影響範囲（ファイル数・編集数・変更ファイル一覧）だけ**を返す。全文は運ばない。
- **References**（`Command::References { path, old }` / `ServerMessage::ReferencesResult { path, locations, total, error }`）: 同じく内容指定で解決し、`includeDeclaration: true` の全参照位置（パス + 0-origin 行）を返す。read-only。
- **プロトコル**: PROTOCOL_VERSION 6 → 7。headless ゲート（`process_command` の制限）の外側、`PeekDefinitionAt` と同じ専用経路（LSP の await は daemon ロック外）。

## なぜ内容指定か

T3 実測（内容指定 apply 3/5 vs 位置指定 edit 0/5・トークン 5 倍）の延長。座標は「計算・陳腐化・曖昧さ」の三重に弱く、位置指定の誤りは「拒否」だけでなく「成功したかのように誤位置へ適用する」事故を生む。LSP rename は位置を要求するが、位置解決は daemon が行い、モデルに計算させない。

## なぜ LSP rename か（apply では代替できない理由）

T5 実測: 出現多数・複数ファイルのリネームで LSP rename 5/5 vs apply ループ 2/5、トークン −49%・コスト −57%。apply は意味解析を持たないため「数え漏れ」（T5-A の失敗様態）が構造的に防げない。

## 適用・検証契約（apply との整合）

- **保存まで**適用する（apply と同じ「1 往復で完結」）。開いている文書はメモリ（Document）に適用して履歴に記録（文書ごとの undo 履歴の invariant を保つ。**リネーム全体は undo 対象外** — ヘッドレスの失敗回復は fresh read → 再適用が実測の正（T3/T7））。開いていないファイルはディスク読み → 書戻し。
- **原子 bunch（拡張）**: 全ファイルの編集を「行がファイルを超える・範囲逆転・範囲重複」の検証を**適用前に全部**済ませ、検証に失敗すればディスク無変更で拒否（apply の「Save 前失敗なら無変更」の複数ファイル版）。適用後の失敗は I/O のみ。
- **リトライ**: rust-analyzer はロード完了前の rename/references に error（ContentModified / "No references found"）や**空の結果**を返す。mina-lsp の Client は LSP error を `Null` に潰すため、結果が「null / 空編集 / 空配列」の間はリトライし、**2 回連続で同一になるまで待つ**（インクリメンタルに増える参照を取りこぼさない — T5 の教訓）。予算 ≈ 10 秒（`SEMANTIC_RETRIES` × 500ms）。予算切れは最後の結果をそのまま返す（参照なしは「0 件」として正直に伝える）。

## 未開ファイルの参照を取りこぼさない（実測必須の didOpen）

M0/M1 実測: **rust-analyzer は didOpen していないファイルの参照を rename/references の結果に含めない**（開いていない main.rs の使用箇所が rename で取りこぼされた）。AB ハーネス（tools/ab）も「ターゲットと同拡張子の全ファイルを didOpen」していた。よってセマンティック要求の前に、ワークスペース（`workspace_root`）内の同拡張子ファイルを didOpen する（`target` / `.git` / `node_modules` 等の生成ディレクトリは除外、400 件 / 64MiB 上限で防護）。開文書の未保存編集はディスクより優先。これによりセッションの `current_uri` が動くため、要求後はフォーカス文書を現在テキストで didOpen し直し+診断を pull して復元する（`restore_focus_after_semantic`。borrows と target==focus の両方。別 root の対象は自己修復に任せる）。

## 失敗の分類（CLI exit コード）

| ケース | 応答 | exit |
|---|---|---|
| 適用成功（no-op 含む） | `error: None`、`renamed: <old> -> <new> (N files, M edits)` | 0 |
| 入力エラー（`rename not supported`・`invalid input`） | `error: Some` | 1（再試行不可） |
| シンボル未解決・LSP エラー・範囲検証失敗・保存失敗 | `error: Some` | 2（再試行可能） |

references も同型（`references not supported` は exit 1、他は 2）。

## 断念した代替

- **tsserver（TypeScript）の追加**: rust-analyzer の rename 不具合（content modified）が T4 で報告されたが、M0 実測で「ワークスペースロード失敗（テストクレートが親 workspace にネスト）＋ settle 不足」が原因と判明し、rust-analyzer 1.98 の rename は内容付きで正常動作。言語追加は `server_for` の拡張のみで対応可能とし、今回のスコープ外。
- **grep fallback**: 取りこぼし（T5）と矛盾するため作らない。対象外言語は明示的に「not supported」。
- **リネームを undo 可能にする（複数ファイル 1 UndoGroup）**: ヘッドレスの回復は再適用が正であり、新機構になるため見送り（Q5）。

## Consequences

- headless ゲートの例外が増える（Rename は読み取り専用でない点で PeekDefinitionAt/Hints と異なる — 編集は daemon 側フェーズで行う）。
- セマンティック要求はワークスペース走査 + 一括 didOpen を伴うため、コールド時は数秒〜10 秒（rust-analyzer のプロジェクトロード待ち）かかる。温まった daemon では数百 ms。
- 別 root の対象を rename した場合、フォーカス文書のセッション診断は次回の Open/編集まで stale になり得る（ADR-0009 と同じ自己修復方針）。
- `uri()` が percent-encode しない既知制限（空白を含むパスの LSP 整合）は引き継がれる（ponytail 注記）。