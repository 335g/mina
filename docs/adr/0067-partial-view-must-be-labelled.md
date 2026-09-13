# references / rename は 1 セッション分の視界しか見ない（部分的な影響範囲を「完全」と見せない）

`ts-frontend` ラウンド（2026-09-13、driver 報告 #12）で、**`.tsx` → `.jsx` の rename が
意味的に誤った編集を exit 0 で作る**ことを実測した:

```
jsxprobe/Card.tsx が Card を export、jsxprobe/list.jsx が import して描画（React の普通の形）
minas references jsxprobe/Card.tsx Card   → 1 references in 1 files（定義のみ。list.jsx 不可視）
minas references jsxprobe/list.jsx Card   → 3 references in 2 files（Card.tsx の定義は見える）
  → **非対称**: .jsx セッションからは .tsx が見え、逆は見えない
minas rename jsxprobe/Card.tsx Card CardView → renamed: 1 files, 1 edits / exit 0
  → list.jsx の import と JSX 使用はそのまま残り、**コンパイルできないファイルが残る**
```

原因は ADR-0064 で「文書化された代償」として受け入れた **languageId ごとのセッション分割**。
`outline`/`at`/`hover` のような単一ファイルの問いでは無害だが、**references / rename は
プロジェクト全体を見る必要がある**コマンドで、しかも失敗する向きが普通の形
（`.jsx` のページが `.tsx` のコンポーネントを使う）だった。

Status: accepted

## Decision

**根治は別の反復に回し、この反復では「沈黙」をやめる**（ADR-0058 の規律: 見ていないものを
確定として返さない）。

1. **stdout の影響範囲行が INCOMPLETE を名乗る**: テキスト網（ADR-0065）が「LSP が見なかった
   ファイルに名前が残っている」と見つけたら、
   `renamed: a -> b (N files, M edits) — INCOMPLETE: K file(s) still mention 'a' and were NOT
   changed (see stderr)` / `N references in M files: — INCOMPLETE: K file(s) still mention …`
   と、**スクリプトが読む行そのもの**に書く（stderr の警告だけでは「verify manually」と読まれ、
   影響範囲の全体として受け取られる）。
2. **テキスト網の文言を言語非依存にする**: 「inactive #[cfg] / not in the crate graph」は Rust の
   語彙で、TS では原因が違う（同一 root の別 languageId セッション / ファイル集合外 /
   コメント・文字列）。「not seen by the LSP（…）— the answer above is a PARTIAL view」に変更。
   さらに「ヒットしたファイル内の未列挙言及」の注記は
   「the LSP left them（文字列・コメント、または**その編集自身が残したテキスト**）。
   **その中の識別子は本物の残り**」と明記する — driver の実測で、`import { Card }` という
   生きた import specifier が「string literals / comments」と分類されていた。
3. **根治（次の反復の課題、実測つき）**: session 鍵を `(root, languageId)` から
   **`(root, server)`** に変え、languageId を**文書ごと**に didOpen で送る。tsserver は
   1 プロセスで 4 言語を扱えるので、これが本来の形（ADR-0030 Stage 4 の決定を書き換える）。
   受け入れ試験は driver のこの repro（3 アンカーが同じ 6 locations を返し、
   `.tsx` 起点の rename が `.jsx` も書き換えること）。

## Considered Options

- **languageId 分割を維持し、警告だけで済ませる（本決定）**: 誤りは残るが**沈黙しなくなる**。
  根治（軸の統合）は session 同一性の変更で、`did_open` の languageId を per-document に
  する配管・caps・復元規律に触るため、単独の反復として扱う。
- **`references`/`rename` を「同一 root の全セッションを順に叩いて合算」する**: 却下（今は）—
  同じ root に複数セッションがある状態を正しいものとして固定してしまう。統合が先。
- **rename を拒否する（複数セッションを検出したら exit 2）**: 却下 — 単一ファイル内の
  リネームは正当に使える。影響を明示して使わせる方が安い。
- **テキスト網の結果を stdout の JSON に混ぜる**: 却下 — references の出力は位置の列で、
  機械可読な形を崩さない（影響行は既に散文の行なのでそこに書く）。

## Consequences

- 不完全な `references`/`rename` は**その場で分かる**（stdout の行 + stderr の一覧）。
  実測（自分の fixture）: 同ディレクトリに文字列の残りがある状態で
  `renamed: delta -> deltaRenamed (1 files, 1 edits) — INCOMPLETE: 1 file(s) still mention
  \`delta\` and were NOT changed (see stderr)`。
- `.cjs` の `module.exports = { … }` が `<unknown>` という名前で出るのは **tsserver が送って
  いる名前**（minae は忠実に通している）。skill に「rename の引数にしない」と明記。
- 残る誤り: languageId 軸の分割そのもの（backlog、上の受け入れ試験つき）。
