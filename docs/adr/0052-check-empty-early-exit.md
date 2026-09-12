# check の空応答は早期に確定する（予算を使い切らない）

`minas check` は**空のときだけ**約 10 秒かかっていた（実測 warm r=3 中央値:
クリーンなファイルで **10154ms**、エラーのあるファイルで 594〜1054ms —
同じコマンドの同じ入力に対して 20 倍の差）。内訳は pull のリトライ予算 20 回 ×
500ms。ADR-0045 は「空 + 予算切れ =
クリーン**未確認**」として `settled: false` を返す設計で、判定は正しい — 問題は
**同じ答えしか返らない待ちに 10 秒払っている**こと。加えて Open 経路の
`settle_open_diagnostics_loop` は空のまま **30 秒**（60 ラウンド × 診断 + hint の
2 pull = 120 往復）回っていた（TUI では「診断取得中」が 30 秒残る）。

計測（2026-09-12、rust-analyzer を直叩き・索引完走後）:

| 壊し方 | pull の応答 | 待つと変わるか |
|---|---|---|
| main.rs の構文エラー | **1 回目（round0）で返る** | 変わらない（round0 == round11） |
| config.rs のフィールド削除 | 1 回目で `no such field` | 変わらない |
| main.rs のメソッド解決エラー（`cfg.validate2()`） | **永久に空**（`cargo check` は検出） | 変わらない |

つまり pull は**同じ入力に対して安定した答えを 1 回で返す**（レイテンシを持たない）。
空が非空へ変わるのを待つ根拠が無く、取りこぼすエラーは待っても取りこぼしたまま。

Status: accepted

## Decision

1. **`pull_diagnostics_settled` は連続 `SEMANTIC_EMPTY_ROUNDS`（= 2）回の空で
   打ち切る**。返すのは `(空, settled: false)` — ADR-0045 の意味は不変
   （空はクリーンの根拠にしない）。
2. **`settle_open_diagnostics_loop`（Open 経路）の空の打ち切りを 60 ラウンド
   （30 秒）から同じ `SEMANTIC_EMPTY_ROUNDS` に共有する**。「空はいつ確定か」は
   1 つの問いなので、答えを 2 箇所に別々に書かない。
3. 非空の判定（2 回連続で同数 = 安定 = `settled: true`）は**変えない**。
4. 2 回という安全代は、pull が解析と競合するサーバ（自分では解析を起こさず
   クライアントを待たせる実装）のため。打ち切っても `settled: false`（未確認）
   なので、このマージンが足りない場合の被害は「エージェントが `cargo` に落ちる
   1 往復」であって、誤ったクリーンではない。

## Considered Options

- **空 + 索引完走 = `settled: true`**: 却下 — pull は索引完走後でも実在する
  エラーを取りこぼす（上表 3 行目・ADR-0045 の追記）。偽クリーンを作る。
- **k を 1 にする**: 却下 — 測定上 1 回で十分だが、他サーバの競合に対する安全代が
  無くなる。2 回のコストは 500ms で、失うものに比べて安い。
- **予算を 20 → 3 に縮める**: 却下 — 「予算切れ」に意味を持たせ続ける限り、
  空の判定は遅いままで、非空でも 1 回だけ返るケース（不安定）の取り扱いが曖昧に
  なる。空と非空で出口を分ける方が契約が読める。
- **push 診断（`textDocument/publishDiagnostics`）を確定シグナルにする**:
  却下（今は）— 解析完了をサーバが push で教える設計は正しいが、ADR-0045 は pull を
  選んでおり、置き換えは別の反復の仕事。上の測定が「pull は 1 回で確定する」と
  示したので、今の契約のままで安くできる。

## Consequences

- `minas check` の空応答が **10154ms → 589ms**（L0 `verify/apply-check`、warm r=3
  中央値。−94%）。cold（索引待ちを含む）も 18375ms → 6415ms。
  エラーがあるときは 590ms 前後で従来どおり `rc=2` + 診断（早期確定はエラーを
  取りこぼさない — L0 `verify-broken`）。
- Open 経路の活動（「診断取得中」）が 30 秒（= `i >= 60` × 500ms）→ 実測 2.7 秒で
  消える。pull も 120 往復 → 約 6 往復（3 ラウンド × 診断 + hint）。
- `check` の `settled` の意味は不変（ADR-0045）。`check` が空を返すときは
  「未確認」であり、クリーンの根拠にはならない（`minas skill errors` の記述も
  そのまま有効）。
- pull が見ないエラー（メソッド解決など）は `check` では検出できない。これは
  rust-analyzer の pull の限界であり、エージェントの二段構え（check → cargo）が
  必要な理由。L0 `verify-blind` がその契約を回帰テストする。
- wire 形状は変えないため PROTOCOL_VERSION は据え置き。

## 関連

- 実装: `minad/src/lsp.rs`（`SEMANTIC_EMPTY_ROUNDS` / `pull_diagnostics_settled`）、
  `minad/src/daemon.rs`（`settle_open_diagnostics_loop`）
- 測定の記録: `docs/loop/log.md` iteration #3
- 判定の既存 ADR: ADR-0045（空応答をクリーンの根拠にしない）、ADR-0051（索引完走ゲート）
