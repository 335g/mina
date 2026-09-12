# minas/minad ループエンジニアリング — 最新の検討結果（次のループの起点）

> git 管理（`docs/loop/`）。更新: iteration #6 完了時（2026-09-12）。
> 入口は [`README.md`](./README.md)。検討方法は [`method.md`](./method.md)。
> **このファイルと method.md を読めば次のループを回せる。**
> 生データと全反復の考察は同じディレクトリの [`log.md`](./log.md)。
> 設計判断は `docs/adr/0051`（索引完走ゲート）/ `0052`（check の空の早期確定）/
> `0053`（編集後の settle を置かない）/ `0054`（意味的リトライ待ち 0ms）/
> `0045`（`settled` の意味）。

## 1. 現在の状態（30 秒サマリ）

- 完了した反復: **#1 計測器の構築 → #2 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定（ADR-0052）→ #4 初回 apply の内訳確定 → #5 編集後の固定 settle 撤去（ADR-0053）→ #6 SEMANTIC_RETRY_WAIT 500ms 撤去（ADR-0054）**
- 効果（L0 実測、warm r=3 中央値）:
  - `check`（クリーン）**10154ms → 87ms（−99%）**（#3 + #6）
  - `symbol`（explore 1 歩目）**583 → 82ms（−86%）**（#6）
  - `rename`（lsp）**1594 → ~1150ms（−28%）**（#6。残りは RA 側の WorkspaceEdit 計算で待ちではない）
  - `apply` **1 回目 1359 → 1133ms / 2 回目 346 → 107ms**（#5。#6 でも不変）
  - `hints` は 707 → 770ms と不変（`pull_inlay_hints` は `request_with_loading_retry` を通らない。影響外）
- cold の無言の誤り（空の `symbol`・部分 `rename`・`check` の LSP error）は 0 件のまま。
  `calls` / `out_B` / `equiv_B` は全 flow で不変（回帰なし）。`--cold` の完全性も維持。
- 全 462 テスト green。`python3 docs/loop/l0.py --selftest` green。
- **#6 の結論**: 「`SEMANTIC_RETRY_WAIT` 500ms は『同じ答えをもう一度買う』待ち」は
  **採用**（500 → 0ms、ADR-0054）。2 回連続同一の確認ロジック自体は残す。
  根拠は「LSP の意味的リクエストは自身が解析完了までブロックしてから返る」
  （ADR-0052 の round0 == round11 と同型）。
- **次の課題（#7）は、warm の初回 `apply` に残る ~1.1s（RA の初回再解析）を
  「診断を返さない apply（`settled:false` 即返し）＋ `check` への委譲」でどれだけ
  削れるかの A/B**（§4）。

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
| **初回 apply の ~1.1s は RA の初回再解析であって待ちではない**: 同じ daemon で `symbol`/`check` を先に叩いてから `apply` すると **137ms / 98ms / 102ms**（`apply` は 100ms 台） | iteration #5 の下調べ（`tmp/loop/recon6.py`）+ #6 でも不変 |
| **`SEMANTIC_RETRY_WAIT` 500ms は毎回の `symbol` / `rename` / `check` に乗っていた**が、0ms で回帰ゼロ（warm -r3 全 flow + cold + -r10 で fails 0）。「2 回連続同一」の確認自体は同一クエリの再送でほぼ無料（request 自身が ~80ms）。待ちは解析と重ならない純粋な遅延 | iteration #6（ADR-0054） |
| **`rename` の残り ~1150ms は RA 側の WorkspaceEdit 計算**（2 回目の確認を省いた案 B でも 1162ms と不変 = 1 回目の request 自体が ~1s）。待ちではない | iteration #6 の案 B 比較 |
| **`hints` の 770ms は `pull_inlay_hints` の RA 側計算**。`request_with_loading_retry` を通らない（直接 request 1 回）ので 500ms 撤去の影響を受けない | iteration #6 |
| cold の支配項は**索引完走ゲート**（`symbol`/`hints`/`rename`/`check` が 8〜9 秒。`apply` は cold でも 120〜140ms） | L0 `--cold`（#2〜#6 で安定） |

**棄却済み（同じ仮説を再試行しない）**:

| 棄却した仮説 | 理由 |
|---|---|
| `apply`→`check` が cargo 委譲より**常に**安い | 当時 `calls`/`equiv_B` は同等で `wall` が 7.4 倍（10154 vs 1569ms）。**iteration #3 で check は 589ms になり逆転した**ので、この棄却は「当時の契約では成立しない」だけ。行番号つき診断 = `check` / 最終的な根拠 = `cargo`（+ 実プロジェクトでの wall の巻き返し）という**使い分けを L2 で測り直す**のが正しい（§4 の別候補） |
| 初回 apply の ~1.1s は **hint pull が主因** | iteration #4: `pull_after_edit` から hint pull を外して A/B → 1 回目 1373→1422ms・2 回目 352→342ms と不変。主因は初回 diag pull 内の RA 再解析（トレースで確定）。hint pull は温まれば ≈0ms |
| hint の消費者は `minas hints`（自前 pull・往復 1 回・ADR-0020）と daemon キャッシュ（=`minas get` の `inlay_hints`）だけ。**`minae`（TUI）は inlay hint を描画しない**（`minae/src/colors.rs:177`「MVP では描画しない」/ `render.rs:1158` はテスト用） | iteration #4 で確認 |
| cold でも warm と同じ結果を返す | 無言の誤り 3 件（ADR-0051） |
| 空 + 索引完走 = クリーン確定（`settled:true`） | pull が実在するエラーを取りこぼす（ADR-0045 追記） |
| skill 参照が判断を変える | T6/T8（L2）: トークン +21〜61% で効果なし |
| **`PULL_SETTLE` は「解析前の空」を避けるために要る**（削ると診断が消える） | iteration #5: 50ms・0ms のどちらでも空 pull 回帰が出ない。settle は買えるものが無い純粋な遅延だった → 撤去（ADR-0053） |
| **「2 回連続同一」を捨て、loading（null / 空）のときだけリトライすれば 1 往復削れる** | iteration #6 の案 B: `symbol` 84ms（案 A 82ms）・`rename` 1162ms（案 A 1150ms）と差なし。2 回目の確認は同一クエリの再送でほぼ無料。代わりに loading 時 100ms 待つコードが残るだけ → 不採用 |
| **`SEMANTIC_RETRY_WAIT` を 50ms・100ms 等に縮めて様子見する** | iteration #6: 0ms で回帰ゼロだったので二分探索の必要が無かった（ADR-0054）。0ms 一択 |

## 3. 基準値（L0、iteration #6 で再測）— 次回の比較はここから

### warm（r=3 中央値）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `explore/lsp` | 3 | 1014 | 1562 | 310 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 178 | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 783 | 0 | True |
| `rename/lsp` | 2 | 217 | 434 | 1364 | 0 | True |
| `rename/apply` | 5 | 402 | 1410 | 1779 | 0 | True |
| `verify/apply-check` | 2 | 244 | 351 | 1323 | 0 | True |
| `verify/apply-cargo` | 2 | 107 | 214 | 1377 | 0 | True |
| `verify/hunks-cargo` | 2 | 153 | 306 | 1353 | 0 | True |
| `verify/apply2-cargo` | 3 | 216 | 540 | 1487 | 0 | True |
| `verify-blind/check` | 2 | 240 | 344 | 892 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 994 | 0 | True |
| `verify-broken/check` | 2 | 450 | 555 | 851 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 917 | 0 | True |

（`out_B` / `equiv_B` は #5 と 1〜4B の表示揺れ。契約の形は同じ）

ステップ別（`-v`、1 run の観測。`calls` / `equiv_B` は決定論的、`wall` は揺れる）:

```
explore/lsp        symbol 82ms → at 129ms → read 84ms          ← symbol は 583ms から −86%
hints/hints        hints 770ms                                  ← RA 側計算（待ちではない。影響外）
rename/lsp         rename 1150ms → cargo 168ms                 ← 残りは RA の WorkspaceEdit 計算
rename/apply       apply 1199ms → 104ms → 104ms → 136ms → cargo 154ms
verify/apply-check apply 1143ms → check 87ms                   ← check は 589ms から −85%
verify/apply-cargo apply 1155ms → cargo 168ms
verify/apply2-cargo apply 1回目 ~1150ms → 2回目 ~100ms → cargo ~160ms
verify-broken/check apply 852ms → check 85ms（rc=2 / 345B = Syntax Error 2 件）
verify-blind/check  apply 808ms → check 84ms（settled:false 契約を維持）
```

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
#6 の 0ms 化はゲート時間に影響しない（ゲートは別機構）。

