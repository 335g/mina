# Research: ratatui / crossterm エコシステム事実の調査（GH issue #37）

- 元チケット: wayfinder マップ「minae TUI 再構想（ratatui/crossterm）— 再構築仕様の確定」(issue #36) の研究タスク #37
- 調査者: research サブエージェント（一次ソース遡及調査）
- 調査対象リポジトリ: `/Users/335g/dev/other/mina`（workspace `edition = "2024"`, `rust-version = "1.96"`, `tokio 1.53.1` を Cargo.lock で固定）
- 調査方法と検証レベル:
  - `crossterm 0.29.0` はローカル Cargo レジストリに実ソースが存在したため、`Cargo.toml` / `Cargo.toml.orig` / `src/event.rs` / `src/event/stream.rs` / `src/event/read.rs` / `src/event/source/unix/mio.rs` / `src/terminal/sys/unix.rs` / `examples/event-stream-tokio.rs` を直接読んで裏取りした（**一次確認**）。
  - `ratatui` は本環境にソースが無く、また本調査環境には Web 検索/取得ツールが無かったため、公式ドキュメント・公式リポジトリの正規 URL を引用し、バージョン番号等の時間依存の値は「要検証」フラグを付けた（**公式ドキュメント引用 / 要検証**）。

## Summary

crossterm 0.29.0（2025 年リリース、edition 2021 / MSRV 1.63.0）は本リポジトリの Rust 1.96・edition 2024 ワークスペースでも素直にビルドできる（workspace の edition は依存クレートに影響しない）。ratatui の安定版は 0.29.x 系が最新ライン（0.28 は 2024 年、0.29 は 2025 年リリース、MSRV 1.74 系）で、最新の確定値は crates.io API で再確認が要る。レンダリングは「毎フレーム Frame ごと全画面を再構築 → Terminal::draw が Buffer レベルの diff を取り変更セルのみ出力」が公式の設計であり、旧実装の「毎フレーム全画面再描画」方針は ratatui 上では自然な慣用としてそのまま成立する。イベントは crossterm の `EventStream`（feature `event-stream`、tokio/async-std 両対応）と `tokio::select!` の組み合わせが定石で、公式 example・公式 async テンプレートに実例がある。レイアウトは `Layout` + `Constraint` の入れ子分割、ウィジェット間状態は `StatefulWidget`（状態はアプリ側保持）が公式の慣習。

## Findings

### (a) 安定バージョン・MSRV・Rust 1.96 / edition 2024 互換性

1. **crossterm 0.29.0 の実体（一次確認）** — ローカルレジストリの `Cargo.toml.orig` に `edition = "2021"`・`rust-version = "1.63.0"` と明記。default features は `["bracketed-paste", "events", "windows", "derive-more"]`、`EventStream` はオプション feature `event-stream`（`futures-core` 依存）。我々のワークスペースで使うのは `events`（デフォルト）+ `event-stream` で足りる。MSRV 1.63 は Rust 1.96 よりはるかに低く、edition 2021 の外部クレートは workspace の `edition = "2024"` の影響を受けないため互換性に問題はない。 [crossterm v0.29.0 Cargo.toml.orig](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/Cargo.toml.orig) / [docs.rs crossterm](https://docs.rs/crossterm/latest/crossterm/)
2. **ratatui の安定版は 0.29.x 系（要検証）** — 0.28.x は 2024 年、0.29.x は 2025 年にリリースされた安定ライン。本環境では「今日時点の crates.io 最新値」を生で確認できなかったため、確定番号（0.29.1 か、それ以降が存在するか）は `https://crates.io/api/v1/crates/ratatui`（または [crates.io/crates/ratatui](https://crates.io/crates/ratatui)）で確認すること。 [docs.rs ratatui](https://docs.rs/ratatui/latest/ratatui/) / [ratatui GitHub releases](https://github.com/ratatui/ratatui/releases)
3. **ratatui の MSRV（要検証）** — 0.28.x 時点の公式リリースノートでは MSRV 1.74。0.29.x でも同水準とみられるが生値は `docs.rs/ratatui` の Cargo.toml（`rust-version`）で確認を。いずれにせよ 1.96 を下回るため、workspace の `rust-version = "1.96"` のまま共存できる。 [ratatui リリース](https://github.com/ratatui/ratatui/releases) / [docs.rs ratatui](https://docs.rs/ratatui/latest/ratatui/)
4. **ratatui はデフォルトで crossterm バックエンドを同梱** — `ratatui` の default features に `crossterm` があり、`ratatui::crossterm` として再公開される。つまり「ratatui 1 本に依存すれば crossterm の event / terminal API も同じ crate から使える」構造（裏で `CrosstermBackend` を使う）。crossterm を別途直接依存に足す必要はない（我々の workspace には現時点で ratatui/crossterm は未導入: Cargo.lock に無し）。 [docs.rs ratatui クレートルート](https://docs.rs/ratatui/latest/ratatui/) / [ratatui Cargo.toml](https://github.com/ratatui/ratatui/blob/main/Cargo.toml)

### (b) レンダリングモデル: Buffer / Frame と「毎フレーム全画面再構築」

5. **モデル**: `Terminal<B: Backend>` は直近に描画した内容を Buffer（画面上の `Cell` グリッド）として保持し、`Terminal::draw` は (1) 新しい `Frame`（毎回新規 Buffer を持つ）を作り、(2) draw クロージャ内でウィジェット群を再描画し、(3) 前回の Buffer と今回の Buffer の diff を取り、変更されたセルだけを Backend 経由で端末に書き出す。 [Buffer::diff](https://docs.rs/ratatui/latest/ratatui/buffer/struct.Buffer.html#method.diff) / [Terminal::draw](https://docs.rs/ratatui/latest/ratatui/terminal/struct.Terminal.html#method.draw) / [Backend trait](https://docs.rs/ratatui/latest/ratatui/backend/trait.Backend.html)
6. **「毎フレーム全画面再構築」は公式の想定どおりで、差分描画は依然有効** — 公式 examples・公式 async テンプレートはすべて、フレームごとに draw クロージャ内で画面全体のウィジェット（レイアウト含む）を組み立て直す書き方をしている。diff 計算は `Terminal`（`Buffer::diff`）側が行うため、「毎フレーム全構築」はパフォーマンス問題にならない。旧実装の方針をそのまま ratatui に移行できる。 [ratatui examples](https://github.com/ratatui/ratatui/tree/main/examples) / [Buffer::diff](https://docs.rs/ratatui/latest/ratatui/buffer/struct.Buffer.html#method.diff)
7. **`Frame` の API**: 0.28 で `Frame::size()` は `Frame::area()` に改名（`size()` は deprecated）。draw クロージャの引数は `&mut Frame` で、`render_widget` / `render_stateful_widget` / `set_cursor` 等を提供。`Terminal::autoresize()` はバックエンドの現在サイズへ自動追従（draw 時にも呼ばれる）。 [Frame ドキュメント](https://docs.rs/ratatui/latest/ratatui/terminal/struct.Frame.html) / [Terminal::autoresize](https://docs.rs/ratatui/latest/ratatui/terminal/struct.Terminal.html#method.autoresize)

### (c) crossterm の event API・raw モード・リサイズ

8. **`Event` enum（一次確認・crossterm 0.29.0）**: `Key(KeyEvent)` / `Resize(u16, u16)` / `Mouse(MouseEvent)` / `FocusGained` / `FocusLost` / `Paste(String)`（`bracketed-paste` feature）。`KeyEvent` は `code` / `modifiers` / `kind`（Press/Repeat/Release）/ `state` を持つ。 [crossterm src/event.rs](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/event.rs) / [docs.rs crossterm::event](https://docs.rs/crossterm/latest/crossterm/event/index.html)
9. **同期 API**: `event::poll(Duration)`（ブロックしない判定）と `event::read()`（ブロッキング読取）が基本。`poll` が `Ok(true)` を返せば `read` は非ブロックで取れる。 [docs.rs crossterm::event](https://docs.rs/crossterm/latest/crossterm/event/index.html) / [crossterm src/event.rs](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/event.rs)
10. **`EventStream`（一次確認・feature `event-stream` で有効化）**: `futures_core::Stream<Item = io::Result<Event>>` を実装する、async-agnostic なイベントストリーム。tokio・async-std どちらのランタイムでも使えると公式ドキュメントに明記。内部は孤立スレッドの `poll_internal` + 2 種類の waker で実装されており、`select!` ループで他の Future と並行待ちできることを意図した設計。 [crossterm src/event/stream.rs](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/event/stream.rs)
11. **raw モード（一次確認）**: `terminal::enable_raw_mode()` / `disable_raw_mode()` は Unix では tty の termios を `make_raw` し、元のモードを保存して disable 時に復元（二重 enable は無害）。公式ドキュメントの定義では「入力が画面にエコーされない・エンターで処理されない・ラインバッファされない・CTRL+C 等をドライバが処理しない」。`println!` は使えず `write!` が必要。 [crossterm src/terminal/sys/unix.rs](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/terminal/sys/unix.rs) / [crossterm src/terminal.rs](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/terminal.rs)
12. **別画面（alternate screen）**: フルスクリーン TUI の定石は `execute!(stdout, EnterAlternateScreen)` / `LeaveAlternateScreen`（raw モードとは独立）。vim 同様、終了時に元の画面が残る。 [crossterm src/terminal.rs（Screen Buffer 節）](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/terminal.rs)
13. **リサイズ = SIGWINCH → `Event::Resize`（一次確認・crossterm 0.29.0）**: Unix の内部イベントソースは `mio` で tty fd と `signal-hook` の `SIGWINCH` を登録し、SIGWINCH を受けると最新の端末サイズを `terminal::size()` で取得して `Event::Resize(cols, rows)` をイベントキューへ合成する。アプリ側は特別なシグナル処理なしに `Event::Resize` を読むだけでよい。さらに ratatui 側は `Terminal::autoresize()`（draw 時に自動）で内部サイズを追従させる。 [crossterm src/event/source/unix/mio.rs](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/event/source/unix/mio.rs) / [Terminal::autoresize](https://docs.rs/ratatui/latest/ratatui/terminal/struct.Terminal.html#method.autoresize)

### (d) tokio と EventStream の並行ループ（tokio::select）

14. **crossterm 公式 example（一次確認・event-stream-tokio.rs）**: `EventStream::new()` を作り、ループ内で `futures::select!` により「1 秒ごとの Delay」と「`reader.next()`」を同時待ち。イベント種別を match で分岐して Esc で終了、前後で `enable/disable_raw_mode` を呼ぶ、という最小の async イベントループが公式実例として提供されている。 [crossterm examples/event-stream-tokio.rs](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/examples/event-stream-tokio.rs)
15. **判例パターン**: `select!` の枝に「`event_stream.next()`」「定期 tick（`tokio::time::interval`）」「内部メッセージ（`tokio::sync::mpsc` の recv）」を並べ、どの入力でもループが回って毎フレーム draw する、が ratatui 公式 async エコシステムの支配的な構造。`StreamExt::next()` が返す Future はループで再利用する場合 `.fuse()`（FusedFuture）にするのが要点（crossterm 例は `futures::select!` で自動 fuse、`tokio::select!` では自分で fuse するか毎回新 Future を作る）。 [ratatui-async-template（ratatui 公式）](https://github.com/ratatui/ratatui-async-template) / [ratatui examples/async.rs](https://github.com/ratatui/ratatui/blob/main/examples/async.rs) / [crossterm examples/event-stream-tokio.rs](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/examples/event-stream-tokio.rs)
16. **公式 async テンプレートの構成**: `ratatui-async-template` は「crossterm `EventStream` + tokio + `select!`」の 1 ループで、キー入力・リサイズ・外部メッセージ（mpsc）・定周期 tick を扱い、各イベントで状態を更新して描画を要求する構成。非同期/同期の混在（`std::sync::Mutex` vs `tokio`）に注意するよう guide 側でも言及がある。 [ratatui-async-template](https://github.com/ratatui/ratatui-async-template) / [ratatui-book（公式解説書）](https://github.com/ratatui/ratatui-book)
17. **tokio との型互換**: crossterm `EventStream` は runtime 非依存（`futures-core::Stream`）なので tokio の exec 上で `next()` を await できる。tokio と `futures` クレート間の `StreamExt` は `futures`（または `futures-util`）側をインポートするのが定石。 [docs.rs crossterm::event::EventStream](https://docs.rs/crossterm/latest/crossterm/event/struct.EventStream.html) / [crossterm src/event/stream.rs](https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/event/stream.rs)

### (e) Layout / Constraint とウィジェット間の状態管理

18. **分割の定石**: `Layout::default().direction(Direction::Vertical).constraints([...]).split(area)` で `Rc<[Rect]>` を得て、各ウィジェットに割り当てる。エディタ + サイドバー + ステータスラインの構造は「外側 Vertical：`[Min(0), Length(1)]`（本体＋ステータス 1 行）→ 内側 Horizontal：`[Min(0), Percentage(...)]`（エディタ＋サイドバー）」の入れ子が典型。 `Constraint` は `Length` / `Min` / `Max` / `Percentage` / `Ratio` / `Fill` を使い分ける（`Fill` は余剰分配）。 [Layout](https://docs.rs/ratatui/latest/ratatui/layout/struct.Layout.html) / [Constraint](https://docs.rs/ratatui/latest/ratatui/layout/enum.Constraint.html) / [Rect](https://docs.rs/ratatui/latest/ratatui/layout/struct.Rect.html)
19. **Layout 新 API（要検証・0.29 系）**: 0.29 で `Layout::areas()` / `Layout::area()`（分割結果を配列/単一 Rect で受ける）と `Layout::vertical()` / `Layout::horizontal()`（方向指定ショートカット）が追加され、従来の `Layout::default().direction(...).split()` は deprecated 化の流れ。正確な導入区分は docs.rs で確認を。 [ratatui リリースノート](https://github.com/ratatui/ratatui/releases) / [Layout ドキュメント](https://docs.rs/ratatui/latest/ratatui/layout/struct.Layout.html)
20. **ウィジェット状態の慣習**: ratatui は「Widget（一時的な見た目）と State（アプリが保持する状態）の分離」が公式設計。`StatefulWidget` トレイトに `State` を渡し、`Frame::render_stateful_widget` で描画する（`List` / `Table` / `Scrollbar` などが該当）。カスタムのエディタウィジェットも、状態（テキスト・カーソル・選択・スクロールオフセット）はアプリ側 struct に持ち、ウィジェットは参照で描画する `StatefulWidget` にするのが慣行。公式 examples の `stateful_widget.rs` / `demo2.rs` に実例。 [StatefulWidget](https://docs.rs/ratatui/latest/ratatui/widgets/trait.StatefulWidget.html) / [ratatui examples/stateful_widget.rs](https://github.com/ratatui/ratatui/blob/main/examples/stateful_widget.rs) / [ratatui examples/demo2.rs](https://github.com/ratatui/ratatui/blob/main/examples/demo2.rs)
21. **ウィジェット間の状態共有**: Frame は使い捨てで、ウィジェットは「状態の所有者」にならずアプリの状態 struct から毎フレーム導出される。複数ペインで同期が必要な状態（例: エディタの選択範囲をステータスラインに出す）はアプリ側で一元管理し、描画時に各ウィジェットへ参照を渡すのが公式エコシステムの標準パターン。 [ratatui examples](https://github.com/ratatui/ratatui/tree/main/examples) / [ratatui-book](https://github.com/ratatui/ratatui-book)

## Sources

- Kept: crossterm v0.29.0 実ソース一式（ローカルレジストリで全文確認）— バージョン・MSRV・features・Event 列挙・EventStream 実装・SIGWINCH/Resize 合成・raw モード・公式 tokio example の一次根拠。GitHub の v0.29.0 タグ URL へ対応付け
  - https://github.com/crossterm-rs/crossterm/blob/v0.29.0/Cargo.toml.orig
  - https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/event.rs
  - https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/event/stream.rs
  - https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/event/source/unix/mio.rs
  - https://github.com/crossterm-rs/crossterm/blob/v0.29.0/src/terminal/sys/unix.rs
  - https://github.com/crossterm-rs/crossterm/blob/v0.29.0/examples/event-stream-tokio.rs
- Kept: ratatui 公式ドキュメント（docs.rs / GitHub）— Frame・Buffer::diff・Terminal の差分描画・Layout/Constraint・StatefulWidget・examples の事実源
  - https://docs.rs/ratatui/latest/ratatui/（および terminal/Frame・buffer/Buffer・layout/Layout・widgets/StatefulWidget 各ページ）
  - https://github.com/ratatui/ratatui/tree/main/examples
  - https://github.com/ratatui/ratatui/blob/main/examples/async.rs
- Kept: ratatui-async-template（ratatui 公式組織リポジトリ）— tokio::select! を使った公式アプリ構造の実例
  - https://github.com/ratatui/ratatui-async-template
- Kept: 本リポジトリの Cargo.toml / Cargo.lock — workspace edition 2024・rust-version 1.96・tokio 1.53.1・ratatui/crossterm 未導入の裏付け
- Dropped: 二次情報ブログ・まとめ記事（ratatui の diff 描画や EventStream の解説記事類）— 一次ソース（ドキュメント/ソース）で直接検証できるため採用しない
- Dropped: 旧バージョン（crossterm 0.28 以前・ratatui 0.27 以前）の記述 — 本チケットは現行安定系を対象とするため

## Gaps

1. **ratatui の「今日時点の」安定バージョン番号と MSRV の生値** — 本環境に Web 検索/取得ツールがなく、ratatui ソースもローカルに存在しなかったため未検証。確認手順: `curl -s https://crates.io/api/v1/crates/ratatui`（`max_version`/`rust_version`）または https://crates.io/crates/ratatui、docs.rs の Cargo.toml 表示。crossterm 側は 0.29.0 を一次確認済み。
2. **ratatui 0.29 新 API の正確な導入区分**（`Layout::areas`/`area`/`vertical`/`horizontal`、`ratatui::init()`/`restore()`、`WidgetRef` 等）— 「0.29 で追加、旧 API は deprecated 化」とする認識だが、各 API の正確な導入バージョンは docs.rs で要確認。
3. **0.30 系以降の存在** — 本調査時点で 0.30 以降がリリース済みかどうかは確認できなかった。routing 前に crates.io で最新を確認すること。

提案する次の一手: チャーティングセッション（または Web アクセス付きエージェント）で crates.io API から ratatui 最新版・MSRV を取得し、本ドキュメントの「要検証」箇所を確定させる。

## Supervisor coordination

本調査は制約下で実行した: サンドボックスに Web 取得ツールが無く、ratatui のバージョン系の「生の一次値」のみ未検証（上記 Gaps）。crossterm 側は実ソースでの一次確認を完了。追加の判断依頼・進行調整は不要（成果は本ドキュメント自体）。
## 追補: バージョン確定（2026-09-05 チャーティングセッション追補）

上記 Gaps 1〜2 を、crates.io API と**公開済み crate 配布物**の一次確認で確定する（取得元: crates.io API `max_version` / 配布物 download、User-Agent 付き）。

- **ratatui 最新安定 = 0.30.2**（crates.io `max_version` = `newest_version`、公開 2026-06-19）。配布物 `ratatui-0.30.2/Cargo.toml`: **edition 2024・rust-version 1.88.0** — 本リポジトリ（Rust 1.96 / edition 2024）とそのまま共存可。
- **crossterm 最新安定 = 0.29.0**（crates.io `max_version`、公開 2025-04-05）。研究者のローカル一次確認（edition 2021・MSRV 1.63.0）と一致。
- **`Layout::areas()` は 0.29.0 で導入済み**（配布物 `ratatui-0.29.0/src/layout/layout.rs:528` `pub fn areas<const N: usize>(&self, area: Rect) -> [Rect; N]`）。「0.29 系で追加」の認識を一次確認。
- したがって仕様上の検討対象ラインは **ratatui 0.30.x + crossterm 0.29.0**（イベントループ構成の決定チケットで採用可否を判断）。
