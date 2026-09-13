# 配布用スキルラッパー（`minas skill --md`）— 本文を二重に持たずに引き金だけ配る

`minas skill` は**バイナリ同梱・pull 型**のスキル棚として設計してある（`skills.json` を
データ源に、無引数 = 薄い索引・`<topic>` = 本文）。T6〜T8 の L2 実測（skill 本文を読ませても
判断は変わらずトークン +21〜61%、`docs/benchmarks/agent-editor-evaluation-summary.md`）が
支持する範囲はそこまでで、**測っていない穴が 1 つ残っている**: 「読ませ方」ではなく
**「呼ばせ方」**。minas の存在を知らないエージェントは `minas skill` を永久に呼ばない。

mina 自身の開発ではこの穴は AGENTS.md の 1 文が塞いでいる（「rust を参照・編集するときは
minas。`minas skill` 参照」）。しかしそれは mina のリポジトリにしか無く、**minas のユーザの
リポジトリには存在しない**。README も棚の存在は説明しているが、エージェントにそれを最初に
打たせる手段は配っていない。

Status: accepted

## Decision

`usage` トピックを `skills.json` の**先頭**に足し、`minas skill --md [topic]` が
**配布用ラッパー**（YAML frontmatter + 本文 + ビルド世代の刻印）を stdout に出す。

- **本文はラッパー側に持たない**。正本は `skills.json` のままで、`--md` はそれを写すだけ。
  **索引の写しも埋め込まない**（「最初に `minas skill` を実行せよ」とだけ書く）。
  ラッパーが持つのは「いつ使うか」という *when/why* ＝ 既存 14 トピック（全部 *how*）に
  無かった空白と、**最初の 1 コマンドより前に context に無いと壊れる規則 3 つ**だけ:
  位置計算禁止（`apply` が正面入口・`edit` は `expected_text` 必須）/ `check` は build で
  はない / fallback は申告する。
- **索引を写さない理由**: 写すとトピックを足すたびにラッパーが腐る。既にその形の重複が
  1 つある（`~/.pi/agent/skills/dogfooding-driver/SKILL.md` のコマンド表は flags まで写して
  おり、ADR-0060 で `minas delete` を足す手間が発生している）。配布物には増やさない。
- **刻印**: `Generated from minas <cli_generation> (build_ts <ts>) by minas skill --md`。
  配布物は「ユーザが一度貼ったきり」なので、古い文言を使い続ける事故を検知できるようにする
  （`minas info` の `cli_generation` と比較 → 再生成）。
- **書き込まない**: `--install <dir>` のような自動配置はしない。ランナーごとに置き場が
  違い（pi / Claude Code / codex / AGENTS.md）、テストできるのは pi だけなので、stdout に
  出して利用者・エージェントが置く。ユーザのリポジトリの指示ファイルを勝手に書き換えるのは
  侵襲的でもある。
- `--md <topic>` はそのトピック版（frontmatter の `name` は `usage` だけ `minas`、他は
  `minas-<topic>`）。未知トピックは通常の `minas skill` と同じ文言で exit 1。

## Considered Options

- **ランナー別の SKILL.md を手書きで同梱**: 却下 — 本文の写しが 2 つ以上になり、ADR ごとの
  「skill を更新」が 3 箇所になる。`cargo install` で配れない（バイナリと別経路）。
- **索引ごとラッパーに埋め込む**: 却下 — トピック追加のたびに腐る。索引は `minas skill`
  が 1 呼び出し・15 行で出すので、埋め込む価値が無い。
- **`minas setup` / `--install`（検出したランナーの skill ディレクトリへ書き込む）**:
  却下（今は）— 副作用つきの書き込みと runner 検出を持ち込む割に、検証できる runner が pi
  しかない。stdout なら全 runner を同じ経路でカバーできる。
- **skill の内容をさらに厚くする（判断の一行ヒントを本文に足す）**: 却下 — T6/T8 の実測
  （本文を読ませても判断は変わらない）と逆行する。判断はツール説明と契約の側に置く。

## Consequences

- 配布は「`minas skill --md` を貼る」の 1 経路になり、内容の更新はバイナリの更新に追従する
  （ビルド世代の刻印でずれを検出できる）。
- 索引が 15 行になる（先頭が `usage`）。薄さの設計要件は維持。
- **効くかどうかは未測定**。T6〜T8 は「読んだ場合」しか測っておらず、現状の dogfooding
  loop は driver に必ずブリーフするので「知らないエージェントが手を伸ばすか」を測っていない。
  判定の計器は既にある: (a) ラッパーだけを渡したセッションと、コマンド表を渡したセッションの
  `fallbacks` 比較、(b) 指示を一切出さないセッション。0 回なら「ラッパーが無いと発見され
  ない」が実測で確定し、出るなら表を縮められる。
- ラッパーは *when* しか持たないので、*how* は依然として 1 呼び出し先（`minas skill`）に
  ある。エージェントが `minas skill` を打たない運用では、このラッパーは何も保証しない。
