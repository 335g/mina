# minas/minad ループエンジニアリング — 最新の検討結果（次のループの起点）

> git 管理（`docs/loop/`）。更新: iteration #7 完了時（2026-09-12）。
> 入口は [`README.md`](./README.md)。検討方法は [`method.md`](./method.md)。
> **このファイルと method.md を読めば次のループを回せる。**
> 生データと全反復の考察は同じディレクトリの [`log.md`](./log.md)。
> 設計判断は `docs/adr/0051`（索引完走ゲート）/ `0052`（check の空の早期確定）/
> `0053`（編集後の settle を置かない ＋ **追記: pull は撤去できない**）/
> `0054`（意味的リトライ待ち 0ms）/ `0045`（`settled` の意味）。

## 1. 現在の状態（30 秒サマリ）

- 完了した反復: **#1 計測器 → #2 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定
  （ADR-0052）→ #4 初回 apply の内訳確定 → #5 編集後の settle 撤去（ADR-0053）→
  #6 SEMANTIC_RETRY_WAIT 撤去（ADR-0054）→ #7 apply の pull 撤去（棄却。ADR-0053 追記）**
- 効果（L0 実測、warm r=3 中央値）:
  - `check`（クリーン）**10154ms → 87ms（−99%）**（#3 + #6）
  - `symbol`（explore 1 歩目）**583 → 82ms（−86%）**（#6）
  - `rename`（lsp）**1594 → ~1150ms（−28%）**（#6。残りは RA 側の WorkspaceEdit 計算）
  - `apply` **1 回目 ~1.1s / 2 回目 ~100ms**（#5 以降不変）。#7 でこの 1.1s の正体が
    **「pull が強制する RA 解析」**であることを A/B で確定（撤去はできない — §2）
- cold の無言の誤り（空の `symbol`・部分 `rename`・`check` の LSP error）は 0 件のまま。
  `calls` / `out_B` / `equiv_B` は全 flow で不変（回帰なし）。`--cold` の完全性も維持。
- 全 462 テスト green。`python3 docs/loop/l0.py --selftest` green。
- **#7 の結論**: 「`apply` の診断 pull を外せば 1.1s が消える」は**棄却**（ADR-0053 追記）。
  (a) 診断 pull を外しても hint pull が同じ解析を買うので `apply` は不変。
  両方外すと `apply` は **153ms** になるが `check` が **1122ms** を払う（トータル ±0）。
  (b) 編集後の pull は「daemon のスナップショットが編集後診断を反映する」契約を担っており、
  外すと daemon テストが 3 件 fail（TUI が編集前の診断を表示し続ける）。
  **収穫**: cargo 検証経路ではこの先払いが純粋な二重払いで、外すと **−70〜80%**
  （apply-cargo 1344→268ms、apply2-cargo 1524→361ms）。取り出すには「ブロックしない」
  形（背景化）が要る。
- **次の課題（#8）は、`sync_after_edit` の pull を Save 応答のクリティカルパスから外す
  （背景タスク化）**。契約（編集後追従の 3 テスト）を保ったまま `apply` を ~150ms に
  できるかの A/B（§4）。

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
| **編集後の pull は契約**（daemon のスナップショットが編集後診断・ヒントを反映する）。外すと `inlay_hints_follow_edits_and_snapshot` / `get_inlay_hints_serves_arbitrary_path_and_restores_focus` / `reopen_rs_path_respawns_lsp_and_reannounces_current_text` が落ちる（計 3 件） | iteration #7（`cargo test`。診断 pull だけ外すとこのうち 2 件） |
| **cargo 検証経路では `apply` の先払い解析が二重払い**（RA 解析と rustc コンパイルは別物）。pull を外すと apply-cargo **1344→268ms（−80%）**、apply2-cargo **1524→361ms（−76%）**、hunks-cargo **1450→275ms（−81%）** | iteration #7 A/B(b) |
| **`apply` の wall は 700ms / 1100ms に割れる**（Open 時の背景 settle と編集後 pull が同じ解析を奪い合うため）。同一バイナリで 807ms と 1241ms の両方を観測 | iteration #7（A/B(a) の 1 回目と再測）。apply の wall を 1 回の観測で結論しない |
| cold の支配項は**索引完走ゲート**（`symbol`/`hints`/`rename`/`check` が 8〜9 秒。`apply` は cold でも 120〜140ms） | L0 `--cold`（#2〜#6 で安定） |

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
| **「2 回連続同一」を捨て、loading（null / 空）のときだけリトライすれば 1 往復削れる** | iteration #6 の案 B: `symbol` 84ms（案 A 82ms）・`rename` 1162ms（案 A 1150ms）と差なし。2 回目の確認は同一クエリの再送でほぼ無料。代わりに loading 時 100ms 待つコードが残るだけ → 不採用 |
| **`SEMANTIC_RETRY_WAIT` を 50ms・100ms 等に縮めて様子見する** | iteration #6: 0ms で回帰ゼロだったので二分探索の必要が無かった（ADR-0054）。0ms 一択 |
| **`apply` の診断 pull を外せば 1.1s が消える**（`settled:false` 即返し・契約変更） | iteration #7: A/B(a) は `apply` 不変（1155ms）で daemon テスト 2 件 fail。A/B(b) は `apply` 153ms だが `check` 1122ms（apply-check のトータル ±0）でテスト 3 件 fail。1.1s は pull が強制する解析で、pull を外すと次に pull した者が払う。編集後追従の契約も壊れる（ADR-0053 追記） |
| **診断 pull だけ外す（hint pull は残す）で `apply` が 720ms になる** | iteration #7: 同じバイナリの再測で 1155ms（基準値と同値）。719ms は Open 時背景 settle との競合で先に解析が済んでいた場合の観測だった（揺れ。§2 の「700ms / 1100ms に割れる」） |