計測の内訳が知りたいとき: `python3 docs/loop/l0.py -f <flow> -v`。

## 4. 次の課題設定（iteration #7）

> **iteration #6 の結末（この課題の起点）**: `SEMANTIC_RETRY_WAIT` 500ms の撤去
> （ADR-0054）で `symbol` 583→82ms / `rename` ~1594→~1150ms / `check` 589→87ms。
> 残る最大の壁は **warm の初回 `apply` の ~1.1s**（#5 で「RA の初回再解析」と確定。
> settle も 500ms 待ちも撤去済みなので、純粋に push なし pull の RA 再解析コスト）。
> `apply` を毎回打つエージェントの編集フローでは、この 1.1s が編集のたびに乗る
> （1 ファイル 1 編集の繰り返しなら N 回 ×1.1s）。

### 課題

`minas apply` は編集後に `pull_diagnostics_settled`（+ hint pull）で診断を取り込み、
**解析完了までブロック**する（RA は lazy で、pull が来るまで解析を始めない — iteration
#4 のトレース）。これが初回（または前回編集時点からテキストが変わるたび）の ~1.1s の
正体。編集自体は ~150ms。

「パフォーマンスの節約」ではなく「**診断を返すことを契約から外す**」ことで、この
ブロックを編集と解離させられる可能性がある: `apply` は `settled:false` で即返し、
後続の `minas check` が pull で確定する。

### 仮説

`apply` の診断を返す部分（`pull_diagnostics_settled`）を無くし `settled:false` で
即返しにすると、`apply` の wall が **~1.1s → ~150ms** になる。エージェントが編集後に
`check` を打つフロー（`verify/apply-check` がまさにそれ）なら、1.1s を check 1 往復
（~90ms、すでに RA は解析済みなら pull は速い）に置き換えられる。

### 測り方（順序を守る）

1. **対象の隔離**: `verify/apply-check` の apply step が ~1.1s / check step ~90ms と
   分離されていることを再確認（`-f verify -r 3 -v`）。
2. **A/B（1 変更だけ）**: `minad/src/lsp.rs` の `pull_after_edit` から
   `pull_diagnostics_settled` を外して `settled:false` の空で即返す（一時変更）→
   `cargo build` → `-f verify -f verify-broken -f verify-blind -r 3 -v`。
   - `apply` step が ~1.1s → ~150ms になるか
   - `check` step は変わらないか（RA は apply 経由で解析済みのはず。増えるなら
     「コストが apply から check に移っただけ」で、削れていない）
   - `verify-broken/check` が rc=2 を返し続けるか（編集の検出性が落ちないか）
3. **契約としての整合**: `apply` の `settled:false` 化が `calls` / `out_B` / `equiv_B`
   に与える影響を測る。`apply` の出力から診断 JSON が消える分 `out_B` は減るはず。
   エージェント視点で「apply の結果で直ちに編集の正しさを見る」場面（apply 単体で
   rc!=0 を期待する検証フロー）が壊れないかを確認する（`verify-broken/apply` 相当）。
4. **→ 本採用の判断**: A/B で wall が本当に下がり、検出性（broken で rc=2 相当）が
   保たれるなら、契約変更（`apply` は診断を返さない。`check` が返す）として
   PROTOCOL_VERSION を上げて実装し ADR を書く。下がらない/検出性が落ちるなら、
   「初回再解析 1.1s は RA が編集を解析する本来のコスト」と確定して課題を閉じる。

### 受理条件 / 棄却条件

- **受理**: `apply` の wall が ~1.1s → ~150ms に下がり、`check` が「前と同じか
  それ以下」で**トータル（apply + check）の wall が下がる**。`calls` は不変
  （apply の契約は「診断を含まない」になるだけで往復数は同じ。エージェントが
  apply 後に check を打つかはフローの話で、L0 は apply-check flow のまま測れる）。
  エラー検出（`verify-broken`）が rc=2 で保たれる。`cargo test` 462 green。
  **wall 単独の改善は受理しない**（method.md §7）— 検出性と契約（`settled` の意味、
  ADR-0045）が保たれることが主条件。
