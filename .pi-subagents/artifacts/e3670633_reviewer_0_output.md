All verification complete (issue read, all sources read, greps run, 133 tests pass). Report follows.

---

# Angle D Review — Adversarial verification of Issue #1 vs code

## CONFIRMED-GOOD (spec points verified to hold)

| # | Issue statement | Evidence in code | Verdict |
|---|---|---|---|
| G1 | S0 骨格: protocol types + socket listen/connect + auto-start, GetState↔StateSnapshot round trip | `mina-protocol/src/lib.rs` (Command/StateSnapshot, NDJSON framing), `daemon.rs:130-150` serve/accept, `client.rs:166-196` ensure_daemon, `client.rs:98-114` request | ✅ implemented + tested |
| G2 | S1: termina rendering + per-mode prefix-trie keymap, client-side resolution only | `render.rs` (full-screen escape sequences), `keymap.rs` (3 mode tries; daemon receives only resolved `Command`) | ✅ |
| G3 | S2: insert/delete/undo/redo/save/quit + path/dirty in mina-view | `editor.rs:214-247` apply/undo/redo, `daemon.rs:151-177` Save, `editor.rs:105-112` is_dirty | ✅ |
| G4 | S3: built-in rust-analyzer table, spawn on first `.rs` Open (no eager), diagnostics in snapshot | `lsp.rs:36-42` server_for (rs only), `daemon.rs:136-152` spawn-on-Open, `lsp.rs:129-155` drain → snapshot.diagnostics, status-line count + underline in `render.rs:254-259, 177-195` | ✅ |
| G5 | S4: `mina session get/exec` | `session.rs` — get / exec JSON, exit 1 on transport/parse error | ✅ |
| G6 | Out-of-scope — **mouse**: absent | zero hits for mouse/マウス in all crates; `EventStream::new(…, \|_\| true)` in `client.rs:89` captures no mouse | ✅ truly absent |
| G7 | Out-of-scope — **multiple simultaneous TUI clients**: no per-client View ownership; mode + viewport are daemon-global; connection cap with effectively-1-client comment | `daemon.rs:77-80` (ponytail), `daemon.rs:107` MAX_CONNECTIONS=4, `client.rs:160-163` spawn race comment | ✅ |
| G8 | Out-of-scope — **diff snapshots / partial send**: every response is a full `StateSnapshot` | `daemon.rs:122-127` ponytail: full snapshot O(n)/command, "差分送信に差し替える"; ADR-0006 | ✅ |
| G9 | Out-of-scope — **LSP config TOML**: hardcoded table, no config loading | `lsp.rs:36-42` ("設定ファイル化はサーバが増えてから") | ✅ |
| G10 | Out-of-scope — **split views not wired**: mina-view split tree exists but unreachable from mina-term | `editor.rs:167-210` (split/focus/close) + `tree.rs` — **zero** callers in `mina-term/` (grep: only `stream.into_split()` matches); no `Command` variant, no keymap binding | ✅ deliberately unwired, exactly as the issue declares ("必要になったら単発コマンドで載せられる") |
| G11 | Out-of-scope — **daemon stop command**: none; daemon persists until killed | `main.rs:14-23` modes = daemon serve / session / TUI only; no shutdown IPC | ✅ (see F3 for a footgun) |
| G12 | Out-of-scope — **LSP hover/goto/completion**: only diagnostics | `mina-lsp/src/lib.rs` — initialize/notify/didOpen/didChange/publishDiagnostics only; no hover/gotoDefinition/completion | ✅ |
| G13 | Trade-off ponytail comments: **full snapshot per response** | `daemon.rs:125` | ✅ present |
| G14 | Trade-off ponytail comments: **1-client assumption** | `daemon.rs:77`, `lsp.rs:264` | ✅ present |
| G15 | Trade-off ponytail comments: **socket path single-user** | `daemon.rs:480-481` (temp_dir/mina.sock, uid なし) | ✅ present |
| G16 | ADR-0002: Selection lives outside Document, per-View | `mina-view/src/editor.rs:23-31` View holds selection; `ADP-0002` | ✅ |
| G17 | ADR-0004: termina 0.3 backend | `Cargo.toml:16` `termina = { version = "0.3", features = ["event-stream"] }` | ✅ |
| G18 | ADR-0005: daemon owns all state; auto-start (emacsclient style); `mina daemon serve` | `daemon.rs:101-110`, `client.rs:166-196` | ✅ |
| G19 | ADR-0006: request/response only, NDJSON, full snapshot, no push channel | `protocol/src/lib.rs:1-5`, `daemon.rs:126-238` (single response per command) | ✅ |
| G20 | ADR-0007: undo group closes on leaving Insert (→Normal **or** Select) and on client disconnect | `daemon.rs:396-406` (both transitions), `daemon.rs:90-98` on_client_disconnect; tests `leaving_insert_for_select_closes_undo_group`, `disconnect_in_insert_mode_closes_group_and_resets_mode` | ✅ |
| G21 | ADR-0008: reject non-regular & >16MiB, 1MiB cmd line | `daemon.rs:55-57, 262-281` + tests `open_rejects_oversized_file`, `open_rejects_non_regular_file`, `oversized_command_line_closes_connection` | ✅ |
| G22 | ADR-0009: LSP outside daemon lock; 2s write / 10s request timeouts; respawn on next .rs Open; try_lock drain; dead-server detection | `mina-lsp/src/lib.rs:130-132` (10s), `:164-173` (2s), `:227` is_dead; `mina-term/src/lsp.rs:217-271` ensure, `:296-311` drain_into try_lock | ✅ |
| G23 | Full test suite green | `cargo test --workspace` → 133 passed (10 suites) | ✅ |
| G24 | No scope creep found: every wire `Command`, keymap binding, and crate maps to S0–S4; extra mina-core APIs (search) predate the issue and are unwired | protocol enum + keymap bindings cross-checked | ✅ |

