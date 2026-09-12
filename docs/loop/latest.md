# minas/minad ループエンジニアリング — 最新の検討結果（次のループの起点）

> git 管理（`docs/loop/`）。更新: iteration #9 完了時（2026-09-12）。
> 入口は [`README.md`](./README.md)。検討方法は [`method.md`](./method.md)。
> **このファイルと method.md を読めば次のループを回せる。**
> 生データと全反復の考察は同じディレクトリの [`log.md`](./log.md)。
> 設計判断は `docs/adr/0051`（索引完走ゲート）/ `0052`（check の空の早期確定）/
> `0053`（編集後の settle を置かない ＋ 追記: pull は撤去できない）/
> `0054`（意味的リトライ待ち 0ms）/ `0055`（編集後 pull の背景化）/
> **`0056`（計時ログ `MINAD_TRACE` — iteration #9）** / `0045`（`settled` の意味）。

## 1. 現在の状態（30 秒サマリ）

- 完了した反復: **#1 計測器 → #2 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定
  （ADR-0052）→ #4 初回 apply の内訳確定 → #5 編集後の settle 撤去（ADR-0053）→
  #6 SEMANTIC_RETRY_WAIT 撤去（ADR-0054）→ #7 apply の pull 撤去（棄却。ADR-0053 追記）→
  #8 編集後 pull の背景化（ADR-0055）→ #9 計時ログ（ADR-0056）**
- 効果（L0 実測、warm r=3 中央値）:
  - `check`（クリーン）**10154ms → 87ms（−99%）**（#3 + #6 + #8 で維持）
  - `symbol`（explore 1 歩目）**583 → 82ms（−86%）**（#6）
  - `rename`（lsp）**1594 → ~1150ms（−28%）**（#6。残りは RA 側の WorkspaceEdit 計算）
  - `apply` **~1.1s → ~130ms（−88%）**（#8）
  - ギャップありの **apply + check の和: ~1270ms → 222ms（−82%）**、cargo 経路も −77〜82%
- cold の無言の誤り（空の `symbol`・部分 `rename`・`check` の LSP error）は 0 件のまま。
  `calls` / `out_B` / `equiv_B` は全 flow で不変（回帰なし）。`--cold` の完全性も維持。
- 全 463 テスト green（462 + #9 で足した trace 形式テスト 1 件）。
  `python3 docs/loop/l0.py --selftest` green。
- **#9 の結論**: 計時ログ（`MINAD_TRACE=1` で `minad.trace <span> <phase> <ms>`）を
  **採用**（ADR-0056。依存追加なし・既定 off・wire と PROTOCOL_VERSION は不変）。
  method.md §7 の「コマンド内部の内訳は A/B 除去でしか測れない」は解消。
  最初の計測で **#10 の課題が変わった**（下記）。
- **#9 で判明した最重要の事実**: L0 の**すべての flow の wall に、`minas` 呼び出し
  1 回あたり ~65ms の固定費**が乗っている（デーモンの応答は 0.27ms — Python 実測）。
  原因は (1) `minas` が死活確認に**捨てる接続**を開いて即 close し、(2) daemon の
  `accept_loop` が peer uid 検査をループ内で行い、相手が既に閉じていると
  `peer_cred()` が ENOTCONN で **5ms × 最大 10 回 sleep** する間 accept が止まる
  ため、後続（本命）接続の最初の往復が遅れる。A/B 実測: 単一接続 0.27ms vs
  probe→本接続 **67.2ms**。`explore/lsp` 269ms のうち 234ms がこれ。
