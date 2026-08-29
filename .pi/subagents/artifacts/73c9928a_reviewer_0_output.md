# Review — Slice C: ADR-0008 Open guards / ADR-0009 LSP isolation / ADR-0010 WorkspaceRoot sessions / issue #7 reopen / position conversion

I read in full: `minae-term/src/daemon.rs` (2426 lines), `minae-term/src/lsp.rs`, `minae-term/src/session.rs`, `minae-term/src/client.rs`, `minae-lsp/src/lib.rs`, `minae-lsp/src/position.rs`, `minae-lsp/src/bin/mock_server.rs`, `minae-lsp/tests/mock.rs`, `minae-protocol/src/lib.rs`, `minae-view/src/editor.rs` (relevant sections), plus ADRs 0008/0009/0010/0012 and CONTEXT.md. I have no shell tool, so `gh issue view` and `cargo test` could not be executed from this session; static verification only (commands listed at the end for the supervisor).

## Summary
Core mechanics of all four ADRs are implemented correctly and unusually defensively (metadata-before-read, fd-based TOCTOU re-check, lock timeouts, respawn-on-Open-only). I found one CRITICAL data-loss gap in the #7 fix (relative-path Open from the agent CLI bypasses document reuse and re-reads disk), one MAJOR unbounded-memory leak (LSP notification channel never drained in production), and several MINOR deviations.

## Findings

### F1 — CRITICAL (data loss) — #7 reuse is string-identity based; `minae session exec '{"Open": …}'` with a relative path re-reads disk and clobbers unsaved edits
- Spec: issue #7 (already-open path must reuse document + history, never re-read disk); ADR-0012 §"The daemon never auto-reloads — a document must never be clobbered from disk (the same principle as the already-open-path fix, #7)".
- Code evidence:
  - Reuse decision is exact `PathBuf` equality: `minae-view/src/editor.rs:141` — `self.paths.iter().find(|(_, p)| p.as_path() == path)`; called from `minae-term/src/daemon.rs:370`. On miss, the else branch does `read_open_target` + `open_with_path` (daemon.rs:407–425) — a fresh document, fresh history, disk content.
  - Path absolutization exists **only** in the TUI: `absolutize` is a private fn in `minae-term/src/client.rs:31`, used only at client.rs:91. The headless agent path `minae session exec <JSON>` (`minae-term/src/session.rs:56–70`) forwards the raw `Command::Open { path }` unchanged; the daemon never canonicalizes (`grep canonicalize` → no matches outside client.rs).
- Consequence: TUI opens `/abs/path/a.rs` (absolutized), agent edits it (unsaved, dirty), then an agent `Open` with `"a.rs"` or `"./a.rs"` misses the reuse lookup → daemon re-reads disk into a **new** document, replacing the visible state; the dirty doc is orphaned (not evicted only because dirty, but invisible). This is exactly the cross-client scenario #7 was filed for, still reachable through the documented agent interface.
- Verdict: DEVIATES (incomplete fix).
- Fix direction: normalize identity in the daemon — `std::fs::canonicalize` (fallback: lexical normalize) both the stored path (in `open_with_path`) and the incoming Open path before `focus_open_path`/map insert; or apply `absolutize` in `session.rs` `exec` as well. A daemon-side canonicalization covers all present and future clients.

