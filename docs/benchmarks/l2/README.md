# L2 — 規模掃引による効果測定

`docs/loop` の L0（開発判断用・無料・決定論的）とは別に、**外向けの主張を作るための実測**を
ここに置く。1 run = 1 行の JSONL（`history.jsonl`）を git に残し、集計器で読む。

## 走らせ方

```sh
cargo build                                    # minas / minad（debug で可）
python3 tools/ab/ab.py selftest                # 計器の自己検証（LLM 費用ゼロ・1〜2分）
python3 tools/ab/ab.py sweep --n 5             # 105 runs / 約 1.7 時間 / 約 $1.5
python3 tools/ab/ab.py --help                   # 位置指定 arm などの追加実験も同じ runner
python3 tools/ab/report.py --svg curve.svg --safety-svg safety.svg   # 素の集計も stdout に出る
```

`curve.svg`（費用の 3 パネル）と `safety.svg`（壊さないの 2×2）はトップの README が引用している。
どちらも履歴 JSONL から直接描くので、数字が図と表で食い違うことがない。

凍結した 3 arm の掃引（native / naive / minas）を壊さないため、追加 arm は
`--arms positional` のように明示したときだけ走る。

掃引は各 run の前に `pkill -f "opencode run"` を打つので、**同じ Mac で他の opencode を
走らせない**。測定は `history.jsonl` に追記され、同じ key（task/scale/drift/arm/idx）の
再走は「最後の行」が採用される（履歴は消さない）。

## 計器（何を固定し、何を変えるか）

| 項目 | 内容 |
| :-- | :-- |
| runner | opencode 1.18.31、`opencode/gpt-5.4-nano`、`--agent <固定> --pure` |
| タスク | `t11` 固定。5 項目の機能追加（`src/config.rs` と `src/main.rs`）。変えるのは**行数だけ** |
| 規模 | 2 ファイル合計 150 / 600 / 1,200 / 2,400 / 9,600 行（ファイルあたり 75〜4,800 行） |
| 反復 | 各セル n=5。arm を idx ごとに**交互**に走らせる（paired 設計で分散を消す） |
| drift | 最初の編集の直後に**外部プロセスが** `src/main.rs` の末尾へ `const EXTERNAL_REVISION: u64 = 7;` を追記する |
| 判定 | GREEN / INCOMPLETE / LOUD-FAIL（ビルド不能）/ CORRUPT（ノイズ領域を壊したまま正常終了） |
| 安全 | `ext_lost` = 外部変更を最後のファイルから消したまま正常終了した run |
| 遵守 | arm ごとに許した tool 以外が 1 回でも呼ばれたら NC（測定に使わない） |

### arm の定義（3 arm とも **bash を持たない**）

| arm | ファイルアクセス | 意味 |
| :-- | :-- | :-- |
| `native` | `read` / `edit` / `write` / `glob` / `grep` / `apply_patch` | 素朴な最強の対照（弱い対照にしない） |
| `naive` | custom tool `mread`（**全文**）+ `medit`（**無検証**の先頭一致置換） | シェルで `cat`/`sed` するのと同じ契約 |
| `minas` | custom tool `mread`（**範囲**・行番号付き）+ `medit`（**検証付き** apply） | mina の契約（ADR-0048/0029） |

### なぜ bash を外したか（実測）

opencode 1.18 では bash がある限り道具面を強制できない。確認した 3 点:

1. `apply_patch` は PATH のコマンドではなく **bash ツール内部の特例**で、permission フックを持たない。
   しかも opencode 自身が「手編集には常に apply_patch を使え」とモデルに指示している。
2. **bash の permission deny パターンが効かない**（`{"cat":"deny"}` も `{"cat*":"deny"}` も、
   `--auto` の有無に関わらず `cat` が実行された）。
3. prompt で apply_patch を禁じると **`perl -0777 -i -pe` に切り替わる**。

そこで bash を落とし（`tools: {bash: false}` は効く）、ファイルアクセスは opencode の custom
tool（`.opencode/tools/*.ts`）だけにした。これで「モデルの気まぐれ」ではなく**道具面そのもの**
が変数になる。採用 209 run のうち**遵守 C は 209/209**（NC ゼロ。除外した 31 run は計器故障時の測定で、
遵守違反ではない）。

## 結果1: 費用は規模に対して増えない（主張の本体）

中央値、drift 無し。`summary.md` は `report.py` の生出力。

| ファイルあたり行数 | native | naive | **minas** | minas/native | minas/naive |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 75 | 13,226 | 10,627 | **13,973** | +3% | +1% |
| 300 | 17,950 | 20,749 | **19,130** | +8% | -4% |
| 600 | 26,619 | 42,827 | **18,859** | -29% | -58% |
| 1,200 | 46,428 | 67,799 | **20,106** | -60% | -70% |
| 4,800 | 56,036 | 83,422 | **17,770** | **-70%** | -83% |

