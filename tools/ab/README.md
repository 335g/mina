# tools/ab — 実 LLM A/B ハーネス（L2）

> 位置づけ: これは **L2（実 LLM・分単位・高分散）**。費用ゼロで何度も回す **L0（LLM なし・
> 決定論的）** は `docs/loop/l0.py`。**L0 で差が出た仮説だけ**をここで確認する — 手順は
> `docs/loop/method.md`。
>
> 計器の契約・結果・限界の本体は **[docs/benchmarks/l2/README.md](../../docs/benchmarks/l2/README.md)**。
> 生データは [history.jsonl](../../docs/benchmarks/l2/history.jsonl)、集計と作図は `report.py`。
> トップの README が引用している `curve.svg` / `safety.svg` も `report.py` が描く。

## 何を測るか（t11 = 現在の主力）

固定した 1 タスク（2 ファイルの Rust クレートにフィールドを 1 つ足す、編集 5 箇所）を、
**行数だけを変えて** 5 規模（合計 150 / 600 / 1,200 / 2,400 / 9,600 行 = 1 ファイルあたり
75 / 300 / 600 / 1,200 / 4,800 行）で走らせる。arm ごとに**与える道具だけ**を変える:

| arm | 道具 |
| :-- | :-- |
| `native` | opencode 標準の read / edit / write / glob / grep（内部 `apply_patch` を含む） |
| `naive` | 全文 read（`mread`）+ 無検証の全体置換（`medit`） |
| `minas` | 番号付き範囲 read（`mread`）+ 検証付き `apply`（`medit`） |
| `positional` | 範囲 read（`mread`、行ごとの char offset 付き）+ 位置指定 `edit`（`medit`） |

**どの arm にも shell はない。** shim を bash 経由で強制しようとすると、モデルは opencode 内蔵の
`apply_patch` や `sed -n` に抜けて比較が崩れる（opencode 1.18 は `apply_patch` に permission
フックが無く、bash の deny パターンも効かない）。そこでファイルアクセスは opencode の
**custom tool**（run ワークスペースの `.opencode/tools/*.ts`）だけにし、遵守は「その run が実際に
呼んだツール名」で判定する（許した道具以外を 1 回でも呼んだ run は採用しない = `comp=C` のみ集計）。

結果の分類は 4 通り（`CORRUPT` = 終了コード 0 のまま意図しない領域が書き換わった = 静かな破壊）:

| 分類 | 判定 |
| :-- | :-- |
| GREEN | 期待した 5 箇所 + `cargo check` が通る |
| LOUD-FAIL | ビルドが壊れた / 検証に弾かれたまま終了（音を立てて失敗 = 安い失敗） |
| INCOMPLETE | 一部が未編集（余計な変更なし） |
| CORRUPT | ノイズ領域が壊れているのに正常終了 |

並行変更の実験は、**ツール非依存**に注入する: run 開始前に watcher を立て、対象ファイルが
外部から変更された直後にマーカー行を追記する。run 終了時にマーカーが消えていれば、その run は
**他人の変更を黙って捨てた**（`ext_lost`）。

## 使い方

```bash
python3 tools/ab/ab.py selftest                  # 計器の自己検証（LLM 費用ゼロ。最初に走らせる）
python3 tools/ab/ab.py fixture t11 --scale 1200  # フィクスチャの行数を確認
python3 tools/ab/ab.py run t11 minas 1 --scale 150 --drift 0   # 1 run
python3 tools/ab/ab.py stats t11 minas 1 --scale 150 --drift 0 # DB から再集計
python3 tools/ab/ab.py sweep --n 5               # 凍結 3 arm × 5 規模 × drift 2 条件 = 105 run
python3 tools/ab/report.py --svg curve.svg --safety-svg safety.svg   # 集計 + 作図
```

- `--arms` / `--scales` / `--drift-only` / `--idx-from` で部分的な再測定ができる
  （例: `sweep --scales 9600 --drift-only --idx-from 11 --n 20 --arms naive,minas`）。
- **掃引は各 run の前に `pkill -f "opencode run"` を打つ。同じ Mac で他の opencode を走らせない。**
- run は `docs/benchmarks/l2/history.jsonl` に **1 行追記**（同じ key の再走は最後の行が採用される）。
  行には計器メタ（commit / generation / build / model / runner）と判定材料
  （facts / noise / cargo / comp / stray / drift_fired / marker / ext_lost）が入る。

環境変数で上書きできる: `OPENCODE_BIN`、`MAB_MODEL`（既定 `opencode/gpt-5.4-nano`）、
`MAB_MINABIN`（既定 `target/debug/minas`）。opencode は PATH より先に env → mise の
インストール先 glob の順で解決する（非対話 shell では mise shim が無いため）。

## selftest が固定しているもの（LLM なし・毎回）

赤ければ掃引を回さない。計器自身のバグで測定点が壊れるのを防ぐ:

- 4 分類の妥当性（地面の真実 → GREEN、ノイズ損壊 → CORRUPT、ビルド不能 → LOUD-FAIL、未編集 → INCOMPLETE）
- 5 規模のフィクスチャ行数（75 / 300 / 600 / 1,200 / 4,800）
- **無検証の全文書戻しは外部変更を消す** / **`minas apply` は同じ状況でも保つ**（安全側の機構テスト）
- 位置指定の `minas edit` 経路が動く（offset と checksum は `minas read` の出力から取る）
- drift 注入が当たる / 当たらなければ `miss` として記録される
- 各 arm の生成した custom tool（`.ts`）の構文（`node --check`）

## 注意（この計器で踏んだ穴）

- `minas read` は daemon が開いているファイルでは **buffer** を返す（ディスクではない）。
  外部書き換えの reload は非同期なので、offset と checksum は同じ情報源から取ること。
- 生成した `.ts` が壊れていると opencode は `undefined is not an object (evaluating 'mo.output')`
  という無関係なエラーを返し、shim は一度も走らない（構文検査を selftest に入れてある）。
- opencode 1.18 は **PWD 環境変数**からセッションのプロジェクトを決める。Python から起動する
  ときは `env["PWD"]` をワークスペースに合わせる（でないと cwd が無視される）。

## t1〜t10（履歴）

`ab.py` には t1〜t10 も残っているが、これらは **`minae session` 時代（改名前）の CLI**を
前提にした古いテストで、シム層は現在の `minas` CLI に合わせて書き直してある。当時の公開数値
（[docs/benchmarks/agent-editor-ab-results.md](../../docs/benchmarks/agent-editor-ab-results.md) や
[agent-editor-evaluation-summary.md](../../docs/benchmarks/agent-editor-evaluation-summary.md)）は
**そのままでは再現できない**（runner・モデル・計器が違う）。現在の主張は t11 の掃引に基づく。
