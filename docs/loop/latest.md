# minas/minad ループエンジニアリング — 最新の検討結果（次のループの起点）

> git 管理（`docs/loop/`）。更新: iteration #11 完了時（2026-09-14）。
> 入口は [`README.md`](./README.md)。検討方法は [`method.md`](./method.md)。
> **このファイルと method.md を読めば次のループを回せる。**
> 生データと全反復の考察は同じディレクトリの [`log.md`](./log.md)。
> 設計判断は `docs/adr/0051`（索引完走ゲート）/ `0052`（check の空の早期確定）/
> `0053`（編集後の settle を置かない ＋ 追記: pull は撤去できない）/
> `0054`（意味的リトライ待ち 0ms）/ `0055`（編集後 pull の背景化）/
> `0056`（計時ログ `MINAD_TRACE` — iteration #9）/ `0071`（peer uid 検査を accept
> ループの外へ — iteration #10）/ **`0072`（LSP へ送る直前に現在のテキストを取り直す
> — iteration #11）** / `0045`（`settled` の意味）。

## 1. 現在の状態（30 秒サマリ）

- 完了した反復: **#1 計測器 → #2 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定
  （ADR-0052）→ #4 初回 apply の内訳確定 → #5 編集後の settle 撤去（ADR-0053）→
  #6 SEMANTIC_RETRY_WAIT 撤去（ADR-0054）→ #7 apply の pull 撤去（棄却。ADR-0053 追記）→
  #8 編集後 pull の背景化（ADR-0055）→ #9 計時ログ（ADR-0056）→
  #10 接続経路の固定費除去（ADR-0071）→ #11 cold の check の巻き戻し修正（ADR-0072）**
- 効果（L0 実測。**#10 で計測器を直したので §3 の表が新基準**）:
  - `check`（クリーン）**10154ms → 87ms（−99%）**（#3 + #6 + #8 で維持）
  - `symbol`（explore 1 歩目）**583 → 82ms（−86%）**（#6）
  - `explore/lsp`（3 calls の探索）**310 → 82–89ms（−74%）**、`explore/dump`
    **164 → 28–29ms（−83%）**（#10）
  - `minas` 呼び出しの**固定費 76.6ms → 9.1ms**（#10。`minas info` 実測）
  - `rename`（lsp）**1594 → ~1150–1350ms**（#6。残りは RA 側の WorkspaceEdit 計算）
  - `apply` **~1.1s → ~130ms**（#8 の daemon 側背景化。**ただし ADR-0061 以降の CLI は
    初回 ~1.0s = §4 の #12**）。ギャップありの apply + check の和 **~1270 → 222ms**
    （−82%）、cargo 経路も −77〜82%
- cold の無言の誤り（空応答 + exit 0）は**新たに出ていない**（`-r 10` も fails 0）。
  `calls` / `out_B` / `equiv_B` は全 flow で不変（回帰なし）。
- 全 **486 テスト** green（485 + #11 の回帰テスト 1）。`python3 docs/loop/l0.py --selftest` green。
- **#10 の結末**: 接続経路の固定費を**採用**（ADR-0071）。(a) daemon の peer uid 検査を
  accept ループから接続タスクの先頭へ、(b) minas の捨て接続をやめて死活確認の接続を
  本命に再利用 — の**両方**を入れた（測定上はどちらか片方で固定費が消える。
  `explore/lsp` 310 → 82ms、`dump` 164 → 28ms、プローブの probe→本接続 68.4 → 0.52ms）。
- **#10 で見つけた計測器の穴（重要）**: L0 の flow の step コマンドは `minas` と**名前で**
  書いてあり、**PATH の minas**（インストール済み）が実行されていた。`L0_MINAS` は
  `server_metrics` にしか効かず、**クライアント側の変更は L0 の step で一度も測られて
  いなかった**。`l0.py` の `run()` を修正して解決（§3 の旧値は PATH minas での値）。
- **#11 の結末（#10 で見つけた `clean-unverified` の穴）**: 原因は**索引ゲートでも
  早期確定でもなく、サーバ側の文書が編集前に巻き戻っていた**こと。`Command::Open` の
  背景タスクが spawn 時のテキストを `ensure`（cold では数秒）の後に送っていたため、
  その間の編集が RA 側で戻り、pull は「編集前の正しい空」を返していた。
  送る直前に現在のテキストを取り直す修正で採用（ADR-0072）。cold の
  `verify-broken/check` は `-r 10` で **10/10 が rc=2 + Syntax Error**（fails 0）。
