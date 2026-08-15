# 色能力は環境変数から検出し、NO_COLOR は色のみ無効化する

スキームの色は `Color` (Ansi / Index / Rgb) で表現されるが、既定・VIVID とも ANSI16 のみを使い、端末の能力を考慮していなかった。端末の色深度 (truecolor / 256 パレット / ANSI16) はクライアント起動時に環境変数から 1 回検出し、`ColorCapability` + `no_color` フラグとして renderer の純粋関数にパラメータで渡す。優先順位は **`NO_COLOR` (非空) > `COLORTERM` (truecolor|24bit) > `TERM` ("256color" 部分一致) > ANSI16**。`NO_COLOR` は色のみ無効にし、下線・反転などの属性は維持する (no-color.org の標準挙動)。Rgb は能力に応じて 256 (6×6×6 キューブ + グレー 24 段) → 16 (xterm 基準 RGB) へユークリッド最近傍で近似する。

## 検討した代替案

- **TERM の完全な端末 DB 判定 (terminfo 等)**: `screen-256color` / `tmux-256color` 等の例外が多く、依存と複雑さに見合う価値がない。COLORTERM と "256color" 部分一致で実用上十分。
- **グローバル状態に検出結果を持つ**: テスト容易性 (純粋関数としての renderer) を損なう。
- **スキームを能力に合わせて事前変換して渡す**: 「適応済みスキーム」という二重構造になるだけで、描画時の 1 色ずつの変換 (adapt_color) と出力は同じ。

## 帰結

- `Ansi16` + `no_color=false` では従来出力と完全等価 (既存テストは引数追加のみで通過)。
- Rgb ベースの VIVID が truecolor 端末で正確な色になり、16 色端末では近似 (従来の ANSI16 見た目と一致) する。
- 端末 DB 判定が必要になった場合、`detect_capability` の 1 関数を差し替えればよい。
