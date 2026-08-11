# Task for researcher

Research the current (2025) Rust terminal library landscape for building a minimal modal terminal text editor in the style of Helix. Compare these candidates: crossterm, ratatui, termion, termina (helix-editor's fork), termwiz, and any other serious candidates. For each: (1) maintenance status — last release date, actively maintained?, (2) abstraction level — raw terminal/event handling vs widget framework, (3) capabilities — raw mode, alternate screen, key events with modifiers, resize, mouse, (4) rendering model — immediate-mode diffed buffer vs direct writes, (5) fit for a custom-drawn editor with multi-cursor rendering and split views. End with ONE concrete recommendation for a minimal Helix-like editor with rationale, plus the exact crate versions to put in Cargo.toml (rust-version 1.96, edition 2024). Prefer primary sources: crates.io pages, GitHub repos, official docs.

---
**Output:**
Write your findings to exactly this path: /Users/335g/dev/other/mina/.pi-subagents/artifacts/outputs/e305ad88/research.md
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