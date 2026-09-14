# watched-files 通知を Save 応答の経路から外す — 初回 `apply` の ~1.0s を消す

iteration #11 で見つけた穴（`docs/loop/log.md`）: `minas apply` の**初回だけ ~1.0s**
かかる（2 回目以降は ~30ms）。内訳（Python プローブ = 同じプロトコルを直叩き、
warmup 済み daemon）:

| 往復 | 初回 | 2 回目 |
|---|---|---|
| `Open` | 37ms | 1.5ms |
| `DocumentEdit`（背景 pull を spawn） | 3.5ms | 3ms |
| `Save` | **965ms** | 1.5ms |

`MINAD_TRACE=1` の順序（修正前）: `edit total 2` → 応答 `write` → 背景 `sync.bg`
（`didChange 2` → `sync.lock 986` → `sync.pull diag 979`）→ **Save の応答 `write` が
その後に出る**。つまり Save ハンドラ末尾の `notify_watched_files(...).await`
（ADR-0061）が `timeout(LSP_LOCK_TIMEOUT, session.lock())` を待ち、**背景 pull
（ADR-0055 が応答経路から外したはずの ~1s）が応答経路に戻っていた**。ADR-0061 は
2026-09-13 16:58 追加 = #8 の後なので、#8 の「apply ~130ms」は warm でも
「解析済みでロックが空いている」場合の値だった。

Status: accepted

## Decision

**`notify_watched_files` の呼び出しを背景タスクへ移す**（`Save` と `DeletePath` の
2 箇所。どちらも「書いた／消した直後に通知してから応答」という同じ形をしていた）。

- `notify_watched_files_in_background(daemon, changes)` を足し、`tokio::spawn` で
  `notify_watched_files` を実行する。呼び出し側は `await` しない。
- 通知は**冪等**（RA はファイルを読み直すだけ）なので、届くのが遅れても content は
  変わらない。応答に必要な情報はハンドラ内で既に作ってあるため、応答は待たない。
- 送信順序は実用上保たれる: ロックが背景 pull に取られている場合は、通知タスクが
  その後ろに並び、**次のコマンドの LSP 要求より先**に送られる（tokio Mutex は FIFO）。
  ロックが空いていれば即送られる。どちらでも「次の編集で再通知される」という
  ADR-0061 の取りこぼし規定の範囲内。

## Considered Options

- **`try_lock` で即諦める（落とす）**: 却下 — ADR-0061 の穴（新規ディレクトリ・
  新メンバーの 1 回きりの通知）が戻る。落として良いのは「次の書き込みで再通知される」
  ときだけで、新規作成の通知は落とすと**黙って部分的な答え**に戻る。現行実装の
  `timeout(3s)` も同じ理由で「3 秒待ってから諦める」形を保っている。
- **保留キューに積み、次の Save/編集の `didChange` の前（同じセッションロックの中で）
  に送る**: 却下（今は）— 送信側は同じだが、「編集が来ないまま次の意味クエリが来る」
  場合に通知が遅れ続ける（新規メンバーの `symbol` が空のまま）。背景タスクなら
  ロックが空いた時点で送られる。キュー + ポンプの設計は、通知の再送・合流が実測で
  要求されたときに。
- **Save の応答前に通知を送りつつ、`await` ではなく `try_lock` して取れなければ
  背景へ**: 却下（今は）— 分岐が増えるだけで、得られるのは「ロックが空いているときの
  送信順序」だけ。背景タスクでも同じ順序になり、L0 の `calls`/`out_B` は変わらない。
- **通知の対象を「新規作成の可能性があるとき」だけに絞る**: 却下 — daemon は
  「この書き込みがファイルを作ったのか既存を変えたのか」を知らない（CLI の `apply` は
  新規ファイルを touch → Open → 保存する。ADR-0061 の Considered Options と同じ理由）。

## Consequences

- **初回 `apply` の `Save` 往復は 965ms → 1.8ms**（プローブ、3 回再現）。L0 の
  apply step は **1006–1123ms → 53ms**（`-r 3` の中央値、同一セッションでの
  before/after 交互測定）。
- L0（warm, r=3）: `verify/apply-cargo` **1202 → 183ms**、`hunks-cargo`
  **1214 → 192ms**、`apply2-cargo` **1192 → 216ms**、`rename/apply`（apply ループ）
  **1379 → 268ms**。`calls`/`out_B`/`equiv_B` は全 flow で不変、fails 0。
- **ギャップあり（`verify-gap`、sleep 3 = 推論遅延の代理）の apply + check は
  ~1140 → 71ms**（apply 53 + check 18）。背景 pull がエージェントの思考中に終わる。
- **ギャップなしの `apply` + `check` は和が変わらない**（~1010 → ~1017ms）。理由は
  「待ちが別の場所へ移った」のではなく、**同じ 1 回の RA 解析を誰が待つか**が変わった
  だけ: 修正前は Save がロックを待ってから応答し、`check` は解析済みを読む。修正後は
  Save が即返り、`check` が背景 pull のロックを待つ（#9 の計時で `check total 686 =
  borrow 682 + pull 2` — check 自身は解析を買っていない）。**解析の二重払いではない**
  （#7 の棄却理由とはそこが違う）。この残りの待ち（ギャップなし check の ~700ms）は
  「背景 pull と check を同じ 1 回の要求に合流させる」設計変更の課題として残る
  （`latest.md` §4 の別候補）。
- **通知の契約は維持**: 新規追加した daemon テスト
  `save_does_not_wait_for_the_background_pull_before_notifying_watched_files` が
  (1) Save が背景 pull を待たない、(2) 通知が（遅れてでも）LSP に届く、の両方を見る
  （mock サーバに `MOCK_WATCH_LOG` / `MOCK_DIAG_LOG` / `MOCK_DIAG_DELAY_MS` を追加。
  **修正を戻すと失敗する**ことを確認）。ADR-0061 の受け入れ試験（稼働中に
  `minas apply` で新メンバー `crates/c` を作り 6 秒後に `minas symbol` が `c_helper` を
  返す）も再実行して通ることを確認（`tmp/loop/probe_watch_member.py`）。
- `cargo test --workspace`: **487 passed**（486 + 新規 1）。cold の
  `verify-broken/check` は `-r 5` で fails 0（#11 の契約は維持）。cold 全 flow は
  fails 0・無言の誤り 0（cold の wall は索引完走ゲート支配で ±10% 以内）。
- 通知は**応答の後に送られうる**。ADR-0061 の「応答前に通知する」という含意は
  （テストでも文書でも）契約にしていなかったので変更ではないが、今後
  「Save の応答 = 通知済み」を前提にした設計をしないこと。