## 3. 基準値（L0、iteration #7 で再測）— 次回の比較はここから

### warm（r=3 中央値）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `verify/apply-check` | 2 | 246 | 354 | 1228 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 1344 | 0 | True |
| `verify/hunks-cargo` | 2 | 154 | 308 | 1450 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 1524 | 0 | True |
| `verify-broken/check` | 2 | 450 | 555 | 984 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 995 | 0 | True |
| `verify-blind/check` | 2 | 240 | 344 | 940 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 903 | 0 | True |

（#7 で再測したのは verify 系。`explore` / `hints` / `rename` は iteration #6 の値のまま —
前回との差は 1〜4B の表示揺れで、契約の形は同じ:

| flow/arm | calls | out_B | equiv_B | wall_ms |
|---|---|---|---|---|
| `explore/lsp` | 3 | 1014 | 1562 | 310 |
| `explore/dump` | 2 | 4750 | 9260 | 178 |
| `hints/hints` | 1 | 110 | 110 | 783 |
| `rename/lsp` | 2 | 217 | 434 | 1364 |
| `rename/apply` | 5 | 402 | 1410 | 1779 |

）

ステップ別（`-v`、1 run の観測。`calls` / `equiv_B` は決定論的、`wall` は揺れる）:

```
explore/lsp        symbol 82ms → at 129ms → read 84ms          ← symbol は 583ms から −86%
hints/hints        hints 770ms                                  ← RA 側計算（待ちではない。影響外）
rename/lsp         rename 1150ms → cargo 168ms                 ← 残りは RA の WorkspaceEdit 計算
rename/apply       apply 1199ms → 104ms → 104ms → 136ms → cargo 154ms
verify/apply-check apply 1179ms → check 87ms                   ← apply の 1.1s は「pull が強制する RA 解析」（#7）
verify/apply-cargo apply 1211ms → cargo 133ms                  ← cargo 経路ではこの先払いが二重払い（#7）
verify/apply2-cargo apply 1回目 ~1376ms → 2回目 ~100ms → cargo ~164ms
verify-broken/check apply 893ms → check 92ms（rc=2 / 345B = Syntax Error 2 件）
verify-blind/check  apply 873ms → check 90ms（settled:false 契約を維持）
```

**注意（#7 で判明）**: `apply` の wall は **700ms / 1100ms に割れる**（Open 時の背景
settle と編集後 pull が同じ解析を奪い合うため。同一バイナリで 807ms / 1241ms を観測）。
`apply` を含む比較は `-r 3` を必須とし、A/B は**同一セッションで交互に**測る。

### cold（1 回観測、iteration #6 で rename / explore を再測。他は #5 の値）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `explore/lsp` | 3 | 1017 | 1568 | 8205 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 169 | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 8067 | 0 | True |
| `rename/lsp` | 2 | 219 | 438 | 9127 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 597 | 0 | True |
| `verify/apply-check` | 2 | 246 | 354 | 8258 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 419 | 0 | True |
| `verify/hunks-cargo` | 2 | 154 | 308 | 426 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 498 | 0 | True |
| `verify-blind/check` | 2 | 240 | 344 | 9095 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 434 | 0 | True |
| `verify-broken/check` | 2 | 450 | 555 | 8793 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 338 | 0 | True |

