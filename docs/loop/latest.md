# minas/minad ループエンジニアリング — 最新の検討結果（次のループの起点）

> git 管理（`docs/loop/`）。更新: iteration #8 完了時（2026-09-12）。
> 入口は [`README.md`](./README.md)。検討方法は [`method.md`](./method.md)。
> **このファイルと method.md を読めば次のループを回せる。**
> 生データと全反復の考察は同じディレクトリの [`log.md`](./log.md)。
> 設計判断は `docs/adr/0051`（索引完走ゲート）/ `0052`（check の空の早期確定）/
> `0053`（編集後の settle を置かない ＋ 追記: pull は撤去できない）/
> `0054`（意味的リトライ待ち 0ms）/ `0055`（編集後 pull の背景化 — **iteration #8**）/
> `0045`（`settled` の意味）。

## 1. 現在の状態（30 秒サマリ）

- 完了した反復: **#1 計測器 → #2 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定
  （ADR-0052）→ #4 初回 apply の内訳確定 → #5 編集後の settle 撤去（ADR-0053）→
  #6 SEMANTIC_RETRY_WAIT 撤去（ADR-0054）→ #7 apply の pull 撤去（棄却。ADR-0053 追記）→
  #8 編集後 pull の背景化（ADR-0055）**
- 効果（L0 実測、warm r=3 中央値）:
  - `check`（クリーン）**10154ms → 87ms（−99%）**（#3 + #6 + #8 で維持）
  - `symbol`（explore 1 歩目）**583 → 82ms（−86%）**（#6）
  - `rename`（lsp）**1594 → ~1150ms（−28%）**（#6。残りは RA 側の WorkspaceEdit 計算）
  - `apply` **~1.1s → ~130ms（−88%）**（#8。pull が応答をブロックしなくなった。
    エージェントの Save 応答待ちが消えた。背景 pull は次ターン推論中に走る）
  - ギャップありの **apply + check の和: ~1270ms → 222ms（−82%）**、cargo 経路も
    **−77〜82%**（apply-cargo 1344→240ms・hunks-cargo 1450→256ms・apply2-cargo
    1524→344ms）
- cold の無言の誤り（空の `symbol`・部分 `rename`・`check` の LSP error）は 0 件のまま。
  `calls` / `out_B` / `equiv_B` は全 flow で不変（回帰なし）。`--cold` の完全性も維持。
- 全 462 テスト green。`python3 docs/loop/l0.py --selftest` green。
- **#8 の結論**: 「`sync_after_edit` の pull は Save 応答のクリティカルパスから外せる
  （背景タスク化）」は**採用**（ADR-0055）。pull を外す（#7）と次に pull した者が
  解析を払うが、**応答を待たせない**形ならエージェントの次ターン（LLM 推論。
  数秒）の間に背景 pull が解析を終える。編集後追従の契約（テスト 3 件）は維持。
  **§4 の前提誤りも発見**（下記）: 「テストは snapshot を poll で待つ」は誤りで、
  実際は編集コマンドの応答自体で検証していた。検証ポイントを poll に移し 462 green。
- **次の課題（#9）は計測基盤の穴埋め: daemon に計時ログ（tracing）を足し、
  ServerMetrics の穴（rename カウンタ）を埋める**。理由は §4。

## 2. 確定した事実（再測定は不要）

