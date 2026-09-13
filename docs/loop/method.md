# minas/minad ループエンジニアリング — 検討方法（運用マニュアル）

> git 管理（`docs/loop/`）。作成: 2026-09-12。
> **ループエンジニアリングの成果物はこのディレクトリで完結する。**
> **次のセッションはこのファイルと [`latest.md`](./latest.md) だけで次のループを回せる。**
> 入口は [`README.md`](./README.md)（何を先に読むか）。
>
> | このディレクトリの中身 | 役割 |
> |---|---|
> | `README.md` | 入口。どこから読むか・現状・次の一手 |
> | `method.md`（本ファイル） | 検討方法・運用マニュアル |
> | `latest.md` | 最新の結果・確定した事実・次の課題設定 |
> | `log.md` | 全反復の生データ＋考察（`l0.py --log` が追記） |
> | `l0.py` | 計測器 L0（LLM なし・決定論的） |
> | `probe_progress.py` / `probe_pull_diagnostics.py` | LSP 直叩きプローブ（§6） |
>
> 設計判断は **`docs/adr/0051`（索引完走ゲート）/ `0052`（check の空の早期確定）**にあり、
> `docs/adr/0045` にも追記してある。コードのコメントもこの番号を参照している
> （判断がコードと離れないように追跡側に置く）。
>
> 注意: `tmp/loop/`（スクラッチ・結果 JSON）は checkout ごとの gitignore 対象。
> linked worktree で回すと fixture と JSON はそちらに作られる（この文書自体は worktree にも複製される）。

## 0. 目的（何を最大化するか）

minas の存在理由は**エージェントのコスト低減**。改善は思いつきではなく
**課題設定 → 検証 → 考察 → 修正 → 効果確認**のループとして回す。
各段で見るのは「迷い（hesitation）が減ったか」「無駄な往復が減ったか」。

## 1. 2層のループ（ここを混同しない）

| 層 | 道具 | 費用/時間 | 役割 |
|---|---|---|---|
| **L0（内側）** | `docs/loop/l0.py` | 無料・数十秒〜1分・決定論的 | 仮説の**棄却**。ここで差が出ない仮説は L2 を回さない |
| **L2（外側）** | `tools/ab/ab.py`（実LLM A/B） | 数十円/run・分単位・高分散 | 実LLMで本当に安くなったかの**最終確認** |

L2 だけを回すと 1 仮説に数十分と数十円、しかも分散（nano n=5 で −5% 級は判定不能）で
判断がぶれる。**L0 で棄却 → 差が出たものだけ L2** が原則。

## 2. 指標（なぜこの4つか）

エージェントのトークン費用は往復ごとにコンテキスト全体を再送する構造:

```
cost ≈ Σ_{t=1..T} (C_0 + Σ_{i<t} o_i) = T·C_0 + Σ_i (T-i)·o_i
```

- `calls` — 往復数 T。**支配項**（`--hunks-stdin` のような往復削減がここに出る）
- `out_B` — 総出力バイト（コンテキストに残る量）
- `equiv_B` — `Σ o_i + Σ (T-i)·o_i`。**序盤の大出力ほど高い**（後に何度も再送される）
- `wall_ms` — 実時間（LSP ロード・settle 待ち・cargo を含む）
- `fails` — 失敗ステップ数（= 迷いの発生源。期待 exit を `ok_rc` で指定できる）

**迷い（hesitation）は直接測れない**ので代理指標で捉える:

| 代理指標 | 見る場所 |
|---|---|
| 契約が要求する最小往復数（`calls`） | L0（契約が N 回を強制するなら全エージェントが N 回迷う） |
| 失敗・拒否のステップ数（`fails` / `silent`） | L0 |
| cold での無言の誤り（空応答 + exit 0） | L0 `--cold` |
| 最初の一手の種類・再読回数・skill 参照回数 | L2 の audit log |

## 3. ループの回し方（5段）

