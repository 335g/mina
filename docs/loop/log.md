# agent-loop-log — minas コスト低減ループの記録

課題設定（仮説）→ L0 検証 → 考察 → 修正 → 効果確認 の各反復の生データ。
検討方法: [`method.md`](./method.md)。最新結果と次の課題設定: [`latest.md`](./latest.md)。
L2（実LLM A/B）の結果は `docs/benchmarks/agent-editor-ab-results.md`。

指標: `calls` 往復数 / `out_B` 総出力バイト / `equiv_B` 再送込みトークン等価量 /
`wall_ms` 実時間 / `fails` 失敗ステップ数（迷いの代理） / `ok` 目的達成。

---


## 2026-09-12 02:07 — 仮説: (1) `apply`→`check` は検証の最短経路で cargo 委譲より安い。(2) 全文 read より LSP 探索（symbol→at→read --lines）の方が equiv_B で優位。(3) 複数箇所の編集は `--hunks-stdin` で往復が減る。

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568    1300.4         0  True
explore  dump                  2      4750       298      4510      9260     168.0         0  True
rename   lsp                   2       219         3       219       438    1773.1         0  True
rename   apply                 5       406         4      1018      1424    2925.2         0  True
verify   apply-check           2       246         2       108       354   11550.7         0  True
verify   apply-cargo           2       108         1       108       216    1569.4         0  True
verify   hunks-cargo           2       154         1       154       308    1890.9         0  True
verify   apply2-cargo          3       218         2       327       545    1836.9         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2


## 2026-09-12 02:08 — 仮説: LSP を使う契約（`symbol` / `rename` / `check`）は warm と cold で同じ結果を返す（cold に無言の誤りは無い）。

cold 測定（warmup なし・1 回観測）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3       886        17       289      1175    1880.2         1  True
explore  dump                  2      4750       298      4510      9260     168.7         0  True
rename   lsp                   2       138         2       138       276    2672.4         1 False
rename   apply                 5       406         4      1018      1424    2251.8         0  True
verify   apply-check           2       111         2       108       219    5671.2         1  True
verify   apply-cargo           2       108         1       108       216     930.1         0  True
verify   hunks-cargo           2       154         1       154       308     761.5         0  True
verify   apply2-cargo          3       218         2       327       545    1381.6         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=69, symbol_search_total=1
- `explore/lsp` 失敗 step (silent): `minas symbol src/main.rs validate` rc=0 
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/lsp` 失敗 step: `cargo check --offline` rc=101 Checking l0fix v0.1.0 (/Users/335g/dev/other/mina/tmp/loop/rename-lsp-020800-14306)
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-check` 失敗 step: `minas check src/config.rs` rc=2 check src/config.rs: LSP error: diagnostics pull failed (server dead or session lock timeout)
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2

### 考察（iteration #1）

**計測器の校正**: 初版は warmup を固定 sleep でやっていたため、rust-analyzer の索引が
残っている間の LSP セッションロック待ちが `wall_ms` に混ざり 0.4〜16s と暴れた。校正後は
「プローブ（`minas symbol …`）が 1500ms 以内に 2 回連続で当たる」まで待つ方式にした
（`warm_lsp`）。校正前の観測は表から破棄。`calls` / `out_B` / `equiv_B` は校正の影響を受けない
（決定論的）。

**仮説 (1) `apply`→`check` は最短の検証経路 — 棄却。**
`calls` も `equiv_B` も `apply`→`cargo check` と同一（2 / 216〜354）だが、`wall_ms` が
1569 → 11551ms（7.4 倍）。しかも `check` が返すのは `settled: false`（clean-unverified）で、
**クリーンの根拠にならない**（ADR-0045 の設計どおりの挙動）。クリーンを確認するには
結局 cargo へ落ちるので、`check` の往復は純損失になる。`check` の `rc` も非決定
（10.1s で `rc=0 settled:false` と、約 4〜5s で `rc=2` LSP error の両方を観測）。
**迷いの源**: エージェントは `check` の 2 通りの非回答（未確認 / エラー）を見て、次の一手を
契約から決められない。

**仮説 (2) LSP 探索は全文 read より equiv_B で優位 — 確認。**
`explore/lsp` 1568 vs `explore/dump` 9260 equiv_B（−83%）。代償は `calls` +1 と `wall` +1.1s。
再送加重では「早いターンの大出力」が支配的なので、3 往復で 1KB を返す方が 2 往復で 4.75KB より安い。
T1 の知見（範囲 read）を L0 で決定論的に再現した。

**仮説 (3) 複数編集は `--hunks-stdin` で往復が減る — 確認。**
`hunks-cargo` 2 calls / 308 equiv 対 `apply2-cargo` 3 calls / 545 equiv（−44%）。
往復削減がそのまま `equiv_B` に出る（同じ 2 編集）。

**追加で確認**: `rename/lsp` 2 calls / 438 equiv / 1773ms 対 `rename/apply` 5 calls / 1424 equiv /
2925ms。T5/T9 の LSP 優位を L0 で再現。

**仮説 (4) cold でも warm と同じ結果 — 棄却（本反復の最重要）。** cold では 3 つ壊れる:

- `minas symbol` が `[]` + **exit 0**（無言の誤り。2 回目で当たる。`rc` では検出できない）
- `minas rename` が `src/main.rs` を落としたまま「成功」を報告し、`cargo check` で
  初めて E0609 として検出（`silent` ではなく `rc` で出るのはハーネスが検証を持っているから）
- `minas check` が `rc=2` / `LSP error: diagnostics pull failed (server dead or session lock timeout)`
  （実体は「まだ索引中」。メッセージが原因を誤って伝えるので回復手順を誤る）

**共通の根本原因（1 箇所）**: daemon は rust-analyzer の `$/progress`（索引・workspace ロード）を
購読していない（`minad/src/lsp.rs` に progress の扱いは 0 箇所）。したがって LSP の
「仕事が終わった」を知る手段が無く、索引未完の応答を「準備完了」として返す。ADR-0045 は
「解析完了を LSP 応答から確実に検知する手段は無い」として `settled:false` に倒したが、
RA は `$/progress` を送っており、購読すれば検知できる。3 つの症状は同じ 1 箇所の修正対象。

### iteration #2 の課題設定

**仮説**: daemon が `$/progress` を購読し、`workspace/symbol` / `rename` / `check` を
「当該 workspace の索引完了まで待つ」ゲートに通せば、(a) cold の無言の誤りが消え、
(b) `check` はクリーンを `settled: true` で即返せる（10s → 数十 ms）。

**受理条件（L0 `--cold`）**: `silent` = 0 / `rename/lsp` の `ok` = True / `apply-check` の
`wall_ms` が 11551 から 1s 未満に低下し、`check` が `settled:true` を返す。
**棄却条件**: `$/progress` 購読でも RA が clean に対して安定シグナルを出さない →
`check` の契約を作り直す（クリーン確認を cargo に委ね、`check` を診断取得専用に降格）。

**計測の穴（ループの前提）**: `minas info` の `ServerMetrics` はサブコマンド別の
`*_total` / `*_bytes` を持つが、`rename` のカウンタがなく、`get_state_total` にも bytes がない。
L0 は stdout バイトを外側で数えるので比較はできるが、「測れないものは改善できない」ので、
サブコマンドを追加したら同時にカウンタも追加するのを習慣にする。

**限界**: fixture が小さく `cargo check` が 0.2〜0.5s で終わる。仮説 (1) の結論をそのまま
実プロジェクトに外挿しない（規模での確認は L2 の仕事）。また LSP セッションが存在すると
1 編集あたりが 127ms → 345ms に鈍る（`apply` が LSP へ通知するため）ことも観測 — 実クレートで
再測する価値がある。

## iteration #2 — 修正: LSP の索引完走（`$/progress`）を待つゲート

## 2026-09-12 02:19 — 修正後の cold 計測（1 回観測）。ゲートを入れ、索引未完の応答は待ってから返す。

cold 測定（warmup なし・1 回観測）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568    9130.8         0  True
explore  dump                  2      4750       298      4510      9260     168.9         0  True
rename   lsp                   2       219         3       219       438    9608.1         0  True
rename   apply                 5       406         4      1018      1424    1325.5         0  True
verify   apply-check           2       246         2       108       354   18375.3         0  True
verify   apply-cargo           2       108         1       108       216     418.1         0  True
verify   hunks-cargo           2       154         1       154       308     415.1         0  True
verify   apply2-cargo          3       218         2       327       545     749.5         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2


## 2026-09-12 02:25 — 仮説: LSP の索引完走（`$/progress`）を待つゲートを入れれば、(a) cold の無言の誤り（空の symbol・部分 rename・`check` の LSP error）が消え、(b) warm の契約費用は変わらない。warm 計測（r=3 中央値）

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568    1323.4         0  True
explore  dump                  2      4750       298      4510      9260     180.5         0  True
rename   lsp                   2       219         3       219       438    1815.2         0  True
rename   apply                 5       406         4      1018      1424    2700.0         0  True
verify   apply-check           2       246         2       108       354   11152.4         0  True
verify   apply-cargo           2       108         1       108       216    1158.0         0  True
verify   hunks-cargo           2       154         1       154       308    1423.3         0  True
verify   apply2-cargo          3       218         2       327       545    1516.5         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2

### 考察（iteration #2）

**修正**: daemon が `window.workDoneProgress: true` を advertise し、`$/progress` を
購読して、索引完走（outstanding が空 + 静止確認 300ms）まで `ensure` が待つ
（ADR-0051）。待ちはセッションごとに 1 回だけ・ロックを持たずに待つ・逃げ道 2 つ
（進捗を送らないサーバは 1s、上限 10s）。

**受理条件の判定**（cold）:

- `silent` = 0 ✓（`explore/lsp` の `minas symbol` が空を返さなくなった。
  `out_B` が 886 → 1017B に増えたのは実データが返ったため）
- `rename/lsp` の `ok` = True ✓（219B = 2 ファイル分。以前は 138B の部分 rename を
  「成功」として報告していた）
- `apply-check` の `rc=2`（`LSP error: diagnostics pull failed`）が消えた ✓
- **受理条件の 3 番目「`apply-check` の wall が 1s 未満かつ `settled:true`」は撤回する。**
  実測で前提が崩れたため（下記）。cold の `apply-check` は 18375ms — 索引待ち
  （約 8 秒）+ 従来の settle 予算（10 秒）。誤った応答を速く返していた分が、
  正しい応答を待つ時間に変わっただけ。この 10 秒は iteration #3 の課題。

**warm は無変化**（`calls` / `equiv_B` は iteration #1 と同一、`wall_ms` は誤差内）。
確認済みフラグにより、静止確認 300ms はセッション生成時の 1 回しか払わない。
L0 の warmup フェーズがその 1 回を吸収する。

**撤回の根拠（H5 の再検証）**: 「空応答 + 索引完走 = クリーン確定」に倒せるかを
実測した（warm・索引完走済みの rust-analyzer）。**倒せない**:

| 壊し方 | `check main.rs` | `check config.rs` |
|---|---|---|
| config.rs のフィールド削除（参照は main.rs に残る） | 空 + `settled:false`（exit 0） | 「no such field」を検出（exit 2, 1054ms） |
| 関数名タイポ（`validate` → `validate2`） | 空 + `settled:false` | 空 + `settled:false` |
| config.rs に未知の型 | 空 + `settled:false` | 「missing structure fields」を検出 |

つまり **rust-analyzer の pull は、索引完走後でも実在するエラーを取りこぼす**
（コンパイルは壊れているのに exit 0 + 空が返る）。ADR-0045 が `settled:false` に
倒した判断は正しく、進捗の購読では置き換えられない。`check` の空を「クリーン」に
する修正はしない。

**計測器への反映**: L0 の warmup プローブが「索引が読めたか」を外から待つ処理は、
本体に同じ判断が入ったので冗長になった（残してある — 本体の退行を L0 が検出できる
ようにするため、外から独立に待つのが正しい）。

### iteration #3 の課題設定

**課題**: `check` は**空（クリーンまたは取りこぼし）のときだけ**約 10 秒かかる。
エラーがあるときは 594〜1054ms で返る（実測）。10 秒は「空のまま予算切れ」を
`settled:false` として返すための待ちで、エージェントは何も得られない。

**仮説**: 索引完走済み + outstanding が空のセッションでは、pull が空を返した時点で
「これ以上待っても答えは変わらない」と判定できる（`$/progress` は索引作業しか
表さないが、少なくとも「サーバが仕事中だから待たされている」わけではない）。
`check` の空応答を k 回の pull の後に早期確定すれば、`wall_ms` が 10000 → k×500ms に
なる。

**測定すべきこと（棄却条件）**: k を小さくすると**実在するエラーを取りこぼす**
（実測: エラーの初出は 594〜1054ms）。k ごとに「壊れたコードを検出できるか」と
`wall_ms` を測り、両方を満たす最小の k を選ぶ。満たす k が無ければ、`check` を
「診断取得専用（空は未確認のまま即返す）」に降格し、クリーン確認を `cargo` に
一本化する（契約の変更。skill / README / ADR-0045 の更新を伴う）。

