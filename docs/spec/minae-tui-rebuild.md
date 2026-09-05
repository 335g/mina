# minae TUI 再構築仕様書（ratatui/crossterm + per-client 分離）

> ステータス: **確定**（wayfinder マップ「minae TUI 再構想（ratatui/crossterm）— 再構築仕様の確定」の全チケット解決により編纂）。
> この 1 文書 + docs/adr/0035〜0039 + docs/spec/research/（調査成果 3 本）で、実装セッションは地図を開かずに構築を開始できる。
> 未決定事項が残っていないことを編纂時点で確認済み。実装時の注意事項は §16。

## 1. 目的と前提

- minae TUI を termina から **ratatui 0.30.2 + crossterm 0.29.0** へ全面換装してゼロベースで再構築する（ADR-0035）。
- 再構築の根幹 = **per-client フォーカス分離**（ADR-0037）: クライアント/エージェントごとに独立したフォーカス文書・カーソル・ビューポート。人間の操作がエージェントに引きずられない／逆もない。
- **状態の分裂はしない**: 文書集合・undo 履歴・LSP セッションはデーモン共有のまま（検証付き編集・自動リロード・dirty 追跡の正しさを維持）。
- 踏襲: **mina-text**（rope コア）完全踏襲 / **mina-view**（Editor/View/History/Mode/Split-tree。UI 非依存を検証済み — research）踏襲 / プロトコルの全文スナップショット契約（ADR-0006）は維持。

## 2. 採用ライブラリとクレート構成

| ライブラリ | バージョン | 用途 | 根拠 |
|---|---|---|---|
| ratatui | 0.30.2 | Terminal / Buffer diff 描画 / Layout / Widget | ADR-0035・R1 調査・プロト実証 |
| crossterm | 0.29.0 | イベント（EventStream）/ raw モード / 代替画面 | 同上（feature `event-stream`） |
| tokio | 既存 workspace | ランタイム（mina-conn が前提） | ADR-0036 |
| unicode-width | 既存 workspace | 全角表示幅 2 | 旧 render.rs 踏襲 |
| ropey / smallvec / unicode-segmentation | 既存 | mina-text | 踏襲 |

- クレート: **minae**（bin）が TUI 本体。mina-text / mina-view / mina-protocol / mina-conn / mina-lsp / mina-loader / minad / minas は既存のまま（protocol は v12 拡張、minas は `--name` 対応を追加、TUI は mina-conn の部品を利用）。
- **workspace の termina 依存宣言を撤去**する。

## 3. 実行モデル（ADR-0036）

- **単一 `tokio::select!` ループ**: crossterm `EventStream`・daemon ソケット読み・スピナー tick を同時待ち（`ratatui-async-template` 直系。タスク分割しない）。
- **描画トリガー**: スナップショット到着・キーイベント・リサイズ時のみ + `activities` 非空の間だけスピナー tick。フレーム毎の全画面再構築は `Buffer::diff` が吸収するので描画頻度は自由。
- **送信**: コマンド直列送信 + ソケット常時読み（`Response` = 直近コマンドの答え、`Push` = 逐次適用）。`WaitFor` は TUI では使わない。**request_id は未採用** — 根拠: デーモンは接続ごとに直列処理、対話入力は人間ペース（実測 248KB で ~10ms/打鍵 ≪ キーリピート 33ms）。発火条件 = 全文 RTT 30〜50ms 超の大ファイル帯（その際に再検討 — §16）。
- **起動・回復**: mina-conn の `daemon_exe`（`MINAD_EXE` → PATH の `minad`）/ `spawn_daemon`（setsid）/ `wait_ready` で自動起動。接続断はステータス報知 + 数秒バックオフ再接続、復帰時 `GetState` で再同期。
- **端末**: raw モード + 代替画面、終了/パニック時に復元（guard）。リサイズ `Resize` → レイアウト再計算 → `Command::SetViewport { height }` を直列経路で送信。

## 4. 分離の意味論（ADR-0037）

- **接続 = 1 View**（doc / selection / first_line）。mina-view の View 機構を接続ごとに割り当てる。文書・undo 履歴・LSP は共有。
  - 実装注: デーモンは「接続 → View」のマップを持ち、コマンド適用は発信接続の View に作用させる。スナップショットは接続の View から合成する。mina-view は「接続レス」の中立状態として維持。
