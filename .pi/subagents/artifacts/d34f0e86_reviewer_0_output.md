I have completed a full read of the slice. Here is the adversarial review.

## Summary

ADR-0011 (Command/DocumentEdit split) and ADR-0007 (undo-group boundaries) are implemented and tested to a high standard, with genuine e2e socket coverage. However, two real defects survive: **(1) CRITICAL** — a DocumentEdit that shortens the document before the cursor panics the daemon task in `scroll_to_cursor` (`char_to_line` on an out-of-range selection), leaving the edit applied but unlogged (no generation bump, no event) and the daemon's selection permanently out of range; **(2) MAJOR** — an open undo group is orphaned (left `grouping=true`) on the original document when another client Opens a different path mid-Insert-session, so a later write from another client merges into the previous session's group, violating ADR-0007's "a single UndoGroup never contains edits from different Clients".

## Findings

### 1. CRITICAL — DocumentEdit can panic the daemon task; edit applied without generation/event; selection left out of range
- **Spec**: ADR-0011 requires DocumentEdit to "neither read nor change the Selection" (snapshot selection byte-identical) while still being a real edit; ADR-0012 requires generation bump + ChangeEvent on every state-changing operation.
- **Code evidence**: `apply_edit` (minae-term/src/daemon.rs:748) validates, then `preempt`, then:
  - daemon.rs:770 `let selection_after = daemon.editor.selection();` (raw, unmapped)
  - daemon.rs:771 `daemon.editor.apply(tx, selection_after)` — `Editor::apply` (minae-view/src/editor.rs:319) stores the **unchanged** selection and inserts the new doc
  - daemon.rs:772 `daemon.editor.scroll_to_cursor(...)` — editor.rs:389-393 calls `rope.char_to_line(selection.primary().head())`, which **panics** when `head > len_chars` (ropey 1.6.1 rope.rs:689, `char_to_line` → `try_char_to_line(...).unwrap()`).
- **Trigger**: doc "hello world", TUI cursor at 11 (`Goto DocumentEnd`), agent sends `DocumentEdit { start:0, end:11, text:"hi", checksum:fnv1a64(b"hello world") }`. Bounds/checksum pass; doc becomes "hi" (2 chars); selection stays [11,11]; `char_to_line(11)` panics. Since the panic occurs after `Editor::apply` but before the handler's `record_event` (daemon.rs:627-635), the edit **is** applied (text, dirty, history) with **no generation bump and no event** — violating ADR-0012's change-detection contract. The connection task dies without a response, and the daemon's stored selection stays [11,11], so subsequent commands that call `scroll_to_cursor` (e.g. `Move` Line via `j/k` — `step_line` returns the out-of-range `char_pos` unchanged, daemon.rs:876) panic again until a clamping command (Char move, Insert) runs. The existing e2e test `document_edit_replaces_range_without_touching_selection` (daemon.rs:1694) avoids this by placing the cursor at [0,0].
- **Verdict**: DEVIATES — **CRITICAL** (panic risk + spec-breaking generation/event contract + daemon wedge).
- **Fix direction**: clamp only for the scroll computation in `scroll_to_cursor` (e.g. `head.min(len_chars())` — and `first_line` arithmetic) without mutating the stored selection, preserving the byte-identical selection required by ADR-0011. Optionally also reject/repair in `apply_edit`.

