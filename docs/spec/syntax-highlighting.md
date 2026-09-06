# 構文ハイライト仕様 (Syntax Highlighting Spec)

minae の TUI に構文ハイライトを実装するための仕様。決定事項は ADR-0016/0017/0018 に記録し、本稿は実装可能な詳細を定義する。

## スコープ

**今回の範囲**: トークナイズ (tree-sitter) → daemon 側の Syntax 状態 → StateSnapshot 経由の配信 → TUI の描画まで、マイルストーン M1〜M3。

**スコープ外（後回し）**: Colorscheme 機構（名前付きスキーム・切替）、構造編集 (AST ベースの移動・選択)、grammar の動的ロード・ランタイム取得、言語定義ファイル (languages.toml 相当)、差分描画、巨大ファイルの再パース最適化。

## 用語

`CONTEXT.md` 参照: **Syntax**, **HighlightGroup**, **Colorscheme**。本稿では「グループ」= HighlightGroup の各値、「ハイライト範囲」= テキストのある区間に割り当てられたグループ。

## アーキテクチャ

```
┌─ mina-loader (新規クレート) ─────────────────────────────┐
│  言語定義レジストリ: 拡張子/言語名 → grammar + ハイライトクエリ │
│  deps: tree-sitter, tree-sitter-rust (M1)                │
└──────────────┬────────────────────────────────────────────┘
               │ language_for_path()
┌─ minae-term (daemon) ──────────────────────────────────────┐
│  SyntaxStore: DocumentId → { tree, ハイライト範囲 }        │
│  全編集源 (interactive / DocumentEdit / Reload) で更新     │
│  → StateSnapshot.highlights: Vec<HighlightRange>          │
└──────────────┬────────────────────────────────────────────┘
               │ 既存の応答 + push パイプライン (ADR-0006/0013)
┌─ minae-term (client / render.rs) ──────────────────────────┐
│  draw_line: (カーソル, 選択, 診断, グループ) でスタイル合成 │
└────────────────────────────────────────────────────────────┘
```

## データモデル

### HighlightGroup (13 種)

wire・テーマ形式は小文字。Rust は `mina-protocol` の enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HighlightGroup {
    Comment, Keyword, String, Number, Constant,
    Function, Type, Parameter, Field, Operator,
    Punctuation, Attribute, Error,
}
```

| グループ | 対象の例 |
|---|---|
| `comment` | コメント |
| `keyword` | 制御構文・キーワード |
| `string` | 文字列リテラル（エスケープ含む） |
| `number` | 数値リテラル |
| `constant` | 定数・真偽値・nil |
| `function` | 関数定義・呼び出し |
| `type` | 型名 |
| `parameter` | 引数 |
| `field` | 構造体フィールド・プロパティ |
| `operator` | 演算子 |
| `punctuation` | 括弧・区切り |
| `attribute` | アトリビュート（`#[...]`・デコレータ） |
| `error` | tree-sitter の ERROR ノード（未構文） |

### UI ロール（taxonomy に含める。実装は Colorscheme セッションで enum 化）

カーソル / 選択 / 診断 Error / 診断 Warning / ステータス行 / コマンドライン / ポップアップ。現状 render.rs のハードコード（`44m` 青背景・`7m` 反転・`4m` 下線）に対応する。M3 では変更しない。

### HighlightRange（wire 型）

char インデックス（Diagnostic と同型・コードベースの慣習）:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HighlightRange {
    pub start: usize, // char index (inclusive)
    pub end: usize,   // char index (exclusive)
    pub group: HighlightGroup,
}
```

`StateSnapshot` に `pub highlights: Vec<HighlightRange>` を追加（プロトコル変更）。JSON 例:

```json
{ "start": 0, "end": 7, "group": "comment" },
{ "start": 9, "end": 12, "group": "keyword" }
```

範囲は重複しない（同じ char は1つのグループに属する）。未割り当ての char は「既定テキスト」。

## Daemon 側の動作契約（SyntaxStore）

- **従属**: Open で初期化、Close で破棄、Reload（ADR-0015）で再パース。Document の ID・Path・LSP 対応は維持。
- **更新**: トランザクション適用ごとに tree-sitter のインクリメンタル再パース（対象は編集範囲のみ）。編集源を問わない（interactive Command / headless DocumentEdit / 外部 Reload）。
- **不変条件**: スナップショット内の `highlights` は同じスナップショットのテキストと一致する。スナップショット生成時に範囲を再計算する。
- **grammar 不在**（未知拡張子・未登録言語）: `highlights` は空。ハイライトなし（ADR-0017 のフォールバックなし方針）。
- **性能**: 巨大ファイルで実用上問題が出た場合のみ対策を検討する（debounce 等）。プロファイリングが先（ponytail）。

## mina-loader（M1）

新クレート `mina-loader`（ADR-0003 が指名した将来クレート。依存方向: daemon → mina-loader → tree-sitter で成立）。

```rust
pub struct LanguageDef {
    pub name: &'static str,             // "rust"
    pub extensions: &'static [&'static str], // [".rs"]
    pub grammar: tree_sitter::Language, // tree-sitter-rust の LANGUAGE
    pub highlights: &'static str,       // include_str! した .scm
}