1. **課題設定** — 仮説を**指標ペア**で書く。悪い例「check を速くする」／良い例
   「空の早期確定で `wall_ms` が 10000→600ms になり、かつ pull が見るエラーは
   rc=2 のまま取りこぼさない（`fails` 0）」。**受理条件と棄却条件を先に書く。**
2. **検証（L0）** — 仮説をアームとして `docs/loop/l0.py` の `FLOWS` に足す。
   `calls`/`equiv_B`/`fails` に差が無ければ**棄却して次へ**（L2 は回さない）。
3. **考察** — 差を `calls` / `out_B` / `equiv_B` / `wall_ms` に分解して帰属させる。
   期待と逆なら計測器か仮説の欠陥。**原因を書く**（"なんとなく速くなった" は棄却）。
4. **修正** — 一度に 1 仮説。**共通経路 1 箇所**を直す（呼び出し側に散らさない）。
5. **効果確認** — L0 を再実行し、主要な変更だけ L2 を 1 本。`--log` で
   `log.md` に追記（**棄却結果も消さない**）。

**ドキュメント自体の検証（手法として有効）**: 次の反復の準備ができたら、**別コンテキストの
セッション**（subagent の scout 等）に `method.md` と `latest.md` **だけ**を読ませて
「次に何をするか（コマンド列）」「不足・曖昧・壊れている点」を報告させる。
iteration #3 の直後にこれをやって、(a) `apply` の 1 回目/2 回目の差で**仮説の前提が
割れている**こと、(b) 棄却条件が**検証不能**（TUI は hint を描画しない）、
(c) `hints` flow が存在しない、など 6 件が見つかり全部直した。
自分で書いた文書は自分では読めない — 冷読セッションに 1 回通すのが最も安いレビュー。

## 4. コマンド（コピペ用）

```bash
cd /Users/335g/dev/other/mina
L0=docs/loop/l0.py

# 計測器の健全性（daemon 不要）
python3 $L0 --selftest

# 全フロー / フロー・アーム絞り込み / ステップ別内訳
python3 $L0
python3 $L0 -f verify -f verify-broken -a check -v
python3 $L0 -f verify -r 3                     # wall は中央値（-r 3 推奨）

# 記録（仮説を必ず書く → log.md に追記される）
python3 $L0 -r 3 --log -n "仮説: …"

# cold 税（LSP ワークスペースロード前）
python3 $L0 --cold

# 1 コマンドの内訳（計時ログ。ADR-0056）: MINAD_TRACE=1 を付けて測り、
# アーム専用 TMPDIR の daemon ログを読む
MINAD_TRACE=1 python3 $L0 -f verify -r 1
rg 'minad.trace' tmp/loop/verify-apply-check-*/.tmp/minad.log   # span/phase/ms が 1 行ずつ
#   write bytes=N / serialize / socket   … 応答の直列化 + 書き込み
#   edit total / sync total / sync.bg total … apply のデーモン側内訳（背景 pull は sync.bg）
#   check total = borrow + caps + pull + restore、check.pull round0/round1 … check の内訳
#   rename total = prepare/resolve/request/convert/apply、lsp.retry attempt1/attempt2
#   hints total = pull（RA の inlayHint 計算）

# LSP 直叩き（daemon の挙動が曖昧なとき。§6）
python3 docs/loop/probe_pull_diagnostics.py tmp/loop/probe 3 0.2
python3 docs/loop/probe_progress.py tmp/loop/probe 8 progress

# 実LLM A/B（L0 で差が出た仮説だけ）
python3 tools/ab/ab.py run t11 A 1 && python3 tools/ab/ab.py run t11 B 1

# 回帰
cargo test          # 全 485 テスト
cargo build
```

バイナリは `L0_MINAS` / `L0_MINAD` で差し替え可（既定は `target/debug/` → PATH の順）。
**注意**: コードを触ったら `cargo build` を先に（計測対象は `target/debug/`）。
結果 JSON は `tmp/loop/` に落ちる（`--json <path>` で指定可）。