## FINDINGS

**F1 — MAJOR — `mina-term/src/daemon.rs:279` — non-UTF8 file open fails *silently*, breaking the wire contract.**
- Issue says: 非 UTF-8 は v1 スコープ外 → 対象外機能は明確に拒否されるべき（scope-out = refuse loudly）。Also `Command::Open`'s own doc (`mina-protocol/src/lib.rs:15`): "読み込み失敗は StateSnapshot.status に報告".
- Code does: `(tokio::fs::read_to_string(path).await.ok(), None)` — a UTF-8 decode error is swallowed: `contents=None, status=None`. The Open branch then snapshots with `status: None`, text `""`, path `None`. The client (TUI or agent) sees a successful open of an *empty* document, indistinguishable from opening an empty file. Agent-driven flow (the primary v1 use case) would then edit/save a phantom buffer. (No file overwrite: path is never registered, Save reports "no file name" — data loss avoided, but the misdirection stands.)
- Fix (one line): map the decode error to status — `Err(e) => (None, Some(format!("cannot read {path}: {e}")))` instead of `.ok()`.

**F2 — MINOR — `mina-term/src/daemon.rs:279` — required `ponytail:` comment for "UTF-8 のみのファイル I/O" is missing.**
- Issue says: 実装上の割り切り4点を「コードに `ponytail:` で記録する」。3/4 present (F: snapshot `daemon.rs:125`, 1-client `daemon.rs:77`, socket uid `daemon.rs:480`). The UTF-8-only I/O trade-off has **no** ponytail comment anywhere (grep: zero hits near the read path or `read_to_string`).
- Fix: add `// ponytail: UTF-8 のみ（read_to_string）。非 UTF-8 対応は v1 対象外` above `read_open_target`.

**F3 — MINOR — `mina-term/src/main.rs:17-18` — `mina daemon <anything>` silently starts serving.**
- Issue says: 明示的な daemon 停止コマンドは v1 対象外 (so no stop command is *correct*).
- Code does: `Some("daemon") => daemon::run()` ignores `args[2..]`, so `mina daemon stop`, `mina daemon --help`, or any typo launches a real daemon that binds `temp_dir/mina.sock` and lingers. A user who tries `mina daemon stop` gets a daemon *started* — the exact opposite of the intent.
- Fix: match `args.get(2)` == `"serve"` and error on anything else (`Unknown daemon subcommand`).

**F4 — NOTE — `mina-term/src/lsp.rs:262-266` — ensure() race leaves an orphaned spawned process.** Already self-documented ("プロセスは orphan") and within the declared 1-client assumption; no action needed, recorded for the multi-client future.

**F5 — NOTE — Hardening fixes (H1/H2/H3, M4/M5/M6/M7, SEC-1/SEC-2) live only in code comments, not ADRs.** Security-relevant ones are covered (SEC-1→ADR-0008, M1/M2/M3→ADR-0009); the rest are post-ADR bug fixes. Not scope creep (all are robustness of declared features), just doc drift — fold the significant ones into ADRs when touching them.

**F6 — NOTE — `mina-term/src/lsp.rs:38` — file:// URI percent-encoding missing** (documented ponytail: paths with spaces break LSP). Confirmed honest, out-of-issue scope.

---

## Residual risks
- **F1 (major, unfixed — review-only)**: silent empty-document open on non-UTF8 files; must be fixed before agents are pointed at arbitrary file trees.
- Auto-spawn race (`client.rs:160-163`): two simultaneous clients → one daemon exits with "another daemon is running"; transient, documented.
- `dirty` survives undo-to-saved-state (`editor.rs:114`, ponytail) — a false "dirty" indicator is harmless, a false "clean" would not be; acceptable as documented.
- Termina 0.3 pre-1.0 API instability (ADR-0004 notes crossterm fallback) — external dependency risk, unchanged.

## Acceptance report