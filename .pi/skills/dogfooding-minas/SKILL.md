---
name: dogfooding-minas
description: Run the minas dogfooding loop. One pane (the driver) builds a real app in another repo with minas while a second pane (the implementer) triages the driver's feedback into minas/minad. Use when the user wants to dogfood minas against a real codebase, sets up a driver/implementer pane pair, or a peer pane starts sending minas feedback.
---

Two panes, two roles, one loop: the **driver** uses minas to build something real in a *different* repo; the **implementer** (this pane, the mina repo) turns the driver's reports into minas/minad changes and answers the rest. The driver is the customer; the implementer ships.

The loop only earns its cost if the driver actually reaches for minas every time instead of falling back to `rg`/`sed`/herdr-free editing — a driver that stops using the tool stops producing feedback.

**Vary the driver, and measure the yield.** A loop that repeats the same project with the same model re-measures what is already measured. Each session picks a **profile** (what is being built — `§Driver profiles`) and may pick a **different model** for the driver (`DRIVER_MODEL`), and every session writes one row + one section into `docs/verification/dogfood-log.md` (finding classes, the six counts, the stop criterion). Read that file before starting: it says which profiles are still unexercised and whether the loop has already met its stopping rule.

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
3b. Pick the **profile** and the **driver model** (see `§Driver profiles` and
   `docs/verification/dogfood-log.md` §5 for the table and what each one stresses):
   ```bash
   # $2 = profile name (default: the first unexercised one in the log's table)
   DRIVER_PROFILE=${2:-rust-api}
   # $3 = model for the driver pane. Default = THIS pane's model (same weights both
   # sides is the control condition). Set it when you want a different driver:
   #   a cheaper/weaker model produces different findings (more fallbacks, more retries)
   #   a stronger one stresses the contract instead of the ergonomics.
   DRIVER_MODEL=${3:-$PI_PROVIDER/$PI_MODEL}
   DRIVER_THINKING=${DRIVER_THINKING:-$PI_REASONING_LEVEL}
   echo "profile=$DRIVER_PROFILE model=$DRIVER_MODEL"
   ```
   Record both in the session row — the log is unreadable if the model is not written down.
4. Get a driver pane, then launch Pi in it with the chosen model. Reuse a prepared
   empty pane if one exists (e.g. labelled `driver`); otherwise split one:
   ```bash
   set -a; . ./.env; set +a                      # implementer's credentials (never printed)
   herdr pane split --current --direction right --cwd "$DRIVER_REPO" --no-focus \
     --env "OPENCODE_API_KEY=$OPENCODE_API_KEY"
   ```
   A split inherits neither the caller's env nor its model: without the key Pi boots
   with an unresolved model and cannot answer at all, and without `--model` it picks
   its own default, so the panes reason with different weights (observed: default
   `deepseek-v4-flash` vs the implementer's `deepseek-v4.1-flash`). This pane's identity
   is in its env (`PI_PROVIDER`, `PI_MODEL`, `PI_REASONING_LEVEL`); pass `$DRIVER_MODEL`
   explicitly either way:
   ```bash
   # one command for either route (an existing pane needs the cd; a fresh split has it already)
   herdr pane run <driver_pane> "cd $DRIVER_REPO && OPENCODE_API_KEY='$OPENCODE_API_KEY' \
     pi --tui-mode fullscreen --model $DRIVER_MODEL --thinking $DRIVER_THINKING"
   # equivalent, Herdr-managed route (gives the agent a name up front):
   herdr agent start <driver> --kind pi --pane <driver_pane> \
     -- --model "$DRIVER_MODEL" --thinking "$DRIVER_THINKING"
   ```
   `pane run` types the command into the pane's shell; `agent start` args after `--` go
   to `pi` itself (`argv` in its response shows them).
5. Verify the driver is alive **and on the model you asked for** before briefing: the
   intercom list shows each session's model — it must equal `$DRIVER_MODEL` (not
   `unknown`, not a third value; an override that silently failed is worse than no
   override, because the log would record the wrong condition). Then ask it to say
   "pong" with the intercom tool (`action: ask`, `to: <driver session id>`). A non-answer
   is an auth/env failure — fix the credentials, do not send the activation yet.
6. Send the activation (`## Activation`, below) with the intercom tool (`action: send`).
7. The driver sends the baseline first; keep it — you will diff against it when a "fixed" claim is contested.

## Activation (send only this)

The driver's working rules and report format live in
**`~/.pi/agent/skills/dogfooding-driver/SKILL.md`** (user scope, so it loads in any
driver repo). That file is the single source of truth for the format — do not restate
it here. The activation message therefore carries only the goal and the peer:

```
You are the DRIVER in a minas dogfooding loop. Repo: <driver_repo> (scratch).
Profile: <driver_profile>. Goal: <goal>.
Follow your `dogfooding-driver` skill (working rules, report format, evidence bar,
escalation) and report to me — the implementer pane, session <implementer_id>
(cwd = the mina repo). Send your First message (repo, goal, profile, minas info
build_ts, socket version, your model) and start.
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

## Driver profiles

The goal must change between sessions, or the loop keeps measuring the same surface. The
profile table (what to build, what each one stresses, which are still unexercised) lives in
**`docs/verification/dogfood-log.md` §5** — read it, pick an unexercised profile, and write the
goal from that row's outline rather than inventing a new app shape each time. Two rules:

- **One profile per session, and not the same one twice in a row.** The stop criterion counts
two consecutive clean sessions *of different profiles*; the same profile twice is one measurement.
- **Write the round's hypotheses BEFORE the driver starts, and make them falsifiable.** When the
profile's purpose is to press an unexercised path (a new language, a new project shape, a new
scale), the activation should carry 2-4 concrete predictions of the form "if this gap is real,
you will observe X" (measured stage 2 example: `.tsx` absent from `file-types` → `check
<file>.tsx` says "not supported"; a non-BMP char through the TS path → columns/positions shift
by one if the UTF-16↔char conversion is missing; a JS monorepo → one session per package if the
sharing fix is Rust-shaped). Two reasons, both from this loop's data: a clean round then counts
as *evidence* rather than absence (the stop criterion's `new_class = 0` sessions only mean
something if the round had something to find), and a false hypothesis is as useful as a true one
(it names the thing that IS handled). Put the hypotheses in the session row with their outcome.
- **Vary the model when the question is about the model.** Same model both sides is the control
condition (it isolates the tool). A cheaper/weaker driver finds ergonomic and contract gaps
(more fallbacks, more retries); a stronger one pushes the contract itself. Record which one you
used in the session row.

### `self-host` (the mina repo itself)

This profile's workspace **is the mina repo**, not a throwaway — so it is the one profile where
the driver could damage the product, and it runs in **ramped stages** (the ramp and its
preconditions are in `docs/verification/dogfood-log.md` §5):

- **Stage 1 — read-only.** The driver's goal must literally forbid edits (`apply` / `edit` /
  `rename` / `delete`), and must say that *wanting* an edit is a finding, not an action. Reads
  only: `read` / `search` / `outline` / `at` / `symbol` / `references` / `hover` / `peek` /
  `check` / `info` / `wait`. Enforce it by audit, not by trust: before the session record the
  current HEAD, and after it run `git log --oneline <recorded>..HEAD` and check the
  `Checkpoint-Session:` trailer of any new commit — the auto-commit hook commits from the pane
  that made the change, so a commit from the *driver's* session id is a write that must be
  reverted and reported. This stage measures the tool at 400k lines / 10 crates (cold index,
  big-file outline, `check` on a virtual workspace) — it is where `sw` at scale shows up.
- **Stage 2 — leaf crates** (`mina-text`, `mina-view`, `mina-loader`: no daemon involvement).
- **Stage 3 — `minad` / `minas` themselves** (the tool editing the tool: `tools/dev-restart.sh`
  installs and swaps the daemon, then proves a new pid answers — see
  `docs/verification/dogfood-log.md` §5 precondition 4).

Do not start a stage until the previous one's gate held: `sw = 0` **and every `fallback` in
that session accounted for** (either fixed, or declined-with-disclosure in a skill — an
unexplained fallback is what blocks the next stage). Measured on stage 1: `sw = 0` with two
fallbacks that were both declined-and-disclosed (directory enumeration, cross-file text search),
so `fallbacks = 0` is the wrong bar — it would block stage 2 forever on tool-scope boundaries
that are deliberate.

## Measurement (per session)

Record one row + one section in **`docs/verification/dogfood-log.md`** (§3 table, §6 template).
The six counts and their definitions are in that file (§2); do not invent new ones — the whole
point is that session N is comparable with session N-1. In short: classify every finding into
the fixed classes (§1), count `sw` (silent-wrong) / `dl` (data-loss) / `findings` /
`new_class` / `fallbacks` / `reverify`, and note the driver's cost if the panes report it.

Two habits that make the numbers trustworthy:

- **`fallbacks` comes from the driver's own report** (its skill requires it to say when it left
the tool and why). Ask for it explicitly in the round summary if a session did not mention it —
a silent fallback is a missing row, not a zero.
- **`new_class` is what the loop is actually buying.** Re-finding a known class in a new profile is
worth a fix but is not new evidence of value; the first session that touches a class is.

Cost is measured by the other instrument, not here: `docs/loop/l0.py` (deterministic:
`calls` / `out_B` / `equiv_B` / `wall_ms`) for the flows the session exercised, and
`tools/ab/ab.py` when a fix needs real-LLM confirmation.

## Stop criterion

`docs/verification/dogfood-log.md` §4 holds the rule (five conditions, plus the reset rule and the
"declare remaining profiles out of scope" requirement). Read it **before** starting a session and
say in your first report whether the loop has already met it — continuing past the criterion is
allowed but must be a decision, not an accident. A session with `sw > 0` or `dl > 0` or
`new_class > 0` resets the counter.

## Reply format

Short, evidence-first, and always ends with state:

```
Fixed: <one line>.
<command that shows it>  →  <output>
State: <tests / installed / daemon restarted / socket vNN>.
```

## Close out

When the user stops the loop, report: what shipped (grouped by theme), what was declined and why, the backlog still open, and the current build/daemon/test state. Leave the driver's tree clean and the daemon on the current socket version. Ask what to do with the driver workspace: a `/tmp` scratch can be deleted (`rm -rf`) or kept for re-verification — say which path it was either way, since a scratch dir has no other record of existing.

**Close out the session in `docs/verification/dogfood-log.md` before writing that report** — one row in the §3 table and one section from the §6 template, with the six counts and the finding classes. Then state the stop-criterion status explicitly (`§4`: met / not met, and which condition is missing). A loop that ends without a row leaves the next session unable to tell whether the last one was worth its cost — the numbers are the only durable product of a session whose fixes are already committed.
