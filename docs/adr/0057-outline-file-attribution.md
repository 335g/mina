# outline のファイル帰属（`--recursive` の range に path を付ける）

ドッグフーディング（2026-09-13、driver 報告 #1）で指摘:
`minas outline <path> --recursive`（ADR-0049）は別ファイルのモジュールを
children に inline するが、**range は char オフセットだけ**で帰属ファイルを持たない。
親の range は渡したファイル（例 `src/main.rs`）、子の range は定義ファイル
（例 `src/http.rs`）のオフセットで、**オフセット空間が混ざる**:

```
{"name":"http","kind":"module","range":{"anchor":143,"head":152},      ← main.rs の 143
 "children":[{"name":"MAX_BODY_BYTES","range":{"anchor":146,...}},     ← http.rs の 146
             {"name":"json" の子が 203,...}]}                          ← 数値が親と衝突
```

skill は range を「`read --lines` / `apply` に渡す住所」として宣伝しているため、
**親のパスに子の range を渡すと黙って別の領域を編集する**（`expected_text` は
range ローカルではなく文書全体 checksum なので検出できない — ADR の
`expected_text` 規律（rust2 #5-2）と同じ失敗クラス）。実測（driver）: 親を
`src/main.rs`、子を `src/http.rs` と解決しないと住所が一意に決まらない。

Status: accepted

## Decision

`OutlineSymbol` に `path: String` を追加し、**ファイルが変わる境界にだけ**置く。
`--recursive` でモジュールを展開したとき、inline した**直下の子**に定義ファイルの
パスを入れる（`minad/src/daemon.rs` の `expand_outline_modules`）。

- **解決規則（1 行）**: ノードの range のファイル = 自身の `path` → 無ければ親の
  ファイル → トップレベルなら応答の `path`。モジュールノード自身は親ファイルの
  ままなので `path` を持たない（range が親ファイルにあるため）。
- **全ノードには付けない**: 兄弟で同一パスを繰り返しても情報が増えない。
  同一ファイル内の反復は応答を膨らませるだけ（`outline` の存在理由はトークン
  削減）。実測: mina 自身の `minas outline minas/src/main.rs --recursive`
  （204 ノード・28,334 B）で、全ノードに付けると +5.5 KB（+19%）。
- **非再帰応答は完全に不変**: `path` は空文字で `skip_serializing_if` により
  JSON に出ない（`minas outline <path>` の wire 形状は据え置き）。
- **直下の子だけに付ける理由**: 子孫は「親と同じファイル」で解決できるため。
  深い展開（`mod` の中の `mod`）では、その境界の直下に改めて `path` が付く。
- **変換層（`minad/src/lsp.rs` の `convert_symbol_list`）は path を知らない**:
  1 ファイル分の変換なので常に空。帰属を決めるのは再帰展開の側（そこだけが
  「どのファイルから inline したか」を知っている）。
- **skill を更新**: `minas skill outline` に「range はファイルとセットで解決する」
  規則と、誤ったファイルへ適用すると黙って壊れることを明記する。
- **PROTOCOL_VERSION を 21 → 22** に上げる（応答 wire の変更。ADR-0039 の bump 方式）。

## Considered Options

- **全ノードに `path` を付ける**（driver の当初要望）: 却下 — 解決規則を覚えなくて
  済むが、兄弟で同一パスを繰り返し、実測 +19%（非再帰応答も膨らむ）。「ファイルが
  変わる所にだけ置く」方が情報量当たりのコストが良く、規則も 1 行で書ける。
- **モジュールノードに `children_path` を付ける**（子のファイルを親側に持たせる）:
  却下 — フィールドが 2 種類（自身の range のファイル / 子のファイル）になり、
  「このノードの range はどちら?」という同じ曖昧さを別の形で持ち込む。
- **応答をファイル単位に分ける**（`[{path, symbols}]`）: 却下 — ツリーの形（ADR-0049
  が再利用を決めた `ServerMessage::Outline`）を崩し、呼び出し側の走査コードを
  書き換えさせる。追加フィールドで足りる。
- **`read` / `apply` 側で「親の範囲外オフセット」を弾く**: 却下 — オフセットは
  ファイル内で正当な値なので、別ファイルの偶然の一致を検出できない。
- **何もせず skill に「per-file で呼べ」と書く**: 却下 — `--recursive` の存在理由
  （1 往復でクレート全体を掴む）を捨てることになる。

## Consequences

- エージェントは `--recursive` の結果を、ノードごとに正しいファイルへ解決して
  `read` / `at` / `apply` に渡せる。`--recursive` が住所として使えるようになる。
- 既存の非再帰応答・`--depth 1` の応答形状は不変（`path` が空なので JSON に出ない）。
- 帰属の無いノードは「親と同じファイル」を意味する。応答の先頭（`path`）が
  トップレベルのファイルを与える。
- テスト: `outline_recursive_follows_file_modules_via_mock` に、inline した子が
  定義ファイルを指し、親が `path` を持たないことの assert を追加。
