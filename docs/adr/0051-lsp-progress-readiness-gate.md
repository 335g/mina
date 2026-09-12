# LSP の索引完走(work-done progress)を待ってから索引依存の要求を出す

L0 計測(2026-09-12, `docs/gitignore/loop/l0.py --cold`)で、daemon が索引未完の
言語サーバへ要求を出すと**無言の誤り**が出ることを実測した:

- `minas symbol <path> <query>` が `[]` を返す(exit 0)。「見つからない」と
  区別できない。2 回目の呼び出しでは当たる
- `minas rename <path> <old> <new>` が**別ファイルの出現を取りこぼしたまま
  「成功」を報告**する(`N files, M edits` が小さく出る)。`cargo check` で
  初めて E0609 として検出される
- `minas check <path>` が `LSP error: diagnostics pull failed (server dead or
  session lock timeout)` を exit 2 で返す。実体は「まだ索引中」(メッセージが
  原因を誤って伝えるため、エージェントは回復手順を誤る)

原因は 1 箇所: daemon は rust-analyzer の `$/progress`(索引の進捗)を購読して
いなかった。さらに `window.workDoneProgress: true` を advertise していない
ため、**rust-analyzer は進捗を 1 件も送っていなかった**(実測: advertise すると
`rustAnalyzer/Fetching` → `Building CrateGraph` → `Roots Scanned` →
`cachePriming`(title "Indexing")を begin/end 付きで送る)。daemon には
「サーバがまだ仕事中」を知る手段が無く、索引未完の応答を準備完了として扱って
いた。ADR-0045 が「解析完了を LSP 応答から確実に検知する手段は無い」としたのは
この購読が無かったことによる(進捗を購読すれば検知できる)。

Status: accepted

## Decision

1. **`window.workDoneProgress: true` を advertise する**(`minad/src/lsp.rs` の
   initialize パラメータ)。advertise しないとサーバは進捗を送らない。
2. **`$/progress` を LSP クライアントの reader タスクで捕まえ、セッションごとの
   状態に集約する**(`mina_lsp::Progress`)。トークンの begin/end を
   outstanding 集合に反映し、「outstanding が空」+ 静止確認(`quiesce` = 300ms)で
   索引完走と判定する。静止確認は相次ぐ phase の合間の空
   (実測: 60ms, 100ms)を完走と誤認しないため。`report`(進捗率)は使わない。
3. **進捗通知は通知チャネルに流さない**。索引中は毎秒何十件も届き、容量 64 の
   bounded チャネルから `publishDiagnostics` を押し出す(無言の診断欠落)。
4. **`ensure` が索引完走を待ってからセッションを返す**(`await_indexed`)。呼び出し
   側は 1 箇所も変更しない(`symbol` / `rename` / `references` / `hover` /
   `peek` / `check` / Open 直後の診断 settle が同じ経路を通る)。
   待機はデーモンロック・LSP セッションロックを握ったまま行わない —
   握ると編集中の didChange を締め出し(`LSP_LOCK_TIMEOUT` でスキップされ)、
   以後の解析が古いテキストのままになる。
5. 待機は**セッションごとに 1 回だけ**。確認済みフラグを `Progress` が持ち、
   以後は即返る(1 要求ごとに静止確認 300ms を払わない)。
6. 逃げ道を 2 つ持つ: 進捗を 1 つも送らないサーバ(work-done progress 非対応)は
   `arm`(1s)で「進捗を使っていない」と判定して進む。
   根拠(実測 2026-09-12): rust-analyzer の最初の進捗は `initialized` の約 0.26 秒後、
   typescript-language-server は進捗を 1 件も送らない(TS は 1 セッション 1 回だけ
   1 秒を払う)。初回進捗が 1 秒を超えるサーバを見つけたら `arm` を上げる。`cap`(10s)で打ち切って
   未確認のまま進む(待ち続けて要求を止めない)。どちらも「正しい応答が遅れて
   届く」側に倒し、無言の誤りは作らない。

## Considered Options

- **索引依存の各要求の前に個別に待つ**(呼び出し側に 5 箇所): 却下 — 同じ
  ガードを 5 箇所に置くと、新しい要求を足したときに 1 箇所忘れる。`ensure` は
  LSP を使う全経路の合流点なので 1 箇所で足りる。
- **待機中もセッションロックを保持する**: 却下 — didChange のスキップ(上記)と、
  別クライアントの要求が `LSP_LOCK_TIMEOUT` で落ちる(いま直している誤りの
  再現)。
- **`$ /progress` に依存しない方法**(結果が空なら再試行): 却下 — 空の symbol は
  再試行で直るが、**部分 rename は成功として返る**ので再試行の引き金が無い。
- **cap を長くする / settle 予算を延ばす**: 却下 — 索引の完了は進捗で判定できる
  ので、時間で待つ必要が無い。上限はハング防止のためだけに置く。

## Consequences

- cold の無言の誤り 3 件が消える(L0 `--cold`: `silent` 0 / `rename` ok /
  `check` の LSP error なし)。
- 初回の LSP 呼び出しは**索引の実時間を待つ**(実測: 小クレートで 8〜9 秒。
  以前は同じ時間を「誤った応答を返しながら」並行に使っていた)。以降の呼び出しは
  待たない。
- `check` の `settled` の意味は変えない。空応答を「クリーン確定」に倒すのは
  別問題: **索引完走後でも rust-analyzer の pull は実際のエラーを取りこぼす**
  (実測: config.rs のフィールド削除で main.rs の参照が壊れていても、warm な
  pull は main.rs に空を返し exit 0。関数名タイポでは両ファイルとも空)。
  ADR-0045 の判定は維持し、`check` の待ち時間の無駄は別の反復で扱う。
- `Progress` は wire 形状を変えないため PROTOCOL_VERSION は据え置き。
- 計測: 索引待ちの実時間は `wall_ms` に出る。L0 では warmup フェーズが
  この待ちを吸収するため、契約費用(測定区間)は変わらない。

## 関連

- 実装: `mina-lsp/src/lib.rs`(`Progress` / `ReadyPolicy`)、
  `minad/src/lsp.rs`(`LSP_READY` / advertise)、`minad/src/daemon.rs`(`await_indexed`)
- 反復の記録: `docs/gitignore/loop/log.md`（手元の作業用） iteration #2
- 実測の方法: `docs/gitignore/loop/method.md`(L0)
- 判定の既存 ADR: ADR-0045(空応答をクリーンの根拠にしない)
