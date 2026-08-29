# minae

> A terminal editor for agents. A resident daemon owns the editor state; every frontend — the headless session CLI, an agent, or the optional TUI — is just a client.

**minae** = **min**(imize) + **A**gent + **E**ditor — a cost-minimizing agent editor.

[English] · [日本語](./README.ja.md)

## Why minae

For agent-driven editing, minae's session contract measures better than generic file tooling. In a controlled A/B test on a realistic two-file feature task (adding a field across a ~600-line Rust crate, five edits, verified by `cargo check`, with an external file change injected mid-task):

| | Naive tools (full-file reads, unverified replaces) | minae session contract (range reads, verified applies) |
|---|---|---|
| Input tokens (median) | 72,851 | 43,088 (**−41%**) |
| Cost (median) | $0.0456 | $0.0258 (**−43%**) |
| Success | 4/5 | 5/5 |

The contract that produces this: numbered range reads instead of full-file reads, and content-addressed applies validated against the daemon's state (a stale edit is rejected with a specific reason instead of silently corrupting the file). The daemon also auto-reloads external changes, so agents never edit against a stale view.

Methodology, per-run data, and the full discussion: [docs/benchmarks/agent-editor-ab-results.md](docs/benchmarks/agent-editor-ab-results.md) (T11/M3). Harness: [`tools/ab/`](tools/ab/README.md).

## What is minae

minae is a terminal editor with a daemon/client split, built for agent-driven editing first and interactive use second.

A resident **Daemon** holds all editor state — open documents, undo histories, selections, LSP sessions. **Clients** (the headless `session` CLI, an agent, or the optional TUI) connect over a local socket, send commands, and render state snapshots. Clients can come and go; the daemon and its state remain. Because every frontend speaks the same protocol, the tools an agent uses in a script are the same tools the TUI uses interactively — and the TUI is optional.

## Capabilities

- **Editing core** (UI-agnostic): documents, selections, undo groups, Normal / Insert / Select modes, search, external-change reload
- **Language support** via LSP, per workspace root: diagnostics, inlay hints, definition peek, semantic rename and references
- **Headless agent interface**: `minae session` — bounded numbered reads (`get --lines`), one-shot verified edits (`apply`), positional edits (`edit`), state-change waiting (`wait`), hints and peek without full text, semantic rename (`rename`)
- **Terminal UI** (optional): `minae open`
- **Configable**: `languages.toml` for language servers (daemon-side), `config.toml` and user colorschemes (client-side), built-in `minae skill` guides for agents

## Getting started

```console
# the published crate (builds the `minae` binary; daemon, TUI, and session CLI in one)
$ cargo install minae

# start editing a file in the TUI (the daemon auto-starts)
$ minae open path/to/file.rs

# headless, from a script or an agent
$ minae session apply path/to/file.rs "old text" "new text"
```

Alternatively build from source with `cargo build --release`. Language servers (rust-analyzer, typescript-language-server, …) are not bundled — install them separately if you want LSP features; tree-sitter syntax highlighting works out of the box.

The daemon is started on demand by clients; run `minae daemon serve` explicitly to keep a persistent session.

## For agents

```console
# bounded read: only the numbered lines you need
$ minae session get --lines 1:40

# verified content-addressed edit (rejected with a reason if the text no longer matches)
$ minae session apply src/lib.rs "let old = 1" "let old = 2"

# block until the state advances past a generation
$ minae session wait 42

# LSP-backed rename across the workspace root
$ minae session rename src/lib.rs "USD" "JPY"
```

Each command returns JSON. If an edit is rejected, re-read the affected range and retry — the rejection message says what no longer matches. `minae session hints <path>` and `minae session peek <path> <line>:<col>` fetch inlay hints and definition popups without the full text.

#### Agent skills

`minae skill` ships a load-on-demand skill shelf for agents: no arguments prints a thin index (one line per topic); `minae skill <topic>` prints just that topic's guide.

```console
$ minae skill           # index: read, edit, rename, persist, errors
$ minae skill read      # the read contract (session get --lines, numbered output)
```

The guides cover tool choice (read / apply / rename contracts) and error recovery. They are written for machine parsing: English text, exit 0 on success; an unknown topic exits 1 with the reason and the available topics. If an edit is rejected, `minae skill errors` is the first stop.

## Architecture

- **Daemon**: owns documents, undo histories, selections, LSP clients, and syntax highlighting. Serves clients over a local socket.
- **minae-core**: UI-agnostic editing core — documents, selections, transactions
- **minae-protocol**: wire types for daemon/client IPC
- **minae-lsp**: LSP client (spawn, JSON-RPC, position conversion)
- **minae-view** / **minae-loader**: UI-agnostic editor state; tree-sitter grammars and highlight queries
- **minae-term**: the binary crate — CLI, daemon, TUI, and the headless session interface

## Documentation

- [CONTEXT.md](CONTEXT.md) — canonical glossary for the domain model
- [docs/helix-architecture.md](docs/helix-architecture.md) — architecture and design notes
- [docs/benchmarks/](docs/benchmarks/) — published agent-editor A/B measurement results, test plans, and evaluation notes
- [docs/adr/](docs/adr/) — decision records
- [docs/spec/](docs/spec/) — wire protocol specification

minae is under active construction; expect rough edges.

## License

MIT — see [LICENSE](LICENSE).

Copyright (c) 2026 335g