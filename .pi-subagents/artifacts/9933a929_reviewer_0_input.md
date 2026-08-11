# Task for reviewer

Adversarial red-team review of the daemon/client IPC editor at /Users/335g/dev/other/mina against its spec (GitHub Issue #1: daemon owns all editor state, clients send Command and receive a full StateSnapshot; NDJSON over a unix socket in temp_dir/mina.sock; effectively-one-client assumption is a documented trade-off; daemon auto-starts from clients via setsid; LSP = rust-analyzer only, spawned on first .rs open; ADR-0008 caps file size at 16MiB and command lines at 1MiB; ADR-0009 runs LSP outside the daemon lock with 2s write / 10s request timeouts and try_lock diagnostic drains; ADR-0007 closes undo groups on leaving Insert AND on client disconnect).
Hunt for REAL bugs and spec violations, not style nits. Focus on:
1. IPC trust boundary: is the socket path predictable/unprotected (temp_dir/mina.sock, no uid in path — single-user assumption noted)? Can a malformed NDJSON frame, oversized line, or truncated connection hang or crash the daemon? Is the 1MiB command-line cap actually enforced on the read path?
2. Race conditions: ensure_daemon double-start (two clients racing to spawn), client disconnect during an in-flight request, daemon shutdown mid-write, connection reset while daemon holds a lock.
3. Undo group correctness: what happens on disconnect while in Insert mode — is the group closed and mode reset to Normal as ADR-0007 requires? Can edits from two different clients merge into one undo unit?
4. LSP: does a hung server actually fail to block the daemon lock (try_lock)? What if the LSP channel closes mid-request? Is didChange sent on EVERY edit (full text, O(n))?
5. Open/TOCTOU: ADR-0008 rejects non-regular files and files > 16MiB; check the metadata check vs read race, and whether the recently-added Open TOCTOU fix is sound.
6. StateSnapshot correctness: does every response actually carry the full state (document text, selection, mode, viewport, diagnostics)? Any path where a response is partial or stale?
Read mina-term/src/{daemon.rs,client.rs,lsp.rs,render.rs,session.rs}, mina-lsp/src/lib.rs, mina-protocol/src/lib.rs. Run `cargo test --workspace` and `cargo clippy --workspace` (note any warnings). Output: a prioritized list of findings, each with file:line, severity (CRITICAL/HIGH/MEDIUM/LOW/NIT), and a one-line suggested fix. Distinguish confirmed bugs from theoretical risks.

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