## 5. 計測器の作り（数字を信じるための規則）

- **アームごとに専用 daemon**: アーム専用 `TMPDIR`（= 専用 socket）で `minad serve` を
  起動。`minas info` の前後差分がそのアームだけの計測になる。
- **fixture は毎回生成**（アームごとに新規作業ディレクトリ。状態の持ち越し無し）。
- **warm / cold を分ける**: 既定は「LSP がワークスペースを読むまで待ってから」。
  `--cold` は起動直後。cold でしか出ない失敗は「契約の欠陥」として課題に上げる。
- **非決定な step は複数回測る**（`-r 3`）。1 回の観測で結論を出さない。
- `expect` は **stdout + stderr** を見る。期待と違えば `silent` として `fails` に乗る。
- **step・warmup プローブの `minas` は解決済みのバイナリへ置換する**（`run()` が
  `^minas` と `| minas` を `MINAS` = `target/debug` → `L0_MINAS` に置き換える）。
  iteration #10 以前は step が PATH のインストール済み `minas` を実行しており、
  **クライアント側の変更が L0 で測られていなかった**（計測器の欠陥）。
- 期待する非 0 exit は `ok_rc` で宣言する（エラー検出を検証する flow 用）。

### L0 の構造（仮説の足し方）

```python
FLOWS = {
  "flow名": {
     "goal": "何を達成するか（1行）",
     "fixture": "rust",              # 生成関数（FIXTURES）
     "warmup": True,                 # LSP 索引完走を待ってから測る
     "verify":  "cargo check --offline && grep -q …",  # 実行後の状態検査
     "need":    "診断メッセージ",      # トランスクリプトに必要な文字列
     "ok_rc":   {"arm名": {1: 2}},    # 期待 exit（0 以外が正常な flow）
     "expect":  {"arm名": ["", "validate2"]},  # step ごとの期待（silent 検出）
     "arms": { "arm名": ["minas apply …", "minas check …"] },  # sh -c で実行
  },
}
```
step は `{meta}` で fixture の位置（`{validate_pos}` 等）を注入できる。
fixture は `fixture_rust()` が `[workspace]` 付きのクレート（親 workspace への
ネストを避ける — 過去の実測知見）を生成し、`probe`（索引完走プローブ）も返す。

## 6. プロトコルレベルで測る（daemon の挙動が曖昧なとき）

L0 は「minas の契約」を測るが、「その背後で LSP が何を返しているか」は見えない。
そこを取り違えると**間違った結論**を書く（実例: 「pull は構文エラーを取りこぼす」と
誤診 → 実際は probe が前ケースの汚染を拾っていた）。疑ったら LSP を直叩きする:

- `docs/loop/probe_progress.py <fixture> [秒] [progress|no-progress]` —
  rust-analyzer の `$/progress` ストリームを時刻つきで出す。**`window.workDoneProgress`
  を advertise しないと 1 件も送られない**こと、`cachePriming`（title "Indexing"）が
  索引完走の目印であることを確認した道具。ADR-0051 の根拠。
- `docs/loop/probe_pull_diagnostics.py <work-dir> [rounds] [gap_sec]` —
  編集後に `textDocument/diagnostic` を繰り返し、**どのエラーが pull に見えるか**と
  **何回目で安定するか**を出す。ケースごとに新しい fixture と新しい rust-analyzer を
  作る（前ケースの変更が残ると診断が混ざって誤診する）。ADR-0052 の根拠
  （round0 == round11、メソッド解決エラーは永久に空）。
- どちらも fixture が無ければ同じディレクトリの `l0.py` の `fixture_rust()` で生成する。

```bash
python3 docs/loop/probe_pull_diagnostics.py tmp/loop/probe 3 0.2
python3 docs/loop/probe_progress.py tmp/loop/probe 8 progress
```

