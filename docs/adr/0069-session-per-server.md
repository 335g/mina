# セッションの鍵を (root, languageId) から (root, サーバ id) に統合する

ADR-0030 Stage 4 はセッションを `(workspace_root, languageId)` で分けていた。Rust では
1 サーバ = 1 言語なので無害だが、**tsserver は 1 プロセスで 4 言語**（typescript /
typescriptreact / javascript / javascriptreact）を扱うため、同一 root に言語ごとの
セッションが並ぶ。各セッションは自分の languageId の project しか見ないので:

- `references` は片側の言語しか返さず、**どちらが欠けるかは tsconfig 次第**
  （実測: tsconfig あり = `.tsx` 側は全件・`.jsx` 側は部分 / tsconfig なし =
  **両方とも部分**で互いに盲目。driver #1/#4）
- その部分集合は exit 0 で返り、`INCOMPLETE` マーカーが唯一の合図だった（ADR-0067）
- 1 root に言語ごとのプロセスが並ぶ（実測: 8 roots で 14 プロセス。driver #4）

Status: accepted

## Decision

1. **`session_key` / `ensure` の鍵を `(root, サーバ id)` に変える**。サーバ id は
   `languages.toml` の `[language-server.<id>]` の id（`ServerSpec.id` を追加）。
2. **`textDocument.languageId` は文書ごと**に didOpen で送る（`did_open` /
   `did_open_keep` / `open_document` / `open_document_keep` が引数で受ける）。
   `LspSession` は spawn 時の言語を既定値として持つだけ。
3. **事前 didOpen（`open_workspace_files`）はアンカーの拡張子ではなく、そのサーバが
   扱う全 file-types** を対象にする（`extensions_for_server`）。`.ts` を anchor に
   しても `.tsx` / `.js` の消費者が開かれる。
4. 付随して直したもの（同時に driver が見つけた計器の誤り）:
   `LspServerInfo.running_sessions` は **セッション数**（以前は `len(roots)`。
   同一 root に言語別セッションが 3 つあるとき 1 と報告していた）。
5. **ADR-0030 Stage 4 の決定を置き換える**（言語ごとに分ける → サーバごとに共有）。

## Considered Options

- **同一 root の全セッションを references/rename で順に叩いて合算する**: 却下 —
  分裂した状態を正しいものとして固定する。1 サーバ 1 セッションが本来の形。
- **languageId を鍵に残し、`didOpen` の直前に他言語セッションを閉じる**: 却下 —
  言語を跨ぐ問い（`.tsx` → `.jsx`）が原理的に解けない。
- **`ServerSpec.id` ではなく `command` を鍵にする**: 却下 — 同じ実行ファイルを
  別 args で定義したテーブルで衝突する。id が正。
- **事前 didOpen をアンカーの拡張子のままにする**: 却下 — 1 セッションに統合した
  意味が薄れる（`.tsx` が視界に入らない）。

## Consequences

- 同一 root は **1 セッション**（実測: 3 クレート/4 言語を触っても 1。
  `minas info` の `running_sessions` と `roots` が一致するようになった）。
- tsconfig の有無に関わらず、両アンカーが**同じ参照リスト**を返す
  （実測: マーカーなし root で `a.ts` / `b.js` とも 4 references in 2 files、
  INCOMPLETE なし。`.tsx`+`.jsx` の rename が両ファイルを書き換え）。
- Rust は 1 サーバ 1 言語なので**挙動不変**（`references … DocumentEdit` = 46/4 files
  が回帰試験としてそのまま使える）。
- 残る穴（backlog）: 死んだ/消えた root のセッションを回収する経路が無い（reap なし。
  driver #4 の観測）、`INCOMPLETE` が正当なローカル束縛 rename でも出る（`e2b8d5a8`）、
  push ベースの `check`、pnpm workspace。
- テスト: 旧「混在言語は分割する」テストを新前提（同一サーバは共有・別サーバは分割）に
  書き換え。`lsp_sessions` のキーを直接挿入するテスト群もサーバ id に更新。
