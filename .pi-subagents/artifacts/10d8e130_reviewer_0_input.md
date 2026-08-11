# Task for reviewer

ADVERSARIAL SPEC COMPLIANCE REVIEW (Japaneseで回答):
リポジトリ /Users/335g/dev/other/mina のソースコードを GitHub Issue #1（gh issue view 1 で確認可）のスペックと徹底的に突き合わせてください。Issue #1 の主張と実装のズレを「見つける」ことが目的です。確認のための前提知識はファイルを読んで取得してください（git status はクリーン、HEAD=e58f2ae）。

突き合わせる観点:
1. アーキテクチャ決定（ADR-0005 daemon/client 分割、ADR-0006 unix socket+NDJSON 毎レスポンス全量スナップショット、ADR-0004 termina 0.3、キーマップはクライアント側のみ解決）が実装と一致するか。docs/adr/0004,0005,0006 を読んで照合。
2. クレート構成: mina-protocol（依存なし・wire型）、mina-lsp（新）、mina-term（bin "mina": client/daemon/session 3モード）が Issue 記載どおりか。
3. 各スライス S0-S4 の完了条件が実装で満たされているか（S0 socket疎通、S1 表示移動、S2 編集保存undo+dirty、S3 LSP rust-analyzerのみ・初回.rsオープンでspawn・診断表示、S4 session get/exec）。
4. v1 スコープ外と明記された事項（マウス、複数TUI、差分スナップショット、LSP設定ファイル、非UTF-8、スプリット、daemon明示停止、LSP hover/goto/補完）が実装に漏れ込んでいないか。
5. 実装上の割り切り（毎レスポンス全量、UTF-8のみ、1クライアント前提、temp_dir/mina.sock）がコードに ponytail: コメントとして記録されているか。
6. Issue で「確定済み」とされた設計判断と矛盾する実装がないか。

重要なファイル: mina-term/src/{main,daemon,client,session,keymap,lsp,render}.rs, mina-protocol/src/lib.rs, mina-lsp/src/lib.rs, mina-view/src/*.rs, docs/adr/*.md, CONTEXT.md
対象外: コーディング規約のスタイル指摘は不要。Issue との適合/不適合の事実のみ。

出力: 各観点ごとに 適合/不適合/要確認 の判定と、不適合・要確認の具体的箇所（ファイル:行番号と根拠）。最後に総合判定（Issue のスペックを実装が満たしているか）を YES/NO/部分一致 で。事実ベースで、憶測で断定しないこと。

## Acceptance Contract
Acceptance level: attested
Completion is not accepted from prose alone. End with a structured acceptance report.

Criteria:
- criterion-1: Return concrete findings with file paths and severity when applicable

Required evidence: review-findings, residual-risks

Finish with a fenced JSON block tagged `acceptance-report` in this shape:
Use empty arrays when no items apply; array fields contain strings unless object entries are shown.
`criteriaSatisfied[].status` must be exactly one of: satisfied, not-satisfied, not-applicable.
`commandsRun[].result` must be exactly one of: passed, failed, not-run.
`manualNotes` and `notes` are optional strings; an empty string means no note and does not satisfy `manual-notes` evidence.
```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "specific proof"
    }
  ],
  "changedFiles": [
    "src/file.ts"
  ],
  "testsAddedOrUpdated": [
    "test/file.test.ts"
  ],
  "commandsRun": [
    {
      "command": "command",
      "result": "passed",
      "summary": "short result"
    }
  ],
  "validationOutput": [
    "validation output or concise summary"
  ],
  "residualRisks": [
    "none"
  ],
  "noStagedFiles": true,
  "diffSummary": "short description of the diff",
  "reviewFindings": [
    "blocker: file.ts:12 - issue found, or no blockers"
  ],
  "manualNotes": "anything else the parent should know"
}
```