- **ロードとフォーカスの切り離し**: ロード（Document）= 共有・パスが正統な実体（1 文書 1 インスタンス）。フォーカス（View）= 接続別。
  - `Open` = 「ロード（既ロードなら再利用）+ 自分の View をその文書へ」（Interactive）/「ロード + idle view の文書を更新」（ヘッドレス）。ロードのみ・フォーカスのみの別コマンドは追加しない（YAGNI）。
- **ヘッドレス**: View を持たない（パス明示コマンドの応答は target のスナップショット）。**共有 idle view** を 1 つ維持 — パス無し GetState（`session state` 相当）の観測点で「最後に開かれた文書」。エージェントの Open は idle view の文書を動かす。
- **`reset_cursor_on_disconnect`**（ADR-0027 の再解釈）: 最後の Interactive 切断時、その接続の Hello 宣言に従い **idle view のカーソルを先頭へ**（既定 true）。接続 View は切断で消えるため、リセット対象は idle view のみ。
- **push は購読者別合成**: 各 Interactive 購読者に「自分の View を反映した snapshot」を配る（グローバル snapshot のブロードキャストから変更。origin スキップ・世代不変不送の既存枠組みは維持 — research）。
- **LSP**: セッションは WorkspaceRoot 単位で複数並存（ADR-0010 既存）。セッション内は**共有解析フォーカス 1 文書**（最後に解析要求された文書 — フォーカス切替・エージェントの解析要求でも動く）。診断・inlay は解析フォーカス文書のものを、**その文書を見ている View のスナップショットにのみ**載せる。
  - 単一フォーカス文書は実装単純化（LSP の didOpen は複数文書可）— 複数開き・並列 root 閲覧（コミット比較等）への拡張を阻害しない設計原則として維持。

## 5. プロトコル v12（ADR-0039 の枠組み + 追加フィールド）

bump 方式で **PROTOCOL_VERSION 11 → 12**、新ソケット `minae-12.sock`。新デーモン + 新クライアントで一括導入。新旧は物理的に交わらない。

- **Hello 拡張**: `name: String`（既定 `"unknown"`。自己申告ラベル）。TUI は既定 `"tui"`。minas は `MINAE_CLIENT_NAME` / `--name`。
- **StateSnapshot 拡張**:
  - `activity: Vec<ActivityRecord>`（bounded ・既定 100 件。`{ actor, kind, ok, detail }`）— 更新で generation を進め push に乗せる。記録は Headless 由来の状態変更意図操作の成功・失敗（読み取り系は対象外）。ChangeEvent は現状維持で並存。
  - 接続別合成（§4）に伴う意味論変更: `path` / `selection` / `first_line` / `diagnostics` / `inlay_hints` / `highlights` / `activities` は「その接続の View」基準。
- **互換**: GetServerInfo は温存（拡張なし）。テストは v12 形状へ更新。
- 詳細: ADR-0037 / 0038 / 0039、research/connection-compat-findings.md。

## 6. レイアウト（P1 プロトタイプで確定）

- 基本: **本文（エディタ）+ ステータス行 1 行**。`Layout::vertical([Min(0), Length(1)])` の入れ子でオーバーレイ群を載せる。
- オーバーレイ（すべてホットキー開閉・デーモンコマンド不要のクライアントローカル表示モード）:
  - **ファイルツリー** = ほぼ全画面ピッカー型（周囲に小マージン）
  - **診断専用 view** = 全幅・高さ約 30%・下側固定（コードと同時可視）
  - **活動履歴** = 全幅・高さ約 30%・下側固定（診断 view と同型）
- オーバーレイ位置の設定ファイル化は将来オプション。

## 7. 描画（エディタ）

- ソース: `StateSnapshot`（text / first_line / selection / highlights（可視範囲・ADR-0021）/ diagnostics…）を毎フレーム描画。フレーム毎の全画面再構築でよい（diff あり）。
- **ガター**: 行番号（1 始まり・右詰め固定幅）。**診断マーカー列を行番号より左**に置く（ソースと区別するため。1 文字幅固定）。
- **インライン診断マーカー**: 該当行を覆う診断の**最上位重要度 1 文字**（Error > Warning > Info > Hint）。複数行スパン診断は各可視行にマーカー。ステータスに診断件数は出さない。
- **構文ハイライト**: スナップショットの `highlights`（HighlightRange = char index・可視範囲）を行に適用。grammar 不在言語は空。全角は unicode-width で表示幅 2（行インデックスのキャッシュ — 旧 LineIndexCache 相当を ratatui セルバッファに合わせて再実装）。
- **カーソル**: エディタのアクティブカーソルを自色で描画（`set_cursor_position`）。操作は**カーソル移動式**（ビューポートはカーソル追従スクロール — ADR-0023 の既定）。
- デーモンからの応答は全文スナップショット（ADR-0006 維持）— 受信ごとに再描画。

