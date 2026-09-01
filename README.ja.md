# minae

> エージェントのためのターミナルエディタ。常駐デーモンがエディタ状態を保持し、すべてのフロントエンド —— ヘッドレス session CLI・エージェント・オプションの TUI —— はただのクライアントです。

**minae** = **min**(imize)（コスト最小化）+ **A**gent + **E**ditor —— コストを最小化するエージェントエディタ。

[English](./README.md) · 日本語

## minae を使う効果

エージェント駆動の編集において、minae の session 契約は素朴なファイルツールより良い計測結果を示します。実用的な2ファイル機能追加タスク（~600行の Rust クレートにフィールドを1つ追加、編集5箇所、`cargo check` で検証、タスク途中に外部ファイル変更を注入）での A/B テスト結果:

| | 素朴なツール（全文 read・無検証の置換） | minae session 契約（範囲 read・検証付き apply） |
|---|---|---|
| 入力トークン（中央値） | 72,851 | 43,088（**−41%**） |
| コスト（中央値） | $0.0456 | $0.0258（**−43%**） |
| 成功率 | 4/5 | 5/5 |

この差を生む契約: 全文 read ではなく番号付きの範囲 read、そしてデーモンの状態に対して検証される内容指定の apply（古いテキストへの編集は具体的な理由つきで拒否され、ファイルを静かに壊さない）。デーモンは外部変更を自動リロードするため、エージェントは古いビューに対して編集しません。

方法論・run 単位のデータ・詳細な考察: [docs/benchmarks/agent-editor-ab-results.md](docs/benchmarks/agent-editor-ab-results.md)（T11/M3）。ハーネス: [`tools/ab/`](tools/ab/README.md)。

## minae とは

minae は daemon/client 分割のターミナルエディタです。エージェント駆動の編集を第一に、対話利用はそれに次ぐ位置づけです。

常駐する **Daemon** がすべてのエディタ状態を保持します —— 開いているドキュメント、undo 履歴、セレクション、LSP セッション。**Client**（ヘッドレス `session` CLI・エージェント・オプションの TUI）はローカルソケットで接続し、コマンドを送って状態スナップショットを描画します。クライアントは出入りしても、デーモンとその状態は残ります。すべてのフロントエンドが同じプロトコルを話すため、エージェントがスクリプトで使うツールは TUI が対話で使うツールと同じであり、TUI はなくても構いません。

## できること

- **編集コア**（UI 非依存）: ドキュメント・セレクション・undo グループ・Normal / Insert / Select モード・検索・外部変更のリロード
- **LSP による言語サポート**（ワークスペースルート単位）: 診断・inlay hints・定義 peek・セマンティック rename / references
- **言語サーバ**（ADR-0030 に基づく検証済みの埋め込み）: Rust に `rust-analyzer`、TypeScript に `typescript-language-server` —— 同じ2言語分の tree-sitter 構文ハイライトも同梱。ほかの LSP はユーザーの `languages.toml` で追加可能（未検証サーバも標準機能はネゴシエーションで動作）
- **ヘッドレスエージェントインターフェース**: `minae session` —— 有界な番号付き read（`get --lines`）、ワンショット検証付き編集（`apply`）、位置指定編集（`edit`）、状態遷移の待機（`wait`）、全文なしの hints / peek、セマンティック rename（`rename`）、構造把握（`outline`）と位置解決（`at`）
- **ターミナル UI**（オプション）: `minae open`
- **設定可能**: 言語サーバの `languages.toml`（デーモン側）、`config.toml` とユーザーカラースキーム（クライアント側）、エージェント向けビルトインの `minae skill` ガイド

## はじめに

```console
# 公開クレート（`minae` バイナリをビルド。daemon・TUI・session CLI が1つに入っている）
$ cargo install minae

# TUI でファイルを編集（デーモンは自動起動）
$ minae open path/to/file.rs

# スクリプトやエージェントからヘッドレスに
$ minae session apply path/to/file.rs "old text" "new text"
```

ソースからは `cargo build --release` でもビルドできます。言語サーバ（rust-analyzer、typescript-language-server など）は同梱されないため、LSP 機能を使う場合は別途インストールしてください。tree-sitter による構文ハイライトはそのまま動きます。

デーモンはクライアントから必要に応じて起動されます。常駐セッションを明示的に持つには `minae daemon serve` を実行します。

## エージェント向け

```console
# 有界 read: 必要な番号付き行だけ
$ minae session get --lines 1:40

# 検証付き内容指定編集（テキストが一致しなければ理由つきで拒否される）
$ minae session apply src/lib.rs "let old = 1" "let old = 2"

# 世代が進むまでブロック
$ minae session wait 42

# ワークスペースルートをまたぐ LSP rename
$ minae session rename src/lib.rs "USD" "JPY"
```

各コマンドは JSON を返します。編集が拒否されたら該当範囲を読み直して再試行してください —— 拒否メッセージは何が一致しなくなったかを示します。`minae session hints <path>` と `minae session peek <path> <line>:<col>` は全文なしで inlay hints と定義ポップアップを取得します。

#### Agent skills（エージェント向けスキル）

`minae skill` はエージェント向けのオンデマンド型スキル棚です: 無引数なら薄い索引（1トピック1行）を、`minae skill <topic>` ならそのトピックの内容だけを返します。

```console
$ minae skill           # 索引: read, outline, at, edit, rename, references, persist, errors
$ minae skill read      # read 契約（session get --lines の番号付き出力）
```

ガイドはツール選択（read / apply / rename 契約）とエラー回復を扱います。エージェントがパースできるよう、出力は英語・成功は exit 0 に統一され、未知トピックは exit 1 で理由と利用可能トピック一覧を返します。編集が拒否されたら `minae skill errors` が最初の参照先です。

## アーキテクチャ

- **Daemon**: ドキュメント・undo 履歴・セレクション・LSP クライアント・構文ハイライトを保持。ローカルソケットでクライアントにサービス提供
- **minae-core**: UI 非依存の編集コア —— ドキュメント・セレクション・トランザクション
- **minae-protocol**: daemon/client IPC のワイヤ型
- **minae-lsp**: LSP クライアント（起動・JSON-RPC・位置変換）
- **minae-view** / **minae-loader**: UI 非依存のエディタ状態 / tree-sitter 文法とハイライトクエリ
- **minae-term**: バイナリクレート —— CLI・デーモン・TUI・ヘッドレス session インターフェース

## ドキュメント

- [CONTEXT.md](CONTEXT.md) — ドメインモデルの正規用語集
- [docs/helix-architecture.md](docs/helix-architecture.md) — アーキテクチャと設計ノート
- [docs/benchmarks/](docs/benchmarks/) — 公開しているエージェントエディタ A/B 計測結果・テスト計画・評価ノート
- [docs/adr/](docs/adr/) — 意思決定記録
- [docs/spec/](docs/spec/) — ワイヤプロトコル仕様

minae は開発中です。荒い部分がある前提でお願いします。

## ライセンス

MIT — [LICENSE](LICENSE) を参照。

Copyright (c) 2026 335g