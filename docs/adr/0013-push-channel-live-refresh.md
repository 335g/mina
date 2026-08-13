# Server-initiated push channel for live refresh (supersedes the "no push" part of 0006)

The TUI must react to other clients' changes while idle (an agent editing the same document). ADR-0006's request/response-only framing leaves the TUI stale until the user's next keypress — the screen shows old text and the user cannot watch the agent work. So the daemon gains a push channel: after any state-changing command it broadcasts the resulting StateSnapshot to subscribed (Interactive) clients, and the TUI redraws on arrival.

Mechanics:

- **Wire**: every daemon→client message is a tagged envelope, `{"type":"response"|"push","snapshot":{...}}` (NDJSON, one message per line). Responses used to be bare snapshots; the uniform envelope is deliberately breaking for CLI compatibility — one message shape keeps the spec and the reader simple. One-shot headless clients never receive pushes, so the `session` CLI still reads exactly one line per command.
- **Subscription**: only Interactive clients (the TUI) subscribe. Headless one-shot clients are never pushed to — a push would corrupt their "one command, one response" contract.
- **Trigger**: the daemon keeps a `tokio::sync::watch` channel holding the latest snapshot. On every command it sends only when `generation` advanced (watch's `send` wakes receivers on every send, not only on value change — so unchanged responses such as GetState, rejected edits, and no-ops must not be sent). The originator also receives its own pushes; the TUI drops pushes whose generation equals its last drawn state (self-edit is already drawn from the response).
- **Ordering/backpressure**: `watch` holds only the latest value, so slow subscribers miss intermediate generations but always converge to the newest — full snapshots make skipped pushes harmless. The connection handler `select!`s between the next command line and push notification; command lines are read via `Lines::next_line` (tokio documents it cancel-safe, unlike `read_line`).

Chosen over TUI-side polling: push is instant, costs nothing while idle, and the latency is not tied to an interval. Chosen over daemon-side originator exclusion: sending to everyone and deduping by generation client-side needs no connection registry and is only one extra snapshot write per state change.

Consequences: the TUI exits when the daemon dies even while idle (EOF on the push/response stream), instead of only on the next keypress. Auto-restart of a dead daemon is deferred. Pushes that change viewport state without advancing generation (multi-TUI SetViewport) are not redrawn — deduping by generation misses them; acceptable for the agent+one-TUI case.