- **次の課題（#10）はこの ~65ms 固定費の除去**（accept ループを sleep で止めない +
  捨て接続をやめる）。詳細と受理・棄却条件は §4。

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
| **編集後の固定 settle（`PULL_SETTLE`）は撤去できる**（ADR-0053）。250ms → 0ms で `apply` が毎回 −250ms、空 pull 回帰は `-r 10` × 3 flow = 30 run で 0 件 | iteration #5（L0 + プローブ直叩き） |
| **初回 apply の ~1.1s は RA の初回再解析であって待ちではない**: 同じ daemon で `symbol`/`check` を先に叩いてから `apply` すると **137ms / 98ms / 102ms** | iteration #5 の下調べ（`tmp/loop/recon6.py`）+ #6/#7 でも不変 |
| **`SEMANTIC_RETRY_WAIT` 500ms の撤去は回帰ゼロ**（0ms。ADR-0054）。「2 回連続同一」の確認自体は同一クエリの再送でほぼ無料 | iteration #6（warm -r3 全 flow + cold + -r10 で fails 0） |
| **「2 回連続同一」の確認の実測コストは 2ms**（rename の `lsp.retry` attempt1 912ms / **attempt2 2ms**） | iteration #9（計時ログ） |
| **`rename` の残り ~900ms は RA 側の WorkspaceEdit 計算**（`rename request` = 898ms。prepare 2 / resolve 12 / convert 2 / apply 2） | iteration #9（計時ログ）/ #6 の案 B 比較 |
| **`hints` の ~580ms は `pull_inlay_hints` の RA 側計算**（`hints total 578ms = pull 578`）。キャッシュヒット時は ~0ms | iteration #9（計時ログ）/ #6 |
| **`apply` の ~1.1s は「pull が強制する RA 解析」そのもの**。診断 pull だけ外しても hint pull が同じ解析を買うので `apply` は不変（両方外すと `apply` 153ms だが `check` が 1122ms を払う） | iteration #7 A/B(a)(b) |
| **編集後の pull は契約**（daemon のスナップショットが編集後診断・ヒントを反映する）。外すとテスト 3 件が落ちる | iteration #7（`cargo test`） |
| **cargo 検証経路では `apply` の先払い解析が二重払い**（RA 解析と rustc コンパイルは別物）。pull を外すと apply-cargo **1344→268ms**、apply2-cargo **1524→361ms**、hunks-cargo **1450→275ms** | iteration #7 A/B(b) |
| **編集後 pull の背景化で Save 応答の解析待ちが消える**（ADR-0055）。apply の wall は ~130ms で安定。ギャップあり（sleep 3 = 推論遅延の代理）で apply + check の和 **222ms（現行比 −82%）** | iteration #8（L0 `verify-gap` 追加。warm -r3） |
| **ギャップなし（即時 check）では、check は「背景 pull が買った 1 回の解析」をセッションロック越しに待つ**（`check total` 686ms = `borrow` 682（`ensure` の `await_indexed` が `session.lock()` を待つ）+ `pull` 2ms。背景 pull が先に終わっていれば `check total` **3ms**、ギャップありなら **5ms**）。**check 自身は解析を買っていない** | iteration #9（計時ログ・n=12 中央値）。#8 の「check が解析を買う」は言い過ぎだった（下記の修正） |
| **編集後 pull の背景タスクの内訳**: `didChange` 2ms → 診断 pull 892ms（= RA 解析）→ ヒント pull 6ms → 反映・push 0ms。`didChange` が 300〜650ms になるのは前の背景 pull のロック待ち（応答はブロックしない） | iteration #9（計時ログ） |
| **`minas` の 1 回の起動でデーモン側は 0.27ms で答える**（Python で同一プロトコルを叩いた実測。GetServerInfo） | iteration #9（`tmp/loop/probe-client/`） |
| **`minas` 呼び出し 1 回ごとに ~65ms の固定費**（`minas --help` 12ms / `minas info` 83ms / `minas apply` 91ms = Open 往復 59 + edit 9 + Save 2）。原因は「捨て接続 + accept ループ内 peer uid 検査の sleep」。**probe→本接続で 67.2ms、単一接続なら 0.27ms**（Python で再現） | iteration #9（A/B 実測） |
| 編集後追従のテスト 3 件は「応答時点」で検証していた（#8 の前提誤り）。検証ポイントを「追従までの poll（最大 10 秒）」に移して契約を維持 | iteration #8（ADR-0055） |
| **計時ログ（`MINAD_TRACE=1`）で 1 コマンドの span/phase/ms が取れる**（既定 off、wire・PROTOCOL_VERSION 不変、回帰なし） | iteration #9（ADR-0056） |
| cold の支配項は**索引完走ゲート**（`symbol`/`hints`/`rename`/`check` が 8〜9 秒。`apply` は cold でも 120〜140ms） | L0 `--cold`（#2〜#8 で安定） |

**棄却済み（同じ仮説を再試行しない）**:

| 棄却した仮説 | 理由 |
|---|---|
| `apply`→`check` が cargo 委譲より**常に**安い | 当時 `calls`/`equiv_B` は同等で `wall` が 7.4 倍。**iteration #3 で check は 589ms になり逆転**したので、この棄却は「当時の契約では成立しない」だけ。行番号つき診断 = `check` / 最終的な根拠 = `cargo` という使い分けを L2 で測り直すのが正しい（§4 の別候補） |
| 初回 apply の ~1.1s は **hint pull が主因** | iteration #4: hint pull を外して A/B → 不変。主因は初回 diag pull 内の RA 再解析。#7 で「どちらの pull でも同じ解析を買う」ことが確定した |
| hint の消費者は `minas hints` と daemon キャッシュだけ。**`minae`（TUI）は inlay hint を描画しない** | iteration #4 で確認 |
| cold でも warm と同じ結果を返す | 無言の誤り 3 件（ADR-0051） |
| 空 + 索引完走 = クリーン確定（`settled:true`） | pull が実在するエラーを取りこぼす（ADR-0045 追記） |
| skill 参照が判断を変える | T6/T8（L2）: トークン +21〜61% で効果なし |
| **`PULL_SETTLE` は「解析前の空」を避けるために要る** | iteration #5: 50ms・0ms のどちらでも空 pull 回帰が出ない → 撤去（ADR-0053） |
| **「2 回連続同一」を捨て、loading のときだけリトライすれば 1 往復削れる** | iteration #6 案 B: `symbol` 84ms（案 A 82ms）・`rename` 1162ms（案 A 1150ms）と差なし。iteration #9 の計時で確認の実費は **2ms** と確定 → 不採用 |
| **`SEMANTIC_RETRY_WAIT` を 50ms・100ms 等に縮めて様子見する** | iteration #6: 0ms で回帰ゼロだったので二分探索の必要が無かった（ADR-0054） |
| **`apply` の診断 pull を外せば 1.1s が消える**（`settled:false` 即返し・契約変更） | iteration #7: A/B(a) は `apply` 不変で daemon テスト 2 件 fail。A/B(b) は `apply` 153ms だが `check` 1122ms（和 ±0）でテスト 3 件 fail |
| **診断 pull だけ外す（hint pull は残す）で `apply` が 720ms になる** | iteration #7: 同じバイナリの再測で 1155ms（基準値と同値）。719ms は Open 時背景 settle との競合で先に解析が済んでいた場合の観測だった |
| **背景化ではテスト 3 件（編集後追従）が落ちる = 契約が壊れる** | iteration #8: 検証していたのは「応答時点の追従」= 実装の偶然。契約の本質は背景 pull + push でも満たされる（ADR-0055） |
| **ギャップなし check の ~800ms は「check が解析を買う」二重払い** | iteration #9: `check` 自身の pull は 2ms で、~700ms は `ensure` の `await_indexed` が背景 pull のセッションロックを待つ時間。解析は 1 回だけ（同じ 1 回を待っている）。二度買ってはいない |
| **計時ログは A/B 除去か一時トレースでしか取れない**（method.md §7 の旧記述） | iteration #9: `MINAD_TRACE=1` で恒久的に取れる（ADR-0056） |
| **計時（trace）を入れると wall が歪む** | iteration #9: trace on/off の同一条件比較（verify、n=9 ずつ）で apply-check **−5.3%**・apply-cargo −2.2%（minas の step のみの arm）。既定 off では `Instant::now()` 1 回と分岐だけ |

## 3. 基準値（L0）— 次回の比較はここから

### warm（r=3 中央値。計測は #8/#9 で再測済み。**wall は #9 の ~65ms 固定費を含む**）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `verify/apply-check` | 2 | 246 | 354 | 1020 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 240 | 0 | True |
| `verify/hunks-cargo` | 2 | 154 | 308 | 256 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 344 | 0 | True |
| `verify-gap/apply-gap-check` | 3* | 262 | 494 | 3239 | 0 | True |
| `verify-gap/apply-gap-cargo` | 3* | 116 | 348 | 3272 | 0 | True |
| `verify-broken/check` | 2 | 450 | 555 | 483 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 217 | 0 | True |
| `verify-blind/check` | 2 | 240 | 344 | 606 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 266 | 0 | True |
| `explore/lsp` | 3 | 1017 | 1568 | 278 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 156 | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 670 | 0 | True |
| `rename/lsp` | 2 | 219 | 438 | 1150 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 600 | 0 | True |