cold の支配項は**索引完走ゲート**（`symbol`/`hints`/`rename`/`check` が 8〜9 秒）。
`apply` は cold でも 120〜140ms（ゲートは `apply` の pull には乗らない）。
`rename/lsp`（9.1s）≒ `explore/lsp`（8.2s）なので、cold の主因は
`open_workspace_files` ではなくゲート（§4 の別候補を参照）。
#6 の 0ms 化・#7 の pull 撤去はゲート時間に影響しない（ゲートは別機構）。

計測の内訳が知りたいとき: `python3 docs/loop/l0.py -f <flow> -v`。

## 4. 次の課題設定（iteration #8）

> **iteration #7 の結末（この課題の起点）**: 「`apply` の診断 pull を外せば 1.1s が消える」は
> 棄却（ADR-0053 追記）。分かったのは (1) 1.1s は **pull が強制する RA 解析**そのもので、
> pull を外すと次に pull した者（`check`）が必ず払う、(2) 編集後の pull は
> **「daemon のスナップショットが編集後の診断・ヒントを反映する」契約**を担っている
> （テスト 3 件で固定）、(3) ただし契約が要求するのは「**pull が起きること**」だけで、
> 「**Save 応答の前に起きること**」ではない（テストは snapshot を poll で最大 10 秒待つ）、
> (4) 診断 pull が無い状態では **cargo 検証経路が −70〜80%** になる（apply-cargo
> 1344→268ms）= エージェントが cargo で検証するフローでは先払いが純粋な二重払い。

### 課題

`sync_after_edit` の pull（`pull_after_edit` → `set_focus_diagnostics` / `cache_hints`
/ `drain_into` / push）を **Save 応答のクリティカルパスから外す（背景タスク化）**。
契約（編集後追従）は「pull が起きること」しか要求しないので、応答を待たせずに満たせる
はず — その見込みを A/B で確かめ、実装する。

### 仮説

背景化すると `apply`（Save 応答）が **~1.1s → ~150ms** になり、エージェントの次ターン
（LLM 推論。数秒）の間に解析が終わるので、後続の `check` は **87ms のまま**。
エージェントの待ち時間は **~1.27s → ~0.24s（−80%）**、cargo 経路も −70〜80%
（#7 A/B(b) と同じ）。daemon のスナップショットは背景 pull の完了時（push）に更新され、
編集後追従の 3 テストは poll で待つので通る。

### 測り方（順序を守る）

1. **実装（1 変更）**: `sync_after_edit` の第 1 引数を `&Arc<Mutex<Daemon>>` に変え
   （3 呼び出し側 `daemon.rs:1222`（watch_disk 再読込）/ `4348`（Command 編集）/
   `4388`（DocumentEdit）はすべて `Arc` を持つ）、pull とその反映を `tokio::spawn` に移す。
   応答は現状のスナップショット（= 編集前の診断）を即返す。
   - ロック規律: `Open` の背景 settle（`daemon.rs:3882` 付近）と同じ形にする
     （daemon ロックを await またいで持たない・`LSP_LOCK_TIMEOUT` の外側で待たない）。
   - `activity`（`ReloadSync` 等）の追加/除去も背景タスク側に移す（応答に
     「同期中」が残らないように）。
2. **L0 に計測用の flow を足す**: `apply` → `sleep 3`（**エージェントの次ターン推論遅延の
   代理**）→ `check`（+ `apply` → `sleep 3` → `cargo check` の腕）。
   - 指標は **step 別 wall の apply + check の和**（`-v` の値）。ギャップはエージェントの
     待ちではないので flow の `wall_ms` を使わない（使うと sleep 分で比較が濁る）。
   - 受理側の見込み: apply ~150ms + check ~90ms = ~240ms（現行 ~1270ms）
3. **即時 check の非悪化を確認**: 既存 `verify/apply-check`（ギャップ無し）は
   apply ~1.1s が背景に移り、check が背景タスクの完了を待つ（= ~1.2s のまま）。
   「即座に check を打つエージェントを悪化させない」ことが受理の一部。
4. **回帰**: `cargo test` **462 green**（編集後追従の 3 テストが通ることが主条件）、
   L0 `-r 3` で fails 0、`verify-broken/check` の rc=2（345B）・`verify-blind` の
   `settled:false` 維持、`calls`/`out_B`/`equiv_B` 不変。
   `-r 10` で**空 pull 回帰**（背景化で編集後 pull が失われないか）と cold（`apply` の
   cold 120〜140ms が悪化しないか）も見る。
