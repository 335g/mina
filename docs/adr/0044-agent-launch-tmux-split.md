# minae 内エージェント起動（#54）

比較レビューのコメントをその場で AI に投げる。minae にペイン分割も
ターミナルエミュレーションも持たせず、tmux の隣ペインに
`minas review | <agent_command>` を投げる。抽出コマンド（#50）の再利用で、
エージェントは通常の headless クライアントとして参加する。

Status: accepted

## Considered Options

- 埋め込み PTY＋端末エミュレータ（tmux 内蔵型）: 却下 — PTY・emulator の
  新規依存、raw モードの入れ子、フォーカス競合（ADR-0037 の接続=1View と
  衝突）、ゾンビ管理が要る。「ペインが要る」だけのために大きすぎる。
- ワンショット実行（minae が子プロセスで完結待ち）: 却下 — 対話型
  エージェントは PTY 前提で、パイプ完結は print 対応品にしか効かない。
  進行中の様子見・追加指示もできない。
- tmux 分割＋パイプ（採用）: ペイン・フォーカス・寿命は tmux が持つ。
  minae は argv を渡すだけ（ゾンビなし）。daemon・プロトコル無変更 —
  エージェントは既存の headless 経路で参加する。

## Consequences

- `E`（Normal・Mode 2。gap モーダル中は除く）で起動。設定は
  `config.toml` の `agent_command`（エージェント部分のみ。例
  `agent_command = "claude -p"`）。未設定・空は案内 flash のみ。
- エージェント側の契約: レビュー JSON を stdin から読むこと。引数が要る
  品はラッパースクリプトを書いて指定する。
- tmux 外（`$TMUX` なし）では起動せず、実行文面を flash で案内する
  （コピペで隣ペイン実行できる）。
- セキュリティ: コマンドはユーザ自身の設定ファイル由来（shell rc と同信
  任）。minae は既に git へ shell-out しており、新たな信頼境界はない。
- コメント 0 件でも起動する（TUI キャッシュの陳腐化で誤判定するより、
  明示キー押下を優先する）。
