# minas read: パス指定の軽量テキスト読み(バッファ非依存)

第2回検証(docs/gitignore/study/minas-debug-study.md)と第3回検証
(docs/gitignore/study/minas-hypothesis-study.md H1)で指摘: `minas get` は
**パス指定不可・バッファ依存**で、読めるのは「最後に Open / apply で開いた
文書」だけ。outline / at / hover / peek / symbol / references / check は
任意パスを取るのに、**テキストを読む唯一のコマンド `get` だけがバッファ
依存**という操作非対称があり、任意ファイルの行を読みたいのに apply(書込み)
が必要になる。多ファイル検証ループでは「読みたいファイルを読むのに編集
コマンドを使う」歪みが実害になる(H1 ✓ 実測)。

Status: accepted

## Decision

**`minas read <path> [--lines <start>:<end>]`** を追加する — パス指定・
バッファ非依存・LSP 非依存の軽量テキスト読み。

- プロトコル: `Command::ReadPath { path: String }` 追加。応答は全文
  スナップショット(`StateSnapshot`)を運ばない軽量
  `ServerMessage::ReadPath { path, generation, text, error }` —
  ADR-0031 / 0032 の軽量応答と同じ思想(エージェントが読む量を節約)。
- **LSP に触れない**: 純粋なテキスト読みのため didOpen 不要。テキスト解決は
  既存の `resolve_doc_text`(開文書優先・ディスク読み — prepare_borrowed_session
  前半と同じ形)を再利用。開文書は未保存編集込みのテキストが読める。
- **状態を変えない**: フォーカス・idle view・generation・push・イベントは
  一切動かさない(読み取り専用)。`get` のバッファ依存と apply 経由の歪みを
  両方解消する。
- `--lines` は **CLI 側で処理**(get の `print_line_range` と同じ方式):
  daemon は全文を返し、CLI が範囲を切って番号付きで出力する。ローカル
  ソケット転送は無料で、節約は「LLM が読む量」で成立する(既存 get と同設計)。
- メトリクス: `ServerMetrics` に `read_total` / `read_bytes` を追加
  (issue #27 と同じ効果検証の目的 — 「全文を読まなくて済んだ量」を観測)。
  get_state_total(全文再読の近似)と対比できる。
- headless ゲート: 既存の読み取り専用コマンド(outline / peek / check 等)と
  同じく `handle_connection` の専用分岐で処理し、process_command の
  headless ゲートは変更しない。

## Considered Options

- `get` 自体にパス引数を追加(`Command::GetState { path: Option<String> }`):
  却下 — 応答が StateSnapshot(全文 + ハイライト + 診断 + inlay hints)のまま
  で重く、「範囲 read で 1 行だけ」の用途に合わない。get は現状のまま
  「現在の状態スナップショット」に特化させ、read を別コマンドにする方が
  応答形状も意味も明快。既存クライアントへの影響もゼロ。
- Open を読み取り専用化するフラグ付きで使い回す: 却下 — Open はフォーカス
  移動・外部変更検知・baseline 管理など読み取り専用に不要な副作用を伴い、
  「読む」意図が曖昧になる。専用コマンドの方が契約が明確。
- tree-sitter での読み: 却下 — 読みは編集前の「対象を把握する」操作で、
  テキストそのものが必要。構文解析は無関係。

## Consequences

- エージェントは `minas read <path> --lines 10:20` で任意ファイルを
  バッファを汚さず読める。多ファイル検証ループで apply を読みに使う歪みが
  消える。
- `get` は現状維持(後方互換)。read と get の役割分担が明確になる:
  **read = 任意パスの軽量読み / get = 現在の状態(全文)スナップショット**。
- skill の read トピックに read コマンドを追記し、「get は現在開いている
  文書のみ」を明記する。
- PROTOCOL_VERSION を 17 → 18 に上げる(コマンド + 応答の追加。古い daemon に
  新コマンドを送っても動作しないため — v8 / v9 と同じ bump 流儀、ADR-0039)。