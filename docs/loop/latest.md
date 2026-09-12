# minas/minad ループエンジニアリング — 最新の検討結果（次のループの起点）

> git 管理（`docs/loop/`）。更新: iteration #3 完了時（2026-09-12）。
> 入口は [`README.md`](./README.md)。検討方法は [`method.md`](./method.md)。
> **このファイルと method.md を読めば次のループを回せる。**
> 生データと全反復の考察は同じディレクトリの [`log.md`](./log.md)。
> 設計判断は `docs/adr/0051`（索引完走ゲート）/ `0052`（check の空の早期確定）/ `0045`（`settled` の意味）。

## 1. 現在の状態（30 秒サマリ）

- 完了した反復: **#1 計測器の構築 → #2 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定（ADR-0052）**
- 効果（L0 実測）: `check`（クリーン）**10154ms → 589ms（−94%）**。
  cold の無言の誤り（空の `symbol`・部分 `rename`・`check` の LSP error）は 0 件。
  `explore` / `rename` の `calls` / `equiv_B` は不変（回帰なし）。
- 全 462 テスト green。`python3 docs/loop/l0.py --selftest` green。
- **次にやるのは §4（iteration #4）**。まず §5 の手順で環境を確認する。

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

**棄却済み（同じ仮説を再試行しない）**:

| 棄却した仮説 | 理由 |
|---|---|
| `apply`→`check` が cargo 委譲より**常に**安い | 当時 `calls`/`equiv_B` は同等で `wall` が 7.4 倍（10154 vs 1569ms）。**iteration #3 で check は 589ms になり逆転した**ので、この棄却は「当時の契約では成立しない」だけ。行番号つき診断 = `check` / 最終的な根拠 = `cargo`（+ 実プロジェクトでの wall の巻き返し）という**使い分けを L2 で測り直す**のが正しい（§4 の別候補） |
| cold でも warm と同じ結果を返す | 無言の誤り 3 件（ADR-0051） |
| 空 + 索引完走 = クリーン確定（`settled:true`） | pull が実在するエラーを取りこぼす（ADR-0045 追記） |
| skill 参照が判断を変える | T6/T8（L2）: トークン +21〜61% で効果なし |

## 3. 基準値（L0、iteration #3 時点）— 次回の比較はここから

### warm（r=3 中央値）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `explore/lsp` | 3 | 1017 | 1568 | 1282 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 166 | 0 | True |
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

## 4. 次の課題設定（iteration #4）

> **前提の修正（iteration #3 の後、別セッションの冷読検証で判明）**: 当初この節は
> 「hint の計算量が支配的」と書いていたが、生データを見直すと根拠が弱い。
> `verify/apply2-cargo` は同じ arm で 2 回 apply しており、**1 回目 1470ms /
> 2 回目 348ms**（iteration #3 の生データ、別セッションの再測 1423ms / 348ms でも再現。
> 2 セッションで一致）。差 ≈1.1 秒は hint のサイズ依存ではなく
> **「そのセッションで最初の edit が払うコスト」**を示す。まず 1 回目/2 回目の差を
> 確認し、そのうえで hint pull の寄与を切り分ける。

### 課題

`apply` が **セッション最初の 1 回だけ 1.0〜1.7 秒**かかる（2 回目以降は約 350ms）。
その内訳が計測できていない（`-v` は step の合計まで、daemon に計時ログが無い）。
候補経路は `sync_after_edit` → `lsp::sync` + `pull_after_edit`
（`PULL_SETTLE` 250ms + 診断 pull + **inlay hint pull**）。

### 仮説

初回の ~1.1 秒は「冷たい状態での pull」で、主要因は**診断と inlay hint をまとめて
計算する初回コスト**。hint pull を外せば初回 apply が短くなる。
対案として、hint pull は残して**編集時に hint キャッシュを破棄**するだけでもよい
（古い hint をエージェントに見せない。0 往復で済むのでこちらが安い）。

### 測り方（順序を守る）