*verify-gap の calls=3 は計測 step の `sleep 3`（エージェントの次ターン推論遅延の
代理）を含むため。契約の往復は apply + check/cargo の 2 回のまま。

**注意（#9 で確定）**: 上記の wall には **`minas` 呼び出しごとの ~65ms 固定費**
（§4 の課題）が含まれる。`calls` が多い flow ほど大きく、`explore/lsp`（3 calls）
では 234ms = wall の 87% がこれ。したがって **#9 以前の wall の絶対値は「実処理 +
calls×65ms + クライアント起動 12ms」として読む**。相対比較（同一セッションの A/B）
はこれまで通り有効。

**揺れの目安（#9 で再測）**: 同一条件でも `verify` 系の n=9 中央値は
apply-check **846〜894ms** / apply-cargo 275〜282 / hunks-cargo 285〜308 /
apply2-cargo 496〜576 と動く（`cargo check` の step は同一セッションでも
124〜1555ms と振れる）。**1 回の観測で結論を出さない**（`-r 3` 以上 + 同一
セッションでの交互測定）。

### warm の計時の内訳（iteration #9、`MINAD_TRACE=1`。verify 系 n=12 中央値）

```
verify/apply-check
  edit total 2          ← DocumentEdit 適用 0 + 応答構築 1（デーモン側はほぼ無料）
  sync.bg total 902     ← didChange 2 → 診断 pull 892（= RA 解析）→ hint pull 6 → 反映/push 0
  check total 686       ← borrow 682（ensure が背景 pull のロックを待つ）+ pull 2（round0 0 / round1 2）
                          ※背景 pull が先に終わっていれば check total 3ms
verify-gap/apply-gap-check
  check total 5         ← 背景 pull 完了後なら check 自身は 5ms
rename/lsp
  rename total 918      ← prepare 2 / resolve 12 / request 898（attempt1 912 + attempt2 2）/ convert 2 / apply 2
hints/hints
  hints total 578       ← pull 578（RA の inlayHint 計算）
クライアント側（probe 実測・デバッグビルド）
  minas --help 12.4ms / minas info 83ms / minas read 84ms / minas apply 91ms
  daemon の 1 往復（Python）0.27ms ／ probe→本接続 67.2ms（= 65ms 固定費）
```

計測の内訳が知りたいとき: `MINAD_TRACE=1 python3 docs/loop/l0.py -f <flow> -r 1` の後、
`rg 'minad.trace' tmp/loop/<flow>-<arm>-*/.tmp/minad.log`。

### cold（1 回観測、iteration #8。他は #6 の値）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `verify/apply-check` | 2 | 246 | 354 | 8690 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 425 | 0 | True |
| `verify/hunks-cargo` | 2 | 154 | 308 | 429 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 510 | 0 | True |
| `verify-gap/apply-gap-check` | 3 | 262 | 494 | 8747 | 0 | True |
| `verify-gap/apply-gap-cargo` | 3 | 116 | 348 | 3307 | 0 | True |
| `explore/lsp` | 3 | 1017 | 1568 | 8205 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 169 | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 8067 | 0 | True |
| `rename/lsp` | 2 | 219 | 438 | 9127 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 597 | 0 | True |
| `verify-blind/check` | 2 | 240 | 344 | 9095 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 434 | 0 | True |
| `verify-broken/check` | 2 | 450 | 555 | 8793 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 338 | 0 | True |

cold の支配項は**索引完走ゲート**（`symbol`/`hints`/`rename`/`check` が 8〜9 秒。
`apply` は cold でも 120〜140ms）。ゲートは背景 pull と独立の機構で、~65ms の
固定費（§4）はこの中に埋もれる。

## 4. 次の課題設定（iteration #10）

> **iteration #9 の結末（この課題の起点）**: 計時ログを採用（ADR-0056）し、
> その最初の計測で「すべての `minas` 呼び出しに ~65ms の固定費がある」ことが
> 判明した。内訳の実測で #8 の前提の一部も修正された（ギャップなし check は
> 解析を買っておらず、背景 pull のロックを待っているだけ = `check pull` 2ms）。
> apply の ~130ms はデーモンではなくクライアント（Open/DocumentEdit/Save の
> 3 往復 + 起動 12ms + 固定費 65ms）。rename/hints の残りは RA 側の計算
> （各 ~900ms / ~580ms）で、削る対象ではない。

### 課題