**クライアントの固定費を測る（iteration #9 の手法）**: `minas` 側に待ちがあると
疑ったら、Python で同じプロトコルを直に喋って daemon の応答時間と比較する。
`tmp/loop/probe-client/` の形（`fixture_rust` を生成 → `minad serve` を同じ
コマンド内で起動 → socket に `Hello` + コマンドを送って 1 行読む）で、
「単一接続」「probe 接続（接続→即 close）を挟む」「同一接続で 3 往復」を測る。
iteration #9 はこれで **daemon の応答は 0.27ms、`minas` の 1 呼び出しに ~68ms
の固定費**（捨て接続 + accept ループ内の peer uid 検査の sleep）を特定し、
iteration #10 で**除去した**（ADR-0071。`minas info` 76.6 → 9.1ms、probe→本接続
68.4 → 0.52ms）。注意: 手で起動した daemon は**残ることがある**（`minas` の自動起動は
`setsid` で切り離すので、スクリプト終了後も rust-analyzer ごと生き残る）。測る前に
`ps aux | grep -E "minad serve|rust-analyzer" | wc -l` が 0 であることを確認し、
残っていれば `kill -9` する（macOS の `pgrep` に `-c` は無い）。

## 7. 落とし穴（実際に踏んだもの）

- **warmup を固定 sleep でやらない**: rust-analyzer の索引が残っている間の LSP
  セッションロック待ちが `wall_ms` に混ざり 0.4〜16s と暴れた。L0 は
  「プローブが上限時間内に 2 回連続で当たる」まで待つ方式に校正済み。
  校正前の観測は破棄した（`equiv_B` は校正の影響を受けない）。
- **probe の汚染**: 同一 fixture を複数ケースで使い回すと、前のケースの変更が残り
  診断が混ざる。ケースごとに新しい fixture（新しい RA）を使う。
- **セッションロックを握ったまま待たない**: 待機中に didChange を締め出すと編集の
  同期がスキップされ（`LSP_LOCK_TIMEOUT` で諦める設計）、以後の解析が古いテキストの
  ままになる。待つのはロックの外（`await_indexed` のコメント参照）。
- **fixture が小さい**: `cargo check` が 0.2〜0.5s で終わるので、`wall_ms` の結論は
  実プロジェクトに外挿しない（`calls` / `equiv_B` は契約の形で決まるので外挿可）。
  規模の確認は L2 の仕事。
- **`minas apply` の残りの ~1 秒は RA の初回再解析**（diagnostics の pull が解析完了まで
  ブロックする。iteration #4 のトレースで確定、#5 で下調べ済み）。他のコマンド
  （`symbol` / `check`）が先に解析を起こしていれば `apply` は 100ms 台。
  編集処理自体は ~150ms（旧記述の「診断 + hint の pull の固定待ち」は #5 で撤去済み — ADR-0053）。
  #7 で「pull を外しても hint pull が同じ解析を買うので `apply` は変わらない。両方外すと
  153ms だが `check` が 1122ms を払い、編集後追従のテスト 3 件が落ちる」まで確定した
  （ADR-0053 追記）。この 1.1s をエージェントの待ち時間から外す道は「ブロックしない」
  （pull の背景化）で、それが iteration #8 の課題。
- **`apply` の wall は 700ms / 1100ms に割れる**: `Open` は背景 settle を
  `tokio::spawn` する（Open 応答をブロックしない）ので、その解析と編集後 pull の解析が
  同じ RA の中で競合する。どちらが先に解析を起こしたかで `apply` の wall が変わる
  （同一バイナリで 807ms と 1241ms を観測 — #7）。**`apply` を含む比較は `-r 3` 必須、
  同一セッションで交互に測る**。1 回の観測で「改善した」と結論しない。
- **1 アーム 1 回の観測**: `equiv_B` / `calls` は決定論的（同じ契約なら同じ値）、
  `wall_ms` は揺れる（`-r 3` で中央値）。
