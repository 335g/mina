All source read and tests/clippy run. Here is the review.

## Review

**Scope checked**: mina-term/src/{daemon,client,lsp,render,session}.rs, mina-lsp/src/{lib,position}.rs, mina-protocol/src/lib.rs, mina-view/{editor,history,mode}.rs, ADR-0005…0009, Issue #1. `cargo test --workspace` → 139 passed. `cargo clippy --workspace --all-targets` → 0 errors, 9 warnings.

---

### Confirmed bugs

**HIGH-1 — Undo group / Insert-mode ownership is daemon-global, not connection-scoped** — `mina-term/src/daemon.rs:68-73` (`on_client_disconnect`), `daemon.rs:262` (runs on *every* connection teardown), `mina-view/src/history.rs:66-72` (`push` appends to the open group regardless of connection). Two distinct failures, both reachable in the documented v1 use case (agent `mina session exec` interleaved with a human TUI):
- (a) The TUI is in Insert mode; the agent's one-shot `session exec` connection ends → `on_client_disconnect` sees the *global* mode == Insert, calls `end_group()` + `set_mode(Normal)`. The TUI's group is closed mid-session and the daemon mode silently flips to Normal while the TUI's cached snapshot still says Insert. Subsequent keystrokes are applied ungrouped (each key = its own undo unit) and the client/daemon mode state diverges until the user presses a mode key.
- (b) While the TUI's Insert group is open (`grouping && group_open`), an agent's `Insert` command appends into the same undo unit (`history.rs:69-72`). One undo then reverts the human's typing *and* the agent's edit together.
The existing tests (`disconnect_in_insert_mode_closes_group_and_resets_mode`, `leaving_insert_for_select_closes_undo_group`) only exercise a single logical connection, so the cross-connection path is untested. Fix: track the connection that opened the group (`insert_owner: Option<ConnectionId>` in `Daemon`); only reset/close when that connection disconnects, and route non-owner edits to a separate group.

