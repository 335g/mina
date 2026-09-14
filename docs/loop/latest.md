# minas/minad ループエンジニアリング — 最新の検討結果（次のループの起点）

> git 管理（`docs/loop/`）。更新: iteration #12 完了時（2026-09-14）。
> 入口は [`README.md`](./README.md)。検討方法は [`method.md`](./method.md)。
> **このファイルと method.md を読めば次のループを回せる。**
> 生データと全反復の考察は同じディレクトリの [`log.md`](./log.md)。
> 設計判断は `docs/adr/0051`（索引完走ゲート）/ `0052`（check の空の早期確定）/
> `0053`（編集後の settle を置かない ＋ 追記: pull は撤去できない）/
> `0054`（意味的リトライ待ち 0ms）/ `0055`（編集後 pull の背景化）/
> `0056`（計時ログ `MINAD_TRACE` — iteration #9）/ `0071`（peer uid 検査を accept
> ループの外へ — iteration #10）/ `0072`（LSP へ送る直前に現在のテキストを取り直す
> — iteration #11）/ **`0073`（watched-files 通知を Save 応答の経路から外す
> — iteration #12）** / `0045`（`settled` の意味）/ `0061`（watched-files 通知
> — #12 の追記あり）。

## 1. 現在の状態（30 秒サマリ）

- 完了した反復: **#1 計測器 → #2 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定
  （ADR-0052）→ #4 初回 apply の内訳確定 → #5 編集後の settle 撤去（ADR-0053）→
  #6 SEMANTIC_RETRY_WAIT 撤去（ADR-0054）→ #7 apply の pull 撤去（棄却。ADR-0053 追記）→
  #8 編集後 pull の背景化（ADR-0055）→ #9 計時ログ（ADR-0056）→
  #10 接続経路の固定費除去（ADR-0071）→ #11 cold の check の巻き戻し修正（ADR-0072）→
  #12 watched-files 通知を Save 応答から外す（ADR-0073）**
- 効果（L0 実測。**#10 で計測器を直したので §3 の表が新基準**）:
  - `check`（クリーン。**#3 当時の L0 flow**の値で、いまの §3 には同じ flow が無い —
    `verify-blind/check`（未確認な空）と `verify-broken/check`（構文エラー）が近い）
    **10154ms → 87ms（−99%）**（#3 + #6 + #8 で維持）
  - `symbol`（`explore/lsp` の 1 歩目）**583 → ~80ms（−86%）**（#6）
  - `explore/lsp`（3 calls の探索）**310 → 79.5ms（−74%）**、`explore/dump`
    **164 → 27.9ms（−83%）**（#10）
  - `minas` 呼び出しの**固定費 76.6ms → 9.1ms**（#10。`minas info` 実測 = 起動 + 接続 +
    1 往復。`minas --help` は 12.4ms で **別経路**（help 描画込み・接続なし）。この 2 つを
    混同しない — §3 の「起動 ~12ms」は前者から接続・往復を引いた残り）
  - `rename`（lsp）**1594 → ~1352–1398ms**（#6。残りは RA 側の WorkspaceEdit 計算）
  - **`apply` の初回 ~1.0s が消えた**（#12。Save 往復 **965 → 1.8ms**、L0 の apply step
    **1006–1123 → 53ms**）。ギャップありの apply + check の和 **~1140 → 71ms（−94%）**、
    `apply-cargo` **1202 → 183ms（−85%）**、`hunks-cargo` 1214 → 192ms、
    `apply2-cargo` 1192 → 216ms、apply ループ（`rename/apply`）1379 → 268ms
  - **ギャップなしの apply + check は和が不変**（~1010 → ~1017ms）。これは二重払いでは
    なく「同じ 1 回の RA 解析を誰が待つか」が変わっただけ（`check` が背景 pull の
    セッションロックを待つ。§3 の注記）
- cold の無言の誤り（空応答 + exit 0）は**新に出ていない**（`-r 10` も fails 0）。
  `calls` / `out_B` / `equiv_B` は全 flow で不変（回帰なし）。
- 全 **487 テスト** green（486 + #12 の回帰テスト 1）。`python3 docs/loop/l0.py --selftest` green。
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
- **#12 の結末**: `minas apply` の初回 ~1.0s の正体は、**Save 応答の末尾の watched-files
  通知（ADR-0061、09-13 追加 = #8 の後）が背景 pull（ADR-0055）のセッションロックを
  待っていた**こと（プローブで Open 37 / DocumentEdit 3.4 / **Save 965ms**。`MINAD_TRACE`
  でも Save の応答 `write` が `sync.bg` の後）。通知を背景タスクへ移して**採用**
  （ADR-0073）。Save 往復 **965 → 1.8ms**・L0 の apply step **1006–1123 → 53ms**。
  **通知の契約は維持**（mock の回帰テスト + ADR-0061 の受け入れ試験を再実行）。
