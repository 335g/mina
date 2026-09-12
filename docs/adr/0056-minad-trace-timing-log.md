# minad に計時ログ（`MINAD_TRACE`）を入れる — コマンド内部の内訳を恒久的に測れるようにする

iteration #7/#8 の痛み: 「apply の ~1.1s の内訳」「ギャップなし check の ~800ms の
内訳」を知るために**毎回 1 変更だけ入れて A/B 除去する**（#4 の hint pull 除去、
#7 の pull 除去）か、一時 `eprintln` を埋めて revert する（#4/#8）必要があった
（method.md §7「1 つのコマンドの内側の内訳は測れない」）。`minad` に
計時ログが無いことが、このループの**検証速度そのもののボトルネック**だった。

iteration #9 では、計時ログを恒久的な計測基盤として入れる。**エージェントの
wire・応答・契約は変えない**（PROTOCOL_VERSION 不変・応答バイト不変）。

## Decision

- `minad` に `src/trace.rs`（120 行）を追加し、`MINAD_TRACE=1` のときだけ
  stderr に 1 フェーズ 1 行を出す:

  ```text
  minad.trace <span> <phase> <ms>      フェーズの所要（直前の mark から）
  minad.trace <span> <key>=<value>     補足値（rounds / retries / bytes / settled 等）
  minad.trace <span> total <ms>        スパン全体（Drop で必ず出る）
  ```

- スパンは 1 コマンド・1 LSP 要求に対応する: `write`（シリアライズ + socket 書込み）、
  `edit`（DocumentEdit の適用）/ `sync` / `sync.bg`（編集後の背景同期）/
  `sync.lock` / `sync.pull`（診断・ヒント）/ `didChange` / `didOpen`（ロック待ちと
  送信を分離）、`borrow`（didOpen 前の準備）、`check` / `check.pull`（round 別）、
  `rename`、`hints`、`lsp.retry`（attempt 別 + retries）。
- `Drop` で `total` を出す（途中 return でも合計が残る。`mark` を忘れても計測が
  壊れない）。
- 既定は無効。無効時のコストは `Instant::now()` 1 回と分岐だけで、`enabled()` は
  `OnceLock` で env を 1 回しか読まない。
- `write_message` の無条件 `eprintln!("[conn N] writing M bytes")` を撤去した
  （同じ情報が `minad.trace write bytes=M` で opt-in で取れる。応答ごとの
  stderr 書き込みが消える）。

**tracing クレートは足さない**（ponytail）: 欲しいのは「どこで何 ms 使ったか」
だけで、span の親子関係・レベルフィルタ・subscriber は要らない
（`Instant` + `eprintln!` で足りる）。親子ツリーや出力先の切替が要るように
なったら tracing へ移す。

## 使い方（計測手順）

```bash
MINAD_TRACE=1 python3 docs/loop/l0.py -f verify -r 3 --log -n "…"   # L0 経由
# アーム専用 TMPDIR のデーモンログから読む:
rg 'minad.trace' tmp/loop/verify-apply-check-*/.tmp/minad.log
```

## Consequences（iteration #9 で分かったこと）

計時ログ導入で、#8 まで推測だった内訳が実測になった（L0 warm、trace on、
`verify` 系 n=12 の中央値）:

| 問い | 実測 |
|---|---|
| ギャップなし check の ~800ms は何か | `check total` 686ms = `borrow` 682（内 `ensure` = LSP セッションロック待ち）+ `pull` 2ms（round0 0 + round1 2）。背景 pull が完了していれば `check total` **3ms**。**check は解析を買っていない**（買っているのは背景 pull の 1 回の `pull_diagnostics` = 892ms）。`ensure` の `await_indexed` が `session.lock().await`（タイムアウト無し）で背景 pull のロックを待つ |
| apply の ~130ms の内訳 | デーモン側は `edit total` 2ms（適用 0 + 応答構築 1）+ `write` ~1ms。**残りは minas クライアント**（下記の ~65ms + 3 往復 + 起動 12ms） |
| rename の ~1150ms の内訳 | `rename total` 918ms = prepare 2 + resolve 12 + `request` 898（RA の WorkspaceEdit 計算。`lsp.retry` attempt1 912ms / attempt2 2ms）+ convert 2 + apply 2。**2 回目の安定確認は 2ms**（ADR-0054 の「確認はほぼ無料」を直接確認） |
| hints の 770ms | `hints total` 578ms = `pull` 578（RA の inlayHint 計算）。キャッシュヒット時は ~0ms |
| 編集後の追従 | `sync.bg` didChange 2 + pull 898（diag 892 + hint 6）+ reflect/push 0。`didChange` が 300–650ms になるのは前の背景 pull のロック待ち（応答はブロックしない） |