- **#11 で見つけた新しい穴（次の課題）**: `minas apply` の**初回だけ ~1.0s**（2 回目以降
  は ~30ms）。プローブ実測で **Save 往復に ~963ms**（Open 37ms / DocumentEdit 3.4ms、
  daemon 側の edit は 2ms）。ADR-0061（09-13 追加。**#8 の後**）の watched-files 通知が
  Save 応答の末尾で `session.lock()`（最大 3s）を待つため、背景 pull（~1s）が
  再び応答経路に乗っている。**#8 の「apply ~130ms」は warm（解析済み）での値**。
- **次の課題（#12）はこの `apply` の初回 ~1.0s**。詳細と受理・棄却条件は §4。

## 2. 確定した事実（再測定は不要）

| 事実 | 根拠 |
|---|---|
| 読む量＝費消の主因。範囲 read（`symbol`→`at`→`read --lines`）は全文 dump より `equiv_B` **−83%**（1568 vs 9260） | L0 `explore`（iteration #1〜#3 で安定） |
| 編集は**内容指定**（`apply`）。位置指定は失敗しやすい | T3（L2）/ L0 で `apply` が正面入口 |
| 複数編集は `--hunks-stdin` で往復削減（2 編集: 3 calls/545 → 2 calls/308 equiv、−44%） | L0 `verify/hunks-cargo` vs `apply2-cargo` |
| 複数箇所・複数ファイルのリネームは LSP `rename`（2 calls/438 vs apply ループ 5 calls/1424） | L0 `rename`（T5/T9 を再現） |
| **rust-analyzer は `window.workDoneProgress` を advertise しないと `$/progress` を送らない** | プローブ実測（`probe_progress.py`）/ ADR-0051 |
| 索引完走（`cachePriming` = title "Indexing" の end）を待たないと、`symbol` が空・`rename` が部分適用を「成功」報告・`check` が LSP error | L0 `--cold`（iteration #2 前）/ ADR-0051 |
| **pull 診断は 1 回目の要求で最終集合を返す**（round0 == round11）。空が後から非空に変わることは無い | プローブ実測（`probe_pull_diagnostics.py`）/ ADR-0052。**warm での結果** |
| **cold の pull は索引完走（`cachePriming` の end）まで空で、完走直後に非空になる**（RA は `workspace/diagnostic/refresh` も送る）。索引中は pull が数秒ブロックする（2.9〜3.0s の実測あり） | iteration #11 のプローブ（`tmp/loop/probe_cold_pull.py`） |
| **空は「送ったテキストが最新」のときにだけ「見た結果の空」になる**。サーバ側の文書が古いと、pull は古い文書に対して正しく空を返し、早期確定（ADR-0052）がそれを確定にしてしまう | iteration #11（ADR-0072） |
| **同一テキストの冗長な didChange は RA の診断を無効化しない**（直後の pull も 1 回目で非空） | iteration #11 のプローブ（`tmp/loop/probe_seq.py`。当初はこれを疑ったが棄却） |
| **cold の `verify-broken/check` が `clean-unverified` だったのは、Open の背景タスクが編集前の全文を送り直し、サーバの文書が巻き戻っていたため**。送る直前に現在のテキストを取り直して解決（−7.2、`-r 10` で 10/10 が rc=2） | iteration #11（ADR-0072） |
| **`minas apply` の初回 ~1.0s は Save 往復**（プローブで Open 37ms / DocumentEdit 3.4ms / **Save 963ms**。daemon の `edit total` は 2ms、背景 `sync.bg pull` は ~1030ms）。Save 末尾の ADR-0061 の watched-files 通知が `session.lock()`（最大 3s）を待つため、背景 pull が応答経路に戻る。2 回目以降は 29ms | iteration #11 の補助プローブ（`tmp/loop/probe_apply.py`）/ L0 `apply2-cargo`（1061 → 29ms）/ ADR-0061（09-13 16:58 追加 = **#8 の後**） |
| pull が**見る**: 構文エラー / 同一ファイル内の型エラー（`no such field` 等）。**見ない**: メソッド解決エラー（`cfg.validate2()` は永久に空）、クロスファイル型エラー | 同上。だから `check` の空は `settled:false`（未確認）で、`cargo` が最終的な根拠 |
| 空応答の settle 予算（20×500ms）と Open の 30 秒 settle は「同じ答えしか返らない待ち」 | ADR-0052 |
| **編集後の固定 settle（`PULL_SETTLE`）は撤去できる**（ADR-0053）。250ms → 0ms で `apply` が毎回 −250ms、空 pull 回帰は `-r 10` × 3 flow = 30 run で 0 件 | iteration #5（L0 + プローブ直叩き） |
| **初回 apply の ~1.1s は RA の初回再解析であって待ちではない**: 同じ daemon で `symbol`/`check` を先に叩いてから `apply` すると **137ms / 98ms / 102ms** | iteration #5 の下調べ（`tmp/loop/recon6.py`）+ #6/#7 でも不変。**#11 で精密化**: daemon の編集処理は 2msで、解析は背景（ADR-0055）。**CLI の apply が ~1.0s かかるのは Save 末尾の watched-files 通知が背景 pull のセッションロックを待つため**（ADR-0061 以降。§4 の #12） |
| **`SEMANTIC_RETRY_WAIT` 500ms の撤去は回帰ゼロ**（0ms。ADR-0054）。「2 回連続同一」の確認自体は同一クエリの再送でほぼ無料 | iteration #6（warm -r3 全 flow + cold + -r10 で fails 0） |
| **「2 回連続同一」の確認の実測コストは 2ms**（rename の `lsp.retry` attempt1 912ms / **attempt2 2ms**） | iteration #9（計時ログ） |
| **`rename` の残り ~900ms は RA 側の WorkspaceEdit 計算**（`rename request` = 898ms。prepare 2 / resolve 12 / convert 2 / apply 2） | iteration #9（計時ログ）/ #6 の案 B 比較 |
| **`hints` の ~580ms は `pull_inlay_hints` の RA 側計算**（`hints total 578ms = pull 578`）。キャッシュヒット時は ~0ms | iteration #9（計時ログ）/ #6 |
| **`apply` の ~1.1s は「pull が強制する RA 解析」そのもの**。診断 pull だけ外しても hint pull が同じ解析を買うので `apply` は不変（両方外すと `apply` 153ms だが `check` が 1122ms を払う） | iteration #7 A/B(a)(b) |
| **編集後の pull は契約**（daemon のスナップショットが編集後診断・ヒントを反映する）。外すとテスト 3 件が落ちる | iteration #7（`cargo test`） |
| **cargo 検証経路では `apply` の先払い解析が二重払い**（RA 解析と rustc コンパイルは別物）。pull を外すと apply-cargo **1344→268ms**、apply2-cargo **1524→361ms**、hunks-cargo **1450→275ms** | iteration #7 A/B(b) |
| **編集後 pull の背景化で Save 応答の解析待ちが消える**（ADR-0055）。apply の wall は ~130ms で安定。ギャップあり（sleep 3 = 推論遅延の代理）で apply + check の和 **222ms（現行比 −82%）** | iteration #8（L0 `verify-gap` 追加。warm -r3）。**ただし ADR-0061（09-13）以降、初回の apply は Save の watched-files 通知が背景 pull のロックを待って ~1.0s に戻った（§4 の #12）** |
| **ギャップなし（即時 check）では、check は「背景 pull が買った 1 回の解析」をセッションロック越しに待つ**（`check total` 686ms = `borrow` 682（`ensure` の `await_indexed` が `session.lock()` を待つ）+ `pull` 2ms。背景 pull が先に終わっていれば `check total` **3ms**、ギャップありなら **5ms**）。**check 自身は解析を買っていない** | iteration #9（計時ログ・n=12 中央値）。#8 の「check が解析を買う」は言い過ぎだった（下記の修正） |
| **編集後 pull の背景タスクの内訳**: `didChange` 2ms → 診断 pull 892ms（= RA 解析）→ ヒント pull 6ms → 反映・push 0ms。`didChange` が 300〜650ms になるのは前の背景 pull のロック待ち（応答はブロックしない） | iteration #9（計時ログ） |
| **`minas` の 1 回の起動でデーモン側は 0.27ms で答える**（Python で同一プロトコルを叩いた実測。GetServerInfo） | iteration #9（`tmp/loop/probe-client/`） |
| **接続経路の固定費 ~68ms の正体は accept ループ内の peer uid 検査**（捨て接続を accept すると ENOTCONN で 5ms×10 sleep し、その間 accept が止まる）。検査を接続タスクの先頭へ移す (a) か、捨て接続をやめる (b) のどちらかで消える（probe→本接続 **68.4 → 0.52ms**。L0 `explore/lsp` **310 → 82ms**・`dump` **164 → 28ms**。 (a)(b)(c) 同等・2 ラウンド再現） | iteration #10（ADR-0071。プローブ + A/B） |
| **`minas` 1 呼び出しの固定費は 76.6 → 9.1ms になった**（`minas info`、baseline デーモン相手。PATH の 0.0.5 も 78.5ms = 同じ旧経路）| iteration #10（ADR-0071） |
| **L0 の step コマンドは PATH の `minas` を実行していた**（`L0_MINAS` は `server_metrics` にしか効かず、step は `"minas …"` の文字列を shell に渡していた）。つまり**クライアント側の変更は L0 の step で一度も測られていなかった**（#10 で発見し `l0.py` の `run()` を修正） | iteration #10（A/B の (b) が効いて見えなかった原因）/ ADR-0071 |
| **手動テストで起動した daemon は残る**（`minas` の自動起動は `setsid` で切り離す。`(… &)` で起動したものも同様）。残ると rust-analyzer ごと CPU を食い、`minas symbol` が 12ms のはずが 90ms に見える。**測る前に `ps aux \| grep -E "minad serve\|rust-analyzer" \| wc -l` を 0 にする**（macOS の `pgrep -c` は無い） | iteration #10（実際に 3 個の stray で汚染された） |
| ~~cold の `verify-broken/check` は `clean-unverified`~~ → **#11 で修正済み**（原因は送信直前の取り直し漏れ。ADR-0072）。cold でも rc=2 + Syntax Error（`-r 10` で 10/10） | iteration #10（発見）/ #11（修正・ADR-0072） |
| **Open の背景タスクは `ensure` をまたいで spawn 時のテキストを送ってはならない**（送る直前の取り直しが規律。`settle_open_diagnostics_loop` は以前から毎ラウンド読み直していた） | iteration #11（ADR-0072）/ 回帰テスト `open_background_settle_sends_the_edited_text_not_the_snapshot`（mock の `MOCK_INIT_DELAY_MS` で initialize を 1.5s 遅らせる。修正を戻すと失敗することを確認） |
| **cold の `check` の wall は索引完走ゲートが支配**（5.4〜6.3s）。#11 の修正で +0.4s 以内（±10%）、探索 flow も ±10% 以内 | iteration #11（`--cold` 全 flow） |
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
| **L0 の step は計測対象（`L0_MINAS` / `target/debug`）のバイナリを実行している** | iteration #10: 実際は PATH の `minas`（インストール済み）で、`L0_MINAS` は `server_metrics` にしか効かなかった（計測器の欠陥）。`l0.py` の `run()` を修正 |
| **索引完走ゲートの判定を「対象ファイルの解析完了の観測」に変えれば cold の `check` が確定できる** | iteration #11: 原因は待ち方ではなく、背景タスクが送る古い全文で**サーバの文書が巻き戻っていた**こと。索引完走直後の pull は（正しいテキストなら）非空を返す |
| **cold では空の早期確定（ADR-0052）が効きすぎるので無効化する** | iteration #11: 同上。早期確定が見ていたのは「古い文書の空」で、判定自体は ADR-0045/0052 のまま正しい（前提は「送ったテキストが最新」） |
| **同一テキストの冗長 didChange が RA の pull 診断を無効化する** | iteration #11: プローブ（`tmp/loop/probe_seq.py`）で同一テキストの didChange 後も pull は 1 回目で非空。無効化していたのは**違う（古い）テキスト** |
| **計時（trace）を入れると wall が歪む** | iteration #9: trace on/off の同一条件比較（verify、n=9 ずつ）で apply-check **−5.3%**・apply-cargo −2.2%（minas の step のみの arm）。既定 off では `Instant::now()` 1 回と分岐だけ |