**次の測定**: fixture を「エラーが後から顕在化する」形（編集直後に pull が空を
返し、数百 ms 後に非空になる）にして、pull の初出遅延を分布で測る。

## iteration #3 — 修正: 空の pull は早期に確定する（10 秒待たない）

## 2026-09-12 10:21 — 仮説: 空の pull は 1 回で最終集合（測定済み）なので、空の早期確定（連続2回）で check の wall が 10000ms 台から 1000ms 未満に落ち、かつ pull が見えるエラー（構文）は取りこぼさない。warm 計測（r=3 中央値）

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568    1282.1         0  True
explore  dump                  2      4750       298      4510      9260     165.8         0  True
rename   lsp                   2       219         3       219       438    1796.3         0  True
rename   apply                 5       406         4      1018      1424    2721.1         0  True
verify   apply-check           2       246         2       108       354    2021.8         0  True
verify   apply-cargo           2       108         1       108       216    1575.0         0  True
verify   hunks-cargo           2       154         1       154       308    1882.2         0  True
verify   apply2-cargo          3       218         2       327       545    1971.8         0  True
verify-blind check                 2       240         2       104       344    1635.1         0  True
verify-blind cargo                 2       104         1       104       208    1232.2         0  True
verify-broken check                 2       450         2       105       555    1671.2         0  True
verify-broken cargo                 2       105         1       105       210    1155.8         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=386, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-12 10:23 — 仮説: 空の早期確定は cold でも効き、索引待ち（ゲート）以外の待ちを無くす。索引未完の無言の誤りも出ない。cold 計測（1 回観測）

cold 測定（warmup なし・1 回観測）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568    6740.2         0  True
explore  dump                  2      4750       298      4510      9260     173.3         0  True
rename   lsp                   2       219         3       219       438    6712.3         0  True
rename   apply                 5       406         4      1018      1424    1322.8         0  True
verify   apply-check           2       246         2       108       354    6414.8         0  True
verify   apply-cargo           2       108         1       108       216     424.5         0  True
verify   hunks-cargo           2       154         1       154       308     434.8         0  True
verify   apply2-cargo          3       218         2       327       545     721.7         0  True
verify-blind check                 2       240         2       104       344    6254.1         0  True
verify-blind cargo                 2       104         1       104       208     436.5         0  True
verify-broken check                 2       450         2       105       555    5800.6         0  True
verify-broken cargo                 2       105         1       105       210     336.7         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1

### 考察（iteration #3）

**最初の測定（`pull` の初出遅延）**: rust-analyzer を直叩きして、編集後の pull を
50ms 間隔で 12 秒観測した。結果は **round0 == round11**（1 回目で最終集合）で、
空が後から非空に変わることは無かった。クロスファイルの型エラーは
**12 秒待っても空のまま**。つまり予算 20×500ms は「同じ答えしか返らない待ち」。
daemon の settle 判定が「非空が 2 回連続で同数」なのは、そもそも空を確定できない
構造だったため（ADR-0045 の `settled:false` は正しいが、待つ意味は無い）。

**pull が見るエラー / 見ないエラー（warm・索引完走後・汚染なしの再測定）**:

| 壊し方 | main.rs の pull | config.rs の pull |
|---|---|---|
| 構文エラー（main.rs） | **round0 で出る** | — |
| メソッド解決（`cfg.validate2()`） | **永久に空**（`cargo check` は検出） | — |
| フィールド削除（config.rs） | 空のまま | round0 で `no such field` |

→ pull は「同じファイルに閉じた構文エラーと一部の型エラー」を即返し、
「メソッド解決の失敗」のような実在するエラーを取りこぼす。**待っても直らない**。
`settled:false`（空 = 未確認）はこのため必要で、早期確定と両立する。

**修正**: `pull_diagnostics_settled` は連続 2 回の空で打ち切り（ADR-0052）。
Open 経路の `settle_open_diagnostics_loop` も同じ定数を共有（30 秒 → 実測 2.7 秒、
pull 120 往復 → 約 6 往復）。非空の安定判定（2 回連続同数 = `settled:true`）は不変。

**結果（warm r=3）**: `check`（クリーン）**10154 → 589ms（−94%）**、`apply`→`check`
合計 11550 → 2022ms。`explore` / `rename` は不変（回帰なし）。cold も全アーム
`ok` / `fails=0` / `silent=0` で、索引待ち（約 6 秒）以外の待ちが消えた。

**棄却条件の確認（早期確定がエラーを落とさないか）** — 新 flow 2 本で常時検証:

- `verify-broken`（構文エラー = pull が見る）: `check` は rc=2 + `Syntax Error`
  を **591ms** で返す ✓（落としていない）
- `verify-blind`（メソッド解決 = pull が見ない）: `check` は rc=0 + `settled:false`
  + 空を **592ms** で返す（偽クリーンを主張しない ✓）。同じ編集で `cargo check` は
  rc=101 で検出する（= 二段構えが必要な理由がデータで残る）

**L0 計測器の改善**: 期待 exit コード（`ok_rc`）と per-step の `expect` を足した。
「0 以外の exit が正常」な flow（エラー検出の検証）を `fails`/`ok` で正しく扱える。
`expect` は stderr も見る（cargo のエラーは stderr に出る）。mock サーバは TODO の
無いテキストで診断をでっち上げるのをやめた（空応答の経路をテストできるように）。

**契約の更新**: `minas skill errors` の「settled:false は約 10 秒の予算切れ」という
記述を「早期に返る。待ってもクリーンにはならない」に更新（エージェントの行動は
不変 — 空を信用せず cargo に行く）。

### iteration #4 の課題設定

**課題**: `apply` が 1 ファイルあたり 1.0〜1.5 秒かかる（`check` の 0.59 秒より高い）。
`apply` の応答は `sync_after_edit`（didChange + `pull_after_edit`）を通り、
`pull_after_edit` は `PULL_SETTLE`(250ms) + 診断 pull + **inlay hint pull** を行う。
300 行の config.rs では hint の計算が重い（apply が main.rs で 1.0 秒、config.rs で
1.5 秒という差が hint のコストを示唆）。

**仮説**: 編集直後の hint pull は、編集の応答に不要（hint は次の描画・明示要求で
足りる）。`sync_after_edit` から hint を外せば `apply` の wall が
1.0〜1.5 秒 → 0.3〜0.4 秒になり、hint は Open 経路の settle か明示 pull で埋まる。

**棄却条件**: hint を外した結果、(a) TUI の編集直後のヒントが消えたままになる、
(b) `minas hints` の往復が増える、のいずれかが L0/L2 で見えたら戻す。
測り方: `apply` ステップの wall（hint あり / なし）と、`hints` を使う flow を追加して
往復数とバイトを比較する。

**別候補（前回から保留）**: `open_workspace_files`（rename/references の前に
ワークスペース全ファイルを didOpen する回避策）が、索引完走ゲートの導入後も必要か。
不要なら複数ファイル rename のコストが大きく下がる。


## 2026-09-12 12:39 — iteration #4: 初回 apply の ~1.1s の主因は hint pull か? — step2 で pull_after_edit から hint pull を外して A/B → 1回目不変（仮説棄却）。トレースで内訳確定: 初回コスト = 初回 pull_diagnostics 内の RA 再解析 ~600-1100ms（hint pull ≈0ms）。hints flow を追加

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568    1290.6         0  True
explore  dump                  2      4750       298      4510      9260     164.5         0  True
hints    hints                 1       110         8         0       110     730.8         0  True
rename   lsp                   2       219         3       219       438    1737.2         0  True
rename   apply                 5       406         4      1018      1424    2648.3         0  True
verify   apply-check           2       246         2       108       354    1964.7         0  True
verify   apply-cargo           2       108         1       108       216    1521.8         0  True
verify   hunks-cargo           2       154         1       154       308    1812.5         0  True
verify   apply2-cargo          3       218         2       327       545    1913.0         0  True
verify-blind check                 2       240         2       104       344    1589.3         0  True
verify-blind cargo                 2       104         1       104       208    1168.3         0  True
verify-broken check                 2       450         2       105       555    1549.4         0  True
verify-broken cargo                 2       105         1       105       210    1144.7         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=386, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## iteration #4 — 考察（hint pull 仮説は棄却。内訳はトレースで確定）

**結論**: §4 の仮説「初回 apply の ~1.1s は hint pull が主因」は**棄却**。
内訳は daemon に一時トレースを入れて直接確定した（A/B 除去でなく実測）。

### 測り方 1（内訳の再現確認）

`-f verify -r 3 -v`（iteration #4 開始時、変更なし）:

- `apply2-cargo`: 1 回目 apply **1373ms** / 2 回目 **352ms**（iteration #3 の
  1470/348、別セッション再測 1423/348 と一致 — 初回 vs 2 回目の差 ≈1.0s は再現）

### 測り方 2（1 変更だけ: hint pull 除去）→ 仮説棄却

`pull_after_edit` から `pull_inlay_hints` だけ外して再 build（行為の契約は他に触れない）、
`-f verify -f explore -f rename -r 3`:

| flow/arm | 比較 | 結果 |
|---|---|---|
| `verify/apply-cargo` 1 回目 | 1344 → **1430ms** | 不変（むしろ微増。ノイズ） |
| `verify/apply2-cargo` 1 回目 / 2 回目 | 1373/352 → **1422/342ms** | 不変 |
| `rename/apply` 1 回目 | — → **1400ms**（他は 350/343/389） | 不変 |
| `explore/*` `rename/lsp` | calls/equiv_B | 完全不変 |

→ 棄却条件 (c)「apply の 1 回目が下がらない」に該当。**hint pull は初回コストの
主因ではない**（温まれば ≈0ms。2 回目 apply も除去前後で 352→342ms と不変）。
対案（測り方 3: キャッシュ破棄）は「hint pull が高い」前提が崩れたので動機なし。
変更は revert（`hints = None` のまま残すと編集後ヒントが stale になり棄却条件 (b)）。

### 内訳の直接測定（仮説の修正）

`pull_after_edit` と `sync_after_edit` に一時トレース（eprintln → minad.log）を入れ、
`verify/apply2-cargo` を 2 回適用で観測:

```
[trace-sync] sync=0ms pull_total=856ms      ← 1 回目 apply（apply 合計 ~1003ms）
[trace-pull] settle=251ms lock_wait=0ms diag_pull=604ms ns=0
[trace-sync] sync=0ms pull_total=253ms      ← 2 回目 apply（合計 ~352ms）
[trace-pull] settle=252ms lock_wait=0ms diag_pull=0ms ns=0
```

- `lsp::sync` ≈0ms — didChange は fire-and-forget。RA は lazy で応答不要。
- 初回コストの実体は **初回 `pull_diagnostics` 内の RA 再解析 ≈600ms**（他 run では
  ~1100ms。run 間で 1003–1518ms と振れるのはここ）。didChange 直後の最初の診断
  要求がクレート再解析を背負う。
- 2 回目は diag_pull ≈0ms（増分解析）。hint pull は 1〜数 ms。
- `PULL_SETTLE` 250ms は**毎回の apply に定数で乗る**（1 回目も 2 回目も）。

**重要な観察**: settle と diag_pull は加法（856 = 251 + 604）— 待っている間 RA の
解析は進んでいない＝**RA は診断要求まで解析を始めない**（lazy）。すると settle は
「解析が終わるのを待つ」のではなく「didChange 通知が RA に消費されるのを待つ」
だけの可能性が高い。250ms はそのためには過大で、次の課題にできる
（iteration #5: `PULL_SETTLE` の削減 — 空 pull の回帰を verify-broken/blind で
見張りながら）。

### 測り方 4（hints flow 追加 — 計測器の穴を埋める）

`docs/loop/l0.py` の `FLOWS` に `hints` を追加:
- 対象は `src/main.rs`（config.rs は let 束縛が無くヒントが無い — `[]` が正常）。
  `let cfg = Config::default()` の型ヒント `: Config` を `need` にする。
- 結果: 1 call / 110B / ~731ms（fresh daemon の初回取得は初回解析を背負う。回帰は
  calls と応答欠けで見る）。キャッシュでなく自前 pull（ADR-0020）なので往復 1 回
  は契約どおり。

### 資産

- 仮説「hint pull が初回 apply の主因」を **L0 で棄却**（L2 は回さない — method.md §3）。
- 初回 apply の内訳が確定: 250ms 定数（settle）+ RA 初回再解析 ~600–1100ms + 編集処理 ~150ms。
- `hints` flow 追加で hint 経路の費用・応答欠けを今後実測できる。
- iteration #5 の課題は「毎回の apply に乗る `PULL_SETTLE` 250ms」に置ける
  （初回再解析自体は LSP の本質コストで、契約上は外せない — 外すなら check の
  settled 契約を変えることになり、別の大きい課題）。

