# Task for reviewer

Task: ANGLE C — CLAIM-vs-CODE ADVERSARY: The code comments claim specific fixes (H1, H3, M1, M3, M4, M5, M7, SEC-1, 5c, 6b, ADR-0007/0008/0009). For EACH claim, read the code path and verify the claim is actually true, not just asserted. Known suspicious spots to stress: (1) M5 first-status retention in client.rs (is there a path where status is lost or wrongly kept?); (2) H3 mark_saved_doc vs focused doc in daemon.rs Save; (3) ADR-0007 undo-group open/close on SetMode and disconnect — is there a leak path (client crashes mid-Insert without disconnect handler? Select→Insert→Normal ordering?); (4) M1 'LSP outside daemon lock' — trace every lsp:: call for accidental lock-held awaits; (5) drain_into try_lock skip — is there a race where diagnostics never arrive; (6) M7 invalid command response — can a client get stuck forever waiting on any path; (7) SEC-1 oversize line — does `take(MAX+1).read_line` actually bound memory; (8) is_edit() list — does every editing command that changes text get LSP sync, and are non-edits (SetMode, Goto) correctly excluded? Also check undo-history 1000-cap (commit 0306a07) and 'dirty survives undo past save point' claim.

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