## 3. 基準値（L0）— 次回の比較はここから

### warm（r=3 中央値、`-r 3 --log` の全 flow、**#11 の修正後**）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `verify/apply-check` | 2 | 275 | 383 | 1141 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 1250 | 0 | True |
| `verify/hunks-cargo` | 2 | 169 | 338 | 1264 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 1280 | 0 | True |
| `verify-gap/apply-gap-check` | 3* | 291 | 523 | 4042 | 0 | True |
| `verify-gap/apply-gap-cargo` | 3* | 116 | 348 | 4267 | 0 | True |
| `verify-broken/check` | 2 | 473 | 578 | 747 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 815 | 0 | True |
| `verify-blind/check` | 2 | 269 | 373 | 727 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 843 | 0 | True |
| `explore/lsp` | 3 | 1085 | 1672 | **80.0** | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | **28.0** | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 685 | 0 | True |
| `rename/lsp` | 2 | 309 | 618 | 1398 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 1379 | 0 | True |

`out_B` の ±2〜数バイトは、応答に**fixture の絶対パス**が入るため（`tmp/loop/<flow>-<arm>-<時刻>-<pid>/…` の長さが違う）。`calls`/`equiv_B` は不変。verify 系の wall は**初回 `apply` の ~1.0s**（#12 の課題）を含む。

