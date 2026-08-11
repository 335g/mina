All verification complete. Here is the adversarial spec-completeness review.

---

# ANGLE A — Spec Completeness Review: Issue #1 vs. Code

Scope read in full: Issue #1 body + 5 progress comments, all 5 crates (`mina-core`, `mina-view`, `mina-protocol`, `mina-lsp`, `mina-term`), ADR-0004/0005/0006/0007/0008/0009, `Cargo.toml`, plus termina 0.3.3 `KeyCode` (to verify the Enter-key claim). 133 tests pass; clippy exit 0 with warnings.

## CONFIRMED-GOOD (spec points verified to hold)

| # | Issue requirement | Evidence |
|---|---|---|
| 1 | 5-crate structure | `Cargo.toml` members = core/view/protocol/lsp/term; `mina-term` has sole bin `mina` (`mina-term/Cargo.toml`) |
| 2 | mina-protocol: no deps, wire `Command`/`StateSnapshot`/`Diagnostic` (serde) | `mina-protocol/Cargo.toml` (serde only); `mina-protocol/src/lib.rs` — all 3 types + Mode/Movement/Range/Severity; round-trip tests |
| 3 | daemon owns all edit state; client stateless (ADR-0005) | `daemon.rs:39-57` `Daemon{ editor, viewport_height, lsp, diagnostics }`; `client.rs` holds no editor state |
| 4 | Unix socket + NDJSON, request/response only, full snapshot per response (ADR-0006) | `daemon.rs:99-106,160-162` — read 1 line → apply → write exactly 1 snapshot line; no push/subscription channel anywhere |
| 5 | Keymap: mode-specific prefix trie → Command, resolved client-side only | `keymap.rs` (3 tries, `gg`/`G` prefixes) is referenced only from `client.rs`; daemon never sees keys |
| 6 | Auto-start daemon, emacsclient-style, survives TUI exit | `client.rs:134-160` `ensure_daemon` — connect probe → spawn `daemon serve` with `libc::setsid` (`pre_exec`), 50×50ms poll; daemon persists after connection close (`accept_loop` continues, `on_client_disconnect` only resets mode/undo group) |
| 7 | LSP: built-in table, rust-analyzer only; spawn on first .rs Open, no eager | `lsp.rs:52-57` `server_for` maps only `"rs"`; `Daemon::new` has `lsp: None`; `daemon.rs:155-158` calls `ensure` only when `contents.is_some() && server_for(...).is_some()` |
| 8 | LSP pipeline: initialize → didOpen → didChange → publishDiagnostics → in snapshot | `lsp.rs` `new()/did_open/did_change/drain_diagnostics` + `drain_into` (daemon.rs:132); UTF-8/UTF-16 negotiation + conversion (`mina-lsp/src/position.rs`); 3 e2e tests vs mock server (`mina-lsp/tests/mock.rs`) |
| 9 | `mina session get/exec` headless CLI, shared command vocabulary | `session.rs` — `get`→GetState, `exec`→wire `Command` JSON; reuses `client::ensure_daemon`/`request`; parse errors → `Err` from `main` → exit 1 (matches issue comment); command failure → snapshot `status`, exit 0 |
| 10 | S0–S4 completion criteria | S0 roundtrip (`client.rs:39-46` first request); S1 browse/move (render.rs + keymap); S2 edit→save→undo incl. undo-group-per-Insert-session (ADR-0007, `daemon.rs:340-351`, `on_client_disconnect`); S3 diagnostics (underline + `[nE nW]`); S4 CLI |
| 11 | Scope-outs respected (nothing implemented that the issue deferred) | mouse: no `Event::Mouse` handling; diff/partial snapshots: none; LSP TOML config: none; non-UTF-8 I/O: `read_to_string` fails → `cannot open`; splits exist in mina-view but **no** protocol command wires them; no daemon stop command; no hover/goto/completion |
| 12 | `ponytail:` tradeoffs recorded as promised | `daemon.rs:81,113` (O(n) snapshot), `daemon.rs:299-302` (temp_dir/mina.sock), `client.rs:156-159` (1-client race), `render.rs:2-5` (full redraw) |
| 13 | termina 0.3 + tokio (ADR-0004) | workspace deps `termina = "0.3"` + `tokio` full feature set; `render.rs` owns escape-code rendering (no cell buffer, matching ADR-0004's stated intent) |

## FINDINGS

### F1 — MAJOR: dead LSP session is never replaced, contradicting ADR-0009 and the code's own M3 comment
- **File**: `mina-term/src/lsp.rs:236-269` (`ensure`)
- **Issue/ADR says**: ADR-0009 — "If the server dies … the session respawns on the next .rs Open (re-`initialize` + `didOpen` from scratch)". Code comment at lsp.rs:233-234 says the same ("M3: 既存セッションが死んでいたら新しいセッションで置き換える").
- **Code does**: `ensure` detects a dead session (lsp.rs:245-253), spawns a replacement (lsp.rs:258), but then lsp.rs:262-266 returns the *existing* session whenever `d.lsp` is `Some` — and `d.lsp` is only ever assigned at `Daemon::new` (`None`) and lsp.rs:268 (`Some`). After the first successful ensure, lsp.rs:267 is unreachable, so the replacement is **always** discarded and the dead session returned. Symptom: rust-analyzer dies → every subsequent .rs Open spawns a fresh rust-analyzer (leaked/orphaned child), returns the dead session, `did_open` fails silently (`let _ =`), diagnostics never recover until daemon restart. The `sync()` dead-check (lsp.rs:283-287) correctly stops syncing, so the failure is silent. No test covers this path (e2e tests drive `mina_lsp::Client` directly, never `ensure`).
- **Fix**: in the second lock, replace unconditionally when the existing session is dead: `d.lsp = Some(arc.clone()); return Ok(arc);` (only keep the "existing wins" branch for the genuinely concurrent-alive case).

### F2 — MINOR: TUI Insert mode cannot type Enter (or Tab) — newline insertion impossible from the terminal
- **File**: `mina-term/src/keymap.rs:305-315` (insert bindings) and `resolve_with_insert_fallback` (keymap.rs:333-348)
- **Issue says**: S2 "Insert モード入力" / S2 completion "編集→保存→undo が一通り動く" — plain text entry, which in any editor includes newlines.
- **Code does**: termina reports Enter as `KeyCode::Enter` (termina-0.3.3 `event.rs:298`, distinct from `Char`); the insert fallback only maps `KeyCode::Char` (keymap.rs:338-344), and no `Enter`/`Tab` binding exists in the insert trie. Result: `Resolution::NoMatch` — Enter/Tab do nothing in the TUI. Newlines are only insertable headlessly via `mina session exec '{"Insert":{"text":"\n"}}'` (daemon-side `Insert` handles any text — so the *protocol* is fine; only the keymap is blind).
- **Fix**: bind `Enter → Insert{text:"\n"}` and `Tab → Insert{text:"\t"}` in the Insert keymap (or extend the fallback to `KeyCode::Enter`/`Tab`).

### F3 — MINOR: S3 "行マーカー" is implemented as underline only — no line/gutter markers
- **File**: `mina-term/src/render.rs:145-153,226-233` vs issue S3: "publishDiagnostics → StateSnapshot に載せて表示（ステータス行カウント + 行マーカー）"
- **Code does**: diagnostics rendered as underline on the exact range (`\x1b[4m`, render.rs:167-171) + `[nE nW]` status count. There is no marker column/gutter; the diagnostic *message* is never shown anywhere. The author's own S3 progress comment frames the intended rendering as "診断範囲に下線 + ステータス行に [nE nW] カウント", so underline is a defensible reading of "行マーカー" — flagging the ambiguity since the issue text literally says line markers.
- **Fix**: none required if underline was the intent; otherwise add a gutter marker column when a diagnostic range intersects the line.

### F4 — MINOR: CLI arg parsing is lenient — advertised command surface not enforced
- **File**: `mina-term/src/main.rs:12-19`
- **Issue says**: "`mina daemon serve` の明示起動も可" (and the 3 modes: client/daemon/session).
- **Code does**: any first arg other than `daemon`/`session` is treated as a *file path* — so `mina --help`, `mina -h`, or a typo opens a file (or a confusing `cannot open` TUI); `mina daemon` (no `serve`) and even `mina daemon foo` start the daemon because `args[2]` is never validated; `session exec` ignores trailing args beyond the JSON. Harmless to the v1 flows, but the surface is sloppier than the spec advertises.
- **Fix**: validate `daemon serve` arity and add explicit `help`/`-h` handling before falling through to the file-path branch.

### F5 — NOTE: daemon accepts up to 4 concurrent connections while declaring "事実上 1 クライアント前提"
- **File**: `mina-term/src/daemon.rs:79-82` (`MAX_CONNECTIONS = 4`), `on_client_disconnect` (daemon.rs:57-65)
- **Issue says**: multi-TUI simultaneous connection is explicitly out of v1 scope; `ponytail:` records "事実上 1 クライアント前提".
- **Code does**: connections beyond 1 are actually *accepted* and interleave on one global viewport/undo-group/daemon state; any disconnect while another client is mid-Insert forcibly resets mode to Normal and closes that client's undo group (daemon.rs:106-108). This matches the declared 1-client assumption (documented), but the open door at 4 connections means the daemon half-implements a scope-out item with no per-client isolation — a latent footgun rather than a contradiction.
- **Fix**: none for v1 (matches issue); if ever tightened, drop to 1 concurrent connection or scope mode/undo groups per connection.

### F6 — NOTE: "clippy クリーン" (progress comments) is overstated — 9 style warnings
- **File**: workspace-wide; e.g. `mina-term/src/lsp.rs:143` (single-pattern match), `mina-term/src/client.rs:119` and `mina-term/src/render.rs:27` (counter loops), plus collapsible `if`s, a never-used `lsp_pos_to_char`, `.filter_map`→`.map`. `cargo clippy --workspace --all-targets` exits 0 but emits warnings.
- **Fix**: `cargo clippy --fix` or silence the specific lints if intentional (e.g., `#[allow]` on `lsp_pos_to_char` which is test-only).

### F7 — NOTE: dirty is not cleared by undo back to saved state (not specified in issue, recorded in code)
- **File**: `mina-view/src/editor.rs:139-145` — `ponytail:` comment admits undo-to-saved-point leaves `dirty=true` (no saved-point marker in history). Issue doesn't specify this semantics, so it's a documented simplification, not a contradiction — noting so it isn't mistaken for a regression later.

**No critical blockers.** Nothing *implemented* directly contradicts an issue statement; the one substantive deviation from declared behavior is F1 (contradicts ADR-0009, which the issue incorporates by reference: "設計判断の詳細は各 ADR を参照").

## Review
- **Correct**: 12/13 spec clusters verified as implemented exactly as specified (see CONFIRMED-GOOD table), including all four `ponytail:` tradeoffs and all nine scope-out items remaining unimplemented.
- **Blocker**: none.
- **Fixed**: none (review-only, no files modified).
- **Note**: F1–F7 above; of these only F1 (major) and F2 (minor) change observable behavior of v1 flows; F3–F7 are documentation/robustness gaps.