- **`C_0`（システムプロンプト等の固定費）は測らない**: アーム間で同じなら比較に影響しない。
- **計測の穴**: `minas info` の `ServerMetrics` に `rename` のカウンタが無く、
  `get_state_total` にも bytes が無い。サブコマンドを足すときはカウンタも足す。
  **ただし `ServerMetrics` は `ServerInfo` 応答の wire の一部**なので、追加は
  ADR-0039 の bump 方針（v18 の `read_total`/`read_bytes` が前例）に従って
  `PROTOCOL_VERSION` を上げる必要がある（loop 側の制約と衝突するときは、
  次に wire を変える用事と束ねる — ADR-0056）。
- **1 つのコマンドの内側の内訳は `MINAD_TRACE=1` で測る**（iteration #9 / ADR-0056）。
  `minad.trace <span> <phase> <ms>` が stderr（daemon のログ）に出る。既定は無効
  なので通常の計測には影響しない。span は `write` / `edit` / `sync` / `sync.bg` /
  `sync.lock` / `sync.pull` / `didChange` / `didOpen` / `borrow` / `check` /
  `check.pull` / `rename` / `hints` / `lsp.retry`。内訳が無いときは A/B 除去
  （#4〜#7 のやり方）を最後の手段にする。
- **同じ arm の中で複数回叩くと「初回 vs 2 回目」が分かる**: 初回だけ高いコスト
  （初回 pull・初回 didChange 後の解析など）はここで検出する。既知の例: `apply` は
  #8 の背景化で **~130ms で安定**（初回の RA 再解析は背景 pull が買う。内訳は #9 の
  `edit total` 2ms + `sync.bg` の診断 pull ~900ms — 応答は待たない）。
  固定待ち（`PULL_SETTLE` 250ms・`SEMANTIC_RETRY_WAIT` 500ms）は #5 / #6 で 0 になり、
  応答経路に残っている待ちは無い。
- **`minas` の接続経路の固定費は #10 で除去済み**（ADR-0071。`minas info` 76.6 →
  9.1ms、`explore/lsp` 310 → 82ms）。残っているのは `minas` 起動の ~12ms × calls と、
  `apply` の 3 往復。`wall_ms` を読むときは「実処理 + calls×12ms + 往復数×~10ms」と分ける。
- **L0 の step・warmup は解決済みのビルド済みバイナリを実行する**（#10 で `l0.py` を
  修正。以前は `minas` が PATH 解決でインストール済みバイナリを実行していた）。
  **コードを触ったら `cargo build` を先に行う**（忘れると古いバイナリを測る）。`l0.py` は
  `MINAS = target/debug → L0_MINAS` の順で解決し、デーモンは `target/debug/minad`。
- **手動テストの daemon / rust-analyzer が残ると wall が汚染される**（実測で +60ms。
  `minas symbol` が 12ms のはずが 90ms になった）。測る前に
  `ps aux | grep -E "minad serve|rust-analyzer" | wc -l` が 0 であることを確認する。

## 8. 結果の書き先（どこに何を書くか）

| 書くもの | 置き場所 |
|---|---|
| 生データ（表・daemon 計測・失敗 step） | `log.md`（`--log` が追記。git 管理なので差分が残る） |
| 考察と次の課題設定 | `log.md` に手書きで追記 + 要約を `latest.md` に反映 |
| 最新の結果と次の一手 | `latest.md`（上書き） |
| 検討の進め方（この文書） | `method.md` |
| 確定した設計判断（コードを変えたとき） | `docs/adr/00NN-*.md`（追跡側。コードのコメントから番号で参照する） |
| 一時データ（スクラッチ・JSON） | `tmp/loop/`（gitignore） |

**`--log` を忘れると生データが消える**（JSON は `tmp/loop/` に落ちるだけ）。
計測したら必ず `--log` 付きで回す。
