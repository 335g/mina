# クライアント共通接続層(minae-conn)の抽出と socket パスの契約化

TUI を別リポジトリへ切り出す下準備として（ADR-0033 の配布物分離の延長）、bin
クレート内にあったクライアント側の接続・リクエスト配線（元 `conn` モジュール:
absolutize / send_hello / request / request_hints / request_peek）を新規公開
crate **minae-conn** に抽出した。TUI（別バイナリ）は minae-protocol + minae-conn への
依存だけで成立する。

- **socket パスは minae-protocol へ移動**（`socket_path()`）: `minae-{PROTOCOL_VERSION}.sock`
  という名前は wire 契約の一部で、protocol crate の doc が既に命名規則を宣言していた。
  daemon とクライアントの両方が参照する契約なので、単一の実体を持つ（複製しない）。
- **`ensure_daemon` は分解した**: `connect`（接続試行） / `spawn_daemon(exe, args)`
  （detached spawn） / `wait_ready`（起動確認）に分離し、自動起動の組み立て（どの実行
  ファイルを spawn するか）は呼び出し側の責務にした。旧実装は「自分の exe に daemon が
  ある」という同一バイナリ前提を持っており、TUI を別バイナリにすると壊れるため。
- **`open_session` を追加**: session CLI の各コマンドの共通冒頭（接続 + Hello(Headless)）
  を一元化した（約14箇所の繰り返しを DRY 化）。
- config（クライアント設定）は今回の対象外。接続と設定は別関心で、まとめると
  grab-bag になる。TUI リポジトリ実働時に判断する。

## 検討した代替案

- **minae-protocol に混入**: 純粋ワイヤ型の crate に I/O・プロセス管理（しかも unix 依存）
  が入り契約 crate を汚染するため却下。
- **config も同じ crate に（minae-client 化）**: 接続(conn)とクライアント設定(config)は
  別関心。今回は見送り。
- **socket_path の複製**: 契約の重複になるため却下。

## 残る負債（今回の範囲外）

session.rs / client.rs のセマンティック要求（rename / references / outline / hover /
symbol / check）の応答解析は依然重複している。TUI リポジトリ分割時に統合を検討する。