## 2026-09-12 14:14 — iteration #5: 毎回の apply に乗る PULL_SETTLE 250ms は通知消費待ちだけで、pull 自身が解析完了までブロックする（ADR-0052）ので削れる。50ms / 0ms の A/B で空 pull 回帰なし → settle を撤去（ADR-0053）

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568    1283.5         0  True
explore  dump                  2      4750       298      4510      9260     163.7         0  True
hints    hints                 1       110         8         0       110     707.0         0  True
rename   lsp                   2       219         3       219       438    1740.6         0  True
rename   apply                 5       406         4      1018      1424    1637.9         0  True
verify   apply-check           2       246         2       108       354    1652.4         0  True
verify   apply-cargo           2       108         1       108       216    1280.6         0  True
verify   hunks-cargo           2       154         1       154       308    1274.9         0  True
verify   apply2-cargo          3       218         2       327       545    1352.4         0  True
verify-blind check                 2       240         2       104       344    1285.8         0  True
verify-blind cargo                 2       104         1       104       208     878.8         0  True
verify-broken check                 2       450         2       105       555    1370.2         0  True
verify-broken cargo                 2       105         1       105       210     843.8         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=386, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## iteration #5 — 考察（`PULL_SETTLE` 250ms は削れた。しかも 0 まで）

**結論**: §4 の仮説「`PULL_SETTLE` を短くすると apply の wall が毎回下がる」は**採用**。
予想より強く、**50ms で空回帰が出なかったので 0ms（撤去）まで試し、それも
空回帰が出なかったため settle 自体を削除した**（ADR-0053）。apply 系全体が
毎回 ~230ms 下がった。

### 測り方 1（50ms で A/B）

`minad/src/lsp.rs` の `PULL_SETTLE` を 250ms → 50ms にして `cargo build`、
`-f verify -r 3 -v`（iteration #4 開始時の基準値は apply-cargo 1 回目 1359ms /
2 回目 346ms。今回の開始時再測は 1 回目 ~1300–1400ms / 2 回目 346ms）:

| step | 250ms | 50ms | 差 |
|---|---|---|---|
| `verify/apply-cargo` apply 1 回目 | 1296–1405ms | 1188ms | −150〜−200ms |
| `verify/apply2-cargo` apply 2 回目 | 346ms | 156ms | **−190ms** |
| `verify/apply2-cargo` apply 1 回目 | 1359ms | 1132ms | −227ms |

2 回目の −190ms は「settle の短縮分がそのまま出た」形（2 回目は増分解析で
diag_pull ≈0ms なので settle がほぼ全額）。**仮説どおり、settle は解析と重ならない
純粋な遅延**だった（iteration #4 の加法の観察と整合）。

空 pull の回帰は無し: `verify-broken/check` は rc=2 / 345B に `Syntax Error` 2 件
（fails 0）、`verify-blind/*` も fails 0。

### 測り方 2（0ms = 撤去で A/B）

「50ms が安全」なら「0ms も安全」かを確かめた（固定待ちが本当に要るのか、
要らないなら定数ごと消す方が小さい）:

| step | 250ms | 50ms | 0ms（撤去） |
|---|---|---|---|
| `verify/apply2-cargo` apply 1 回目 | 1359ms | 1132ms | 1133ms |
| `verify/apply2-cargo` apply 2 回目 | 346ms | 156ms | **107ms** |
| `verify-broken/check` | rc=2 345B | rc=2 345B | **rc=2 345B（回帰なし）** |
| `verify-blind/*` | fails 0 | fails 0 | **fails 0** |

さらに回帰の取りこぼしを減らすため **`-r 10`（10 回観測）で 30 run** 回した:
`verify-broken`（check/cargo）・`verify-blind`（check/cargo）・`hints` の
**fails 0 / ok True 全件**、`hints` は 1 call 110B で不変（編集後ヒントも出続ける）。
1 回目の apply が 50ms と 0ms で同値（1132 vs 1133ms）なのは、撤去後に残る
**RA の初回再解析 ~600–1100ms が下限**だからで、settle 分はもう乗っていない。

### 測り方 3（プロトコル直叩き — daemon を介さない独立の根拠）

「なぜ settle 無しで空が出ないのか」を daemon の外で確かめた。
`python3 docs/loop/probe_pull_diagnostics.py tmp/loop/probe0 4 0.0`（round 間の
待ちを 0 にした）:

| ケース | round0 | round3 |
|---|---|---|
| 構文エラー（main.rs） | `Syntax Error` ×2 | 同じ（round0 == round3） |
| フィールド削除 / 未知フィールド（config.rs） | `no such field` | 同じ |
| メソッド解決エラー | 空 | 空（ADR-0052 どおり永久に空） |

**didChange 直後（待ち 0）の 1 回目で最終集合が返る** — pull 自身が解析完了まで
ブロックするので、こちらで待つ必要が無い。

### 原因の帰属（なぜ空が出ないか）

1. **順序**: `notify(didChange)` と `request(textDocument/diagnostic)` は同じ
   `LspSession` の `write` に、同じ mutex（`session.lock()`）の下で**直列に**書かれる
   （`mina-lsp` の `Client::notify` は `write_frame` の完了を await して返る）。
   daemon 側で notify が fire-and-forget に見えるのは「応答を待たない」だけで、
   書き込み自体は完了している。よって pull が didChange を追い越すことはない。
2. **RA は lazy**: 診断要求が来るまで解析を始めない（iteration #4 の加法の観察）。
   解析は**要求の中で**走り、pull はその完了を待って最終集合を返す（ADR-0052:
   round0 == round11）。つまり 250ms は「解析の先走り」を買えていなかった。

### 採用した修正

`minad/src/lsp.rs`: `PULL_SETTLE` 定数と `tokio::time::sleep` を削除。
`pull_after_edit` の doc コメントは「撤去した理由（ADR-0053）」＋回帰時の
upgrade path（固定待ちを戻すのではなく push を解析完了シグナルに使う）に置換。
設計判断は `docs/adr/0053-no-pull-settle-after-edit.md` に、`docs/adr/0020`
（inlay hint の settle に触れている）には注記を追加。

### 回帰

- `cargo test`: **462 passed**（変更前と同じ）
- `calls` / `out_B` / `equiv_B`: **全 flow 完全不変**（契約は変えていない。
  `explore` 1017/1568、`rename/lsp` 219/438、`rename/apply` 406/1424、
  `verify/apply2-cargo` 218/545 …）
- `fails`: 全 flow 0（30 run の追加観測を含む）
- `hints`: 1 call / 110B のまま（編集後の hint pull も従来どおり）

### 数字の読み方（外挿の範囲）

- `wall_ms` の絶対値は fixture が小さいので外挿不可（method.md §7）。ただし
  「毎回 250ms の固定待ちが消えた」は**契約とコードの事実**で、規模に依らず効く。
- `explore` / `rename` の wall も同程度下がっている（`rename/apply` では
  1 回目の apply が減る。順序の都合で中央値は揺れる）。

### 資産

- `PULL_SETTLE` の撤去（ADR-0053）— apply 系全体で毎回 ~250ms。
- 「settle が要る」という前提が**計測で 2 回否定された**（iteration #4: 加法、
  #5: 0ms でも空回帰なし）。次に同じ症状（診断が消える）を見たら、settle ではなく
  順序（didChange が書かれたか）か push 通知を疑う。
- 計測器: `verify-broken` / `verify-blind` の `-r 10` 運用が「空回帰の見張り」として
  機能することを確認（fails / silent がそのまま検出器になる）。

### 次の課題（iteration #6）の下調べ — 残っている ~600ms の正体

`apply` から settle を外した後、もう一度「1 コマンドの内訳」を測り直した
（`tmp/loop/recon6.py`、同じ daemon に順に投げて wall を見るだけ。索引完走後）:

| コマンド | 1 回目 | 2 回目 | 3 回目 |
|---|---|---|---|
| `minas symbol src/main.rs validate` | 586ms | 588ms | — |
| `minas at src/config.rs 17:12` | 637ms | 84ms | — |
| `minas hints src/main.rs` | 730ms | 82ms | — |
| `minas check src/config.rs` | 945ms | 594ms | 587ms |
| `minas apply …` | 137ms | 98ms | 102ms |
| `minas rename …` | 609ms | 609ms | — |

- `symbol` と `rename` は**毎回 ~590–610ms**（初回だけではない）。`apply` は
  100ms 台 — settle 撤去の効果がそのまま出ている。ここで `apply` が速いのは、
  このシーケンスでは `symbol`/`check` が先に解析を起こしているため
  （= 初回 apply の ~1s は RA の初回再解析であって待ちではない、の裏取り）。
- `check` の 594ms も、既知の 500ms（`SEMANTIC_RETRY_WAIT`）+ 1 往復に一致。
- 残りの正体は `minad/src/lsp.rs` の `request_with_loading_retry`:
  **「2 回連続で同一」を確かめるために `SEMANTIC_RETRY_WAIT`（500ms）を挟んで
  必ず 2 回目を投げる**。結果が既に完全でも 500ms を払う（`symbol` / `rename` /
  `references` が全部これを通る）。`pull_diagnostics_settled` の 500ms も同じ定数。
  → iteration #6 の課題はここに置く（`latest.md` §4）。

## 2026-09-12 14:17 — iteration #5 の cold 再測（PULL_SETTLE 撤去後 — ADR-0053）。§3 の cold 表を更新し、索引完走ゲート（ADR-0051）が cold rename を守っていることを確認する

cold 測定（warmup なし・1 回観測）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568    8825.4         0  True
explore  dump                  2      4750       298      4510      9260     168.7         0  True
hints    hints                 1       110         8         0       110    8067.4         0  True
rename   lsp                   2       219         3       219       438    9027.6         0  True
rename   apply                 5       406         4      1018      1424     581.9         0  True
verify   apply-check           2       246         2       108       354    8257.6         0  True
verify   apply-cargo           2       108         1       108       216     419.1         0  True
verify   hunks-cargo           2       154         1       154       308     426.3         0  True
verify   apply2-cargo          3       218         2       327       545     497.8         0  True
verify-blind check                 2       240         2       104       344    9094.7         0  True
verify-blind cargo                 2       104         1       104       208     433.8         0  True
verify-broken check                 2       450         2       105       555    8793.3         0  True
verify-broken cargo                 2       105         1       105       210     338.1         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-12 14:55 — iteration #6 baseline: SEMANTIC_RETRY_WAIT=500ms のまま

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568    1298.2         0  True
explore  dump                  2      4750       298      4510      9260     161.8         0  True
rename   lsp                   2       219         3       219       438    1718.0         0  True
rename   apply                 5       406         4      1018      1424    1571.9         0  True
verify   apply-check           2       246         2       108       354    1698.4         0  True
verify   apply-cargo           2       108         1       108       216    1271.9         0  True
verify   hunks-cargo           2       154         1       154       308    1257.3         0  True
verify   apply2-cargo          3       218         2       327       545    1374.9         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2


## 2026-09-12 15:01 — iteration #6 A/B: SEMANTIC_RETRY_WAIT 500→0ms (仮説: symbol/rename/check が毎回 ~500ms 下がる)

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568     294.5         0  True
explore  dump                  2      4750       298      4510      9260     169.4         0  True
rename   lsp                   2       219         3       219       438    1312.5         0  True
rename   apply                 5       406         4      1018      1424    1694.1         0  True
hints    hints                 1       110         8         0       110     770.4         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4


## 2026-09-12 15:06 — iteration #6 正しさの見張り: verify/verify-broken/verify-blind (check step が ~500ms 下がるか・契約不変か)

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354    1229.6         0  True
verify   apply-cargo           2       108         1       108       216    1323.3         0  True
verify   hunks-cargo           2       154         1       154       308    1371.5         0  True
verify   apply2-cargo          3       218         2       327       545    1461.1         0  True
verify-broken check                 2       450         2       105       555     850.5         0  True
verify-broken cargo                 2       105         1       105       210     916.7         0  True
verify-blind check                 2       240         2       104       344     892.2         0  True
verify-blind cargo                 2       104         1       104       208     994.4         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-12 15:07 — iteration #6 cold 見張り: SEMANTIC_RETRY_WAIT=0ms で索引ゲートが cold の完全 rename / 非空 symbol を守るか

