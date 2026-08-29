# Syntax の計算・保持は Daemon が所有し、Document への乗せは放棄する

`minae-core` の `Document` (document.rs) は「テキスト以外のプロパティ（構文・診断・編集履歴）は実際に必要なステップが来た時点でここに追加する」と想定している。しかし診断は daemon のフィールド、編集履歴は minae-view の `History` に置かれる前例が既にあり、`lib.rs` は「core はターミナル・キーマップ・構文ハイライトについて何も知らない」と明言している。そこで tree-sitter のパース結果とグループ割り当て（Syntax）は LSP 解析と同じく **Daemon が DocumentId キーで所有** する。interactive / headless (`DocumentEdit`) / 外部リロード (ADR-0015) の**全編集源**でトランザクション適用のたびにインクリメンタル再パースし、結果は StateSnapshot のハイライト範囲列として配信する。

## 動作契約

- Syntax 状態は Document に従属する: Open で初期化、Close で破棄、Reload で再パース。
- スナップショット内のハイライト範囲は、同じスナップショットのテキストと常に一致する（不変条件）。
- `minae-core` は tree-sitter に依存しない。

## 検討した代替案

- **minae-core の Document 上に保持**: document.rs の想定フック通りだが、core に tree-sitter 依存が入り、lib.rs の純粋性宣言を書き換えることになる。機能コア (ADR-0002) のテスト容易性を分析状態で汚す。
- **クライアント側でパース**: プロトコル変更なし・daemon 負荷なしだが、参照設計 (docs/helix-architecture.md item 6) の構造編集 (AST ベースの移動・選択・テキストオブジェクト) が daemon 側に別実装を要求する。全編集源で一貫した状態はクライアントでは保てない。

## 帰結

- document.rs の想定フック（構文を Document に載せる）は正式に放棄する（実装時に該当コメントを更新する）。
- StateSnapshot に新フィールド `highlights` が加わる（仕様: docs/spec/syntax-highlighting.md）。
- 将来の構造編集は同じ Syntax 状態をそのまま利用できる。