| 事実 | 根拠 |
|---|---|
| 読む量＝費消の主因。範囲 read（`symbol`→`at`→`read --lines`）は全文 dump より `equiv_B` **−83%**（1568 vs 9260） | L0 `explore`（iteration #1〜#3 で安定） |
| 編集は**内容指定**（`apply`）。位置指定は失敗しやすい | T3（L2）/ L0 で `apply` が正面入口 |
| 複数編集は `--hunks-stdin` で往復削減（2 編集: 3 calls/545 → 2 calls/308 equiv、−44%） | L0 `verify/hunks-cargo` vs `apply2-cargo` |
| 複数箇所・複数ファイルのリネームは LSP `rename`（2 calls/438 vs apply ループ 5 calls/1424） | L0 `rename`（T5/T9 を再現） |
| **rust-analyzer は `window.workDoneProgress` を advertise しないと `$/progress` を送らない** | プローブ実測（`probe_progress.py`）/ ADR-0051 |
| 索引完走（`cachePriming` = title "Indexing" の end）を待たないと、`symbol` が空・`rename` が部分適用を「成功」報告・`check` が LSP error | L0 `--cold`（iteration #2 前）/ ADR-0051 |
| **pull 診断は 1 回目の要求で最終集合を返す**（round0 == round11）。空が後から非空に変わることは無い | プローブ実測（`probe_pull_diagnostics.py`）/ ADR-0052 |
| pull が**見る**: 構文エラー / 同一ファイル内の型エラー（`no such field` 等）。**見ない**: メソッド解決エラー（`cfg.validate2()` は永久に空）、クロスファイル型エラー | 同上。だから `check` の空は `settled:false`（未確認）で、`cargo` が最終的な根拠 |
| 空応答の settle 予算（20×500ms）と Open の 30 秒 settle は「同じ答えしか返らない待ち」 | ADR-0052 |
| **編集後の固定 settle（`PULL_SETTLE`）は撤去できる**（ADR-0053）。250ms → 0ms で `apply` が毎回 −250ms、空 pull 回帰は `-r 10` × 3 flow = 30 run で 0 件 | iteration #5（L0 + プローブ直叩き） |
| **初回 apply の ~1.1s は RA の初回再解析であって待ちではない**: 同じ daemon で `symbol`/`check` を先に叩いてから `apply` すると **137ms / 98ms / 102ms** | iteration #5 の下調べ（`tmp/loop/recon6.py`）+ #6/#7 でも不変 |
| **`SEMANTIC_RETRY_WAIT` 500ms の撤去は回帰ゼロ**（0ms。ADR-0054）。「2 回連続同一」の確認自体は同一クエリの再送でほぼ無料 | iteration #6（warm -r3 全 flow + cold + -r10 で fails 0） |
| **`rename` の残り ~1150ms は RA 側の WorkspaceEdit 計算**（2 回目の確認を省いた案 B でも 1162ms = 1 回目の request 自体が ~1s） | iteration #6 の案 B 比較 |
| **`hints` の 770ms は `pull_inlay_hints` の RA 側計算**。`request_with_loading_retry` を通らない（直接 request 1 回）ので 500ms 撤去の影響を受けない | iteration #6 |
| **`apply` の ~1.1s は「pull が強制する RA 解析」そのもの**。診断 pull だけ外しても hint pull が同じ解析を買うので `apply` は不変（1155ms = 基準値）。両方外すと `apply` 153ms だが、その解析は次に pull した者（`check` 1122ms）が必ず払う | iteration #7 A/B(a)(b) |
| **編集後の pull は契約**（daemon のスナップショットが編集後診断・ヒントを反映する）。外すと `inlay_hints_follow_edits_and_snapshot` / `get_inlay_hints_serves_arbitrary_path_and_restores_focus` / `reopen_rs_path_respawns_lsp_and_reannounces_current_text` が落ちる（計 3 件） | iteration #7（`cargo test`） |
| **cargo 検証経路では `apply` の先払い解析が二重払い**（RA 解析と rustc コンパイルは別物）。pull を外すと apply-cargo **1344→268ms（−80%）**、apply2-cargo **1524→361ms（−76%）**、hunks-cargo **1450→275ms（−81%）** | iteration #7 A/B(b) |
| **編集後 pull の背景化で Save 応答の解析待ちが消える**（ADR-0055）。apply の wall は **~130ms で安定**（#7 の 700/1100ms の割れは解消 — 「解析を買う」のが apply 応答から背景タスクに移った）。ギャップあり（sleep 3 = 推論遅延の代理）で apply + check の和 **222ms（現行比 −82%）**、check step 92ms | iteration #8（L0 `verify-gap` 追加。warm -r3） |
| **ギャップなし（即時 check）では check が解析を買う**（apply ~130ms + check ~800ms = ~1.0s。現行 1.2s よりは速い・悪化なし）。「解析は誰かが買う」は背景化後も不変 — 差は「誰が・いつ買うか」だけ | iteration #8（`verify/apply-check` -r10: 904〜1047ms） |
| **編集後追従のテスト 3 件は「応答時点」で検証していた**（§4 #8 の前提誤り）。`request()` は応答を返し途中の push は読み飛ばす。背景化で応答は pull 前の値になり 3 件が落ちた → 検証ポイントを「追従までの poll（最大 10 秒）」に移して 462 green を維持（テストの意図 = 編集後に追従することは不変） | iteration #8（`cargo test`。ADR-0055） |
| cold の支配項は**索引完走ゲート**（`symbol`/`hints`/`rename`/`check` が 8〜9 秒。`apply` は cold でも 120〜140ms — 背景化後も 130〜136ms で不変） | L0 `--cold`（#2〜#8 で安定） |

