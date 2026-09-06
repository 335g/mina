# ADR-0032: session hover / symbol / check（LSP 軽量問い合わせの拡充）

## 状態: Accepted

## Context

エージェントのコストは「読む量 × 往復数 × 試行回数」で決まる（docs/benchmarks/
agent-editor-performance-considerations.md）。既存の LSP 連携（peek / hints /
outline / at / rename / references）は「位置→内容の解決」と「全文なしの構造取得」
に寄っていたが、エージェントの最頻操作のうち以下は 1 往復で済んでいなかった:

- **型を知る**: 現状は定義本体を読んで推論（または全文 read）。hover が最短路。
- **「どこで定義されているか」を探す**: 現状は `rg`（行全体 + 誤ヒット = 高トークン）
  か workspace 全体の複数 read。workspace/symbol が最短路。
- **編集→検証ループ**: 現状は「edit → wait → get（全文）→ JSON から診断を読む」。
  診断は generation を進めないため wait が効かず、全文スナップショットを運ぶ。

## Decision

`Command::HoverAt` / `WorkspaceSymbol` / `CheckDiagnostics`（読み取り専用。
PROTOCOL_VERSION を 8 → 9 に上げる）を追加し、それぞれ軽量応答
`ServerMessage::Hover` / `WorkspaceSymbols` / `Check` で返す。全文スナップショット
は運ばない（peek / outline と同じ軽量経路の流儀）。

- **hover**: 1-origin 行:列 → `textDocument/hover`。`contents` は string /
  MarkedString / MarkupContent / 配列のどれでも扱い、表示テキストを連結して
  上限 2000 文字で切り詰める（F8 の断片化と同じ方針。doc コメントは長くなり得る）。
  型・シグネチャを主目的とし、hover の無い位置（空白・コメント）は空テキストの
  成功応答（exit 0 — peek の空定義と同じ流儀）。
- **symbol**: `path` でワークスペース root（LSP セッション）を決め、`query` を
  `workspace/symbol` に投げる。結果は名前・種別・パス・1-origin 行のみ
  （`ReferenceLocation` と同じ「位置だけ渡す」原則）。解析待ち（null）は
  references と同じリトライ規律で待つが、空配列は「該当なし」の正常応答として
  待たない。空クエリは invalid input（exit 1）。
- **check**: 対象パスの診断が安定するまで pull を繰り返し、診断だけを返す
  （severity / 1-origin 行 / char 範囲 / message）。安定判定は
  settle_open_diagnostics と同じ: 非空が 2 回連続で同数 = 安定、空のまま予算
  （約 10 秒）を使い切ったら「クリーン」として空を返す。従来の
  「wait → get → 全文 → パース」の代わりに 1 コマンドで検証できる。
  CLI は error 診断が 1 件でもあれば exit 2（警告のみなら 0）— エージェントは
  $? だけで分岐でき、JSON パースを省ける（F7 の出口コード分類）。
- 能力ゲート（outline / rename / references と同じ）: サーバが対応機能を
  advertise していなければ即「not supported」（exit 1、再試行不可）。LSP エラー
  は再試行可能（exit 2）。
- 計測: 要求回数と応答シリアライズ bytes を ServerMetrics に累積する
  （hover_total/hover_bytes, symbol_search_total/symbol_search_bytes,
  check_total/check_bytes — ADR-0031 の outline 計測と同じ目的）。

## Consequences

- エージェントの 3 最頻操作（型参照・シンボル探索・編集後検証）がそれぞれ
  1 往復・全文なしになる。check はさらに検証ループの「wait + get + 全文」を
  丸ごと置き換える。
- 懸念: check の安定判定は件数ベース（settle_open_diagnostics と同じ妥協）。
  冷えたワークスペースでは解析完了前に「クリーン」を返し得る — 本文書の
  settle 契約と同じ許容であり、LSP 高速経路の限界として明記する。
- 懸念: workspace/symbol はサーバの実装依存で、エージェントが開いていない
  ファイルに弱いサーバがある — rust-analyzer はワークスペース全体を索引する。
  開いていないファイルを取りこぼすケース（tsserver 等）は将来
  open_workspace_files の併用を検討する。
- mock サーバ（mina-lsp）に hover / workspace/symbol のハンドラを追加し、
  daemon 統合テストで 3 コマンドを検証した。

## 実装上の注意（デッドロックの教訓）

daemon ロックと LSP 応答の計測を 1 式に書くと一時ガードが .await を跨いで生き、
内部の再ロックが自分自身に待たされる（実測でハング）。メッセージは先に
`let msg = ...` で組み立ててから計測関数へ渡す（引数式内に daemon.lock() を
置かない）。