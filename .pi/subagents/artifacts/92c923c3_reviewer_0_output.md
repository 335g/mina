I've read all slice-D files fully (document, selection, edit, movement, search, transaction, editor, history, mode, tree, protocol, daemon, client, session, keymap, main), traced the git log (commit fa1d46d confirmed as the multibyte line-move fix), and cross-checked ADR-0002/0006/0007/0011/0012 against the code. Note on tooling: this reviewer has no shell tool, so `gh issue view` and `cargo test` could not be executed — issue #1-#4/#7-#9 acceptance texts were not independently fetched, and test execution must be done by the parent. Findings below are from full code reading.

## Summary

The functional core (ADR-0002), transaction invertibility, dirty/save window, movement correctness (incl. the fa1d46d fix), ADR-0007 undo-group ownership, and ADR-0012 generation/events all conform to spec, with the Save path showing no data-loss window. Deviations found are concentrated in: issue-#7 path-reuse matching (lexical, and `session exec` doesn't absolutize), dirty being set by no-op edits and never cleared by undo-to-saved-state (documented), stale diagnostics/disk_changed across reuse-Open focus switches, and a no-op Undo from a non-owner silently closing an open Insert group without a generation bump.

## Findings

**F1 — Reuse-Open (issue #7) is bypassed by path spelling; `session exec` sends relative paths**
- Spec: CONTEXT View/Daemon + #7 intent in `editor.rs:170` "既に開かれているパスへフォーカスを戻す（#7）…文書のテキスト・dirty フラグ・undo/redo ヒストリーは保持される（ディスクからの再読込はしない）".
- Code: `minae-view/src/editor.rs:186` `self.paths.iter().find(|(_, p)| p.as_path() == path)` — exact `PathBuf` equality, no normalization; `minae-term/src/daemon.rs:377` `PathBuf::from(&path)` then `focus_open_path`. Absolutization exists only in the TUI (`minae-term/src/client.rs:38` `absolutize`); `minae-term/src/session.rs` `run`/`execute` pass `Command::Open` JSON through unchanged, so `minae session exec '{"Open":{"path":"rel/file"}}'` resolves against the daemon's spawn cwd and never matches an already-open absolute path.
- Verdict: DEVIATES. Severity: MAJOR (documented agent surface opens a second document for the same file — unsaved edits stay in the invisible old doc; `..`/symlink spellings also defeat reuse).
- Fix direction: canonicalize/normalize paths daemon-side before storing and comparing (or absolutize in `session.rs` like `client.rs` does).

**F2 — No-op edits set dirty**
- Spec: CONTEXT Dirty — "A Document that has been edited since it was last saved".
- Code: `minae-view/src/editor.rs:283` `self.dirty.insert(doc_id)` unconditionally in `apply`. Reachable: `session exec '{"Insert":{"text":""}}'`, `DeleteRange` on a cursor, `DeleteBackward` at pos 0.
- Verdict: DEVIATES (minor). Severity: MINOR. Fix direction: set dirty only when the transaction actually changes the text.

**F3 — Undo/redo never clear dirty, even when the restored text equals the saved text**
- Spec: CONTEXT Dirty — cleared by a successful Save only if text unchanged (implies: text == saved text ⇒ not dirty).
- Code: `minae-view/src/editor.rs:317-318,326-327` — `undo`/`redo` always `dirty.insert`; `is_dirty` comment at :145 documents the ponytail simplification ("履歴に保存時点のマーカーを持たない"); test `undo_after_save_marks_dirty` asserts it.
- Verdict: DEVIATES but deliberate and documented, conservative direction (never wrong-cleans). Severity: MINOR. Fix direction: save-point marker in History (deferred by design; safe to keep).

**F4 — Stale diagnostics and disk_changed across reuse-Open (focus switch)**
- Spec: ADR-0012 — diagnostics carried in snapshot; `disk_changed` flag per focused doc.
- Code: `minae-term/src/daemon.rs:410-440` reuse branch never clears `d.diagnostics` nor `d.disk_changed` (fresh-Open branch at :460-462 clears both). Between focus switch and the spawned `settle_open_diagnostics`, the snapshot reports the previous document's diagnostics; a stale `disk_changed=true` from doc A rides into doc B's snapshots.
- Verdict: DEVIATES (transient). Severity: MINOR. Fix direction: clear diagnostics/disk_changed in the reuse branch (re-pull self-heals diagnostics).

**F5 — Selection invariants enforced only in debug builds; duplicate ranges pass even in debug**
- Spec: CONTEXT Selection/Range — set of ranges, primary_index.
- Code: `minae-core/src/selection.rs:96-101` `debug_assert!` only (`ranges.windows(2).all(|w| w[0].end() <= w[1].start())` — allows `[2,2),[2,2)` duplicates). No release-mode validation; a duplicate cursor would double-insert. No reachable daemon path constructs duplicates today (find_matches emits strictly increasing starts).
- Verdict: AMBIGUOUS. Severity: MINOR (residual). Fix direction: enforce strict non-decreasing `end() < next.start()` for non-empty ranges.

**F6 — `map_pos` doc comment contradicts behavior for replaced-range interiors**
- Spec: `transaction.rs` doc "削除された範囲の内側の位置は削除開始点に詰められる".
- Code: `minae-core/src/transaction.rs:203-210` — for an insert-with-replacement, an interior position maps to `start + len(text)` (the Insert op advances `new` before the Delete clamps), not the deletion start. Only the replaced ranges themselves are ever mapped, and both endpoints land after the inserted text as intended — no reachable wrong behavior.
- Verdict: DEVIATES (doc only). Severity: MINOR. Fix direction: correct the comment.

**F7 — Word movement can stop mid-grapheme on combining sequences**
- Spec: CONTEXT/movement model — grapheme-correct movement (fa1d46d class).
- Code: `minae-core/src/movement.rs:246-256` `categorize` — combining mark U+3099 is neither alphanumeric nor whitespace → `Other`; `step_word` (char-based, no slicing → no panic) treats か+゙ as Word|Other and stops at char 1, mid-grapheme. Char/Line movement are grapheme-safe.
- Verdict: AMBIGUOUS (semantic quirk vs Helix; no panic). Severity: MINOR. Fix direction: skip combining-mark chars in word scanning (or accept as documented difference).

**F8 — Non-owner no-op Undo/Redo closes the owner's Insert group without any event or generation bump**
- Spec: ADR-0007 last-write-wins ("a write from any other Client closes it first") + ADR-0012 ("generation bumped on every state-changing operation (…mode changes…)").
- Code: `minae-term/src/daemon.rs:847-856` — `preempt` runs unconditionally in the Undo/Redo arms before `editor.undo()`, even when `can_undo()` is false (event guard at :806 `Command::Undo if d.editor.can_undo()` only suppresses the event). Result: an agent's no-op `{"Undo":null}` flips mode Insert→Normal, closes the human's session, and the snapshot shows the mode change with generation unchanged.
- Verdict: DEVIATES (edge; event/generation inconsistency). Severity: MINOR. Fix direction: skip preempt when the operation is a no-op (`!can_undo()`/`!can_redo()`).

**F9 — `watch_disk` performs blocking `std::fs::metadata` while holding the async daemon Mutex**
- Spec: ADR-0012 (2s poll).
- Code: `minae-term/src/daemon.rs:322-331`. Blocking I/O inside the async lock stalls all clients for the duration of the stat (microseconds normally, but on a wedged FS it blocks the whole daemon).
- Verdict: AMBIGUOUS (perf). Severity: MINOR. Fix direction: `tokio::fs::metadata` or stat outside the lock.

**F10 — Still-open Insert group: undoable; group semantics after undo** (asked by task)
- Code: `minae-view/src/history.rs:172-190` — `undo` pops the top group regardless of `grouping`/`group_open`; after the pop, `group_open=false` so subsequent pushes start a fresh group (never append to the undone one); redo restores the group but closed. Reaches daemon users only via `session exec Undo` as non-owner (TUI Insert keymap has no Undo binding — `keymap.rs:203-210`), which preempts first.
- Verdict: CONFORMS (ADR-0007 intent: closed groups never receive appends).

**F11 — Dirty/save window (task item 3)**
- Spec: CONTEXT Dirty — cleared only by successful Save and only if text unchanged since save began; per-Document.
- Code trace: `daemon.rs:521-551` — capture `(text, path, doc_id)` under lock → `tokio::fs::write` outside lock → `mark_saved_doc(doc_id, &text)` (`editor.rs:160-175`) compares current text to the captured text and removes only that doc's dirty; write failure → status only. Undo-during-save and evicted-doc branches are benign. No edit-lost or wrongly-cleared window found; concurrent Save race can only produce a cosmetic stale "still dirty" status.
- Verdict: CONFORMS.

**F12 — Functional core (ADR-0002)** — `Document` owns only `Rope` (`document.rs:11-17`); `Selection` never stored on Document; all edit primitives are `(document, selection) → (new_document, new_selection)` (`edit.rs:22-50`); imperative shell keeps per-View selections (`editor.rs:86-96`, split test `views_keep_independent_selections`). CONFORMS.

**F13 — Transaction invertibility** — inverse precomputed at construction with deleted text embedded (`transaction.rs:75-135`); `invert()` is a double swap; History undoes in strict LIFO, per-document, so each inverse applies to the exact state its change produced; all edits route through `History::push` (DocumentEdit included). CONFORMS.

**F14 — Movement correctness (task item 4)** — fa1d46d fix verified present (`movement.rs:158-166` relative-byte→relative-char conversion + regression test `move_line_over_multibyte_lines_does_not_panic`). Sibling edges checked: empty doc (Char/Line/Word all return 0), doc without trailing newline (`line_end == len` → clamp), Backward at pos 0, forward at doc end, extend anchor crossing — no slicing panic paths found (all slicing happens at char boundaries derived from `char_to_byte`). CONFORMS.

**F15 — ADR-0007 ownership model** — invariant `mode==Insert ⟺ insert_owner==Some` holds across SetMode (daemon.rs:906-946), preempt (:726-739), and owner-only disconnect cleanup (:187-198); agent DocumentEdit preempts (:786-791); owner's own DocumentEdit appends to its own open group (per ADR: same-client writes may merge). CONFORMS.

**F16 — Generation/ChangeEvent (task item 7)** — bump on Open/Save(ok)/edit/undo/redo/SetMode-actual-change/ExternalChange only; GetState/Move/Extend/Goto/Scroll/SetViewport do not bump (matches ADR-0012's enumerated list and interaction-only exclusion); ring bounded at 128; events carry post-op generation; snapshot mirrors generation/events; Hello handshake gates source tagging. CONFORMS (with F8 exception).

**F17 — tree.rs / multi-View (task item 6)** — `tree.rs` is the split-**View** tree (ViewId leaves), not a selection tree; selections live per-View (`editor.rs:86-96`). Multi-View is fully implemented in minae-view but the daemon/protocol expose only the focused View (snapshot has one selection, no view id). Consistent with ADR-0002's rationale and ADR-0006/0007's effectively-one-client v1 scope. Verdict: AMBIGUOUS vs CONTEXT's multi-View language (v1-scope gap, no protocol surface). Severity: MINOR (note).

## Test results

Not run — no shell tool available to this reviewer. Code-level inspection found the suites listed below present and coherent; the parent must execute:
- `cargo test -p minae-core -p minae-view` (task item 8; covers document/selection/transaction/edit/movement/search, history group semantics, editor dirty/split/scroll).
- `cargo test -p minae-term` (daemon unit + socket e2e: H1/H3/MEDIUM/SEC-1/ADR-0008/0011/0012 suites).
- `cargo test --workspace` (LSP tests need `target/debug/mock-server`; they self-skip when absent).

## Untested claims

- Issues #1-#4 (S0-S2) acceptance semantics: not verified — `gh issue view` unavailable to this reviewer; only code comments/ADRs (S2 = edit+save) were cross-checked.
- `find_next` (search.rs) is exported but has no caller/daemon wiring; wrap-around documented as caller responsibility — end-to-end behavior unexercised.
- `mark_saved_doc`'s evicted-doc branch (`None → true`, editor.rs:169-174) has no test.
- F8 (non-owner no-op Undo preempt) has no test.
- F1 (relative-path Open via `session exec`) has no test.
- F4 (stale diagnostics/disk_changed on reuse-Open) has no test.