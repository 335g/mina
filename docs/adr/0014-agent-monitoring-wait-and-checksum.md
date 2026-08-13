# Agent monitoring: blocking wait and checksum in snapshot

Agents monitor a document without polling and without reimplementing FNV-1a.

- **`Command::WaitFor { generation }`** (new wire command, read-only): the daemon blocks the request until the global `generation` exceeds the given value, then replies with the snapshot. It reuses the watch channel introduced in ADR-0013 — no polling, no traffic while idle. Returns immediately when the generation already passed. Allowed for headless clients (added to the #13 allowlist). No timeout: Ctrl-C on the CLI aborts; a daemon-side wait outliving a killed client is accepted (self-heals on the next state change, when the response write fails).
- **`StateSnapshot.checksum`** (new field): the FNV-1a 64 of the full document text, computed in the single `snapshot()` builder. An agent passes it straight into `DocumentEdit.checksum` — no reimplementation of the hash (previously each agent language had to duplicate `fnv1a64`).

Chosen over agent-side subscription to the ADR-0013 push channel: agents are one-shot CLI invocations, so a persistent push connection doesn't fit them; a blocking request keeps the one-command/one-response contract. Chosen over a CLI-side polling loop: the daemon already owns the change signal (watch), so the wait costs nothing while idle.

The monitoring loop an agent previously wrote as "poll `session get` every N seconds" becomes a single `mina session wait <last_generation>`.