*verify-gap の calls=3 は計測 step の `sleep 3`（エージェントの次ターン推論遅延の
代理）を含むため。契約の往復は apply + check/cargo の 2 回のまま。

**値を読むときの注意（#10 で確定）**:

1. **この表は「step がビルド済みバイナリを実行する」修正後の値**。旧表は step が
   **PATH の minas（インストール済み 0.0.5）**を実行した値で、`out_B` もずれる
   （例 `explore/lsp` 1017 → 1082）。計測対象は `cargo build` した `target/debug`。
2. **`cargo check` を含む arm の wall は cargo の揺れが支配的**（同一セッションでも
   124〜1555ms。このセッションは ~700〜900ms に寄った）。固定費の信号として
   使わない。信号は calls に決定的な `explore` と `minas info`。
3. **固定費の前後（#10 の A/B、同一セッション交互・2 ラウンド、n=3 中央値）**:

   | arm | `explore/lsp` | `explore/dump` |
   |---|---|---|
   | baseline（両方 未修正） | 310.0 / 299.2ms | 163.5 / 168.0ms |
   | (a) daemon のみ修正 | 82.0 / 82.3ms | 28.5 / 27.7ms |
   | (b) minas のみ修正 | 81.6 / 82.8ms | 28.1 / 29.5ms |
   | (c) 両方修正 | 84.8 / 83.6ms | 29.3 / 30.2ms |

   固定費 **~76ms/call** が消える（3 calls で −228ms、2 calls で −136ms）。(a)(b)(c) が
   同等 = 同じ 1 個の固定費への冗長な対策。