**棄却済み（同じ仮説を再試行しない）**:

| 棄却した仮説 | 理由 |
|---|---|
| `apply`→`check` が cargo 委譲より**常に**安い | 当時 `calls`/`equiv_B` は同等で `wall` が 7.4 倍（10154 vs 1569ms）。**iteration #3 で check は 589ms になり逆転した**ので、この棄却は「当時の契約では成立しない」だけ。行番号つき診断 = `check` / 最終的な根拠 = `cargo`（+ 実プロジェクトでの wall の巻き返し）という**使い分けを L2 で測り直す**のが正しい（§4 の別候補） |
| 初回 apply の ~1.1s は **hint pull が主因** | iteration #4: `pull_after_edit` から hint pull を外して A/B → 1 回目 1373→1422ms・2 回目 352→342ms と不変。主因は初回 diag pull 内の RA 再解析（トレースで確定）。#7 で「どちらの pull でも同じ解析を買う」ことが確定した |
| hint の消費者は `minas hints`（自前 pull・往復 1 回・ADR-0020）と daemon キャッシュ（=`minas get` の `inlay_hints`）だけ。**`minae`（TUI）は inlay hint を描画しない**（`minae/src/colors.rs:177`「MVP では描画しない」/ `render.rs:1158` はテスト用） | iteration #4 で確認 |
| cold でも warm と同じ結果を返す | 無言の誤り 3 件（ADR-0051） |
| 空 + 索引完走 = クリーン確定（`settled:true`） | pull が実在するエラーを取りこぼす（ADR-0045 追記） |
| skill 参照が判断を変える | T6/T8（L2）: トークン +21〜61% で効果なし |
| **`PULL_SETTLE` は「解析前の空」を避けるために要る**（削ると診断が消える） | iteration #5: 50ms・0ms のどちらでも空 pull 回帰が出ない。settle は買えるものが無い純粋な遅延だった → 撤去（ADR-0053） |
| **「2 回連続同一」を捨て、loading（null / 空）のときだけリトライすれば 1 往復削れる** | iteration #6 の案 B: `symbol` 84ms（案 A 82ms）・`rename` 1162ms（案 A 1150ms）と差なし。代わりに loading 時 100ms 待つコードが残るだけ → 不採用 |
| **`SEMANTIC_RETRY_WAIT` を 50ms・100ms 等に縮めて様子見する** | iteration #6: 0ms で回帰ゼロだったので二分探索の必要が無かった（ADR-0054）。0ms 一択 |
| **`apply` の診断 pull を外せば 1.1s が消える**（`settled:false` 即返し・契約変更） | iteration #7: A/B(a) は `apply` 不変（1155ms）で daemon テスト 2 件 fail。A/B(b) は `apply` 153ms だが `check` 1122ms（apply-check のトータル ±0）でテスト 3 件 fail。1.1s は pull が強制する解析で、pull を外すと次に pull した者が払う。編集後追従の契約も壊れる（ADR-0053 追記） |
| **診断 pull だけ外す（hint pull は残す）で `apply` が 720ms になる** | iteration #7: 同じバイナリの再測で 1155ms（基準値と同値）。719ms は Open 時背景 settle との競合で先に解析が済んでいた場合の観測だった（揺れ。「700ms / 1100ms に割れる」） |
| **背景化ではテスト 3 件（編集後追従）が落ちる = 契約が壊れる** | iteration #8: 素の背景化では落ちたが、テストが検証していたのは「応答時点の追従」= 実装の偶然（同期 pull が応答前に完了していた）。契約の本質（編集後に追従すること）は背景 pull + push でも満たされる。検証ポイントを poll に移して 462 green（ADR-0055） |

