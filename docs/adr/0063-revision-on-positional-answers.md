# 位置の答えに revision（内容 checksum）を持たせる

ドッグフーディング（2026-09-13、driver 報告 #3）で、**位置を返す答えが「どの内容に
対する位置か」を言えない**ことを指摘された:

- `read` / `search` の `generation` は disk 読みで **0 固定**（バッファ世代であって
  内容 revision ではない）。定数なので revision id に見えるが動かない。
- `outline` / `at` / `hover` には revision フィールドが**無い**。
- ツールが推奨するパイプラインは位置ベース（`outline` / `search` → `read --lines`）なので、
  「本物だが古いテキスト」を、その範囲が指していたテキストとして読んでしまう。
  241 行のファイルでも 12,967 行のファイルでも同じ。しかも失敗は無言。
- ツールはバッファ側では既に解決している（`expected_text`・文書 checksum・
  `wait <generation>`）。disk 読みだけが同等のものを持っていなかった。
- 同じ応答の中で、`read --lines 12960:12970` は
  `"note": "clamped to last line 12967 (requested end 12970)"` と**自己申告**する。
  位置の答えにも同じ正直さが必要。

Status: accepted

## Decision

位置を返す 5 つの応答に**内容 revision**（`checksum: u64` = テキストの `fnv1a64`。
daemon が outline キャッシュと文書 checksum で既に使っているハッシュ）を載せる:

| 応答 | 載せ方 |
|---|---|
| `read` / `get --lines` | JSON envelope に `"checksum"`（`get` は snapshot の document checksum） |
| `search` | JSON に `"checksum"` |
| `at` | JSON に `"checksum"`（応答をそのまま直列化するので追加フィールドのみ） |
| `hover` | JSON に `"checksum"` |
| `outline` | **stderr に `outline: revision <n>`**（応答は素の配列なので形を変えない）。`--recursive` は出さない — ツリーが複数ファイルにまたがり、単一の値が嘘になる。ノードの `path` と、そのファイルの `read` の checksum で照合する |

- 単一ファイルの `outline` は残る。範囲はその値に属する。
- `symbol` は**対象外**（workspace 全体の索引に対する答えで、1 ファイルの内容に紐づかない）。
- `minas info` のセッション root と同様、値は「比較して初めて意味を持つ」ので、
  `minas skill read` / `outline` に比較手順を書く（位置を古い呼び出しから得たら、
  使う前に checksum を比べる）。
- **PROTOCOL_VERSION を 23 → 24**（応答 wire の追加。ADR-0039）。ADR-0062 の
  `LspServerInfo.roots` と同じ bump に同梱。

## Considered Options

- **`generation` を disk 読みでも進める**: 却下 — バッファ世代と内容 revision は別物で、
  混ぜると「文書の編集」と「ファイルの内容」の区別が壊れる（この混同自体が
  前ラウンドの #7 だった）。
- **mtime / size を載せる**: 却下 — 同一サイズ + mtime 復元の外部書き込みを
  検知できない（ADR-0059 の #17a で実測済み）。内容ハッシュの方が強く、既存の
  ハッシュ関数を再利用できる。
- **`outline` の応答を `{revision, symbols}` に包む**: 却下 — 素の配列を前提にした
  呼び出し側（エージェント・テスト・skill の例）を壊す。stderr 1 行で足りる。
- **位置の答えに revision を載せない（現状維持 + 文書化）**: 却下 — 無言の誤りが
  残る。このプロジェクトの規律（ADR-0045/0058: 見ていないものを確定として返さない）
  に反する。
- **全文 checksum を毎回返して呼び出し側にハッシュさせる**: 却下 — daemon は既に
  計算している（outline キャッシュ・文書 checksum）。再計算を押し付けない。

## Consequences

- `outline` → `read --lines` の定石が**検証可能**になった: `outline` の stderr の
  数値と `read --lines` の `checksum` が一致しなければ、その範囲は古い内容のもの。
- 追加されるのは 1 フィールド（u64）と 1 行の stderr のみ。既存の出力形は不変
  （`at`/`hover`/`search`/`read` は JSON オブジェクトへの追加、`outline` は配列のまま）。
- `symbol` と `--recursive` の位置は依然として revision を持たない — 何を比較すれば
  よいかを含めて skill に書く（黙って持たないのではなく、持てない理由を書く）。
