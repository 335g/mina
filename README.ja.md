# mina

> エージェントに操作させるためのエディタ。常駐デーモンがエディタ状態 —— ドキュメント・undo・LSP —— を保持し、エージェントはファイルを書き換えるのではなく session 契約（検証付きの内容指定編集）を通して編集します。フロントエンド —— エージェントが呼ぶヘッドレス `minas` session CLI、人が使うオプションの TUI —— は、ただのクライアントです。

**mina** = **min**imize cost for AI **a**gent（AI エージェントのためのコスト最小化）—— コストを最小化するエージェントエディタ。サフィックスで: mina**e** = editor（TUI）、mina**d** = daemon、mina**s** = session（ヘッドレス CLI）。

[English](./README.md) · 日本語

**計測した効果**（詳細と生データは [docs/benchmarks/l2/README.md](docs/benchmarks/l2/README.md)、図は下）:

1. **大きいファイルで費用が増えない** —— 4,800 行/ファイルのタスクで入力トークンは素朴な全文 read 比 **−79%**、コスト **−61%**（中央値）。素朴なツールは規模に比例して増え、minas はほぼ横ばい。交差点は 300〜600 行/ファイル。
2. **外部からの変更を黙って消さない** —— 編集の最中に別プロセスが同じファイルを書き換える状況を注入した実験で、**209 run 中 1 件だけ**変更を失い（それは全文を書き戻す arm）、minas の 2 経路は **0/51**。加えて、その失敗モードが実在すること自体を決定論的なテストで毎回検証しています。

---

## はじめに

```console
# 1. daemon + session CLI。エージェント向けツールのみで、TUI は別途
$ cargo install minad minas

# 2. 言語サーバは同梱されない —— 使う言語のものを入れる
$ rustup component add rust-analyzer
$ npm install -g typescript-language-server typescript

# 3. 何が見つかったか確認する: daemon の世代 + 設定済みサーバ
$ minas info
```

デーモンは最初のクライアントから必要に応じて起動されます。常駐させるには `minad serve` を実行します。言語サーバはワークスペースルート単位で起動されるため、`PATH` に通っていればすぐに `symbol` / `rename` / `check` が動きます。tree-sitter による構文ハイライトはそのまま動きます。ソースからは `cargo build --release` でもビルドできます。

### 最初のセッション

```console
$ minas outline src/lib.rs              # 全文なしで構造を把握
$ minas read src/lib.rs --lines 40:60   # 必要な番号付き行だけ
$ minas apply src/lib.rs "old" "new"    # 検証付き編集（古ければ理由つきで拒否）
$ minas check src/lib.rs                # LSP 診断 —— 最終ゲートは build
$ minas rename src/lib.rs "USD" "JPY"   # ワークスペース横断のセマンティック rename
```

`minas apply` は**位置ではなく内容**で対象を特定します。テキストが見つからなければ、黙って書かずに失敗します:

```console
$ minas apply a.rs "let x = 2;" "let x = 3;"
NOT FOUND: "let x = 2;"          # rc=2。ファイルは変更されない
```

read / edit に設定は不要です。挙動を変えるファイルは 2 つだけ、どちらも任意です:

- `~/.config/minae/languages.toml`（`$XDG_CONFIG_HOME/minae/`、デーモン側）: 言語サーバの追加・上書き。埋め込み既定にマージされ、サーバ起動時に読み直されます
- `~/.config/minae/config.toml`（クライアント側、TUI のみ）: `colorscheme` と `agent_command`（TUI がレビューコメントから起動するエージェント）。ユーザーカラースキームは `~/.config/minae/colorschemes/` に TOML で置きます

### エージェントに minas を使わせる

`minas skill` は pull 型です。minas の存在を知らないエージェントは永遠に `minas skill` を呼びません。エージェントが指示を読む場所へ、引き金を一度だけ配ります:

```console
$ minas skill --md > ~/.claude/skills/minas/SKILL.md   # ~/.pi/agent/skills/minas/SKILL.md でも同じ
$ minas skill --md >> AGENTS.md                         # リポジトリの指示文に直接追記してもよい
```

ラッパーは棚の写しを持たず（「`minas skill` を実行せよ」だけ）、トピックが増えても腐りません。ファイル末尾の刻印と `minas info` の `cli_generation` が違っていたら再生成してください。TUI は `minae [file ...]` です。