- **#12 で分かった残りの構造**: 編集後の RA 解析（~900ms）は**誰かが 1 回だけ待つ**。
  ギャップなしの apply + check の和が不変なのはそのためで、二重払いではない
  （#9 で `check total 686 = borrow 682 + pull 2` と確定済み）。残る大項は RA 側
  （`rename request` 898ms・`hints` 578ms・編集後解析 ~900ms・cold の索引 6〜9s）と、
  `minas` 起動 **~12ms × calls**（debug ビルドでの値）。
- **次の課題（#13）は `minas` 起動の固定費を release で測ること**（12ms は debug の産物か
  実コストか）。詳細と受理・棄却条件は §4。

## 2. 確定した事実（再測定は不要）

| 事実 | 根拠 |
|---|---|
| 読む量＝費消の主因。範囲 read（`symbol`→`at`→`read --lines`）は全文 dump より `equiv_B` **−82%**（**1672 vs 9260**。同一セッションの §3 の値。当時の 1568 は探索応答の内容が違う — `symbol_search_bytes` 69 vs 200） | L0 `explore`（iteration #1〜#3 で安定） |
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
| **`minas apply` の初回 ~1.0s は Save 往復**（プローブで Open 37ms / DocumentEdit 3.4ms / **Save 963ms**。daemon の `edit total` は 2ms、背景 `sync.bg pull` は ~1030ms）。Save 末尾の ADR-0061 の watched-files 通知が `session.lock()`（最大 3s）を待つため、背景 pull が応答経路に戻る。2 回目以降は 29ms。→ **#12 で修正**（通知を背景化。Save **965 → 1.8ms**、L0 の apply step **1006–1123 → 53ms**） | iteration #11 の補助プローブ（`tmp/loop/probe_apply.py`）/ L0 `apply2-cargo`（1061 → 29ms）/ ADR-0061（09-13 16:58 追加 = **#8 の後**）/ #12（ADR-0073） |
| pull が**見る**: 構文エラー / 同一ファイル内の型エラー（`no such field` 等）。**見ない**: メソッド解決エラー（`cfg.validate2()` は永久に空）、クロスファイル型エラー | 同上。だから `check` の空は `settled:false`（未確認）で、`cargo` が最終的な根拠 |
| 空応答の settle 予算（20×500ms）と Open の 30 秒 settle は「同じ答えしか返らない待ち」 | ADR-0052 |
| **編集後の固定 settle（`PULL_SETTLE`）は撤去できる**（ADR-0053）。250ms → 0ms で `apply` が毎回 −250ms、空 pull 回帰は `-r 10` × 3 flow = 30 run で 0 件 | iteration #5（L0 + プローブ直叩き） |
| **初回 apply の ~1.1s は RA の初回再解析であって待ちではない**: 同じ daemon で `symbol`/`check` を先に叩いてから `apply` すると **137ms / 98ms / 102ms** | iteration #5 の下調べ（`tmp/loop/recon6.py`）+ #6/#7 でも不変。**#11 で精密化**: daemon の編集処理は 2msで、解析は背景（ADR-0055）。**CLI の apply が ~1.0s かかっていたのは Save 末尾の watched-files 通知が背景 pull のセッションロックを待っていたため**（ADR-0061 以降。#12 で通知を背景化して解消 — ADR-0073） |
| **`SEMANTIC_RETRY_WAIT` 500ms の撤去は回帰ゼロ**（0ms。ADR-0054）。「2 回連続同一」の確認自体は同一クエリの再送でほぼ無料 | iteration #6（warm -r3 全 flow + cold + -r10 で fails 0） |
| **「2 回連続同一」の確認の実測コストは 2ms**（rename の `lsp.retry` attempt1 912ms / **attempt2 2ms**） | iteration #9（計時ログ） |
| **`rename` の残り ~900ms は RA 側の WorkspaceEdit 計算**（`rename request` = 898ms。prepare 2 / resolve 12 / convert 2 / apply 2） | iteration #9（計時ログ）/ #6 の案 B 比較 |
| **`hints` の ~580ms は `pull_inlay_hints` の RA 側計算**（`hints total 578ms = pull 578`）。キャッシュヒット時は ~0ms | iteration #9（計時ログ）/ #6 |
| **`apply` の ~1.1s は「pull が強制する RA 解析」そのもの**。診断 pull だけ外しても hint pull が同じ解析を買うので `apply` は不変（両方外すと `apply` 153ms だが `check` が 1122ms を払う） | iteration #7 A/B(a)(b) |
| **編集後の pull は契約**（daemon のスナップショットが編集後診断・ヒントを反映する）。外すとテスト 3 件が落ちる | iteration #7（`cargo test`） |
| **cargo 検証経路では `apply` の先払い解析が二重払い**（RA 解析と rustc コンパイルは別物）。pull を外すと apply-cargo **1344→268ms**、apply2-cargo **1524→361ms**、hunks-cargo **1450→275ms** | iteration #7 A/B(b) |
| **編集後 pull の背景化で Save 応答の解析待ちが消える**（ADR-0055）。apply の wall は ~130ms で安定。ギャップあり（sleep 3 = 推論遅延の代理）で apply + check の和 **222ms（現行比 −82%）** | iteration #8（L0 `verify-gap` 追加。warm -r3）。ADR-0061（09-13）以降は初回の apply が Save の watched-files 通知で ~1.0s に戻っていたが、**#12 で通知を背景化して再び解析済み相当になった**（ADR-0073。ギャップありの和 **1140 → 71ms**） |
| **ギャップなし（即時 check）では、check は「背景 pull が買った 1 回の解析」をセッションロック越しに待つ**（`check total` 686ms = `borrow` 682（`ensure` の `await_indexed` が `session.lock()` を待つ）+ `pull` 2ms。背景 pull が先に終わっていれば `check total` **3ms**、ギャップありなら **5ms**）。**check 自身は解析を買っていない** | iteration #9（計時ログ・n=12 中央値）。#8 の「check が解析を買う」は言い過ぎだった（下記の修正）。**#12 後のギャップなし apply + check はこの形そのもの**: apply 54ms + check ~919ms（= 背景 pull のロック待ち）で和は ~1017ms。**待ちは Save から check へ移っただけで、解析は 1 回のまま**（棄却条件 (c) の「二重払い」ではない） |
| **編集後 pull の背景タスクの内訳**: `didChange` 2ms → 診断 pull 892ms（= RA 解析）→ ヒント pull 6ms → 反映・push 0ms。`didChange` が 300〜650ms になるのは前の背景 pull のロック待ち（応答はブロックしない） | iteration #9（計時ログ） |
| **`minas` の 1 回の起動でデーモン側は 0.27ms で答える**（Python で同一プロトコルを叩いた実測。GetServerInfo） | iteration #9（`tmp/loop/probe-client/`） |
| **接続経路の固定費 ~68ms の正体は accept ループ内の peer uid 検査**（捨て接続を accept すると ENOTCONN で 5ms×10 sleep し、その間 accept が止まる）。検査を接続タスクの先頭へ移す (a) か、捨て接続をやめる (b) のどちらかで消える（probe→本接続 **68.4 → 0.52ms**。L0 `explore/lsp` **310 → 82ms**・`dump` **164 → 28ms**。 (a)(b)(c) 同等・2 ラウンド再現） | iteration #10（ADR-0071。プローブ + A/B） |
| **`minas` 1 呼び出しの固定費は 76.6 → 9.1ms になった**（`minas info`、baseline デーモン相手。PATH の 0.0.5 も 78.5ms = 同じ旧経路）| iteration #10（ADR-0071） |
| **L0 の step コマンドは PATH の `minas` を実行していた**（`L0_MINAS` は `server_metrics` にしか効かず、step は `"minas …"` の文字列を shell に渡していた）。つまり**クライアント側の変更は L0 の step で一度も測られていなかった**（#10 で発見し `l0.py` の `run()` を修正） | iteration #10（A/B の (b) が効いて見えなかった原因）/ ADR-0071 |
| **手動テストで起動した daemon は残る**（`minas` の自動起動は `setsid` で切り離す。`(… &)` で起動したものも同様）。残ると rust-analyzer ごと CPU を食い、`minas symbol` が 12ms のはずが 90ms に見える。**測る前に `ps aux \| grep -E "minad serve\|rust-analyzer" \| wc -l` を 0 にする**（macOS の `pgrep -c` は無い） | iteration #10（実際に 3 個の stray で汚染された） |
| **Save の応答は「通知を送ってから」である必要が無い**（通知は冪等で、順序は「次の LSP 要求より先」で足りる）。応答経路で `session.lock()` を待つと、背景 pull（ADR-0055）が応答に戻る — #12 の原因 | iteration #12（ADR-0073）/ `MINAD_TRACE`（Save の応答 `write` が `sync.bg` の後）|
| **編集後 pull の背景タスクと `check` は「同じ 1 回の解析」を共有している**（どちらもセッションロック越し）。したがって「どちらが待つか」を変えても**和は変わらない**（ギャップありなら重ならないので和が下がる） | iteration #12（L0 `verify/apply-check` と `verify-gap/apply-gap-check` の A/B）/ #9 |
| **`minas` 1 呼び出しの ~12ms は debug ビルドでの値**（`minas --help` 12.4ms / `minas info` 9.1ms。L0 は `target/debug` を測る）。release での値は未測定 — §4 の #13 | iteration #9/#10（クライアント側プローブ）/ method.md §7 の「実処理 + calls×12ms」という読み方の前提 |
| **watched-files 通知の送信を背景化しても ADR-0061 の目的（新メンバーが解析に入る）は満たされる**（稼働中に `minas apply` で `crates/c` を作り、6 秒後に `minas symbol` が `c_helper` を返す） | iteration #12（`tmp/loop/probe_watch_member.py`）/ mock 回帰テスト `save_does_not_wait_for_the_background_pull_before_notifying_watched_files` |
| ~~cold の `verify-broken/check` は `clean-unverified`~~ → **#11 で修正済み**（原因は送信直前の取り直し漏れ。ADR-0072）。cold でも rc=2 + Syntax Error（`-r 10` で 10/10） | iteration #10（発見）/ #11（修正・ADR-0072） |
| **Open の背景タスクは `ensure` をまたいで spawn 時のテキストを送ってはならない**（送る直前の取り直しが規律。`settle_open_diagnostics_loop` は以前から毎ラウンド読み直していた） | iteration #11（ADR-0072）/ 回帰テスト `open_background_settle_sends_the_edited_text_not_the_snapshot`（mock の `MOCK_INIT_DELAY_MS` で initialize を 1.5s 遅らせる。修正を戻すと失敗することを確認） |
| **cold の `check` の wall は索引完走ゲートが支配**（5.4〜6.3s）。#11 の修正で +0.4s 以内（±10%）、探索 flow も ±10% 以内 | iteration #11（`--cold` 全 flow） |
| 編集後追従のテスト 3 件は「応答時点」で検証していた（#8 の前提誤り）。検証ポイントを「追従までの poll（最大 10 秒）」に移して契約を維持 | iteration #8（ADR-0055） |
| **計時ログ（`MINAD_TRACE=1`）で 1 コマンドの span/phase/ms が取れる**（既定 off、wire・PROTOCOL_VERSION 不変、回帰なし） | iteration #9（ADR-0056） |
| cold の支配項は**索引完走ゲート**（`symbol`/`hints`/`rename`/`check` が 6〜9 秒。`apply` は cold でも 310〜420ms = 索引ゲートに隠れる） | L0 `--cold`（#2〜#12 で安定。§3 の cold 表） |

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
| **watched-files 通知を `try_lock` で即諦める（落とす）** | iteration #12: ADR-0061 の穴（新規ディレクトリ・新メンバーの 1 回きりの通知）が戻る。「次の書き込みで再通知される」ときだけ落として良い。背景タスク + 既存の `timeout(3s)` を維持（ADR-0073） |
| **通知を保留キューに積み、次の Save/編集の `didChange` の前（同じセッションロックの中）にまとめて送る** | iteration #12: 編集が来ないまま次の意味クエリが来ると通知が遅れ続ける（新規メンバーの `symbol` が空のまま）。背景タスクならロックが空いた時点で送られる。キュー + ポンプは実測で要求されたら（ADR-0073） |
| **ギャップなし check の ~700–900ms を「背景 pull と check の合流」で削る** | iteration #12: 両者は同じ 1 回の RA 解析をセッションロック越しに共有している（#9）ので、待つ側を変えても和は変わらない（解析は 1 回）。削るには「編集後診断を誰も待たない」契約変更（= #7 の棄却）か RA の高速化。優先度を下げる（§4 の別候補） |
| **計時（trace）を入れると wall が歪む** | iteration #9: trace on/off の同一条件比較（verify、n=9 ずつ）で apply-check **−5.3%**・apply-cargo −2.2%（minas の step のみの arm）。既定 off では `Instant::now()` 1 回と分岐だけ |

