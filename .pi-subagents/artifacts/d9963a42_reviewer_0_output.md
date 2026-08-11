All verification complete. I have traced every claim, run the full test suite, and live-probed the daemon over the real unix socket. Here is the adversarial review.

---

# ANGLE C — CLAIM-vs-CODE ADVERSARY REVIEW

Scope: Issue #1 (mina-term v1 spec) vs. the code at `HEAD` (90b6bd8). Evidence: full source read of all listed files + ADRs 0005–0009, `cargo test`/`clippy`, and live socket probes against the built daemon (invalid command, oversize line, abrupt-disconnect undo grouping, save→undo dirty tracking).

## CONFIRMED-GOOD (claims verified to hold)

| Claim | Evidence |
|---|---|
| **H1 / ADR-0007**: undo group closes on leaving Insert (incl. →Select) and on disconnect, mode reset to Normal | daemon.rs:66-72 (`on_client_disconnect`), daemon.rs:390-407 (SetMode begin/end), tests `leaving_insert_for_select_closes_undo_group`, `disconnect_in_insert_mode_closes_group_and_resets_mode`. **Live probe**: abrupt close in Insert → next client sees Normal, text intact; session B typing undoes separately (`abcd`→undo1 `abc`→undo2 `''`). No leak path found: crash/EOF/read-error/write-error all funnel to the break → handler. Only residual: a panic inside the handler task would skip the close (no plausible panic path found in `apply`/`snapshot`). |
| **H3**: Save marks the *saved* doc clean, not the focused one | daemon.rs:213-218 captures `doc_id` under lock before the async write, then `mark_saved_doc(doc_id)`; editor.rs `mark_saved_doc` removes only that ID from `dirty` (BTreeSet). Tests `save_marks_the_saved_doc_not_the_current_focus` + `mark_saved_doc_marks_only_the_named_document` cover the focus-change case. |
| **M1 / ADR-0009**: every `lsp::` await runs outside the daemon lock | All 7 `lsp::` call sites traced: `ensure` (daemon.rs:156) holds the daemon lock only in two brief no-await sections (lsp.rs:242-247, 262-267); `open_document` (daemon.rs:182) and `sync` (daemon.rs:234) are called after `drop(d)`; `drain_into` (daemon.rs:186,237) is synchronous (`try_lock`, no await). Lock order is daemon→lsp only, never inverted → no deadlock. |
| **drain_into `try_lock` skip**: no permanent diagnostic loss | lsp.rs:308-320. Notifications sit in an unbounded channel; every command drains; within one connection, `sync`/`open_document` complete before the drain, so the lock is free. A skipped drain just delays diagnostics to the next command (documented "次のスナップショット" design). |
| **M7**: invalid command cannot leave the client waiting forever | daemon.rs:240-245 returns a status snapshot on parse error; oversize line closes the connection (client gets EOF → error, not a hang). **Live probe**: `"this is not json"` → response with `status:"invalid command"`; 1MiB+1 line → connection closed, 0 bytes. No code path sends no response without also dropping the socket halves. |
| **SEC-1**: `take(MAX_CMD_LINE+1).read_line` bounds memory | daemon.rs:135-147. The `Take` adapter caps bytes read at MAX+1, so `line` cannot exceed 1MiB+1; oversized line breaks the connection. Test `oversized_command_line_closes_connection` + live probe confirm. |
| **is_edit()**: all text-changing commands synced, non-edits excluded | daemon.rs:228-236. List = Insert/DeleteBackward/DeleteForward/DeleteRange/Undo/Redo (complete — no other command mutates text); SetMode/Goto/Move/Extend/Scroll/SetViewport/GetState correctly excluded; Open/Save handled in the I/O branch. `sync` also guards `current_uri == focused doc`, so non-.rs edits never hit the server. |
| **3a / commit 0306a07**: undo history capped at 1000 | history.rs:20-22, push() trims oldest groups; test `undo_history_is_bounded` (undoable == 1000). Redo is transitively bounded (each undo moves exactly one group). |
| **M4** TerminalGuard restores terminal on all paths; **M6** stale-socket probe (daemon.rs:100-118); **M2** notify 2s / request 10s timeouts (mina-lsp lib.rs:101-131); **M3** dead-server detection via channel close + respawn only on .rs Open (lsp.rs:284-289, ADR-0009); **5c** LineIndex O(line) conversion (lsp.rs:177-217); **5f** MAX_FRAME_BYTES (mina-lsp lib.rs:13); **SEC-2** render sanitization; **H2** CRLF handling — all match their comments and carry tests. |

## FINDINGS

**F1 — minor (verified live): dirty false-negative after save→undo.** daemon.rs Save clears `dirty`; `Editor::undo` (editor.rs:214-221) never re-marks. Probe: open→Insert "X"→Save→Undo ⇒ `dirty:false` while text `'original content\n'` ≠ disk `'Xoriginal content\n'`. The comment at editor.rs:114 acknowledges only the false-positive direction ("undo で保存時点まで戻っても dirty は残る"); the riskier false-negative (buffer differs from disk, no dirty marker) is unacknowledged and contradicts a literal reading of the "dirty survives undo past save point" claim. *Fix: keep a save-point marker in History, or re-derive dirty by diffing current text against last-saved content on undo/redo.*

