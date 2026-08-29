# languages.toml: Daemon が読む第 2 設定ソース（ADR-0030）

rust-analyzer 専用だった LSP 連携（サーバコマンド・languageId・初期化オプション・
WorkspaceRoot 判定）を言語別テーブルに汎用化し、2 言語目以降（まず TypeScript）を
足せるようにする。

## 形式

Helix の `languages.toml` を踏襲した 2 セクション構成。

```toml
[language-server.rust-analyzer]
command = "rust-analyzer"
args = []
config = { "rust-analyzer" = { inlayHints = { typeHints = { enable = true }, parameterHints = { enable = true } } } }

[[language]]
name = "rust"
file-types = ["rs"]
language-server = "rust-analyzer"
root-markers = []  # 明示すれば汎用集合を置換（空 = マーカーなし = 親フォールバックのみ）
# grammar = "rust"    # Stage 4 で有効化（mina-loader がハイライトを供給）
```

- **`[language-server.<id>]`**: サーバ起動と初期化の定義。`command` / `args` /
  `config`（= LSP `initializationOptions` としてそのまま送る不透明 JSON）。
  複数言語が同一サーバを共有できる（ts/tsx/js はサーバ 1 つ）。
- **`[[language]]`**: ファイル種別の定義。`name`（= `textDocument.languageId`）/
  `file-types` / `language-server` 参照 / `root-markers` / 任意の `grammar`。

## 置き場所と読み手

`$XDG_CONFIG_HOME/mina/languages.toml`（なければ `~/.config/mina/languages.toml`）。
読み手は **Daemon** — LSP を spawn・保持する主体だから。既存 client Config
（config.toml）の「Daemon は読まない」契約（CONTEXT.md の Config 項）は維持し、
languages.toml は **Daemon 側の第 2 設定ソース**として別カテゴリとする。

代替案は却下:
- **config.toml に統合**: 「Daemon は読まない」契約を破る。サーバ spawn は Daemon の
  責務であり client-local 設定に載せられない。
- **Hello で送信**: Client が読んだテーブルを接続時に運ぶ形。再接続や複数クライアントで
  設定が揺れ、サーバのライフサイクル（長命）と不整合。

## 読み込みタイミング

- **起動時**: 初期ロード（Daemon の状態に保持）。
- **以後**: `languages_refresh` が languages.toml の **mtime 差分だけ再読込**する。
  呼び出しは (a) LSP 対応ゲート（拡張子 → サーバ有無の判定）と (b) セッション spawn
  （`ensure`）の**両方** — ゲートが spawn より先に走るため、spawn 時だけの再読込では
  新言語の追加がゲートを通過できず再起動まで反映されない（敵対的検証で発見して修正）。
- 従って、編集した languages.toml は**次にゲート判定が走る時点で反映**され、
  稼働中セッションはネゴシエーション済みのまま（再 initialize しない。既存エントリの
  変更は新規 spawn から、新規言語の追加は次のゲートから効く）。
- 破損したユーザーファイルは警告して**埋め込み既定のみ**にフォールバック
  （config.toml と同方針）。
- バイナリは既定テーブル（現在は rust-analyzer 分）を埋め込み、ユーザーファイルは
  **name / id 単位で上書き・追加**する（言語は `name`、サーバは id）。深いマージはしない。

## 形式の注意（未対応キー）

上記の例に `grammar` を書いたが、**このキーは現行ステージでは拒否される**
（`deny_unknown_fields` によりファイル全体が破棄され、既定テーブルにフォールバック）。
Stage 4 で有効化するまで、ユーザーファイルに書かないこと（例をそのまま貼り付けると
全上書きが消える）。`root-markers` は Stage 2 で有効化済み。

## 規定サーバ方針

- **既定同梱エントリ = mina が検証済みのサーバのみ**。検証 = t9 風の A/B
  （tools/ab）で rename / references / inlay hints の実動作を確認済み、という意味。
  既定の init options（inlay hints のチューニング等）もこの検証とセットで同梱する。
- **ユーザーが任意サーバを languages.toml に足すのは許可・非保証**。LSP 標準 + initialize
  応答の capability 動的判定（Stage 3）が共通機能（診断・定義ジャンプ等）を保証し、
  inlay hints / rename 等はサーバが advertise しない限り自然に無効になる（10 秒リトライに
  突入しない）。「ブロックする」のではなく「既定として何を検証・同梱するか」で規定する。
- 初期化オプションのスキーマはサーバ固有（rust-analyzer は `initializationOptions` で
  inlay hints を明示 ON にしないと既定オフ。TS / gopls は別キー・別経路）。よって
  config は言語ではなく**サーバ定義**に持たせる。capabilities から導出しない —
  capabilities は「機能を持っているか」であり「機能が有効か」はサーバ独自のツマミで、
  capability には載らないため。

## root-markers（Stage 2 で実装済み）

- 言語に `root-markers` の**明示がなければ汎用集合**（`Cargo.toml` / `package.json` /
  `pyproject.toml` / `go.mod` + `.git`）で WorkspaceRoot を判定。
- **明示があれば汎用集合を置換**（`.git` を含めたければ自分で書く。マーカーなしは
  ファイル親にフォールバック = 既存挙動維持）。union 方式は `.git` を除外できないため不採用。
- git 管理していないディレクトリの markdown 等はファイル親フォールバックで動き、
  root を固定したい場合は明示マーカーを書ける。「git 管理しているがそこを root に
  したくない」は置換で `.git` を除外して解決。
- 最寄りマーカー勝ちの挙動は不変（ADR-0010）。Rust workspace の複数 Cargo.toml は
  従来どおり最寄りの Cargo.toml（メンバークレート）が root になる。

## 将来の衝突点（Stage 4 で解消）

- セッションは WorkspaceRoot キー（ADR-0010）。同一 root に複数言語が混在する場合
  （.rs + .ts 等）はキーを **(root, language)** に拡張し、言語ごとにサーバを分ける。
- `grammar` キー: `[[language]]` の任意キー。mina-loader が grammar 名でハイライトを
  供給し、未登録ならハイライト無し・LSP のみ（tree-sitter grammar は静的リンクのまま）。
- 後回し: ファイル監視 + 稼働中セッションの再初期化、プロジェクト別 languages.toml、
  サーバ特有の癖（references 前のワークスペース全 didOpen 等 — ADR-0029）の言語別設定化。

## Consequences

- `lsp::server_for`（拡張子 → サーバコマンドの静的テーブル）は廃止され、
  `LanguageTable` 参照に置き換わる。daemon の「LSP 対応ゲート」はすべてテーブル参照になる。
- `didOpen` の `languageId` はセッション生成時の言語名（spawn 元ファイルの言語）。
  セッションは生成後に言語が変わらない（Stage 4 のキー拡張で保証）。
- 言語追加 = 検証 + 埋め込み既定エントリ追加（+ 必要なら tree-sitter grammar 依存）。
- テストシーム `MINA_LSP_COMMAND`（環境変数によるサーバ差し替え）は維持する。