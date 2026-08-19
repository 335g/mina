# Activity: 進行中処理の公開と generation 契約

ユーザーに処理中 (LSP 初期化、診断/ヒントの settle、外部変更の LSP 同期、Save) を知らせるため、進行中の非同期作業の単位を **Activity** として導入し、StateSnapshot に集合として載せる。Activity は開始で追加・終了で除去され、結果の成否は語らない。ヘッドレスは Push を受信できない (購読は Interactive のみ) ため、スナップショットに載せるのが両種クライアントへ確実に届く唯一の経路である。

Activity の増減は generation を進めるが、診断・inlay hint の反映は進めない。WaitFor は generation の増加でしか wake されないので、増減が世代を進めなければヘッドレスは「処理が終わった」ことを待てない。一方、settle ループの毎回の pull 結果に世代を進めると、エージェントが診断進行ごとに wake され、ADR-0013 の「no-op 応答で push しない」趣旨と衝突する。Activity の増減は処理ごとに数回しか起きないため、この例外は許容範囲である。

アニメーション (スピナー) はクライアント側の描画関心事であり、Activity 自体には含めない。TUI は ~15Hz のローカルタイマーで、Activity が active な間だけ回転を描画し、on/off は既存の push + 内容比較 (`snapshot != state`) 経由で受け取る。Open 応答待ち (LSP init、最大 10 秒、ワークスペースごとに最初の 1 回のみ) はクライアントのメインループが応答待ちでブロックするためスピナーを回せない — 静止を許容し、in-flight 中の描画 (request の select 分岐への昇格) は将来イテレーションに回す。

wire 形状: `StateSnapshot.activities: Vec<Activity>`、`Activity { kind: ActivityKind, label: String }`、`ActivityKind` は `LspInit` / `DiagnosticsSettle` / `ReloadSync` / `Save`。Activity はフォーカス文書に紐づく (settle はフォーカス移動で終了する既存仕様と整合)。描画は StatusLine の reverse 背景を流用し、Colorscheme の色ロールは新設しない。コマンドモード (`:` 入力) 中はステータス行が置き換わるためスピナーは消える — 許容する。

## Considered Options

- **単一の busy フラグ**: 不採用。同時発生 (LSP ロード中の外部変更 Reload 等) を表現できない。
- **status フィールド (一過性メッセージ) の再利用**: 不採用。次のコマンド応答で上書きされ、複数スナップショットに跨る「貼り付く状態」にならない。
- **daemon が定期 push してアニメーション駆動**: 不採用。ネットワークと daemon の無駄。アニメは描画層の問題。

## Consequences

- WaitFor は Activity の増減で wake されるようになる。診断到達では従来通り wake しない。
- CONTEXT.md の Generation 定義は Activity 増減の例外を明記する (ADR-0028 参照)。
- ヘッドレスは GetState 応答のスナップショットを読むだけで Activity を参照でき、クライアント側の変更は不要。
- 「まだ診断が届いていない」と「診断が無い」の区別は従来通り不可能なまま — DiagnosticsSettle の終了は settle ループの既存出口 (非空が 2 回連続で同数 / 空が 30 秒 / フォーカス移動 / サーバ死) を hook する妥協案である。サーバの push 通知を読む正確な案は将来イテレーション。