## 8. ステータス行

- **左**: モード + パス **のみ**。診断件数・generation は表示しない。
- **右**: 報知エリア（スピナー（`activities`）+「今の活動」（`activity` 最新 1 件 / ChangeEvent））+ キーヒント。
- 見た目は P1 プロトタイプで確定（配色の実値は §11 で調整）。

## 9. キーマップ

- **構成**: クライアント側・モード別（Normal / Insert / Select）prefix トライ（旧 `keymap.rs` を移植 — `Vec<(KeyEvent, Node)>` 線形探索・`Resolution` 列挙）。キー → `mina_protocol::Command` を解決して直列送信（解決はクライアント側 — research）。
- **モード別バインディング**（旧 keymap.rs のバインディング表を移植。以下は MVP のコア）:
  - Normal: `h`/`j`/`k`/`l`（Char/Line 移動）・`w`/`b`/`e`（単語）・`g g`（先頭）・`G`（末尾）・`Home`/`End`（行頭/末尾）・`i`/`a`/`o`/`O`（挿入・Helix 流）・`x`（行選択）・`Esc`（Normal 復帰）・`/`・`?`（検索・ライブ送信）・`:`（コマンドライン）・`Space k`（PeekDefinition）・`r`（置換）・`R`（リネーム）
  - Insert: 未バインドキーは挿入文字へ（Insert fallback）。`Esc` で Normal。単語削除（`C-w` 等）
  - Select: 移動 = Extend（movement_bindings(true) を共用）
- **オーバーレイキー（新規・暫定）**: `T` = ツリー / `G` = 診断 view / `A` = 活動履歴 / `C` = カラースキーム切替（デバッグ用でも可）。確定は実装時のキーマップチューニングへ委ねる。
- プロトタイプの `G` キー案はそのまま採用（診断 view）。

## 10. 設定（config.toml）

- 場所: `~/.config/minae/config.toml`（クライアントローカル。デーモンは読まない — CONTEXT.md の Config）。
- **MVP 最小キー**: `colorscheme: Option<String>` のみ（他は `known_keys` で未知キーとして拒否 — 旧 config.rs の機構を移植）。
- `reset_cursor_on_disconnect` は config ではなく **Hello 宣言**（ADR-0027・既存）。
- 読込タイミング: クライアント起動時（変更は次回起動から反映）。

## 11. カラースキーム

- データモデル: 旧 `colorscheme.rs` 踏襲（TOML ファイル・組込 + ユーザーファイル優先（ADR-0022）・`Colorscheme { name, syntax: [(HighlightGroup, Style)], ui: [(UiRole, Style)] }`）。
- **組込 2 種**: `iceberg-dark`（Default）/ `catppuccin-mocha`。**実値はプロトタイプの近似値から実物の palette に調整する（必須 TODO）**。
- **変換層**: 旧 `Color`（SGR 直生成）を廃止し、`ratatui::style::Color::Rgb/Indexed/Ansi` へのマッピング層を実装（UiRole → ratatui Style）。
- `ColorCapability` 検出（ADR-0019: `NO_COLOR` > `COLORTERM` truecolor/24bit > 256 > Ansi16）は維持し、変換に適用。

## 12. ファイルツリー

- **提示**: ホットキーで開閉する**ほぼ全画面ピッカー型オーバーレイ**（周囲に小マージン。他の情報に意識が行かないようにする）。
- **根**: 起動時 **cwd 固定**。開いているファイルを**ハイライト**（フォーカス文書への追従はしない — 将来オプション）。
- **表示と開閉**: 初期はルート直下のみ（ディレクトリは畳んだ状態）。**Enter でディレクトリを開くと 1 段下の階層**を表示。並び順: ディレクトリ優先・名前順（仮置き）。
- **操作範囲**: **Open のみ**（ナビゲーション専用。新規作成・削除・リネームは別目的地）。Enter = Open → 自分の View へ（§4 の Open 意味論）。
- ツリーのデータはクライアント側の fs 走査（プロトコル変更なし）。

