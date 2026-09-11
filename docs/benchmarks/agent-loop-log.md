# agent-loop-log — minas コスト低減ループの記録

課題設定（仮説）→ L0 検証 → 考察 → 修正 → 効果確認 の各反復を追記する。
方法と指標の定義: [`../../tools/loop/README.md`](../../tools/loop/README.md)。
L2（実LLM A/B）の結果は [`agent-editor-ab-results.md`](./agent-editor-ab-results.md)。

指標: `calls` 往復数 / `out_B` 総出力バイト / `equiv_B` 再送込みトークン等価量 /
`wall_ms` 実時間 / `fails` 失敗ステップ数（迷いの代理） / `ok` 目的達成。

---

## 2026-09-12 00:48 — 仮説: apply→check は「検証の最短経路」であり、cargo に委ねるより安い（calls/equiv_B/wall_ms すべてで優位）
```flow     arm               calls     out_B out_lines  resend_B   equiv_B   wall_ms     fails    ok
--------------------------------------------------------------------------------------------------
explore  lsp                   3      1014        17       548      1562    5232.9         0  True
explore  dump                  2      4750       298      4510      9260     157.7         0  True
rename   lsp                   2       217         3       217       434    6111.0         0  True
rename   apply                 5       402         4      1008      1410    6654.8         0  True
verify   apply-check           2       244         2       107       351   16729.0         0  True
verify   apply-cargo           2       107         1       107       214    4539.6         0  True
verify   hunks-cargo           2       153         1       153       306    4025.5         0  True```
- `explore/lsp` daemon 計測: read_bytes=4941, read_total=1, symbol_range_bytes=248, symbol_range_total=1, symbol_search_bytes=199, symbol_search_total=1
- `explore/dump` daemon 計測: read_bytes=5338, read_total=2
- `rename/apply` daemon 計測: edits_expected_text_used=4, edits_total=4, save_total=4
- `verify/apply-check` daemon 計測: check_bytes=177, check_total=1, edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/apply-cargo` daemon 計測: edits_expected_text_used=1, edits_total=1, save_total=1
- `verify/hunks-cargo` daemon 計測: edits_expected_text_used=2, edits_total=2, save_total=1
