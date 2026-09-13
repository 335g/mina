# push 専用サーバで「settle 済みの空」を提示しない

`ts-frontend` ラウンド（2026-09-13、driver 報告 #2/#7）で、**typescript-language-server
に対して minas の検証経路が丸ごと無い**ことが分かった。しかも悪いのは欠落そのものより
「空を settled として提示する」形だった:

- `check` は pull（`textDocument/diagnostic`）実装なので、`diagnosticProvider` を
  advertise しないサーバでは `check not supported …` / exit 1。TS では**構造的に使えない**
  （driver はアプリの `tsc --noEmit` / `vitest` をゲートに使った）。
- さらに `pull_diagnostics` は pull 非対応サーバに対して **`Some(vec![])`（空の成功）** を
  返していた。その結果、Open の診断 settle ループが「安定した空」を診断として確定させ、
  活動（`診断取得中`）が正常終了する。実測: 本物の TS2322 があるファイルで
  `get` の `diagnostics` が `[]` のまま「クリーン」と区別できず、しかも settle が
  完了したように見えた（driver の言葉: 緑のライトが空集合の上で点く）。

Status: accepted

## Decision

1. **`pull_diagnostics` は pull 非対応なら `None`（取得不能）を返す**。`Some(vec![])` は
   「診断が無い」と「取得できない」を潰す嘘で、ADR-0045/0058 の規律
   （見ていないものを確定として返さない）に反する。
2. **settle ループは pull 非対応サーバでは即座に return する**（settle する対象が無い）。
   活動の除去は既存の wrapper が行う。
3. **Open は 1 回だけ status で告げる**（新規 Open と既存文書の再 Open の両経路）:
   `<path>: diagnostics unavailable (the LSP server does not support pull diagnostics) —
   use the project's own check (tsc / vitest / cargo)`
   内部の一回限り警告チャネル（旧 `external_warning` → `pending_warning` に改名）を再利用し、
   wire 形状は変えない（`status` は既存フィールド）。
4. **`minas skill check` に transport を明記**: pull 専用であること、rust-analyzer は可 /
   typescript-language-server は不可、TS/JS のゲートはプロジェクト自身のツール
   （`tsc --noEmit` / `vitest`）であること。**push ベースの `check` は backlog**
   （購読して push 集合に独自の verdict を与える必要があり、taxonomy の判断を伴う）。

## Considered Options

- **push 診断を購読して `check` を二本立てにする**: 保留（backlog）— 正しい方向だが、
  「push がまだ来ていない」を clean と区別する verdict 語彙が必要（ADR-0045 の語彙は
  pull が何か返したことを前提にしている）。セッション同一性を変えるバッチに同乗させない。
- **空のまま黙って返す（現状）**: 却下 — 無言の偽グリーン。このプロジェクトの最上位の失敗クラス。
- **status ではなく新しいフィールドを足す**: 却下 — 既存の一回限り警告チャネルで足りる。
  wire を増やすと bump が要る。
- **`check` を「pull 非対応なら exit 0 の空」にする**: 却下 — もっと悪い偽グリーン。

## Consequences

- TS/JS では「診断は未取得」が明示され、settle 完了の見かけが消える。実測（自分の fixture）:
  Open 後に `status` が 1 回出て、`activities` は空、`diagnostics` は空のまま —
  ただし**何も「見た」と言っていない**。
- メトリクスの副次修正: `check_total` を `serve_check_diagnostics` の入口で数えるようにし、
  **失敗した check も数える**（driver の観測: 失敗した check が 8 回あっても
  `check_total` が動かず、検証の試行が観測から消えていた — ドッグフーディングのループが
  最も見たい信号）。
- 残る作業: push ベースの `check`（backlog）。それまでの TS ゲートは `tsc`/`vitest` で、
  driver がそう使ったことは fallback ではなく「アプリ自身のツールチェーン」として記録する。
