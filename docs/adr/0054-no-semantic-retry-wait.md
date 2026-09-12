# 意味的リクエストのリトライ待ち（SEMANTIC_RETRY_WAIT）を 0ms にする

`symbol` / `rename` / `references`（`request_with_loading_retry`）と `check`
（`pull_diagnostics_settled`）は、結果が「2 回連続で同一」になるまでリトライする。
その間の待ち `SEMANTIC_RETRY_WAIT` が **500ms** 固定で、1 回目の結果が既に完全でも
毎回必ず挟まっていた（結果の確認に 500ms 待ってから 2 回目を投げる）。

iteration #5（ADR-0053）の撤去と同じ性質を疑った: LSP の意味的リクエストは
**自身が解析完了までブロックしてから結果を返す**（pull 診断の round0 が最終集合、
ADR-0052）。ならば「待ってからもう一度聞く」の待ち部分は、解析と重なっていない
純粋な遅延（= 同じ答えをもう一度買っているだけ）のはず。

iteration #6 で 0ms の A/B を回した:

| step | 500ms（#5 基準） | **0ms** |
|---|---|---|
| `symbol`（explore/lsp） | 583ms | **82ms** |
| `rename`（rename/lsp） | ~1594ms | **~1150ms** |
| `check`（verify/apply-check） | 589ms | **87ms** |

回帰ゼロ: warm r=3 × 3 flow + cold（`--cold` rename / explore）+ `-r 10`（explore /
rename）= 40 run 相当で fails 0。`calls` / `out_B` / `equiv_B` は全 flow で不変。
`verify-broken/check` は rc=2 のまま、`verify-blind/check` は `settled:false` 契約を維持。
索引完走ゲート（ADR-0051）が cold の完全 rename / 非空 symbol を引き続き守っている。
`cargo test` 462 green。

「ローディング形（null / 空）のときだけリトライ」に縮める案（1 往復削減）も比較した
が、2 回目の確認は同一クエリの再送でほぼ無料（`symbol` 82ms vs 84ms の差なし）で、
代わりに loading 時に 100ms 待つコードが残るだけなので不採用。

Status: accepted

## Decision

1. **`SEMANTIC_RETRY_WAIT` を 500ms → 0ms にする**。リトライループと
   「2 回連続で同一 = 安定」の確認ロジック自体は**残す**（遅いサーバ・大きい
   プロジェクトでの安全弁。`SEMANTIC_RETRIES` の予算は維持）。
2. **待ちの予算（20×500ms ≈ 10 秒）は 0ms×20 になる**が、予算機構（ループ回数
   上限）はそのまま安全弁として残る。
3. **回帰したときは待ちを戻さない**。症状が「部分 rename / 空 symbol」なら、まず
   索引完走ゲート（ADR-0051）を疑い、次にリトライの「同一」判定の条件を疑う。

## Considered Options

- **100ms に縮める**: 却下 — 0ms で回帰しなかった。待ちを残す理由が無いなら
  （ADR-0053 と同じ論理）0ms 一択。
- **loading のときだけリトライ（1 往復削減）**: 却下（今は）— 実測で 2 回目確認が
  ほぼ無料（request 自身が ~80ms かかるので、その 1 往復を削っても誤差）。
  複雑さだけ増える。
- **500ms のまま**（この反復をやらない）: 却下 — `symbol` / `rename` / `check` が
  毎回 500ms、エージェントの探索・検証のたびに払っていた。

## Consequences

- `symbol` 583 → 82ms（−86%）、`check` 589 → 87ms（−85%）、`rename` ~1594 → ~1150ms
  （−28%。残りは RA 側の WorkspaceEdit 計算で待ちではない）。
- `calls` / `out_B` / `equiv_B` は不変（契約も wire 形状も変えないため
  PROTOCOL_VERSION は据え置き）。
- 予算切れの最終結果（空 / 不完全の可能性）を返す動作は変わらない。
- `pull_diagnostics_settled`（`check`）の「空は早期に返す（`SEMANTIC_EMPTY_ROUNDS` 回
  連続空で settled:false）」は待ち 0ms でもそのまま機能する（0ms なので「早期」は
  さらに早い。ADR-0052 Decision 3 の非空判定はロジックが残るので不変）。

## 関連

- 実装: `minad/src/lsp.rs`（`SEMANTIC_RETRY_WAIT`、`request_with_loading_retry`、
  `pull_diagnostics_settled`）
- 測定の記録: `docs/loop/log.md` iteration #6
- 判定の既存 ADR: ADR-0051（索引完走ゲート）、ADR-0052（pull は 1 回目で最終集合 /
  check の空の早期確定）、ADR-0053（編集後の fix 待ち撤去 — 同じ「解析完了まで
  block する」型の無駄待ち）、ADR-0045（空をクリーンの根拠にしない）