## 3. 基準値（L0）— 次回の比較はここから

### warm（r=3 中央値、`-r 3 --log` の全 flow、**#12 の修正後**）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `verify/apply-check` | 2 | 275 | 383 | 1017 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | **183** | 0 | True |
| `verify/hunks-cargo` | 2 | 169 | 338 | **192** | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | **216** | 0 | True |
| `verify-gap/apply-gap-check` | 3* | 291 | 523 | **3087** | 0 | True |
| `verify-gap/apply-gap-cargo` | 3* | 116 | 348 | **3203** | 0 | True |
| `verify-broken/check` | 2 | 473 | 578 | 682 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 127 | 0 | True |
| `verify-blind/check` | 2 | 269 | 373 | 656 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 191 | 0 | True |
| `explore/lsp` | 3 | 1085 | 1672 | **79.5** | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | **27.9** | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 648 | 0 | True |
| `rename/lsp` | 2 | 309 | 618 | 1352 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | **268** | 0 | True |

`out_B` の ±2〜数バイトは、応答に**fixture の絶対パス**が入るため（`tmp/loop/<flow>-<arm>-<時刻>-<pid>/…` の長さが違う）。`calls`/`equiv_B` は不変。**#12 で apply を含む arm の wall が大きく下がった**（初回 apply の ~1.0s が消えた）。

