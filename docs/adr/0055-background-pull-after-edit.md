# 編集後の pull を Save 応答から外し背景タスク化する

iteration #7 までに分かっていたこと: `apply`（Save 応答）の ~1.1s は
**pull が強制する RA 解析**そのもので、diagnostics と hints の両方の pull が
同じ解析を買う（pull を外すと次に pull した者が必ず払う）。しかし編集後の
pull は **「daemon のスナップショットが編集後の診断・ヒントを反映する」契約**
（テスト 3 件 + TUI 表示）を担っており、外すと契約が壊れる（#7 で A/B 確定）。

iteration #8 で「pull を**外す**」のではなく「**応答を待たせない**」方向を試した:
`sync_after_edit` の pull とその反映を `tokio::spawn` の背景タスクに移し、
Save 応答は現状のスナップショット（= pull 前の診断・ヒント）を即返す。
背景タスク完了時に push で全購読者へ編集後の診断・ヒントを配る
（Open の背景 settle — `settle_open_diagnostics` — と同じ形。
応答をブロックしない・発信元はコマンドでないため `None` を包む）。

エージェント（headless）の次ターンは LLM 推論で数秒かかるため、その間に
背景 pull が解析を終え、後続の `check` は温まった解析を得られる — という仮説。

## 実装中の前提の誤り（この ADR が記録すべき追加事実）

課題設定（latest.md §4）は「テストは snapshot を poll で最大 10 秒待つ」と
読んでいたが、**実際のテスト 3 件は編集コマンドの応答スナップショット自体に
編集後の診断・ヒントを要求していた**（`request()` は応答を返し、途中の push は
読み飛ばす）。背景化すると応答は pull 前の値になり、3 件とも落ちる。

- `inlay_hints_follow_edits_and_snapshot`: 応答の `inlay_hints[0].position` が
  編集後（6）であることを要求
- `get_inlay_hints_serves_arbitrary_path_and_restores_focus`: 応答の
  `diagnostics[0].start` と `inlay_hints[0].position` が編集後であることを要求
- `reopen_rs_path_respawns_lsp_and_reannounces_current_text`: 応答の
  `diagnostics[0].start` が編集後であることを要求

テストの意図は「編集（didChange）→ 診断・ヒントの追従」で、応答時点の検証は
「同期 pull が応答前に完了していた」という実装の偶然に依存していた。契約の
本質（編集後に追従すること）は背景 pull + push でも満たされるため、3 件の
検証ポイントを「応答」から「追従までの poll（最大 10 秒）」に移した
（コメントに ADR-0055 を追記）。テストの意味する契約は変わらない。

## Decision

- `sync_after_edit` の第 1 引数を `&Arc<Mutex<Daemon>>` に変え、`push_tx` を
  受け取る。`target` が `Some` のとき、LSP 同期（`lsp::sync`）・pull
  （`pull_after_edit`）・daemon への反映・push を `tokio::spawn` で実行する。
  応答は共通経路（`drain_into` + `snapshot(status)`）で即返す
  （= pull 前の診断・ヒント。テキスト・世代は編集済み）。
- ロック規律は Open の背景 settle と同じ: daemon ロックを await またぎで持たず、
  LSP の await はロック外（`lsp::sync` / `pull_after_edit` はロック取得を
  `LSP_LOCK_TIMEOUT` で諦める設計のため、次のコマンドを待たせない）。
- `activity` の追加・除去も背景タスク側に移す（応答に「同期中」が残らない —
  リロード時のみ使用。ADR-0028）。
- 背景タスクの push は発信元がコマンドでないため `(None, snap)` で送り、
  全購読者（TUI）に届く。受信側は内容比較で再描画する（既存の settle push と同様）。
- wire 形状は不変（PROTOCOL_VERSION は上げない）。`apply` の応答テキスト
  （`applied: …`）も不変。

## Options Considered

- **pull を外す（#7 で棄却済み）**: 次に pull した者（`check`）が解析を払う +
  編集後追従のテストが落ちる。→ 却下（この反復では取らない）。
- **`apply --no-diagnostics`（エージェントが契約を選ぶ）**: 背景化より単純だが
  既定の `apply` は 1.1s のまま。→ 後回し（latest.md §4 参照）。
- **テストを書き換えず背景化を見送る**: ⇒ 1.1s は「編集と解析が直列である限り
  エージェントが払う」と確定して終わる。背景化でエージェントの待ちが −80% に
  なることが計測で示されたため却下。

## Consequences

計測（L0。warm r=3 中央値。`apply` を含む比較は同一セッションで交互に測定）:

| flow/arm | 現行（#7） | 背景化後 | 差 |
|---|---|---|---|
| `verify-gap/apply-gap-check`（ギャップ 3s あり） | apply ~1228ms + check 87ms | **apply 130ms + check 92ms** | **−82%（和 222ms）** |
| `verify-gap/apply-gap-cargo` | apply-cargo 1344ms | **126ms + cargo 133ms** | **−81%** |
| `verify/apply-check`（ギャップなし） | 1228ms | **~1020ms（apply ~130ms + check ~800ms）** | −17%（非悪化） |
| `verify/apply-cargo` | 1344ms | **240ms** | −82% |
| `verify/hunks-cargo` | 1450ms | **256ms** | −82% |
| `verify/apply2-cargo` | 1524ms | **344ms** | −77% |
| `verify-broken/check` | rc=2（345B） | **rc=2（345B）維持** | 不変 |
| `verify-blind/check` | settled:false | **settled:false 維持** | 不変 |

- `calls` / `out_B` / `equiv_B` 不変（apply と check の契約はそのまま）。
- 空 pull 回帰 0（`-r 10` × 5 arm = 50 run で fails 0）。
- cold の `apply` 130〜136ms（背景化前の 120〜140ms と同等 — 悪化なし）。
- 462 test green（編集後追従 3 件は poll 検証に変更）。
- ギャップなし apply-check の check ~800ms は「didChange 直後の初回再解析」を
  check 自身が買うため。ギャップあり（エージェントの次ターン推論中）なら背景
  pull が解析を終えて check は 92ms。エージェントの待ち = apply 応答時間だけを
  見れば 1.1s → 130ms（**−88%**）。

注意: TUI（Interactive）の編集直後の表示は、pull 前の診断・ヒントになる
（push で 1 回追従する。headless の `apply` 応答は診断を出力しないため影響なし）。

## 関連

- 実装: `minad/src/daemon.rs`（`sync_after_edit`、呼び出し側 3 箇所
  `watch_disk` / Command 編集 / DocumentEdit）
- 測定の記録: `docs/loop/log.md` iteration #8（`verify-gap` flow を追加）
- 判定の既存 ADR: ADR-0053（編集後 pull の settle 撤去・pull は撤去できない）、
  ADR-0052（pull は 1 回目で最終集合）、ADR-0051（索引完走ゲート）、
  ADR-0028（Activity）