## 13. 診断表示

- **2 モード**: エディタ内インライン ⇄ 専用 view。切替はクライアントローカル（キーは §9 — `G` 暫定）。
- **インライン**: §7 のとおり（行番号より左・最上位 1 文字）。
- **専用 view（オーバーレイ）**:
  - 全幅・高さ約 30%・**下側固定**（コードと同時可視 — view が下ならコードは上）。設定ファイルでの変更は将来。
  - **アクティブな診断 1 件のみ表示**（全文・1 行制限なし・アクションなしでできるだけ全文表示）。診断リストの全件は出さない。
  - **重要度フィルタ**（ホットキーで Error / Warning / Info / Hint 切替）: 選択した重要度の診断だけがアクティブ候補。
  - **重要度マーカーエリア（左上）**: 存在する重要度にマーカー、アクティブな重要度だけハイライト色。
  - **「次へ」キー**: フィルタ内の次の診断へ進み、**コード側も対応位置へ同期移動**（診断 range の start（char → 行・列変換）へカーソル移動 + ビューポート追従）。
- **非モーダル・アクティブ追従**:
  - フォーカス切替式（view ⇄ エディタ。両側でナビ・編集可能 — 診断を見て直す動線）。
  - 専用 view 表示中は**必ずアクティブ診断箇所をフォーカス**。乖離は「専用 view に戻ったタイミングでエディタ位置をアクティブ診断位置へリセット」で解消。
  - **修正 → 保存でアクティブ診断が消えたら、フィルタ内の次の診断へ自動前進**（スナップショットの診断変化に追従）。
- 診断の由来: 解析フォーカス文書（= 自分の View の文書）のときスナップショットに載る（§4）。

## 14. 活動可視化（ADR-0038）

- データ: `activity`（§5）。actor は自己申告ラベル（`unknown` 既定）。
- **表示**: ステータス右側報知エリアに「今の活動 1 件」+ 活動履歴オーバーレイ（§6: 全幅・30%・下側・診断 view 同型）+ **フィルタ（全 / 成功のみ / 失敗のみ）** — エラー率評価用。切替キーは「`A`」暫定（§9）。
- エージェントのカーソル可視化は**不採用**（長命セッション新設は将来目的地）。

## 15. 接続と自動起動（再掲）

- 起動: `daemon_exe` → なければ `spawn_daemon` → `wait_ready`。起動失敗は明示エラー。
- 接続断: ステータス報知 + バックオフ再接続（数秒間隔）。復帰は `GetState` で再同期。終了は明示操作のみ。
- 常駐デーモンの状態は切断後も保持（デーモンの責務）。

## 16. 実装時の注意・将来事項（fog からの引き継ぎ）

- **カラースキーム実値**: プロトタイプの近似値 → 実物 palette（iceberg-dark / catppuccin-mocha）へ調整必須（§11）。
- **request_id の発火条件**: 全文 RTT 30〜50ms 超が計測されたら再検討（§3）。
- **大ファイル帯の線形スケーリング**: 全文スナップショット + フル再構築のスケーリングは未実測 — 別目的地（#32 由来の fog）。
- **分割・タブ・複数 View の UI**: MVP 外（mina-view の Tree 機構はデーモンが未使用のまま）。
- **inlay hints / Peek ポップアップの描画**: 旧 render.rs に実装あり・MVP 外。
- **キーマップ override 等の設定拡張**: MVP 外（known_keys 拒否で明示的）。
- **ツリー根のフォーカス追従・オーバーレイ位置の設定化**: 将来オプション。
- **長命エージェントセッション・コミット比較（複数 root 並列閲覧）**: 別目的地。LSP の複数開き・解析フォーカスの拡張で阻害しない設計（§4）。
- **多クライアント・大規模時の push 合成コスト**: ピア数増加時に差分化を検討。

## 付録: 決定ソースの索引

- ADR: 0035（backend）・0036（event loop）・0037（分離）・0038（活動可視化）・0039（protocol v12）
- research: `docs/spec/research/ratatui-ecosystem-findings.md`（ライブラリ事実）/ `mina-assets-findings.md`（現行資産境界）/ `connection-compat-findings.md`（接続・互換）
- プロトタイプ: `/tmp/minae-layout-proto`（使い捨て・見た目確定の実物）
- マップ: GitHub issue #36（Decisions so far に各チケットの要旨とリンク）