cold 測定（warmup なし・1 回観測）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
rename   lsp                   2       219         3       219       438    9126.5         0  True
rename   apply                 5       406         4      1018      1424     596.8         0  True
explore  lsp                   3      1017        17       551      1568    8205.0         0  True
explore  dump                  2      4750       298      4510      9260     169.4         0  True
```

- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2


## 2026-09-12 15:14 — iteration #6 -r10: 部分結果の取りこぼしを確率的に踏む (SEMANTIC_RETRY_WAIT=0ms)

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568     314.9         0  True
explore  dump                  2      4750       298      4510      9260     178.4         0  True
rename   lsp                   2       219         3       219       438    1309.9         0  True
rename   apply                 5       406         4      1018      1424    1776.9         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4


## 2026-09-12 15:22 — iteration #6 A/B2: loading のみリトライ + wait 100ms (1往復削減が成立するか)

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1014        17       548      1562     300.4         0  True
explore  dump                  2      4750       298      4510      9260     173.6         0  True
rename   lsp                   2       217         3       217       434    1259.2         0  True
rename   apply                 5       402         4      1008      1410    1739.1         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4941, read_total=1, symbol_range_bytes=248, symbol_range_total=1, symbol_search_bytes=199, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5338, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4


## 2026-09-12 15:29 — iteration #6 確定: SEMANTIC_RETRY_WAIT=0ms 採用（案A・2回確認維持）。回帰全体確認

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1014        17       548      1562     309.9         0  True
explore  dump                  2      4750       298      4510      9260     177.8         0  True
rename   lsp                   2       217         3       217       434    1363.8         0  True
rename   apply                 5       402         4      1008      1410    1779.1         0  True
hints    hints                 1       110         8         0       110     783.4         0  True
verify   apply-check           2       244         2       107       351    1323.1         0  True
verify   apply-cargo           2       107         1       107       214    1376.9         0  True
verify   hunks-cargo           2       153         1       153       306    1353.4         0  True
verify   apply2-cargo          3       216         2       324       540    1487.1         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4941, read_total=1, symbol_range_bytes=248, symbol_range_total=1, symbol_search_bytes=199, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5338, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=177, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2


## 2026-09-12 15:40 — iteration #6 考察: SEMANTIC_RETRY_WAIT 500ms → 0ms（採用・ADR-0054）

### 仮説と A/B の設計

- 仮説: `request_with_loading_retry` は「2 回連続で同一になるまで」確認するが、その間の
  `SEMANTIC_RETRY_WAIT` 500ms は毎回必ず挟まる（1 回目の結果が既に完全でも待つ）。
  要求は自身が解析完了までブロックしてから返る（§4 の根拠・iteration #5 と同型）ので、
  この待ちは「同じ答えをもう一度買っている」だけ。0ms にすれば `symbol`/`rename`/`check`
  の wall が毎回 ~500ms 下がるはず。
- 案 A（採用）: `SEMANTIC_RETRY_WAIT` 500 → **0ms**。2 回連続確認ロジックは残す。
- 案 B（比較のみ・不採用）: 「2 回連続同一確認」を捨て、**loading（null/空）のときだけ
  リトライ**に縮める（1 往復削減）。wait は 100ms に戻して測った。

### 検証結果（warm、r=3 中央値）

| step | #5 基準 (500ms) | 案 A (0ms) | 案 B (loadingのみ+100ms) |
|---|---|---|---|
| `symbol`（explore/lsp 1step） | 583ms | **82ms** | 84ms |
| `rename`（rename/lsp 1step） | 1594ms（2120 揺れ） | **1150ms** | 1162ms |
| `check`（verify/apply-check） | 589ms | **87ms** | — |
| `hints`（pull_inlay_hints、影響外） | 704ms | 770ms | — |

- `calls` / `out_B` / `equiv_B` は全 flow で不変（案 A で explore/lsp 1568→1562、rename/lsp
  438→434 は 1〜4B の表示揺れ。契約の形は同じ）。
- `-r 10`（explore / rename）: fails 0。部分結果の取りこぼしなし。
- `verify` / `verify-broken` / `verify-blind`（r=3）: fails 0。`verify-broken/check` は
  rc=2（Syntax Error 2 件）のまま、`verify-blind/check` は `settled:false` 契約を維持。
- `--cold`（rename / explore を 1 回）: `rename/lsp` の verify（cargo check green = 4 箇所
  すべてリネーム）・`explore/lsp` の非空 symbol（134B）とも維持。索引完走ゲート
  （ADR-0051）が cold の完全性を引き続き守っている。
- `cargo test`: 462 passed。

### 考察

1. **待ちは本当に無駄だった**: 0ms で回帰ゼロ（warm r=3 × 3 flow + cold + -r10 =
   40 run 相当で fails 0）。`apply` の settle 撤去（#5）と同じ結論 — 「解析完了まで
   ブロックして返る」LSP 要求には待ちを挟む余地がない。ADR-0052 の「予算 20×500ms
   ≈10 秒」は実質 0 になったが、予算機構（SEMANTIC_RETRIES のループ）自体は残す
   （遅いサーバ・大きいプロジェクトでの安全弁。loading 時のリトライ回数上限）。
2. **案 B は差が出なかった**: 1 回目の request 自体が ~80ms かかっており、2 回目の確認は
   同一クエリの再送でほぼ無料。1 往復削減の効果（〜40ms）は誤差で、代わりに loading 時に
   100ms 待つコードが残る。**「2 回連続同一確認」は login 上の防御としては安いまま**。
   案 B は複雑さだけ増すので不採用。
3. **残った wall の内訳**: `symbol` 82ms / `rename` 1150ms / `check` 87ms。`rename` が
   他より桁違いに高いのは WorkspaceEdit の計算（RA 側の編集コスト）で、これは待ちでは
   ない。`rename` の 1150ms の内訳は未調査（#7 の候補: トレース）。
4. **hints は影響を受けない**: `pull_inlay_hints` は `request_with_loading_retry` を
   通らない（直接 request 1 回）。770ms は RA 側の inlay hint 計算コストと推定。
5. **ADR の更新**: ADR-0051（索引ゲート）と ADR-0052（check の空の早期確定）の
   「安全代」は、待ち時間ではなく**ロジック（2 回連続同一確認・空の 2 回連続）**が本尊。
   待ちなしでも契約は維持できた。ADR-0054 として記録。

### 結論

- **採用**: `SEMANTIC_RETRY_WAIT` = 0ms（リトライループと 2 回連続確認は残す）。
  `symbol` 583→82ms（−86%）、`rename` ~1594→~1150ms（−28%）、`check` 589→87ms（−85%）。
- 効果確認: `calls`/`equiv_B` 不変・fails 0・462 test green。

## 2026-09-12 17:33 — iteration #7 基準値（現行: apply は pull_after_edit で解析完了までブロック ~1.1s。warm r=3 中央値）

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354    1227.6         0  True
verify   apply-cargo           2       108         1       108       216    1343.9         0  True
verify   hunks-cargo           2       154         1       154       308    1449.5         0  True
verify   apply2-cargo          3       218         2       327       545    1524.2         0  True
verify-broken check                 2       450         2       105       555     984.3         0  True
verify-broken cargo                 2       105         1       105       210     994.6         0  True
verify-blind check                 2       240         2       104       344     939.6         0  True
verify-blind cargo                 2       104         1       104       208     903.3         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-12 18:08 — iteration #7 A/B(a): pull_after_edit から診断 pull のみ外す（hint pull は残す）→ ブロックはどちらの pull が持つか

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354     807.4         0  True
verify   apply-cargo           2       108         1       108       216     860.3         0  True
verify   hunks-cargo           2       154         1       154       308     870.8         0  True
verify   apply2-cargo          3       218         2       327       545     950.8         0  True
verify-broken check                 2       450         2       105       555     598.9         0  True
verify-broken cargo                 2       105         1       105       210     601.1         0  True
verify-blind check                 2       240         2       104       344     576.7         0  True
verify-blind cargo                 2       104         1       104       208     949.7         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-12 18:15 — iteration #7 A/B(a) 再測: 診断 pull のみ外す（hint pull は残す）。前回 719ms は Open 時背景 settle との競合による揺れで、再測は baseline と同値（改善ゼロ）。契約は daemon テスト 2 件が fail

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354    1241.3         0  True
verify   apply-cargo           2       108         1       108       216    1295.5         0  True
verify   hunks-cargo           2       154         1       154       308    1316.4         0  True
verify   apply2-cargo          3       218         2       327       545    1436.6         0  True
verify-broken check                 2       450         2       105       555     867.4         0  True
verify-broken cargo                 2       105         1       105       210     919.6         0  True
verify-blind check                 2       240         2       104       344     879.1         0  True
verify-blind cargo                 2       104         1       104       208     946.8         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-12 18:19 — iteration #7 A/B(b): pull_after_edit から診断+hint pull の両方を外す（apply は診断を返さない = settled:false 相当）→ コストは check へ移るか

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354    1310.1         0  True
verify   apply-cargo           2       108         1       108       216     267.8         0  True
verify   hunks-cargo           2       154         1       154       308     274.6         0  True
verify   apply2-cargo          3       218         2       327       545     361.4         0  True
verify-broken check                 2       450         2       105       555     607.8         0  True
verify-broken cargo                 2       105         1       105       210     220.3         0  True
verify-blind check                 2       240         2       104       344     841.3         0  True
verify-blind cargo                 2       104         1       104       208     280.6         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## iteration #7 — 考察（「apply の診断 pull を外す」は棄却。1.1s は *pull が強制する解析* であって待ちではない。ただし cargo 検証経路では −70〜80% の余地）...

> 課題（§4 で事前登録）: `apply` の `pull_after_edit` を外して `settled:false` 即返しに
> すると apply が ~1.1s → ~150ms になり、トータル（apply + check）も下がるか。
> 受理条件: apply ~150ms・check は前以下・トータルが下がる・`calls` 不変・
> `verify-broken` の rc=2 維持・462 test green。
> 棄却条件: (a) check が同じだけ増える（コストが移るだけ）(b) 検出性が壊れる
> (c) `settled` の意味が壊れる。

### 測り方 1 — 隔離の再確認（基準値）

`-f verify -f verify-broken -f verify-blind -r 3 -v --log`（17:33 のログ）:
`apply` 1179ms / `check` 87ms（apply-check）、`apply` 1211ms / `cargo check` 133ms
（apply-cargo）、`verify-broken` apply 893ms / check 92ms（rc=2・345B）。
§4 の前提どおり apply step と check step は分離している。

### 測り方 2 — A/B(a): 診断 pull だけ外す（hint pull は残す）

| 観測 | apply（apply-check） | apply（apply-broken） | check | daemon テスト |
|---|---|---|---|---|
| baseline | 1179ms | 893ms | 87ms | 462 green |
| A/B(a) 1 回目（18:08） | **719ms** | 511ms | 87ms | — |
| A/B(a) 再測（18:15） | **1155ms** | 782ms | 86ms | **2 fail** |

同じバイナリで 719ms と 1155ms に割れた。原因は **Open 時の背景 settle**
（`Open` は `tokio::spawn` で `settle_open_diagnostics` を投げる = Open 応答を
ブロックしない）と、編集後の hint pull が**同じ解析を奪い合う**ためで、
「どちらの要求が先に解析を起こしたか」で wall が変わる。**再測を採用**（1 回目は
outlier。両方ログに残した）。

再測の結論: **診断 pull を外しても apply は変わらない**（baseline と同値）。
hint pull が同じ RA 解析を買っているため、`pull_diagnostics` の分は相乗りで消える。
さらに daemon テストが 2 件 fail（`get_inlay_hints_serves_arbitrary_path_and_restores_focus`
の「復元後の編集が同期される」= **編集後の診断追従**、`reopen_rs_path_respawns_lsp_and_reannounces_current_text`）。

### 測り方 3 — A/B(b): 診断 pull と hint pull の両方を外す（apply は診断を返さない）

| flow/arm | baseline wall | A/B(b) wall | 差 |
|---|---|---|---|
| `verify/apply-check` | 1228ms（1179+87） | 1275ms（**153**+1122） | **±0**（コストが check へ移動） |
| `verify/apply-cargo` | 1344ms | **268ms** | −80% |
| `verify/hunks-cargo` | 1450ms | **275ms** | −81% |
| `verify/apply2-cargo` | 1524ms | **361ms** | −76% |
| `verify-broken/check` | 984ms | **608ms** | −38% |
| `verify-broken/cargo` | 995ms | **220ms** | −78% |
| `verify-blind/check` | 940ms | **841ms** | −11% |
| `verify-blind/cargo` | 903ms | **281ms** | −69% |

- `apply` は 153ms（想定どおり ~150ms）。`calls` / `out_B` / `equiv_B` / `fails` は不変、
  `verify-broken/check` は rc=2・345B のまま、`verify-blind` は `settled:false` 維持
  → **L0 の契約指標では回帰なし**。
- しかし **daemon テスト 3 件 fail**: `inlay_hints_follow_edits_and_snapshot`
  （編集後のヒント追従）、`get_inlay_hints_serves_arbitrary_path_and_restores_focus`
  （編集後の診断追従）、`reopen_rs_path_respawns_lsp_and_reannounces_current_text`。
  = **「編集後のスナップショットは新しい診断・ヒントを反映する」という daemon の契約**が
  テストで固定されている。`apply` が pull しないと、daemon は編集前の診断を表示し続ける。

### 考察

1. **1.1s の正体は「解析」であって「待ち」ではない**（#4/#5/#6 と同じ結論の再確認）。
   診断 pull を外しても hint pull が同じ解析を買うので apply は変わらない（A/B(a)）。
   両方外せば 1.1s は消えるが、その解析は **次に pull した者（`check`）が必ず払う**
   （A/B(b) の apply-check は ±0）。§4 の棄却条件 (a) そのもの。
2. **「apply は診断を返さない」は契約として成立しない**（棄却条件 (b) の変種）。
   エージェント向けの `apply` 出力自体には診断は元から含まれていない（`applied: …` の
   1 行だけ）。壊れるのは **daemon のスナップショット（TUI の下線・件数の出所）の
   編集後追従**で、これはテストで固定された契約（表示の正しさ）。ADR-0045 の
   `settled` とは別問題（あちらは「空をクリーンの根拠にしない」）。
3. **ただし大きな発見が 1 つ**: pull を外すと **cargo 検証経路と複数編集経路が
   −70〜80%** になる（apply-cargo 1344→268ms、apply2-cargo 1524→361ms）。
   `apply` が先払いする RA 解析は、**cargo で検証するフローでは二重払いだった**
   （RA の解析と rustc のコンパイルは別物。エージェントが受け取る `apply` の出力は
   どちらでも `applied: …` の 1 行）。編集を N 回打つフロー（`apply2`・`rename/apply`）
   では N 倍に効く。
4. **残っている唯一のレバーは「ブロックしないこと」**: 契約（編集後追従）は
   "pull が起きること" を要求するが、**Save 応答の前に起きること**は要求していない
   （テストは snapshot を poll で最大 10 秒待つ）。`sync_after_edit` の pull を
   背景タスクに移せば、A/B(b) の apply 153ms を取りつつ 3 テストは通る見込み。
   エージェントの実フローでは次のターン（LLM 推論）が数秒あるので、その間に解析が
   終わり `check` は 87ms のまま = **1 編集あたり ~1s がエージェントの待ち時間から消える**。
   → #8 の課題。
5. **計測の落とし穴（新規・重要）**: `apply` の wall は Open 時背景 settle と編集後 pull の
   競合で **700ms / 1100ms に割れる**（同一バイナリで 807ms と 1241ms を観測）。
   apply の wall を 1 回の観測で結論しない（`-r 3` 必須）。A/B は同一セッションで
   交互に測るのが望ましい（#7 の A/B はセッションをまたいだが、baseline 17:33 /
   A(a) 18:15 / A(b) 18:19 と連続で、A(b) の apply 153ms は 3 run とも 130〜153ms なので
   結論は揺れていない）。

### 結論（iteration #7）

- **棄却**: 「`apply` の診断 pull を外せば 1.1s が消える」は成立しない。
  (a) コストは次に pull した者へ移る（apply-check は ±0）、(b) 編集後の診断追従という
  daemon の契約が壊れる（テスト 3 件 fail）。`settled` 契約（ADR-0045）は不変。
- **採用**（収穫）: 計測上の事実として「apply の ~1.1s = pull が強制する RA 解析」を
  A/B で分離して確定し、**cargo 検証経路ではこの先払いが二重払い**であることを示した
  （−70〜80%）。ただし契約を壊さずに取り出すには背景化（#8）が要る。
- 効果確認: baseline 復帰後に `cargo test` **462 passed**、L0 fails 0。
- 参照: `docs/adr/0053`（追記。撤去できない理由 = 契約と解析）。

## 2026-09-12 19:58 — 基準値: iteration #8 前

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354    1222.9         0  True
verify   apply-cargo           2       108         1       108       216    1332.3         0  True
verify   hunks-cargo           2       154         1       154       308    1351.7         0  True
verify   apply2-cargo          3       218         2       327       545    1451.2         0  True
verify-broken check                 2       450         2       105       555     856.3         0  True
verify-broken cargo                 2       105         1       105       210     919.6         0  True
verify-blind check                 2       240         2       104       344     832.0         0  True
verify-blind cargo                 2       104         1       104       208     956.0         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-12 20:12 — iteration #8: sync_after_edit の pull を背景化（ADR-0055）。仮説: apply ~150ms、後続 check ~90ms（和が現行比 -70% 以上）。既存 verify は即時 check が背景プル完了を待ち ~1.2s のまま（非悪化）

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354     585.2         0  True
verify   apply-cargo           2       108         1       108       216     250.2         0  True
verify   hunks-cargo           2       154         1       154       308     256.4         0  True
verify   apply2-cargo          3       218         2       327       545     342.8         0  True
verify-gap apply-gap-check         3       262         2       232       494    3238.5         0  True
verify-gap apply-gap-cargo         3       116         1       232       348    3271.6         0  True
verify-broken check                 2       450         2       105       555     483.3         0  True
verify-broken cargo                 2       105         1       105       210     217.4         0  True
verify-blind check                 2       240         2       104       344     606.0         0  True
verify-blind cargo                 2       104         1       104       208     266.3         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-gap/apply-gap-check` daemon 計測: check_bytes=187, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-12 20:14 — iteration #8 cold: 背景化後の cold 税確認（apply の cold 120-140ms が悪化しないか）

