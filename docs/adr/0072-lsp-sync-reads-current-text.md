# 背景タスクは LSP へ送る直前に現在のテキストを取り直す — 古い全文の didChange が文書を巻き戻す

iteration #10 で見つけた穴（L0 `--cold` の `verify-broken/check` が構文エラーを
確定できず `clean-unverified` を返す）の原因を iteration #11 で特定した。

## 実測（iteration #11）

- **再現**: cold の `minas apply`（`fn main() {` → `fn main( {`）→ `minas check` は
  ~5.4–6.3 秒待って
  `{"diagnostics":[],"settled":false,"verdict":"clean-unverified"}` を **rc=0** で返す
  （6/6）。**直後の 2 回目の `check` は 17ms で rc=2 + Syntax Error 2 件**を返す
  — サーバは正しい答えを持っているのに、1 回目が「未確認」で終わっていた。
- **pull は原因ではない**: 反復が `wait_ready`（`$/progress` の静止 300ms）を終えた
  直後の pull は、RA が idle でも **677ms かけて空**を返した。RA を直叩きした
  プローブ（`tmp/loop/probe_cold_pull.py`）では、cold の pull は索引中ずっと空で、
  `cachePriming`（Indexing）の終了直後に非空へ変わる — つまり古いテキストが
  入っていなければ空にならないタイミング。
- **決定打（一時的な DBG 計測。実装後に撤去）**: pull の直前に
  `->RA didChange main.rs v=3 len=240` が届いていた。**len=240 は編集前の全文**で、
  他の送信はすべて編集後（len=239）。この送信の直後の pull から空になり、
  `check` の早期確定（空 2 連続。ADR-0052）が成立して `clean-unverified` になった。
  2 回目の `check` が len=239 を送ると 1.2ms で構文エラー 2 件が返る。
- **送り主**: `Command::Open` の背景タスク（`daemon.rs`）。spawn 時に
  `text` を clone し、`ensure`（cold では索引完走待ちで数秒）の**後**に
  `lsp::open_document(&session, &path, &text_task, …)` を呼んでいた。didOpen 済みの
  文書では `did_open_inner` が `did_change`（全文同期）に落ちるため、**その数秒の間に
  入った編集がサーバ側で巻き戻る**。pull は巻き戻った（編集前の、正しい）文書に対して
  空を返すので、`check` の「空 2 連続 = 未確認」は正しい判定のまま誤った答えになる。

Status: accepted

## Decision

**`current_text_or(daemon, path, fallback)` を足し、Open の背景タスク 2 箇所
（既存文書の再利用経路と未開パスの新規経路）は `ensure` の後に現在のテキストを
取り直してから `open_document` に渡す。** 文書が閉じられていたら spawn 時の
テキストを使う（次の Open が正を運ぶ）。同じ理由で `settle_open_diagnostics_loop`
は前から毎ラウンド現在のテキストを読んでおり、その規律を LSP への didOpen 送信側にも
揃えたことになる。

## Considered Options

- **`pull_diagnostics_settled` の空の早期確定（ADR-0052）を cold で無効化する /
  索引ゲートを「対象ファイルの解析完了の観測」に変える**: 却下 — どちらも
  「空を確定と見なすまでの待ち方」を変えるだけで、**サーバが古いテキストを
  持っている**という原因を直さない。実測でも、待ちを延ばしても 5.4 秒経った pull が
  空のままで、次の `check` が即座に正解を返している（待ち時間の問題ではない）。
- **`LspSession` で「前回送ったテキストと同じなら送らない」ガード**:
  却下 — この競合はテキストが**違う**（古い）ので効かない。
- **didOpen/didChange を文書 revision で順序付ける（daemon が revision を持ち、
  LSP 層が古い書き込みを捨てる）**: 却下（今は）— API/wire を広げる割に、実測された
  競合は「数秒の窓」で、送る直前に取り直すだけで閉じる。順序付けが要るのは
  同種の窓が他で観測されてから。
- **Open の背景 settle から didOpen をやめる**（settle の pull は現在のテキストを
  読むので十分に見える）: 却下 — pull は `current_uri` が対象と一致しているときしか
  応えない（`pull_diagnostics` のゲート）。didOpen はその前提を作っている。

## Consequences

- cold の `verify-broken/check` が **rc=2 + Syntax Error 2 件**（`--cold
  -f verify-broken -r 10` で 10/10・fails 0）。cold の他 flow（`explore/lsp`・
  `hints`・`rename/lsp` = 索引完走ゲート依存）は**不変**（±10% 以内。cold の wall は
  ゲートが支配する）。
- warm の全 flow は不変（`calls`/`out_B`/`equiv_B` 同一・fails 0）。`verify-blind` の
  `settled:false` 維持、`verify-broken/cargo` の rc=101 維持。
- **回帰テスト**: `open_background_settle_sends_the_edited_text_not_the_snapshot`
  （`minad/src/daemon.rs`）。mock サーバの `MOCK_INIT_DELAY_MS` で `initialize` を
  1.5 秒遅らせて `ensure` を長くし、その間に編集を入れて「サーバが最終的に持つ
  テキスト」を診断位置（0 vs 4）で確かめる。**修正を戻すと失敗することを確認済み**。
- **残る同類の窓**: 編集経路（`sync_after_edit` → `lsp::sync`）も spawn 時のテキストを
  送るが、窓は `ensure` を挟まない sub-ms で、tokio Mutex の FIFO が順序を保つ
  （実測で症状は出ていない）。同じ手（送る直前に取り直す）が使えるが、観測されるまで
  触らない。
- wire 形状は変えないため `PROTOCOL_VERSION` は据え置き。

## 関連

- 実装: `minad/src/daemon.rs`（`current_text_or` / Open の背景タスク 2 箇所）、
  `mina-lsp/src/bin/mock_server.rs`（`MOCK_INIT_DELAY_MS`）
- 測定の記録: `docs/loop/log.md` iteration #11、`docs/loop/latest.md` §4
- 判定の既存 ADR: ADR-0051（索引完走ゲート）、ADR-0052（空の早期確定 — この修正後も
  「空は未確認」の意味は不変）、ADR-0045（空をクリーンの根拠にしない）、
  ADR-0068（didOpen 再送の禁止と didChange による更新）
