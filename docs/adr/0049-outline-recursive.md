# outline のモジュール横断(--recursive / --depth)

第3回検証(docs/gitignore/study/minas-hypothesis-study.md #3)で指摘:
`minas outline <path>` は LSP の `textDocument/documentSymbol` を **1
ファイルに対して** 投げる(ADR-0031)ため、「ファイル分割されたモジュール」
(`mod model;` と書かれた lib.rs の中身 = 別ファイル model.rs)の
**children が空**になる。多ファイル構成では構造把握にファイル数分の
outline コールが必要で、「1ファイルの outline で全体構造を掴む」ことが
できない。

Status: accepted

## Decision

`minas outline <path>` に **`--recursive`(モジュール横断)と `--depth N`
(最大深さ)** を追加する。`--recursive` 指定時、Module シンボルの children が
空(ファイル分割モジュール)なら、そのモジュール名位置へ
`textDocument/definition` を投げて**定義ファイルを解決 → そのファイルを
再帰的に outline して children に埋める**。応答形状は既存
`ServerMessage::Outline` を再利用(ツリーは children が深くなるだけ)。

- プロトコル: `Command::OutlineRecursive { path: String, depth: u32 }` を
  **追加**(既存 `Command::Outline { path }` の形状は変えない — 既存分岐の
  `serde_json::from_str::<Command::Outline>` 完全一致マッチ(daemon.rs:1486)を
  壊さないため)。応答は既存 [`ServerMessage::Outline`] を再利用。
- **深さの既定値**: CLI 側で 3(`--depth` 未指定時)。`--depth 1` = 直接の子
  モジュールまで。0 は許容しない(CLI が拒否)。
- **検出**: ツリーの Module シンボルで children が空のものを候補にする。
  その `selection_range`(モジュール名トークン)の先頭位置を line:col に変換し、
  既存の definition 経路(lsp.rs の `textDocument/definition` + 
  `first_definition_target`)で解決。解決先が**同一ファイル内**なら
  inline `mod { }` と判断して再帰しない。
- **防御(無限ループ・応答爆発防止)**:
  - **visited パス集合** — 循環参照(現実には Rust のモジュールに循環は
    ないが、異常系の防御)で同一パスへの再帰を打ち切る。
  - **depth 上限** — `--depth` で指定した深さを超えない。
  - **シンボル総数上限** — 再帰で得たシンボルの合計が 500 件を超えたら
    打ち切り、応答の `truncated: true` で通知する(既存
    [`ServerMessage::Outline`] に `truncated: bool` を追加)。
- **キャッシュ**: 再帰先でも既存の outline キャッシュ(ADR-0031、パスキー +
  text checksum)を適用する。同一パスへの再 outline は cache hit で LSP に
  触れない。
- **settle 規律**: 各ファイルの documentSymbol は既存の
  `request_with_loading_retry`(2回連続同一まで待つ)をそのまま使う。再帰全体は
  daemon ロック外の非同期で回し、最後に `restore_focus_after_semantic` で
  フォーカス文書を復元する(ADR-0031 と同じ規律)。
- **headless ゲート**: 既存 outline と同じく handle_connection の専用分岐で
  処理する(変更なし)。

## Considered Options

- フラグなしで常に再帰: 却下 — 大規模クレートで応答が巨大化し、トークン
  削減という outline の目的に反する。デフォルトは 1 ファイル(現状維持)、
  明示指定でのみ横断する。
- CLI 側で再帰(outline の応答を見てコマンドを繰り返す): 却下 —
  モジュール→定義ファイルの解決は LSP が要る位置情報(selection_range と
  definition)で、CLI からは1ファイルずつ往復する羽目になる。daemon 内で
  1 往復に閉じる方がトークン節約(ADR-0031 の思想)に合う。
- tree-sitter で `mod` 宣言を直接パース: 却下 — ADR-0031 で明記した通り
  「LSP 非設定言語のために tree-sitter fallback を作らない」(単一経路の維持)。
  定義ジャンプは LSP が正しく解決する(rust-analyzer の `mod` 解決と
  macro 展開まで)ため、既存経路を再利用する。
- `--depth` を無限(未指定 = 無制限)にする: 却下 — 大規模クレートで
  応答爆発する。既定 3 を安全側に置く。

## Consequences

- エージェントは `minas outline lib.rs --recursive` でクレート全体の構造を
  1 往復で掴める(第3回の「ファイル数に比例して outline コールが増える」
  が解消)。
- `--recursive` の応答は大きくなり得る: 上限 500 シンボル + `truncated`
  通知で「切り捨てた」ことを明示する。エージェントは必要なら per-file の
  outline で補う。
- 既存 `minas outline <path>`(非再帰)は**完全に後方互換**(応答に
  `truncated: false` が付くだけ)。
- skill の outline トピックに --recursive / --depth を追記する。
- PROTOCOL_VERSION を 18 に上げる(`Command::OutlineRecursive` 追加 +
  `ServerMessage::Outline` に `truncated` フィールド追加。ADR-0039 流儀)。