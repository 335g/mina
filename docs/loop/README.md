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

## 現在地（2026-09-12 時点）

- 完了: **#1 計測器の構築 → #2 LSP 索引完走ゲート（ADR-0051）→ #3 check の空の早期確定（ADR-0052）→ #4 初回 apply の内訳確定 → #5 編集後の固定 settle 撤去（ADR-0053）→ #6 意味的リトライ待ち 500ms 撤去（ADR-0054）→ #7 apply の診断 pull 撤去（棄却。ADR-0053 追記）→ #8 編集後 pull の背景化（ADR-0055）**
- 効果（L0 実測）: `check`（クリーン）**10154ms → 87ms（−99%）**。`symbol` 583→82ms（−86%）。`rename` ~1594→~1150ms（−28%）。**`apply` ~1.1s → ~130ms（−88%）**。ギャップありの apply + check の和 **~1270ms → 222ms（−82%）**、cargo 経路も **−77〜82%**。cold の無言の誤り 0 件。`calls` / `equiv_B` は不変（回帰なし）
- **#8 の結末**: 「`sync_after_edit` の pull は Save 応答のクリティカルパスから外せる（背景タスク化）」は**採用**（ADR-0055）。pull を外す（#7。）と次に pull した者が解析を払うが、**応答を待たせない**形ならエージェントの次ターン（LLM 推論。数秒）の間に背景 pull が解析を終える。編集後追従の契約（テスト 3 件）は維持（検証ポイントを応答 → poll に移した — §4 の前提「テストは poll で待つ」は誤りだった）。
- 次: **iteration #9**（計時ログの追加。コマンド内部の内訳を恒久的に測れるようにして、#10 以降の仮説検証の基盤を埋める）。詳細と受理・棄却条件は [`latest.md`](./latest.md) §4

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
#                                                     # （cargo clean 後は l0.py が PATH の
#                                                     #  minas/minad に黙って落ちるので注意）
python3 docs/loop/l0.py --selftest                   # 計測器の健全性
python3 docs/loop/l0.py -f verify -r 3 -v            # 現状の基準値（apply 初回 ~1.1s・2回目 ~100ms・check 87ms）
cargo test                                           # 462 passed が期待値
```

そのあと `latest.md` §4 の「測り方」を順に進める。計測したら
`python3 docs/loop/l0.py -r 3 --log -n "仮説: …"` で `log.md` に記録し、
考察を `log.md` に、要約を `latest.md` に反映する（反復番号の規則は `latest.md` §5）。