（入力トークン。`curve.svg` が同じデータの図。コストも同形で、最小・最大の比は
native 比 +9% → -52%、naive 比 +16% → -68%。トップの README はこの表と図を引用している）

- **minas の入力トークンは規模に対してほぼ一定**（14.0k → 17.8k）。native と naive は行数に
  ほぼ比例して増える。**曲線の形そのものが主張**で、単一の削減率より強い。
- **交差点はファイルあたり 300〜600 行の間**（2 ファイル合計 600〜1,200 行）。これが
  「どこから mina を使うべきか」の境界線。**小さいタスクでは mina は負ける**（+3〜+8%）。
- 最小・最大の paired 比（同一 idx）: 4,800 行で入力 **-70%**・コスト **-52%**（n=10、
  10/10 が同方向、符号検定 p=0.002）。
- **n を挙げたのは 4,800 行の drift 条件だけ**（idx 11〜30 を追加、n=25）。
  **minas/naive = -84% 入力・-70% コスト**、25/25 が同方向（p<0.001）、
  bootstrap 95% CI は [-85%, -78%]。ここだけは推測ではなく推定として言える。
- **wall は minas が一貫して遅い**（4,800 行で +49%、条件によって +6〜+61%）。往復を減らす代わりに
  1 往復が重い。「速く正解に着く」ではなく「**安く着く**」であり、正直に併記する。
- 品質は同等: 凍結 3 arm の採用 187 run で GREEN 180・LOUD-FAIL 7・CORRUPT 0。
- drift の影響は小さい（入力トークンで naive +1%、minas +8%）。minas は再読が範囲 read なので
  絶対値が変わらず最小（4,800 行で 17.4k vs naive 84.5k）。

## 結果2: 並行変更を黙って消さない（安全側）

`ext_lost` = 外部プロセスが足した変更が最後のファイルから消えているのに、run は 5 項目を
満たし `cargo check` も通っている（= タスクの外側を黙って壊した）。

| 規模 | native | naive | minas |
| ---: | ---: | ---: | ---: |
| 75 行 | 0 / 5 | 0 / 5 | 0 / 6 |
| 4,800 行 | 0 / 6 | **1 / 25** | **0 / 25** |

- 機構は決定論的に示せる（`ab.py selftest` に含まれる・LLM 不要）: 「読む → 外部が追記 →
  古い内容で全体を書き戻す」で追記は消える。同じ状況で `minas apply` は追記を保つ
  （open のたびにディスクを見るため）。
- LLM 実測では 4,800 行で naive が 1/25 回消した。minas は 0/25。**率は 1/25 で CI が広く
  （約 0.1〜20%）、頻度を主張できる n ではない。** 言えるのは「**その失敗モードは実在し、
  検証付き契約では観測されなかった**」までで、決定論的な機構の証明と合わせて読む。
- drift 下のコスト: 3 arm とも入力トークンの増分は +1〜+8% で小さい（drift の入る位置が
  タスク範囲外のため）。minas の絶対値は変わらず最小。

## 結果3: 位置指定（offset）経路は高い（minas 自身の第2の編集経路）

`minas` には内容指定の `apply` 以外に **位置指定の `edit`** がある（モデルが char offset を
渡す）。それは実際の CLI なので、strawman ではなく **minas 自身のもう一つの経路** を測った
（`--arms positional`）。

| 条件 | positional / minas（入力） | 同（コスト） | 同（wall） | 非 GREEN |
| :-- | ---: | ---: | ---: | :-- |
| 150 行・drift 無 | **2.61x** | 3.46x | 2.34x | 3 / 7 |
| 150 行・drift 有 | 2.21x | 2.78x | 1.97x | 1 / 5 |
| 4,800 行・drift 無 | 1.95x | 2.66x | 1.96x | 1 / 5 |
| 4,800 行・drift 有 | **4.11x** | 3.41x | 1.78x | 0 / 5 |

（同一 idx の paired、n=5。中央値。`summary.md` に絶対値表）

- **位置指定は minas の内容指定より 2〜4 倍高い。** 理由は計器が数えた通り: 1 編集に
  `Open` + `edit` + `Save` の 3 往復が必要（`apply` は 1 往復で検証と保存まで終わる）、
  さらに**モデルの計算した offset が外れて却下される**（1 run あたり中央値 1〜5 回。apply は 0）。
