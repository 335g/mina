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
grammar = "rust"  # minae-loader の grammar 名（ハイライト・シンボル解決）
```

- **`[language-server.<id>]`**: サーバ起動と初期化の定義。`command` / `args` /
  `config`（= LSP `initializationOptions` としてそのまま送る不透明 JSON）。
  複数言語が同一サーバを共有できる（ts/tsx/js はサーバ 1 つ）。
- **`[[language]]`**: ファイル種別の定義。`name`（= `textDocument.languageId`）/
  `file-types` / `language-server` 参照 / `root-markers` / 任意の `grammar`。

## 置き場所と読み手

`$XDG_CONFIG_HOME/minae/languages.toml`（なければ `~/.config/minae/languages.toml`）。
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
- **root-markers を稼働中に編集した場合**: セッションは spawn 時のキーのまま（再 initialize
  しない方針に整合）。パス→セッションの lookup は、最新テーブルの root にセッションが
  なければ「パスを包含する最長の既存キー」へフォールバックする（`session_root_for`）ので
  稼働中セッションを見失わない（同期スキップ・診断消失・重複 spawn を防止。敵対的検証
  P1）。spawn 判定（`ensure`）はフォールバック**しない** — ネストしたワークスペース
  （/a と /a/c の両セッションが正当に共存）で祖先セッションを誤って再利用しないため。
  使われなくなった旧キーの温かいセッションはそのまま残る（ADR-0010 の reap なし方針）。
- 破損したユーザーファイルは警告して**埋め込み既定のみ**にフォールバック
  （config.toml と同方針）。
- バイナリは既定テーブル（現在は rust-analyzer 分）を埋め込み、ユーザーファイルは
  **name / id 単位で上書き・追加**する（言語は `name`、サーバは id）。深いマージはしない。

## 既定エントリ（Stage 4 で TypeScript を追加）

- `typescript`（typescript-language-server 5.x、`--stdio`、file-types `ts`）。
- tree-sitter-typescript をハイライト・シンボル解決に使用（`grammar = "typescript"`）。
- inlay hints の初期化チューニング（tsserver の `preferences`）は**t9 風 A/B 検証で
  確定してから**既定 config に足す（規定サーバ方針: 検証済みの設定だけを同梱）。
- 未検証のまま使えるのは LSP 標準機能（診断・定義・rename/references — 上記 E2E で確認）。
- tsx / js / jsx 等の file-types は server 共有の別エントリとして追加可能（Stage 4 以降）。

## 規定サーバ方針

- **既定同梱エントリ = minae が検証済みのサーバのみ**。検証 = t9 風の A/B
  （tools/ab）で rename / references / inlay hints の実動作を確認済み、という意味。
  既定の init options（inlay hints のチューニング等）もこの検証とセットで同梱する。
- **ユーザーが任意サーバを languages.toml に足すのは許可・非保証**。LSP 標準 + initialize
  応答の capability 動的判定（実装済み — 下の「能力ゲート」節）が共通機能（診断・定義
  ジャンプ等）を保証し、inlay hints / rename 等はサーバが advertise しない限り自然に無効に
  なる（10 秒リトライに突入しない）。「ブロックする」のではなく「既定として何を検証・
  同梱するか」で規定する。

## 能力ゲート（Stage 3 で実装済み）

initialize 応答の capabilities から [`ServerCapabilities`]（pull_diagnostics / inlay_hints /
rename / references / definition）を導出し、機能ごとに要求を止める。

- **diagnostic（pull）**: `diagnosticProvider` が無ければ診断を空にする（pull しない）。
- **inlay hints**: `inlayHintProvider` が無ければヒントを空にする（編集ごとの pull をしない）。
- **rename / references**: `renameProvider` / `referencesProvider` が無ければ daemon が
  即「`rename not supported` / `references not supported`」（exit 1・再試行不可、ADR-0029 の
  分類をそのまま使う）を返す — ワークスペース走査（didOpen-all）と 10 秒リトライ予算を
  消費しない。
- **peek（定義）**: `definitionProvider` が無ければ peek なし（空応答）。
- キー欠落・明示 `false` は非対応扱い。`true` とオブジェクト形式（RenameOptions 等）は
  対応扱い。`renameProvider: false` を advertise するサーバは稀だが正しく扱える。
- **ゲートや中間エラーで早期 return する場合もフォーカス文書へ復元する**:
  `prepare_borrowed_session` が対象を didOpen 済みのため、借用していたら `restore_focus_*`
  を呼んでから応答する（呼ばないと LSP の current_uri が対象のままになり、次回編集の同期
  スキップ・診断消失を招く — 敵対的検証で発見して修正。rename/references の中間エラー
  経路も err 応答の共通ルートで同じ復元を行う）。
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
- 注意: rust の**外側フォールバック**は旧実装（Cargo.toml か .git）より広い — 汎用集合の
  package.json 等が途中にあればそこで root になり得る（例: node パッケージ内の Rust
  bindings）。これは汎用集合の合意設計どおりで、除外したければ rust に明示的な
  `root-markers` を書く（置換）。

## 将来の衝突点（Stage 4 で実装済み）

- **セッションキー**: WorkspaceRoot キー（ADR-0010）を **(WorkspaceRoot, languageId)** に拡張
  した。同一 root に複数言語が混在する場合（.rs + .ts 等）も言語ごとに別セッション
  （別サーバ）。`session_root_for` / `borrows_focus_session` も同言語キーのみを対象にする。
- **`grammar` キー（実装済み）**: `[[language]]` の任意キー。minae-loader の静的レジストリを
  grammar 名で引く。未登録・未指定ならハイライト無し・tree-sitter シンボル解決なし
  （単語境界フォールバック）。シンボル位置解決（rename / references の `old` 解決）も
  この grammar を使うため、TS の rename もコメント・文字列を除外した識別子に解決する。
- **開文書の保持（keep-open）**: セマンティック要求の前段（`open_workspace_files`）で
  ワークスペースの同拡張子ファイルを**開いたまま保持**する（`did_open_keep`）。
  1 セッション = 1 開文書の設計で前の文書を didClose すると、**tsserver は閉じた
  ファイルの rename/references を null/空で返す**（probe 実測）。要求対象も開き直す。
  rust-analyzer の「開いていないファイルの参照を取りこぼす」対策（ADR-0029）と同方向。
- 後回し: ファイル監視 + 稼働中セッションの再初期化、プロジェクト別 languages.toml、
  サーバ特有の癖の言語別設定化。

## Consequences

- `lsp::server_for`（拡張子 → サーバコマンドの静的テーブル）は廃止され、
  `LanguageTable` 参照に置き換わる。daemon の「LSP 対応ゲート」はすべてテーブル参照になる。
- `didOpen` の `languageId` はセッション生成時の言語名（spawn 元ファイルの言語）。
  セッションは生成後に言語が変わらない（Stage 4 のキー拡張で保証）。
- 言語追加 = 検証 + 埋め込み既定エントリ追加（+ 必要なら tree-sitter grammar 依存）。
- テストシーム `MINA_LSP_COMMAND`（環境変数によるサーバ差し替え）は維持する。