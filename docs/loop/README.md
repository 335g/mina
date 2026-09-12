# docs/loop — ループエンジニアリング（minas/minad のコスト低減）

エージェントのコスト低減を、**課題設定 → 検証 → 考察 → 修正 → 効果確認**のループとして
回すための作業場所。計測器・方法・全反復の記録・設計判断の参照をここに集約する。

## まず読むもの

1. [`latest.md`](./latest.md) — **最新の結果と次の課題設定（§4）**。ここから再開する
2. [`method.md`](./method.md) — 回し方（2層ループ・指標・コマンド・落とし穴）
3. 必要なら [`log.md`](./log.md)（全反復の生データ＋考察）/ `docs/adr/0051`・`0052`（設計判断）

新しいセッションへの指示文は [`latest.md`](./latest.md) §5 にある。プロンプトテンプレート
**`.pi/prompts/dev_minas.md`**（このリポジトリ内）に同じ内容を置いてあるので、`/dev_minas`
と打てば「README → latest §4 から次の iteration」の指示が展開される。

## 現在地（2026-09-12 時点）

- 完了: **#1 計測器の構築 → #2 LSP 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定（ADR-0052）→ #4 初回 apply の内訳確定 → #5 編集後の固定 settle 撤去（ADR-0053）**
- 効果（L0 実測）: `check`（クリーン）**10154ms → 589ms（−94%）**。`apply` は
  **1 回目 1359 → 1133ms / 2 回目 346 → 107ms**（毎回の固定 250ms が消えた）。
  cold の無言の誤り 0 件。`explore` / `rename` の `calls` / `equiv_B` は不変（回帰なし）
- **#5 の結末**: 「`PULL_SETTLE` 250ms は通知消費待ちで削れる」は**採用**（50ms で
  空回帰が出ず、0ms まで削って撤去。ADR-0053）。根拠は「pull 自身が解析完了まで
  ブロックして最終集合を返す」
- 次: **iteration #6**（`symbol` / `rename` / `check` が毎回払っている
  `SEMANTIC_RETRY_WAIT` 500ms の削減）。詳細と受理・棄却条件は [`latest.md`](./latest.md) §4

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

計測の一時データ（結果 JSON・fixture）は `tmp/loop/`（gitignore）。

## 最短の起動手順

```bash
cd /Users/335g/dev/other/mina
cargo build                                          # 計測対象は target/debug
python3 docs/loop/l0.py --selftest                   # 計測器の健全性
python3 docs/loop/l0.py -f verify -r 3 -v            # 現状の基準値（apply 初回/2回目・check 0.59s）
cargo test                                           # 462 passed が期待値
```

そのあと `latest.md` §4 の「測り方」を順に進める。計測したら
`python3 docs/loop/l0.py -r 3 --log -n "仮説: …"` で `log.md` に記録し、
考察を `log.md` に、要約を `latest.md` に反映する（反復番号の規則は `latest.md` §5）。
