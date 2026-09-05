# 接続ループと互換性機構の実態調査

検証日: 2026-09-05（wayfinder セッションで直接実施。サブエージェントランナーの環境障害により、本セッションがコード読解で一次確認）。
チケット: 「接続ループと互換性機構の実態調査」（GH issue #44）。
一次ソース: minad/src/daemon.rs・mina-protocol/src/lib.rs・mina-conn/src/lib.rs・minas/src/session.rs（ファイル:行を引用）。

## 1. サーブループと接続処理の構造（daemon.rs）

- `serve(path)`（`daemon.rs:618`）: `bind_listener`（stale socket 除去付き二重起動検知、`daemon.rs:606`）→ `watch::channel<(Option<u64>, StateSnapshot)>` を push 用に生成（`daemon.rs:625`）→ `watch_disk` タスク + `accept_loop` を spawn（`daemon.rs:630`）。
- `accept_loop`（`daemon.rs:808`）: `listener.accept()` → 接続ごとに `conn_id = NEXT_CONN_ID.fetch_add`（`daemon.rs:840`）→ 接続タスクとして `handle_connection` を spawn（`daemon.rs:841-843`）。**接続 = 1 タスク**。
- `handle_connection`（`daemon.rs:857`）: Hello 読み取り（kind / reset_cursor_on_disconnect、`daemon.rs:870`）→ Interactive なら `interactive_clients.insert(conn_id)`（`daemon.rs:886`）→ Interactive のみ `push_tx.subscribe()`（`daemon.rs:892`、既読化で初回偽 push 防止）。
- メインループは `tokio::select!`（`daemon.rs:901-907`）: コマンド行（`lines.next_line()`、`take` で行長上限 — SEC-1）と push 通知（`rx.changed()`）を同時待ち。**read_line は cancel unsafe のため lines() 方式**。

## 2. push（ADR-0013）の配信機構

- チャネル値は `(Option<u64> origin_conn_id, StateSnapshot)`（`daemon.rs:662`）— **push の発信元をタグ**。
- コマンド由来の変更: `process_command` 後、世代が進んだ場合のみ `push_tx.send((Some(conn_id), snapshot.clone()))`（`daemon.rs:1060-1064`）。GetState・拒否・no-op 等の世代不変な応答では送らない。
- デーモン起動の変更（外部リロード = watch_disk、LSP settle）: `(None, snap)` で送り、**全購読者が受ける**（`daemon.rs:792` 等）。
- 受信側: `origin == Some(conn_id)` なら**自発信をスキップ**（応答で既に持つ。JSON 転送節約、`daemon.rs:925-929`）→ それ以外は `ServerMessage::Push { snapshot }` を書く。
- `watch` は最新 1 件のみ保持 — 中間世代の欠落は許容し、フルスナップショットなので必ず最新に収束（`daemon.rs:912-914` のコメント）。

## 3. 接続別状態の全量（分離設計が載せる土台）

現時点で接続別（conn_id 単位）なのは**以下のみ**:

1. `interactive_clients: HashSet<u64>`（`daemon.rs:134`）— 最後の Interactive 切断時のカーソルリセット判定（ADR-0027）
2. `insert_owner: Option<u64>`（UndoGroup 所有権、ADR-0007）— 切断時グループ閉鎖・Mode 復帰（`daemon.rs:495-510`）
3. push の origin タグ（`(Some(conn_id), …)`、`daemon.rs:662/926`）
4. `EventSource::Interactive | Headless`（ClientKind 由来、`daemon.rs:880-881`）— ChangeEvent の source タグ
5. 接続タスクの write_half / push 購読

**それ以外（フォーカス文書・Selection・first_line・Mode・診断・ハイライト…）はすべて単一グローバル Editor が持つ** — `snapshot()` は `daemon.editor` から一括で組み立てる（`daemon.rs:4125` 〜、R2 成果と整合）。「接続別の View」は存在せず、mina-view の複数 View 機構も未使用。

## 4. ヘッドレス（minas）が観測するもの

- ワンショット接続（`mina_conn::open_session` → `request`）は 1 行読んで切断。push は購読しない（headless が push を受けると壊れる、`daemon.rs:889-893` のコメント）。
- `minas session` のパス無し GetState は**グローバルなフォーカス文書の全文**（path / selection / first_line とも単一 Editor 由来）を返す（R2 成果 `minas/src/session.rs:247/253`）。
- 編集系はすべてパス明示（DocumentEdit 等）で Selection 不変。

## 5. 互換性機構

- **PROTOCOL_VERSION はソケット名に埋め込む方式**（`mina-protocol/src/lib.rs:35` `pub const PROTOCOL_VERSION: u32 = 11`、`socket_path()` = `minae-11.sock` を temp_dir に、`lib.rs:50-51`。ADR-0034）。『wire 形式が変わったら必ず上げる』。
- 分離の意味論: バージョン不整合の応答を**一切受けない**（旧 daemon は旧ソケットに残り、新クライアントは新ソケットで新 daemon を自動起動する — `MINAD_EXE` / PATH 検出 + spawn + wait_ready の ensure パターン）。つまり**新旧デーモンは別ソケットで同時に共存でき、ネゴシエーションは不要**という設計。
- 追加コマンドも「後方互換だが古い daemon で動かないため version を上げる」方針（v5〜v11 の履歴コメント、`lib.rs:15-35`）。**機能追加 = version bump = 別ソケット**が一貫した慣習。
- **GetServerInfo**（issue #27）: 軽量応答 `ServerMessage::ServerInfo { generation: MINA_GIT_HASH（option_env、無ければ "unknown"）, daemon_build_ts: MINA_BUILD_TS（無ければ 0）, metrics }`（`daemon.rs:1098-1108`）— 目的は「古いビルドの daemon が新プロトコル項目を黙殺していないか（silent ignore）をクライアント側で検知可能にすること」。読み取り専用（世代・push・イベントを進めない）。

## 6. 分離設計への示唆（決定は「接続別フォーカスの意味論」ほか）

1. **push は「グローバル snapshot のブロードキャスト」**。接続別フォーカスにすると、配信値が接続ごとに異なる（自分の View の snapshot）ため、`(origin, snapshot)` の形から**購読者ごとの合成**に変わる — 配信経路の再設計ポイント。
2. **LSP セッションは同時に 1 文書しか開けない**。GetInlayHints の別パス要求は「対象を didOpen → pull → フォーカス文書を didOpen し直し + 診断再 pull」で対応（`daemon.rs` 1100 行付近、Q10-(c)）。**2 クライアントが別文書にフォーカスする分離後は、フォーカス切り替えのたびに LSP didOpen の入れ替えが起きる** — 「接続別フォーカスの意味論」で LSP フォーカスの扱いを決める必要がある。
3. **互換性の現行解 = version bump によるソケット分離**。既存慣習に従えば、分離拡張も PROTOCOL_VERSION を上げて「新デーモン + 新クライアント」で導入でき、旧クライアントは旧デーモンを自動起動して動き続ける（同時共存可）。「新旧クライアント互換性方針」はこの慣習の延長で設計できる。
4. **watch_disk の外部リロード・LSP settle は origin None の全購読者向け push** — 分離後も「自分のフォーカス文書が外部変更された」通知として接続別に合成されるべきで、None-origin の扱いが引き継がれる。