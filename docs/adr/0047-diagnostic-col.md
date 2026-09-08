# CheckDiagnostic に行内列(col)を追加 — 診断位置を at/peek/hover の住所に直渡し

第2回検証(docs/gitignore/study/minas-debug-study.md)で指摘: `peek` は対象
トークンから外れると**最も近い別トークン**(std の `println!` 定義など)へ
ジャンプする。check 診断の行番号は取れるが、列は目視で合わせる必要があり、
1 桁ずれると別物にジャンプする。check が返す診断位置をそのまま
`at` / `peek` / `hover` の `line:col` 住所として渡せれば、位置計算が
ゼロになり誤ジャンプが構造的に防げる。

Status: accepted

## Decision

`CheckDiagnostic` に **`col: u32`(1-origin 行内列、char 単位)** を追加する。

- daemon は既に診断の char 範囲(start/end)を持つため、行頭から start までの
  char 数を数えるだけ(診断 text も持っているので追加の LSP 往復なし)。
- `line`(1-origin 行)と `col`(1-origin 列)の組は、そのまま
  `minas at <path> <line>:<col>` / `peek` / `hover` の住所に使える
  (CLI の parse_position は 1-origin 行:列 — 既存の仕様)。
- `start`/`end`(char 範囲)は従来どおり `apply` と `--lines` の住所。
  診断 1 件に「行:列」と「char 範囲」の両方が揃うことで、検証ループが
  check → at/peek → apply を位置計算なしで一周できる。

PROTOCOL_VERSION を 16 → 17 に上げる(応答 wire の変更。ADR-0039 bump 方式)。

## Considered Options

- `peek`/`at` を char インデックスで直接受け付ける: 却下 — 住所体系をまた
  増やすと CLI の位置 DSL が二重化する。既存の `line:col`(ADR-0031 の
  parse_position)を CheckDiagnostic が満たす方が小さい。
- CLI ヘルパーで `line:char` → `line:col` を変換する: 却下 — 変換は
  daemon が既に持つテキストでしか計算できず、CLI にファイル読み込みが
  必要になる(全文なしの原則に反する)。daemon 側で 1 フィールド足す方が
  軽い。

## Consequences

- エージェントは診断行の `line:col` をそのまま at/peek/hover に渡せる
  (第2回の「列の目視合わせ・誤ジャンプ」が解消する)。
- 旧クライアントは未知フィールドを無視するが、wire 変更のため bump(v17)。
- protocol round-trip・daemon 統合テスト・mock サーバを col 対応に更新。