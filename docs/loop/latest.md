# minas/minad ループエンジニアリング — 最新の検討結果（次のループの起点）

> git 管理（`docs/loop/`）。更新: iteration #5 完了時（2026-09-12）。
> 入口は [`README.md`](./README.md)。検討方法は [`method.md`](./method.md)。
> **このファイルと method.md を読めば次のループを回せる。**
> 生データと全反復の考察は同じディレクトリの [`log.md`](./log.md)。
> 設計判断は `docs/adr/0051`（索引完走ゲート）/ `0052`（check の空の早期確定）/
> `0053`（編集後の settle を置かない）/ `0045`（`settled` の意味）。

## 1. 現在の状態（30 秒サマリ）

- 完了した反復: **#1 計測器の構築 → #2 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定（ADR-0052）→ #4 初回 apply の内訳確定 → #5 編集後の固定 settle 撤去（ADR-0053）**
- 効果（L0 実測、warm r=3 中央値）:
  - `check`（クリーン）**10154ms → 589ms（−94%）**（#3）
  - `apply` **1 回目 1359 → 1133ms / 2 回目 346 → 107ms**（#5。固定 250ms が毎回消えた）
  - `rename/apply` の apply ループも各ステップ ~100ms 台に
- cold の無言の誤り（空の `symbol`・部分 `rename`・`check` の LSP error）は 0 件のまま。
  `calls` / `out_B` / `equiv_B` は全 flow で不変（回帰なし）。
- 全 462 テスト green。`python3 docs/loop/l0.py --selftest` green。
- **#5 の結論**: 「`PULL_SETTLE` 250ms は通知消費待ちで、削れる」は**採用**。しかも
  50ms で空回帰が出なかったので 0ms（撤去）まで試し、撤去した。根拠は
  「pull 自身が解析完了までブロックして最終集合を返す」（ADR-0052 の round0 == round11 を
  待ち 0 のプローブで再確認）。
- **次の課題（#6）は、`symbol` / `rename` / `check` が毎回払っている `SEMANTIC_RETRY_WAIT` 500ms の削減**
  （下調べで正体を特定済み。§4）。
- **次にやるのは §4（iteration #6）**。まず §5 の手順で環境を確認する。

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
| **初回 apply の内訳**: `lsp::sync` ≈0ms（didChange は fire-and-forget）、初回 `pull_diagnostics` 内の RA 再解析 **~600–1100ms**、hint pull ≈0ms（温まれば）。**固定 250ms の settle はこの上に毎回乗っていた**（iteration #4 のトレース。settle と diag_pull が加法 = RA は診断要求まで解析を始めない = lazy） | iteration #4 の daemon トレース（`verify/apply2-cargo` 実測） |
| **編集後の固定 settle（`PULL_SETTLE`）は撤去できる**（ADR-0053）。250ms → 0ms で `apply` が毎回 −250ms、空 pull 回帰は `-r 10` × 3 flow = 30 run で 0 件。didChange が書かれる順序は mutex 下の直列 write で保証され、pull は解析完了までブロックする（待ち 0 のプローブでも round0 が最終集合） | iteration #5（L0 + プローブ直叩き） |
| **初回 apply の ~1s は RA の初回再解析であって待ちではない**: 同じ daemon で `symbol`/`check` を先に叩いてから `apply` すると **137ms / 98ms / 102ms**（`apply` は 100ms 台） | iteration #5 の下調べ（`tmp/loop/recon6.py`） |
| **`symbol` / `rename` は毎回 ~590–610ms、`check` は ~594ms**。正体は `SEMANTIC_RETRY_WAIT` 500ms（`request_with_loading_retry` が「2 回連続で同一」を確かめるために必ず挟む）+ 1 往復 | 同上（iteration #6 の課題） |

**棄却済み（同じ仮説を再試行しない）**:

| 棄却した仮説 | 理由 |
|---|---|
| `apply`→`check` が cargo 委譲より**常に**安い | 当時 `calls`/`equiv_B` は同等で `wall` が 7.4 倍（10154 vs 1569ms）。**iteration #3 で check は 589ms になり逆転した**ので、この棄却は「当時の契約では成立しない」だけ。行番号つき診断 = `check` / 最終的な根拠 = `cargo`（+ 実プロジェクトでの wall の巻き返し）という**使い分けを L2 で測り直す**のが正しい（§4 の別候補） |
| 初回 apply の ~1.1s は **hint pull が主因** | iteration #4: `pull_after_edit` から hint pull を外して A/B → 1 回目 1373→1422ms・2 回目 352→342ms と不変。主因は初回 diag pull 内の RA 再解析（トレースで確定）。hint pull は温まれば ≈0ms |
| hint の消費者は `minas hints`（自前 pull・往復 1 回・ADR-0020）と daemon キャッシュ（=`minas get` の `inlay_hints`）だけ。**`minae`（TUI）は inlay hint を描画しない**（`minae/src/colors.rs:177`「MVP では描画しない」/ `render.rs:1158` はテスト用） | iteration #4 で確認（旧 #4 の「TUI の編集直後のヒントが消える」棄却条件は検証不能として撤回） |
| cold でも warm と同じ結果を返す | 無言の誤り 3 件（ADR-0051） |
| 空 + 索引完走 = クリーン確定（`settled:true`） | pull が実在するエラーを取りこぼす（ADR-0045 追記） |
| skill 参照が判断を変える | T6/T8（L2）: トークン +21〜61% で効果なし |
| **`PULL_SETTLE` は「解析前の空」を避けるために要る**（削ると診断が消える） | iteration #5: 50ms・0ms のどちらでも空 pull 回帰が出ない（`verify-broken/check` が rc=2 で `Syntax Error` を返し続ける・`-r 10` で 30 run fails 0）。settle は買えるものが無い純粋な遅延だった → 撤去（ADR-0053） |

## 3. 基準値（L0、iteration #5 で再測）— 次回の比較はここから

### warm（r=3 中央値）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `explore/lsp` | 3 | 1017 | 1568 | 1284 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 164 | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 707 | 0 | True |
| `rename/lsp` | 2 | 219 | 438 | 1741 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 1638 | 0 | True |
| `verify/apply-check` | 2 | 246 | 354 | 1652 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 1281 | 0 | True |
| `verify/hunks-cargo` | 2 | 154 | 308 | 1275 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 1352 | 0 | True |
| `verify-blind/check` | 2 | 240 | 344 | 1286 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 879 | 0 | True |
| `verify-broken/check` | 2 | 450 | 555 | 1370 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 844 | 0 | True |

ステップ別（`-v`、1 run の観測。`calls` / `equiv_B` は決定論的、`wall` は揺れる）:

```
explore/lsp        symbol 580ms → at 620ms → read 86ms
hints/hints        hints 707ms
rename/lsp         rename 1594ms → cargo 174ms
rename/apply       apply 1098ms → 96ms → 97ms → 136ms → cargo 166ms   ← 2 回目以降は 100ms 台
verify/apply-check apply 1026ms → check 587ms
verify/apply-cargo apply 1083ms → cargo 142ms
verify/apply2-cargo apply 1回目 1100ms前後 → 2回目 ~107ms → cargo ~140ms
verify-broken/check apply 820ms → check 588ms（rc=2 / 345B = Syntax Error 2 件）
```

### cold（1 回観測、iteration #5 で再測）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `explore/lsp` | 3 | 1017 | 1568 | 8825 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 169 | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 8067 | 0 | True |
| `rename/lsp` | 2 | 219 | 438 | 9028 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 582 | 0 | True |
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
`rename/lsp`（9.0s）≒ `explore/lsp`（8.8s）なので、cold の主因は
`open_workspace_files` ではなくゲート（§4 の別候補を参照）。

計測の内訳が知りたいとき: `python3 docs/loop/l0.py -f <flow> -v`。

## 4. 次の課題設定（iteration #6）