### 2. MAJOR — Open undo group is orphaned when focus leaves the document during an Insert session; later cross-client write merges into it
- **Spec**: ADR-0007: group "closes when it leaves Insert (to Normal or Select), or when the owning Client disconnects"; "a write from any other Client closes it first (last write wins — a single UndoGroup never contains edits from different Clients)".
- **Code evidence**: `begin_group`/`end_group` are per-document (`Editor::end_group` → `history_mut()`, editor.rs:374). The only closers — SetMode handler (daemon.rs:920-925), `preempt` (daemon.rs:783-791), `on_client_disconnect` (daemon.rs:171-177) — all call `end_group` on the **currently focused** document only. `Editor::open`/`focus_open_path` (editor.rs:186-196) switch `view.doc` without touching the previous document's History.
- **Trigger**: TUI (conn 1) on X: `SetMode(Insert)` → `begin_group` on X; `Insert "a"` → X.history `grouping=true, group_open=true, undo=[[a]]`. Agent (conn 2) sends `Open(Y)` → focus switches to Y; mode stays Insert. TUI sends `SetMode(Normal)` → `end_group` runs on **Y's** history (no-op); X's history remains `grouping=true, group_open=true`, `insert_owner=None`. Agent then sends a `DocumentEdit`/`session exec Insert` on X: `preempt` is a no-op (`insert_owner` is `None`), and `History::push` (history.rs:66-81) appends to the still-open `[a]` group → one group now contains the TUI's edit *and* the agent's edit; a single undo reverts both (the exact failure ADR-0007 was written to prevent). The same orphan results from owner-disconnect while focus is on Y. Not covered by any test.
- **Verdict**: DEVIATES — **MAJOR** (edge-case sequence, but directly contradicts the ADR-0007 invariant and destroys undo granularity).
- **Fix direction**: close the open group on the **previous** focused document whenever focus moves away (Open handler), and/or have the disconnect/preempt/leave-Insert paths close any still-grouping history.

### 3. MINOR — DocumentEdit has no `MAX_FILE_SIZE` growth cap (asymmetric with `Command::Insert`)
- **Spec**: ADR-0008 caps Open at 16 MiB; the daemon extended the same cap to `Command::Insert` (daemon.rs:804) "Insert による無制限の文書成長を防ぐ".
- **Code evidence**: `apply_edit` (daemon.rs:748-777) performs no size check; only the 1 MiB `MAX_CMD_LINE` bounds each single edit, so repeated `session edit` inserts can grow a document past 16 MiB without bound (each full-snapshot response then doubles in size — the exact memory vector ADR-0008 addresses). No test.
- **Verdict**: DEVIATES (code does less than the sibling path) — **MINOR**.
- **Fix direction**: apply the same `doc_bytes + text.len() > MAX_FILE_SIZE` rejection in `apply_edit` before `preempt`/apply.

### 4. AMBIGUOUS/MINOR — "non-char-boundary" start/end cannot be detected or rejected; byte-miscounting clients get silent char-index semantics
- **Spec**: task requirement — non-char-boundary start/end rejected with status, NOT clamped, NO panic.
- **Code evidence**: the wire `Range` and `DocumentEdit.start/end` are documented as **char indices** (minae-protocol/src/lib.rs:78, 92; ADR-0011 "explicit character ranges"), and bounds are checked as `start <= end && end <= text.chars().count()` (daemon.rs:753-756). Every index in `[0, len_chars]` is a valid char boundary, so the daemon cannot distinguish a client that miscounted bytes. A byte-miscounting client (e.g. `start:1` inside a 2-byte char) passes checksum+bounds and the edit is applied at char-index semantics — no rejection, but also no panic and no clamping (verified: `Transaction::insert` uses ropey char-index `slice`; movement helpers clamp via `char_to_byte` → `text.len()`).
- **Verdict**: AMBIGUOUS — **MINOR**. The safety properties (no panic, no clamp) hold; the rejection requirement is un-implementable with the char-index wire type and is only observable if the wire were byte-addressed.
- **Fix direction**: none required for v1; if byte-addressed edits are ever needed, add a separate wire type or document that indices are char-based.

### 5. MINOR observation — #7 reuse does not restore the re-opened document's cursor position
- **Spec**: #7 requires re-Open of an already-open path to preserve unsaved edits, dirty, and history.
- **Code evidence**: `focus_open_path` (editor.rs:186-196) changes only `view.doc`; selection/first_line remain whatever the previous Open left (e.g. point 0). Text/dirty/history are preserved (tested in `reopen_already_open_path_preserves_unsaved_edits`), but the TUI's cursor position on the re-focused doc is lost. The daemon comment ("再利用 = 何も変えない") documents this as intentional.
- **Verdict**: CONFORMS to the letter of #7 — **MINOR** UX note, not a defect.

