# Task for reviewer

Architecture/ADR conformance audit of /Users/335g/dev/other/mina against GitHub Issue #1's stated architecture decisions and the ADRs in docs/adr/.
Check each:
1. ADR-0004: termina 0.3.x with event-stream feature is the terminal backend; mina owns cell buffer + frame diff (no ratatui/crossterm). Verify Cargo.toml and render.rs actually diff frames rather than redrawing naively.
2. ADR-0005: daemon owns documents/histories/LSP; clients are stateless-ish shells. Verify daemon-side state ownership (is any editor state held client-side?).
3. ADR-0006: request/response only, NO server-push channel; every response is a full StateSnapshot; NDJSON framing; no binary serialization. Verify no hidden push/event path exists (e.g., a notification loop, a separate streaming channel).
4. Keymap: prefix trie per Mode, resolved client-side only (issue: "解決はクライアント側のみ"). Verify daemon has no keymap and client resolves sequences like gg/G.
5. Daemon lifecycle: auto-start (emacsclient-style) from client, plus explicit `mina daemon serve`. Verify both exist and auto-start is raceless-ish.
6. LSP: embedded table (rust-analyzer only), spawned on FIRST .rs open (no eager spawn at daemon boot). Verify no other LSP is spawnable and no eager spawn.
7. ADR-0007 undo groups: open on Insert entry, close on leaving Insert (to Normal OR Select) or client disconnect (which also returns to Normal). Verify the daemon-side open/close logic and the disconnect path.
8. ADR-0008: Open rejects non-regular files and files > 16MiB with status message and NO editor state change; command lines bounded at 1MiB.
9. ADR-0009: LspSession behind its own Arc<Mutex>, all LSP I/O awaited OUTSIDE the daemon lock with 2s write / 10s request timeouts; diagnostic drains use try_lock; dead server detected via channel close; respawn only on next .rs Open (no respawn loop). Also check docs/adr/0009 was written to match the actual code (commit 05fe623).
10. Newer commits beyond the issue: f70f4bd (Insert Enter/Tab), c15642d (document count limit + evict), 0306a07 (undo history cap 1000) — do these contradict any v1 scope decision? Note: issue is still OPEN with needs-triage label despite "v1 完了" comment — flag this process staleness.
Read docs/adr/*.md fully and the source in mina-term/, mina-lsp/, mina-view/, mina-core/. Run cargo test --workspace. Output: per-item CONFORM / DEVIATION / STALE with file:line evidence, plus a short list of ADR-vs-code inconsistencies where the docs describe something the code doesn't do (or vice versa).

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