> **iteration #5 の結末（この課題の起点）**: `PULL_SETTLE` の撤去で `apply` の
> 固定費 250ms は消えた（ADR-0053）。しかし**別の 500ms が `symbol` / `rename` /
> `check` に毎回乗っている**ことが下調べで判明した（`tmp/loop/recon6.py`）:
> ```
> minas symbol …   586ms / 588ms      ← 毎回。初回だけではない
> minas rename …   609ms / 609ms      ← 毎回
> minas check  …   945ms / 594ms / 587ms
> minas apply  …   137ms / 98ms / 102ms   ← settle 撤去で 100ms 台
> ```
> 正体は `minad/src/lsp.rs` の `request_with_loading_retry`:
> ```
> loop {
>     結果 = request(method, params);          // 1 回目
>     if !loading && prev == 結果 { return }    // 2 回連続で同一 = 安定
>     prev = 結果; sleep(SEMANTIC_RETRY_WAIT)   // ← 500ms（毎回必ず挟まる）
> }
> ```
> 1 回目の結果が**既に完全でも**、確認のために 500ms 待って 2 回目を投げる。
> `workspace/symbol` / `textDocument/rename` / `references` が全部この経路。
> `pull_diagnostics_settled`（`check`）も同じ定数を使う（ADR-0052 が「安全代」として
> 意図的に置いた 500ms）。

### 課題

`SEMANTIC_RETRY_WAIT`（500ms）が毎回の `symbol` / `rename` / `check` に乗っている。
ADR-0052 の測定（pull は round0 で最終集合）と iteration #5 の測定（待ち 0 でも
同じ答え）が示すとおり、要求は**自分で解析を完了させてから**返るので、この
「待ってからもう一度聞く」は同じ答えをもう一度買っているだけの可能性が高い。
`apply` の settle と同じ形の無駄で、しかもこちらの方が当たるコマンドが多い
（探索の入口 `symbol`・意味リネーム `rename`・検証 `check`）。

### 仮説

`SEMANTIC_RETRY_WAIT` を 500ms → **0ms**（= 2 回目の確認だけ残し、間の待ちを捨てる）
にすると `symbol` / `rename` / `check` の wall が毎回 ~500ms 下がる
（`explore/lsp` の symbol step 586 → ~90ms、`rename/lsp` 1594 → ~1100ms、
`check` 594 → ~94ms）。`calls` / `out_B` / `equiv_B` は不変のはず（契約は変えない）。

リスク: 遅いサーバ・大きいプロジェクトで**不完全な結果が 2 回連続で一致**し、
「安定」と誤判定して部分結果を返す（ADR-0051 が索引ゲートで塞いだ「部分 rename の
成功報告」が戻る）。索引完走ゲートは残るので、cold でも完全性が保たれるかを
`--cold` の rename で見張る。

### 測り方（順序を守る）

1. **0ms で A/B（1 変更だけ）**: `minad/src/lsp.rs` の `SEMANTIC_RETRY_WAIT` を
   `0ms` にして `cargo build` → `-f explore -f rename -f hints -r 3 -v`。
   `symbol` / `rename` の step が ~500ms 下がるか、`calls` / `equiv_B` が不変かを見る。
2. **正しさの見張り**: `-f verify -f verify-broken -f verify-blind -r 3`
   （`check` の step が ~500ms 下がるか・`verify-broken/check` が rc=2 のままか・
   `verify-blind` が `settled:false` 契約を守るか）。加えて **`--cold -f rename -f explore`**
   （索引ゲートが cold の完全 rename / 非空 symbol を守っているか）。
3. **往復の確認（必要なら）**: `-r 10` で `explore` / `rename`（部分結果の取りこぼしを
   確率的に踏みに行く。iteration #5 の空回帰の見張りと同じ運用）。
4. **結論を固定**: 0ms で回帰が出ないなら採用（`SEMANTIC_RETRY_WAIT` を削除し、
   `request_with_loading_retry` を「2 回連続同一の確認（待ち無し）」に単純化）。
   回帰が出るなら 50ms・100ms と二分探索し、回帰しない最小値に決める。
   **待ちの代わりに「ローディング形（null / 空）でのみリトライ」に縮める**案も
   同じ A/B で比較してよい（そちらは `is_loading` が既に判定している）。
5. **回帰の全体確認**: `-f explore -f rename -f hints -f verify -r 3`（calls / equiv_B
   不変）+ `cargo test` 462 green。