**副産物（この反復の最大の収穫）**: すべての `minas` 呼び出しに **~65ms の
固定費**があることを特定した。デーモンは Python クライアント相手に
**0.33ms** で応答するのに、`minas info` は 73–78ms（`minas --help` = 12ms）。
原因は (1) `minas` が「デーモンが生きているか」の確認に**捨てる接続**を 1 本
開いてすぐ閉じる、(2) daemon の `accept_loop` が peer uid 検査を
**accept ループ内で**行い、`peer_cred()` が ENOTCONN（閉じた相手）のとき
5ms×最大 10 回 sleep する — この間 accept ループが止まるため、直後の本命
接続の**最初の往復が ~65ms 遅れる**。A/B 実測: 単一接続 0.27ms vs
probe→本接続 67.2ms（Python で再現）。L0 の全 flow の wall はこの固定費を
含んでいた（explore/lsp 269ms のうち 3 呼び出し × 78ms = 234ms がこれ）。
修正は iteration #10 の課題。

**回帰**: 463 test green（462 + trace の形式テスト 1 件）。trace on の L0 は
全 15 arm で `calls`/`out_B`/`equiv_B` が #8 の値と一致し `fails` 0。
trace on/off の wall 比較（verify、n=9 ずつ）: apply-check −5.3%・apply-cargo
−2.2%（minas だけの step で構成される arm）で悪化なし。`cargo check` を含む
arm は同一セッション内でも cargo の wall が 124–1555ms と振れるため、~1ms/命令
の計時コストを分解できない（比較の対象外）。

## Options Considered

- **`tracing` + `tracing-subscriber` を入れる**: 依存 +2、出力整形・フィルタ
  設定が要る。欲しい情報（フェーズの ms）は span の親子では増えない → 却下。
- **`ServerMetrics` に rename カウンタ / `get_state_bytes` を足す**（§4 の対象 (d)）:
  `ServerMetrics` は `ServerInfo` 応答の wire の一部で、v18 の前例（`read_total`
  / `read_bytes` 追加時に PROTOCOL_VERSION を bump）どおりなら version bump が
  要る。§4 の制約「PROTOCOL_VERSION 不変」と ADR-0039（bump 方式）が衝突する
  ため**今回は保留**。rename の files/edits は `minad.trace rename files=… edits=…`
  で per コマンド取れるので、恒久カウンタの必要性は下がった。次に wire を
  変える用事（v20）と束ねる。
- **一時トレースを毎回埋める**: #4〜#8 で毎回 revert が必要だった運用。恒久化
  すればその往復が消える → 採用（本 ADR）。

## Status

accepted（iteration #9）。

## 関連

- 実装: `minad/src/trace.rs`、`minad/src/daemon.rs`（`write_message` /
  `prepare_borrowed_session` / `serve_inlay_hints` / `serve_check_diagnostics` /
  `serve_rename` / `sync_after_edit` / `DocumentEdit` アーム）、
  `minad/src/lsp.rs`（`open_document` / `sync` / `pull_after_edit` /
  `pull_diagnostics_settled` / `request_with_loading_retry`）
- 測定の記録: `docs/loop/log.md` iteration #9 / 方法: `docs/loop/method.md` §4・§7
- 判定の既存 ADR: ADR-0051（索引完走ゲート）・0052（check の空の早期確定）・
  0053（編集後 pull の settle 撤去）・0054（SEMANTIC_RETRY_WAIT 0ms = 確認は
  ほぼ無料）・0055（編集後 pull の背景化）・0039（PROTOCOL_VERSION bump 方式）
