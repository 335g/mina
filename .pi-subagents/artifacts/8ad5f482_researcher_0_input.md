# Task for researcher

Fact-check three things for a Helix-style Rust editor (termina 0.3 + possible tokio): (1) Which popular LSP language servers support non-stdio transports (unix socket or TCP) in addition to stdio? Check specifically: rust-analyzer, typescript-language-server, clangd, pyright/pylsp, gopls. For each state the transport options (e.g. rust-analyzer --socket/--port?). (2) Does termina (helix-editor/termina, 0.3.x) have any async/tokio integration (event-stream feature, async reader), or is it strictly synchronous poll/read? (3) How does Helix bridge termina's synchronous event reading into its tokio event loop (helix-term/src/application.rs) — briefly, what mechanism (blocking task + channel, spawn_blocking, etc.)? Cite primary sources (GitHub, docs.rs, crates.io). Keep the brief under 400 words, bullet format.

---
**Output:**
Write your findings to exactly this path: /Users/335g/dev/other/mina/.pi-subagents/artifacts/outputs/8ad5f482/research.md
This path is authoritative for this run.
Ignore any other output filename or output path mentioned elsewhere, including output destinations in the base agent prompt, system prompt, or task instructions.

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