- **棄却**: (a) `apply` を軽くしても `check` が同じだけ増える（コストが移るだけ）
  → 1.1s は編集の解析コストそのもので、削る場所が無い（課題を閉じる）。
  (b) `verify-broken` でエラーが取りこぼされる（`apply` が診断を返さないことで、
  検証フローが壊れる）→ 契約変更は成立しない。
  (c) `settled` の意味が壊れる（`apply` の `settled:false` を `check` が引き継ぐ
  設計が ADR-0045 の「空をクリーンの根拠にしない」と矛盾する）。

### 別候補（後回し）

- `apply` の**初回再解析が「前回の解析済み状態」から始まる**: `minas check` を
  打った直後の `apply` は 137ms / 98ms / 102ms（#5 下調べ）。エージェントの実フロー
  （探索→編集→検証）で「apply の前に意味的コマンドが一度は入る」なら、1.1s は
  実フローでは稀かもしれない。L2 で「apply の直前までに解析が温まっていたか」を
  確認する価値がある（該当なら §4 の仮説は L2 側で解決済みになる可能性がある）。
- `open_workspace_files`（rename/references の前に全ファイル didOpen する回避策）が
  索引完走ゲートの導入後も必要か。ただし cold の主因は**ゲート**（`explore/lsp`
  8.2s ≒ `rename/lsp` 9.1s）で、fixture が小さいため L0 では didOpen 分の差が出ない
  （規模の課題 = L2 向き）。
- L2 での規模検証（fixture が小さいので `wall_ms` は外挿不可。`calls` / `equiv_B` は外挿可）。
- `ServerMetrics` の穴（`rename` のカウンタ、`get_state_total` の bytes）。
- 上位モデルでの再現（L2 は nano のみ）。
- **計時ログが無い**のでコマンド内部の内訳は A/B 除去か一時トレースでしか測れない。
  恒久的に測れるようにするなら daemon に計時ログを足す（計測の穴を埋める）。

## 5. 次のセッションの起動手順

**新しいセッションの入口**（`AGENTS.md` から `docs/loop/README.md` に辿れる。
プロンプトテンプレート `.pi/prompts/dev_minas.md` = `/dev_minas` でも同じ指示を展開できる。
手で貼るならこれ）:

```text
minas/minad のループエンジニアリング（エージェントのコスト低減）の続きをやって。
まず docs/loop/method.md と docs/loop/latest.md を読み、
latest.md の §4（次の課題設定）から iteration #7 を回して。計測したら --log で
log.md に記録し、考察を追記し、latest.md を更新して。
```

**注意**: `tmp/loop/`（結果 JSON・fixture）は checkout ごとの gitignore 対象。
worktree で測る場合はそちらに作られる（この文書群は git 管理なので worktree にもある）。
`minad` / `minas` を止めずに複数の l0 実行を重ねない（索引・CPU が競合して `wall_ms` が濁る。
`l0.py` はアームごとに専用 daemon を立てるので、同時実行は避ける）。

```bash
cd /Users/335g/dev/other/mina
cargo build                                  # 計測対象は target/debug。コードを触ったら必須
cargo test                                   # 期待値: 462 passed（毎回確認）
python3 docs/loop/l0.py --selftest          # 計測器の健全性
python3 docs/loop/l0.py -f explore -f rename -f verify -r 3 -v   # 現状確認（symbol 82ms / rename 1150ms / check 87ms / apply 1回目 ~1.1s）
git log --oneline -5                         # 直前の変更を確認（auto-checkpoint が並ぶ）
```

そのあと §4 の「測り方」1 → 2 → … の順に進める。
`--log` は必ず付ける（JSON は `tmp/loop/` に落ちるだけなので、付け忘れると生データが消える）。

**反復番号の規則**: 今回の反復番号は §4 の見出しの番号（いまは **iteration #7**）。
結果は `log.md` に「iteration #7」として書き、考察の要約をこの `latest.md` に反映したうえで、
§4 を次の番号（#8）の課題設定に書き換える。§5 はこの手順のまま使う。