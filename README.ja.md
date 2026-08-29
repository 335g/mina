# mina

> エージェントのためのターミナルエディタ。常駐デーモンがエディタ状態を保持し、すべてのフロントエンド —— ヘッドレス session CLI・エージェント・オプションの TUI —— はただのクライアントです。

[English](./README.md) · 日本語

## mina を使う効果

エージェント駆動の編集において、mina の session 契約は素朴なファイルツールより良い計測結果を示します。実用的な2ファイル機能追加タスク（~600行の Rust クレートにフィールドを1つ追加、編集5箇所、`cargo check` で検証、タスク途中に外部ファイル変更を注入）での A/B テスト結果:

| | 素朴なツール（全文 read・無検証の置換） | mina session 契約（範囲 read・検証付き apply） |
|---|---|---|
| 入力トークン（中央値） | 72,851 | 43,088（**−41%**） |
| コスト（中央値） | $0.0456 | $0.0258（**−43%**） |
| 成功率 | 4/5 | 5/5 |

この差を生む契約: 全文 read ではなく番号付きの範囲 read、そしてデーモンの状態に対して検証される内容指定の apply（古いテキストへの編集は具体的な理由つきで拒否され、ファイルを静かに壊さない）。デーモンは外部変更を自動リロードするため、エージェントは古いビューに対して編集しません。

方法論・run 単位のデータ・詳細な考察: [docs/benchmarks/agent-editor-ab-results.md](docs/benchmarks/agent-editor-ab-results.md)（T11/M3）。ハーネス: [`tools/ab/`](tools/ab/README.md)。

## mina とは

mina は daemon/client 分割のターミナルエディタです。エージェント駆動の編集を第一に、対話利用はそれに次ぐ位置づけです。

常駐する **Daemon** がすべてのエディタ状態を保持します —— 開いているドキュメント、undo 履歴、セレクション、LSP セッション。**Client**（ヘッドレス `session` CLI・エージェント・オプションの TUI）はローカルソケットで接続し、コマンドを送って状態スナップショットを描画します。クライアントは出入りしても、デーモンとその状態は残ります。すべてのフロントエンドが同じプロトコルを話すため、エージェントがスクリプトで使うツールは TUI が対話で使うツールと同じであり、TUI はなくても構いません。

## できること

- **編集コア**（UI 非依存）: ドキュメント・セレクション・undo グループ・Normal / Insert / Select モード・検索・外部変更のリロード
- **LSP による言語サポート**（ワークスペースルート単位）: 診断・inlay hints・定義 peek・セマンティック rename / references
- **ヘッドレスエージェントインターフェース**: `mina session` —— 有界な番号付き read（`get --lines`）、ワンショット検証付き編集（`apply`）、位置指定編集（`edit`）、状態遷移の待機（`wait`）、全文なしの hints / peek、セマンティック rename（`rename`）
- **ターミナル UI**（オプション）: `mina open`
- **設定可能**: 言語サーバの `languages.toml`（デーモン側）、`config.toml` とユーザーカラースキーム（クライアント側）、エージェント向けビルトインの `mina skill` ガイド

## はじめに

```console
$ cargo build --release

# TUI でファイルを編集
$ mina open path/to/file.rs

# スクリプトやエージェントからヘッドレスに
$ mina session apply path/to/file.rs "old text" "new text"
```

デーモンはクライアントから必要に応じて起動されます。常駐セッションを明示的に持つには `mina daemon serve` を実行します。

## エージェント向け

```console
# 有界 read: 必要な番号付き行だけ
$ mina session get --lines 1:40

# 検証付き内容指定編集（テキストが一致しなければ理由つきで拒否される）
$ mina session apply src/lib.rs "let old = 1" "let old = 2"

# 世代が進むまでブロック
$ mina session wait 42

# ワークスペースルートをまたぐ LSP rename
$ mina session rename src/lib.rs "USD" "JPY"
```

各コマンドは JSON を返します。編集が拒否されたら該当範囲を読み直して再試行してください —— 拒否メッセージは何が一致しなくなったかを示します。`mina session hints <path>` と `mina session peek <path> <line>:<col>` は全文なしで inlay hints と定義ポップアップを取得します。`mina skill` は read / edit 契約のビルトインガイドを表示します。

## アーキテクチャ

- **Daemon**: ドキュメント・undo 履歴・セレクション・LSP クライアント・構文ハイライトを保持。ローカルソケットでクライアントにサービス提供
- **mina-core**: UI 非依存の編集コア —— ドキュメント・セレクション・トランザクション
- **mina-protocol**: daemon/client IPC のワイヤ型
- **mina-lsp**: LSP クライアント（起動・JSON-RPC・位置変換）
- **mina-view** / **mina-loader**: UI 非依存のエディタ状態 / tree-sitter 文法とハイライトクエリ
- **mina-term**: バイナリクレート —— CLI・デーモン・TUI・ヘッドレス session インターフェース

## ドキュメント

- [CONTEXT.md](CONTEXT.md) — ドメインモデルの正規用語集
- [docs/helix-architecture.md](docs/helix-architecture.md) — アーキテクチャと設計ノート
- [docs/benchmarks/](docs/benchmarks/) — 公開しているエージェントエディタ A/B 計測結果とテスト計画
- [docs/verification/](docs/verification/) — 内部評価ノート（非公開）
- [docs/adr/](docs/adr/) — 意思決定記録
- [docs/spec/](docs/spec/) — ワイヤプロトコル仕様
- [docs/verification/](docs/verification/) — エージェントエディタ A/B 計測結果と評価ノート

mina は開発中です。荒い部分がある前提でお願いします。

## ライセンス

MIT — [LICENSE](LICENSE) を参照。

Copyright (c) 2026 335g