*verify-gap の calls=3 は計測 step の `sleep 3`（エージェントの次ターン推論遅延の
代理）を含むため。契約の往復は apply + check/cargo の 2 回のまま。**表の wall は
`sleep 3` を含む**ので、「apply + check の和」は **wall − `sleep 3` の実測**で読む
（#12 の A/B: 3087 − 3014 = 73ms ≒ apply 53 + check 18。`-v` の step 別 wall が確実）。

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
4. **apply の前後（#12 の A/B、同一セッションで before→after→before、`L0_MINAD` で
   修正前バイナリを指す。apply **step** の wall = `-r 3` 中央値）**:

   | arm / step | before | after |
   |---|---|---|
   | `verify/apply-check` の `apply` | 1006 / 1010ms | **54ms** |
   | `verify/apply-check` の `check` | 17ms | 919ms（背景 pull のロック待ち） |
   | （和） | ~1029ms | ~1017ms（**不変**） |
   | `verify/apply2-cargo` の `apply`#1 / #2 | 1010 / 30ms | 53 / 30ms |
   | `verify-gap/apply-gap-check` の `apply` | 1123ms | **53ms** |
   | （ギャップありの和 apply + check） | **1140ms** | **71ms（−94%）** |

5. **揺れの目安**: `explore` 系は同一セッションで ±3ms 程度（上の #10 A/B）、
   `verify` 系は cargo の揺れで ±数百ms 動く。**1 回の観測で結論を出さない**
   （`-r 3` 以上 + 同一セッションでの交互測定）。

