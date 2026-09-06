# 配布物の分割 — TUI は tui feature 化（デフォルト off）

## 決定

公開クレート `minae`（minae-term ディレクトリの bin クレート）のデフォルトビルドは
**エージェント向けのみ**（`daemon` / `session` / `config` / `skill`）とする。TUI
（`open` = client モジュール、render / keymap / colorscheme、依存 termina /
cliclack / futures-lite / unicode-width）は `tui` feature の後ろに置き、
`cargo install minae --features tui` で従来同等のフル構成になる。

- デフォルト: `cargo install minae` → daemon + session CLI + skill（TUI なし）
- TUI 込み: `cargo install minae --features tui` → `minae open` が使える
- `minae config edit`（cliclack の対話編集）も `tui` feature 限定。非 tui ビルドでは
  「--features tui が必要」とエラーを返す。
- 共有配線の整理: 従来 client.rs に同居していた接続ヘルパー（absolutize /
  ensure_daemon / send_hello / request / request_hints / request_peek）は
  新設の **conn モジュール** へ移し、常時コンパイルされるようにした。
  session CLI と TUI の両方が同じ配線を通る（変更なし）。

## 理由

- エージェント契約（daemon + session）が製品の中核（ADR-0005）。デフォルト
  インストールを「エージェントの道具」として名乗れるようにする。
- リポジトリ分割はしない。TUI はデーモンの参照クライアントであり、同一リポジトリ・
  同一バージョンでプロトコル（現在 v11）と常に同調を保つ。別リポジトリ/別公開
  crate にするとバージョン skew 管理が発生する。
- 減らせる依存は termina / cliclack / futures-lite / unicode-width のみ。
  tree-sitter（構文は daemon 所有。ADR-0016/0017）と mina-view
  （ADR-0005 の Editor 状態）はデーモン側で必要なので残る。

## 代替案（却下）

- **別リポジトリ**: TUI に独立した開発者・リリース周期が生まれるまで見送る。
- **別公開 crate（minae-tui）**: 共有コードの lib crate 抽出が必要で、現時点の
  リファクタ量に見合わない。