## 3. 基準値（L0、iteration #8 で再測）— 次回の比較はここから

### warm（r=3 中央値。verify 系 + verify-gap は #8 の実装後、explore/hints/rename は #6 の値）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `verify/apply-check` | 2 | 246 | 354 | 1020 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 240 | 0 | True |
| `verify/hunks-cargo` | 2 | 154 | 308 | 256 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 344 | 0 | True |
| `verify-gap/apply-gap-check` | 3* | 262 | 494 | 3239 | 0 | True |
| `verify-gap/apply-gap-cargo` | 3* | 116 | 348 | 3272 | 0 | True |
| `verify-broken/check` | 2 | 450 | 555 | 483 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 217 | 0 | True |
| `verify-blind/check` | 2 | 240 | 344 | 606 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 266 | 0 | True |

*verify-gap の calls=3 は計測 step の `sleep 3`（エージェントの次ターン推論遅延の
代理）を含むため。契約の往復は apply + check/cargo の 2 回のまま
（`out_B` / `equiv_B` も既存 verify と同値）。

ステップ別（`-v`、1 run の観測。`calls` / `equiv_B` は決定論的、`wall` は揺れる）:

```
verify/apply-check     apply 140ms → check 447ms     ← check が解析を買う（ギャップなし。和 ~1.0s）
verify-gap/apply-gap-check  apply 130ms → sleep 3 → check 92ms   ← 背景 pull が推論中に解析を終える（和 222ms）
verify/apply-cargo     apply 119ms → cargo 121ms     ← 先払い解析が応答から消えた（#7 の二重払い解消）
verify/apply2-cargo    apply 132ms → apply 84ms → cargo 128ms
verify-broken/check    apply 126ms → check 245ms（rc=2 / 345B = Syntax Error 2 件）
verify-blind/check     apply 129ms → check 477ms（settled:false 契約を維持）
explore/lsp            symbol 82ms → at 129ms → read 84ms
hints/hints            hints 770ms（RA 側計算）
rename/lsp             rename 1150ms → cargo 168ms（RA の WorkspaceEdit 計算）
```

**注意（#8 で確定）**: `apply` の wall は **~130ms で安定**（#7 の 700/1100ms の
割れは解消）。代わりに**ギャップなしの check が解析を買う**（~800ms、和 ~1.0s）。
「解析は誰かが買う」原則は不変。比較のときは `-r 3` 以上で、適用後すぐ check を
打つケース（`verify/apply-check`）と、推論のギャップがあるケース
（`verify-gap/apply-gap-check`）を分けて見る。

### cold（1 回観測、iteration #8 で verify 系のみ再測。他は #6 の値）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `verify/apply-check` | 2 | 246 | 354 | 8690 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 425 | 0 | True |
| `verify/hunks-cargo` | 2 | 154 | 308 | 429 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 510 | 0 | True |
| `verify-gap/apply-gap-check` | 3 | 262 | 494 | 8747 | 0 | True |
| `verify-gap/apply-gap-cargo` | 3 | 116 | 348 | 3307 | 0 | True |
| `explore/lsp` | 3 | 1017 | 1568 | 8205 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 169 | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 8067 | 0 | True |
| `rename/lsp` | 2 | 219 | 438 | 9127 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 597 | 0 | True |
| `verify-blind/check` | 2 | 240 | 344 | 9095 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 434 | 0 | True |
| `verify-broken/check` | 2 | 450 | 555 | 8793 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 338 | 0 | True |

cold の支配項は**索引完走ゲート**（`symbol`/`hints`/`rename`/`check` が 8〜9 秒。
`apply` は cold でも 130〜136ms — 背景化後も悪化なし）。ゲートは背景 pull と
独立の機構。

