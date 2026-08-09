# Helix エディタのアーキテクチャ調査ノート

> 一次情報: Helix ソースコード (v25.7.1, https://github.com/helix-editor/helix を shallow clone して調査)
> 公式の内部解説ドキュメント `docs/architecture.md` が存在する。本ノートはそれをコードで裏取りしたもの。

---

## (a) 全体像と設計思想

Helix は Rust 製のモーダル・ターミナルエディタ。設計の柱は以下の通り。

1. **モーダル編集** — `Mode` は `Normal` / `Select` / `Insert` の3つ。
   出典: `helix-view/src/document.rs` (`pub enum Mode`)

2. **複数カーソルが第一級の編集プリミティブ** — カーソル自体も1要素の selection として定義される。
   > "Selections are the primary editing construct. Even cursors are defined as a selection range."
   出典: `helix-core/src/selection.rs` (ファイル冒頭の doc comment)

3. **Rope によるバッファ表現** — 編集コストが低く、clone が安価なためテキスト状態のスナップショットが容易。
   出典: `docs/architecture.md` (Core 節)、`helix-core/src/lib.rs` (`pub use ropey::{self, ..., Rope, ...}`)

4. **関数型コア** — core のプリミティブは破壊的操作をせず新しいコピーを返す。CodeMirror 6 の設計に強く影響を受けている。
   出典: `docs/architecture.md` (Core 節)

5. **OT 風 Transaction による編集と undo** — 文書への変更は `Transaction` として表現し、逆変換 (invert) で undo を実現。Selection は Transaction 上で写像して新テキスト状態へ追従する。
   出典: `docs/architecture.md` (Core 節)、`helix-core/src/transaction.rs`

6. **tree-sitter による構文ハイライト＋構造編集** — ハイライトだけでなく、AST ノード単位の移動・選択・削除・テキストオブジェクトに使う。
   出典: `helix-core/src/syntax.rs`、`docs/architecture.md` (Core 節: "`Syntax` is the interface used to interact with tree-sitter ASTs")

7. **LSP 前提の編集モデル** — 言語サーバからの診断・補完・フォーマット・コードアクションが組み込み。オートフォーマットやペア文字も言語設定に含まれる。
   出典: `helix-core/src/syntax/config.rs` (`LanguageConfiguration` の `auto_format`, `auto_pairs`, `language_servers` 等)

8. **非同期 (tokio) ベースのイベントループ** — 端末イベント・LSP・ジョブを `tokio::select!` で多重化し、UI 応答をブロックしない。
   出典: `helix-term/src/application.rs` (`event_loop_until_idle` 内の `tokio::select!`)

9. **Cursive 風のレイヤー合成 UI** — すべての UI 部品は `Component` トレイト、`Compositor` がレイヤー (Vec) として管理し、上に重ねて描画。ポップアップ/ピッカーはレイヤーの積み重ね。
   出典: `helix-term/src/compositor.rs` (冒頭コメント "Cursive-inspired")、`docs/architecture.md` (View 節)

---

## (b) crate 構成と責務分担

Cargo workspace (14メンバー)。公式 `docs/architecture.md` の表と一致する。

| crate | 責務 |
|---|---|
| **helix-stdx** | 標準ライブラリ拡張 (`env`, `path`, `range`, `rope`, `uri`, `faccess`)。rust-analyzer の stdx に触発 |
| **helix-core** | 編集プリミティブ (関数型)。バッファ・選択・移動・インデント・Transaction・tree-sitter 統合。**UI を知らない** |
| **helix-view** | バックエンド非依存の UI 抽象 (imperative shell)。`Editor` / `Document` / `View` / テーマ。現状はターミナル UI に密着気味 (公式も認める) |
| **helix-term** | ターミナル UI 本体 (実行バイナリ)。イベントループ・Compositor・コマンド・キーマップ・UI ウィジェット |
| **helix-tui** | TUI プリミティブ。tui-rs からフォーク、Cursive に触発 (`Buffer`, `Terminal`, `widgets`, `backend`) |
| **helix-lsp** | LSP クライアント (プロセス管理・JSON-RPC・位置変換) |
| **helix-lsp-types** | LSP プロトコルの型定義 (`lib.rs` 約97KB、全メッセージ) |
| **helix-dap** | Debug Adapter Protocol クライアント |
| **helix-dap-types** | DAP プロトコル型定義 |
| **helix-event** | エディタ内イベントとフック (`AsyncHook`、debounce、`TaskController`)。非同期処理を UI から疎結合にする |
| **helix-loader** | 外部リソースの取得・ビルド・ロード (tree-sitter grammar の fetch/build、runtime ディレクトリ、workspace 信頼) |
| **helix-vcs** | VCS diff 統合。現状は git のみ (`DiffHandle`, `DiffProviderRegistry`) |
| **helix-parsec** | パーサコンビネータ (snippet 解析用) |
| **xtask** | 開発用タスク |

依存の向き (上位→下位): `helix-term` → `helix-view` → `helix-core` → `helix-stdx`。`helix-lsp` / `helix-loader` / `helix-event` / `helix-vcs` / `helix-parsec` は core または view 層から参照される横断的ユーティリティ。
出典: `/Cargo.toml` (workspace members)、`docs/architecture.md`、各 crate の `src/lib.rs`

---

## (c) 主要モジュールと型

### コア (`helix-core`)

- **`Rope`** — バッファ本体。`ropey` を再エクスポート。clone 安価・grapheme 境界処理を持つ。
  出典: `helix-core/src/lib.rs`
- **`Range`** — `anchor` (動かない側) + `head` (拡張時に動く側) + 前回の visual position。範囲は左包含・右非包含。
  出典: `helix-core/src/selection.rs`
- **`Selection`** — `SmallVec<[Range; 1]>` + `primary_index`。1要素なら単一カーソル。
  出典: `helix-core/src/selection.rs:417`
- **`Transaction`** — `Operation` (`Retain`/`Delete`/`Insert`) の列。`invert()` で undo、Selection を写像できる。`ChangeSet` はドキュメント差分の集合表現。
  出典: `helix-core/src/transaction.rs`、`helix-core/src/lib.rs`
- **`Syntax` / `Loader`** — 言語設定 (`languages.toml`) をロードし、tree-sitter の grammar・query (highlights / textobjects / indent / injection) を管理。`tree_house` クレートが tree-sitter をラップ。
  出典: `helix-core/src/syntax.rs:275`、`helix-core/src/syntax/config.rs`
- **`movement.rs` (83KB)** — カーソル移動・行移動・ワード移動等のアルゴリズム群。
  出典: `helix-core/src/movement.rs`

### ビュー層 (`helix-view`)

- **`Document`** — `Rope` + 各 View の `Selection` + `Syntax` + `History` (undo) + LSP 診断を束ねる。**1つの文書に複数 View があるため、selection は ViewId ごとに持つ**。
  出典: `helix-view/src/document.rs`、`docs/architecture.md`
- **`View`** — 1スプリットを表す。gutter・ステータスライン・診断・コード表示領域を内包。
  出典: `docs/architecture.md` (View 節)、`helix-view/src/view.rs`
- **`Tree`** — スプリットのツリー構造。`Node` は `View` か `Container`。`SlotMap<ViewId, Node>` で管理。
  出典: `helix-view/src/tree.rs`
- **`Editor`** — グローバル状態。`mode`, `tree`, `documents: BTreeMap<DocumentId, Document>`, `registers`, マクロ録画/再生, `language_servers: helix_lsp::Registry`, `diagnostics`, `diff_providers`, `debug_adapters`。
  出典: `helix-view/src/editor.rs:1273`

### ターミナル層 (`helix-term`)

- **`Compositor`** — `layers: Vec<Box<dyn Component>>`。イベントは最前面から背面へ伝播し (イベントバブリング)、最初に Consumed したレイヤーで止まる。`render()` は全レイヤーを順に描画。
  出典: `helix-term/src/compositor.rs`
- **`Component` trait** — `handle_event` / `should_update` (再描画節約) / `render(area, surface, ctx)` / `cursor` / `required_size`。
  出典: `helix-term/src/compositor.rs`
- **`Application`** — `compositor` + `terminal` + `editor` + `jobs` + `signals` を束ね、イベントループを回す。
  出典: `helix-term/src/application.rs`
- **イベントループ** — `event_loop` → `event_loop_until_idle`。`tokio::select!` (biased) で ①シグナル ②端末入力 ③ジョブ完了コールバック ④ステータスメッセージ ⑤ジョブの非同期フューチャ ⑥エディタイベント を多重化。各処理後に `render()`。
  出典: `helix-term/src/application.rs:294-355`
- **`Keymaps`** — キーイベント → コマンドの解決。モード別キーマップ。
  出典: `helix-term/src/keymap.rs:264`
- **`commands.rs` (239KB)** — 全コマンド (キーバインドに紐づくアクション) の実装。
  出典: `helix-term/src/commands.rs`、`docs/architecture.md` (Term 節)

### LSP (`helix-lsp`)

- **`Client`** — 言語サーバ1プロセスに対応。`Child` プロセス、`UnboundedSender<Payload>`、`request_counter: AtomicU64`、`capabilities`。
  出典: `helix-lsp/src/client.rs:56`
- **`Registry`** — `SlotMap<LanguageServerId, Arc<Client>>` + 名前→クライアント群 + 受信ストリーム `incoming` (全サーバのメッセージを select_all で合成)。
  出典: `helix-lsp/src/lib.rs:581`
- **位置変換** — `OffsetEncoding` (`Utf8`/`Utf32`/`Utf16`、デフォルト **Utf16**) を切り替え、LSP と内部 (char offset) を変換。
  出典: `helix-lsp/src/lib.rs` (`OffsetEncoding`)、`util::lsp_pos_to_pos` / `pos_to_lsp_pos`

---

## (d) 主要依存ライブラリと用途

| ライブラリ | 用途 | 出典 |
|---|---|---|
| **ropey** | Rope データ構造 (バッファ) | `/Cargo.toml` (workspace deps) |
| **tree-house** | tree-sitter のラッパー (grammar・query・highlighter) | `helix-core/Cargo.toml`, `helix-loader/Cargo.toml` |
| **tokio** | async runtime (rt-multi-thread, io, time, process, fs) | `helix-term/Cargo.toml` |
| **termina** | 非 Windows の端末 API・イベントストリーム | `helix-term/Cargo.toml`, `application.rs` |
| **crossterm** | Windows の端末バックエンド | `helix-term/Cargo.toml`, `application.rs` |
| **helix-tui** | tui-rs フォークの TUI プリミティブ (Buffer/Terminal/widgets) | `helix-term/Cargo.toml` (`package = "helix-tui"`) |
| **nucleo** | fuzzy マッチング (ファイルピッカー・ファジーファインダ) | `helix-term/Cargo.toml`, `helix-core/Cargo.toml` |
| **imara-diff** | 差分計算 (diff 表示) | `helix-core/Cargo.toml` |
| **arc-swap** | ロックフリーの設定/状態のスワップ (config リロード) | workspace 全体 |
| **slotmap** | 世代付き ID キー (`DocumentId`, `ViewId`, LSP ID) | `helix-view`, `helix-lsp` |
| **smallvec** | 小さな Vec の高速化 (Selection の ranges) | `helix-core/Cargo.toml` |
| **unicode-segmentation / unicode-width** | 書記素クラスタ・表示幅 | `helix-core/Cargo.toml` |
| **serde / serde_json / sonic-rs / toml** | 設定・LSP メッセージの (de)serialization | 各 crate |
| **parking_lot** | 高速 Mutex | workspace |
| **signal-hook** | シグナル処理 | `helix-term/Cargo.toml` |
| **pulldown-cmark** | Markdown ドキュメントポップアップ描画 | `helix-term/Cargo.toml` |
| **ignore / grep-regex / grep-searcher** | ファイルピッカー・テキスト検索 | `helix-term/Cargo.toml` |
| **globset** | ファイルタイプ glob マッチ | `helix-core/Cargo.toml` |
| **encoding_rs** | ファイルエンコーディング処理 | `helix-core/Cargo.toml` |
| **etcetera** | 設定/データディレクトリの解決 | `helix-loader/Cargo.toml` |

---

## (e) Helix のようなエディタを作る際に検討すべき事項

コードから読み取れる設計判断と、その検討ポイント。

1. **バッファのデータ構造** — Helix は **ropey (Rope)**。理由は clone が安価 → テキスト状態のスナップショット (undo・diff・LSP 比較) が楽、という点 (`docs/architecture.md`)。代替は gap buffer / piece table。`Vec<String>` では巨大ファイルで破綻する。
2. **複数カーソルを最初から入れる** — `Selection` はコアの中心で、後付けは全書き換えになる。`Range` の anchor/head モデル (ヘッド移動による選択拡張) が基本。
3. **編集は Transaction で表現する** — 直接バッファを書き換えず、OT 風の変更集合を適用。invert で undo、選択範囲を写像してカーソル位置を保つ。これがないと undo/redo と複数カーソルの整合が取れない。
4. **レンダリング** — 端末なら「バックエンド抽象 (termina/crossterm) + サーフェス (Buffer) + レイヤー合成 (Compositor) + 差分描画 (`full_redraw`, `should_update`)」。GUI にする場合も、この「サーフェスへの描画 + 必要な時だけ再描画」モデルは流用可能。
5. **入力処理** — `KeyEvent` の正規化 (修飾キー・ASCII/Unicode)、モード別キーマップの解決、マクロ録画/再生 (`macro_recording`/`macro_replaying`)。キーマップ解決はコマンドと1対1にせずツリー/前置キー (g, m 等) を考慮する。
6. **ハイライトと構造編集** — tree-sitter 採用なら grammar の取得・ビルド管理 (`helix-loader/src/grammar.rs` が git から取得し共有ライブラリ (.so/.dylib) にビルド) と query ファイル群 (highlights/textobjects/indent/injection) の運用が発生する。自前 lexer は構造編集を諦めるか自前 AST を作ることになる。
7. **LSP 統合** — プロセス管理・JSON-RPC・リクエストID・キャパビリティ交渉。**最大の落とし穴は位置エンコーディング**: LSP は UTF-16 オフセットがデフォルトなので、内部の char インデックスとの変換層 (`OffsetEncoding`, `lsp_pos_to_pos`/`pos_to_lsp_pos`) が必須。
8. **非同期実行モデル** — イベントループを `tokio::select!` で構成し、重い処理 (LSP・保存・diff) は `Jobs` でバックグラウンド化してコールバックで UI に戻す。UI をブロックしないことが最優先。設定リロードは `arc-swap` で lock-free に。
9. **イベント/フック** — `helix-event` の `AsyncHook` (debounce 付き) で「文書変更→保存」「保存→LSP 再解析」のような連鎖を疎結合に。独自設計するならイベント発火の順序保証とキャンセル (TaskController) に注意。
10. **状態の持ち方** — グローバル `Editor` / 文書 `Document` / 表示 `View` + スプリット `Tree` の分離。**1文書を複数 View で表示できるため、カーソル位置 (selection) は文書ではなく View 側に持つ** という判断が重要。
11. **設定・テーマ・言語定義は TOML に集約** — `languages.toml` (156KB) に言語ごとの grammar・LSP・インデント・ペア文字を宣言。コードに言語知識を埋め込まない。
12. **拡張性の割り切り** — Helix はプラグイン API を (現時点で) 持たず、Rust 内で構成する方針。プラグインシステムは設計コストが非常に高いので、「プラグインは後から」と明示的に割り切るのも一つの選択。

---

## (f) 出典一覧 (実在ファイル)

- 公式アーキテクチャ解説: `docs/architecture.md` (crate 表・Core/View/Term/Event 節)
- workspace 構成・依存: `/Cargo.toml` (workspace members / workspace.dependencies)
- モード・Document: `helix-view/src/document.rs` (`pub enum Mode`)
- グローバル状態: `helix-view/src/editor.rs:1273` (`pub struct Editor`)
- スプリットツリー: `helix-view/src/tree.rs` (`pub struct Tree` / `pub enum Content`)
- Selection: `helix-core/src/selection.rs` (冒頭 doc comment, `pub struct Selection` 417行目, `pub struct Range`)
- Transaction: `helix-core/src/transaction.rs` (`pub type Change`, `pub enum Operation`)
- 構文/言語: `helix-core/src/syntax.rs:275` (`pub struct Loader`), `helix-core/src/syntax/config.rs` (`LanguageConfiguration`)
- 移動アルゴリズム: `helix-core/src/movement.rs`
- Compositor/Component: `helix-term/src/compositor.rs`
- イベントループ: `helix-term/src/application.rs:294` (`event_loop_until_idle`), `helix-term/src/main.rs` (`#[tokio::main]`)
- キーマップ: `helix-term/src/keymap.rs:264` (`pub struct Keymaps`)
- コマンド: `helix-term/src/commands.rs`
- LSP: `helix-lsp/src/client.rs:56` (`pub struct Client`), `helix-lsp/src/lib.rs:581` (`pub struct Registry`, `OffsetEncoding`), `helix-lsp/src/jsonrpc.rs`
- DAP: `helix-dap/src/lib.rs`, `helix-dap/src/client.rs`
- VCS: `helix-vcs/src/lib.rs` (`DiffProviderRegistry`)
- grammar 管理: `helix-loader/src/grammar.rs`
- イベント/フック: `helix-event/src/lib.rs` (doc comment), `helix-event/src/debounce.rs` (`AsyncHook`)
- TUI: `helix-tui/src/lib.rs`
