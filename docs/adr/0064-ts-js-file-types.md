# TS/JS のファイル種別を languageId 単位で既定表に入れる

ドッグフーディング（2026-09-13、driver 報告 #1）で、**既定表は `file-types = ["ts"]` だけ**
だったことが判明した:

```
minas check src/App.tsx   → check not supported for …/App.tsx (no LSP server configured)   exit 1
minas outline src/double.js / Widget.jsx → outline not supported …                          exit 1
minas outline src/math.ts → [{"name":"add",…}]                                              exit 0
```

`ts-frontend` という profile 名のラウンドで**最初に当たる壁**がこれで、React/JSX の
プロジェクトは minas では一切扱えない（実際 driver は DOM 直書きのアプリに方針変更した）。
TS/JS エコシステムの慣用ファイル名（.tsx/.jsx/.js/.mts/.cts/.mjs/.cjs）が全部落ちていた。

Status: accepted

## Decision

既定表（`minad/src/default_languages.toml`）に **4 つの language エントリ**を置く。
`languageId` は tsserver の scriptKind を決めるので、**拡張子ごとに正しい id** を持つ:

| `name`（= languageId） | `file-types` |
|---|---|
| `typescript` | ts, mts, cts |
| `typescriptreact` | tsx |
| `javascript` | js, mjs, cjs |
| `javascriptreact` | jsx |

- 4 エントリは**同じサーバ定義**（`typescript-language-server --stdio`）と**同じ grammar**
  （tree-sitter typescript）を共有する。サーバ数は増えない。
- ファイル種別だけを `ts` エントリに足す案は却下: `.tsx` を `languageId: typescript` で
  didOpen すると tsserver は JSX として解析しない（scriptKind が違う）。
- テスト: `default_table_covers_ts_and_js_with_the_right_language_ids`（8 拡張子 →
  期待 languageId、4 エントリが 1 サーバを共有すること）。

## Considered Options

- **`file-types` に 8 拡張子を並べるだけ**: 却下 — 上記の scriptKind 問題。
- **拡張子 → languageId のマップを表のスキーマに足す**（`language-ids = { tsx = "typescriptreact" }`）:
  却下（今は）— 表のスキーマが増える。4 エントリで同じことが表現できる。
- **JSX 対応を後回しにする**: 却下 — `ts-frontend` の通常形（React）が丸ごと使えない。

## Consequences

- `.tsx/.jsx/.js/.mjs/.cjs/.mts/.cts` が LSP 経路（outline / hover / at / symbol /
  references / rename）に入る。実測（自分の fixture）: 4 拡張子すべてがシンボルを返す。
- **代償**: セッション鍵は `(root, languageId)`（ADR-0030 Stage 4）なので、`.ts` と `.tsx` が
  混在するプロジェクトは **languageId ごとに 1 セッション**になる。tsserver は 1 プロセスで
  4 言語を扱えるので「root × server」で共有する方が正しい可能性がある — 別の設計課題
  （ADR-0065 の Considered Options に記載）。
- 診断は pull 非対応のまま（tsserver は push 専用）— `check` が使えないことは ADR-0066 と
  `minas skill check` が明示する。
