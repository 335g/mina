All verification complete. Here's the full audit.

## Review

### 1. ADR-0004 — termina backend, cell buffer + frame diff
- **CONFORM (backend choice)**: `Cargo.toml:18` — `termina = { version = "0.3", features = ["event-stream"] }`; `Cargo.lock` resolves termina 0.3.3. No ratatui/crossterm anywhere in the workspace dependency graph (grep across all `Cargo.toml`s and `Cargo.lock`; matches exist only in `.pi-subagents/artifacts/` transcripts and docs, not in code or deps). `mina-term/src/client.rs:95-100` uses `PlatformTerminal` + `EventStream` directly.
- **DEVIATION (frame diff)**: The ADR claims "mina owns the cell buffer and frame diff itself, exactly as Helix does" — the code does **not** diff frames. `mina-term/src/render.rs:1-4` records the opposite: `ponytail: 毎フレーム全画面を再描画する（差分描画は巨大ファイルや flicker が問題になったら）` (full-screen redraw every frame, diff deferred). `render_text()` (render.rs:63-108) regenerates the complete escape stream per frame; there is no cell buffer and no comparison against the previous frame. Severity: **minor** — functional (139 tests pass, screen renders correctly), but the ADR documents behavior the code deliberately defers.

### 2. ADR-0005 — daemon owns state
- **CONFORM**: `mina-term/src/daemon.rs:46-60` — `Daemon` holds `editor: Editor` (mina-view), which owns documents, per-document histories, paths, dirty set, views, tree, mode (`mina-view/src/editor.rs:37-50`). Client holds no editor state: `mina-term/src/client.rs:1-8` (only pending key events, snapshot, terminal dims). `session.rs` (agent CLI) is equally stateless.

### 3. ADR-0006 — request/response only, full StateSnapshot, NDJSON
- **CONFORM**: `mina-term/src/daemon.rs:135-259` — one loop: read one NDJSON line → apply → write exactly one snapshot line. The only daemon socket write is the command response (daemon.rs:256). No push task, no notification loop, no streaming channel. LSP diagnostics enter the snapshot only via `drain_into` during command processing (mina-term/src/lsp.rs:280-297), per ADR-0006's "carried in the next snapshot". `mina-protocol/src/lib.rs:1-3` — serde JSON over NDJSON, no binary serialization; `StateSnapshot` carries the full text (lib.rs:117-135).

### 4. Keymap — per-Mode prefix trie, client-side only
- **CONFORM**: `mina-term/src/keymap.rs:33-36` — `Keymaps { normal, insert, select }` tries; `resolve()` walks the prefix trie with `pending` (keymap.rs:230-256). `gg` → `Goto DocumentStart` and `G` → `Goto DocumentEnd` resolved client-side (keymap.rs:178-187; Shift normalization at keymap.rs:107-118). The daemon has no keymap — grep confirms no `keymap` reference in `daemon.rs`/`lsp.rs`/`session.rs`; daemon only consumes resolved `Command` values.

### 5. Daemon lifecycle — auto-start + explicit serve
- **CONFORM**: `mina-term/src/main.rs:14-22` — `mina daemon serve` → `daemon::run()`; bare `mina [file]`/`mina` → TUI client. Auto-start: `client.rs:177-208` `ensure_daemon` — connect probe → spawn current exe with `["daemon","serve"]` under `setsid()` → poll 50×50ms. Races handled: `daemon.rs:75-93` `serve()` — AddrInUse → connect probe → stale socket removed → rebind; concurrent double-spawn documented as "片方の daemon が bind に失敗して終了する" (client.rs:186-188). "Raceless-ish": the loser exits, the winner serves; residual race only in the stale-removal path, acceptable under the single-user premise.

### 6. LSP — rust-analyzer only, spawn on first .rs Open
- **CONFORM**: `mina-term/src/lsp.rs:33-37` — embedded `server_for()` maps only extension `rs` → `"rust-analyzer"`; no other server is spawnable. `Daemon::new()` sets `lsp: None` (daemon.rs:65-70); `serve()` spawns nothing. `ensure()` is invoked only from the Open handler when `server_for` is Some (daemon.rs:149-155). No eager spawn at boot.

