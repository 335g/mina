# Task for reviewer

Task: ANGLE B — SECURITY/ROBUSTNESS ADVERSARY: Try to break the daemon. Attack surfaces: unix socket path (temp_dir/mina.sock — world-writable /tmp? symlink attacks? stale socket removal race?), NDJSON parsing (oversized lines, invalid UTF-8, unbounded read_line), file Open path (TOCTOU between metadata check and read, /dev/zero, FIFO, size race, symlinks, non-UTF8), resource exhaustion (MAX_CONNECTIONS=4 — is accept actually bounded? fd leak? semaphore leak on panic?), daemon double-start race (two clients spawning simultaneously), client half-open connections (writer stuck? reader stuck? no timeouts anywhere?), LSP spawn bombs (rust-analyzer restarts, no timeout on non-initialize calls), state corruption via crafted commands (negative/oversized scroll pages, huge insert text, degenerate selections). For each: exploit sketch + whether code actually defends + severity.

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