**`minas` 呼び出し 1 回あたりの ~65ms 固定費を除去する**（L0 の全 flow の wall に
calls × ~65ms で乗っている。最も費用対効果の大きい残存項）。

原因（iteration #9 の A/B で確定）:

1. `minas` の `open_one_shot` は「daemon が生きているか」の確認に**捨てる接続**を
   1 本開き、`conn::connect` が成功したら即座に閉じる（その後 `open_session` が
   本命の接続を張り直す）。
2. daemon の `accept_loop` は接続ごとの **peer uid 検査を accept ループの中で**
   行い、`stream.peer_cred()` が ENOTCONN（相手が既に閉じた = 上記の捨て接続）の
   とき `5ms × 最大 10 回` sleep する。この間 accept ループが止まるため、直後に
   到着した本命接続はカーネルの backlog に座ったまま**最初の往復が ~60ms 遅れる**。

### 仮説

- (a) peer uid 検査を accept ループの外（spawn した接続タスクの先頭。fail closed
  は維持）へ移すと、accept ループが sleep で止まらなくなり、固定費が消える
  （捨て接続の影響を受けなくなる）。
- (b) `minas` の捨て接続をやめ、`conn::connect` の結果をそのままセッションに使う
  （接続 1 本/コマンド）と、引き金そのものが消え、同時にデーモンの無駄な
  accept/drop も消える。
- (c) (a)+(b) で `minas <任意>` の wall が **~65ms 下がる**（`minas info` 83 → ~18ms、
  `minas apply` 91 → ~30ms）。L0 の flow は `calls × 65ms` ぶん下がる
  （`explore/lsp` 278 → ~85ms、`verify/apply-check` 1020 → ~890ms、
  `rename/lsp` 1150 → ~1085ms、`hints` 670 → ~605ms）。`calls`/`out_B`/`equiv_B`
  は不変（契約は触らない）。

### 測り方（順序を守る）

1. **原因の再確認（プローブ）**: `tmp/loop/probe-client` と同じ形で
   「単一接続 vs probe→本接続」を Python で測る（0.27ms vs 67ms が再現するか）。
   ここで再現しなければ仮説が崩れているので、先に進まない。
2. **A/B**: (a) accept ループ修正のみ / (b) minas の接続再利用のみ / (c) 両方。
   同一セッションで `-f verify -r 3` と `-f explore -r 3` を交互に測る
   （wall は中央値。§3 の注意どおり `-r 3` 必須）。
3. **契約の確認**: daemon 側のテスト（peer uid の拒否 = セキュリティ）が緑である
   こと、`cargo test` **463 green**、L0 で `calls`/`out_B`/`equiv_B` 不変・
   `fails` 0・`settled` 契約維持（blind）/ `rc=2` + Syntax Error（broken）。
4. **--cold と -r 10**: cold は索引ゲートが支配的なので変化は小さいはず（確認のみ）。
   固定費は接続経路なので cold でも同じだけ効くはず — 差が出なければ別の原因。
5. **効果確認**: `minas info` の wall（probe 実測）が 83 → ~18ms になり、
   L0 の `explore/lsp` wall が −~190ms になることを確認。
6. **→ 次反復 #11 の課題設定**: 効果確認の結果から、(a) 次の固定費
   （`minas` 起動 12ms × calls / 3 往復の apply）、(b) `ServerMetrics` の穴を
   v20 bump と束ねる、(c) L2（実 LLM）での確認、のどれかを選ぶ。

### 受理条件 / 棄却条件

- **受理**: `minas` の 1 呼び出しの固定費が ~65ms → 数 ms になり、L0 の全 flow で
  wall が `calls × ~65ms` ぶん下がる。`calls`/`out_B`/`equiv_B` 不変・463 green・
  cold の無言の誤り 0・peer uid 検査の fail closed が維持される
  （接続タスクの先頭で検査し、不一致なら即 close）。
- **棄却**: (a) probe の再現で固定費が出ない（= 原因の見立てが誤り）、
  (b) uid 検査を accept ループ外へ出すと他人の接続を弾けなくなる（セキュリティ
  の後退。検査の位置を変えるだけで fail closed は維持できるはず）、
  (c) L0 で差が出ない（= ~65ms の見積もりが誤り）。

### 別候補（後回し）