## mina とは

mina は daemon/client 分割のターミナルエディタです。エージェント駆動の編集を第一に、対話利用はそれに次ぐ位置づけです。

常駐する **Daemon** がすべてのエディタ状態を保持します —— 開いているドキュメント、undo 履歴、セレクション、LSP セッション、外部変更の検知。**Client**（ヘッドレス `minas` CLI・エージェント・オプションの TUI）はローカルソケットで接続し、コマンドを送って状態スナップショットを描画します。クライアントは出入りしても、デーモンとその状態は残ります。すべてのフロントエンドが同じプロトコルを話すため、エージェントがスクリプトで使うツールは TUI が対話で使うツールと同じであり、TUI はなくても構いません。

エージェントにとって重要な帰結が 2 つあります: **編集がデーモンの状態に対して検証される**こと、そして**外部変更が検知されリロードされる**ことです。どちらも「静かに壊さない」ための土台で、下の[主張 2](#主張-2-静かに壊さない)で計測します。

## できること

- **編集コア**（UI 非依存）: ドキュメント・セレクション・undo グループ・Normal / Insert / Select モード・検索・外部変更のリロード
- **LSP による言語サポート**（ワークスペースルート単位）: 診断・inlay hints・定義 peek・セマンティック rename / references
- **言語サーバ**（ADR-0030 に基づく検証済みの埋め込み）: Rust に `rust-analyzer`、TypeScript に `typescript-language-server` —— 同じ2言語分の tree-sitter 構文ハイライトも同梱。ほかの LSP はユーザーの `languages.toml` で追加可能
- **ヘッドレスエージェントインターフェース**: `minas`（下表）
- **ターミナル UI**: `minae` TUI（ratatui/crossterm、接続別 View）
- **設定可能**: `languages.toml`（デーモン側）、`config.toml` とユーザーカラースキーム（クライアント側）、エージェント向けビルトインの `minas skill` ガイド

| コマンド | 何をするか |
| :-- | :-- |
| `read --lines a:b` / `--span` | 有界な番号付き read。全文を読まない |
| `apply <path> <old> <new>` | **内容指定**の検証付き編集（`--whole` / `--hunks-stdin` / `--pair` なども） |
| `edit --path <p> <json>` | **位置指定**の編集。`expected_text` 必須（後述） |
| `check <path>` | 診断の settle を待って報告（`wait` + `get` + JSON パースの代替）。エラー診断があれば exit 2 |
| `rename <path> <old> <new>` | ワークスペース横断のセマンティック rename（LSP） |
| `references` / `symbol` / `at` / `hover` | セマンティック検索と位置解決 |
| `outline` / `hints` / `peek` | 全文なしで構造・inlay hints・定義を取得 |
| `wait <generation>` | 解析（解析中の診断など）が進むまでブロック |
| `exec <command>` / `get` / `delete` / `search` | 生のプロトコルコマンド・状態スナップショット |
| `skill [topic]` | エージェント向けガイドの棚（索引 → トピック別） |
| `info` / `review` / `help` | デーモンと設定の確認・レビューコメント・ヘルプ |

各コマンドは JSON を返します。編集が拒否されたら該当範囲を読み直して再試行してください —— 拒否メッセージは何が一致しなくなったかを示します。

---

## 主張 1: 規模が大きくなっても費用が増えない

![費用は規模に対して増えない](docs/benchmarks/l2/curve.svg)

固定した 1 つのタスク（2 ファイルの Rust クレートにフィールドを 1 つ足す、編集 5 箇所）を、
**行数だけを変えて** 5 つの規模で走らせました（ノイズ行を足すだけで、難易度は変わりません）。
3 つの arm は同じプロンプト・同じモデル・同じ runner で、使えるツールだけが違います:

| arm | 与えたツール |
| :-- | :-- |
| `native` | opencode 標準の read / edit / write / glob / grep（+ 内部 `apply_patch`） |
| `naive` | 全文 read + 無検証の全体置換（いわゆる素朴なシム） |
| `minas` | 番号付き範囲 read + 検証付き `apply`（minas の session 契約） |
| `positional` | 番号付き範囲 read + 位置指定 `edit`（minas 自身のもう 1 つの経路。結果は下） |

すべての arm から **shell を外して**あります。シムを shell 経由で強制しようとすると、モデルは
`apply_patch` や `sed -n` に抜けてしまい比較が崩れるためです（[計測方法](#計測方法と再現)）。
遵守は「その run が実際に呼んだツール名」で判定し、許したツール以外を 1 回でも呼んだ run は
**測定として採用しません**（採用 209 run / 除外 31 run）。

### 入力トークンとコスト（中央値、drift 注入なし）

| 総行数 | 行/ファイル | native | naive | **minas** | minas / native | minas / naive |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 150 | 75 | 13,226 | 10,627 | 13,973 | +3% | +1% |
| 600 | 300 | 17,950 | 20,749 | 19,130 | +8% | −4% |
| 1,200 | 600 | 26,619 | 42,827 | **18,859** | **−29%** | **−58%** |
| 2,400 | 1,200 | 46,428 | 67,799 | **20,106** | **−60%** | **−70%** |
| 9,600 | 4,800 | 56,036 | 83,422 | **17,770** | **−70%** | **−83%** |

コスト（$、中央値）も同じ形です: 4,800 行/ファイルで **minas $0.0101** 対 native $0.0207・naive $0.0257。

読み取れること:

- **minas の費用は規模に対してほぼ一定**（14k → 17.8k、横ばい〜微増）。必要な範囲だけ読むからです。
  native / naive は全文を読むので行数に比例して増えます。
- **交差点は 300〜600 行/ファイル**。それより小さいファイルでは minas のほうが高い（150 行では +6%）。
  つまり「いつも得する」ツールではありません。**得するのは、1 ファイルが大きいとき**です。
- 最大規模では **入力 −68〜−79%、コスト −51〜−61%**。

### 統計的な強さ（正直に）

| 比較 | 条件 | n | 入力差 | p（符号検定） |
| :-- | :-- | ---: | ---: | ---: |
| minas / native | 4,800 行/ファイル | 10 | **−70%** | 0.002 |
| minas / naive | 4,800 行/ファイル | 5 | −83% | 0.062 |
| minas / naive | 4,800 行/ファイル + drift | 25 | **−84%** | **< 0.001** |
| minas / native | 全規模まとめ | 35 | **−33%** [−56%, −14%] | 0.002 |
| minas / naive | 全規模まとめ | 25 | −58% [−70%, −4%] | 0.015 |

（比率は同一 idx の paired 比の中央値。表 1 の中央値そのものの比とは少し違います）

n=5 では符号検定の下限が p=0.0625 なので、規模ごとの 5 run だけでは「有意」とは言えません
（n と最小検出効果の計算は [docs/benchmarks/l2/README.md](docs/benchmarks/l2/README.md)）。
だから 4,800 行/ファイルの条件だけは 25 run まで増やしてあります —— そこが唯一、**同じ向きに 25/25** で
p < 0.001 になる点です。数値はモデル・runner・n に依存するので、**モデルは opencode/gpt-5.4-nano、
runner は opencode 1.18.31、build は debug、日付は 2026-09-16**、という条件つきで読んでください。

## 主張 2: 静かに壊さない

![壊さない](docs/benchmarks/l2/safety.svg)

エージェント編集の壊れ方は 2 種類あります。**大きな音を立てて壊れる**（ビルドが通らない、編集が
拒否される）のは安い失敗です。高くつくのは**静かに壊れる** —— 終了コードは 0 で、エージェントは
「完了しました」と言い、ファイルは壊れている。mina の中心的な主張はここです。

それぞれの run を 4 分類しました:

| 分類 | 意味 |
| :-- | :-- |
| **GREEN** | 期待した 5 箇所が正しく編集され、`cargo check` が通る |
| **LOUD-FAIL** | ビルドが壊れた、または検証に弾かれたまま完了 —— 音を立てて失敗（安い失敗） |
| **INCOMPLETE** | 一部が未編集（余計な変更なし） |
| **CORRUPT** | 終了コード 0・成功と報告しているのに、**意図しない領域が書き換わっている** |

結果: **採用 209 run 中 CORRUPT は 0 件**（ノイズ行の破損も 0）。内訳は

| arm | n | GREEN | LOUD-FAIL | INCOMPLETE | CORRUPT |
| :-- | ---: | ---: | ---: | ---: | ---: |
| native | 56 | 51 | 5 | 0 | **0** |
| naive | 55 | 55 | 0 | 0 | **0** |
| minas | 76 | 74 | 2 | 0 | **0** |
| positional | 22 | 17 | 3 | 2 | **0** |

### 並行変更を黙って消さない

決定的なのはここです。エージェントが作業している最中に、**別プロセスが同じファイルを書き換える**
状況（よくある: formatter、git 操作、別のエージェント、人間の手）を注入しました。注入は
ツール非依存で行い、注入が実際に当たった run だけを数えています（102 run で発火、未発火 0）。

| arm | 規模 | 発火した run | **変更を失った run** |
| :-- | ---: | ---: | ---: |
| native | 4,800 行/ファイル | 11 | 0 |
| naive | 4,800 行/ファイル | 25 | **1** |
| **minas** | 4,800 行/ファイル | 30 | **0** |

`naive` の 1 件は「全文を読む → 置換 → 全文を書き戻す」経路が、**読んだ後に起きた外部変更を
上書きした**ものです。run は正常終了し、エージェントは成功を報告し、`cargo check` も通ります
（失われたのは未使用の定数 1 行なので）。これが典型的な静かな破壊です。

minas が同じ状況で消さないのは、**編集を位置で指定しないから**です。`apply` はデーモンが
ディスクから読み直した現在のテキストに対して「その文字列」を置換対象にします。古いビューで
書いた内容を丸ごと書き戻す操作が存在しないので、他人の変更を巻き戻す経路がありません。

n=25 で 1 件なので、これは**頻度の推定ではなく機構の実証**です（95% 信頼区間はおよそ 0.1〜20%）。
そこで、計器自身が**LLM を使わずに毎回**次を検証するようにしてあります（`ab.py selftest`、
費用ゼロ・1〜2 分。赤ければ掃引を回しません）:

- **無検証の全文書戻しは外部変更を消す** —— 失敗モードが実在することの証明
- **`minas apply` は同じ状況でも保つ** —— 契約が守ることの証明
- **位置指定 `edit` は `expected_text` 無しで拒否される** —— 後述
- drift 注入が当たる／外れたら検出 —— 安全指標そのものの検証
- 各 arm の custom tool の構文 —— 計器の生成ミスを検出

### なぜ「位置指定」を既定にしないのか

minas には位置指定の `edit` もありますが、**`expected_text`（その範囲にある現テキスト）を
要求します**。無しで実行しようとすると拒否されます:

```console
$ minas edit --path a.rs '{"start":0,"end":2,"text":"fn","checksum":1}'
Error: positional edit requires expected_text (the old text at start..end) — document
checksum cannot detect a shifted range. Use `minas apply` for content-addressed editing (rust2 #5-2)
```

理由は「ずれた位置への編集は、ファイル全体の checksum では検出できない」からです。実際に
位置指定の arm を走らせて測ると、その代償ははっきり出ます（[図](docs/benchmarks/l2/safety.svg) 中央）:

| 条件 | positional ÷ apply（入力トークン、同一 idx の中央値） |
| :-- | ---: |
| 75 行/ファイル | **2.61x** |
| 75 行/ファイル + drift | 2.21x |
| 4,800 行/ファイル | 1.95x |
| 4,800 行/ファイル + drift | **4.11x** |

1 編集に `Open` + `edit` + `Save` の 3 往復かかり（`apply` は 1 往復）、さらにモデルが計算した
offset が外れて却下される（1 run あたり中央値 1〜5 回。`apply` は 0 回）。結果として 22 run 中
5 run が非 GREEN でした。つまり位置指定経路の欠点は「黙って壊す」ではなく、**高くつき、
しばしば却下される**ことです。だから README で勧めるのは `apply` です。

### この主張の限界

- CORRUPT 0 件は「頻度が 0」の証明ではありません。n=22〜76 で 0 件なら、上限はおよそ 4〜15% です。
  言えるのは「その失敗モードは決定論的に実在し、minas の契約では観測されなかった」まで。
- 「壊さない」の対象は**並行変更の消失**と**意図しない領域の書き換え**です。意味的に間違った
  編集（正しい場所に間違った内容を書く）は防ぎません。それはエージェントの仕事です。

## 主張 3: トレードオフは何か

正直に書きます。minas は**遅い**です。4,800 行/ファイルの条件で wall は **58〜68 秒 対 native の
42〜44 秒**（中央値。同一 idx の比では +6〜+61% で、大半の条件が +48〜+61%）。step 数も増えます
（12〜14 対 7〜9）。

理由は 3 つあり、いずれも費用と引き換えです: 範囲 read は複数回に分かれる、`apply` は
デーモンとの往復を含む、そして何より**検証と再試行の分だけモデルのターンが増える**
（拒否されたら読み直して出し直す）。エージェントの待ち時間より、**間違ったまま完了する確率**を
下げる側に振ってある、という設計判断です。

コスト面の注意: minas が削るのは**入力トークン**です。トークン単価が下がっても、コンテキストに
入る量が規模に比例しないことの価値は減りません（むしろ小型で安いモデルを長く走らせるほど効きます）。
逆に、**1 ファイルが 300 行未満の作業では minas は払い損**です（図 1 の左端）。

## どこで元が取れるか

- **効く**: 大きいファイル（数百行〜）を、多数のターンにわたって編集する長い作業。小さい・安い
  モデルを使うとき。並行して書き換わるファイル（formatter・複数エージェント・人間）を触るとき。
- **効かない**: 小さいファイルを数箇所だけ直す短い作業。1 回の read で足りるなら全文 read で十分です。

## 計測方法と再現

計器は [`tools/ab/`](tools/ab/README.md) にあります（[`docs/benchmarks/l2/README.md`](docs/benchmarks/l2/README.md)
が契約と結果の本体）。

```console
$ cargo build                              # minas / minad
$ python3 tools/ab/ab.py selftest          # 計器の自己検証（LLM 費用ゼロ）
$ python3 tools/ab/ab.py sweep --n 5       # 105 run / 約 1.7 時間 / 約 $1.5
$ python3 tools/ab/report.py --svg curve.svg --safety-svg safety.svg
```

- **1 run = 1 行を JSONL に追記**（[`docs/benchmarks/l2/history.jsonl`](docs/benchmarks/l2/history.jsonl)）。
  同じ key の再走は最後の行を採用し、履歴は消しません。
- 集計は**遵守できた run（comp=C）のみ**。除外は必ず件数を報告します。
- 効果量は**同一 idx の paired 比**の中央値 + bootstrap 95% CI。p 値は符号検定（分布を仮定しない）。
- 依存ゼロ（標準ライブラリのみ）。図も含めて `tools/ab/report.py` が生成します。
- 以前公開した A/B（旧ハーネス・旧モデルで −41%）は、現在の曲線と直接は比較できません。
  同じ規模（1,200 行）を今の計器で測ると minas/naive −58%、minas/native −29% です。
  数値を引用するときは、**モデル・runner・n・日付を併記**してください。

## アーキテクチャ

- **Daemon**: ドキュメント・undo 履歴・セレクション・LSP クライアント・構文ハイライト・外部変更の検知を保持。ローカルソケットでクライアントにサービス提供
- **mina-text**: UI 非依存の編集コア —— ドキュメント・セレクション・トランザクション
- **mina-protocol**: daemon/client IPC のワイヤ型
- **mina-lsp**: LSP クライアント（起動・JSON-RPC・位置変換）
- **mina-view** / **mina-loader**: UI 非依存のエディタ状態 / tree-sitter 文法とハイライトクエリ
- **mina-conn**: session CLI と TUI が共有するクライアント側の接続・リクエスト配線
- **minad**: daemon バイナリ
- **minas**: ヘッドレス session CLI と skill ガイド
- **minae**: TUI（ratatui/crossterm、接続別 View）

## ドキュメント

- [CONTEXT.md](CONTEXT.md) — ドメインモデルの正規用語集
- [docs/helix-architecture.md](docs/helix-architecture.md) — アーキテクチャと設計ノート
- [docs/benchmarks/](docs/benchmarks/) — エージェントエディタの A/B 計測（[L2 掃引の本体](docs/benchmarks/l2/README.md)・[評価サマリ](docs/benchmarks/agent-editor-evaluation-summary.md)）
- [docs/adr/](docs/adr/) — 意思決定記録
- [docs/spec/](docs/spec/) — ワイヤプロトコル仕様

mina は開発中です。荒い部分がある前提でお願いします。

## ライセンス

MIT — [LICENSE](LICENSE) を参照。

Copyright (c) 2026 335g
