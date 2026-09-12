# minas/minad ループエンジニアリング — 最新の検討結果（次のループの起点）

> git 管理（`docs/loop/`）。更新: iteration #4 完了時（2026-09-12）。
> 入口は [`README.md`](./README.md)。検討方法は [`method.md`](./method.md)。
> **このファイルと method.md を読めば次のループを回せる。**
> 生データと全反復の考察は同じディレクトリの [`log.md`](./log.md)。
> 設計判断は `docs/adr/0051`（索引完走ゲート）/ `0052`（check の空の早期確定）/ `0045`（`settled` の意味）。

## 1. 現在の状態（30 秒サマリ）

- 完了した反復: **#1 計測器の構築 → #2 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定（ADR-0052）→ #4 初回 apply の内訳確定（仮説棄却 + hints flow 追加）**
- 効果（L0 実測）: `check`（クリーン）**10154ms → 589ms（−94%）**。
  cold の無言の誤り（空の `symbol`・部分 `rename`・`check` の LSP error）は 0 件。
  `explore` / `rename` の `calls` / `equiv_B` は不変（回帰なし）。
- **#4 の結論**: 「初回 apply の ~1.1s は hint pull が主因」は **L0 で棄却**。
  トレースで内訳確定 → 初回 diag pull 内の RA 再解析 ~600–1100ms + `PULL_SETTLE` 250ms（毎回定数）。
  **次の課題（#5）は毎回の apply に乗る `PULL_SETTLE` 250ms の削減**。
- 全 462 テスト green。`python3 docs/loop/l0.py --selftest` green。
- **次にやるのは §4（iteration #5）**。まず §5 の手順で環境を確認する。

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
| **初回 apply の内訳**: `lsp::sync` ≈0ms（didChange は fire-and-forget）、`PULL_SETTLE` 250ms（毎回定数・加法）、初回 `pull_diagnostics` 内の RA 再解析 **~600–1100ms**（2 回目以降 ≈0ms = 増分）、hint pull ≈0ms（温まれば） | iteration #4 の daemon トレース（`verify/apply2-cargo` 実測。settle+diag_pull が加法 = RA は診断要求まで解析を始めない = lazy） |
| **`PULL_SETTLE` は解析待ちでなく通知消費待ちの可能性が高い**（settle 中に解析が進まないので、短くしても診断要求が解析完了までブロックして最終集合を返す — ADR-0052 の性質と整合）. ただし短くしすぎると didChange 未消費のまま「解析前の空」が返りうる（#5 で検証） | iteration #4 トレース + ADR-0052 |

**棄却済み（同じ仮説を再試行しない）**:

| 棄却した仮説 | 理由 |
|---|---|
| `apply`→`check` が cargo 委譲より**常に**安い | 当時 `calls`/`equiv_B` は同等で `wall` が 7.4 倍（10154 vs 1569ms）。**iteration #3 で check は 589ms になり逆転した**ので、この棄却は「当時の契約では成立しない」だけ。行番号つき診断 = `check` / 最終的な根拠 = `cargo`（+ 実プロジェクトでの wall の巻き返し）という**使い分けを L2 で測り直す**のが正しい（§4 の別候補） |
| 初回 apply の ~1.1s は **hint pull が主因** | iteration #4: `pull_after_edit` から hint pull を外して A/B → 1 回目 1373→1422ms・2 回目 352→342ms と不変。主因は初回 diag pull 内の RA 再解析（トレースで確定）。hint pull は温まれば ≈0ms |
| hint の消費者は `minas hints`（自前 pull・往復 1 回・ADR-0020）と daemon キャッシュ（=`minas get` の `inlay_hints`）だけ。**`minae`（TUI）は inlay hint を描画しない**（`minae/src/colors.rs:177`「MVP では描画しない」/ `render.rs:1158` はテスト用） | iteration #4 で確認（旧 #4 の「TUI の編集直後のヒントが消える」棄却条件は検証不能として撤回） |
| cold でも warm と同じ結果を返す | 無言の誤り 3 件（ADR-0051） |
| 空 + 索引完走 = クリーン確定（`settled:true`） | pull が実在するエラーを取りこぼす（ADR-0045 追記） |
| skill 参照が判断を変える | T6/T8（L2）: トークン +21〜61% で効果なし |

## 3. 基準値（L0、iteration #4 で再確認）— 次回の比較はここから