1. **内訳を測る（最初にやる）**: `-f verify -r 3 -v` で apply の 1 回目/2 回目を確認
   （1 回目 ≈1.4s / 2 回目 ≈0.35s が再現するか）。再現しなければ課題の書き方を直す
   （＝初回コストの別要因を探す）。
2. **hint pull の寄与を切り分ける（1 変更だけ）**: `pull_after_edit` から
   `pull_inlay_hints` を外して `-f verify -f explore -f rename -r 3`。
   apply の**1 回目と 2 回目の両方**を見る。両方が下がって初めて hint が主因。
3. **対案を測る**: hint pull は残し、`sync_after_edit` で hint キャッシュを破棄する版と比較
   （古いヒントを見せない、という目的はどちらでも満たせる）。
4. **hint 経路を実測で確認**: L0 に `hints` flow（`minas hints <path>` の `calls` / `out_B` /
   `wall_ms`）を追加する（**現状 flow が無い**。`l0.py` の `FLOWS` に足す）。
5. **記録**: `--log -n "仮説: …"` → `log.md` に考察を手書き → この `latest.md` を更新。

### 受理条件 / 棄却条件

- **受理**: 手順 2 か 3 で `verify/*` の apply ステップ wall が ≥50% 下がり、
  `explore` / `rename` / `verify-broken` / `verify-blind` の `calls` / `equiv_B` / `fails` が
  悪化せず、`cargo test` 462 green、**手順 4 で hint 経路が欠けないことを実測**している。
  **wall 単独の改善は受理しない**（method.md §7: fixture が小さいので外挿不可）。
- **棄却**: (a) `minas hints` の往復数 / バイトが増える、または `minas hints` の応答が欠ける、
  (b) 編集直後の `minas get` の `inlay_hints` が編集前のものになる（staleness）、
  (c) apply の 1 回目が下がらない、(d) 1 回目だけ下がって 2 回目（348ms）が不変
  → 初回コストの別要因（初回 didChange 後の RA 解析・ファイル I/O・世代送信）を探す。

> **旧 棄却条件「TUI の編集直後のヒントが消える」は撤回**（検証不能だった）。
> `minae` は inlay hint を描画しない: `minae/src/colors.rs:177` に「MVP では描画しない」、
> `minae/src/render.rs:1158` はテスト用に `vec![]`。hint の消費者はエージェントの
> `minas hints`（ADR-0020。**自前で pull するので往復数は元から 1 回**）と、
> daemon のキャッシュ（= `minas get` スナップショットの `inlay_hints`）だけ。

### 別候補（後回し）

- `open_workspace_files`（rename/references の前に全ファイル didOpen する回避策）が
  索引完走ゲートの導入後も必要か。不要なら複数ファイル rename のコストが下がる
  （cold の 6.7 秒の大半がこれの可能性）。
- L2 での規模検証（fixture が小さいので `wall_ms` は外挿不可。`calls` / `equiv_B` は外挿可）。
- `ServerMetrics` の穴（`rename` のカウンタ、`get_state_total` の bytes）。
- 上位モデルでの再現（L2 は nano のみ）。
- **計時ログが無い**ので `apply` 内部の内訳は A/B 除去でしか測れない。
  恒久的に測れるようにするなら daemon に計時ログを足す（計測の穴を埋める）。

## 5. 次のセッションの起動手順

**新しいセッションの入口**（`AGENTS.md` から `docs/loop/README.md` に辿れる。
プロンプトテンプレート `/dev_minas` でも同じ指示を展開できる。手で貼るならこれ）:

```text
minas/minad のループエンジニアリング（エージェントのコスト低減）の続きをやって。
まず docs/loop/method.md と docs/loop/latest.md を読み、
latest.md の §4（次の課題設定）から iteration #4 を回して。計測したら --log で
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

**反復番号の規則**: 今回の反復番号は §4 の見出しの番号（いまは **iteration #4**）。
結果は `log.md` に「iteration #4」として書き、考察の要約をこの `latest.md` に反映したうえで、
§4 を次の番号（#5）の課題設定に書き換える。§5 はこの手順のまま使う。
