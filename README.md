# mina

> An editor built for agents to drive. A resident daemon owns the editor state — documents, undo, LSP — and agents edit through a session contract (verified, content-addressed edits) instead of rewriting files; every frontend — the headless `minas` session CLI an agent calls, or the optional TUI a human uses — is just a client.

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
- **Language servers** (embedded and verified per ADR-0030): `rust-analyzer` for Rust, `typescript-language-server` for TypeScript — with tree-sitter syntax highlighting for the same two. Any other LSP can be wired up via your `languages.toml` (user-added servers are unvetted but negotiate standard features; see [A first session](#a-first-session) for the path).
- **Headless agent interface**: `minas` — bounded numbered reads (`get --lines`), one-shot verified edits (`apply`), positional edits (`edit`), state-change waiting (`wait`), hints and peek without full text, semantic rename (`rename`), structure discovery (`outline`) and position resolution (`at`), hover type lookup (`hover`) and workspace symbol search (`symbol`), and a diagnostics settle+report shortcut for the edit→verify loop (`check` — replaces `wait` + `get` + JSON parsing)
- **Terminal UI**: the `minae` TUI (ratatui/crossterm, per-client views) — `minae [file ...]` to edit interactively
- **Configurable**: `languages.toml` for language servers (daemon-side), `config.toml` and user colorschemes (client-side), built-in `minas skill` guides for agents

## Getting started

```console
# 1. daemon + session CLI. Agent tooling only; the TUI is optional and separate.
$ cargo install minad minas

# 2. language servers are not bundled — install the ones your languages need
$ rustup component add rust-analyzer
$ npm install -g typescript-language-server typescript

# 3. check what mina found: daemon generation + configured servers
$ minas info
```

The daemon is started on demand by the first client; run `minad serve` explicitly to keep one alive. Servers are spawned per workspace root, so `symbol` / `rename` / `check` start working as soon as the server is on `PATH`. Tree-sitter syntax highlighting works out of the box.

Alternatively build from source with `cargo build --release`; the TUI is `minae [file ...]`.

### Wire minas into your agent

`minas skill` is pull-only: an agent that has never heard of minas will never call it. Push the trigger once, where your runner reads instructions:

```console
$ minas skill --md > ~/.claude/skills/minas/SKILL.md   # also ~/.pi/agent/skills/minas/SKILL.md
$ minas skill --md >> AGENTS.md                         # or inline it in the repo's instructions
```

The wrapper carries no copy of the shelf — only "run `minas skill`" — so it does not go stale as topics are added. Regenerate it when `minas info`'s `cli_generation` differs from the stamp at the bottom of the file.

### A first session

```console
$ minas outline src/lib.rs              # structure, without the full text
$ minas read src/lib.rs --lines 40:60   # only the numbered lines you need
$ minas apply src/lib.rs "old" "new"    # verified edit; rejected with a reason when stale
$ minas check src/lib.rs                # LSP diagnostics — your build stays the final gate
```

Reading and editing need no configuration. Two optional files change behavior:

- `~/.config/minae/languages.toml` (`$XDG_CONFIG_HOME/minae/`), daemon-side: add or override language servers. Merged over the embedded defaults, and re-read when a server is spawned.
- `~/.config/minae/config.toml`, client-side (TUI only): `colorscheme` and `agent_command` (the agent the TUI launches from a review comment). User colorschemes are TOML files in `~/.config/minae/colorschemes/`.

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

An agent that does not know minas exists will never run `minas skill` — the shelf is pull-only. `minas skill --md` prints a thin wrapper to hand it over: YAML frontmatter, the `usage` guide (when to use minas instead of `rg`/`sed`, and the rules that hold from the first command), and the build stamp. Where to put it: [Wire minas into your agent](#wire-minas-into-your-agent).

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