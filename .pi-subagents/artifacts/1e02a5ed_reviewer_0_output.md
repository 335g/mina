## Review

Verified every claim in Issue #1 against the workspace. Ran `cargo test --workspace`: **139 passed, 0 failed (10 suites)** — the issue's "118 at S4" figure is stale (more tests added since; e.g. SEC/H/M regression tests). No blockers found; one claim has a naming variance, several claims are only component-tested, not end-to-end through the socket.

### Per-claim findings

1. **S0 protocol crate — MATCH.** `mina-protocol/src/lib.rs:6-100`: `Command`/`StateSnapshot`/`Diagnostic`(+`Mode`/`Range`/`Severity`/`Movement`/`Direction`/`GotoTarget`) all serde-derive; round-trip tests at `lib.rs:128-159` (`state_snapshot_round_trip`, `command_round_trip`). Crate has no mina-internal deps (`mina-protocol/Cargo.toml`: serde only).

2. **S0 daemon/client skeleton — MATCH.** `mina-term/src/daemon.rs:99-107` UnixListener on `temp_dir()/mina.sock`; NDJSON framing at `daemon.rs:150-153` (read_line) and `client.rs:130-141` (write JSON line). Stale-socket + split-brain guard at `daemon.rs:92-106`. Auto-start with `setsid` at `client.rs:161-168` (`pre_exec(libc::setsid)`), 50×50ms socket-wait poll (`client.rs:147-172`, ponytail-commented). `Cargo.lock:346` termina **0.3.3**.

3. **S1 termina TUI — MATCH.** raw mode + alternate screen + cursor hide at `client.rs:61-68` (`enter_raw_mode`, `\x1b[?1049h`, `\x1b[?25l`), with drop-guard restore (`client.rs:20-43`); `EventStream::new` at `client.rs:79`; live state send/receive via `request()` + `render::draw` loop (`client.rs:88-122`).

4. **S1 keymap trie, client-side only — MATCH.** Per-mode prefix tries (`Keymaps { normal, insert, select }`, `keymap.rs:63-65`), resolution with pending prefix state at `keymap.rs:217-246`. `gg`/`G` at `keymap.rs:99-107`; hjkl/wb/arrows at `keymap.rs:85-98`; `v`→Select + `l`→Extend at `keymap.rs:118,132-138`; Ctrl-D/U → `Scroll{pages:±1}` at `keymap.rs:110-111`. Daemon holds no keymap — it only sees wire `Command`s.

5. **S1 scroll — MATCH.** `daemon.rs:300-302` `scroll_pages` → `mina-view/src/editor.rs:296-297` clamp; viewport-driven cursor-follow scroll `editor.rs:262-273`.

6. **S2 edit commands — MATCH.** `Insert/DeleteBackward/DeleteForward/DeleteRange/Undo/Redo/Save` in `mina-protocol/src/lib.rs:31-64`, handled in `daemon.rs:279-316`, bound at `keymap.rs:112-116,147-151` (x/Backspace/u/U/s; Select-x/Backspace → DeleteRange).

7. **S2 undo grouping daemon-side — MATCH.** `daemon.rs:320-329` begin_group on Enter-Insert / end_group on exit (any direction incl. →Select); disconnect close at `daemon.rs:75-82` + `on_client_disconnect`; verified by tests `insert_mode_typing_undoes_as_one_group`, `leaving_insert_for_select_closes_undo_group`, `disconnect_in_insert_mode_closes_group_and_resets_mode` (`daemon.rs:430-495`).

8. **S2 mina-view path/dirty — MATCH (naming variance).** `open_with_path`/`focused_path`/`is_dirty` at `mina-view/src/editor.rs:126,139,156-162`; the issue says "mark_saved", actual API is **`mark_saved_doc(doc_id)`** (`editor.rs:165-168`), called from `daemon.rs:210`. Functionality matches; name differs.

9. **S2 mina-core delete — MATCH.** `delete_forward_transaction`/`delete_backward_transaction` at `mina-core/src/edit.rs:38-44,67-73`, grapheme-boundary semantics; `Transaction::delete` at `transaction.rs:78`; `Transaction::insert` exists. Shift normalization at `keymap.rs:69-80` + test `uppercase_keys_normalize_shift`.

