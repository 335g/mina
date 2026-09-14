# docs/loop — ループエンジニアリング（minas/minad のコスト低減）

エージェントのコスト低減を、**課題設定 → 検証 → 考察 → 修正 → 効果確認**のループとして
回すための作業場所。計測器・方法・全反復の記録・設計判断の参照をここに集約する。

## まず読むもの

1. [`latest.md`](./latest.md) — **最新の結果と次の課題設定（§4）**。ここから再開する
2. [`method.md`](./method.md) — 回し方（2層ループ・指標・コマンド・落とし穴）
3. 必要なら [`log.md`](./log.md)（全反復の生データ＋考察）/ `docs/adr/0051`〜`0054`（設計判断）

新しいセッションへの指示文は [`latest.md`](./latest.md) §5 にある。プロンプトテンプレート
**`.pi/prompts/dev_minas.md`**（このリポジトリ内）に同じ内容を置いてあるので、`/dev_minas`
と打てば「README → latest §4 から次の iteration」の指示が展開される。

## 現在地（2026-09-14 時点）

- 完了: **#1 計測器の構築 → #2 LSP 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定（ADR-0052）→ #4 初回 apply の内訳確定 → #5 編集後の固定 settle 撤去（ADR-0053）→ #6 意味的リトライ待ち 500ms 撤去（ADR-0054）→ #7 apply の診断 pull 撤去（棄却。ADR-0053 追記）→ #8 編集後 pull の背景化（ADR-0055）→ #9 計時ログ `MINAD_TRACE`（ADR-0056）→ #10 接続経路の固定費除去（ADR-0071）→ #11 cold の check の巻き戻し修正（ADR-0072）**
- 効果（L0 実測）: `check`（クリーン）**10154ms → 87ms（−99%）**。`symbol` 583→82ms（−86%）。`explore/lsp`（3 calls）**310 → 82ms（−74%）**・`dump` **164 → 28ms（−83%）**、`minas` 1 呼び出しの固定費 **76.6 → 9.1ms**（#10）。`rename` ~1594→~1150ms（−28%）。**`apply` ~1.1s → ~130ms（−88%）**。ギャップありの apply + check の和 **~1270ms → 222ms（−82%）**、cargo 経路も **−77〜82%**。`calls` / `equiv_B` は不変（回帰なし）。485 test green
- **#10 の結末**: 接続経路の固定費を**採用**（ADR-0071）。(a) daemon の peer uid 検査を accept ループの外へ、(b) minas の捨て接続をやめて生きている接続を再利用 — の両方。A/B では (a)(b)(c) が同等に効く。**計測器の穴**（L0 の step は PATH の minas を実行していた。クライアント側の変更はそれまで L0 で測られていなかった）も見つけて `l0.py` を修正したので、§3 の参照値は新基準に置き換えた
- **#11 の結末**: cold の `check` が構文エラーを確定できなかった原因は「Open の背景タスクが編集前の全文を送り、サーバ側の文書が巻き戻っていた」こと（cold では `ensure` が数秒なので窓が広い）。送る直前に現在のテキストを取り直す修正で**採用**（ADR-0072）。cold の `verify-broken/check` は **10/10 で rc=2 + Syntax Error**、warm/cold の全 flow 不変、486 test green（回帰テストを 1 件追加）
- 発見（#11）: `minas apply` の**初回だけ ~1.0s**（2 回目以降は ~30ms）。プローブ実測で **Save 往復に ~963ms**（Open 37ms / DocumentEdit 3.4ms、daemon の edit は 2ms）。ADR-0061 の watched-files 通知（09-13 追加 = #8 の後）が Save 応答の末尾で `session.lock()` を待つため、背景 pull が応答経路に戻っている。これが次の課題
- 次: **iteration #12**（`apply` の初回 ~1.0s — Save の watched-files 通知が背景 pull のセッションロック待ち）。詳細と受理・棄却条件は [`latest.md`](./latest.md) §4

## 中身

| ファイル | 役割 |
|---|---|
| `README.md`（本ファイル） | 入口。読む順と現在地 |
| `latest.md` | 最新の結果・確定した事実・次の課題設定 |
| `method.md` | 検討方法・運用マニュアル（コマンド・判定規則・落とし穴） |
| `log.md` | 全反復の生データ＋考察（`l0.py --log` が追記） |
| `l0.py` | 計測器 L0（LLM なし・決定論的。`--selftest` 付き） |
| `probe_progress.py` | rust-analyzer の `$/progress` をダンプ（索引完走の目印を確認） |
| `probe_pull_diagnostics.py` | pull 診断の可視性と安定性を測る（どのエラーが見えるか） |

計測の一時データ（結果 JSON・fixture・`MINAD_TRACE` のログ）は `tmp/loop/`（gitignore）。
コマンド内部の内訳は `MINAD_TRACE=1`（ADR-0056）で `tmp/loop/*/.tmp/minad.log` に出る。
`l0.py` の step・warmup は**解決済みの `minas`（`target/debug` → `L0_MINAS`）**を実行する
（iteration #10 で修正。以前は PATH のインストール済みバイナリが実行され、クライアント側の
変更が L0 で測られていなかった）。

## 最短の起動手順

```bash
cd /Users/335g/dev/other/mina
cargo build                                          # 計測対象は target/debug
#                                                     # （l0.py の step は解決済みのバイナリを実行する。
#                                                     #  cargo build を忘れると古いバイナリを測る）
python3 docs/loop/l0.py --selftest                   # 計測器の健全性
python3 docs/loop/l0.py -f explore -r 3 -v           # 固定費の基準値（lsp ~80ms・dump ~28ms）
MINAD_TRACE=1 python3 docs/loop/l0.py -f verify -a apply-check -r 1 -v
#                                                    # #12 の課題（初回 apply。Save の write が sync.bg の後）
python3 docs/loop/l0.py --cold -f verify-broken -r 5 # #11 の契約（cold の check が rc=2 のまま）
MINAD_TRACE=1 python3 docs/loop/l0.py -f verify -r 1 # 内訳（minad.log に span/phase/ms）
cargo test                                           # 486 passed が期待値
```

そのあと `latest.md` §4 の「測り方」を順に進める。計測したら
`python3 docs/loop/l0.py -r 3 --log -n "仮説: …"` で `log.md` に記録し、
考察を `log.md` に、要約を `latest.md` に反映する（反復番号の規則は `latest.md` §5）。