### F2 — MAJOR — LSP notification channel is never drained in production → unbounded memory growth in a long-lived daemon
- Spec: ADR-0009 "diagnostic drains use try_lock so a busy server never blocks command processing"; flood protection intent (5c / `MAX_DIAGNOSTICS`).
- Code evidence: `minae-lsp/src/lib.rs:41` `notifications: mpsc::UnboundedReceiver`; :66 `mpsc::unbounded_channel()`; reader task :83–84 `tx.send(...)` — never blocks, never drops. `Client::try_recv` (lib.rs:181–182) is used **only** in `minae-lsp/tests/mock.rs:17`. The production drain `minae-term/src/lsp.rs:391–413` (`drain_into`) never calls `try_recv`; it only checks `is_dead()` / `current_uri`. Every `publishDiagnostics` (rust-analyzer publishes on each flycheck), `window/logMessage`, etc., queues forever until the session is replaced.
- Verdict: DEVIATES (the "drain" drains nothing; raw queue is uncapped, bypassing the flood-protection cap which only applies to pull conversion).
- Fix direction: in the reader task, if no one consumes notifications, drop them (e.g. don't send when the receiver is unread, or `try_send` into a bounded channel); push diagnostics are deliberately ignored anyway (lsp.rs:391 comment).

### F3 — MINOR — LSP initialize failure is swallowed (no snapshot status) on the reuse-Open path
- Spec: ADR-0009 "a broken server surfaces its `initialize` failure in the snapshot status."
- Code evidence: reuse branch `daemon.rs:380–384` — `match lsp::ensure(...).await { Ok(s) => Some(s), Err(_) => None }` discards the message; first-open branch daemon.rs:410–413 correctly puts it into `open_status`.
- Verdict: DEVIATES (partial — failure visible on first open, invisible on re-open).
- Fix direction: `Err(msg) => { open_status = Some(msg); None }` in the reuse branch too (the reuse branch currently has no `open_status` variable; plumb one through).

### F4 — MINOR — re-Open of the already-focused path bumps generation and logs an Open event although nothing changed
- Spec: ADR-0012 "bumped on every state-changing operation … never on pure reads"; the reuse code comment itself says "再利用 = 「何も変えない」".
- Code evidence: reuse branch always runs `d.record_event(source, EventKind::Open, None, None)` (daemon.rs:396–397) even when `focus_open_path` re-selected the identical doc (same id, same selection, same first_line).
- Verdict: AMBIGUOUS (ADR-0012 counts Open as state-changing; a client watching generations sees a bump with zero visible delta).
- Fix direction: capture the doc id before `focus_open_path` and skip `record_event` when unchanged; keep the event when focus actually moved.

### F5 — MINOR — disk baseline is not refreshed on reuse-Open, silently disabling external-change detection for the re-focused document
- Spec: ADR-0012 "the daemon detects out-of-band modification of the focused document … a background task stats the file" — focused-document monitoring is the stated coverage mechanism.
- Code evidence: reuse branch (daemon.rs:368–399) never re-stats/updates `disk_baseline`/`disk_changed` (first-open branch does, daemon.rs:427–436). `watch_disk` skips whenever `focused_path != baseline.path` (daemon.rs:243–247). Sequence: open A → open B (baseline=B) → reuse-Open A → baseline stays B → A's external changes are never detected (until A is Saved or re-opened as new).
- Verdict: DEVIATES (monitoring gap after reuse; no data loss — detection/reporting only — but the flag and ExternalChange event go silent).
- Fix direction: in the reuse branch, re-stat the reused path and reset `disk_baseline`/`disk_changed` exactly like the first-open branch.

### F6 — NOTE (acknowledged) — FIFO-swap TOCTOU between metadata and open can block
- Code evidence: `read_open_target` docstring, `daemon.rs:672–676` — "検証→open の間にパスが FIFO に差し替えられた場合は open でブロックし得る（単一ユーザ前提。O_NONBLOCK 化は必要になってから）". fstat-after-open catches the non-regular case only if `open` itself doesn't block first.
- Verdict: CONFORMS to the documented ponytail tradeoff; residual risk (a same-user attacker can stall one blocking-pool thread; tokio spawns 512 such threads, so DoS ceiling is bounded).

### F7 — NOTE (acknowledged) — pull-diagnostic "in-progress empty" wholesale-replaces diagnostics after every edit
- Code evidence: `pull_after_edit` (minae-term/src/lsp.rs:467–484) sleeps 250 ms then `d.diagnostics = diags` even when `diags` is empty; a slow analyzer returns "analysis-in-progress empty" and the file's errors vanish until the next edit's pull (ponytail comment lsp.rs:471–474). Not a spec violation; UX flicker.

### Verified CONFORMS (with evidence)
- ADR-0008: size+is_file checked before read (daemon.rs:686–694), re-verified by fstat on the same fd + bounded read with post-read size check (daemon.rs:699–716); non-UTF-8 → `cannot read` (daemon.rs:709–711, test `open_reports_non_utf8_as_cannot_read`); rejection reports via `status` only — `None => (String::new(), false)` and **no** `record_event` on the rejection path (daemon.rs:430–437), so no generation bump; command-line framing capped at `MAX_CMD_LINE` = 1 MiB with overlong-line disconnect (daemon.rs:351–359, test `oversized_command_line_closes_connection`).
- Issue #7: `focus_open_path` (editor.rs:137–148) only re-points the View's doc; documents/histories/dirty maps untouched; the only production `open_with_path` caller is the first-open branch (daemon.rs:425); e2e tests `reopen_already_open_path_preserves_unsaved_edits` and `reopen_rs_path_respawns_lsp_and_reannounces_current_text` (text + dirty + undo history + LSP re-announce with current buffer) pass by inspection.
- ADR-0009: sessions stored `HashMap<PathBuf, Arc<Mutex<LspSession>>>` (daemon.rs:83–85); `ensure`/`open_document`/`sync`/`pull_after_edit` all awaited after the daemon lock is dropped (daemon.rs:381–393, 447–466, 571–582); notify 2 s (lib.rs:148), request 10 s (lib.rs:117); `drain_into` uses `try_lock` (lsp.rs:406); lock acquisition bounded by `LSP_LOCK_TIMEOUT` = 3 s (lsp.rs:39–42); dead detection via channel close → `is_dead()` = `notifications.is_closed()` (lib.rs:186–188), edits stop syncing (lsp.rs:434–438), stale diagnostics cleared (lsp.rs:408–413), respawn only from the Open handler via `ensure` (no other callers), dead-session replacement with race re-check (lsp.rs:315–370).
- ADR-0010: `workspace_root` = nearest Cargo.toml → .git (file or dir) → parent (lsp.rs:54–72; 4 unit tests); map keyed per root → separate servers with `rootUri` at the root (lsp.rs:322, 346); sessions never removed; diagnostics limited to the focused doc (drain_into clears on uri mismatch / missing session / dead server).
- Position conversion: `utf8_col_to_char` floors to a char boundary (floor_char_boundary), `utf16_col_to_char` counts UTF-16 units (surrogate pairs = 2), both clamp at line end; `LineIndex` keeps line starts in char indices so the total is char-indexed; unit tests + `--cjk` mock e2e (mock.rs `cjk_publishes_utf16_character_offsets`, lsp.rs `pull_converts_cjk_utf16_positions`) cover the byte-vs-char trap.

## Test results
- Not run from this session: no shell tool available. Supervisor should run from the workspace root:
  1. `cargo test -p minae-lsp -p minae-term` — required by the task.
  2. `cargo build --workspace` first if the daemon LSP tests are to exercise the mock: `minae-term` LSP tests reference `target/debug/mock-server` (env `MINA_LSP_COMMAND`) and **skip silently** when it is missing (daemon.rs:1391–1393 etc.); `target/debug/mock-server` currently exists, so a stale binary is the only risk.
  3. Note the LSP e2e tests `pkill -f target/debug/mock-server` (daemon.rs `reopen_rs_path_respawns_lsp_and_reannounces_current_text`, `document_edit_syncs_lsp_full_text`) and serialize behind a test lock — do not run minae-term tests in parallel with other processes named `mock-server`.
- `gh issue view 5 7 8 9` also not run (no shell); issue bodies inferred from ADRs, commit history, and the repo's own regression tests.

## Untested claims
1. "Rejection touches no editor state": implemented (no `record_event`, no text/history change on the None path) and unit-tested only at the `read_open_target` level; **no daemon e2e asserts generation/text/history are unchanged after a rejected Open command** (`open_failure_reports_status` checks status+path only).
2. Device/FIFO rejection: only a directory is tested (`open_rejects_non_regular_file`); `/dev/zero` and FIFO paths are untested (behavior correct by inspection: `!is_file()` → "cannot open", size-0 devices pass the size check but fail is_file).
3. 3 s production session-lock bound: exercised only via the 200 ms test value (`open_document_gives_up_when_session_lock_is_held`).
4. Notification-channel boundedness (F2): no test asserts memory/queue bounds.
5. Cross-client reuse under mismatched path forms (F1): untested — and broken as shown.

## Review
- Correct: ADR-0008 guard mechanics (metadata-before-read, fd-based re-check, bounded read, status-only rejection, 1 MiB line cap), #7 reuse semantics at the Editor layer, ADR-0009 lock/timeout/respawn architecture, ADR-0010 root scoping, position conversion — all conform to spec and are well tested.
- Blocker: F1 — data-loss path remains through `minae session exec` Open with a relative (or otherwise non-identical) path; #7's guarantee holds only for byte-identical path strings.
- Fixed: none (review-only slice; fixes are suggestions in each finding).
- Note: F2 (MAJOR) and F3–F5 (MINOR) as above; F6/F7 are acknowledged tradeoffs.