### 7. ADR-0007 — undo groups
- **CONFORM**: `daemon.rs:441-457` (`SetMode`) — entering Insert (from non-Insert) → `begin_group()`; leaving Insert to Normal **or** Select → `end_group()`. Disconnect path: `daemon.rs:76-82` `on_client_disconnect()` — if mode is Insert, `end_group()` + `set_mode(Normal)`; called at connection teardown (daemon.rs:262). History enforces fresh-group semantics (mina-view/src/history.rs:39-47, 96-110). Covered by tests: `leaving_insert_for_select_closes_undo_group`, `disconnect_in_insert_mode_closes_group_and_resets_mode`, `new_begin_group_starts_fresh_group`.

### 8. ADR-0008 — Open rejects non-regular/oversized, 1MiB command line
- **CONFORM**: `MAX_FILE_SIZE = 16MiB` (daemon.rs:23-29), `MAX_CMD_LINE = 1MiB` (daemon.rs:32-34). `read_open_target` (daemon.rs:274-313): metadata is_file + size pre-check, fstat re-check on the opened fd (TOCTOU), capped read; rejection returns status (`file too large` / `cannot open` / `cannot read`) with `contents = None` → editor state untouched (daemon.rs:160-165). Command lines: `.take(MAX_CMD_LINE+1)` then close on overflow (daemon.rs:139-145). Tests: `open_rejects_oversized_file`, `open_rejects_non_regular_file`, `open_reports_non_utf8_as_cannot_read`, `oversized_command_line_closes_connection`.

### 9. ADR-0009 — LSP outside daemon lock
- **CONFORM**: `LspSession` behind its own `Arc<Mutex<>>` (daemon.rs:56-58). All LSP I/O awaited outside the daemon lock: `ensure()` before lock (daemon.rs:150-155), `open_document` after `drop(d)` (daemon.rs:165-168), `sync` after `drop(d)` (daemon.rs:199-208). Timeouts: 2s write in `notify()` (mina-lsp/src/lib.rs:186-197), 10s request in `request()` (lib.rs:156-181). Diagnostic drains use `try_lock` (mina-term/src/lsp.rs:290). Dead-server detection via channel close: reader task breaks on EOF → tx dropped → `is_dead()` = `notifications.is_closed()` (lib.rs:204-208, 121-145). Respawn only on next .rs Open — `ensure()` is called only from the Open path; `sync()` early-returns on dead (lsp.rs:262-273); test `ensure_replaces_dead_session`. **ADR-0009 doc matches code**: I verified paragraph-by-paragraph against commit 05fe623's claims (outside-lock awaits, 2s/10s timeouts, try_lock drains, EOF channel detection, Open-only respawn, initialize failure surfaced in status) — all present.

### 10. Newer commits vs v1 scope + process staleness
- **CONFORM (no scope contradiction)**: `f70f4bd` (Insert Enter→`\n`, Tab→`\t`) is additive Insert-mode keymap binding within S2's editing scope (keymap.rs:206-210). `c15642d` (doc cap 8 + evict, mina-view/src/editor.rs:31-36, 96-110) and `0306a07` (undo cap 1000, mina-view/src/history.rs:24-28) are memory-bounding safety valves — additive, consistent with the issue's `ponytail:` 割り切り pattern, and they don't contradict any listed v1 non-goal (undo group semantics per ADR-0007 are unchanged; eviction never drops dirty docs).
- **STALE (process)**: Issue #1 is **OPEN** with label `needs-triage` (maintainer must evaluate), yet the issue thread contains the **"v1 完了"** comment (S0–S4 all complete) and all slices are checked `[x]`. The issue should be closed or relabeled (e.g. remove `needs-triage`).

## ADR-vs-code inconsistencies (summary)
1. **ADR-0004**: documents cell-buffer + frame-diff; code does full redraw per frame (render.rs:1-4, 63-108) — the one doc-vs-code mismatch. Minor; the code records it as a deliberate `ponytail:` deferral. Either amend the ADR or implement diffing.
2. ADR-0006's "no push" and "diagnostics in next snapshot" match the code exactly (drain-only intake, lsp.rs:280-297).

## Commands
- `cargo test --workspace` → **passed**: 139 passed (10 suites, 0.39s).
- `gh issue view 1 [--comments/--json]` → confirms OPEN + `needs-triage` + "v1 完了" comment.
- `git log`/`git show` for f70f4bd/c15642d/0306a07 → verified contents.

**No blockers.** One minor doc-vs-code deviation (ADR-0004 frame diff) and one process-staleness item (issue #1 state).