### warm（r=3 中央値）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `explore/lsp` | 3 | 1017 | 1568 | 1282 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 166 | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 731 | 0 | True |
| `rename/lsp` | 2 | 219 | 438 | 1796 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 2721 | 0 | True |
| `verify/apply-check` | 2 | 246 | 354 | 2022 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 1575 | 0 | True |
| `verify/hunks-cargo` | 2 | 154 | 308 | 1882 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 1972 | 0 | True |
| `verify-blind/check` | 2 | 240 | 344 | 1635 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 1232 | 0 | True |
| `verify-broken/check` | 2 | 450 | 555 | 1671 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 1156 | 0 | True |

### cold（1 回観測）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `explore/lsp` | 3 | 1017 | 1568 | 6740 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 173 | 0 | True |
| `rename/lsp` | 2 | 219 | 438 | 6712 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 1323 | 0 | True |
| `verify/apply-check` | 2 | 246 | 354 | 6415 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 424 | 0 | True |
| `verify/hunks-cargo` | 2 | 154 | 308 | 435 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 722 | 0 | True |
| `verify-blind/check` | 2 | 240 | 344 | 6254 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 436 | 0 | True |
| `verify-broken/check` | 2 | 450 | 555 | 5801 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 337 | 0 | True |

計測の内訳が知りたいとき: `python3 docs/loop/l0.py -f <flow> -v`。

## 4. 次の課題設定（iteration #5）

> **iteration #4 の結末（この課題の起点）**: 「初回 apply の主因は hint pull」は棄却済み。
> daemon トレース（`verify/apply2-cargo` 2 回適用、iteration #4 の生データ）で内訳を確定:
> ```
> [trace-sync] sync=0ms pull_total=856ms       ← 1 回目 apply（合計 ~1003ms）
> [trace-pull] settle=251ms lock=0ms diag_pull=604ms
> [trace-sync] sync=0ms pull_total=253ms       ← 2 回目 apply（合計 ~352ms）
> [trace-pull] settle=252ms lock=0ms diag_pull=0ms
> ```
> - `lsp::sync` ≈0ms（didChange は fire-and-forget）。RA は lazy。
> - 初回コスト = 初回 diag pull 内の RA 再解析 ~600–1100ms（run ごとに振れる。2 回目は増分で ≈0ms）。
> - **`PULL_SETTLE` 250ms は 1 回目も 2 回目も毎回定数**。settle と diag_pull が加法
>   （856 = 251 + 604）= **待機中に RA 解析は進んでいない**。

### 課題

`apply` の wall に **`PULL_SETTLE` 250ms が毎回乗る**（1 回目: RA 再解析 600–1100ms に
上乗せ。2 回目: ほぼ全額 250ms/352ms）。待機中に解析が進まない（加法）ことが確定して
いるので、この 250ms は「didChange 通知が RA に消費されるのを待つ」だけの可能性が高い。
それにしては過大で、削れるなら apply 系フロー全体の wall が下がる
（`calls` / `equiv_B` は不変のはず — 契約は変えない）。

### 仮説

`PULL_SETTLE` を 250ms → **50ms** にすると、apply の wall が毎回 ~200ms 下がる。
pull（`textDocument/diagnostic`）は解析完了までブロックして最終集合を返すので
（ADR-0052: round0 == round11）、didChange が消費済みなら短い settle でも結果は同じ。
リスク: 50ms では didChange 未消費のまま pull が「解析前の空」を返し、診断が消える
（次の編集まで stale — 旧 U4 と同じ症状）。どのくらいの settle で空が出なくなるかを
空検出 flow（`verify-broken`）で見張りながら詰める。

### 測り方（順序を守る）

1. **50ms で A/B（1 変更だけ）**: `minad/src/lsp.rs` の `PULL_SETTLE` を `50ms` にして
   `cargo build` → `-f verify -r 3 -v`。apply の 1 回目（RA 再解析が支配的）と
   2 回目（settle が支配的）の**両方**を見る。
2. **空 pull の回帰を監視**: `-f verify-broken -f verify-blind -r 3`。
   `verify-broken/check` に「Syntax Error」が出ない run があれば（silent/fails）
   空を返した証拠。`verify-blind` は settled:false 契約が壊れないことも見る。
3. **結論を固定**: 50ms で空が出ないなら採用。空が出るなら 100ms・150ms と二分探索で
   境界を探し、空が出ない最小値に決める。空が出る原因が settle 長でなく別（例:
   ロック待ち）なら課題の書き方を直す。
