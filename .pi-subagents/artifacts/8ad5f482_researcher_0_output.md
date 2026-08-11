# Research: LSP transports, termina async, Helix event bridge

## Summary
Only gopls and pylsp natively support non-stdio LSP transports (TCP; gopls also unix sockets). rust-analyzer, official typescript-language-server, clangd, and pyright are stdio-only in their shipped binaries (pyright ships a `--socket` flag with inverted client-listens semantics). Termina 0.3 is synchronous poll/read at its core but ships an `event-stream` feature providing a futures `Stream` adapter that parks a helper thread on the blocking poll and wakes the async task. Helix consumes that stream directly in a `tokio::select!` — no `spawn_blocking` for terminal input.

## Findings

### 1. LSP server transports
- **rust-analyzer — stdio only.** The `lsp-server` subcommand accepts only `--version`/`--print-config-schema`; no `--socket`/`--port` flag. [flags.rs](https://github.com/rust-lang/rust-analyzer/blob/master/crates/rust-analyzer/src/cli/flags.rs) TCP support is an open feature request (#14473); `ra-multiplex` proxies stdio↔TCP as a workaround. [issue #14473](https://github.com/rust-lang/rust-analyzer/issues/14473), [ra-multiplex](https://github.com/pr2502/ra-multiplex). (The `lsp-server` crate has a TCP `socket_transport` helper, but the ra binary doesn't expose it.) [socket.rs](https://github.com/rust-lang/rust-analyzer/blob/master/lib/lsp-server/src/socket.rs)
- **typescript-language-server — stdio only (current official).** `cli.ts` is `.requiredOption('--stdio')` + `--log-level` only. [cli.ts](https://github.com/typescript-language-server/typescript-language-server/blob/master/src/cli.ts), [README](https://github.com/typescript-language-server/typescript-language-server). The `--stdio/--node-ipc/--socket <port>` set belongs to the older prabirshrestha fork. [fork README](https://github.com/prabirshrestha/typescript-language-server/blob/master/README.md)
- **clangd — stdio only.** `ClangdMain.cpp` selects transport by env var: `CLANGD_AS_XPC_SERVICE` → XPC (macOS, only if `CLANGD_BUILD_XPC`), else `newJSONTransport(stdin, llvm::outs(), …)`. No TCP/unix-socket flag. [ClangdMain.cpp](https://github.com/llvm/llvm-project/blob/main/clang-tools-extra/clangd/tool/ClangdMain.cpp), [design/code](https://clangd.llvm.org/design/code)
- **pyright — stdio primary.** Ships vscode-languageserver's `--socket <port>`, but semantics are inverted: the *client* binds, the server connects out (fails if nobody is listening). [issue #3932](https://github.com/microsoft/pyright/issues/3932)
- **pylsp — stdio + TCP + WebSocket.** `--tcp --host <addr> --port <port>` (server listens) and `--ws --port`. [pylsp(1)](https://manpages.ubuntu.com/manpages/jammy/man1/pylsp.1.html), [python-lsp-server](https://github.com/python-lsp/python-lsp-server)
- **gopls — stdio sidecar default; daemon mode supports TCP and unix sockets.** `gopls -listen=:37374` / editor sidecar `gopls -remote=:37374`; unix: `-listen="unix;/tmp/gopls-daemon-socket"` / `-remote="unix;/tmp/..."`. Editor still speaks stdio to the forwarder. [daemon.md](https://github.com/golang/tools/blob/master/gopls/doc/daemon.md)

### 2. Termina async (0.3.x)
Core API is synchronous: `PlatformTerminal::event_reader()` → blocking `EventReader::read`/`Terminal::read` (poll/read). [docs.rs](https://docs.rs/termina/latest/termina/), [event.rs](https://github.com/helix-editor/termina/blob/master/src/event/stream.rs). But 0.3 ships an `event-stream` feature (`dep:futures-core` only — no tokio/async-std dep) exposing `termina::EventStream`, a `futures_core::Stream` that spawns a helper thread doing the blocking poll and wakes the async task via `PlatformWaker` + an `mpsc` task channel. [Cargo.toml](https://github.com/helix-editor/termina/blob/master/Cargo.toml), [stream.rs](https://github.com/helix-editor/termina/blob/master/src/event/stream.rs), [docs.rs features](https://docs.rs/crate/termina/latest/features)

### 3. Helix bridge (helix-term/src/application.rs)
- `Application::event_stream()` (non-Windows) returns `termina::EventStream::new(terminal.event_reader(), filter)` — filter drops escape events except theme-mode CSIs. [application.rs](https://github.com/helix-editor/helix/blob/master/helix-term/src/application.rs)
- `main.rs`: `let mut events = app.event_stream(); app.run(&mut events).await?` on `#[tokio::main]`. [main.rs](https://github.com/helix-editor/helix/blob/master/helix-term/src/main.rs)
- `run()` → `event_loop_until_idle()`: `tokio::select!` over `input_stream.next()` (futures StreamExt), `self.signals.next()`, and job-callback channels. No `spawn_blocking` for terminal input — the blocking read happens on termina's internal helper thread; the tokio task only does a 0-timeout `poll_next`. [application.rs](https://github.com/helix-editor/helix/blob/master/helix-term/src/application.rs)
- Helix workspace pins `termina = "0.3"` (switched from crossterm in PR #13307). [Cargo.toml](https://github.com/helix-editor/helix/blob/master/Cargo.toml), [PR #13307](https://github.com/helix-editor/helix/pull/13307)

## Sources
- Kept: rust-analyzer flags.rs & issue #14473 (stdio-only proof); ts-ls cli.ts (stdio-only proof); clangd ClangdMain.cpp (transport selection source); pyright #3932 (--socket semantics); gopls daemon.md (TCP/unix); pylsp manpage; termina Cargo.toml/event.rs/stream.rs (feature + mechanism); helix application.rs/main.rs (bridge mechanism); helix Cargo.toml (termina 0.3 pin).
- Dropped: DeepWiki/elastic ts-ls page (aggregator, secondary); Qwen code docs (generic); Microsoft LSP issue #604 (client-side, not server flags); crossterm docs (termina is not crossterm; used only to confirm termina's design note).

## Gaps
- Did not verify ts-ls history for when `--socket` was removed (current master is authoritative: stdio only).
- pyright-langserver's exact current flag list wasn't confirmed from its source (entry-point file moved); issue #3932 + vscode-languageserver docs establish `--socket` exists with inverted semantics.
- clangd TCP patches exist in some distro forks (e.g. `clangd --socket`) but official LLVM builds are stdio/XPC only — not verified per-distro.