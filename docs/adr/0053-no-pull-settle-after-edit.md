# 編集後の診断 pull に固定 settle 待ちを置かない

`minad` は編集（`minas apply`）の後、診断を取り込むために
`textDocument/diagnostic`（pull）を打つ。その直前に **固定 250ms** 待っていた
（`PULL_SETTLE`）。根拠として書かれていたのは「didChange の直後は解析未完了で
pull が空を返すので待つ」というもの。

iteration #4 の内訳測定で、この待ちが**解析と重なっていない**ことが分かった
（`settle=251ms` と `diag_pull=604ms` が加法。待機中に解析は進まない = 解析は
pull 要求まで始まらない）。iteration #5 で 50ms・0ms の A/B を回した:

| `PULL_SETTLE` | `apply` 1 回目 | `apply` 2 回目（増分解析） | 空 pull 回帰 |
|---|---|---|---|
| 250ms（旧） | 1359ms | 346ms | なし |
| 50ms | 1132ms | 156ms | なし |
| **0ms（撤去）** | 1133ms | **107ms** | なし（`-r 10` × 3 flow = 30 run で fails 0） |

`apply` 1 回目が 50ms と 0ms で同値（1132 / 1133ms）なのは、撤去後に残る下限が
**RA の初回再解析 600–1100ms**（lazy なので診断要求の中で走る）だからで、settle 分は
もう乗っていない。

待たなくても診断が消えない根拠は 2 つ:

1. **順序**: `notify(didChange)` と `request(textDocument/diagnostic)` は同じ
   `LspSession` の `write` に同じ mutex 下で直列に書かれる（`mina-lsp` の
   `Client::notify` は `write_frame` の完了を await して返る）。pull が didChange を
   追い越すことはない。
2. **pull 自身が解析完了までブロックする**: rust-analyzer を直叩きした計測
   （`probe_pull_diagnostics.py`、round 間の待ち 0）で、didChange 直後の
   **round0 が最終集合**を返す（構文エラーは round0 で 2 件、round3 も同じ）。
   これは ADR-0052 の「pull は 1 回目で最終集合を返す」と同じ性質。

Status: accepted

## Decision

1. **`pull_after_edit` から固定待ちを削除する**（`PULL_SETTLE` 定数ごと撤去）。
   「解析前の空を避ける」ための待ちは、pull が解析完了を待つ以上、買えるものが無い。
2. **回帰したときは固定待ちを戻さない**。症状が「編集直後に診断が消える」なら、
   まず順序（didChange が本当に書かれたか）を疑い、次に push
   （`textDocument/publishDiagnostics`）を解析完了シグナルに使う経路を検討する。
3. pull の空を「クリーン確定」に使わない契約は**変えない**
   （ADR-0045 / ADR-0052 の `settled: false`）。

## Considered Options

- **50ms に縮める**: 却下 — 0ms でも回帰しなかった。待ちを残す理由（順序の保険）は
  上記のとおり成立していないので、残すのは遅延を買うだけ。
- **250ms のまま**（この反復をやらない）: 却下 — `apply` ごとに定数で 250ms、
  2 回目の apply の 7 割がこれだった。
- **push 通知を待ってから pull**: 却下（今は）— 実測が「待つ必要が無い」を示した。
  必要になったときの upgrade path として残す（Decision 2）。
- **settle を可変（解析完了の検知）にする**: 却下 — 検知したいものを pull 自身が
  既に待っている。

## Consequences

- `minas apply` の wall が毎回 ~250ms 下がる（L0 warm r=3 中央値:
  `apply` 2 回目 346 → 107ms、1 回目 1359 → 1133ms。`verify/*` の apply ステップ
  すべてに同じ効果）。`rename/apply` のような apply ループも同じだけ下がる。
- `calls` / `out_B` / `equiv_B` は完全不変（契約も wire 形状も変えないため
  PROTOCOL_VERSION は据え置き）。
- 固定待ちが無いので、他サーバ（tsserver 等）で「解析と競合する pull」をする実装が
  空を返す可能性は残る。そのときは診断が 1 編集ぶん古くなるだけで、誤ったクリーン
  （ADR-0045 の禁止）にはならない — `check` の `settled: false` が受け止める。
- 計測上、`apply` の残りのコストは **RA の初回再解析 600–1100ms**（編集処理自体は
  ~150ms）。これを外すには「apply は診断を返さず `check` が pull で確定する」
  契約変更が要る（`calls` が 1 増える取引）。

## 関連

- 実装: `minad/src/lsp.rs`（`pull_after_edit`）
- 測定の記録: `docs/loop/log.md` iteration #4（加法の確定）/ iteration #5（撤去）
- 判定の既存 ADR: ADR-0052（pull は 1 回目で最終集合を返す）、ADR-0045（空を
  クリーンの根拠にしない）、ADR-0020（inlay hint の所有と取得経路）
