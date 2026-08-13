All files read. Compiling the adversarial review now.

## Summary

ADR-0012's core machinery (generation, 128-ring, Hello handshake, external-change detection) is implemented and well-tested end-to-end, but three adversarial findings hold: **rejected/no-op commands still bump the generation and log events** (spec says state unchanged → no bump), and **reuse-Open leaves the disk baseline stale, silently disabling focused-document monitoring** in multi-document flows. `cargo test` could not be executed (no shell tool in this session); tests were verified statically.

## Findings

### 1. MAJOR — Rejected operations still bump generation and log events (spec: state unchanged → no bump)
- **Spec**: ADR-0012 — "bumped on every state-changing operation (edits, undo/redo, mode changes, Open/Save), never on pure reads"; review target: "Does a REJECTED operation (bad checksum, failed Open) bump? (Spec: state unchanged -> no bump.)"
- **Evidence**: `mina-term/src/daemon.rs:520-546` — the event tuple is matched from the command *before* application, and `d.record_event(source, kind, range, text)` at line 546 runs unconditionally after `apply_from`, with no check that state actually changed. The oversized-Insert rejection at `daemon.rs:804-805` (`return snapshot(daemon, Some("file too large: insert rejected".into()))`) provably changes nothing — test `rejected_insert_keeps_undo_group_open` asserts selection, mode, and undo-group are untouched and `!s.dirty`. Yet the wire path still records an `Insert` event and bumps `generation`. (The DocumentEdit path at `daemon.rs:604-606` is correctly guarded by `if rejected.is_none()`; failed Open at `daemon.rs:438-440` correctly does not record.)
- **Verdict**: DEVIATES — **MAJOR** (direct contradiction of the generation contract; rare trigger, ~16MiB docs, but the review explicitly targets it, and the false event pollutes the 128-slot ring).
- **Fix direction**: make the record step conditional on actual state change — e.g. `apply_from` returns whether it applied, or compare `can_undo()`/text before/after; reject paths return a "no change" marker.

### 2. MINOR — No-op deletes bump generation and record Delete events
- **Spec**: same as above ("state unchanged -> no bump").
- **Evidence**: `daemon.rs:524-533` records `EventKind::Delete` for `DeleteBackward`/`DeleteForward`/`DeleteRange` without checking the transaction is a no-op. `mina-core/src/edit.rs` proves no-ops: `delete_backward_at_start_is_noop` (pos 0), `delete_forward_at_end_is_noop` (end), `delete_range_on_cursor_is_noop` (point selection). When the doc is already dirty, nothing at all changes, yet generation bumps + a Delete event with the pre-delete selection range lands in the ring. (Mitigating: on a clean doc, `Editor::apply` does set dirty + a history entry, so the bump is partially justifiable.)
- **Verdict**: DEVIATES — **MINOR** (edge case).
- **Fix direction**: same as #1 — record only when the transaction actually changed the document.