10. **S3 mina-lsp crate — MATCH.** spawn `mina-lsp/src/lib.rs:99-143`; Content-Length write/read `lib.rs:283-320` (with 64MB frame cap); request/response correlation via pending oneshot map `lib.rs:172-194`; UTF-8/UTF-16 ↔ char conversion `mina-lsp/src/position.rs` + negotiation at `mina-term/src/lsp.rs:73-92` (default UTF-16 if server silent).

11. **S3 daemon integration — MATCH.** Not eager: `Daemon::new` has `lsp: None` (`daemon.rs:59`); spawn+initialize only on first successful `.rs` open (`daemon.rs:160-165`, `server_for` gate at `lsp.rs:28-33`); didOpen at `daemon.rs:186-190`; full-text didChange per edit (`daemon.rs:226-241`, `is_edit` at `daemon.rs:257-266`, `sync`→`did_change` full-text `lsp.rs:106-115`); `drain_into` → `daemon.diagnostics` (`lsp.rs:314-334`) included in every `snapshot()` (`daemon.rs:345-346`).

12. **S3 rendering — MATCH.** Underlines via `\x1b[4m` `render.rs:160-176`; `[nE nW]` status counts `render.rs:235-244`; test `diagnostics_get_underlined_and_counted` passes.

13. **S3 both encodings — MATCH.** Both conversion paths implemented; utf-16 negotiated path exercised by mock-server `--cjk` (`mina-lsp/tests/mock.rs:96-106`, `mock_server.rs`).

14. **S4 session CLI — MATCH (execution-verified).** `session.rs:25-60`; shares infra: `session.rs:64-72` calls `client::ensure_daemon` + `client::request`. I ran the real binary: `mina session frobnicate` → exit **1**; `mina session exec 'not json'` → exit **1**; `mina session` → exit **1**; `mina session exec '{"GetState": null}'` → exit **0**, auto-started the daemon (setsid) and returned a full snapshot over the socket — proving the real S0 socket path end-to-end.

15. **Out-of-scope — all ABSENT.** No mouse (no tracking enable sequence anywhere in mina-term; `Event::Mouse` falls into `_ => continue` at `client.rs:113`); no split in protocol/daemon/keymap (`mina-view` split is pre-existing, which the issue itself acknowledges); full snapshot per response only (no diff/partial, `daemon.rs:133` ponytail); no LSP TOML config (`lsp.rs:28-33` built-in table only); UTF-8-only I/O with explicit rejection of non-UTF-8 (`daemon.rs:283-301` + test `open_reports_non_utf8_as_cannot_read`); no daemon stop subcommand (`main.rs:16-24`: daemon/session/path only); no hover/goto/completion (grep across mina-term/mina-lsp: zero hits).

16. **Trade-off `ponytail:` comments — MATCH (all four).** Full snapshot O(n)/key: `daemon.rs:133`; UTF-8-only I/O: `daemon.rs:277`; effectively-1-client: `daemon.rs:89` + `client.rs:152`; single-user socket `temp_dir/mina.sock`: `daemon.rs:383-384`. (Extras: full redraw `render.rs:4`, linear keymap scan `keymap.rs:6`.)

### Notes / residual risks

- **N1** — The daemon-level LSP pipeline (Open `.rs` → spawn → didOpen → diagnostics in snapshot) has **no automated end-to-end test through the socket**. It's covered at component level (`mina-lsp/tests/mock.rs` spawn/initialize/didOpen/didChange round-trips; `mina-term/src/lsp.rs:357-392` `ensure_replaces_dead_session`, which I confirmed runs, not skips). The full chain `Open → diagnostics in StateSnapshot` is untested as one path; needs rust-analyzer or a mock injected into the daemon's `server_for` gate. Residual risk only, feature is present in the real path (`daemon.rs:160-193`).
- **N2** — TUI is not exercised by any automated test (needs a real terminal); raw-mode/alternate-screen/EventStream path is build- and code-verified only.
- **N3** — `lsp_pos_to_char` (`mina-term/src/lsp.rs:225`) is test-only dead code → build warning; harmless.
- **N4** — Test count: issue claims 118 at S4; actual is **139, all passing** (protocol 2, core 53, view 30, lsp 6, term 48).
- **N5** — `cargo build` emits 1 warning (dead_code, N3); `cargo test --workspace` clean, 0 failures.