4. **揺れの目安**: `explore` 系は同一セッションで ±3ms 程度（上の #10 A/B）、
   `verify` 系は cargo の揺れで ±数百ms 動く。**1 回の観測で結論を出さない**
   （`-r 3` 以上 + 同一セッションでの交互測定）。

### warm の計時の内訳（iteration #9 の `MINAD_TRACE=1`。**デーモン側の値は #10/#11 でも不変**。
クライアント側は #10 で変わった — 下の注記）

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
  #10 前: minas --help 12.4ms / minas info 76.6ms（PATH 0.0.5 は 78.5ms）/ minas apply 91ms
          probe→本接続 68.4ms（単一接続 0.71ms）= これが #10 の固定費
  #10 後: minas info **9.1ms**、probe→本接続 **0.52ms**（単一接続 0.46ms）
          残るのは起動 ~12ms 程度（次の候補）
apply の往復（#11 のプローブ = Python で同じプロトコルを直叩き。warmup 済みの daemon）
  open#0   37ms / edit#0   3.4ms / save#0 **963ms** ← 初回だけ。背景 pull (~1030ms) のロック待ち
  open#1  2.4ms / edit#1   ~3ms  / save#1   ~10ms  ← 2 回目以降（L0 apply2: 1061 → 29ms と一致）
