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
   ```
   `intercom` is a **tool, not a shell command** (`intercom: command not found`) — list
   peers with the intercom tool (`action: list`); it shows each session's cwd and model.
2. Name both sides once (the driver pane keeps its own agent name):
   ```bash
   herdr pane rename <driver_pane> <driver> ; herdr agent rename <driver_pane> <driver>
   herdr pane rename --current <implementer> ; herdr agent rename --current <implementer>
   ```
3. Pick the driver's workspace: `$1` if the user named a repo, otherwise a throwaway
   under `/tmp` so the loop never dirties a real tree:
   ```bash
   DRIVER_REPO=${1:-$(mktemp -d /tmp/minas-dogfood-XXXXXX)}
   ```
   A `/tmp` scratch has no `.env`/`.envrc`, so credentials can only arrive through the
   environment (step 4), and it does not survive a reboot — tell the driver it is scratch.
4. Get a driver pane, then launch Pi in it with **the same model as this pane**. Reuse a
   prepared empty pane if one exists (e.g. labelled `driver`); otherwise split one:
   ```bash
   set -a; . ./.env; set +a                      # implementer's credentials (never printed)
   herdr pane split --current --direction right --cwd "$DRIVER_REPO" --no-focus \
     --env "OPENCODE_API_KEY=$OPENCODE_API_KEY"
   ```
   A split inherits neither the caller's env nor its model: without the key Pi boots
   with an unresolved model and cannot answer at all, and without `--model` it picks
   its own default, so the panes reason with different weights (observed: default
   `deepseek-v4-flash` vs the implementer's `deepseek-v4.1-flash`). This pane's identity
   is in its env (`PI_PROVIDER`, `PI_MODEL`, `PI_REASONING_LEVEL`):
   ```bash
   # one command for either route (an existing pane needs the cd; a fresh split has it already)
   herdr pane run <driver_pane> "cd $DRIVER_REPO && OPENCODE_API_KEY='$OPENCODE_API_KEY' \
     pi --tui-mode fullscreen --model $PI_PROVIDER/$PI_MODEL --thinking $PI_REASONING_LEVEL"
   # equivalent, Herdr-managed route (gives the agent a name up front):
   herdr agent start <driver> --kind pi --pane <driver_pane> \
     -- --model "$PI_PROVIDER/$PI_MODEL" --thinking "$PI_REASONING_LEVEL"
   ```
   `pane run` types the command into the pane's shell; `agent start` args after `--` go
   to `pi` itself (`argv` in its response shows them).
5. Verify the driver is alive **and on the same model** before briefing: the intercom
   list shows each session's model — it must equal this pane's (not `unknown`, not a
   different one), then ask it to say "pong" with the intercom tool (`action: ask`,
   `to: <driver session id>`). A non-answer is an auth/env failure — fix the
   credentials, do not send the activation yet.
6. Send the activation (`## Activation`, below) with the intercom tool (`action: send`).
7. The driver sends the baseline first; keep it — you will diff against it when a "fixed" claim is contested.

## Activation (send only this)

The driver's working rules and report format live in
**`~/.pi/agent/skills/dogfooding-driver/SKILL.md`** (user scope, so it loads in any
driver repo). That file is the single source of truth for the format — do not restate
it here. The activation message therefore carries only the goal and the peer:

```
You are the DRIVER in a minas dogfooding loop. Repo: <driver_repo>.
Goal: <goal>.
Follow your `dogfooding-driver` skill (working rules, report format, evidence bar,
escalation) and report to me — the implementer pane, session <implementer_id>
(cwd = the mina repo). Send your First message (repo, goal, minas info build_ts,
socket version) and start.
```

It arrives as an intercom message — no human typing in the driver pane — and the
driver starts working on it directly (verified: a fresh pane + `agent start` + one
message produced a reply unprompted). If the driver's environment has no
`dogfooding-driver` skill (different machine, or skills disabled), inline that file's
contents in the message instead — the driver stops producing feedback when it has to
guess the format.

## Triage policy (implementer)

Route every report by kind, and answer the driver with the route:

| Report | Action |
|---|---|
| Bug with a repro | Reproduce it yourself first. Fix it if the fix is small and safe; otherwise record it as a task in `.pi/todos` (see below) and tell the driver its id. |
| Design question ("should we…?") | Answer, not code. Give the rationale and the ADR/constraint it rests on; accept pushback when the driver's measurement beats it. |
| Wrong/outdated doc or `--help` | Fix the text (skill or clap doc). The tool's own docs are part of the product. |
| The driver fell back to `rg`/`sed` | Treat it as a minas gap: find which command failed them and fix the command or the skill text. |
| Feature request | Implement it, or decline with a reason and record it as backlog. Prefer the ladder: reuse a command, add a flag, or disclose in a skill before adding a command or a protocol field. |
| Praise / confirmation | Acknowledge in one line; do not re-litigate. |

Two habits that keep the loop cheap:

- **Batch protocol changes.** A `PROTOCOL_VERSION` bump costs the driver a re-verification round. Collect small backlog items and ship them in one bump.
- **Every fix leaves one runnable check** (a unit test, or a repro command in the reply) so the next round cannot silently regress it.

Backlog lives in `.pi/todos` (one task per deferred item: what, why deferred, the repro, the driver's message id). Note it is gitignored — local to this machine. Promote an item to a GitHub issue only when it must outlive the machine or reach other people.

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

When the user stops the loop, report: what shipped (grouped by theme), what was declined and why, the backlog still open, and the current build/daemon/test state. Leave the driver's tree clean and the daemon on the current socket version. Ask what to do with the driver workspace: a `/tmp` scratch can be deleted (`rm -rf`) or kept for re-verification — say which path it was either way, since a scratch dir has no other record of existing.
