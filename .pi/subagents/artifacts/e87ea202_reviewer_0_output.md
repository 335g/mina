Supervisor didn't reply within the window — I'll deliver the completed review with the pending commands flagged for the parent.

---

## Summary

Slice E (TUI/transport/keymap/render + ADR-0005/0006/0011 + issues #2/#3/#6) is largely conformant: the type-level Command/DocumentEdit split is real and structural, the transport is strict request/response NDJSON with Hello-first, and the snapshot/generation machinery is well-tested. I found **no CRITICAL or MAJOR violations**; the deviations are all MINOR edge cases concentrated in the daemon's event/generation recording (no-op deletes, rejected inserts) and a few render/UX gaps. I could not run `cargo`/`gh` (no shell tool) — see Test results.

## Findings

**1. Rejected Insert still records a ChangeEvent and bumps generation (ADR-0012 deviation).**
- Spec: `docs/adr/0012-state-awareness-generation-and-events.md` — "bumped on every state-changing operation… never on pure reads"; daemon.rs:28–33 "状態を変える操作だけをイベントとして記録する".
- Code: `minae-term/src/daemon.rs` — the event is computed *before* application and recorded unconditionally after it: event match at daemon.rs:521–530, `apply_from(&mut d, command, conn_id);` then `if let Some((kind, range, text)) = event { d.record_event(...) }` (daemon.rs:538–540). `apply_from`'s Insert branch rejects over-limit inserts with an early return that changes nothing (daemon.rs:809–811: `if doc_bytes + text.len() as u64 > MAX_FILE_SIZE { return snapshot(daemon, Some("file too large…".into())); }`). The rejection still bumps `generation` and pushes a bogus `Insert` event. Note the DocumentEdit path does this correctly (`if rejected.is_none()` at daemon.rs:594–602) — the asymmetry confirms the bug.
- Verdict: **DEVIATES** — Severity: **MINOR** (only on the 16 MiB rejection path; ring/gen lie to clients, and the snapshot built before `record_event` is discarded, so the false event is served to the client).
- Fix direction: gate recording on actual success, mirroring the DocumentEdit path (e.g., compare generation before/after apply or have `apply_from` signal success).

**2. No-op deletes at document boundaries mark the document dirty, bump generation, push a bogus Delete event, and create an empty undo entry.**
- Spec: `CONTEXT.md` **Dirty** — "edited since it was last saved"; ADR-0012 generation semantics ("state-changing operation" only).
- Code: `minae-core/src/edit.rs` — `delete_backward_at_start_is_noop` / `delete_forward_at_end_is_noop` (no-op transactions at positions 0/len). `minae-view/src/editor.rs:319–337` `Editor::apply` unconditionally does `self.dirty.insert(doc_id)` and pushes to history. `minae-term/src/daemon.rs` `apply_from` DeleteBackward/DeleteForward/DeleteRange always calls `editor.apply` (daemon.rs:815–850) and the event guard records `Delete` unconditionally (daemon.rs:524–529).
- Verdict: **DEVIATES** — Severity: **MINOR**. Very reachable: Backspace at document start (Normal or Insert) or `x` at EOF flips the "*" dirty marker on a clean file and pollutes the event ring + undo history. No data loss.
- Fix direction: detect no-op transactions (compare text before/after, or skip when `tx.operations()` contains only Retain) and skip apply + event recording.

**3. Render does not display `disk_changed` or `generation`.**
- Spec: task contract item 3 (render "displays dirty / status / disk_changed / generation"); ADR-0012 requires the flag in the snapshot but display is the only user defense since save-time conflict handling is deferred ("Save-time conflict handling … is deferred to a follow-up").
- Code: `minae-term/src/render.rs` `draw_status` (render.rs:216–268) renders mode, `[nE nW]`, path+dirty, row:col, pending, status — no reference to `disk_changed` or `generation` anywhere in render.rs. A file changed on disk while the user edits it is silently overwritten by Save with no in-editor indication (the data is in the snapshot, just never drawn).
- Verdict: **AMBIGUOUS** (ADR mandates the flag, not its display; task claims display) — Severity: **MINOR**.
- Fix direction: add a `[disk changed]` indicator (and optionally generation) to the status line.

**4. `SetViewport` updates height but does not re-scroll — cursor can be off-screen for the first frame after init/resize.**
- Code: daemon.rs `Command::SetViewport { height } => { daemon.viewport_height = height.min(MAX_VIEWPORT_HEIGHT); snapshot(daemon, None) }` (daemon.rs:908–912) — no `scroll_to_cursor`. Open scrolls with the daemon default height 24 (daemon.rs:427), and the client sends the real height only afterward (client.rs:110–119); on a terminal shorter than 24 rows the initial frame can render without the cursor cell. Same one-frame lag on `WindowResized` (client.rs:152–160).
- Verdict: **DEVIATES** from the intended "cursor-follow scroll via SetViewport(height)" — Severity: **MINOR** (self-heals on the first keypress).
- Fix direction: call `scroll_to_cursor` in the SetViewport branch.

**5. `session exec`'s Open path does not absolutize the path (TUI does).**
- Spec: client.rs:7–12 documents the convention "クライアントはパスを絶対化してから送る" (daemon cwd is fixed at spawn; ADR-0005).
- Code: client.rs `absolutize` is applied in the TUI (client.rs:91) but `session.rs` `execute`/`parse_command` pass `{"Open": {"path": "rel/file"}}` straight through (session.rs:30–70). An agent running from a different directory silently opens/edits/saves the file relative to the *daemon's* spawn cwd, not its own.
- Verdict: **DEVIATES** (internal convention, inconsistent between the two client surfaces) — Severity: **MINOR** (wrong-file save risk, agent-intent mismatch).
- Fix direction: absolutize in session's exec path, or document that agents must send absolute paths.

**6. Daemon death mid-session surfaces as a misleading "invalid response" error; no reconnect.**
- Code: client.rs `request()` — `reader.read_line` returns `Ok(0)` on EOF, the empty string fails serde, mapped to `InvalidData("不正な応答")` (client.rs:180–186). The TUI exits via `?` (TerminalGuard restores the terminal — good), with no `ensure_daemon` retry.
- Verdict: **AMBIGUOUS** (ADR-0005 doesn't require reconnect; exit is defensible) — Severity: **MINOR**.
- Fix direction: special-case EOF as "daemon exited", optionally retry auto-start.

**7. Insert-mode fallback rejects AltGr/Compose-modified printable characters.**
- Code: keymap.rs `resolve_with_insert_fallback` — `key.modifiers.is_empty() || key.modifiers == Modifiers::SHIFT` (keymap.rs:256–261). Chars typed with `CONTROL|ALT` (AltGr layouts, e.g. `@` on German keyboards) can never be inserted in Insert mode.
- Verdict: **AMBIGUOUS** (v1 limitation; spec silent) — Severity: **MINOR**.
- Fix direction: also accept `CONTROL|ALT` when the char is printable.

**8. `Goto` in Select mode collapses the selection to a point instead of extending it.**
- Code: keymap.rs binds `gg`/`G` in Select (keymap.rs:203–214); daemon `Goto` → `Selection::point(pos)` (daemon.rs:891–895). Differs from vim visual-mode convention where `gg`/`G` extend the selection.
- Verdict: **AMBIGUOUS** (spec: Command "reads and may change the Selection"; no extend-on-goto requirement) — Severity: **MINOR**.
- Fix direction: extend to boundary in Select mode, or document the collapse.

**9. Ctrl-C quits the whole TUI in every mode, including Insert.**
- Code: client.rs:142–147 — `quit = (ctrl-c in any mode) || (Normal && q)`. In vim, Ctrl-C in Insert returns to Normal; here it closes the editor. No data loss (daemon persists; reconnect via Open reuses the dirty doc — #7), but accidental Ctrl-C during typing exits the TUI and (via disconnect) closes the undo group.
- Verdict: **AMBIGUOUS** (no spec on quit keys) — Severity: **MINOR**.

**Conforming items verified (not exhaustive):**
- **ADR-0011 type-level split**: `DocumentEdit` appears only in `daemon.rs` (server) and `session.rs` (headless CLI); zero references in client.rs/keymap.rs/main.rs/render.rs (grep). The TUI's only request calls pass `&Command` (client.rs:91–113, 145, 157). Structural, not conventional.
- **Keymap**: trie per mode (keymap.rs:69–71); `g g` → Pending→Goto DocumentStart (test `prefix_g_resolves_after_second_key`); unknown keys → `NoMatch`, pending cleared (test `unmatched_resets_pending`); SHIFT normalization handles termina's `Char('G')+SHIFT` reporting (test `uppercase_keys_normalize_shift`); every resolution yields `Command`, never DocumentEdit. All 15 Command variants reachable — GetState/Open/SetViewport deliberately non-key (initial command / file arg / resize).
- **Transport/ADR-0006**: Hello sent first by TUI (client.rs:100), session (session.rs:56, 75), and minae-debug; daemon rejects non-Hello first messages (daemon.rs:340–352, test `hello_handshake_rejects_non_hello`). One command = one NDJSON line; exactly one full snapshot per command; no server-initiated writes (watch_disk only mutates state; LSP diagnostics ride the next snapshot).
- **DocumentEdit checksum semantics**: protocol doc (FNV-1a 64 of the full text as last read) == daemon comparison (daemon.rs:754–766) == minae-debug/session computation. minae-debug is HEAD (851524ea), computes checksum from a fresh GetState each time, and rejects are reported without state change (daemon.rs:594).
- **Snapshot fields**: text/selection/primary_index/mode/first_line/diagnostics/path/dirty/status/generation/events/disk_changed all populated (daemon.rs `snapshot`, ~915–936).
- **Defensive daemon**: 0600 socket + peer-uid check (MEDIUM-3), 16 MiB open/insert caps (ADR-0008/SEC-1), line-size cap, response-write timeout, silent-connection timeout, viewport/scroll clamps — all with tests.

## Test results

`cargo test -p minae-term` and `cargo check --workspace`: **not run — reviewer subagent has no shell tool** (constraint: "Do not use shell commands"). Supervised review of the in-tree tests: keymap.rs (11 tests), render.rs (13), client.rs (2), session.rs (4), daemon.rs (~35 unit + e2e socket tests incl. Hello/ADR-0012/ADR-0011/high-1), minae-debug (2), minae-protocol (4) — comprehensive and directly target the claimed behaviors; two LSP e2e tests self-skip unless `target/debug/mock-server` is prebuilt. **Supervisor must run both commands and also `gh issue view 2 3 6 7 8 9`** — issue bodies were not readable without `gh`.

## Untested claims

- Generation/event behavior on **rejected** Insert and **no-op** deletes is untested — the existing tests (`insert_over_max_file_size_is_rejected_without_changes`, `delete_backward_and_undo`) assert text/dirty/selection only, not `generation`/`events` (Findings 1–2).
- TUI end-to-end (raw terminal + key events) is not integration-tested; the TUI layer is only covered via unit tests of keymap/render.
- minae-debug compilation is unverified by me (depends on `cliclack 0.5` API); needs `cargo check`.
- "Every key resolves to a Command, never a DocumentEdit" is a type-level property, not an explicit test.
- First-frame cursor visibility after SetViewport (Finding 4) has no test.

---