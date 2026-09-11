# agent-loop-log — minas コスト低減ループの記録

課題設定（仮説）→ L0 検証 → 考察 → 修正 → 効果確認 の各反復を追記する。
方法と指標の定義: [`../../tools/loop/README.md`](../../tools/loop/README.md)。
L2（実LLM A/B）の結果は [`agent-editor-ab-results.md`](./agent-editor-ab-results.md)。

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
