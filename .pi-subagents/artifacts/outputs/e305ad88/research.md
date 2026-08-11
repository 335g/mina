# Research: Rust terminal library landscape for a minimal Helix-style editor

**Date note:** the task says "current (2025)", but the research run reflects a later date — crates.io timestamps and docs.rs builds observed during this run go through 2026-06-19 (e.g. ratatui 0.30.2 published 2026-06-19; docs.rs built termion 4.0.6 docs with rustc 1.98.0-nightly, 2026-06-08). `rust-version = 1.96` matches a ~May 2026 stable toolchain. All versions below are exact values read from crates.io API JSON, with release dates, so they can be re-verified.

## Summary

For a minimal, custom-drawn Helix-like editor, use **termina** — the low-level VT-manipulation crate that Helix itself now uses as its Unix terminal backend (merged 2025-08-31), actively maintained by a Helix core maintainer with the exact features an editor needs (kitty keyboard protocol, bracketed paste, synchronized output, mouse, resize, focus, Windows VT). crossterm is the safe, boring, battle-tested alternative; ratatui is the right choice only if you want layout + frame diffing for free; termion (dormant, no resize events) and termwiz (stalled releases, 100k-line kitchen sink) are not recommended.

## Findings

### Comparison table (all data from crates.io API + GitHub, fetched this run)

| | crossterm | ratatui | termion | termina | termwiz |
|---|---|---|---|---|---|
| Latest version | 0.29.0 | 0.30.2 | 4.0.6 | 0.3.3 | 0.23.3 |
| Last release | 2025-04-05 | 2026-06-19 | 2025-11-21 | 2026-05-30 | 2025-03-20 |
| Maintained? | Yes (repo active; releases slowed — ~14 mo since last) | Yes (very active, monthly) | Effectively dormant (maintenance-only releases) | Yes (active, ~monthly, Helix-driven) | Dev continues in wezterm monorepo; crates.io releases stalled (open issue) |
| MSRV / edition | 1.63 / 2021 | 1.88 / 2024 | none listed / 2015-era | 1.71 / 2021 | none listed / 2018 |
| License | MIT | MIT | MIT | MIT OR MPL-2.0 | MIT |
| Abstraction | Raw terminal + events | Widget framework + diffed renderer over pluggable backends | Raw terminal + events | Raw VT escape codes + events (lowest level) | Emulator toolkit + app lib (kitchen sink) |
| Raw mode / alt screen | ✓ | ✓ (via backend) | ✓ | ✓ | ✓ |
| Key events w/ modifiers | ✓ incl. kitty keyboard protocol (push/pop flags, SUPER/HYPER/META, 0.25+) | via backend | ✓ basic | ✓ incl. kitty protocol, modifiers bitflags | ✓ (best-in-class parser) |
| Resize events | ✓ | ✓ (autoresize on draw) | ✗ no `Event::Resize` | ✓ | ✓ |
| Mouse | ✓ (incl. scroll L/R, SGR) | via backend | ✓ (coords) | ✓ (incl. SGR, large positions) | ✓ |
| Focus / bracketed paste / sync output / OSC52 | ✓ / ✓ / ✓ / ✓ | via backend | ✗ / ✗ / ✗ / ✗ | ✓ / ✓ / ✓ / ✓ | ✓ / ✓ / ✓ / ✓ |
| Rendering model | None — you write escapes; `queue`/`execute` command API | Immediate mode; `Terminal` double-buffers and diffs frames, writes only changed cells | None — direct writes | None — you own buffer/diff (Helix does its own) | Surface-based renderer + diffing, cassowary-layout widgets |
| Windows | ✓ (incl. legacy console, Win7+) | via backend | ✗ (Unix/Redox only) | ✓ (ConPTY/VT; optional `windows-legacy`) | ✓ |
| Fit for custom-drawn editor | Good base; you build buffer/diff | Good; Layout solves splits, `Frame::buffer_mut` allows raw drawing | Poor (no resize events, no modern protocols, dormant) | **Best** — it is Helix's stack | Overkill and stale releases |

