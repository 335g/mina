# 基準 root の登録による過去側解析（比較閲覧 Mode 1）

比較レビューで「削除されたコードの定義元へジャンプ」するため、基準コミットを temp worktree に実体化し、その root を daemon に登録して borrowed-session 読取り（peek/hover/references 等の既存パス指定コマンド）で解析する。読取りコマンドの形状は変えず、基準管理（ライフサイクル・読取り専用強制）の 2 コマンド追加に留めることで、ADR-0039 の bump 方式（v13）に収めた。

Status: accepted

## Considered Options

- リクエスト毎の側指定フラグ: peek/hover/references/outline/inlay/symbol/check の全コマンドに側を thread する。却下 — N 箇所の変更に対して得るものはフルテキスト基準ブラウズの滑らかさだけであり、後送り（(a2) 枠）とした。
- 登録なしの temp 文書利用（借用読取りのみ）: 却下 — LSP セッションが所有者なしで残り（解除手段なし）、基準書込みを構造的に止められず、pin 毎のランダムパスが daemon キャッシュを無効化する。

## Consequences

- 過去側の読取り機能（定義・hover・参照・outline・inlay・シンボル検索・check）は追加実装なしで効く。書込み系（apply/edit/save/rename）は基準配下で status 拒否する。
- 2 つ目のアナライザのコストは残る（極小プロジェクトで約 0.1〜0.5GB を実測）。解除で破棄するため常駐はしない。
- フルテキストの基準ブラウズは `Open` 経由のまま（復帰導線なし）。必要になれば別目的地。
- 副産物: 切断時にフォーカスが死んだ View を指したままになる欠陥（ADR-0037 領域、watch_disk が panic して外部監視が黙って死ぬ）を `drop_conn_view` の idle 復帰で修正した。
