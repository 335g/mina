# mina

> エージェントに操作させるためのエディタ。常駐デーモンがエディタ状態 —— ドキュメント・undo・LSP —— を保持し、エージェントはファイルを書き換えるのではなく session 契約（検証付きの内容指定編集）を通して編集します。フロントエンド —— エージェントが呼ぶヘッドレス `minas` session CLI、人が使うオプションの TUI —— は、ただのクライアントです。

**mina** = **min**imize cost for AI **a**gent（AI エージェントのためのコスト最小化）—— コストを最小化するエージェントエディタ。サフィックスで: mina**e** = editor（TUI）、mina**d** = daemon、mina**s** = session（ヘッドレス CLI）。

[English](./README.md) · 日本語

## mina を使う効果

エージェント駆動の編集において、mina の session 契約は素朴なファイルツールより良い計測結果を示します。実用的な2ファイル機能追加タスク（~600行の Rust クレートにフィールドを1つ追加、編集5箇所、`cargo check` で検証、タスク途中に外部ファイル変更を注入）での A/B テスト結果:

| | 素朴なツール（全文 read・無検証の置換） | minas session 契約（範囲 read・検証付き apply） |
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
- **言語サーバ**（ADR-0030 に基づく検証済みの埋め込み）: Rust に `rust-analyzer`、TypeScript に `typescript-language-server` —— 同じ2言語分の tree-sitter 構文ハイライトも同梱。ほかの LSP はユーザーの `languages.toml` で追加可能（未検証サーバも標準機能はネゴシエーションで動作）
- **ヘッドレスエージェントインターフェース**: `minas` —— 有界な番号付き read（`get --lines`）、ワンショット検証付き編集（`apply`）、位置指定編集（`edit`）、状態遷移の待機（`wait`）、全文なしの hints / peek、セマンティック rename（`rename`）、構造把握（`outline`）と位置解決（`at`）、hover 型参照（`hover`）とワークスペースシンボル検索（`symbol`）、編集→検証ループを 1 コマンドに圧縮する診断 settle+報告（`check` — `wait` + `get` + JSON パースの代替）
- **ターミナル UI**: `minae` TUI（ratatui/crossterm、接続別 View）—— `minae [file ...]` で対話編集
- **設定可能**: 言語サーバの `languages.toml`（デーモン側）、`config.toml` とユーザーカラースキーム（クライアント側）、エージェント向けビルトインの `minas skill` ガイド

## はじめに

```console
# 1. daemon + session CLI。エージェント向けツールのみで、TUI は別途
$ cargo install minad minas

# 2. 言語サーバは同梱されない —— 使う言語のものを入れる
$ rustup component add rust-analyzer
$ npm install -g typescript-language-server typescript

# 3. 何が見つかったか確認する: daemon の世代 + 設定済みサーバ
$ minas info
```

デーモンは最初のクライアントから必要に応じて起動されます。常駐させるには `minad serve` を実行します。言語サーバはワークスペースルート単位で起動されるため、`PATH` に通っていればすぐに `symbol` / `rename` / `check` が動きます。tree-sitter による構文ハイライトはそのまま動きます。

ソースからは `cargo build --release` でもビルドできます。TUI は `minae [file ...]` です。

### エージェントに minas を使わせる

`minas skill` は pull 型です。minas の存在を知らないエージェントは永遠に `minas skill` を呼びません。エージェントが指示を読む場所へ、引き金を一度だけ配ります:

```console
$ minas skill --md > ~/.claude/skills/minas/SKILL.md   # ~/.pi/agent/skills/minas/SKILL.md でも同じ
$ minas skill --md >> AGENTS.md                         # リポジトリの指示文に直接追記してもよい
```

ラッパーは棚の写しを持たず（「`minas skill` を実行せよ」だけ）、トピックが増えても腐りません。ファイル末尾の刻印と `minas info` の `cli_generation` が違っていたら再生成してください。

### 最初のセッション

```console
$ minas outline src/lib.rs              # 全文なしで構造を把握
$ minas read src/lib.rs --lines 40:60   # 必要な番号付き行だけ
$ minas apply src/lib.rs "old" "new"    # 検証付き編集（古ければ理由つきで拒否）
$ minas check src/lib.rs                # LSP 診断 —— 最終ゲートは build
```

