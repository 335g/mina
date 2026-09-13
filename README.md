# mina

> A terminal editor for agents. A resident daemon owns the editor state; every frontend — the headless session CLI, an agent, or the optional TUI — is just a client.

**mina** = **min**imize cost for AI **a**gent — a cost-minimizing agent editor. By suffix: mina**e** = editor (the TUI), mina**d** = daemon, mina**s** = session (the headless CLI).

[English] · [日本語](./README.ja.md)

## Why mina

For agent-driven editing, mina's session contract measures better than generic file tooling. In a controlled A/B test on a realistic two-file feature task (adding a field across a ~600-line Rust crate, five edits, verified by `cargo check`, with an external file change injected mid-task):

| | Naive tools (full-file reads, unverified replaces) | minas session contract (range reads, verified applies) |
|---|---|---|
| Input tokens (median) | 72,851 | 43,088 (**−41%**) |
| Cost (median) | $0.0456 | $0.0258 (**−43%**) |
| Success | 4/5 | 5/5 |

The contract that produces this: numbered range reads instead of full-file reads, and content-addressed applies validated against the daemon's state (a stale edit is rejected with a specific reason instead of silently corrupting the file). The daemon also auto-reloads external changes, so agents never edit against a stale view.

Methodology, per-run data, and the full discussion: [docs/benchmarks/agent-editor-ab-results.md](docs/benchmarks/agent-editor-ab-results.md) (T11/M3). Harness: [`tools/ab/`](tools/ab/README.md).

## What is mina

mina is a terminal editor with a daemon/client split, built for agent-driven editing first and interactive use second.

A resident **Daemon** holds all editor state — open documents, undo histories, selections, LSP sessions. **Clients** (the headless `session` CLI, an agent, or the optional TUI) connect over a local socket, send commands, and render state snapshots. Clients can come and go; the daemon and its state remain. Because every frontend speaks the same protocol, the tools an agent uses in a script are the same tools the TUI uses interactively — and the TUI is optional.

## Capabilities

- **Editing core** (UI-agnostic): documents, selections, undo groups, Normal / Insert / Select modes, search, external-change reload
- **Language support** via LSP, per workspace root: diagnostics, inlay hints, definition peek, semantic rename and references
- **Language servers** (embedded and verified per ADR-0030): `rust-analyzer` for Rust, `typescript-language-server` for TypeScript — with tree-sitter syntax highlighting for the same two. Any other LSP can be wired up via your `languages.toml` (user-added servers are unvetted but negotiate standard features); see the config docs.
- **Headless agent interface**: `minas` — bounded numbered reads (`get --lines`), one-shot verified edits (`apply`), positional edits (`edit`), state-change waiting (`wait`), hints and peek without full text, semantic rename (`rename`), structure discovery (`outline`) and position resolution (`at`), hover type lookup (`hover`) and workspace symbol search (`symbol`), and a diagnostics settle+report shortcut for the edit→verify loop (`check` — replaces `wait` + `get` + JSON parsing)
- **Terminal UI**: the `minae` TUI (ratatui/crossterm, per-client views) — `minae [file ...]` to edit interactively
- **Configable**: `languages.toml` for language servers (daemon-side), `config.toml` and user colorschemes (client-side), built-in `minas skill` guides for agents

## Getting started

```console
# agent tooling only: daemon + session CLI + skill guides. No TUI.
$ cargo install minad minas

# headless, from a script or an agent
$ minas apply path/to/file.rs "old text" "new text"
```

The TUI is available as `minae [file ...]`; the agent interface is the `minad` daemon and the `minas` headless CLI.

Alternatively build from source with `cargo build --release`. Language servers (rust-analyzer, typescript-language-server, …) are not bundled — install them separately if you want LSP features; tree-sitter syntax highlighting works out of the box.

The daemon is started on demand by clients; run `minad serve` explicitly to keep a persistent session.

## For agents

```console
# bounded read: only the numbered lines you need
$ minas get --lines 1:40

# verified content-addressed edit (rejected with a reason if the text no longer matches)
$ minas apply src/lib.rs "let old = 1" "let old = 2"

# block until the state advances past a generation
$ minas wait 42

# LSP-backed rename across the workspace root
$ minas rename src/lib.rs "USD" "JPY"
```

Each command returns JSON. If an edit is rejected, re-read the affected range and retry — the rejection message says what no longer matches. `minas hints <path>` and `minas peek <path> <line>:<col>` fetch inlay hints and definition popups without the full text.

#### Agent skills

`minas skill` ships a load-on-demand skill shelf for agents: no arguments prints a thin index (one line per topic); `minas skill <topic>` prints just that topic's guide. The shelf is compiled into the binary, so it always matches the installed build.

```console
$ minas skill           # index: usage, read, search, wait, edit, check, rename, references, ...
$ minas skill read      # the read contract (session get --lines, numbered output)
```

The guides cover tool choice (read / apply / rename contracts) and error recovery. They are written for machine parsing: English text, exit 0 on success; an unknown topic exits 1 with the reason and the available topics. If an edit is rejected, `minas skill errors` is the first stop.

An agent that does not know minas exists will never run `minas skill` — the shelf is pull-only. `minas skill --md` prints a thin wrapper to hand it over: YAML frontmatter, the `usage` guide (when to use minas instead of `rg`/`sed`, and the rules that hold from the first command), and the build stamp.

```console
$ minas skill --md > ~/.claude/skills/minas/SKILL.md   # or where your runner reads skills
```

Paste it where your agent reads instructions — a skill directory (`~/.pi/agent/skills/minas/SKILL.md`, `~/.claude/skills/minas/SKILL.md`), or your repo's `AGENTS.md` / `CLAUDE.md`. It carries no copy of the index (only "run `minas skill`"), so it cannot go stale as topics are added; if `minas info`'s `cli_generation` differs from the stamp, regenerate it.

## Architecture

- **Daemon**: owns documents, undo histories, selections, LSP clients, and syntax highlighting. Serves clients over a local socket.
- **mina-text**: UI-agnostic editing core — documents, selections, transactions
- **mina-protocol**: wire types for daemon/client IPC
- **mina-lsp**: LSP client (spawn, JSON-RPC, position conversion)
- **mina-view** / **mina-loader**: UI-agnostic editor state; tree-sitter grammars and highlight queries
- **mina-conn**: client-side connection and request plumbing shared by the session CLI and the TUI
- **minad**: the daemon binary
- **minas**: the headless session CLI and skill guides
- **minae**: the TUI (ratatui/crossterm)

## Documentation

- [CONTEXT.md](CONTEXT.md) — canonical glossary for the domain model
- [docs/helix-architecture.md](docs/helix-architecture.md) — architecture and design notes
- [docs/benchmarks/](docs/benchmarks/) — published agent-editor A/B measurement results, test plans, and evaluation notes
- [docs/adr/](docs/adr/) — decision records
- [docs/spec/](docs/spec/) — wire protocol specification

mina is under active construction; expect rough edges.

## License

MIT — see [LICENSE](LICENSE).

Copyright (c) 2026 335g