cold 測定（warmup なし・1 回観測）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354    8689.9         0  True
verify   apply-cargo           2       108         1       108       216     425.0         0  True
verify   hunks-cargo           2       154         1       154       308     428.9         0  True
verify   apply2-cargo          3       218         2       327       545     509.5         0  True
verify-gap apply-gap-check         3       262         2       232       494    8747.0         0  True
verify-gap apply-gap-cargo         3       116         1       232       348    3307.4         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-gap/apply-gap-check` daemon 計測: check_bytes=186, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-12 20:27 — iteration #8 回帰: 空 pull 回帰（編集後 pull が背景化で失われないか）-r 10

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify-broken check                 2       450         2       105       555     617.8         0  True
verify-broken cargo                 2       105         1       105       210     215.0         0  True
verify-blind check                 2       240         2       104       344     606.7         0  True
verify-blind cargo                 2       104         1       104       208     274.7         0  True
verify   apply-check           2       246         2       108       354    1023.5         0  True
verify   apply-cargo           2       108         1       108       216     255.5         0  True
verify   hunks-cargo           2       154         1       154       308     260.7         0  True
verify   apply2-cargo          3       218         2       327       545     351.1         0  True
```

- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2


## iteration #8 考察（2026-09-12）— 背景化の採用

**仮説の検証結果**: 採用。`sync_after_edit` の pull を背景タスク化（ADR-0055）。

**なぜ想定どおりになったか**: 応答を待たせない形にしたので、apply の wall から
「pull が強制する RA 解析」が消えた（130ms = 編集処理本体のみ。drain_into +
snapshot は解析を買わない）。エージェントの次ターン（sleep 3 で代理）の間に
背景 pull が解析を終え、後続 check は 92ms（温まった解析）。cargo 経路は
#7 A/B(b) で見た「pull なし = −70〜80%」と同水準（apply-cargo −82%）。

**§4 の前提誤り（本反復の最大の収穫）**: §4 は「テスト 3 件は snapshot を poll
で待つ」と読んでいたが、実際は**編集コマンドの応答**で編集後の診断・ヒントを
検証していた（`request()` は応答を返し途中の push は読み飛ばす）。背景化すると
3 件とも落ちた。テストの意図（編集後に追従すること）は push/GetState でも満た
されるため、検証ポイントを応答 → poll に移して 462 green を維持した
（ADR-0055 に詳述）。「契約が要求するのは pull が起きることだけで、応答前に
起きることではない」という §4 の解釈は、テストの実装（応答検証）と食い違い、
**実装を経て確定できた**（= テストを実装に合わせたのではなく、意図を損なわず
検証時点を正した）。

**ギャップなし（即時 check）の帰結**: apply-check の和は 1228 → ~1020ms
（apply ~130ms + check ~800ms）。didChange 直後の初回再解析を check 自身が買う
ため。悪化はしない（受理条件 3 を満たす）が、「解析は誰かが買う」原則は
ギャップなしでも不変。**エージェントの実フロー（apply 応答 → LLM 推論 → check）
では背景 pull が推論中に解析を終えるので、check は 92ms で済む** — ここが
ギャップあり flow を追加した理由であり、最大の改善点（エージェント待ち
1.1s → 130ms、−88%）。

**前回課題（#7 収穫）との関係**: 「cargo 検証経路では先払いが二重払い」は、
背景化により pull が応答を待たせなくなったので、apply-cargo 1344 → 240ms で
解消。pull 自体は背景で走るので、check 経路の契約（settled:false・rc=2 検出）も
維持。

**残る課題（次反復の候補）**:
- ギャップなし check の ~800ms（= didChange 直後の解析を check が買う）は、
  「apply 直後に check を打つエージェント」ではまだ払われる。背景 pull の
  完了を check が待つ形にすると 1 往復増える。優先度は低い（実フローでは
  LLM 推論が間に合う）が、L2 で実フローの分布を確認する価値はある。
- 「apply の直前までに解析が温まっていたか」（#5 の下調べ）は背景化後も有効:
  既に解析済みなら background の pull も check も速い。L2 向き。
- 計時ログ（#7 の別候補）: apply の 10 回中央値は ~1.0s で安定（700/1100ms の
  割れは解消。背景化で Open settle との競合が apply 応答から消えた）— ただし
  check 側に同種の分散が出た（背景 pull との競合）。計時ログが無いと説明は
  推測になる。

**確定した事実（再測定は不要）**:
- 背景化後、apply の wall は 800〜1200ms の割れから **~130ms で安定**。
- ギャップありの apply + check の和は **222ms（現行 1228ms 比 −82%）**。
- 編集後追従の 3 テストは、追従の検証を poll で行う形に変更した
  （検証時点のみ。契約の意味は不変）。