pub fn language_for_path(path: &str) -> Option<&'static LanguageDef>;
pub fn language_by_name(name: &str) -> Option<&'static LanguageDef>;
```

- レジストリは静的（const テーブル）。2言語目以降 = Cargo 依存の追加 + エントリ1行。
- ハイライトクエリ (.scm) は各言語の `highlights/` に置き、`include_str!` で同梱。capture 名は **HighlightGroup の小文字名に一致させる**（`@comment`, `@keyword`, `@string` …）。クエリは自前実装（ADR-0001・ADR-0018）。
- 対応表は lsp.rs の `.rs → rust-analyzer` と同じ対応（将来は言語定義ファイルへ移行）。

## Renderer（M3/M4）

`draw_line` のスタイル状態を `(カーソル, 選択中, 診断中)` タプルから **`(カーソル, 選択中, 診断ロール, グループ)`** に拡張する。

**優先順位**: カーソル > 選択 > 診断 > グループ。上位は下位を置換し、属性は現行どおり合成する（カーソル+診断 = 青背景+下線、選択+診断 = 反転+下線）。勝利ロールの Style は全フィールドが尊重される。診断範囲内は「下線 + 診断色（Error/Warning で異なる）」がグループ色を置換する（M4 で確定。M3 の「下線 + グループ色」共存ルールは置き換え）。

**暫定パレット**（M4 で `minae-term/src/colorscheme.rs` の `DEFAULT` スキームとしてデータ化済み。以下はその当初の定義）:

| グループ | SGR |
|---|---|
| comment | `90` (bright black) |
| keyword | `36` (cyan) |
| string | `32` (green) |
| number | `33` (yellow) |
| constant | `35` (magenta) |
| function | `34` (blue) |
| type | `96` (bright cyan) |
| parameter | 既定 |
| field | `94` (bright blue) |
| operator | 既定 |
| punctuation | 既定 |
| attribute | `95` (bright magenta) |
| error | `91` (bright red) + `4` (下線) |

色能力検出 (truecolor/256/16) は Colorscheme セッションの課題（現行どおり ANSI 16 で固定）。

## マイルストーン

- **M1 — mina-loader**: クレート新設、レジストリ、rust grammar + ハイライトクエリ。検証: スニペットをパースして期待グループが得られるユニットテスト。
- **M2 — プロトコル + daemon**: `HighlightGroup` / `HighlightRange` / `StateSnapshot.highlights`、daemon の SyntaxStore（編集ごとのインクリメンタル再パース、Open/Close/Reload のライフサイクル、スナップショット生成時に範囲を充填）。
- **M3 — renderer**: `draw_line` のグループ次元、暫定パレット、優先順位。検証: 既存の render テストを拡張（グループ付き行のエスケープ列検証）。
- **M4 — Colorscheme 機構**: 名前付きスキームと切替（`:colorscheme`）、暫定パレットの外部化、色能力検出、診断 Error/Warning の色分け、UI ロールの enum 化。**全完了** — M4-1 (#18: データモデル + DEFAULT + renderer 参照化) / M4-2 (#19: `:colorscheme` 切替) / M4-3 (#20: 色能力検出、ADR-0019)。

## 関連文書

- ADR-0016 (Syntax は Daemon 所有) / ADR-0017 (tree-sitter 採用) / ADR-0018 (フラット taxonomy)
- ADR-0003 (mina-loader 指名) / ADR-0005 (daemon 所有モデル) / ADR-0006・0012・0013 (スナップショット・push) / ADR-0015 (外部リロード) / ADR-0001 (クリーンルーム)
- docs/helix-architecture.md (参照設計)
- CONTEXT.md (`Syntax`, `HighlightGroup`, `Colorscheme`)
