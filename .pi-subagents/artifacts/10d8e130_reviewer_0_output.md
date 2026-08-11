レビュー完了。以下、Issue #1 と実装の突き合わせ結果です。

## Review

### 観点1: アーキテクチャ決定 — 適合

| ADR | 実装 | 判定 |
|---|---|---|
| ADR-0005 daemon/client 分割 | daemon が `Editor` を所有（daemon.rs:20-31 `Daemon`）、クライアントは Command 送信/StateSnapshot 受信のみ（client.rs:14-17）。自動起動は `ensure_daemon` + `setsid` 離脱（client.rs:144-162、emacsclient 型）。`mina daemon serve` 明示起動も可（main.rs:16-17） | 適合 |
| ADR-0006 unix socket + NDJSON、リクエスト/レスポンスのみ、毎レスポンス全量 | `handle_connection` が read_line で1行 JSON を読み、`snapshot()` を1行で返す（daemon.rs:60-130）。push チャネルなし。診断は「次のスナップショットに含める」（lsp.rs:5-7） | 適合 |
| ADR-0004 termina 0.3 | Cargo.lock で termina 0.3.3、client.rs で `PlatformTerminal`/`EventStream` を使用 | 適合 |
| キーマップはクライアント側のみ解決 | keymap.rs は mina-term 内にあり、daemon 側にキー処理なし。モード別 prefix トライ（keymap.rs:10-40 `Node`） | 適合 |

### 観点2: クレート構成 — 適合
- **mina-protocol（新・依存なし）**: 依存は serde のみ（dev-dep に serde_json）。`Command`/`StateSnapshot`/`Diagnostic` すべて定義（lib.rs:10-117）。
- **mina-lsp（新）**: spawn・JSON-RPC（Content-Length フレーム）・UTF-16 位置変換・診断受信（lib.rs、position.rs）。
- **mina-term（新・bin "mina"）**: client / daemon / session の3モード（main.rs:15-22）。workspace メンバーも Issue 記載の5 crate と一致（Cargo.toml）。

### 観点3: スライス完了条件 — 適合
- **S0**: GetState → StateSnapshot 往復（client.rs:41-43、daemon.rs `GetState => snapshot`）。protocol に round-trip テストあり。
- **S1**: render.rs の描画、keymap.rs のトライ解決、Move/Scroll、`scroll_to_cursor`（daemon.rs:200-216）。
- **S2**: Insert/DeleteBackward/DeleteForward/DeleteRange/Undo/Redo/Save すべて実装（daemon.rs:158-199、Save は daemon.rs:107-125 でロック外書き込み）。dirty/path 管理は mina-view editor.rs に追加（`paths`/`dirty`/`is_dirty`/`mark_saved`）。Insert グループ undo（SetMode の begin/end_group, daemon.rs:264-276）。
  - 注記: 「save/quit」の quit はプロトコルに Quit コマンドがなく、クライアント側の TUI 終了のみ（client.rs:87-90、Ctrl-C/q）。daemon モデルでは整合（daemon 常駐はスコープ外「明示停止」と一致）であり、完了条件（編集→保存→undo）は満たす。
- **S3**: `server_for` は .rs → rust-analyzer のみ（lsp.rs:30-37）。Open 時のみ `ensure` で spawn（daemon.rs:93-104、eager なし）。initialize → didOpen → 編集時 didChange 全文同期 → publishDiagnostics を drain して snapshot に載せる（lsp.rs:147-156）。ステータス行カウント `[NE NW]`（render.rs:231-235）と診断範囲の下線マーカー（render.rs:262-272）。
- **S4**: `mina session get/exec`（session.rs:24-47）、JSON エラー時 exit 1、成功時 pretty JSON 出力。

### 観点4: v1 スコープ外の漏れ込み — 適合（Note 1件）
- マウス: イベントループは Key/WindowResized のみで他は `continue`（client.rs:79-109）。漏れなし。
- 差分スナップショット: なし（毎回全量）。
- LSP設定ファイル: 組み込みテーブル（lsp.rs:30-37、「設定ファイル化はサーバが増えてから」）。
- 非UTF-8: `read_to_string` 失敗 → status に報告（daemon.rs:87, 105）。
- スプリット: mina-view に `Editor::split` が既存（editor.rs:171-180）だが protocol に Command がなく未露出。Issue の「必要になったら載せられる」と一致。
- daemon 明示停止: コマンドなし（常駐）。
- hover/goto/補完: `drain_diagnostics` は publishDiagnostics 以外を破棄（lsp.rs:150-152）。
- **Note**: 複数TUIの「機能」（View 所有など）は未実装だが、`accept_loop`（daemon.rs:58-64）は接続を拒否しないため、複数クライアントが同時接続すると排他なしで共有状態を操作し得る。v1 の割り切りとしては許容範囲だが、下記観点5の未記録と関連。

### 観点5: 実装上の割り切りの ponytail 記録 — 部分不適合

| Issue 記載の割り切り | 記録 |
|---|---|
| 毎レスポンス全量（O(n)/キー） | ✓ daemon.rs:68 `ponytail: 接続ごとに全状態スナップショットを返す（O(n)/コマンド）` |
| socket パス単一ユーザ（temp_dir/mina.sock） | ✓ daemon.rs:361 `ponytail: uid を入れていない（単一ユーザ前提）…`（daemon.rs:6 にも言及） |
| **UTF-8 のみのファイル I/O** | ✗ **記録なし**。daemon.rs:87 `read_to_string` にコメントなし。grep で mina-term/mina-protocol/docs に該当 ponytail コメントなし |
| **事実上 1 クライアント前提** | ✗ **記録なし**。最接近は client.rs:138-140 の「同時に2つのクライアントが spawn した場合…（v1 の割り切り）」だが、これは自動起動の競合についてで、1クライアント前提の言及ではない。daemon.rs・docs/ にも記録なし |

### 観点6: 確定済み設計判断との矛盾 — 適合
「毎レスポンスで StateSnapshot 全体」「コマンド語彙は TUI/agent で共有（session exec が wire の Command を直接受ける）」「LSP は eager spawn しない」「daemon は落とすまで常駐」すべて実装と一致。矛盾する実装は見つからなかった。

### 検証コマンド
- `gh issue view 1` — スペック本文確認（passed）
- `cargo test --workspace` — 118 passed（10 suites）（passed。mina-lsp の mock サーバ統合テスト3件含む）
- `cargo build --workspace` — passed

## 総合判定: **部分一致**

機能面（観点1-4, 6）は Issue のスペックを満たすが、Issue が「コードに `ponytail:` で記録する」と明記した4項目のうち **2項目（UTF-8 のみの I/O、事実上1クライアント前提）がコードに未記録**。ブロッカー級の機能欠落・スコープ外漏れ込みはなし。

## 残存リスク
- daemon.rs:90-104 の Open ハンドラは daemon ロックを握ったまま `lsp::ensure`（initialize、最大10秒タイムアウト）を await する。他クライアントがブロックされ得る（1クライアント前提の帰結。コメント未記録のため将来の誤用リスク）。
- 同時接続の複数クライアントは拒否されず共有状態を操作可能（観点4 Note）。
- S2 の「quit」はプロトコルコマンドでなくクライアント側終了のみ（daemon モデルと整合だが、Issue のスライス記載と字面が異なる）。