# 現行資産の境界検証 — mina-view / mina-conn / プロトコル / 旧クライアント棚卸し

検証日: 2026-09-05（wayfinder チャーティングセッション追補として直接実施。delegate サブエージェントのランナーがクラッシュしたため、本セッションがコード読解で一次確認）。
チケット: 「現行資産の境界検証 — mina-view / mina-conn / プロトコル / 旧クライアント棚卸し」（GH issue #38）。
一次ソース: リポジトリのコード（ファイル:行を引用）。旧クライアントはコミット `006476e^` 時点。

## 1. mina-view の公開 API と UI 依存性 — **踏襲可**

- termina / termion / crossterm / ratatui への参照は **ゼロ**（`grep -rln` で mina-view / mina-conn / mina-protocol / mina-text を検索、一致なし）→ UI 非依存であることを一次確認。
- 公開面（`mina-view/src/lib.rs:12-20`）: `pub mod editor / history / mode / tree`、re-export は `Editor, DocumentId, View` / `History` / `Mode` / `SplitDirection, Tree, ViewId`。
- `Editor`（`editor.rs:47`）: documents / histories / paths / dirty / views / tree（分割ツリー）/ mode を保持する imperative shell。変換は `Transaction` として関数型コアに適用（ADR-0002）。
- `View`（`editor.rs:25`）: `{ doc: DocumentId, selection: Selection, first_line: usize }` — 選択と viewport 上端は**デーモン側**が持つ。`first_line` のカーソル追従スクロール（`editor.rs:566-573`）。
- `MAX_DOCUMENTS: usize = 8`（`editor.rs` 冒頭）: 上限超過時の安全弁。コメントに「タブ UI 等で複数文書を並行保持する必要が出たら LRU に置き換える」— v1 は実質 1 文書焦点の想定。
- → **UI 結合部は存在せず、mina-view はそのまま踏襲対象**。ただし「View はデーモン保有」「クライアントは viewport 高さを通知するだけ」の役割分担を新クライアント設計は尊重する。

## 2. キー→Command 解決の分担 — **解決はクライアント側**

- 現行ワークスペースに **Keymap 概念は存在しない**（mina-view / mina-protocol / mina-conn / minad / minas / mina-text で grep 一致なし）。デーモンは `Command` 列挙（`mina-protocol/src/lib.rs:67`〜）を直接実行する。
- 旧クライアント doc コメント（`006476e^:minae/src/client.rs:5-6`）: 「編集状態は持たない — キーイベントをキーマップで Command に解決して送る」→ キー→Command 解決は**クライアント側**の責務だった。
- 旧 `keymap.rs`: モード別 prefix トライ（`Vec<(KeyEvent, Node)>` 線形探索、`insert` / `get`、`Resolution` 列挙）。`Command` は `mina-protocol` のものを直接保持。
- `Command` 列挙の主な面（TUI が送るもの）: `GetState` / `WaitFor{generation}` / `Open{path}` / `Move` / `Extend` / `Goto` / `Scroll{pages}` / `SetMode` / **`SetViewport{height}`** / `Insert` / `Delete*` / `Select*` / `Append` / `OpenBelow` / `OpenAbove` / `Replace` / `Search*` / `ScrollHalf` / `KillToLineStart/End` / `Change` / `InsertAtLineEnd` ほか。
- → 新クライアントは **keymap モジュールを内蔵**（旧実装を移植ベースに再検討 — MVP 最小のバインディングから出す）。CONTEXT.md の Keymap 定義はクライアント側の概念。

## 3. mina-conn のクライアント向け API — **push 購読の永続接続ループはクライアント側実装**

- 提供部品（`mina-conn/src/lib.rs`）: `absolutize` / `connect` / `daemon_exe`（`MINAD_EXE` → PATH の `minad`）/ `spawn_daemon`（setsid で分離）/ `wait_ready` / `send_hello`（`ClientKind` + `reset_cursor_on_disconnect`、ADR-0012）/ `open_session`（**ワンショット**）/ `request`（`Response` | `Push` 双方のエンベロープから snapshot を取り出す）/ `request_hints`・`request_peek`（軽量応答の専用経路）。
- doc 明記: 「永続接続（push 購読、ADR-0013）の TUI は**独自の接続ループ**を持つため、この crate はワンショットを提供する」。
- ADR-0034 言及: 「将来の TUI リポジトリ（別バイナリ）からも使う想定」。
- → 新クライアントは mina-conn の部品を組み立てて**独自の永続接続ループ**（`connect` + `send_hello(Interactive)` + `request`/`WaitFor` + push 受信 + 自動起動）を実装する。接続断時の扱いは「イベントループとランタイム構成」チケットの決定事項。

## 4. StateSnapshot の全フィールド（`mina-protocol/src/lib.rs:733`〜）