### 1. termina — recommended
- New crate (initial publish 2025-08-31, same day Helix's switch PR merged), maintained by the-mikedavis (Michael Davis, Helix maintainer); Helix pinned `termina = "0.3"` in its workspace Cargo.toml (read from helix master) and uses `TerminaBackend` for all non-Windows rendering plus `termina::Event` for its event loop; crossterm remains only for Windows in Helix. [helix Cargo.toml](https://github.com/helix-editor/helix/blob/master/Cargo.toml), [PR #13307](https://github.com/helix-editor/helix/pull/13307) (merged 2025-08-31), [helix-tui termina backend](https://github.com/helix-editor/helix/blob/master/helix-tui/src/backend/termina.rs), [helix-term application.rs](https://github.com/helix-editor/helix/blob/master/helix-term/src/application.rs)
- Abstraction: lowest level of the serious candidates — escape-code construction (`escape::{csi, dcs, osc}`), event parsing, terminal capability detection (DA1/XTVERSION, kitty protocol detection, synchronized-output detection) with `poll`/`read` internals exposed so an app can interrogate the terminal mid-run. "A cross between Crossterm and TermWiz... exposes escape sequences and pushes handling to the application." [termina README](https://github.com/helix-editor/termina)
- Capabilities (from README + CHANGELOG): kitty keyboard protocol, bracketed paste, mouse incl. SGR and large coordinates, resize/WindowSize, focus events, OSC52 (base64 module), OSC dynamic color sequences (0.3.0), DECRPM 2026 grapheme-clustering parsing (0.3.1), CSI sequences for the **kitty Multiple Cursor protocol** (0.3.0 — directly relevant to multi-cursor work), cursor styles, truecolor, Windows via ConPTY/VT with optional `windows-legacy` feature. [CHANGELOG](https://github.com/helix-editor/termina/blob/master/CHANGELOG.md)
- Release cadence: v0.1.0 2025-08-31 → v0.3.3 2026-05-30; 8 versions; ~2.35M downloads (Helix-scale); MIT OR MPL-2.0; MSRV 1.71 (≤ 1.96). [crates.io API](https://crates.io/api/v1/crates/termina)
- Rendering model: none provided — you own the cell buffer and diff, exactly like Helix does (helix-tui `Buffer::diff` yields `Vec<(u16, u16, &Cell)>` with wide-char handling). [helix-tui buffer.rs](https://github.com/helix-editor/helix/blob/master/helix-tui/src/buffer.rs)
- Risks: young (0.3.x; API churn possible), sparse docs (improved in 0.3.3), effectively a 1–2 maintainer project behind the Helix org. ratatui already ships an official `ratatui-termina` backend, which de-risks ecosystem adoption. [ratatui Cargo.toml](https://github.com/ratatui/ratatui/blob/main/ratatui/Cargo.toml)

### 2. crossterm — the safe alternative
- Latest 0.29.0 (2025-04-05), MSRV 1.63, MIT. No crates.io release in ~14 months as of this run, but repo is maintained (kitty keyboard protocol landed in 0.26, OSC52 in 0.29; the-mikedavis himself contributed the kitty work). [crates.io API](https://crates.io/api/v1/crates/crossterm), [CHANGELOG](https://github.com/crossterm-rs/crossterm/blob/master/CHANGELOG.md)
- Raw terminal/event library: `Command` queue/execute, `event::read`/`poll` with `Event::{Key, Mouse, Resize, FocusGained/FocusLost (Windows), Paste}`, raw mode, alternate screen, cursor styles, synchronized output, bracketed paste, kitty protocol flags, mouse capture (incl. scroll left/right), Windows down to Win7. No buffer, no diffing — you build rendering yourself.
- Fit: the default ecosystem choice (ratatui's default backend, 41M recent downloads). Boring, well-documented, broadly compatible. Downside vs termina: higher-level command API (you fight `execute!`/`queue!` macros when you want raw escape control), and it is not what Helix runs on anymore.

### 3. ratatui — widget framework + diffed renderer
- Latest 0.30.2 (2026-06-19), very active (monthly releases), MSRV 1.88, **edition 2024**. 0.30 modularized into `ratatui-core`, `ratatui-widgets`, `ratatui-macros`, and per-backend crates `ratatui-crossterm`, `ratatui-termina`, `ratatui-termion`, `ratatui-termwiz`; core is `no_std`-capable. [crates.io API](https://crates.io/api/v1/crates/ratatui), [workspace Cargo.toml](https://github.com/ratatui/ratatui/blob/main/Cargo.toml)
- Rendering model: immediate mode — each `draw` pass re-renders the whole UI into a fresh buffer; `Terminal` keeps two buffers, diffs current vs previous frame, and sends only changed cells to the backend (documented pipeline; `Buffer::diff` handles wide chars). [ratatui-core terminal.rs](https://github.com/ratatui/ratatui/blob/main/ratatui-core/src/terminal.rs)
- For an editor: `Layout`/`Constraint` solves split views; `Frame::buffer_mut()` lets you draw raw cells for custom multi-cursor rendering; backend choice is a feature flag — `features = ["termina"]` pairs it with termina's modern VT events. Cost: you inherit the widget/styling model and a bigger API surface than a minimal editor needs. Successor of the archived `tui-rs` (which Helix forked into `helix-tui`).

### 4. termion — historical, dormant
- Latest 4.0.6 (2025-11-21), published by jackpot51 (Redox OS); no meaningful development (community noted zero activity since 2021; 4.x releases are maintenance bumps). 593 dependents, Unix/Redox only. [crates.io API](https://crates.io/api/v1/crates/termion), [r/rust thread](https://www.reddit.com/r/rust/comments/y4o55x/termion_development_status/)
- `Event` enum is exactly `Key | Mouse | Unsupported` — **no resize events** (verified on docs.rs for 4.0.6), no kitty keyboard protocol, no bracketed paste, no focus, no synchronized output, no Windows. [termion docs.rs event enum](https://docs.rs/termion/latest/termion/event/enum.Event.html)
- Fit: poor for a modern modal editor. It was a 2016-era pioneer; Helix-era editors have moved past it.

### 5. termwiz — powerful but wrong shape
- Latest 0.23.3 (2025-03-20); ~100k lines; releases stalled — open wezterm issue #7549 (2026-02-02) asks for a release after 11 months; development continues in the wezterm monorepo but crates.io lags. [crates.io API](https://crates.io/api/v1/crates/termwiz), [wezterm issue #7549](https://github.com/wezterm/wezterm/issues/7549)
- Dual purpose: terminal *emulator* toolkit (Surface, vtparse, input parsing) plus app library (Terminal abstraction, cassowary-layout widgets). Capabilities are the fullest of the set, and its escape parser is best-in-class (wezterm is its own proof).
- Fit: the abstraction is heavy and app-facing docs are thin relative to its size; for a minimal editor you'd use a small fraction of it while carrying the whole dependency. Only reach for it if you need to build an emulator or parse arbitrary VT streams.

### 6. Other candidates considered (not serious contenders for this project)
- `tui-rs`: original, archived/unmaintained; ratatui is its successor. Skip. [ratatui README](https://github.com/ratatui/ratatui)
- `cursive`: retained-mode ncurses-style widget framework (crossterm backend); callbacks and view hierarchy fight a custom immediate-mode editor. [cursive](https://github.com/gyscos/cursive)
- `tui-realm`: stateful Elm/React-style framework layered on ratatui — an extra abstraction on top of the layer you'd already be using. [tui-realm](https://github.com/veeso/tui-realm)
- `termbox2`: C library with thin Rust bindings; not Rust-native; skip. [termbox2](https://github.com/termbox/termbox2)
- `console` (clap org), `iocraft`, `vte`/`vt100`: styling/rich-output helpers or emulator-side escape parsing — not fullscreen editor libraries.

### 7. Recommendation (one concrete pick)

**Use `termina` alone as the terminal layer, with your own small cell buffer + frame diff** — this is exactly the architecture Helix runs (termina backend + helix-tui `Buffer::diff` + view rendering). Rationale:
1. **Proven for this exact use case** — it powers Helix's Unix rendering and event loop; reference implementation available in-repo.
2. **Actively maintained for editor needs** — monthly releases; kitty keyboard protocol (essential for unambiguous modifier key chords in a modal editor), bracketed paste, synchronized output, mouse/SGR, resize, focus, OSC52, and even kitty Multiple Cursor CSI sequences landed in 0.3.0.
3. **Right abstraction level** — escape-code-level API means no framework impedance when custom-drawing multi-cursor selections and split views; you keep full control, and Helix's code shows the idiomatic shape.
4. **Tiny footprint** — no mio/signal-hook event stack, no widget machinery; MSRV 1.71, MIT OR MPL-2.0, works on Unix + Windows (ConPTY).
5. **Ecosystem backstop** — ratatui ships an official termina backend, so if you later want layout/diffing for free you can adopt `ratatui` with the `termina` feature without changing your terminal layer.

If the parent prefers the boring choice: **crossterm 0.29** is fine and 100% compatible with the same architecture (it's what Helix ran before termina). If they want splits + diffing for free: **ratatui 0.30 with the `termina` backend** (edition 2024, MSRV 1.88 — compatible with rust-version 1.96).

**Exact Cargo.toml (rust-version 1.96, edition 2024):**

```toml
[package]
name = "minimal-helix"
version = "0.1.0"
edition = "2024"
rust-version = "1.96"

[dependencies]
# Primary recommendation: termina 0.3.3 (2026-05-30) — Helix's terminal backend
termina = "0.3"

# Alternative A (boring/safe): crossterm 0.29.0 (2025-04-05)
# crossterm = "0.29"

# Alternative B (layout + frame diffing for free): ratatui 0.30.2 (2026-06-19)
# ratatui = { version = "0.30", default-features = false, features = ["termina"] }
```

All three alternatives are ≤ the toolchain: termina MSRV 1.71, crossterm MSRV 1.63, ratatui MSRV 1.88.

## Sources

- Kept: crates.io API JSON (crossterm/ratatui/termina/termion/termwiz) — authoritative max_version, release dates, MSRV, features, licenses. (https://crates.io/api/v1/crates/<name>)
- Kept: helix-editor/termina README + CHANGELOG — capability list, cadence, license, Windows modes. (https://github.com/helix-editor/termina)
- Kept: helix-editor/helix PR #13307 (GitHub API) — merged_at 2025-08-31, termina rationale. (https://api.github.com/repos/helix-editor/helix/pulls/13307)
- Kept: helix master Cargo.toml — `termina = "0.3"` pin; helix-tui backend.rs/termina.rs + application.rs — backend split (termina on Unix, crossterm on Windows). (https://github.com/helix-editor/helix)
- Kept: helix-tui/src/buffer.rs — `Buffer::diff` confirmed (frame diffing with wide-char handling).
- Kept: ratatui Cargo.toml + ratatui-core/src/terminal.rs — edition 2024, MSRV 1.88, backend features, double-buffered diffing pipeline.
- Kept: docs.rs termion 4.0.6 event enum — `Key | Mouse | Unsupported` (no Resize).
- Kept: crossterm CHANGELOG/releases — 0.29.0 date, kitty protocol/OSC52 features.
- Kept: wezterm issue #7549 — termwiz release stall.
- Dropped: libhunt/umatechnology listicles, reddit thread — secondary commentary; facts re-verified against primary sources.

## Gaps

- **Date discrepancy:** task says "2025"; observed environment data is mid-2026 (ratatui 0.30.2 published 2026-06-19, docs.rs builds dated 2026-06). All version numbers are exact as of the research run and re-verifiable from the cited crates.io/GitHub URLs.
- crossterm's *repo* activity level since 0.29.0 was assessed via CHANGELOG/releases pages, not commit history — if release cadence is a hard criterion, check https://github.com/crossterm-rs/crossterm/commits/master directly.
- termina's API stability beyond 0.3.x is unknown (pre-1.0). Mitigation: Helix pins it and tracks releases; ratatui-termina exists.
- No benchmark data gathered on throughput (unnecessary for a screen-sized editor; all candidates write only changed cells or nothing — flicker/perf is not a differentiator here).

## Supervisor coordination

No supervisor contact needed — research complete, no blocking decisions.

```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "Concrete findings with exact versions, release dates, MSRVs, and file paths: all five candidates profiled with primary-source citations (crates.io API JSON, helix-editor/termina README+CHANGELOG, helix PR #13307 merged 2025-08-31, helix master Cargo.toml pinning termina=\"0.3\", ratatui 0.30 Cargo.toml edition=2024/MSRV=1.88, termion 4.0.6 Event enum with no Resize variant). Severity/flags applied where relevant: termion dormant (no resize events), termwiz release-stalled (wezterm #7549), crossterm 14-month release gap, termina pre-1.0 API churn risk. Single recommendation given (termina 0.3) with exact Cargo.toml snippet for rust-version 1.96 / edition 2024 and two fully-pinned alternatives. Written to the authoritative output path."
    }
  ],
  "changedFiles": [
    "/Users/335g/dev/other/mina/.pi-subagents/artifacts/outputs/e305ad88/research.md"
  ],
  "testsAddedOrUpdated": [],
  "commandsRun": [
    {
      "command": "web_search: landscape, helix backend, termina, ratatui releases (2 rounds, 8 queries)",
      "result": "passed",
      "summary": "Identified termina as Helix's new backend; surfaced ratatui 0.30 modularization and crossterm 0.29"
    },
    {
      "command": "fetch_content: crates.io API JSON for crossterm, ratatui, termina, termion, termwiz",
      "result": "passed",
      "summary": "Exact max_version, publish dates, MSRV, edition, license for all five crates"
    },
    {
      "command": "fetch_content: helix-editor/termina README+CHANGELOG, helix PR #13307 (GitHub API), helix master Cargo.toml, helix-tui backend.rs+buffer.rs",
      "result": "passed",
      "summary": "Confirmed termina capabilities, merge date 2025-08-31, helix pin termina=\"0.3\", Buffer::diff rendering model"
    },
    {
      "command": "fetch_content: ratatui workspace Cargo.toml + ratatui-core/src/terminal.rs, docs.rs termion Event enum, crossterm releases, wezterm issue #7549",
      "result": "passed",
      "summary": "Confirmed ratatui edition 2024/MSRV 1.88/diffing pipeline, termion Event variants (no Resize), termwiz release stall"
    }
  ],
  "validationOutput": [
    "Version claims cross-checked against crates.io API JSON (primary): crossterm 0.29.0 (2025-04-05), ratatui 0.30.2 (2026-06-19), termina 0.3.3 (2026-05-30), termion 4.0.6 (2025-11-21), termwiz 0.23.3 (2025-03-20) — all consistent with GitHub CHANGELOGs and release pages."
  ],
  "residualRisks": [
    "Environment date is mid-2026 while the task says 2025; version pins are exact as of the research run and re-verifiable at cited URLs.",
    "termina is pre-1.0 (0.3.x): API churn risk; mitigated by helix tracking it and ratatui-termina existing as a backstop.",
    "crossterm repo activity since 0.29.0 assessed from CHANGELOG/releases pages, not raw commit history.",
    "No throughput benchmarks gathered; judged non-differentiating for a screen-sized editor (all candidates write only changed cells)."
  ],
  "noStagedFiles": true,
  "diffSummary": "Wrote research brief comparing crossterm/ratatui/termion/termina/termwiz across maintenance, abstraction, capabilities, and rendering model, with one recommendation (termina 0.3) plus exact Cargo.toml versions.",
  "reviewFindings": [
    "no blockers - research-only task; key finding: termina is NOT a termion fork but a new Helix-owned VT library (v0.1.0 2025-08-31) that replaced crossterm as Helix's Unix backend; termion lacks resize events entirely (Event = Key|Mouse|Unsupported)."
  ],
  "manualNotes": "Parent should reconcile task's '2025' framing with observed mid-2026 timestamps before pinning versions; termina 0.3.3 is the latest published version. If ecosystem conservatism wins, crossterm 0.29.0 is the drop-in safe pick."
}
```
