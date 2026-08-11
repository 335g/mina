# Task for reviewer

Task: ANGLE A — SPEC COMPLETENESS: Map EVERY requirement of Issue #1 to concrete code: crate structure (5 crates), each slice S0-S4 completion criteria, daemon/client split (ADR-0005), NDJSON unix-socket IPC + request/response only + full StateSnapshot per response (ADR-0006), keymap trie resolved client-side only, auto-start daemon (emacsclient-style), LSP built-in table (rust-analyzer only) + spawn on first .rs open (no eager), `mina session get/exec` headless CLI. Flag anything in the issue that is missing, partially implemented, or implemented but not declared. Also flag anything implemented that contradicts an issue statement.

Repo: /Users/335g/dev/other/mina. FIRST run `gh issue view 1` to read Issue #1 (mina-term v1 spec) in full, then read the source yourself (files: mina-protocol/src/lib.rs, mina-term/src/{daemon,client,session,keymap,render,lsp,main}.rs, mina-view/src/{editor,history,mode,tree}.rs, mina-core/src/*.rs, docs/adr/*.md). Your job is ADVERSARIAL VERIFICATION: hunt for where the code contradicts, over- or under-delivers on the issue. Be skeptical and specific. For each finding: file:line, what the issue says vs what the code does, severity (critical/major/minor), and a one-line recommended fix. Separate CONFIRMED-GOOD items (spec points you verified hold) from FINDINGS. Do not modify any files. Report as structured markdown.

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