- 遂行率も低い（22 run 中 5 run が非 GREEN。同じ条件の apply arm は 22 run 中 1 run）。
- **それでも CORRUPT は 0。** これは偶然ではなく構造の帰結: `minas edit` は
  `expected_text`（対象範囲の現テキスト）が無いと **実行を拒否する**。実際に拒否される:

  ```
  $ minas edit --path f.rs '{"start":0,"end":9,"text":"...","checksum":...}'
  Error: positional edit requires expected_text (the old text at start..end) —
  document checksum cannot detect a shifted range. Use `minas apply` for
  content-addressed editing (rust2 #5-2)          # rc=1
  ```

  「offset がずれても全文 checksum では検出できない」という失敗モードは minas 自身が
  既知で、CLI がその形の実行を許さない。つまり **offset 経路は「高くつく」が「黙って壊す」
  には至らず、ずれは見える却下になる**。だから README で勧めるのは `apply` の方。
- この arm は公平側に倒してある: `mread` は各行の `off`（正確な文字位置）を返す。
  モデルに文字を数えさせず、「どの範囲を選ぶか」だけを測る。これを外すと
  arm は「LLM は文字を数えられない」を測ってしまう。

## 計器が決定論的に固定している安全側の性質（LLM なし・`selftest`）

頻度の推定は n が足りないが、**機構は費用ゼロで毎回検証できる**。`ab.py selftest` が
次を assert する（赤ければ掃引を回さない）:

| 検査 | 意味 |
| :-- | :-- |
| 無検証の全体書戻しは外部変更を消す | naive 系の契約の失敗モードが実在すること |
| minas apply は外部変更を保つ | 同じ状況で minas は消さないこと |
| 位置指定（minas edit）経路が動く | offset 経路が「計器の故障」で 0 点にならないこと |
| 各 arm の `.opencode/tools/*.ts` の TS 構文 | 生成ミスを opencode の不可解なエラーで発見しないため |
| drift 注入が当たる / 外部変更を失ったら検出 | 安全指標 `ext_lost` 自体の検証 |


- **頻度の主張はできない。** n=5（drift は n=25）で、比の bootstrap 95% CI は広い
  （全規模込みの -29% は [-60%, -10%]）。規模ごとの値は 5/5 同方向の一致性で読んでいる。
  より厳しく言うなら規模点ごとに n=20 が要る（`report.py` の CI がそのまま根拠）。
- **モデルは nano 1 種のみ。** 上位モデルではトークン差が縮むと予測しているが未測定。
- **`build=debug` の minas で測定。** L0 が見ている起動固定費の差はここには効かない
  （フィクスチャに LSP が無く、1 run のコストはモデルの往復が支配的）。
- 「壊さない」の対象は**並行変更の消失**に限る。位置（char offset）をモデルに計算させる
  経路は**測定済み**（結果3）: 2〜4 倍高くつき、遂行率も低いが、**CORRUPT は 0**。
  つまり offset 経路の欠点は「黙って壊す」ではなく「高くつく・しばしば却下される」。
- **4 arm すべてで CORRUPT=0、noise 損壊 0。** この計器で「黙って壊す」頻度を推定することは
  できない（n=20 で 0 件なら上限はおよそ 15%）。言えるのは「その失敗モードは決定論的に
  実在し、minas の契約では n=45 の観測で 1 件も出なかった」まで。
- 期間中の runner 修正: 測定開始時、全 shim が**既に存在しない `minas session` サブコマンド**を
  呼んでいた（現行 CLI は `minas read` / `apply` / `check` / `rename`）。修正前の drift 30 run は
  注入が発火していないため `"invalid": "drift-injector-v1"` として履歴に残し、集計から除外している。

## 次

1. 上位モデル 1 種を最小・最大の 2 点で（20 runs・$20〜60）。**どの指標の差が残るか**を見る
   （トークン差は縮み、検証の差は残る、と予測）。
2. 計器の既知の限界を埋める: `selftest` の決定論的検査は daemon の状態に依存するので、
   CI で回すなら daemon を専用に立てる。
3. README に数字を載せるときは、n / model / build / 日付 / 除外 run 数を併記する。

## 計器を作る途中で見つけた穴（同じ穴を掘り返さないために）

- **`minas read` は daemon が開いているファイルでは buffer を返す**（ディスクではない）。
  外部書き換えは非同期に reload されるので、直後の read は古い内容を返しうる。
  位置指定 arm は read と checksum を同じ情報源から取ることで、オフセットが
  モデルの見たテキストと必ず一致するようにしてある。
- **生成した custom tool の TS が壊れていると opencode は `undefined is not an object
  (evaluating 'mo.output')` という無関係なエラーを返し、shim は一度も走らない。**
  位置指定 arm の medit が 13 回連続で失敗した原因は文字列連結ミスだった。今は
  `selftest` が `node --check` で各 `.ts` を検める。
- **`build_workdir` の `rmtree` は `cargo check` の `target/` を消し切れないことがある**
  （Directory not empty）。3 回まで再試行する。
- **opencode 1.18 の `bash` は zsh を実行し、`~/.zshrc` の `mise activate` が PATH を
  再構築する**ので、PATH 前置きの shim は無効化される（ZDOTDIR を空にして回避）。
  これは「bash を外す」設計にした理由の一つ。
