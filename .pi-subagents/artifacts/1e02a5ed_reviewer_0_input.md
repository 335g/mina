# Task for reviewer

You are verifying, item by item, whether the Rust workspace at /Users/335g/dev/other/mina actually implements what GitHub Issue #1 claims. The issue (title "mina-term v1: daemon/client 分割エディタ + 最小 LSP", all slices S0-S4 marked done) claims:
- S0: mina-protocol crate with Command/StateSnapshot/Diagnostic types + serde round-trip tests; mina-term daemon/client skeleton with unix socket + NDJSON framing; auto-start daemon with setsid process separation.
- S1: termina 0.3 TUI (raw mode + alternate screen + EventStream), keymap prefix trie per Mode resolved ONLY client-side (gg / G), movement hjkl/wb/arrows, selection extension (v+l), scroll Ctrl-D/U, real state send/receive with auto-start daemon.
- S2: editing commands Insert/DeleteBackward/DeleteForward/DeleteRange/Undo/Redo/Save; Insert-mode input grouped into ONE undo group with open/close on mode transition managed daemon-side; mina-view path/dirty management (open_with_path / focused_path / is_dirty / mark_saved); mina-core delete_forward + transaction delete_*; Shift+alphabet key normalization.
- S3: mina-lsp crate (spawn, Content-Length framing, request/response correlation, UTF-8/UTF-16 position conversion); daemon spawns rust-analyzer on first .rs open (not eager), initialize -> didOpen, full-text didChange per edit, publishDiagnostics drained into StateSnapshot; rendering underlines diagnostics + status line [nE nW]; supports both position encodings.
- S4: `mina session get` / `mina session exec '<JSON>'` sharing ensure_daemon/request infra with the TUI; invalid JSON / unknown subcommand exit 1.
- Explicitly OUT of v1 scope (must be ABSENT): mouse support, multiple simultaneous TUI clients, diffed/partial snapshots, LSP config TOML file, non-UTF-8 file I/O, split display, explicit daemon stop command, LSP hover/goto/completion.
- Trade-offs recorded as `ponytail:` comments: full snapshot per response (O(n)/key), UTF-8-only I/O, effectively-1-client assumption, socket path single-user (temp_dir/mina.sock).
VERIFY EACH CLAIM against the code (read mina-protocol/src/lib.rs, mina-term/src/{main.rs,daemon.rs,client.rs,keymap.rs,render.rs,session.rs}, mina-view/src/editor.rs, mina-core/src/edit.rs, mina-lsp/src/lib.rs, Cargo.toml). Run `cargo test --workspace` and note the actual pass count (issue says 118 at S4). Report per-claim: MATCH / PARTIAL / MISSING with file:line evidence. For out-of-scope items report ABSENT or PRESENT (a PRESENT finding is a scope violation). Also note anything the issue claims that you cannot find evidence for. Be strict and adversarial: look for claims that are true only in tests but not in the real path. Output a compact numbered findings list.

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