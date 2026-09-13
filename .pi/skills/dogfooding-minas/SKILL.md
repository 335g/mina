---
name: dogfooding-minas
description: Run the minas dogfooding loop. One pane (the driver) builds a real app in another repo with minas while a second pane (the implementer) triages the driver's feedback into minas/minad. Use when the user wants to dogfood minas against a real codebase, sets up a driver/implementer pane pair, or a peer pane starts sending minas feedback.
---

Two panes, two roles, one loop: the **driver** uses minas to build something real in a *different* repo; the **implementer** (this pane, the mina repo) turns the driver's reports into minas/minad changes and answers the rest. The driver is the customer; the implementer ships.

The loop only earns its cost if the driver actually reaches for minas every time instead of falling back to `rg`/`sed`/herdr-free editing — a driver that stops using the tool stops producing feedback.

## Setup (implementer)

1. Confirm the pair exists and is connected:
   ```bash
   test "${HERDR_ENV:-}" = 1 && herdr pane list --workspace "$HERDR_WORKSPACE_ID"
   intercom list
   ```
2. Name both sides once (the driver pane keeps its own agent name):
   ```bash
   herdr pane rename <driver_pane> <driver> ; herdr agent rename <driver_pane> <driver>
   herdr pane rename --current <implementer> ; herdr agent rename --current <implementer>
   ```
   Missing a driver pane? `herdr pane split --current --direction right --cwd <driver_repo>` then `herdr agent start <driver> --kind pi --pane <new_pane>`; read the new IDs from the command output.
3. Send the driver brief (below) with `intercom send`, then verify two-way traffic with one `intercom ask` ("reply with pong").
4. Record the baseline the driver will quote: `minas info` → `daemon_build_ts`, and the running `PROTOCOL_VERSION` (socket `minae-<version>.sock`).

## Driver brief (send verbatim, then fill the goal)

The driver runs in another repo and cannot read this skill, so the brief is self-contained.

```
You are the DRIVER in a minas dogfooding loop. Build <goal> in <repo> using minas
as your primary tool, and send feedback to <implementer> over intercom as you go.

Use these instead of shell tools — that is the experiment:
- read before edit: `minas read <path> --lines a:b`, `minas outline`, `minas at`,
  `minas search` (literal, `-i`, `-w`), `minas symbol`, `minas hover`
- edit: `minas apply` (content-addressed; `--pair old.txt new.txt` for batches,
  `--whole-stdin` for new files) — never compute positions, never `edit` without
  expected_text
- refactor: `minas rename` / `minas references`
- verify: `minas check --crate-root src/lib.rs --include-tests --summary`; the real
  build/clippy stays the final gate (`minas check` is an error-detector, not a
  warning detector)
- wait/sync: `minas wait --brief <generation>`

Report each finding with: exact command, output, exit code, and what you expected.
One finding per message, numbered, so each can be triaged on its own. Say "no
feedback this round" when a round was clean. Say explicitly when a fix you were
asked to verify still fails.

Run `minas skill <topic>` for the tool's own rules (read, search, edit, check,
wait, rename, references, errors, exec, ...). Do not file minas issues yourself —
report to <implementer>.
```

## Triage policy (implementer)

Route every report by kind, and answer the driver with the route:

| Report | Action |
|---|---|
| Bug with a repro | Reproduce it yourself first. Fix it if the fix is small and safe; otherwise `gh issue create` (labels: `needs-triage`, then `ready-for-agent`/`ready-for-human`) and tell the driver the issue number. |
| Design question ("should we…?") | Answer, not code. Give the rationale and the ADR/constraint it rests on; accept pushback when the driver's measurement beats it. |
| Wrong/outdated doc or `--help` | Fix the text (skill or clap doc). The tool's own docs are part of the product. |
| Feature request | Implement it, or decline with a reason and record it as backlog. Prefer the ladder: reuse a command, add a flag, or disclose in a skill before adding a command or a protocol field. |
| Praise / confirmation | Acknowledge in one line; do not re-litigate. |

Two habits that keep the loop cheap:

- **Batch protocol changes.** A `PROTOCOL_VERSION` bump costs the driver a re-verification round. Collect small backlog items and ship them in one bump.
- **Every fix leaves one runnable check** (a unit test, or a repro command in the reply) so the next round cannot silently regress it.

## Evidence bar

Hold the driver — and yourself — to these, and say so in the brief:

- Exit code plus the exact command, and the before/after of the artefact (file text, byte count, line/col). "It works now" is not a report.
- Numbers get recounted independently (a second implementation of the count, a direct file read) before a claim is written down.
- **Suspect the instrument first.** A measuring script, a benchmark run without reset, a static scanner that cannot see through delegation, `contains` on ANSI-coloured output — each has produced a false report. Confirm the measurement before confirming the bug.
- A claim made in a doc ("MSRV 1.89", "`--lines` saves 30%") gets verified against primary sources or a real run; stale claims are bugs.
- Verify in the tool's own repo before telling the driver it is fixed.

## Reply format

Short, evidence-first, and always ends with state:

```
Fixed: <one line>.
<command that shows it>  →  <output>
State: <tests / installed / daemon restarted / socket vNN>.
```

## Close out

When the user stops the loop, report: what shipped (grouped by theme), what was declined and why, the backlog still open, and the current build/daemon/test state. Leave the driver's tree clean and the daemon on the current socket version.