5. **→ 本採用の判断**: 採用なら ADR-0055（wire 形状は不変なので PROTOCOL_VERSION は
   上げない）。棄却なら「1.1s は編集と解析が直列である限りエージェントが払う」と確定し、
   別候補（`apply --no-diagnostics`）に切り替える。

### 受理条件 / 棄却条件

- **受理**: ギャップありで apply step ~150ms・check step ~90ms（**和が現行比 −70% 以上**）、
  462 test green、`calls` / `out_B` / `equiv_B` 不変（`apply` の出力は `applied: …` のまま）、
  `verify-broken` が rc=2 を維持、空 pull 回帰 0、cold の `apply` が悪化しない。
  **wall 単独の改善は受理しない**（method.md §7）— 編集後追従の契約が保たれること
  （テスト 3 件 green）が主条件。
- **棄却**: (a) 背景タスクのセッションロックが次のコマンドを待たせる（ギャップありでも
  `check` が ~1.1s = コストが移るだけ）、(b) 編集後追従の 3 テストが落ちる（背景 pull では
  daemon のスナップショットを更新できない / push が届かない）、(c) レースを入れる
  （`Save` 直後の `check` が古い診断を返す・背景タスクが二重に走る・デッドロック）。

### 別候補（後回し）

- **`apply --no-diagnostics`（エージェントが契約を選ぶ）**: 「cargo で検証する」と宣言した
  呼び出しだけ pull を省く（#7 の収穫を最小の変更で取る）。背景化より単純で、
  `settled` の意味も daemon の契約も変えない（呼び出し側が「診断はいらない」と言う）。
  ただし既定の `apply` は 1.1s のまま（エージェントが明示的に選ぶ必要がある）。
- `apply` の初回再解析が「前回の解析済み状態」から始まる件: `check`/`symbol` を先に
  叩いた後の `apply` は 137ms / 98ms / 102ms（#5）。**L2 で「apply の直前までに解析が
  温まっていたか」を確認**すると、実フローでは 1.1s が稀かもしれない（#8 の背景化が
  効かないフローがあるかの判定にもなる）。
- `open_workspace_files`（rename/references の前に全ファイル didOpen する回避策）が
  索引完走ゲートの導入後も必要か。ただし cold の主因は**ゲート**（`explore/lsp`
  8.2s ≒ `rename/lsp` 9.1s）で、fixture が小さいため L0 では didOpen 分の差が出ない
  （規模の課題 = L2 向き）。
- L2 での規模検証（fixture が小さいので `wall_ms` は外挿不可。`calls` / `equiv_B` は外挿可）。
- `ServerMetrics` の穴（`rename` のカウンタ、`get_state_total` の bytes）。
- 上位モデルでの再現（L2 は nano のみ）。
- **計時ログが無い**のでコマンド内部の内訳は A/B 除去か一時トレースでしか測れない。
  恒久的に測れるようにするなら daemon に計時ログを足す（計測の穴を埋める）。
  特に #7 で見つかった「Open 時背景 settle と編集後 pull の競合」は、計時ログが無いと
  700/1100ms の割れを説明できない（apply の wall の分散要因）。

## 5. 次のセッションの起動手順

**新しいセッションの入口**（`AGENTS.md` から `docs/loop/README.md` に辿れる。
プロンプトテンプレート `.pi/prompts/dev_minas.md` = `/dev_minas` でも同じ指示を展開できる。
手で貼るならこれ）:

```text
minas/minad のループエンジニアリング（エージェントのコスト低減）の続きをやって。
まず docs/loop/method.md と docs/loop/latest.md を読み、
latest.md の §4（次の課題設定）から iteration #8 を回して。計測したら --log で
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
python3 docs/loop/l0.py -f verify -f verify-broken -f verify-blind -r 3 -v   # 現状確認（apply ~1.1s / check 87ms）
git log --oneline -5                         # 直前の変更を確認（auto-checkpoint が並ぶ）
```

そのあと §4 の「測り方」1 → 2 → … の順に進める。
`--log` は必ず付ける（JSON は `tmp/loop/` に落ちるだけなので、付け忘れると生データが消える）。
`apply` を含む比較は `-r 3` 必須・**同一セッションで交互に**測る（§3 の注意）。

**反復番号の規則**: 今回の反復番号は §4 の見出しの番号（いまは **iteration #8**）。
結果は `log.md` に「iteration #8」として書き、考察の要約をこの `latest.md` に反映したうえで、
§4 を次の番号（#9）の課題設定に書き換える。§5 はこの手順のまま使う。
