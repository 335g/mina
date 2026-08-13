# ターミナルバックエンドとして termina を採用

mina のターミナル層は termina (0.3.x) を使う。これは Helix 自身が Unix の描画とイベントに採用した低レベル VT crate (PR #13307、2025-08 マージ) であり、より確立された crossterm や ratatui ウィジェットフレームワークの代わりである。termina はモーダルエディタが必要とするもの — kitty keyboard protocol、bracketed paste、synchronized output、resize、tokio 対応のイベントストリーム — を提供しつつ、Helix 流のカスタム描画に必要なエスケープコードレベルの制御を維持する。mina は Helix とまったく同じように、セルバッファとフレーム差分を自前で持つ。termina の pre-1.0 API が不安定であることが判明した場合、crossterm がドロップインのフォールバックである (Helix が以前使っていたもの)。