### Verified-conformant items (with evidence)
- **Checksum semantics**: FNV-1a64 over current full doc UTF-8 bytes, stable implementation (lib.rs:103-109); check + apply under one daemon lock (no TOCTOU); mismatch → status "document changed since read", no `record_event`, no apply — daemon.rs:753-761, and the `record_event` guard at daemon.rs:627-635. Tests: `document_edit_rejects_stale_checksum`.
- **Out-of-range rejection**: rejected with status, no state change, no undo entry, no panic (daemon.rs:753-766); tested (`document_edit_rejects_out_of_bounds_without_state_change`).
- **Selection untouched on success**: `selection_after` == current selection, recorded as before==after in history so undo doesn't move it; snapshot selection byte-identical (daemon.rs:770-771, editor.rs:319-337); tested at cursor [0,0].
- **Shared Transaction machinery**: same `Editor::apply` → dirty + History push; undo/redo at group level (`document_edit_is_undoable_as_one_group`); LSP didChange full-text sync + pull diagnostics (`document_edit_syncs_lsp_full_text`).
- **Ownership per connection**: `insert_owner: Option<u64>` with per-connection `conn_id` (daemon.rs:88-92, NEXT_CONN_ID); preempt/SetMode-takeover/disconnect are owner-scoped; invariant `mode==Insert ⟺ owner==Some` holds on all paths (tests: `non_owner_disconnect_keeps_insert_session`, `non_owner_edit_takes_over_closing_owners_group`, `document_edit_preempts_open_insert_group`, `real_socket_agent_disconnect_keeps_tui_undo_group`).
- **history.rs stack design (issue #9)**: linear undo/redo stack pair (`Vec<Vec<Change>>` each, history.rs:36-37) — no undo-tree/branching, consistent with "two-stack undo deferred". A **still-open** Insert group IS undoable (`History::undo` pops regardless of `grouping`, history.rs:118-136; undo sets `group_open=false` so later pushes start fresh) — allowed by ADR-0007; the TUI can't trigger it in Insert mode (keymap fallback inserts the char instead).
- **`session edit` CLI**: exists, parses `DocumentEdit` JSON, sends Hello(Headless) + edit, prints pretty snapshot; command success conveyed via `status` (session.rs:24-31, 68-76).
- **Rejections don't disturb undo groups**: `Command::Insert` size rejection and DocumentEdit checksum/OOB rejection occur before `preempt` (daemon.rs:804-808, 760-767); tested for Insert (`rejected_insert_keeps_undo_group_open`).

## Test results

I have no shell/exec tool in this review environment, so **`cargo test -p minae-term -p minae-view -p minae-protocol` was not run**. Code was reviewed read-only; all cited tests were inspected in-source (minae-term/src/daemon.rs test module, minae-view/src/history.rs, editor.rs, minae-core, minae-protocol). Commands the supervisor should run:

- `cargo test -p minae-term -p minae-view -p minae-protocol` (the two LSP tests skip unless `target/debug/mock-server` exists — run `cargo build --workspace` or `cargo test --workspace` first; they print "mock-server が未ビルドのためスキップ（cargo test --workspace で実行）").
- `gh issue view 7`, `gh issue view 8`, `gh issue view 9` — I could not execute these; my issue mapping relies on the ADR docs, CONTEXT.md, and the task statement.

## Untested claims

- **Rejection = zero state change**: tests assert text unchanged and no undo entry, but do **not** assert generation unchanged / no ChangeEvent / selection byte-identical on rejection (code is correct, but the contract is only partially asserted).
- **Byte-identical selection**: only tested with cursor at [0,0]; the critical "cursor at/past the edited range" case is untested — and it is exactly the case that panics (Finding 1).
- **Open-group undoability** while still in Insert: no explicit test (code path verified by reading).
- **`session edit` CLI end-to-end**: no test exercises the CLI binary; only daemon-level DocumentEdit tests and `parse_command` unit tests exist.
- **Finding 2 (orphaned group)** and **Finding 3 (size cap)**: no tests.

## residual risks

1. Daemon task panic + missing generation/event on doc-shortening DocumentEdits (Finding 1) — highest priority.
2. Cross-client undo-group merging via doc switch mid-Insert (Finding 2).
3. Unbounded doc growth via repeated DocumentEdits (Finding 3).
4. Byte-miscounting clients get silent char-index semantics (Finding 4).
5. Mock-server-dependent LSP tests skip unless built; verify with `cargo test --workspace`.