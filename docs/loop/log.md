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

