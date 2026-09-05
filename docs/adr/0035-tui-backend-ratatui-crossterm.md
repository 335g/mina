# ターミナルバックエンドを ratatui/crossterm に換装（ADR-0004 を置換）

minae TUI のゼロベース再構築に伴い、ターミナル層を termina（ADR-0004）から **ratatui 0.30.2 + crossterm 0.29.0** に全面換装する。ADR-0004 は自前セルバッファ + フレーム差分（termina 前提）を選んだが、再構築ではウィジェットフレームワーク前提で設計し、`Terminal::draw` のフレーム差分（`Buffer::diff`）と `Layout` + `StatefulWidget` による構成に載せる。選択の理由は差分描画を自前実装するコストを構造的に解消できることと、crossterm `EventStream` + `tokio::select!` の async 統合が公式サポートされていること（調査: docs/spec/research/ratatui-ecosystem-findings.md）。ビルド・動作はプロトタイプで実証済み。

Status: accepted（ADR-0004 を supersede）

## Consequences

- workspace の termina 依存宣言を撤去する
- mina-text / mina-view / mina-protocol は UI 非依存のまま（termina/描画依存ゼロを検証済み — docs/spec/research/mina-assets-findings.md）
- 旧 minae のエスケープコード直生成（render.rs）および termina 依存の KeyEvent 型（keymap.rs の移植元）は ratatui/crossterm の型へ置き換える