```

計測の内訳が知りたいとき: `MINAD_TRACE=1 python3 docs/loop/l0.py -f <flow> -r 1` の後、
`rg 'minad.trace' tmp/loop/<flow>-<arm>-*/.tmp/minad.log`。

### cold（1 回観測、`--cold`、**#11 の修正後**）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `verify/apply-check` | 2 | 275 | 383 | 8589 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 320 | 0 | True |
| `verify/hunks-cargo` | 2 | 169 | 338 | 325 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 340 | 0 | True |
| `verify-gap/apply-gap-check` | 3 | 291 | 523 | 8627 | 0 | True |
| `verify-gap/apply-gap-cargo` | 3 | 116 | 348 | 3200 | 0 | True |
| `explore/lsp` | 3 | 1085 | 1672 | 7743 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 31 | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 8207 | 0 | True |
| `rename/lsp` | 2 | 309 | 618 | 8907 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 424 | 0 | True |
| `verify-blind/check` | 2 | 269 | 373 | 6186 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 343 | 0 | True |
| `verify-broken/check` | 2 | 473 | 578 | 6111 | **0** | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 259 | 0 | True |

cold の支配項は**索引完走ゲート**（`symbol`/`hints`/`rename`/`check` が 6〜9 秒。
`apply` は cold でも 320〜430ms）。固定費の除去は cold でも同じだけ効くが、
この中に埋もれる。cold の wall は同じバイナリでも ±10% 揺れる（#2〜#11 の範囲）。

**#11 で解消**: `verify-broken/check` の fails は **0**（#10 では 1 = 既知の穴だった）。
cold の `check` も rc=2 + Syntax Error を返す（`--cold -f verify-broken -r 10` で 10/10）。
`--cold` の全 flow は **fails 0・silent 0**。

## 4. 次の課題設定（iteration #12）

> **iteration #11 の結末（この課題の起点）**: cold の `check` が構文エラーを取りこぼす
> 原因は「Open の背景タスクが `ensure` をまたいで編集前の全文を送り、サーバ側の文書が
> 巻き戻っていた」ことだった（ADR-0072）。送る直前に現在のテキストを取り直す修正で、
> cold の `verify-broken/check` は `-r 10` で 10/10 が rc=2 + Syntax Error。warm/cold の
> 全 flow は `calls`/`out_B`/`equiv_B` 不変・fails 0・486 green。
> その過程で見つけた**別の穴**（`minas apply` の初回 ~1.0s）がこの課題。

### 課題

**`minas apply` の初回だけ ~1.0s かかる（2 回目以降は ~30ms）。待ちは daemon の編集でも
解析でもなく、Save 応答の末尾にある watched-files 通知が背景 pull のセッションロックを
待っていること。**

実測（#11 の補助プローブ = Python で同じプロトコルを直叩き。warmup 済みの daemon）:

| 往復 | 初回 | 2 回目 |
|---|---|---|
| `Open` | 37ms | 2.4ms |
| `DocumentEdit` | 3.4ms | ~3ms |
| `Save` | **963ms** | ~10ms |

daemon 側の `MINAD_TRACE=1` では `edit total 2ms`、解析は背景（`sync.bg pull` ~1030ms）。
Save ハンドラは保存後に `notify_watched_files(...).await`（ADR-0061）を呼び、そこが
`timeout(LSP_LOCK_TIMEOUT, session.lock())`（最大 3 秒）を待つため、**背景 pull が
応答経路に戻る**（ADR-0055 の背景化が初回だけ無効化される）。同じ形は L0 にも出ている
（`apply2-cargo` の 1 回目 1061ms / 2 回目 29ms）。ADR-0061 は **2026-09-13 16:58 追加
= #8 の後**なので、#8 の「apply ~130ms」は warm（解析済み）での値。実エージェントの
編集は「初回」「大きめの編集で RA が再解析する」時にこの ~1s を払う。

### 仮説

- (a) `notify_watched_files` を Save 応答の経路から外せば（背景タスク化 or 遅延キュー）、
  Save は ~10ms になり、`apply` の初回 ~1.0s が ~30〜50ms になる。
- (b) 通知を `try_lock` で即諦める（落とす）のは不可 — ADR-0061 の「新規ファイル・新規
  ディレクトリを通知しないと `symbol`/`rename` が黙って部分的な答えを返す」穴が戻る。
  **落として良いのは「次の書き込みで再通知される」ときだけ**で、新規作成の 1 回きりの
  通知は落とすと穴になる（ADR-0061 の根拠そのもの）。
- (c) 通知を保留キューに積み、セッションロックが空いたときに送る（順序は問わない —
  通知は冪等で、RA は読み直すだけ）。

### 測り方（順序を守る）

1. **プローブで確定**: `MINAD_TRACE=1` で warm の `-f verify -a apply-check -r 1` を測り、
   Save の `write` が `sync.bg total` の後に出ることを確認。Python プローブ（#11 の
   `tmp/loop/probe_apply.py` を再利用）で Open/DocumentEdit/Save の往復を個別に測る
   （963ms を再現）。
2. **A/B で犯人を確定**: `notify_watched_files(...).await` を一時的に外したバイナリで
   Save の往復を測る（変わらなければ待ちは別物 — daemon ロック・ファイル I/O を疑う）。
3. **修正**: 通知を応答経路から外す。1 箇所（`Command::Save` の末尾の `await`）と
   `notify_watched_files` の呼び出し側。遅延させるなら「送るべき通知」を持っておき、
   次の Save/編集の didChange の前（同じセッションロックの中で）に送るのが
   順序も自然（同じ 1 つのセッションロックの中で通知してから同期）。
4. **契約の確認**: ADR-0061 の回帰テスト（watched files の通知が届く・新メンバーが
   見える）green、編集後追従の 3 件 green、cold の `verify-broken/check` が rc=2 の
   まま（#11 の契約）、`cargo test` **486 green**。
5. **効果確認**: L0 warm/cold の全 flow（`--log`）。初回 `apply` を含む arm
   （`verify/*`・`verify-gap/*`）の apply step が ~30〜50ms になり、
   `verify-gap/apply-gap-check` の apply + check の和が下がる。#8 の契約
   （apply + check = 222ms 級）が**解析が cold/warm どちらでも**成立するかを見る。
   `explore`（編集しない flow）と cold の探索 flow は不変のはず。
6. **→ 次反復 #13 の課題設定**: (i) `minas` 起動 12ms × calls（**debug ビルド依存の
   疑い。先に release で測る**）、(ii) `apply` の 3 往復を 1〜2 往復へ、(iii)
   `ServerMetrics` の穴（v20 bump）、(iv) L2（実 LLM）での確認（**下記の前提を参照**）。

### 受理条件 / 棄却条件

- **受理**: warm/cold の `verify/*`・`verify-gap/*` の apply step が、初回でも
  warm の 2 回目と同水準（~30〜50ms。今は 637〜1100ms）。`calls`/`out_B`/`equiv_B` は
  全 flow で不変・fails 0・486 green・ADR-0061 の通知契約は維持。
- **棄却**: (a) プローブで Save の待ちが別物（ファイル I/O・daemon ロック）と判明、
  (b) 通知を外す/遅らせると ADR-0061 の回帰テストが落ちる（「応答前に通知する」契約が
  実はあった）、(c) 待ちが別の場所へ移動するだけ（#7 の二重払いの再来。例えば
  `check` が同じ 1 秒を払い、和が変わらない）。

### 別候補（後回し）

- **`minas` 起動 ~12ms × calls**（`explore/lsp` 80ms のうち ~36ms）: 固定費除去後の
  残存項。**L0 は `target/debug`（デバッグビルド）を測っているので、この 12ms は
  ビルド構成の産物かもしれない**。先に release で測り、実コストなら起動の slim 化
  （clap の lazy 化・tokio の縮小）、debug 固有なら `method.md` に注記して棄却。
- **`minas apply` の 3 往復（Open + DocumentEdit + Save）を 1〜2 往復へ**: 固定費除去後は
  apply の wall の主項。契約（checksum/expected_text）の再設計が要る。
- **`ServerMetrics` の穴（rename カウンタ・`get_state_bytes`）**: `ServerInfo` の wire
  変更なので ADR-0039 では PROTOCOL_VERSION bump が要る（v18 の前例）。次に wire を
  変える用事と束ねて v20 として行う（ADR-0056）。
- **ギャップなし check の ~700ms**: `ensure` の `await_indexed` が背景 pull のロックを
  待つ時間で、解析は 1 回しか買われていない（#9 で確定）。削るには「背景 pull と check を
  同じ 1 回の要求に合流させる」設計変更が要る。#12 の修正で同じ形（Save も背景 pull の
  ロックを待つ）を消した後に再測すると、残る待ちがここだけになる。
- **L2（実 LLM）での確認**: **この環境では `opencode` が PATH に無い**（`~/.local/share/
  opencode/` の DB と auth.json は残っている）ので、先に CLI の復旧が要る。L2 の本題は
  規模（`calls`/`equiv_B` は外挿可・`wall_ms` は不可）と上位モデルでの再現。
- **`open_workspace_files`（rename/references の前に全ファイル didOpen）が必要かの再確認**:
  cold の主因は索引完走ゲートで、L0 の fixture では差が出ない（L2 向き）。
- **`apply --no-diagnostics`**: #8 で既定 apply が（warm では）~130ms になったので必要度は低い。
- **編集経路（`sync_after_edit` → `lsp::sync`）も spawn 時のテキストを送る**: 窓は sub-ms で
  実測の症状は無いが、#11 と同じ手（送る直前に取り直す）が使える（ADR-0072 の「残る同類の窓」）。

## 5. 次のセッションの起動手順

**新しいセッションの入口**（`AGENTS.md` から `docs/loop/README.md` に辿れる。
プロンプトテンプレート `.pi/prompts/dev_minas.md` = `/dev_minas` でも同じ指示を展開できる。
手で貼るならこれ）:

```text
minas/minad のループエンジニアリング（エージェントのコスト低減）の続きをやって。
まず docs/loop/method.md と docs/loop/latest.md を読み、
latest.md の §4（次の課題設定）から iteration #12 を回して。計測したら --log で
log.md に記録し、考察を追記し、latest.md を更新して。
```

**注意**: `tmp/loop/`（結果 JSON・fixture・`MINAD_TRACE` のログ）は checkout ごとの
gitignore 対象。worktree で測る場合はそちらに作られる（この文書群は git 管理なので
worktree にもある）。`minad` / `minas` を止めずに複数の l0 実行を重ねない
（索引・CPU が競合して `wall_ms` が濁る。`l0.py` はアームごとに専用 daemon を
立てるので、同時実行は避ける）。**手で起動した daemon は残る**（`minas` の自動起動は
`setsid` で切り離すため、スクリプト終了後も rust-analyzer ごと生き残り、以後の測定を
+60ms 汚染する。`(… &)` で起動したものも同様）。**測る前に**
`ps aux | grep -E "minad serve|rust-analyzer" | wc -l` **が 0 であることを確認する**
（macOS の `pgrep` に `-c` は無く、エラーを握り潰すと「0 個」に見える）。

```bash
cd /Users/335g/dev/other/mina
cargo build                                  # 計測対象は target/debug。コードを触ったら必須
cargo test                                   # 期待値: 486 passed
python3 docs/loop/l0.py --selftest          # 計測器の健全性
python3 docs/loop/l0.py -f verify -f explore -r 3 -v   # 現状確認（explore/lsp ~80ms・dump ~28ms）
MINAD_TRACE=1 python3 docs/loop/l0.py -f verify -a apply-check -r 1 -v  # #12 の課題（初回 apply）
                                                       # → Save の write が sync.bg total の後に出る
python3 docs/loop/l0.py --cold -f verify-broken -r 5   # #11 の契約（cold の check が rc=2 のまま）
python3 tmp/loop/probe_apply.py                        # #12 の課題（Open/Edit/Save の往復。
                                                       # #11 の使い捨てスクリプト。消えていたら
                                                       # method.md §6 の形で作り直す）
git log --oneline -5                         # 直前の変更を確認（auto-checkpoint が並ぶ）
```

そのあと §4 の「測り方」1 → 2 → … の順に進める。
`--log` は必ず付ける（JSON は `tmp/loop/` に落ちるだけなので、付け忘れると生データが消える）。
`apply` を含む比較は `-r 3` 必須・**同一セッションで交互に**測る（§3 の注意）。
**`l0.py` の step は解決済みの `minas` を実行する**（#10 で修正）ので、`cargo build` を
忘れると古いバイナリを測ることになる。

**反復番号の規則**: 今回の反復番号は §4 の見出しの番号（いまは **iteration #12**）。
結果は `log.md` に「iteration #12」として書き、考察の要約をこの `latest.md` に反映したうえで、
§4 を次の番号（#13）の課題設定に書き換える。§5 はこの手順のまま使う。