| フィールド | 内容 |
|---|---|
| `text` | 全文（ADR-0006） |
| `checksum` | 全文 FNV-1a64（ADR-0012 #12） |
| `selection: Vec<Range>` + `primary_index` | アクティブ選択 |
| `mode` | Normal / Insert / Select |
| `first_line` | viewport 上端（クライアント描画の基準） |
| `diagnostics: Vec<Diagnostic>` | 診断（インライン⇄専用 view の素材） |
| `inlay_hints: Vec<InlayHint>` | フォーカス文書の inlay（ADR-0020） |
| **`highlights: Vec<HighlightRange>`** | **可視範囲のみ**（ADR-0021）。`HighlightRange { start, end: char index, group }` は `lib.rs:702` に定義。grammar 不在言語は空 — 構文ハイライト描画の前提成立 |
| `path: Option<String>` / `dirty` / `status` | 開いているファイル・未保存・一時メッセージ |
| `activities: Vec<Activity>` | 進行中非同期処理（ADR-0028、spinner 素材） |
| `generation` / `events: Vec<ChangeEvent>` | 世代・変更イベント輪（ADR-0012） |
| `deleted: Option<String>` | 外部削除待ち（ADR-0015） |
| `peek: Option<Peek>` | 定義プレビュー（`Command::PeekDefinition` の応答のみ） |

- **viewport 高**はスナップショットに載らず、クライアントが `Command::SetViewport { height }` で通知する — リサイズ/レイアウト変更時は必ず同期する契約。ハイライト窓（ADR-0021）・半分スクロール（`ScrollHalf`）はこの高さに依存。
- `ServerMessage` エンベロープ: `Response` / `Push` / `Hints` / `Peek` / `ServerInfo` / `RenameResult` / `ReferencesResult` / `Outline` / `Hover` / `WorkspaceSymbols` / `Check` / `EnclosingSymbol`。

## 5. 旧 minae 実装の棚卸し（`006476e^` 時点・移植/再検討の材料）

- **client.rs**: termina `EventStream` + `tokio::select!`、`mpsc`（`responses` / `pushes` / `semantic` の 3 チャネルでバックグラウンド接続タスクと分離）、`render::draw_with_cache`（`LineIndexCache` をフレーム間保持）、`draw_peek_popup` / `draw_refs_popup`。`Prompt`（`:`, `/`・`?`, `r`, `R` — クライアントローカルの入力バッファ、検索はライブ送信）。alt-screen / カーソル非表示は生エスケープ emit。
- **keymap.rs**: モード別 prefix トライ、`Resolution` 列挙（`Command(...)` ほか）、`Vec<(KeyEvent, Node)>` 線形探索（数十超えたら HashMap 化、の ponytail メモあり）。
- **config.rs**: `Config { colorscheme: Option<String>, reset_cursor_on_disconnect: bool }`（実質 2 キー）、`known_keys()`（typo/未対応キーの拒否）、`config_path()` = `~/.config/minae/config.toml`、`schemes_dir()`（ユーザーカラースキーム置き場、ADR-0022）。
- **colorscheme.rs**: `Color`（Ansi16/256/truecolor の SGR コード生成）、`Style { fg, bg, underline, reverse, dim, italic }`、`UiRole`（UI 要素の役割列挙）、`Colorscheme { name, syntax: [(HighlightGroup, Style)], ui: [(UiRole, Style)] }`、`resolve(name, schemes_dir)`（組込 → ユーザーファイル優先）、`ColorCapability` + `detect_capability()`（`NO_COLOR` > `COLORTERM` truecolor/24bit > 256 > Ansi16、ADR-0019）。
- **render.rs**: 毎フレーム全画面再構築（SGR 列を直接 emit）。全角は unicode-width で表示幅 2。差分描画なし（ratatui 側で解決される方針）。

## 新クライアント設計への示唆（決定は各チケットで）

1. **踏襲確定**: mina-text・mina-view（UI 結合なしを検証済み）・プロトコル契約。Keymap はクライアント再実装。
2. **新規実装が要る部分**: クライアント独自の永続接続ループ（mina-conn 部品の組み立て）、`SetViewport` 同期、keymap トライの移植、カラースキームのデータモデル（TOML・ADR-0022）を保ちつつ **ratatui::Style への変換層**（旧 `Color` は SGR 直生成 — ratatui 版は `ratatui::style::Color::Rgb/Indexed/Ansi` へのマップが必要）、`LineIndexCache`（全角含む行インデックス — ratatui のセルバッファに合わせて再考）、プロンプト UX（`:`, `/`, `?`, `r`, `R`）。
3. **MVP 最小の設定キー**: 旧実装は実質 `colorscheme` + `reset_cursor_on_disconnect` の 2 キー — 「MVP 最小（colorscheme 指定のみ）」とほぼ同義。`reset_cursor_on_disconnect` は Hello で宣言される（ADR-0027）。