- **`ServerMetrics` の穴（rename カウンタ・`get_state_bytes`）**: `ServerInfo` の
  wire 変更なので ADR-0039 では PROTOCOL_VERSION bump が要る（v18 の前例）。
  §4 の「PROTOCOL_VERSION 不変」制約と衝突するため、次に wire を変える用事と
  束ねて v20 として行う（ADR-0056 に記載）。rename の files/edits は
  `minad.trace rename files=… edits=…` で per コマンド取れるので緊急性は低い。
- **`minas apply` の 3 往復（Open + DocumentEdit + Save）を 1〜2 往復へ**: 固定費
  除去後は apply の wall の主項になる（Open 往復 59ms のうち 65ms は固定費、
  残り ~10ms はスナップショットの往復）。契約（checksum/expected_text）の
  再設計が要るので、固定費除去の後で。
- **L2（実 LLM）で verify-gap 相当の実フロー確認**: 背景化の効果は wall（待ち）
  なので L2 のトークン計測では見えない（L2 で確認できるのは「回帰なし」だけ）。
- **ギャップなし check の ~700ms**: `ensure` の `await_indexed` が背景 pull の
  ロックを待つ時間で、解析は 1 回しか買われていない（#9 で確定）。削るには
  「背景 pull と check を同じ 1 回の要求に合流させる」設計変更が要る。実フロー
  では推論が間に合うので優先度は低い。
- L2 での規模検証（fixture が小さいので `wall_ms` は外挿不可。`calls`/`equiv_B`
  は外挿可）／上位モデルでの再現（L2 は nano のみ）。
- `open_workspace_files`（rename/references の前に全ファイル didOpen）が必要かの
  再確認。cold の主因は索引完走ゲートで、L0 の fixture では差が出ない（L2 向き）。
- `apply --no-diagnostics`: 背景化（#8）で既定 apply が ~130ms（うち 65ms は
  上記の固定費）になったので、必要度はさらに下がった。

## 5. 次のセッションの起動手順

**新しいセッションの入口**（`AGENTS.md` から `docs/loop/README.md` に辿れる。
プロンプトテンプレート `.pi/prompts/dev_minas.md` = `/dev_minas` でも同じ指示を展開できる。
手で貼るならこれ）:

```text
minas/minad のループエンジニアリング（エージェントのコスト低減）の続きをやって。
まず docs/loop/method.md と docs/loop/latest.md を読み、
latest.md の §4（次の課題設定）から iteration #10 を回して。計測したら --log で
log.md に記録し、考察を追記し、latest.md を更新して。
```

**注意**: `tmp/loop/`（結果 JSON・fixture・`MINAD_TRACE` のログ）は checkout ごとの
gitignore 対象。worktree で測る場合はそちらに作られる（この文書群は git 管理なので
worktree にもある）。`minad` / `minas` を止めずに複数の l0 実行を重ねない
（索引・CPU が競合して `wall_ms` が濁る。`l0.py` はアームごとに専用 daemon を
立てるので、同時実行は避ける）。**手で起動した daemon は呼び出し元の終了で
死ぬことがある**（probe の測定では、daemon を立てたシェルと同じコマンド内で
測る）。

```bash
cd /Users/335g/dev/other/mina
cargo build                                  # 計測対象は target/debug。コードを触ったら必須
cargo test                                   # 期待値: 463 passed
python3 docs/loop/l0.py --selftest          # 計測器の健全性
python3 docs/loop/l0.py -f verify -f verify-gap -f verify-broken -f verify-blind -r 3 -v   # 現状確認（apply ~130ms / ギャップあり和 222ms）
MINAD_TRACE=1 python3 docs/loop/l0.py -f verify -r 1 -v      # 内訳（minad.log に span）
git log --oneline -5                         # 直前の変更を確認（auto-checkpoint が並ぶ）
```

そのあと §4 の「測り方」1 → 2 → … の順に進める。
`--log` は必ず付ける（JSON は `tmp/loop/` に落ちるだけなので、付け忘れると生データが消える）。
`apply` を含む比較は `-r 3` 必須・**同一セッションで交互に**測る（§3 の注意）。

**反復番号の規則**: 今回の反復番号は §4 の見出しの番号（いまは **iteration #10**）。
結果は `log.md` に「iteration #10」として書き、考察の要約をこの `latest.md` に反映したうえで、
§4 を次の番号（#11）の課題設定に書き換える。§5 はこの手順のまま使う。