4. **回帰の全体確認**: `-f explore -f rename -f hints -r 3`（calls / equiv_B 不変）+
   `cargo test` 462 green。
5. **記録**: `--log -n "仮説: …"` → `log.md` に考察を手書き → この `latest.md` を更新。

### 受理条件 / 棄却条件

- **受理**: `verify/*` の 2 回目 apply step の wall が ~150ms 以上下がり（50ms 採用時）、
  1 回目も RA 再解析分以外は下がる。`verify-broken/check`・`verify-blind/*` が fails 0 のまま
  （空 pull 回帰なし = 診断消失なし）。`explore` / `rename` / `hints` の `calls` / `equiv_B`
  不変。`cargo test` 462 green。**wall 単独の改善は受理しない**（method.md §7: fixture が
  小さいので外挿不可）— 空回帰が出ないことが受理の主条件。
- **棄却**: (a) 50–150ms のどこでも空 pull 回帰が出る（`verify-broken/check` が silent/
  fails）→ settle は解析待ちを兼ねていたと確定し、削減を諦めて課題を閉じる。
  (b) apply の wall が下がらない（= settle は実は解析と重なっていた＝加法でなかった）
  → 250ms は隠れた解析時間で、削る意味が無い。
  (c) 診断は安定するが `minas check` の `settled` 表示が変わる（ADR-0052 の契約違反）。

### 別候補（後回し）

- **check の settled 契約**: 初回 diag pull の RA 再解析 ~600–1100ms は LSP の本質コストで、
  契約上は外せない。外すなら「apply は診断を返さず settled:false で即返し、
  `minas check` が pull で確定」という契約変更になり、check の 1 往復が 2 往復に増える
  （calls の悪化）。iteration #5 の結果次第で次に検討。
- `open_workspace_files`（rename/references の前に全ファイル didOpen する回避策）が
  索引完走ゲートの導入後も必要か。不要なら複数ファイル rename のコストが下がる
  （cold の 6.7 秒の大半がこれの可能性）。
- L2 での規模検証（fixture が小さいので `wall_ms` は外挿不可。`calls` / `equiv_B` は外挿可）。
- `ServerMetrics` の穴（`rename` のカウンタ、`get_state_total` の bytes）。
- 上位モデルでの再現（L2 は nano のみ）。
- **計時ログが無い**ので `apply` 内部の内訳は A/B 除去か一時トレースでしか測れない。
  恒久的に測れるようにするなら daemon に計時ログを足す（計測の穴を埋める）。
  iteration #4 では一時トレースで済ませた（除去済み — `git diff` で確認できる）。

## 5. 次のセッションの起動手順

**新しいセッションの入口**（`AGENTS.md` から `docs/loop/README.md` に辿れる。
プロンプトテンプレート `.pi/prompts/dev_minas.md` = `/dev_minas` でも同じ指示を展開できる。
手で貼るならこれ）:

```text
minas/minad のループエンジニアリング（エージェントのコスト低減）の続きをやって。
まず docs/loop/method.md と docs/loop/latest.md を読み、
latest.md の §4（次の課題設定）から iteration #5 を回して。計測したら --log で
log.md に記録し、考察を追記し、latest.md を更新して。
```

**注意**: `tmp/loop/`（結果 JSON・fixture）は checkout ごとの gitignore 対象。
worktree で測る場合はそちらに作られる（この文書群は git 管理なので worktree にもある）。

```bash
cd /Users/335g/dev/other/mina
cargo build                                  # 計測対象は target/debug。コードを触ったら必須
cargo test                                   # 期待値: 462 passed（毎回確認）
python3 docs/loop/l0.py --selftest          # 計測器の健全性
python3 docs/loop/l0.py -f verify -r 3 -v   # 現状確認（apply の 1 回目/2 回目・check 0.59 秒）
git log --oneline -5                         # 直前の変更を確認（auto-checkpoint が並ぶ）
```

そのあと §4 の「測り方」1 → 2 → … の順に進める。

**反復番号の規則**: 今回の反復番号は §4 の見出しの番号（いまは **iteration #5**）。
結果は `log.md` に「iteration #5」として書き、考察の要約をこの `latest.md` に反映したうえで、
§4 を次の番号（#6）の課題設定に書き換える。§5 はこの手順のまま使う。