### warm の計時の内訳（iteration #9 の `MINAD_TRACE=1`。**デーモン側の値は #10〜#12 でも不変**。
変わったのはクライアント側（#10）と **Save 応答の末尾（#12 で通知が背景へ）** — 下の注記）

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
  #12 前: open#0 37ms / edit#0 3.4ms / save#0 **963ms**（背景 pull ~1030ms のロック待ち）
          open#1 2.4ms / edit#1 ~3ms / save#1 ~10ms
  #12 後: open#0 37ms / edit#0 3.4ms / save#0 **1.8ms**（通知を背景化。3 回再現）
          open#1 1.5ms / edit#1 2.9ms / save#1 1.5ms
  trace（#12 後）: `sync.bg` はプローブ終了までに現れない = 応答が解析を待っていない
```

計測の内訳が知りたいとき: `MINAD_TRACE=1 python3 docs/loop/l0.py -f <flow> -r 1` の後、
`rg 'minad.trace' tmp/loop/<flow>-<arm>-*/.tmp/minad.log`。

### cold（1 回観測、`--cold`、**#12 の修正後**）

| flow/arm | calls | out_B | equiv_B | wall_ms | fails | ok |
|---|---|---|---|---|---|---|
| `verify/apply-check` | 2 | 275 | 383 | 8672 | 0 | True |
| `verify/apply-cargo` | 2 | 108 | 216 | 313 | 0 | True |
| `verify/hunks-cargo` | 2 | 169 | 338 | 313 | 0 | True |
| `verify/apply2-cargo` | 3 | 218 | 545 | 336 | 0 | True |
| `verify-gap/apply-gap-check` | 3 | 291 | 523 | 8607 | 0 | True |
| `verify-gap/apply-gap-cargo` | 3 | 116 | 348 | 3203* | 0 | True |
| `explore/lsp` | 3 | 1085 | 1672 | 7196 | 0 | True |
| `explore/dump` | 2 | 4750 | 9260 | 30 | 0 | True |
| `hints/hints` | 1 | 110 | 110 | 8138 | 0 | True |
| `rename/lsp` | 2 | 309 | 618 | 8879 | 0 | True |
| `rename/apply` | 5 | 406 | 1424 | 412 | 0 | True |
| `verify-blind/check` | 2 | 269 | 373 | 6118 | 0 | True |
| `verify-blind/cargo` | 2 | 104 | 208 | 332 | 0 | True |
| `verify-broken/check` | 2 | 473 | 578 | 5754 | 0 | True |
| `verify-broken/cargo` | 2 | 105 | 210 | 223 | 0 | True |

*cold の `apply-gap-cargo` の 3203ms は **cargo step が速かった場合の観測**（再測で
3619 / 3964ms。cargo が 140 → 803ms に振れる）。cold の wall は cargo の揺れも含む。

cold の支配項は**索引完走ゲート**（`symbol`/`hints`/`rename`/`check` が 6〜9 秒。
`apply` は cold でも 320〜430ms）。固定費の除去は cold でも同じだけ効くが、
この中に埋もれる。cold の wall は同じバイナリでも ±10% 揺れる（#2〜#12 の範囲）。
**#12 の修正は cold の wall を変えない**（apply の待ちは cold では索引ゲートに隠れる）。

**#11 で解消**: `verify-broken/check` の fails は **0**（#10 では 1 = 既知の穴だった）。
cold の `check` も rc=2 + Syntax Error を返す（`--cold -f verify-broken -r 10` で 10/10）。
`--cold` の全 flow は **fails 0・silent 0**（#12 でも `-r 5` で再確認）。

## 4. 次の課題設定（iteration #13）

> **iteration #12 の結末（この課題の起点）**: `minas apply` の初回 ~1.0s の正体は
> **Save 応答の末尾の watched-files 通知（ADR-0061）が背景 pull（ADR-0055）の
> セッションロックを待っていた**ことだった。通知を背景タスクへ移して解決（ADR-0073）。
> Save 往復 **965 → 1.8ms**、L0 の apply step **1006–1123 → 53ms**、ギャップありの
> apply + check **1140 → 71ms**、`apply-cargo` **1202 → 183ms**、`rename/apply`
> **1379 → 268ms**。`calls`/`out_B`/`equiv_B` は全 flow で不変・fails 0・**487 green**・
> ADR-0061 の通知契約は維持（mock 回帰テスト + 受け入れ試験）。
> 同時に**ギャップなしの apply + check の和が不変**（~1017ms）ことも確定した — 同じ
> 1 回の RA 解析を `check` が待つだけで、二重払いではない（#9 の計時と一致）。
> **残る大項は RA 側の解析そのもの**（`rename request` 898ms・`hints` 578ms・編集後解析
> ~900ms・cold の索引 6〜9s）と、`minas` 起動 **~12ms × calls**。

### 課題

**`minas` 1 呼び出しの固定費のうち「起動 ~12ms」は debug ビルド（`target/debug`）の産物か、
実コストか。** L0 は `target/debug` を測っているが、エージェントが実際に走らせるのは
インストール済みの release バイナリ。12ms が実コストなら起動の slim 化（clap の遅延構築・
tokio の feature 縮小）に余地があり、debug 固有なら**計測器の読み方**（method.md §7 の
「実処理 + calls×12ms + 往復数×~10ms」）と §3 の注記を release の値で書き直す。
どちらに転んでも次の判断材料になる — `calls` を減らす契約変更（下の別候補 `apply --check`）の
価値がこの値に依存する。

実測（#9/#10 のクライアントプローブ、debug ビルド）:

| 経路 | debug 実測 | 何を含むか |
|---|---|---|
| `minas --help` | 12.4ms | 起動 + clap 構築 + **help 描画** + stdout（接続なし） |
| `minas info` | **9.1ms** | 起動 + clap + 接続 + 1 往復 + 応答 |
| daemon の応答そのもの（Python 直叩き） | 0.27ms | — |
| probe→本接続（接続経路） | 0.52ms | — |
| `explore/lsp`（3 calls） | 79.5ms | うち ~24–33ms が 3 ×（起動 + 接続） |

`--help` の方が `info` より遅い（help 描画が載る）ので、**`--help` を「起動費」と読まない**。
起動費の推定は `info −（接続 0.52 + daemon 0.27）` ≒ **~8ms/呼び出し**。L0 の
`explore/lsp`（3 calls）の積み上げ ~24–33ms と整合する。

### 仮説

- (a) debug の起動費は release の 2〜4 倍（debug ビルドは起動時の初期化と未最適化の
  走査が重い）。release では 2〜4ms/呼び出しになり、**`minas info` は 9.1 → 3〜5ms、
  `explore/lsp` は 79.5 → 62〜70ms**（受理条件は 70ms 以下）。
- (b) release でも ~12ms が残る → 実コスト。内訳（動的リンク・clap の構築・tokio ランタイム
  起動）を分けて slim 化する（1 変更ずつ A/B）。
- (c) 差が**ノイズ床以下**（手順 0〜2 で実測。`explore` 系の同一バイナリ反復は ±3ms 程度）
  → ビルド構成に依存しない実コストとして扱い、slim 化に進む。

### 測り方（順序を守る）

0. **汚染除去**: `ps aux | grep -E "minad serve|rust-analyzer" | wc -l` が **0** であることを
   確認する（残った daemon / RA は wall を +60ms 汚染する。method.md §7）。以降の測定は
   すべてこの状態で。
1. `cargo build --release`（`target/release/{minas,minad}`。**既存の release バイナリは
   9/13 のビルドなので使わない**）。
2. **起動費プローブ**（`docs/loop/probe_startup.py`。消えていたら method.md §6 の形で作り直す）:
   ```bash
   python3 docs/loop/probe_startup.py 9            # --help / info を debug と release で交互に n=9
   L0_MINAS=target/debug/minas python3 docs/loop/probe_startup.py 9   # ノイズ床（同一バイナリ）
   ```
   同一バイナリを両方の枠に入れて差が出るならプローブが偏っている。**この幅がノイズ床**
   （受理・棄却の判定はこの値で行う）。
3. **L0 を release で走らせる**（固定費が支配的な flow から）:
   ```bash
   L0_MINAS=target/release/minas L0_MINAD=target/release/minad \
     python3 docs/loop/l0.py -f explore -f hints -r 5 -v
   L0_MINAS=target/release/minas L0_MINAD=target/release/minad \
     python3 docs/loop/l0.py -f verify -r 3 -v        # apply を含む比較は -r 3 必須
   ```
   `calls`/`out_B`/`equiv_B` は debug と一致するはず（契約は同じ）。apply を含む flow の差が
   「calls × 起動費」で説明できるかを見る（説明できない差は別の debug 依存 = 計測器の穴）。
4. **内訳**: 起動費の推定は `info −（接続 0.52 + daemon 0.27）`。`--help` は help 描画込みの
   **上限の参考値**としてだけ使う（`--help` > `info` になるので引き算に使わない）。
   release で `info` が ~3ms になっていれば、残るのは接続・往復・実処理。
5. **（実コストなら）slim 化**: 1 変更ずつ A/B する。候補は (i) clap の遅延構築
   （`--help` と `info` の差 = help 描画の寄与を見る）、(ii) tokio の feature 縮小
   （`minas/Cargo.toml` は `tokio.workspace = true`）、(iii) 動的リンク / `strip`、
   (iv) `[profile.release]`（root `Cargo.toml` には無い = 既定 lto なし / strip なし）。
   **`MINAD_TRACE` は daemon 側の span なのでクライアント起動には効かない** — 内訳は
   プローブの定点（`--help` / `info` / 接続だけ）で見る。
6. **→ 次反復 #14 の課題設定**: 下の別候補（特に `apply --check` の往復削減）。

### 受理条件 / 棄却条件

判定は**手順 2 で測ったノイズ床**（同一バイナリ反復の幅。`explore` 系は ±3ms 程度）を
基準にする。「±1ms 以内」のような分解能以下の閾値は使わない。

- **受理（性能）**: release の `minas info` の中央値が debug より**ノイズ床を超えて**速く
  （例 9.1 → <6ms）、かつ L0 の `explore/lsp`（3 calls）が calls 比例で下がる
  （**79.5 → 70ms 以下**）。`calls`/`out_B`/`equiv_B` は全 flow で debug と同じ・fails 0・
  **487 green**。
- **受理（計測器の較正として）**: release と debug の差が**ノイズ床以下** → 「起動費は
  ビルド構成に依存しない実コスト」と確定し、method.md §7 と §3 の注記を release の値で
  書き直す（#10 の「PATH の minas を測っていた」穴と同じクラスの較正）。
- **棄却**: release バイナリで L0 が成立しない（fixture か daemon が動かない、または
  `calls`/`out_B`/`equiv_B` が debug と一致しない = release で契約が変わる）。この場合は
  計測器の欠陥として記録し、release の手順を作り直す。
- **棄却（slim 化に進んだ場合）**: 1 変更ずつの A/B で L0 の `calls` が変わらない・
  `explore` の wall がノイズ床の中にある（= 起動費が実際には支配項ではない）。

### 別候補（後回し）

- **`minas apply --check`（apply + check を 1 往復に）**: verify の契約が「apply（1 往復）→
  check（1 往復）」の 2 往復を強制している。apply は既に編集後 pull を買っているので、同じ
  接続で check まで行い診断を 1 つの応答に載せれば **`calls` 2 → 1**（`verify/apply-check` の
  `equiv_B` 383 → ~275）。wall は ~1s のまま（解析は 1 回。誰が待つかが変わるだけ）。
  1 往復削減は `--hunks-stdin` と同じ形の勝ちで、**#13 の起動費が実コストなら価値が上がる**
  （`calls` × C_0 の項が 1 つ減る）。実装は「同じ接続で `Command::Check` まで続けて撃ち、
  2 つの応答を 1 つの出力にまとめる」CLI 側の変更が中心（`minas` の headless 経路は
  いま 1 接続 1 コマンドなので、そこを広げる必要がある。wire の追加は不要 —
  `Command::Check` は既にある。**要検証**: 出力形式と、check の `settled` / rc を
  CLI の exit code にどう写すか）。
- **`minas apply` の 3 往復（Open + DocumentEdit + Save）を 1〜2 往復へ**: #12 後の apply の
  wall 53ms の主項（起動 12ms + 3 往復 ~7ms + 書き込み・検証・応答）。1 往復あたり ~2–4ms で、
  契約（checksum/expected_text）の再設計が要る。
- **`ServerMetrics` の穴（rename カウンタ・`get_state_bytes`）**: `ServerInfo` の wire 変更
  なので ADR-0039 の bump が要る（v18 の前例）。次に wire を変える用事と束ねて v20（ADR-0056）。
- **ギャップなし check の ~700–900ms**: 背景 pull と**同じ 1 回の解析**をセッションロック
  越しに共有している（#9 の `check total 686 = borrow 682 + pull 2`）ので、「合流」させても
  和は変わらない。削るには「編集後診断を誰も待たない」契約変更（= #7 の棄却）か RA の高速化。
  **優先度は低い**（#12 でそう確定した）。
- **L2（実 LLM）での確認**: この環境では `opencode` が PATH に無い（DB と auth.json は残存）。
  復旧後、#12 のギャップ arm（apply → 思考 → check）が実 LLM でも効くことを見る。L2 の本題は
  規模（`calls`/`equiv_B` は外挿可・`wall_ms` は不可）と上位モデルでの再現。
- **`open_workspace_files`（rename/references の前に全ファイル didOpen）が必要かの再確認**:
  cold の主因は索引完走ゲートで、L0 の fixture では差が出ない（L2 向き）。
- **`apply --no-diagnostics`**: #12 で apply が warm でも 53ms になったので必要度は下がった。
- **編集経路（`sync_after_edit` → `lsp::sync`）も spawn 時のテキストを送る**: 窓は sub-ms で
  実測の症状は無いが、#11 と同じ手（送る直前に取り直す）が使える（ADR-0072 の「残る同類の窓」）。

## 5. 次のセッションの起動手順

**新しいセッションの入口**（`AGENTS.md` から `docs/loop/README.md` に辿れる。
プロンプトテンプレート `.pi/prompts/dev_minas.md` = `/dev_minas` でも同じ指示を展開できる。
手で貼るならこれ）:

```text
minas/minad のループエンジニアリング（エージェントのコスト低減）の続きをやって。
まず docs/loop/method.md と docs/loop/latest.md を読み、
latest.md の §4（次の課題設定）から iteration #13 を回して。計測したら --log で
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
cargo test                                   # 期待値: 487 passed
python3 docs/loop/l0.py --selftest          # 計測器の健全性
python3 docs/loop/l0.py -f verify -f explore -r 3 -v   # 現状確認（explore/lsp ~80ms・dump ~28ms）
                                                       # apply を含む arm は #12 で ~180–270ms になった
