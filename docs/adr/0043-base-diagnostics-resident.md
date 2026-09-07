# 基準診断のスナップショット常駐（#52・a2）

比較閲覧で基準側の問題が見えない（削除 gap の過去側 Peek 止まり）ため、
注目文書の基準側対応物の診断をスナップショットに常駐させる。TUI も
headless も往復なしで読める。ADR-0039 の bump 方式（v15）に収めた。

Status: accepted

## Considered Options

- 都度取得（`minas check <base-path>` の TUI 版）: 却下 — TUI の単一接続では
  .(content truncated 1006 chars)
