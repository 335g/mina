# Rust 前提だった 3 箇所を言語非依存にする（session root・テキスト網）

`ts-frontend` ラウンド（2026-09-13、driver 報告 #4/#5/#6）で、**Rust を前提にしていた
3 箇所**が TS プロジェクトで無言の誤りを生むことが分かった。1 つの根原因（「Rust の
形をした一般化」）の 3 症状なので、まとめて直す。

1. **session root が cargo の workspace しか見ない**（`manifest_kind` は `Cargo.toml` の
   `[workspace]` だけを読み、他の manifest は単独パッケージ扱い）。実測: ルート
   `package.json` に `"workspaces": ["tools"]` があっても `tools/` が別 root/別セッションに
   なり、`+1 パッケージ = +1 セッション`（ADR-0062 が Rust で消した形が JS 側に残っていた）。
2. **manifest が後から現れると古い子 root のセッションが生き残る**。実測: 最初に
   `src/foo.ts` を触る（manifest 無し → root = `…/src`）→ 後からルートに `package.json` を
   置いても `…/src` のセッションが走り続け、**同じパッケージの 3 ファイルが 2 つの
   tsserver で解析される**。どちらが答えるかは `minas info` から見えず、file 集合の違う
   答え（部分的な `references`）を返す原因になった。
3. **テキスト網（#16 の「まだ旧名が残っている」警告）が `.rs` 固定 + `Cargo.toml` アンカー**。
   TS プロジェクトでは**何も走査せず何も警告しない**（実測: `rename` が同ディレクトリの
   文字列リテラルの残りを見落としたまま exit 0 / stderr 空）。フロントエンドは
   非識別子の言及（i18n 文字列・test-id・CSS クラス・JSON fixture）が**普通**なので、
   この profile で最も必要とされるガードだった。

Status: accepted

## Decision

1. **`manifest_kind` に npm を足す**: `package.json` が `workspaces`（配列 or
   `{packages: […]}`）を持てばワークスペースルート（`excludes: false`）。cargo の
   `exclude` に相当する概念が無いため保守側の分岐は不要。
   **実装した規則は「`workspaces` を宣言した manifest が、その配下すべての子孫の
   session root になる」**（「宣言されたメンバーだけが共有する」ではない）。driver の
   追試: ルートが `workspaces` を持つとき、**メンバーでない**入れ子 `extra/package.json` も
   ルートに畳まれる（`workspaces` を外すと `extra2/` は独立 root になる）。npm 意味論では
   非メンバーの入れ子パッケージは独立プロジェクト（fixture / example / vendored）なので、
   これは**過剰共有**だが、過剰共有は「解析が 1 セッションに集まる」側の誤りで、
   過少共有（メンバーが別セッション）より安い。メンバー判定（glob）は実装しない。
2. **子孫 root のセッションを追い出す**: root R のセッションを新規作成したとき、
   `R` の真の子孫を root とする生きたセッションを map から外し、プロセスを kill する
   （既存の UnregisterBaseRoot と同じ「ロック外で kill」の規律）。親のセッションが
   サブツリー全体を見るので、子は不要かつ有害（どちらが答えるかが見えない）。
3. **テキスト網を言語対応にする**:
   - 走査拡張子 = アンカーの言語ファミリ（`.rs`、または TS/JS の 8 拡張子。TS/JS は
     相互 import するので 1 ファミリとして走査する）。未知の拡張子はアンカーと同じ拡張子のみ。
   - アンカー = その言語の manifest を持つ**最も外側**の祖先（Rust は `Cargo.toml`、
     TS/JS は `package.json`/`tsconfig.json`）。manifest が無ければアンカーのディレクトリ。
   - 走査から `node_modules` を除外（500 ファイル上限が依存ツリーで埋まる）。

## Considered Options

- **`file-types` を増やすだけで済ませる**: 却下 — session root とテキスト網は別経路で、
  実測でも別々に失敗していた（#5 と #6）。
- **session 鍵を `(root, server)` にして languageId を per-document にする**: 保留 —
  tsserver は 1 プロセスで 4 言語を扱えるので、これが本来の形である可能性が高い。
  ただし didOpen ごとに languageId を差し替える配管が要り、ADR-0030 Stage 4 の決定
  （言語ごとにセッションを分ける）を書き換える。**実測が要求したら**別の反復で。
- **`pnpm-workspace.yaml` も読む**: 保留 — YAML パーサが依存に無い。境界として
  `minas skill` に明記（pnpm ワークスペースは今のところ 1 パッケージ = 1 セッション）。
- **テキスト網のアンカーを `.git` にする（言語非依存）**: 却下 — git 管理外の
  スクラッチでは何も走査できなくなる。manifest 基準の方が「その言語のプロジェクト」に合う。
- **`workspaces` の否定パターン（`!pkg`）を解釈する**: 却下（今は）— 実測で必要になったら。

## Consequences

- TS/JS プロジェクトで session が 1 つになり（実測: ルート package.json + `workspaces` で
  `src/` を先に触っても最終 root は 1 つ、古い `…/src` は消える）、テキスト網が
  `rename`/`references` で発火する（実測: 同ディレクトリ + 別ディレクトリの文字列言及 2 件を
  stderr に列挙、exit 0）。
- テスト: `session_root_shares_a_cargo_workspace` を拡張（npm workspace / 子孫の追い出しは
  手動 fixture で確認）。
- 残る穴: pnpm のワークスペース定義、`(root, server)` 共有、TS の import を辿る
  `--recursive`（`minas skill outline` に「TS では辿らない・file 集合は tsconfig 由来」と明記）。