python3 docs/loop/l0.py -f verify -a apply2-cargo -r 3 -v  # apply step が 53ms 前後（初回でも）
python3 tmp/loop/probe_apply.py                        # Open/Edit/Save の往復（save#0 が ~2ms）
python3 docs/loop/l0.py --cold -f verify-broken -r 5   # #11 の契約（cold の check が rc=2 のまま）
git log --oneline -5                         # 直前の変更を確認（auto-checkpoint が並ぶ）

# #13 の課題（release の固定費）
cargo build --release                        # target/release/{minas,minad}
#   debug/release を同一セッションで交互に測る（クライアントプローブ。#9 の
#   tmp/loop/probe-client/ の形。消えていたら method.md §6 の形で作り直す）
L0_MINAS=target/release/minas L0_MINAD=target/release/minad \
  python3 docs/loop/l0.py -f explore -f hints -r 5 -v   # 固定費が支配的な flow を release で
#   → explore/lsp 79.5ms のうち ~36ms（3 calls × 12ms）がどうなるかを見る
```

そのあと §4 の「測り方」1 → 2 → … の順に進める。
`--log` は必ず付ける（JSON は `tmp/loop/` に落ちるだけなので、付け忘れると生データが消える）。
`apply` を含む比較は `-r 3` 必須・**同一セッションで交互に**測る（§3 の注意）。
**`l0.py` の step は解決済みの `minas` を実行する**（#10 で修正）ので、`cargo build` を
忘れると古いバイナリを測ることになる。

**反復番号の規則**: 今回の反復番号は §4 の見出しの番号（いまは **iteration #13**）。
結果は `log.md` に「iteration #13」として書き、考察の要約をこの `latest.md` に反映したうえで、
§4 を次の番号（#14）の課題設定に書き換える。§5 はこの手順のまま使う。
