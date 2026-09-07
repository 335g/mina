# 過去2コミットの比較閲覧（Mode 2）

過去2コミットを両側読取り専用で比べる。基準コミットを temp worktree に
実体化して borrowed-session 読取りする方式（ADR-0040）を両側に拡張し、
2 root 登録で旧側・新側の両方を解析する。書込み系の追加はなく、
ADR-0039 の bump 方式にも触れない client-only の変更に留めた。

Status: accepted

## Considered Options

- デーモン側にモード概念を持たせる: 新旧の区別・読取り専用強制を daemon
  が知る。却下 — 登録は既に複数 root 対応で、書込みガードはパス基準の
  ため両 worktree にそのまま効く。TUI が編集キーを出さなければ live 文書
  も汚れない。プロトコル変更ゼロで足りた。
- 新側テキストを Document として開く: 却下 — デーモンの文書集合を汚し、
  undo 履歴・dirty・LSP フォーカスに波及する。canvas は TUI 側の表示用
  テキストに留め、カーソル・viewport も自前で持つ（デーモン選択不干渉は
  gap レビューと同型）。
- Mode 2 でのレビューコメント対応: 見送り — ReviewComment は単一 base の
  スキーマ（ADR-0041）のため、拡張なしでは新旧の区別が付かない。
  過去閲覧の価値（流れで見る）は注釈だけで成立し、AI 連携は Mode 1 で足りる。

## Consequences

- `M` で旧=HEAD~1・新=HEAD を即ピン、`B` でコミットピッカー（o=旧側・
  n=新側）。`D`（Mode 1）とは相互排他で、入ると相手を捨てる。
- worktree は `mina-base-<pid>-<hash>-old/new`。prune/sweep の命名則と互換。
  終了・切替時は両方 Unregister＋撤去し、RA を常駐させない。
- LSP セッションは初回読取り時に lazy 確保のため、使わない側のコストは
  ディスクのみ。2 台稼働時の目安は #53 実測（約 700〜1200MB/台）。
- 新側欠落ファイルは空 canvas＋全 Gap で表す（新規コードなし）。
- worktree 忠実度の限界は #55 に分離（2 worktree で露出が倍になる）。
