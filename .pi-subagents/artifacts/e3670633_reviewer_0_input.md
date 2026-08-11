# Task for reviewer

Task: ANGLE D — SCOPE OUT / CREEP / TRACING: (1) Verify the issue's explicit v1-out-of-scope list is truly absent from code: mouse handling, multiple simultaneous TUI clients, diff snapshots, LSP config TOML, non-UTF8 file I/O, split views actually wired to commands (mina-view has split tree — is it reachable?), explicit daemon stop command, LSP hover/goto/completion. (2) Verify the 'implementation trade-offs recorded as ponytail: comments' exist for: full snapshot per response, UTF-8-only I/O, 1-client assumption, socket path single-user. (3) Cross-check ADRs (0001-0009) against the code that claims to implement them. (4) Look for scope creep: features beyond the issue with no ADR or issue entry.

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