計測の内訳が知りたいとき: `python3 docs/loop/l0.py -f <flow> -v`。

## 4. 次の課題設定（iteration #9）

> **iteration #8 の結末（この課題の起点）**: 「pull は Save 応答から外せる（背景化）」は
> 採用（ADR-0055）。`apply` の wall が **~130ms で安定**し、ギャップありの
> apply + check の和は **222ms（現行比 −82%）**、エージェントの Save 応答待ちは
> **−88%**。編集後追従の契約（テスト 3 件）は検証ポイントを poll に移して維持。
> 同時に分かったのは (1) 「解析は誰かが買う」原則は背景化後も不変（ギャップ
> なしでは check が ~800ms を買う。実フローでは LLM 推論が間に合う）、
> (2) **計時ログが無いので、コマンド内部の内訳は A/B 除去か一時トレースでしか
> 測れない**（check の ~800ms が「背景 pull との競合」なのか「RA の再解析」なのか、
> 今回も直接測れなかった）、(3) `ServerMetrics` に `rename` のカウンタが無い等、
> 計測の穴が残っている（method.md §7）。

### 課題

**daemon に計時ログ（tracing）を足し、コマンド内部の内訳を恒久的に測れるようにする**。
対象は (a) `sync_after_edit`（didChange → 診断 pull → ヒント pull の各フェーズ）、
(b) `serve_check_diagnostics`（pull の待ち内訳）、(c) `request_with_loading_retry`
（リトライ回数）、(d) `ServerMetrics` の穴（`rename` のカウンタ、
`get_state_total` の bytes）。エージェントの動作は変えない（追加は観測のみ）。

### 仮説

計時ログがあれば:
- ギャップなし check の ~800ms が「（背景 pull と並行して）RA が didChange 後の
  再解析を 1 回だけ買う」ことを確定でき、削る手がかり（あるいは「削れない =
  実フローでは推論が間に合うので問題ない」の確定）になる。
- 将来の仮説（apply2 の 2 回目が 84ms なのに 1 回目が 130ms の差、hints の 770ms
  の内訳等）を 1 回の計測で分解できる。A/B 除去のたびにコードを 2 本持つ作業
  （#4〜#7 で毎回やった）が不要になる。

### 測り方（順序を守る）

1. **実装（観測のみ・動作不変）**: `minad` に軽量の計時（`tracing` または
   `ServerMetrics` へのカウンタ追加）を入れる。
   - 方針は「どこに何を足すか」を先に書く（method.md §4 の計測の穴を埋める）。
   - エージェントの wire 形状・応答・契約は変えない（PROTOCOL_VERSION 不変）。
2. **回帰**: `cargo test` **462 green**、L0 `-r 3` で fails 0・
   `calls`/`out_B`/`equiv_B` 不変（計時が wall を歪めないこと — 計時自体の
   オーバーヘッドが同定できるなら、その分を考慮）。
3. **効果確認**: 計時ログで (i) ギャップなし check の ~800ms の内訳、
   (ii) apply の 130ms の内訳、(iii) rename の 1150ms の内訳（server 応答待ち vs
   client 処理）を出す。**これが次反復（#10）の課題設定の入力になる** —
   #9 自体は基盤整備で、エージェントのコストは（多少の計時オーバーヘッドを
   除き）変わらない。
4. **→ 次反復 #10 の課題設定**: #9 の内訳に基づき、(a) check の ~800ms を
   削る / 許容するの判定、(b) rename の 1150ms をどう扱うか（非同期化は
   次ステップの入力になるため難しい。エージェント側の使い分けの検証）、
   (c) 実フロー（L2）での確認、のどれかを選ぶ。

### 受理条件 / 棄却条件

- **受理**: 計時ログが (i)(ii)(iii) の内訳を出せる・462 test green・L0 で回帰なし
  （wall が計時で有意に歪まない）。基盤として有用なら採用。
- **棄却**: (a) 計時で wall が歪み、L0 の基準値が 10% 以上悪化する、
  (b) 計時ログが内訳を出せない（tracing が機能しない・計測に使えない）、
  (c) 実装が大きくなりすぎて 1 変更に収まらない（観測以外の動作を変えている）。