read / edit に設定は不要です。挙動を変えるファイルは 2 つだけ、どちらも任意です:

- `~/.config/minae/languages.toml`（`$XDG_CONFIG_HOME/minae/`、デーモン側）: 言語サーバの追加・上書き。埋め込み既定にマージされ、サーバ起動時に読み直されます
- `~/.config/minae/config.toml`（クライアント側、TUI のみ）: `colorscheme` と `agent_command`（TUI がレビューコメントから起動するエージェント）。ユーザーカラースキームは `~/.config/minae/colorschemes/` に TOML で置きます

## エージェント向け

```console
# 有界 read: 必要な番号付き行だけ
$ minas get --lines 1:40

# 検証付き内容指定編集（テキストが一致しなければ理由つきで拒否される）
$ minas apply src/lib.rs "let old = 1" "let old = 2"

# 世代が進むまでブロック
$ minas wait 42

# ワークスペースルートをまたぐ LSP rename
$ minas rename src/lib.rs "USD" "JPY"
```

各コマンドは JSON を返します。編集が拒否されたら該当範囲を読み直して再試行してください —— 拒否メッセージは何が一致しなくなったかを示します。`minas hints <path>` と `minas peek <path> <line>:<col>` は全文なしで inlay hints と定義ポップアップを取得します。

#### Agent skills（エージェント向けスキル）

`minas skill` はエージェント向けのオンデマンド型スキル棚です: 無引数なら薄い索引（1トピック1行）を、`minas skill <topic>` ならそのトピックの内容だけを返します。棚はバイナリに同梱されているため、常にインストール済みビルドと一致します。

```console
$ minas skill           # 索引: usage, read, search, wait, edit, check, rename, references, ...
$ minas skill read      # read 契約（session get --lines の番号付き出力）
```

ガイドはツール選択（read / apply / rename 契約）とエラー回復を扱います。エージェントがパースできるよう、出力は英語・成功は exit 0 に統一され、未知トピックは exit 1 で理由と利用可能トピック一覧を返します。編集が拒否されたら `minas skill errors` が最初の参照先です。

ただし **minas の存在を知らないエージェントは `minas skill` を呼びません**（棚は pull 型）。`minas skill --md` はそれを渡すための薄いラッパーを出力します: YAML frontmatter ＋ `usage` ガイド（いつ `rg`/`sed` ではなく minas を使うか、最初の 1 コマンドから効かせる規則）＋ ビルド世代の刻印。置き場は[エージェントに minas を使わせる](#エージェントに-minas-を使わせる)を参照してください。

## アーキテクチャ

- **Daemon**: ドキュメント・undo 履歴・セレクション・LSP クライアント・構文ハイライトを保持。ローカルソケットでクライアントにサービス提供
- **mina-text**: UI 非依存の編集コア —— ドキュメント・セレクション・トランザクション
- **mina-protocol**: daemon/client IPC のワイヤ型
- **mina-lsp**: LSP クライアント（起動・JSON-RPC・位置変換）
- **mina-view** / **mina-loader**: UI 非依存のエディタ状態 / tree-sitter 文法とハイライトクエリ
- **mina-conn**: session CLI と TUI が共有するクライアント側の接続・リクエスト配線
- **minad**: daemon バイナリ
- **minas**: ヘッドレス session CLI と skill ガイド
- **minae**: TUI（ratatui/crossterm、接続別 View）

## ドキュメント

- [CONTEXT.md](CONTEXT.md) — ドメインモデルの正規用語集
- [docs/helix-architecture.md](docs/helix-architecture.md) — アーキテクチャと設計ノート
- [docs/benchmarks/](docs/benchmarks/) — 公開しているエージェントエディタ A/B 計測結果・テスト計画・評価ノート
- [docs/adr/](docs/adr/) — 意思決定記録
- [docs/spec/](docs/spec/) — ワイヤプロトコル仕様

mina は開発中です。荒い部分がある前提でお願いします。

## ライセンス

MIT — [LICENSE](LICENSE) を参照。

Copyright (c) 2026 335g