6. **記録**: `--log -n "仮説: …"` → `log.md` に考察を手書き → この `latest.md` を更新。

### 受理条件 / 棄却条件

- **受理**: `explore/lsp` の symbol step と `rename/lsp` の rename step、および
  `check` step がそれぞれ **~500ms 下がり**（0ms 採用時）、`calls` / `out_B` /
  `equiv_B` が全 flow で不変、`need`（応答の中身）が同じ。`fails` が 0 のまま
  （`-r 10` の追加観測を含む）。`--cold` でも `rename/lsp` の `verify`
  （cargo check green = 4 箇所すべてリネーム）と `explore/lsp` の非空 symbol が保たれる。
  `cargo test` 462 green。**wall 単独の改善は受理しない**（method.md §7: fixture が
  小さいので外挿不可）— 完全性が保たれることが受理の主条件。
- **棄却**: (a) 0–100ms のどこでも部分結果（rename の取りこぼし・symbol の欠け）や
  空 pull が出る → 「2 回連続同一 + 500ms」は遅いサーバの安全代だったと確定し、
  削減を諦めて課題を閉じる（ADR-0052 Decision 4 の追認）。 (b) wall が下がらない
  （= 500ms が実は解析と重なっていた）→ 隠れた解析時間で、削る意味が無い。
  (c) `check` の `settled` 表示が変わる（ADR-0052 / ADR-0045 の契約違反）。

### 別候補（後回し）

- **apply の残り（RA 初回再解析 600–1100ms）**: 下調べで「他のコマンドが先に解析を
  起こしていれば apply は 100ms 台」と分かった。外すには「`apply` は診断を返さず
  `settled:false` で即返し、`minas check` が pull で確定」という契約変更が要り、
  `calls` が 1 増える取引（初回 apply 1.1s → 0.14s + check 1 往復）。L0 で
  `calls` / `wall` を並べて測る価値はある。
- `open_workspace_files`（rename/references の前に全ファイル didOpen する回避策）が
  索引完走ゲートの導入後も必要か。ただし cold の主因は**ゲート**（`explore/lsp`
  8.8s ≒ `rename/lsp` 9.0s）で、fixture が小さいため L0 では didOpen 分の差が出ない
  （規模の課題 = L2 向き）。
- L2 での規模検証（fixture が小さいので `wall_ms` は外挿不可。`calls` / `equiv_B` は外挿可）。
- `ServerMetrics` の穴（`rename` のカウンタ、`get_state_total` の bytes）。
- 上位モデルでの再現（L2 は nano のみ）。
- **計時ログが無い**のでコマンド内部の内訳は A/B 除去か一時トレースでしか測れない
  （iteration #5 の下調べは「順に投げて差を見る」で代用した）。恒久的に測れるように
  するなら daemon に計時ログを足す（計測の穴を埋める）。

## 5. 次のセッションの起動手順

**新しいセッションの入口**（`AGENTS.md` から `docs/loop/README.md` に辿れる。
プロンプトテンプレート `.pi/prompts/dev_minas.md` = `/dev_minas` でも同じ指示を展開できる。
手で貼るならこれ）:

```text
minas/minad のループエンジニアリング（エージェントのコスト低減）の続きをやって。
まず docs/loop/method.md と docs/loop/latest.md を読み、
latest.md の §4（次の課題設定）から iteration #6 を回して。計測したら --log で
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
python3 docs/loop/l0.py -f explore -f rename -f verify -r 3 -v   # 現状確認（symbol 586ms / rename 1594ms / check 587ms）
git log --oneline -5                         # 直前の変更を確認（auto-checkpoint が並ぶ）
```

そのあと §4 の「測り方」1 → 2 → … の順に進める。
`--log` は必ず付ける（JSON は `tmp/loop/` に落ちるだけなので、付け忘れると生データが消える）。

**反復番号の規則**: 今回の反復番号は §4 の見出しの番号（いまは **iteration #6**）。
結果は `log.md` に「iteration #6」として書き、考察の要約をこの `latest.md` に反映したうえで、
§4 を次の番号（#7）の課題設定に書き換える。§5 はこの手順のまま使う。
