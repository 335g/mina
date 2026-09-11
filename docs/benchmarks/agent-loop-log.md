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
