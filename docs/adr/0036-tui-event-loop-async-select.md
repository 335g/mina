# TUI クライアントは単一の async select ループ（tokio + crossterm EventStream）

TUI クライアントの実行モデルは、tokio ランタイム上で **crossterm `EventStream`・daemon ソケット読み・スピナー tick を 1 つの `tokio::select!` ループで同時待ち**する単一並行ループとする（ratatui 公式 `ratatui-async-template` 直系）。旧クライアントの 3 チャネル mpsc タスク分割（responses/pushes/semantic）は採用しない。

- **描画トリガー**: スナップショット到着・キーイベント・リサイズ時のみ + `activities` 非空の間だけスピナー tick（フレーム差分があるため描画頻度は自由）
- **送信モデル**: コマンド直列送信 + ソケット常時読み。`Response` は直近コマンドの答え、`Push` は逐次適用。`WaitFor` は TUI では使わない
- **request_id は未採用**: daemon は接続ごとに直列処理でパイプライン化の利得がなく、対話入力は人間ペース（実測 248KB で ~10ms/打鍵 ≪ キーリピート 33ms）。発火条件 = 全文 RTT が 30〜50ms を超える大ファイル帯（ペイロード/差分の話であり、そのときに再検討）
- **起動・回復**: mina-conn の `daemon_exe` / `spawn_daemon` / `wait_ready` で自動起動。接続断はステータス報知 + 数秒バックオフ再接続、復帰時 `GetState` で再同期
- **端末**: raw モード + 代替画面（alt-screen）を起動時に有効化し、終了/パニック時に復元（guard）。リサイズは `Resize` イベント → レイアウト再計算 → `Command::SetViewport { height }` を直列経路で送信

Status: accepted

## Considered Options

- タスク分割（旧 3 チャネル mpsc 構成）: チャネル往復より構造が複雑になるだけで、利得なし → 棄却
- シングルスレッド poll: mina-conn が tokio 前提のため不整合 → 棄却
- request_id による並列送信: 上記の理由で未採用