**F2 — minor: H3 same-doc race leaves an unsaved edit marked clean.** daemon.rs:213-218: if another connection edits the *same* document while the Save write is in flight, `mark_saved_doc(doc_id)` clears the dirty of that un-saved edit. The H3 test covers only the different-doc/focus-change case. *Fix: re-check captured text == current text before clearing dirty.*

**F3 — minor: `utf8_col_to_char` panics on a non-char-boundary byte column.** mina-lsp/src/position.rs:4-7: `line[..byte_col]` panics when `byte_col` splits a multi-byte char (reproduced via scratch harness for `"あい",1` and `"a😀b",2`). Runs inside `drain_into` under both the daemon and lsp locks; a panic kills the connection task (in-flight command gets EOF, group-close on disconnect skipped). Reachable only from a utf-8-encoding server emitting non-boundary columns (rust-analyzer selects utf-16); still a defensive gap in the untrusted-server path. *Fix: clamp `byte_col` to the previous char boundary (walk back while !is_char_boundary).*

**F4 — minor: Open silently no-ops when `read_to_string` fails.** daemon.rs:322-333: `(tokio::fs::read_to_string(path).await.ok(), None)` — on failure (e.g. non-UTF-8 file, which the issue lists as out of scope but must still be *reported*) returns `(None, None)`; the handler then opens nothing and reports `status: None`, contradicting the comment "失敗時は状態を変えず status で報告する" and ADR-0008's status-reporting promise. *Fix: map the read error to a `cannot open …`-style status.*

**F5 — minor: TOCTOU defeats ADR-0008/SEC-1's file bound.** daemon.rs:322-333: metadata check (`is_file` + ≤16MiB) and the actual read are separate syscalls; a path swapped to a FIFO after the check makes `read_to_string` block the handler forever (that client never gets a response — a partial M7 gap), and a file grown past the cap defeats the OOM bound. Requires local adversarial FS races; note the residual rather than treating the claim as airtight. *Fix: open the fd, fstat the fd, and read from the fd.*

**F6 — minor: diagnostics can be positioned against stale text.** mina-term/src/lsp.rs:143-162: no version correlation between the last `didChange(N)` and the queued `publishDiagnostics` (the protocol carries no version). A slow server's diagnostics for text N-1 are converted against current text N → transient wrong underline positions until the server catches up. Self-correcting, consistent with the documented "next snapshot" design; worth a note, not a fix in v1.

**F7 — minor: M7 and M5 have zero automated test coverage.** No test exercises the `Err(_) => "invalid command"` branch (daemon.rs:240-245), and client.rs has no test module at all — the M5 first-status retention (client.rs:39-52) is correct by inspection (SetViewport responses always carry `status:None`, so the merged `first_status` can neither be lost nor wrongly kept) and live-verified, but regression-prone. *Fix: add a socket-level test for both.*

**F8 — minor: "clippy クリーン" is overstated.** `cargo clippy --workspace --all-targets` → 0 errors but **9 warnings** (dead_code `lsp_pos_to_char` at lsp.rs:225, 4× collapsible-if, 2× identical blocks in mock_server, loop-counter lint in render.rs:27). No functional impact.

**F9 — minor: 5th concurrent connection waits forever.** daemon.rs:95-109: `MAX_CONNECTIONS=4` semaphore; a 5th client sits unaccepted with no response indefinitely if 4 others stay connected — an M7-class permanent wait, mitigated only by the effectively-one-client model. *Fix: accept and reply "busy", or add a connect timeout.*

**F10 — info: the 1000-cap bounds *groups*, not memory.** history.rs:20-22 + push(): a single Insert session accumulates unbounded changes inside one group, so "メモリ有界化" is partial (memory scales with change count, not group count). Practical exposure is low (1MiB/command cap dominates).

**F11 — info (acknowledged in code):** concurrent `.rs` Opens can orphan a spawned rust-analyzer process (lsp.rs:262-267, `ponytail:` comment).

## Review
- **Correct**: H1, H3, ADR-0007, M1/ADR-0009, drain_into skip, SEC-1 (cmd line), is_edit(), 1000-cap, M2/M3/M4/M5/M6/M7, 5c/5f, SEC-2, H2 — all verified by code trace, tests, and/or live socket probes. Issue #1's v1 spec is faithfully implemented.
- **Blocker**: none. No critical/major defect found; the strongest claims (M1, H1, H3, M7, SEC-1) hold under adversarial tracing.
- **Note**: all findings are minor/info. F1 is the only one with user-visible behavioral consequence (wrong dirty marker after save→undo, verified live). F3/F5 are the ones to harden if the trust model ever extends beyond "well-behaved local LSP server / single user".
- No files were modified during this review.