### 3. MAJOR — Reuse-Open leaves disk baseline stale → external-change monitoring stops/misattributes
- **Spec**: ADR-0012 — "records (path, mtime, size) at Open and after each successful Save, and a background task stats the file every ~1–2 seconds" for "the focused document"; review target: "baseline refreshed on next Open/Save".
- **Evidence**: the reuse branch `daemon.rs:397-401` records an `Open` event but never touches `disk_baseline` or `disk_changed`; only fresh Open (`daemon.rs:431-438`) and Save (`daemon.rs:496-503`) refresh them. `watch_disk` at `daemon.rs:259-265` skips whenever `focused_path() != baseline.path`. Scenario: Open A (baseline=A) → Open B (baseline=B) → agent re-Opens A (reuse, focus=A, baseline still B) → `watch_disk` skips every tick, so external changes to the focused document A are never detected, while `disk_changed` in A's snapshots reflects B's state. "Focused-document monitoring covers the whole surface" (spec) is violated.
- **Verdict**: DEVIATES — **MAJOR** (feature silently disabled; wrong-document flag attribution; triggered by the spec's own canonical flow "an agent Opens before editing" when ≥2 docs are involved).
- **Fix direction**: on every Open including reuse, re-stat the focused path and refresh the baseline (keep `disk_changed` set if the same doc still diverges; clear only on baseline refresh for a same-state doc).

### 4. MINOR/AMBIGUOUS — SetMode takeover and preempt mode changes are invisible in the event stream
- **Spec**: ADR-0012 — "one event per state-changing op"; event kinds include SetMode.
- **Evidence**: `daemon.rs:538` guards the SetMode event with `convert_mode(*mode) != d.editor.mode()`, so a non-owner `SetMode(Insert)` while already Insert (takeover) records no event even though `apply_from` runs `end_group()`+`begin_group()` (undo-group boundary changes). Likewise `preempt` (`daemon.rs:771-781`) flips mode Insert→Normal as a side effect of another client's edit, which is reported only under the edit's event kind, not SetMode.
- **Verdict**: AMBIGUOUS — **MINOR** (undo grouping/mode side effects are state, but the spec's event-per-op granularity doesn't explicitly cover preempt collateral; mode/text visible in the snapshot).
- **Fix direction**: accept as documented behavior, or emit a SetMode event when preempt/takeover changes mode or group ownership.

### 5. MINOR — Empty-text Insert / empty DocumentEdit recorded as state changes
- **Evidence**: `daemon.rs:521-523` records an Insert event for `text == ""`; `apply_edit` records ReplaceRange for a no-op `start==end`+empty-text edit. `Editor::apply` does insert a history entry and set dirty, so a (marginal) state change exists.
- **Verdict**: CONFORMS-with-nuance — **MINOR** observation, not a deviation worth fixing alone; folded into #1's fix (record only real changes).

### Verified CONFORMS (per review-target checklist)
- **Generation on real ops**: Open (fresh `daemon.rs:438`, reuse `:401`), Insert/Delete/Undo/Redo/SetMode (`:546`), Save (`:503`) all bump; test `generation_increments_on_state_changes_only`. Rejected DocumentEdit (bad checksum / out-of-bounds, `daemon.rs:604-606`) and failed Open (`:438-440`) and failed Save (`:507`) do **not** bump — the intended guard exists and is correct for these paths.
- **No bump on reads/interaction**: `_ => None` at `daemon.rs:539-540` covers Move/Extend/Goto/Scroll/SetViewport/GetState.
- **Ring**: bounded 128, oldest evicted (`daemon.rs:138-141`); test `event_ring_is_bounded_and_evicts_oldest`. `event.generation == snapshot.generation` by construction (`record_event` bumps then stamps; `snapshot` at `daemon.rs:723-724` carries `daemon.generation`). Ring rides every snapshot.
- **Kinds/sources**: protocol `EventKind` set matches spec exactly (`mina-protocol/src/lib.rs`); ExternalChange stamped `EventSource::External` (`daemon.rs:276`).
- **Hello handshake**: first-line-only enforcement with timeout (`daemon.rs:325-341`); non-Hello and invalid kind → disconnect (test `hello_handshake_rejects_non_hello`); TUI sends `ClientKind::Interactive` (`client.rs:76-79`), session exec **and** edit send `Headless` (`session.rs:112-117` and `:126-131`); source derives from the handshake, not message type (`daemon.rs:342-344`, used at 401/438/503/546) — verified by test `events_carry_correct_source` (DocumentEdit from a Headless client is labeled Headless; TUI commands Interactive).
- **External change**: 2s interval (`daemon.rs:251`), mismatch (size/mtime) or stat error → deleted file (`:272-273`) → `disk_changed=true` + one ExternalChange event (suppressed until baseline refresh, `:256-257`); no auto-reload (text untouched; test `external_change_is_detected_without_auto_reload`); baseline refreshed on fresh Open and successful Save.

## Test results

- `cargo test --workspace` — **not-run** by this reviewer: no shell tool available in this session. Static analysis: the 5 ADR-0012 acceptance tests (`generation_increments_on_state_changes_only`, `events_carry_correct_source`, `event_ring_is_bounded_and_evicts_oldest`, `hello_handshake_rejects_non_hello`, `external_change_is_detected_without_auto_reload`) are well-formed and exercise the real socket path; the two LSP tests self-skip when `target/debug/mock-server` is absent, and `cargo test --workspace` builds it (binary in `mina-lsp/Cargo.toml`), so the acceptance flow is coherent. **Supervisor must run it.**
- `cargo test -p mina-term` — **not-run**, same reason. **Supervisor must run it.**
- `gh issue view 9` — **not-run**, same reason (issue acceptance criteria were taken from the task text).

## Untested claims

- Rejected-oversized-Insert bump behavior through the wire path (finding 1) — unit tests call `apply_from` directly, bypassing `record_event`, so the bug is invisible to the suite.
- Reuse-Open baseline/`disk_changed` behavior (finding 3) — no test covers multi-doc refocus + external change.
- No-op delete generation bump (finding 2) — no test.
- Generation stability of Move/Scroll/SetViewport — only GetState is tested for non-bump.
- External **deletion** of the file (only content modification is tested; the `Err(_) => true` path is untested).
- Failed Save → no event/no bump — untested.
- Rejected DocumentEdit generation behavior — tests assert text/undo unaffected but not generation/events.