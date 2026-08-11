All evidence gathered. Here is the adversarial verification report.

---

# ANGLE B — Security/Robustness Adversary Review

**Target**: Issue #1 (mina-term v1 spec) vs. the implementation at `a5c8e0e`.
**Method**: Read Issue #1 + all ADRs (0001–0009), read every source file, ran the test suite, and live-probed the daemon over the socket with crafted commands (temp dir only; **no repo files modified**).

---

## CONFIRMED-GOOD (spec points verified to hold)

| # | Claim (issue/ADR) | Evidence |
|---|---|---|
| 1 | NDJSON oversized lines bounded | `daemon.rs:135` `.take(MAX_CMD_LINE+1)` + break at `daemon.rs:140`; test `oversized_command_line_closes_connection`; 1 MiB matches ADR-0008 |
| 2 | Invalid UTF-8 on wire can't OOM/misparse | `BufReader::read_line` returns `InvalidData` → `Err(_) => break` (`daemon.rs:142`) |
| 3 | `/dev/zero`, FIFO, directory rejected | `read_open_target` metadata check (`daemon.rs:278` `m.is_file() && m.len() <= MAX_FILE_SIZE`); probe: `/dev/zero` → `cannot open /dev/zero`, FIFO → `cannot open`; sparse-file oversize test exists |
| 4 | MAX_CONNECTIONS actually bounds accept | Semaphore `acquire_owned` before spawn (`daemon.rs:108–119`); when full, loop parks 1 connection and kernel backlog absorbs the rest. fd/task count is bounded. Probe after a task panic: daemon served subsequent requests (tokio Mutex doesn't poison; permit/fd released on unwind) |
| 5 | Scroll pages negative/oversized safe | `scroll_pages` → `saturating_mul` + `clamp(0, max)` (`editor.rs:343–344`); probe with `isize::MAX` — daemon alive, clamped |
| 6 | Degenerate selections not wire-reachable | Wire has no range/selection input; selection is daemon-internal and `move_selection`/`extend_selection` clamp at doc boundaries (`movement.rs`) |
| 7 | Daemon double-start: loser exits | Probe: 5 concurrent `daemon serve` → all 5 losers exit `another daemon is running`; stale socket probe-then-remove (`daemon.rs:86–99`) |
| 8 | LSP timeouts everywhere on the daemon side | `mina-lsp/src/lib.rs`: request 10s timeout, notify 2s write timeout, non-blocking `try_recv` drain; spawn+initialize outside daemon lock (ADR-0009 honored); respawn only on .rs Open |
| 9 | Terminal injection defense | `render.rs`: control chars → U+FFFD in `draw_line` and status (SEC-2); tests `control_chars_are_replaced_not_emitted`, `status_line_sanitizes_path_and_message` |
| 10 | Response framing safe | `serde_json::to_string` escapes `\n`/control chars → 1 NDJSON line per snapshot; probe responses parse cleanly |
| 11 | M4/M5/M6/M7 + ADR-0007 + H1/H3 | TerminalGuard restores on error; first-status retained; probe-before-remove; invalid command gets a status snapshot; undo group closes on leaving Insert and on disconnect (tests present) |
| 12 | Issue scope-outs honored | No mouse, no multi-TUI, no diff snapshots, no LSP config, no non-UTF-8 I/O, no split wire commands, no daemon stop command |
| 13 | `cargo test --workspace` | **133 passed** (10 suites) |

---

## FINDINGS

### F1 — Open TOCTOU / size-race: metadata check and read are separate path lookups — **major**
- **Location**: `mina-term/src/daemon.rs:276–283` (`read_open_target`), ADR-0008.
- **Issue says**: "The daemon checks metadata (is_file + size) before reading … an unbounded read is an OOM and hang vector."
- **Code does**: `tokio::fs::metadata(path)` then a **fresh** `tokio::fs::read_to_string(path)` — two independent `open()`s. Between them the path can be swapped (attacker) or the file can grow (a legitimately-appended log, no attacker needed) past 16 MiB. A swapped-in FIFO blocks `read_to_string` **forever** (connection task hangs holding a `MAX_CONNECTIONS` permit; 4 such hangs = permanent connection DoS); a swapped-in huge file is read fully (OOM). ADR-0008's guarantee only holds at check time.
- **Severity**: major (violates the ADR's stated defense even without an adversary).
- **Fix**: open once, `File::metadata()` (fstat) the open fd, then read that same fd with a byte cap (e.g., `AsyncReadExt::read` loop or `spawn_blocking`), never re-open by path. Same pattern for `Save` (write through a swapped symlink).

### F2 — Documents are never evicted: repeated `Open` grows daemon memory without bound — **major**
- **Location**: `mina-view/src/editor.rs:80–100` (`open`/`open_with_path` insert into `documents`/`histories`; nothing ever removes), `daemon.rs:170–173` (Open arm); protocol has no Close command.
- **Issue says**: v1 is "effectively 1 client"; nothing about document lifetime.
- **Code does**: every `Open` inserts a new `Document` + `History` (+ path) forever. A 16 MiB file opened 100 times ≈ 1.6 GB retained — even opening *distinct* files in one daemon session leaks them all. Memory-exhaustion DoS and a leak in normal use.
- **Severity**: major.
- **Fix**: bound the document set (evict non-focused documents on Open, or add a Close/Replace command, or cap count and evict oldest).

### F3 — Socket path predictable + no permission hardening: arbitrary file read/overwrite if connectable — **major** (documented premise, unmentioned consequence)
- **Location**: `daemon.rs:502` `socket_path()` = `temp_dir()/mina.sock`, no chmod/umask handling anywhere.
- **Issue says**: "socket パスは単一ユーザ前提（temp_dir/mina.sock）" — recorded as a simplification only.
- **Code does**: on Linux `temp_dir()` is `/tmp` (world-writable, predictable). Consequences not documented: (a) anyone who can connect (socket mode = `0777 & ~umask`; with `umask 000` → world-connectable) can use `Open`+`Insert`+`Save` for arbitrary file **read** (≤16 MiB) and arbitrary file **overwrite** as the daemon user — no auth, no provenance; (b) any local user can pre-plant a socket at `/tmp/mina.sock` so the victim's auto-starting client connects to the attacker's daemon (path disclosure via Open commands, fake "saved" status, crafted snapshots).
- **Severity**: major on multi-user Linux; minor on macOS (per-user temp dir). 
- **Fix**: include uid in the path (`mina-<uid>.sock`) or use `$XDG_RUNTIME_DIR`, and `chmod 0700` the socket; the issue's "single-user" note should mention the file-read/write consequence.

### F4 — 4 idle connections permanently block all clients (empirically confirmed) — **minor** (by-design, but DoS-able)
- **Location**: `daemon.rs:79,108–119` (`MAX_CONNECTIONS = 4`, no idle timeout — deliberate: "TUI は読書中もアイドルになるのが正常").
- **Issue says**: "上限を超えた接続はキューに残る（accept されない）" as fd/task protection.
- **Code does**: probe — 4 clients connect and send nothing → 5th client's `GetState` got **no response for 3s+**. The permit is held for the whole connection lifetime; a silent connection holds it forever (client dead but fd open is cleaned on EOF; a *live-but-silent* client is not). Same-user buggy process, or cross-user attacker (per F3), = permanent request DoS with no recovery until the idlers close.
- **Severity**: minor (single-user premise; 4 TUIs legitimately reading also exhaust it).
- **Fix**: if revisited, accept-and-drop when the pool is full instead of parking, or add an idle timeout to the *connection* while keeping the *TUI* from being killed by it.

### F5 — `SetViewport { height: usize::MAX }` → overflow panic (empirically confirmed) — **minor**
- **Location**: `mina-view/src/editor.rs:332` `cursor_line >= first + height` (and `first_line = cursor_line - height + 1`).
- **Issue says**: nothing (viewport height is trusted from clients).
- **Code does**: probe: Open 20-line file, viewport 5, move down (first_line > 0), then `SetViewport {height: 18446744073709551615}`, then any edit/move → **`thread 'tokio-rt-worker' panicked at editor.rs:332: attempt to add with overflow`** in the debug build; the connection task dies without a response (client sees EOF), `on_client_disconnect` is skipped (undo group can leak open if in Insert mode). Release build wraps (display glitch, no panic). Daemon survives either way (tokio Mutex not poisoned — probe confirmed).
- **Severity**: minor (no state corruption, single connection).
- **Fix**: clamp `height` at the wire boundary (e.g., `min(height, 100_000)`) in the `SetViewport` arm.

### F6 — `utf8_col_to_char` slices at an unvalidated byte offset → panic on mid-codepoint position — **minor**
- **Location**: `mina-lsp/src/position.rs:6` `line[..byte_col].chars().count()`; called from `drain_diagnostics` (`lsp.rs:126`) **while the daemon lock is held**.
- **Issue says**: LSP is rust-analyzer-only (trusted server).
- **Code does**: a diagnostic position whose UTF-8 byte column lands inside a multi-byte char panics the daemon task mid-request (str slicing always validates char boundaries, release included). rust-analyzer reports char-boundary offsets in practice, so this is defense-in-depth — but any future/off-spec server (or malformed input) turns into a hung client (no response) + skipped disconnect cleanup. `utf16_col_to_char` is safe (iterates chars).
- **Severity**: minor.
- **Fix**: `line.floor_char_boundary(byte_col as usize)` before slicing (stable since 1.79) or iterate chars like the UTF-16 path.

### F7 — Client: no timeouts, unbounded response read, TUI freezes on daemon hang — **minor**
- **Location**: `client.rs:149–163` (`request`: `read_line(&mut response)` unbounded; no timeout on write or read), `client.rs:172–199` (`ensure_daemon` poll).
- **Issue says**: no client-side timing requirements stated.
- **Code does**: (a) an unbounded response line (spoofed daemon per F3, or a bug) can OOM the client; (b) if the daemon's connection task hangs (F1 FIFO hang, F5/F6 panic paths where a *different* client owns the hang), the TUI blocks in `request()` inside the event loop — keys not processed, and in raw mode Ctrl-C doesn't raise SIGINT → frozen TUI, recoverable only by killing from another terminal. Session CLI hangs the same way.
- **Severity**: minor.
- **Fix**: cap response line size (`take(MAX_CMD_LINE+1)` like the daemon) and add a timeout around `request`.

### F8 — Double-start remove_file race can unlink a live socket — **minor**
- **Location**: `daemon.rs:94–99` (`serve`: probe fails → `remove_file(path)` → `bind`).
- **Issue says**: "connect プローブで判定し … スプリットブレイン防止".
- **Code does**: probe-then-remove has a window: if another daemon binds between our failed probe and our `remove_file`, we unlink its *live* socket and take over; the first daemon becomes an unreachable orphan (process leak, no state loss — both start empty). Common case verified good (probe ×5).
- **Severity**: minor.
- **Fix**: after `remove_file`, re-probe (connect) before binding, or `rename`-then-bind.

### F9 — Concurrent .rs Open leaks orphaned rust-analyzer processes; respawn churn unrate-limited — **minor**
- **Location**: `lsp.rs:236–266` (`ensure`; loser's session dropped at `lsp.rs:266`), `mina-lsp/src/lib.rs` — `Client` has `kill()` but **no `Drop` impl**; tokio `Child` doesn't kill on drop. `lsp.rs:46` drops the reader `JoinHandle`.
- **Issue says**: "No respawn loop is possible because respawn only happens on Open" (ADR-0009).
- **Code does**: two concurrent `.rs` Opens spawn two rust-analyzer; the loser's process is never killed (orphan, per the code's own `ponytail:` comment). A crash-looping server + agent/attacker looping `Open .rs` = unbounded spawn+initialize churn (no rate limit or cooldown; each attempt up to 10s).
- **Severity**: minor.
- **Fix**: `impl Drop for Client { fn drop(&mut self) { let _ = self.child.start_kill(); } }` and add a respawn cooldown/backoff.

### F10 — One Insert session's undo group is byte-unbounded — **minor**
- **Location**: `mina-view/src/history.rs:53–84` (`push`; `MAX_UNDO_GROUPS` caps group *count*, not bytes or changes per group).
- **Issue says**: "巨大挿入ループの悪用対策" (3a) — memory bounded.
- **Code does**: 1000 × 1 MiB inserts inside one Insert session = one group of 1000 changes, each retaining inserted text + inverse → ~1–2 GB retained, never evicted (cap only trims beyond 1000 *groups*). Document text itself also grows past the 16 MiB open cap via inserts, so snapshot responses grow unbounded (O(n²) total). Per-command 1 MiB line cap is the only bound.
- **Severity**: minor (inherent to an editor; diff snapshots are a v1 non-goal).
- **Fix**: cap group size in bytes or changes per group (evict oldest change within the group).

### F11 — Auto-spawned daemon inherits the client's stdin — **minor**
- **Location**: `client.rs:186` — `cmd.stdout(null).stderr(null)` but **stdin not redirected**.
- **Issue says**: nothing.
- **Code does**: the daemon inherits the TUI's terminal stdin fd; while the daemon lives, that tty never reaches EOF, so later shell jobs in that terminal that read stdin until EOF hang (classic daemon-tty bug). `setsid` detaches the controlling terminal but doesn't close the fd.
- **Severity**: minor.
- **Fix**: `cmd.stdin(Stdio::null())`.

---

## Residual risks (accepted by design, worth recording)

- Cross-user socket (F3): the issue's "single-user premise" is honored in code but its file-read/overwrite consequence is undocumented; revisit before any multi-user deployment (Linux `/tmp`).
- Full-snapshot O(n)/command and O(n)/key LSP sync: documented ponytail; caps at 16 MiB open but not on post-open growth (F10).
- No daemon shutdown command (per scope-out): daemon lifecycle is kill + stale-socket recovery.
- `cargo build` emits a `dead_code` warning (unused item in a non-test module) — cosmetic.

---