**MEDIUM-2 — No write timeout on snapshot responses → connection-slot DoS** — `daemon.rs:253-256`. A client that sends a command and never reads (SIGSTOP'd TUI, stalled agent — accidental, not malicious) fills the socket buffer; `write_half.write_all` parks the handler forever holding one of `MAX_CONNECTIONS = 4` permits (`daemon.rs:54-61`). Four such connections wedge the daemon: the 5th client's `connect()` succeeds (kernel backlog) but its `request()` (no client-side timeout, `client.rs:116-126`) hangs forever. Fix: bound the response write with a timeout and drop the connection on stall.

**MEDIUM-3 — Stale diagnostics persist after the LSP server dies** — `mina-term/src/lsp.rs` `drain_into` (~line 300): `try_recv()` on the closed channel returns `TryRecvError` → no update → `daemon.diagnostics` keeps the last pre-death set, now stale w.r.t. the doc (edits after death are never synced). ADR-0009 documents silent stop + respawn on next .rs Open, but the stale underlines/counts are an undocumented side effect. Fix: in `drain_into`, clear `daemon.diagnostics` when `client.is_dead()`.

**MEDIUM-4 — Hung-but-alive LSP stalls every edit by up to 2s and holds the LSP mutex** — `mina-lsp/src/lib.rs:160-175` (`notify` 2s timeout). A server that doesn't die but stops reading stdin: every keystroke's `didChange` parks 2s inside `sync` (outside the daemon lock, so other clients aren't blocked — per ADR-0009 that part is correct), but a concurrent `open_document` (`lsp.rs` `open_document`) awaits the same `LspSession` mutex with no timeout, so an Open can be delayed 2s+ too. Matches the ADR's stated bounds — flagging as a design risk, not a violation.

---

### Confirmed correct (with evidence)

- **1MiB command-line cap is enforced on the read path**: `daemon.rs:204-210` — `take(MAX_CMD_LINE+1)` + `line.len() > MAX_CMD_LINE` → connection closed before JSON parse; covered by `oversized_command_line_closes_connection`. Malformed JSON gets a status-carrying snapshot, no client hang (`daemon.rs:238-243`).
- **ADR-0008 Open checks are sound**: pre-check `metadata` (fast reject) + `fstat` on the *opened fd* + byte-capped read from the *same fd* (`daemon.rs:288-321`). The TOCTOU fix (3efbb0a) is correct: metadata and read can no longer traverse different paths; post-fstat growth is capped. Tests cover oversized (sparse), non-regular (dir), and non-UTF-8. Residual: a path swapped to a FIFO *before* `File::open` can block a blocking-pool thread (documented `ponytail` comment).
- **SetViewport clamp is sound** (`daemon.rs:46-50`, `475`): `usize::MAX` height can't overflow `first_line + height`; test present.
- **ADR-0007 owning-client disconnect is correct**: `on_client_disconnect` closes the group and resets Normal; `begin_group`/`end_group`/`group_open` bookkeeping in `history.rs:92-102` prevents cross-*session* merging even for the orphaned-group case.
- **LSP mid-request channel close**: reader task drop → `oneshot` senders dropped → immediate `Err("サーバが終了した")`, no hang (`mina-lsp/src/lib.rs:136-151`); `is_dead()` via `notifications.is_closed()` is sound; `try_lock` drains never block the daemon lock.
- **StateSnapshot completeness**: every handler path returns the full snapshot (text, selection, primary_index, mode, first_line, diagnostics, path, dirty, status); no partial/stale response path found. Full-text `didChange` on every edit (incl. Undo/Redo) is the documented O(n) trade-off.
- **ensure_daemon double-start converges**: bind → AddrInUse → connect-probe → stale-remove → rebind (`daemon.rs:96-110`); a losing daemon exits, both clients' polls converge on the winner. Orphan LSP-session race is documented (`ponytail:`).
- **Daemon lock is never held across LSP or file I/O** (`daemon.rs` Open/Save/`is_edit` paths) — verified.
- **Terminal injection sanitized** in both document and status rendering (`render.rs` `draw_line`/`sanitize_status_data` + tests).

---

### Theoretical risks / LOW / NIT

- **LOW-5 — Socket path predictability** (`daemon.rs:334-337`, documented `ponytail`): `temp_dir/mina.sock`, no uid, default umask. A local user can pre-squat the path → daemon's probe "succeeds" against the squatter → "another daemon is running" → permanent editor DoS (clients fail after the 2.5s poll). Permissive umask (000) would additionally let others connect and drive `Save` to arbitrary paths. Fix: `mina-<uid>.sock` + explicit `0600` (ADR-level acknowledged trade-off).
- **LOW-6 — FIFO swap can block a blocking-pool thread** (`read_open_target`; documented): `tokio::fs::File::open` on a FIFO with no writer blocks indefinitely. Not under the daemon lock, so only that thread is lost. Fix when multi-user concerns arrive: `O_NONBLOCK`.
- **LOW-7 — Cap off-by-one**: the trailing `\n` counts toward `MAX_CMD_LINE`, so a valid 1MiB−1-byte command + newline is rejected (conservative, harmless).
- **LOW-8 — Response expansion**: a 16MiB control-char-heavy file serializes to ~6× (~96MiB) JSON per response on both ends; bounded by `MAX_FILE_SIZE`, documented O(n).
- **NIT-9** — `Insert{text:""}` pushes a no-op transaction onto the undo stack (`daemon.rs:418-427`).
- **NIT-10** — `uri()` has no percent-encoding (documented `ponytail:`); paths with spaces/UTF-8 break LSP URIs.
- **NIT-11** — No `didClose` when switching .rs → non-LSP file (LSP retains the old doc; harmless in v1, protocol drift).
- **NIT-12** — clippy: 9 warnings (collapsible `if`s ×4, identical blocks in mock_server, unused `lsp_pos_to_char`, `if let` vs match, loop counter, `filter_map`→`map`). Cosmetic.

**No CRITICAL findings**: I found no crash, hang, or data-loss path reachable from the wire on a correctly-permissioned socket (JSON parsing is bounded by the line cap; all edits route through daemon-owned selections; the lock is never held across awaits; the disconnect path is idempotent).