### 別候補（後回し）

- **L2（実 LLM）で verify-gap 相当の実フロー確認**: 背景化の効果は wall（待ち）
  なので L2 のトークン計測では見えない。L2 で確認できるのは「回帰なし
  （calls/equiv_B 不変・無言の誤りなし）」だけ。tools/ab にタスク追加が必要。
- **ギャップなし check の ~800ms の削減**: 「解析は誰かが買う」原則があるため、
  削るには「apply 直後の check が背景 pull の結果を待つ」等の形になる
  （= settle の再導入に近い。ADR-0052/0053 の精神と要調整）。実フローでは
  推論が間に合うので優先度は低い。**#9 の内訳を見てから判断する**。
- `apply --no-diagnostics`（#8 の別候補）: 背景化で既定 apply が 130ms になった
  ので、必要度は下がった。
- `open_workspace_files`（rename/references の前に全ファイル didOpen する回避策）
  が索引完走ゲートの導入後も必要か。ただし cold の主因はゲートで、fixture が
  小さいため L0 では didOpen 分の差が出ない（規模の課題 = L2 向き）。
- L2 での規模検証（fixture が小さいので `wall_ms` は外挿不可。`calls` / `equiv_B`
  は外挿可）。
- 上位モデルでの再現（L2 は nano のみ）。
- 計時ログが無いことは「コマンド内部の内訳は A/B 除去か一時トレースでしか測れない」
  ことを意味する（特に #7 の「Open 時背景 settle と編集後 pull の競合」は、
  計時ログが無いと apply の wall 分散を説明できなかった → #8 の背景化で解消）。
  **この穴を埋めるのが #9 本体**。

## 5. 次のセッションの起動手順

**新しいセッションの入口**（`AGENTS.md` から `docs/loop/README.md` に辿れる。
プロンプトテンプレート `.pi/prompts/dev_minas.md` = `/dev_minas` でも同じ指示を展開できる。
手で貼るならこれ）:

```text
minas/minad のループエンジニアリング（エージェントのコスト低減）の続きをやって。
まず docs/loop/method.md と docs/loop/latest.md を読み、
latest.md の §4（次の課題設定）から iteration #9 を回して。計測したら --log で
log.md に記録し、考察を追記し、latest.md を更新して。
```

**注意**: `tmp/loop/`（結果 JSON・fixture）は checkout ごとの gitignore 対象。
worktree で測る場合はそちらに作られる（この文書群は git 管理なので worktree にもある）。
`minad` / `minas` を止めずに複数の l0 実行を重ねない（索引・CPU が競合して `wall_ms` が濁る。
`l0.py` はアームごとに専用 daemon を立てるので、同時実行は避ける）。

```bash
cd /Users/335g/dev/other/mina
cargo build                                  # 計測対象は target/debug。コードを触ったら必須
                                             # （cargo clean すると target/debug が消え、
                                             #  l0.py は PATH の minas/minad に黙って落ちる）
cargo test                                   # 期待値: 462 passed（毎回確認）
python3 docs/loop/l0.py --selftest          # 計測器の健全性
python3 docs/loop/l0.py -f verify -f verify-gap -f verify-broken -f verify-blind -r 3 -v   # 現状確認（apply ~130ms / check 92ms / ギャップあり和 222ms）
git log --oneline -5                         # 直前の変更を確認（auto-checkpoint が並ぶ）
```

そのあと §4 の「測り方」1 → 2 → … の順に進める。
`--log` は必ず付ける（JSON は `tmp/loop/` に落ちるだけなので、付け忘れると生データが消える）。
`apply` を含む比較は `-r 3` 必須・**同一セッションで交互に**測る（§3 の注意）。

**反復番号の規則**: 今回の反復番号は §4 の見出しの番号（いまは **iteration #9**）。
結果は `log.md` に「iteration #9」として書き、考察の要約をこの `latest.md` に反映したうえで、
§4 を次の番号（#10）の課題設定に書き換える。§5 はこの手順のまま使う。