## 2026-09-12 21:59 — iteration #9: MINAD_TRACE=1（計時 on・1 run ずつ）。内訳（minad.log）と契約の回帰確認（calls/equiv_B 不変・settled 維持）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1017        17       551      1568     269.5         0  True
explore  dump                  2      4750       298      4510      9260     155.9         0  True
hints    hints                 1       110         8         0       110     669.5         0  True
rename   lsp                   2       219         3       219       438    1145.6         0  True
rename   apply                 5       406         4      1018      1424     599.6         0  True
verify   apply-check           2       246         2       108       354     963.2         0  True
verify   apply-cargo           2       108         1       108       216     227.8         0  True
verify   hunks-cargo           2       154         1       154       308     248.9         0  True
verify   apply2-cargo          3       218         2       327       545     353.5         0  True
verify-blind check                 2       240         2       104       344     602.8         0  True
verify-blind cargo                 2       104         1       104       208     539.1         0  True
verify-broken check                 2       450         2       105       555     566.2         0  True
verify-broken cargo                 2       105         1       105       210     213.6         0  True
verify-gap apply-gap-check         3       262         2       232       494    3232.3         0  True
verify-gap apply-gap-cargo         3       116         1       232       348    3270.2         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4942, read_total=1, symbol_range_bytes=249, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5340, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-check` daemon 計測: check_bytes=187, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-12 22:08 — iteration #9: 計時ログ追加後の基準値（trace off・verify 4 arm・warm r=3）。次の trace on と同条件で wall を比較する

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354     775.6         0  True
verify   apply-cargo           2       108         1       108       216     597.8         0  True
verify   hunks-cargo           2       154         1       154       308     352.9         0  True
verify   apply2-cargo          3       218         2       327       545     337.0         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2


## 2026-09-12 22:16 — iteration #9: trace on（MINAD_TRACE=1・verify 4 arm・warm r=3）。直前の trace off と同条件で wall を比較（計時のオーバーヘッドが 10% 未満であること）

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354     885.1         0  True
verify   apply-cargo           2       108         1       108       216     275.4         0  True
verify   hunks-cargo           2       154         1       154       308     276.5         0  True
verify   apply2-cargo          3       218         2       327       545     359.1         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2


## 2026-09-12 22:25 — iteration #9: trace off 反復（verify 4 arm・warm r=6）。trace on との wall 比較を n 倍にして揺れを絞る

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354     894.5         0  True
verify   apply-cargo           2       108         1       108       216     272.8         0  True
verify   hunks-cargo           2       154         1       154       308     285.4         0  True
verify   apply2-cargo          3       218         2       327       545     594.0         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2


## 2026-09-12 22:31 — iteration #9: trace on 反復（MINAD_TRACE=1・verify 4 arm・warm r=6）。trace off（r=6）との wall 比較

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       246         2       108       354     846.0         0  True
verify   apply-cargo           2       108         1       108       216     276.1         0  True
verify   hunks-cargo           2       154         1       154       308     410.3         0  True
verify   apply2-cargo          3       218         2       327       545     745.1         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2


## iteration #9 考察（2026-09-12）— 計時ログ（MINAD_TRACE）の採用と、全コマンド ~65ms 固定費の発見

**仮説の検証結果**: **採用**（ADR-0056）。`minad` に `src/trace.rs`（120 行）+
各経路の mark を入れ、`MINAD_TRACE=1` のときだけ stderr に
`minad.trace <span> <phase> <ms>` を出す。wire・応答・PROTOCOL_VERSION は不変。
`--log` を付けた 5 回の L0（trace on r=1 全 flow / off r=3 / on r=3 / off r=6 /
on r=6）の生データは上の 5 ブロック。

**回帰**: 463 test green（462 + trace の形式テスト 1 件）。trace on の L0
（全 15 arm）で `calls`/`out_B`/`equiv_B` が #8 の §3 と**全 arm 一致**・
`fails` 0・`ok` True。`settled:false`（blind）/ rc=2・Syntax Error（broken）も
維持。計時の wall への影響は、minas の step だけで構成される apply-check で
trace off 893.7 → on 846.0ms（n=9 ずつの中央値、**−5.3%**）、apply-cargo で
−2.2%。`cargo check` を含む arm（hunks/apply2）は同一セッション内でも cargo の
wall が 124〜1555ms と振れ、~1ms/命令の計時コストを分解できないため比較から
除外した（計時は既定 off = `Instant::now()` 1 回と分岐のみ）。棄却条件 (a) には
該当しない。

### 内訳の実測（§4 の (i)(ii)(iii)）

**（i）ギャップなし check の ~800ms は「解析の二重買い」ではなくロック待ち**
（`verify/apply-check`、trace on、n=12 中央値）:

```
minad.trace edit total 2           ← DocumentEdit の処理（apply_edit 0 + 応答構築 1）
minad.trace sync.bg didChange 2
minad.trace sync.pull diag 892     ← RA の textDocument/diagnostic（唯一の解析）
minad.trace sync.pull hint 6
minad.trace sync.bg total 902
minad.trace check borrow 682       ← ensure の await_indexed が session.lock() を待つ
minad.trace check.pull round0 0    ← 自分の pull は 0ms（解析は買っていない）
minad.trace check.pull round1 2    ← 2 回連続同一の確認（settle）
minad.trace check total 686
```

背景 pull が先に完了していれば `check total` は **3ms**（実測例あり）。
ギャップあり（sleep 3）では `check total` **5ms**。つまり #8 の
「ギャップなしでは check が解析を買う」は**言い過ぎ**で、正しくは
「背景 pull が買った 1 回の解析を、check がセッションロック越しに待つ」。
待ちは `ensure` の `await_indexed` の `session.lock().await`（タイムアウト無し）
に現れる。check 自身が払うのは 2ms。**「解析は誰かが買う」原則は不変**で、
ギャップなしの場合の和 ~0.9s は同じ 1 回の解析の下限。

**（ii）apply の ~130ms はデーモンではなく minas クライアント**:
デーモン側は `edit total` 2ms + `write` ~1ms（n=12）。残りはクライアントで、
プローブ実測（`tmp/loop/probe-client/`、デバッグビルド）で分解できた:

| 測定 | 実測 |
|---|---|
| `minas --help`（起動 + clap） | 12.4ms |
| daemon の 1 往復（Python クライアント、GetServerInfo） | **0.27ms** |
| `minas info` / `minas read` | 82.8 / 84.1ms |
| `minas apply`（内訳つき一時計測） | Open 往復 59ms + DocumentEdit 往復 9ms + Save 往復 2ms → 計 91ms |
| probe 接続（接続→即 close）を挟んだ往復 | **67.2ms**（挟まない場合は 0.27ms） |

**原因（A/B で確定）**: (1) `minas` は「デーモンが生きているか」を確かめるために
**捨てる接続**を 1 本開いて即 close する（`open_one_shot` の `conn::connect`）、
(2) daemon の `accept_loop` は peer uid 検査を **accept ループ内**で行い、
`stream.peer_cred()` が ENOTCONN（相手が既に閉じた）のとき `5ms × 最大 10 回`
sleep する。その間 accept ループが止まるので、直後に来た本命接続は
カーネルの backlog に座ったまま**最初の往復が ~65ms 遅れる**。Python で
「単一接続 0.27ms / probe→本接続 67.2ms」を再現した。**すべての minas 呼び出しが
この固定費を払っており、L0 の全 flow の wall に含まれていた**（例:
`explore/lsp` 269.5ms = 3 呼び出し × 78ms + 実処理 ~25ms。`hints` 669.5ms の
うち 78ms、`rename/lsp` 1145.6ms のうち 78ms）。

**（iii）rename の ~1150ms の実体は RA の WorkspaceEdit 計算**（n=2 + 純計測）:

```
minad.trace rename prepare 2      ← didOpen
minad.trace rename resolve 12     ← 識別子位置の解決（全ファイル didOpen 済み）
minad.trace lsp.retry method=textDocument/rename
minad.trace lsp.retry attempt1 912   ← RA の計算（ここが全部）
minad.trace lsp.retry attempt2 2     ← 2 回目の安定確認は 2ms
minad.trace rename request 898
minad.trace rename convert 2 / apply 2
minad.trace rename total 918
```

`minas rename` の step wall 965ms = 起動 12 + **固定費 65** + デーモン 918 −
（重複計上分を除く）≈ 一致。ADR-0054 の「2 回連続同一の確認自体はほぼ無料」は
**2ms と直接確認**できた（案 B「確認を省く」を採る価値は無い）。
hints も同じ形: `hints total` 578ms = `pull` 578（RA の inlayHint 計算）。
キャッシュヒット時は ~0ms。

### 想定外の収穫と次の一手

計時ログを入れた目的（内訳を恒久的に測れるようにする）は達成し、その最初の
計測で**#10 の課題が「apply の内訳」ではなく「全コマンドの ~65ms 固定費」に
変わった**。これは L0 の全 flow に効く最大の残存項で、修正は 2 箇所の小変更:

- daemon: `accept_loop` の peer uid 検査（最大 50ms の sleep）を accept ループの
  外（＝spawn した接続タスク側）へ移す。**accept ループを sleep で止めない**。
  これは minas に限らず、即 close する接続（死活監視・TUI 再接続）すべてに
  効く共有経路の修正。
- minas: 死活確認のための捨て接続をやめ、`conn::connect` の結果をそのまま
  セッションに使う（接続 1 本/コマンドに減る）。

期待効果（L0 で測れる）: `explore/lsp` −~195ms、`verify/apply-check` −~130ms、
`apply2-cargo` −~195ms、`rename/lsp` −~65ms、`hints` −~65ms。`calls`/`out_B`/
`equiv_B` は不変（契約は触らない）。

**保留した項目**: `ServerMetrics` の穴（rename カウンタ・`get_state_bytes`）。
`ServerInfo` 応答の wire 変更なので ADR-0039 では version bump が要る（v18 の
`read_total`/`read_bytes` 追加が前例）。§4 の制約「PROTOCOL_VERSION 不変」と
衝突するため、次に wire を変える用事と束ねる（ADR-0056 に記載）。

**確定した事実（再測定は不要）**:
- `MINAD_TRACE=1` で 1 コマンドの内訳（span/phase/ms）が取れる（既定 off・
  wire 不変・463 test green）。method.md §7 の「内訳は A/B 除去でしか測れない」
  は解消。
- ギャップなし check の ~700ms は **`ensure` の `await_indexed` が
  背景 pull のセッションロックを待つ**時間。check 自身の pull は 2ms。
- すべての `minas` 呼び出しに **~65ms の固定費**（捨て接続 + accept ループ内
  peer uid 検査の sleep）。デーモンの応答は 0.27ms（Python 実測）。
- rename の 2 回目（安定確認）は **2ms**。hints の 578ms は RA の inlayHint 計算。
- `sync.bg` の `didChange` が 300–650ms になるのは前の背景 pull のロック待ち
  （応答はブロックしない — ADR-0055 の設計どおり）。

## 2026-09-13 22:42 — iteration #10 baseline: 固定費除去前（a/b とも未適用）

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       275         2       108       383    1222.8         0  True
verify   apply-cargo           2       108         1       108       216    1268.4         0  True
verify   hunks-cargo           2       169         1       169       338    1315.7         0  True
verify   apply2-cargo          3       218         2       327       545     917.9         0  True
explore  lsp                   3      1085        18       587      1672     299.0         0  True
explore  dump                  2      4750       298      4510      9260     163.5         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `explore/lsp` daemon 計測: read_bytes=4974, read_total=1, symbol_range_bytes=281, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5403, read_total=2


## 2026-09-13 23:54 — iteration #10: 固定費（~65ms/呼び出し）除去後（a: daemon の peer uid 検査を accept ループ外へ + b: minas の捨て接続をやめて接続を再利用）。l0.py の step が PATH の minas を使っていたバグも修正済み

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1082        18       584      1666      88.5         0  True
explore  dump                  2      4750       298      4510      9260      29.1         0  True
hints    hints                 1       110         8         0       110     640.0         0  True
rename   lsp                   2       307         3       307       614    1348.6         0  True
rename   apply                 5       402         4      1008      1410    1285.5         0  True
verify   apply-check           2       273         2       107       380    1056.9         0  True
verify   apply-cargo           2       107         1       107       214    1237.8         0  True
verify   hunks-cargo           2       168         1       168       336    1259.8         0  True
verify   apply2-cargo          3       216         2       324       540    1242.0         0  True
verify-blind check                 2       267         2       103       370     649.9         0  True
verify-blind cargo                 2       103         1       103       206     811.2         0  True
verify-broken check                 2       471         2       104       575     645.9         0  True
verify-broken cargo                 2       104         1       104       208     753.4         0  True
verify-gap apply-gap-check         3       289         2       230       519    4022.0         0  True
verify-gap apply-gap-cargo         3       115         1       230       345    3758.0         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4973, read_total=1, symbol_range_bytes=280, symbol_range_total=1, symbol_search_bytes=199, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5401, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=177, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=175, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=384, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-check` daemon 計測: check_bytes=186, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-14 00:03 — iteration #10 回帰確認: -r 10（fails 0・必須文字列の取りこぼし 0・calls/out_B 不変の確認）

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
verify   apply-check           2       273         2       107       380    1042.8         0  True
verify   apply-cargo           2       107         1       107       214    1170.0         0  True
verify   hunks-cargo           2       168         1       168       336    1167.3         0  True
verify   apply2-cargo          3       216         2       324       540    1194.4         0  True
explore  lsp                   3      1082        18       584      1666      82.6         0  True
explore  dump                  2      4750       298      4510      9260      28.5         0  True
```

- `verify/apply-check` daemon 計測: check_bytes=177, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `explore/lsp` daemon 計測: read_bytes=4973, read_total=1, symbol_range_bytes=280, symbol_range_total=1, symbol_search_bytes=199, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5401, read_total=2


## 2026-09-14 00:04 — iteration #10 cold 確認: 索引完走ゲート支配（無言の誤り 0・cold 税は接続経路の修正と独立）

cold 測定（warmup なし・1 回観測）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1085        18       587      1672    7397.3         0  True
explore  dump                  2      4750       298      4510      9260      31.6         0  True
hints    hints                 1       110         8         0       110    7878.7         0  True
rename   lsp                   2       309         3       309       618    9202.8         0  True
rename   apply                 5       406         4      1018      1424     444.6         0  True
verify   apply-check           2       275         2       108       383    8576.3         0  True
verify   apply-cargo           2       108         1       108       216     329.8         0  True
verify   hunks-cargo           2       169         1       169       338     326.5         0  True
verify   apply2-cargo          3       218         2       327       545     356.3         0  True
verify-blind check                 2       269         2       104       373    6444.1         0  True
verify-blind cargo                 2       104         1       104       208     354.3         0  True
verify-broken check                 2       271         2       105       376    6312.1         1  True
verify-broken cargo                 2       105         1       105       210     287.4         0  True
verify-gap apply-gap-check         3       291         2       232       523    8106.9         0  True
verify-gap apply-gap-cargo         3       116         1       232       348    3220.2         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4974, read_total=1, symbol_range_bytes=281, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5403, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=177, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` 失敗 step (silent): `/Users/335g/dev/other/mina/target/debug/minas check src/main.rs` rc=0 
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-check` daemon 計測: check_bytes=186, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## iteration #10 考察（2026-09-14）— 接続経路の ~68ms 固定費を除去（ADR-0071）

**仮説の検証結果**: **採用**（ADR-0071）。仮説 (a)「peer uid 検査を accept ループの外へ」と
(b)「minas の捨て接続をやめて接続を再利用」の**両方**を入れた。測定上はどちらか片方で
固定費は消える（下の A/B）が、(a) は他のクライアント（古い minas・TUI・将来の接続）の
connect→close でも accept が止まらない**位置の修正**、(b) は 1 コマンド 1 接続という
契約の整理。`calls`/`out_B`/`equiv_B` は不変（契約は触っていない）。

### 測り方 1（原因の再確認 — Python プローブ、`tmp/loop/probe-client/probe10.py`）

| 接続パターン | デーモン 修正前 | デーモン 修正後 |
|---|---|---|
| 単一接続（Hello + GetServerInfo） | min 0.34 / med 0.71 / max 6.78ms | 0.43 / 0.46 / 2.16ms |
| 捨て接続 → 本命接続 | 66.5 / **68.4** / 68.9ms | 0.39 / **0.52** / 0.93ms |
| 差 | **67.7ms** | **0.06ms** |

#9 の 0.27ms vs 67.2ms を再現（n=5）。原因の見立ては正しい。

### 測り方 2（A/B — 同一セッションで交互、`-f explore -r 3` × 2 ラウンド）

まず**計測器の穴**が見つかった: L0 の flow の step コマンドは `minas` と**名前で**書いて
あり、実行は **PATH の minas**（インストール済み 0.0.5）だった。`L0_MINAS` は
`server_metrics`（`{MINAS} info`）にしか効いておらず、**クライアント側の変更は L0 の
step で一度も測られていなかった**。`l0.py` の `run()` で `minas` を解決済みパスへ
置換するよう修正（`| minas` のパイプ形も同様）。修正後の A/B:

| arm | `explore/lsp`（3 calls） | `explore/dump`（2 calls） |
|---|---|---|
| baseline（両方 未修正） | 310.0 / 299.2ms | 163.5 / 168.0ms |
| (a) daemon のみ修正 | 82.0 / 82.3ms | 28.5 / 27.7ms |
| (b) minas のみ修正 | 81.6 / 82.8ms | 28.1 / 29.5ms |
| (c) 両方修正 | 84.8 / 83.6ms | 29.3 / 30.2ms |

固定費 **−228ms（3 calls ≈ 3×76ms）** / **−136ms（2 calls ≈ 2×68ms）**。
(a)(b)(c) が同等 = 同じ 1 個の固定費への冗長な対策。(b) が単独で効くのは
**接続を 1 本にするので accept ループを sleep させる引き金そのものが無くなる**ため、
(a) が単独で効くのは **捨て接続を accept してもループが止まらない**ため。
修正前の測定で (b) が効かなかったのは、step が PATH の minas（捨て接続あり）を
実行していたから（計測器の穴）。

### 測り方 3（契約の確認）

- `cargo test`: **485 passed**（0 failed。latest.md の「463」は古い。HEAD が進んで
  テストが 22 件増えていた）。
- L0 全 flow（`-r 3`・両修正後・`--log`）: **fails 0 / ok True**。
  `calls`/`out_B`/`equiv_B` は arm 間で一致（baseline 1082/1666 → 修正後 1082/1666）。
- `verify-blind/check`: `settled:false`（偽クリーンを主張しない）。
  `verify-broken/*`: `cargo check` rc=101（構文エラーを検出）。
- peer uid の fail closed は不変（検査は接続タスクの先頭で、不一致なら stream を
  drop）。`peer_uid_mismatch_is_rejected`（純関数）は green。uid 不一致の
  統合テストは uid を偽装できないため無し（既存も無し）。

### 測り方 4（cold と `-r 10`）

- `-r 10`（verify + explore）: **fails 0**、`out_B`/`equiv_B` は `-r 3` と同値
  （`explore/lsp` 1082/1666、`apply-check` 273/380）。
- `--cold`: 索引完走ゲート支配で不変（`explore/lsp` 7397ms、`hints` 7879ms、
  `rename/lsp` 9203ms、`apply-check` 8576ms — #2〜#9 の cold 値と同水準）。
  固定費は cold でも同じだけ効くが、8〜9 秒の中に埋もれる。

### 効果確認

- `minas info`（baseline デーモン相手）: **76.6ms → 9.1ms**（PATH の 0.0.5 も 78.5ms
  = 同じ旧経路）。
- L0 `explore/lsp`: **310 → 82ms（−74%）**、`dump`: **164 → 28ms（−83%）**。
- §3 の旧参照値（PATH minas 0.0.5 で測定）とは out_B もずれる（例 `explore/lsp`
  1017 → 1082 バイト）。**§3 の表を新参照値に置き換えた**（latest.md §3）。

### 落とし穴（今回踏んだもの）

- **手動テストの daemon が残ると wall が +60ms 汚染される**: `minas` の自動起動は
  `setsid` で daemon を切り離すため、スクリプト終了後も rust-analyzer ごと生き残る
  （`(… &)` で起動したものも同様）。実際に 3 個の stray daemon が残り、`minas symbol`
  が 12ms のはずが 90ms に見えた。**測る前に `ps aux | grep -E "minad serve|rust-analyzer" | wc -l` が 0 であることを確認する**
  （macOS の `pgrep` に `-c` は無く、エラーを握り潰すと「0 個」に見えるので注意 —
  今回これで 1 回空振りした）。
- **L0 の step は PATH の minas だった**（上記）。`--log` の過去の数値もこの経路で
  測られているため、クライアント側のコードを触った反復では参照値の意味が変わる。

### 発見（この反復の回帰ではない。iteration #11 の候補）

- **cold の `verify-broken/check` が rc=0 の `clean-unverified` を返す**: 構文エラーを
  入れた直後（warmup なし）に `minas check src/main.rs` を打つと、~6.3 秒待って
  `{"diagnostics":[],"settled":false,"verdict":"clean-unverified"}` と rc=0 を返す
  （期待は rc=2 + Syntax Error）。**修正前後のバイナリで同一**（4/4 再現）なので
  #10 の回帰ではないが、§3 の cold 表（`verify-broken/check` 8793ms・fails 0）とは
  食い違う = #9 の後に HEAD へ入った変更で cold のゲートが「未確認を返す」側に
  倒れた。契約上は「未確認」を明示しているので嘘ではないが、cold では構文エラーすら
  確定できない（索引ゲートの予算切れ）。warm の `verify-broken/check` は rc=2 で
  通る（fails 0）。

### iteration #11 の課題設定（→ latest.md §4）

1. **cold の `check` が `clean-unverified` に落ちる**（上記）。cold でも構文エラーを
   確定できるようにする（ゲートの予算・pull の再試行・`settled` の意味の見直し）。
   L0 の `--cold -f verify-broken` が計器。受理は「cold で rc=2 + Syntax Error」、
   棄却は「warm の値が悪化する」「cold が伸びる（索引ゲートの意味が無くなる）」。
2. 効果確認の結果、次の固定費の候補: `minas` 起動 12ms × calls（`explore/lsp` 82ms の
   うち ~36ms）、`apply` の 3 往復、`ServerMetrics` の穴（v20 bump と束ねる）、
   L2（実 LLM）での確認。

## 2026-09-14 20:57 — iteration #11 仮説: cold の check が構文エラーを確定できないのは、Open の背景タスクが ensure（cold では数秒）を挟んで spawn 時のテキスト（編集前）を didOpen/didChange で送り直し、サーバ側の文書が巻き戻るため。送る直前に現在のテキストを取り直せば、cold でも pull が構文エラーを返す（rc=2・fails 0）はず。warm/探索 flow は不変のはず

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1085        18       587      1672      80.0         0  True
explore  dump                  2      4750       298      4510      9260      28.0         0  True
hints    hints                 1       110         8         0       110     684.5         0  True
rename   lsp                   2       309         3       309       618    1397.5         0  True
rename   apply                 5       406         4      1018      1424    1378.9         0  True
verify   apply-check           2       275         2       108       383    1140.9         0  True
verify   apply-cargo           2       108         1       108       216    1250.1         0  True
verify   hunks-cargo           2       169         1       169       338    1264.0         0  True
verify   apply2-cargo          3       218         2       327       545    1279.5         0  True
verify-blind check                 2       269         2       104       373     726.8         0  True
verify-blind cargo                 2       104         1       104       208     842.5         0  True
verify-broken check                 2       473         2       105       578     746.9         0  True
verify-broken cargo                 2       105         1       105       210     815.4         0  True
verify-gap apply-gap-check         3       291         2       232       523    4042.4         0  True
verify-gap apply-gap-cargo         3       116         1       232       348    4267.2         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4974, read_total=1, symbol_range_bytes=281, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5403, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-check` daemon 計測: check_bytes=187, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-14 20:58 — iteration #11 効果確認（cold 全 flow）: 修正後は cold の check が構文エラーを確定できる（verify-broken/check が fails 0・rc=2）。索引完走ゲートに依存する探索 flow（explore/lsp・hints・rename/lsp）の wall は悪化しないはず（±10% 以内）

cold 測定（warmup なし・1 回観測）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1085        18       587      1672    7742.7         0  True
explore  dump                  2      4750       298      4510      9260      30.5         0  True
hints    hints                 1       110         8         0       110    8207.3         0  True
rename   lsp                   2       309         3       309       618    8907.0         0  True
rename   apply                 5       406         4      1018      1424     423.5         0  True
verify   apply-check           2       275         2       108       383    8588.6         0  True
verify   apply-cargo           2       108         1       108       216     320.2         0  True
verify   hunks-cargo           2       169         1       169       338     325.1         0  True
verify   apply2-cargo          3       218         2       327       545     340.4         0  True
verify-blind check                 2       269         2       104       373    6185.7         0  True
verify-blind cargo                 2       104         1       104       208     342.8         0  True
verify-broken check                 2       473         2       105       578    6111.3         0  True
verify-broken cargo                 2       105         1       105       210     259.2         0  True
verify-gap apply-gap-check         3       291         2       232       523    8627.4         0  True
verify-gap apply-gap-cargo         3       116         1       232       348    3200.1         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4974, read_total=1, symbol_range_bytes=281, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5403, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-check` daemon 計測: check_bytes=186, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


---

## iteration #11 考察（2026-09-14）— cold の `check` が構文エラーを取りこぼす原因は「背景タスクが編集前の全文を送り直す」こと（ADR-0072）

**仮説の検証結果**: **採用（修正の場所は §4 の仮説とは別）**。§4 の (a)「索引完走ゲートの
判定をファイル単位の解析完了観測へ変える」と (b)「cold で空の早期確定を無効化する」は
**棄却** — どちらも「空を確定と見なすまでの待ち方」の話で、実測は
**サーバが編集前のテキストを持っていた**ことを示した。修正は (c) の変種
「LSP へ送る直前に現在のテキストを取り直す」（ADR-0072）。

### 測り方 1（プローブで事実確認 — cold の pull は索引完走後に非空になる）

`tmp/loop/probe_cold_pull.py`（RA を起動直後に didOpen + 構文エラーを入れ、pull を
50ms 間隔で回す）:

| 時刻 | 観測 |
|---|---|
| ~6ms〜4.4s | pull は `{"items":[]}`（`resultId:"rust-analyzer"`）または `None`（索引中は応答が数秒ブロックする） |
| 4.7s | `Roots Scanned` end → `cachePriming`(Indexing) begin/end |
| **4.8s** | pull が初めて **Syntax Error 2 件**を返す |
| 同時刻 | `workspace/diagnostic/refresh`（冪等な再 pull 要求）、`publishDiagnostics` |

→ **cold の空は「索引中の正しい空」**で、索引完走直後に非空へ変わる。つまり
`check` の空確定が早すぎるのではなく、**確定の時点でサーバが古い文書を見ていた**
（＝待ちの問題ではない）。`docs/loop/probe_cold_pull.py` は `tmp/loop/` のスクラッチ
（gitignore）。

### 測り方 2（ゲートと pull の内訳 — `MINAD_TRACE=1`。足りずに一時 DBG を追加）

cold の `minas apply` → `check`（`MINAD_TRACE=1`、`tmp/loop/coldcheck.py`）:

```
borrow ensure 53        ← 完走ゲートは既に ready（Open の背景タスクが先に待っていた）
check.pull round0 0     ← 空
check.pull round1 6041  ← wait_ready 5345（静止 300ms で ready）+ pull（空）
check.pull round2 4     ← 空 → 空 2 連続で早期確定
check settled=false / diags=0 / total 6098
（直後の 2 回目の check: 17ms で rc=2 + Syntax Error 2 件）
```

trace だけでは「どの pull が何を返したか」が見えないので、**一時的な DBG 計測**を
`lsp.rs` / `mina-lsp` に足して（実装後に撤去済み。`git diff` は clean）送受信を並べた:

```
5173 ->RA didChange main.rs v=3 len=240   ← 編集前の全文（他はすべて len=239）
5173 pull go → 5852 空（678ms）
5854 pull go → 5856 空（2ms）              → 空 2 連続で早期確定
5872 (2 回目の check) didChange v=5 len=239 → pull 1.2ms で 2 件
```

**決定打**: 空を返した pull の直前に **`len=240`（= 編集前の `fn main() {`）の
didChange が届いていた**。送り主は `Command::Open` の背景タスク（`daemon.rs`）で、
spawn 時に `text` を clone し、`ensure`（cold では索引完走待ちで数秒）の**後**に
`lsp::open_document(&session, &path, &text_task, …)` を呼んでいた。didOpen 済みの
文書では `did_open_inner` が `did_change`（全文同期）に落ちるので、その数秒の間に
入った編集がサーバ側で巻き戻る。

### 測り方 3（修正と効果確認）

修正: `current_text_or(daemon, path, fallback)` を足し、Open の背景タスク 2 箇所は
`ensure` の後に現在のテキストを取り直してから `open_document` へ渡す。

| | before | after |
|---|---|---|
| `--cold -f verify-broken -a check`（本セッションの再現） | **6/6 が `clean-unverified`**（rc=0・~5.4s） | `-r 10` で **10/10 が rc=2 + Syntax Error**（fails 0） |
| 追加観測 | （#10 で 4/4） | `tmp/loop/coldcheck.py` 1 + `--cold` 全 flow 1 + `-r 5` 5 = 計 17/17 |
| cold `verify-broken/check` の wall | ~5.4s | ~5.8–6.2s（索引完走が支配。`out_B` 471→473 は fixture パス長の差） |
| cold 探索 flow（ゲート依存） | explore/lsp 7397・hints 7879・rename/lsp 9203ms | 7743（+4.7%）・8207（+4.2%）・8907ms（−3.2%）= ±10% 以内 |
| warm 全 flow | §3 の表 | `calls`/`out_B`/`equiv_B` 同一・fails 0 |

cold 全 flow（`--log` 済み）: **fails 0**（before は `verify-broken/check` が fails 1）。

### 測り方 4（契約と回帰テスト）

- `cargo test`: **486 passed**（485 + 追加 1）。`verify-blind/check` の `settled:false`
  維持、`verify-broken/cargo` の rc=101 維持、cold の無言の誤り 0。
- **回帰テストを追加**: `open_background_settle_sends_the_edited_text_not_the_snapshot`
  （`minad/src/daemon.rs`）。mock サーバに `MOCK_INIT_DELAY_MS`（env）を足し、
  `initialize` を 1.5 秒遅らせて `ensure` を長くする。その間に編集を入れ、
  「サーバが最終的に持つテキスト」を診断位置で確かめる。
  **修正を戻して実行すると失敗する**ことを確認（診断位置 **0** = 編集前 vs 期待 **4**）。

### 考察

- **「空 + 未確認」の判定（ADR-0045/0052）は正しかった**。誤りの実体は
  「サーバの文書が巻き戻っていた」ことで、`check` は巻き戻った文書に対して正しく
  空を返していた。ADR-0052 の実測（pull は 1 回で最終集合）も
  「**別の全文が入ってくる**」ケースは覆わない — 空を確定にしてよいのは
  「送ったテキストが最新である」ことが前提。
- **原因の形は ADR-0068 と同じクラス**（古い全文を送るとサーバの文書が巻き戻る）。
  ADR-0068 は「didOpen の再送」を直したが、今回は「背景タスクが掴んだスナップショットを
  await（ensure）をまたいで送る」経路が残っていた。**待つ処理をまたいで全文を持つと
  必ず古くなる** — 同じ規律を `settle_open_diagnostics_loop`（毎ラウンド読み直す）が
  既に守っていたのに、その 1 つ手前の didOpen 送信が守っていなかった。
- **cold でだけ出る理由**: ensure が数秒（索引完走待ち）で窓が広い。warm では窓が
  sub-ms なので症状が出ない（数反復測っても見えない）。「cold でしか出ない失敗は契約の
  欠陥」という method.md §5 の規則どおりだった。
- **測る順序が効いた**: §4 の順序（1. プローブで「索引完走後に非空」を確認 →
  2. trace で内訳 → 3. A/B）で、「待ちが足りない」という見かけの説明を先に潰せた。
  プローブで「索引完走直後に non-empty」を見ていなければ、settle 予算や早期確定の
  条件を弄る（効果の無い）修正に流れていた。

### 残る同類の窓（次以降の候補）

- 編集経路（`sync_after_edit` → `lsp::sync`）も spawn 時のテキストを送る。窓は
  `ensure` を挟まない sub-ms で、tokio Mutex の FIFO が順序を保つ（実測で症状なし）。
  同じ手（送る直前に取り直す）が使えるが、観測されるまで触らない。
- `pull_after_edit` は同じ `text` を診断の座標変換にも使うので、厳密には
  「送ったテキスト」と「変換に使うテキスト」を 1 つに揃えるのが次の形。

### iteration #12 の課題設定

→ `latest.md` §4（別候補から選ぶ: `minas` 起動 12ms × calls / `apply` の 3 往復 /
`ServerMetrics` の穴（v20 bump）/ L2 での確認）。

## 2026-09-14 21:44 — iteration #12: Save 応答の末尾の watched-files 通知（ADR-0061）を背景へ移せば、初回 apply の ~1.0s（背景 pull のセッションロック待ち）が消える（apply step が 1006-1123ms → 53ms）。calls/out_B/equiv_B 不変・fails 0・通知は遅れてでも届く（ADR-0073）。ギャップなしの apply+check の和は「同じ 1 回の解析を誰が待つか」なので不変（check がロックを待つ）— ギャップありは 1140 → 71ms

warm 測定（warmup あり・wall は中央値）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1085        18       587      1672      79.5         0  True
explore  dump                  2      4750       298      4510      9260      27.9         0  True
hints    hints                 1       110         8         0       110     647.9         0  True
rename   lsp                   2       309         3       309       618    1352.4         0  True
rename   apply                 5       406         4      1018      1424     267.6         0  True
verify   apply-check           2       275         2       108       383    1017.3         0  True
verify   apply-cargo           2       108         1       108       216     182.6         0  True
verify   hunks-cargo           2       169         1       169       338     191.5         0  True
verify   apply2-cargo          3       218         2       327       545     215.9         0  True
verify-blind check                 2       269         2       104       373     655.9         0  True
verify-blind cargo                 2       104         1       104       208     191.1         0  True
verify-broken check                 2       473         2       105       578     682.4         0  True
verify-broken cargo                 2       105         1       105       210     126.8         0  True
verify-gap apply-gap-check         3       291         2       232       523    3086.5         0  True
verify-gap apply-gap-cargo         3       116         1       232       348    3203.4         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4974, read_total=1, symbol_range_bytes=281, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5403, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-check` daemon 計測: check_bytes=187, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## 2026-09-14 21:45 — iteration #12 (cold): 修正後。apply を含む arm の apply step は cold でも速い（apply-check の wall は索引完走ゲート支配）。cold の無言の誤り 0・fails 0（#11 の契約は維持）

cold 測定（warmup なし・1 回観測）

```
flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1085        18       587      1672    7196.5         0  True
explore  dump                  2      4750       298      4510      9260      30.4         0  True
hints    hints                 1       110         8         0       110    8137.8         0  True
rename   lsp                   2       309         3       309       618    8879.2         0  True
rename   apply                 5       406         4      1018      1424     412.0         0  True
verify   apply-check           2       275         2       108       383    8671.6         0  True
verify   apply-cargo           2       108         1       108       216     313.0         0  True
verify   hunks-cargo           2       169         1       169       338     312.6         0  True
verify   apply2-cargo          3       218         2       327       545     336.1         0  True
verify-blind check                 2       269         2       104       373    6118.1         0  True
verify-blind cargo                 2       104         1       104       208     331.6         0  True
verify-broken check                 2       473         2       105       578    5754.1         0  True
verify-broken cargo                 2       105         1       105       210     222.6         0  True
verify-gap apply-gap-check         3       291         2       232       523    8607.3         0  True
verify-gap apply-gap-cargo         3       116         1       232       348    3203.2         0  True
```

- `explore/lsp` daemon 計測: read_bytes=4974, read_total=1, symbol_range_bytes=281, symbol_range_total=1, symbol_search_bytes=200, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5403, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=178, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
- `verify/apply2-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=2
- `verify-blind/check` daemon 計測: check_bytes=176, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-blind/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/check` daemon 計測: check_bytes=385, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-broken/cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-check` daemon 計測: check_bytes=186, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify-gap/apply-gap-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1


## iteration #12 — 修正: watched-files 通知を Save 応答の経路から外す（ADR-0073）

> 生データは上の 2 ブロック（warm r=3 / cold r=1、修正後）。ここには A/B と考察を書く。

### 課題（§4 の #12）

`minas apply` の初回だけ ~1.0s（2 回目以降 ~30ms）。待ちは daemon の編集（`edit total 2ms`）
でも解析そのものでもなく、**Save 応答の末尾にある watched-files 通知（ADR-0061）が
背景 pull（ADR-0055）のセッションロックを待っていること**。ADR-0061 は 09-13 16:58 追加
= #8 の後なので、#8 の「apply ~130ms」は「解析済みでロックが空いている」場合の値だった。

### 測り方 1（プローブで再現・確定）

`tmp/loop/probe_apply.py`（Python で同じプロトコルを直叩き、warmup 済み）:

| 往復 | before（2 回観測） | after（3 回観測） |
|---|---|---|
| `Open` | 36–50ms | 37–45ms |
| `DocumentEdit` | 3.4–4.3ms | 2.9–4.1ms |
| `Save` | **907 / 966ms** | **1.5 / 1.8 / 1.9ms** |

`MINAD_TRACE=1`（before）: `edit total 2` → 応答 `write` → 背景 `sync.bg`
（`didChange 2` / `sync.lock 986` / `sync.pull diag 979` / `sync.bg total 990`）→
**Save の応答 `write` が最後**。Save ハンドラの `notify_watched_files(...).await` が
`session.lock()` を待っている（待ち時間 = 背景 pull の長さ = Save の wall）。
after の trace には `sync.bg` が現れない（プローブが終了する方が背景 pull より速い）。

### 測り方 2（A/B・同一セッションで交互）

`L0_MINAD` / `L0_MINAS` で修正前バイナリを指して交互に測った（before→after→before）。
L0 の apply step（`-r 3` 中央値）:

| arm | step | before | after |
|---|---|---|---|
| `verify/apply-check` | `apply` | 1006 / 1010ms | **54ms** |
| | `check` | 17ms | 919ms（背景 pull のロック待ち） |
| | 和 | ~1029ms | ~1017ms（**不変**） |
| `verify/apply-cargo` | `apply` | 1040ms | ~53ms |
| `verify/hunks-cargo` | `apply` | 1009ms | ~53ms |
| `verify/apply2-cargo` | `apply`#1 / #2 | 1010 / 30ms | 53 / 30ms |
| `verify-gap/apply-gap-check` | `apply` | 1123ms | **53ms** |
| | 和（apply + check） | **1140ms** | **71ms（−94%）** |
| `verify-gap/apply-gap-cargo` | `apply` | 931–1056ms | 54ms |

arm 全体（warm r=3・上のログ）: `apply-cargo` 1202 → 183ms、`hunks-cargo`
1214 → 192ms、`apply2-cargo` 1192 → 216ms、`rename/apply`（apply ループ）1379 → 268ms。
`calls` / `out_B` / `equiv_B` は全 flow で不変、fails 0。`explore`（編集しない flow）は
79.5 / 27.9ms で不変。

### 測り方 3（cold・契約）

- cold 全 flow: **fails 0・無言の誤り 0**。wall は §3 の cold 表と ±10% 以内
  （`apply-check` 8589 → 8672、`explore/lsp` 7743 → 7197、`rename/lsp` 8907 → 8879ms）。
  cold の支配項は索引完走ゲートなので、この修正は cold では見えない。
- cold `verify-broken/check`（#11 の契約）は `-r 5` で fails 0（rc=2 + Syntax Error のまま）。

### 測り方 4（回帰テストと ADR-0061 の受け入れ）

- `cargo test --workspace`: **487 passed**（486 + 追加 1）。
- 追加: `save_does_not_wait_for_the_background_pull_before_notifying_watched_files`
  （`minad/src/daemon.rs`）。mock サーバに `MOCK_WATCH_LOG`（通知を受けたら記録）/
  `MOCK_DIAG_LOG`（pull の処理中を記録）/ `MOCK_DIAG_DELAY_MS` を足し、
  **「背景 pull がロックを握っている最中に Save を撃つ」**ことを決定的にしたうえで
  (1) Save が 100ms 未満で返る、(2) 通知は 50ms 以上遅れて届く（= 応答が通知を
  待っていない）、の両方を見る。**修正を戻すと失敗する**ことを確認。
- ADR-0061 の受け入れ試験を再実行（`tmp/loop/probe_watch_member.py`）: 稼働中の
  daemon に `minas apply` で新メンバー `crates/c` を作り、6 秒後に
  `minas symbol crates/a/src/lib.rs c_helper` が `crates/c/src/lib.rs` の `c_helper` を
  返す（= 通知は遅れても届き、RA は読み直す）。

### 考察

- **原因の形は #8 と同じクラス**: ADR-0055 が「pull を背景へ」で消した待ちを、
  ADR-0061 が**別の口（Save の末尾）から応答経路に戻していた**。ADR-0061 は #8 の
  5 日後（09-13）に追加されたので、#8 の L0 はこの回帰を測っていなかった
  （`apply2-cargo` の 1 回目 1061ms は §3 の表に載っていたが、「初回だけ」として
  説明されていなかった）。**同じ経路の待ちは、後から足した処理でも復活しうる**。
- **「誰が待つか」は指標の上で見えにくい**。`apply` を含む arm の wall は
  `verify/apply-check` では変わらない（~1017ms）。変わったのは内訳で、
  `apply` 54ms + `check` 919ms になった。エージェントにとっては「編集の応答が即返り、
  次の一手（別ファイルの編集・思考）が解析と重なる」ので、**ギャップありの和
  1140 → 71ms** が実効の改善になる。L0 は `sleep 3` のギャップ arm を持っていたので
  これを直接測れた。
- **ギャップなしの `apply` + `check` が不変なのは棄却理由にならない**（§4 の棄却条件 (c)
  は「二重払いの再来」を指す）。#9 の計時で `check total 686 = borrow 682 + pull 2` と
  確定済み = **解析は 1 回しか買われていない**。待ちの位置が変わっただけで、
  `check` の待ちを消すには「背景 pull と check を同じ 1 回の要求に合流させる」設計変更が
  要る（§4 の別候補に残す）。
- **通知を落とさないことが本質**（ADR-0061 の穴は「黙って部分的な答え」）。`try_lock` で
  即諦める案は棄却、背景タスク + 既存の `timeout(3s)` を維持した。ロックが取れない
  ときは通知タスクが FIFO で次の LSP 要求より前に並ぶので、実用上の順序も保たれる。
- **計測器の使い方が効いた**: ①プローブで往復ごとに割る → ②`MINAD_TRACE` で
  「Save の応答が `sync.bg` の後」を確認 → ③`L0_MINAD` で A/B、の順で
  「Save が背景 pull のロックを待っている」まで一度で確定できた（A/B の除去実験は不要）。

### iteration #13 の課題設定

→ `latest.md` §4（ギャップなし check の ~700ms（背景 pull と check の合流）/ `minas` 起動
12ms × calls（先に release で測る）/ `apply` の 3 往復 / `ServerMetrics` の穴（v20 bump）/
L2 での確認）。
