EDIT — content-resolved edits (the front door)

Use:      minas apply <path> <old> <new>
          minas apply <path> --hunks-stdin   (JSON array of {"old","new"} for many edits)
What it does: Open -> find <old> -> verify (checksum + expected_text) -> replace -> Save,
all in one command. Position math is done for you.

Rules
- Prefer content-resolved edits over positional ones. Measured: positional editing
  (`minas edit` with computed char offsets) fails 0/5 vs 3/5 for content-resolved,
  costs ~5x tokens, and can silently apply to the WRONG occurrence (a successful
  exit with the wrong spot changed). Do not compute start/end char indices.
- After an edit, VERIFY with `minas check <path>` (ADR-0032): it waits for LSP
  diagnostics and returns only errors — 1 round trip, no full text. Exit 2 means
  at least one error diagnostic (warnings alone exit 0). For semantics beyond the
  LSP (borrow checker etc.), still run the real build.
- Locate before you edit: minas outline / minas at return the exact span of
  the symbol you are changing, so your <old> text targets the right region
  instead of a look-alike occurrence elsewhere.
- One invocation replaces the FIRST occurrence of <old>. Repeat for the next one,
  or batch many changes with --hunks-stdin (faster: ~3x on one connection).
- Small targeted replacements beat one huge <old> block: a big mismatch wipes too
  much. For whole-file rewrites use --whole-stdin.
- On rejection (exit 2) the message names the expected/found text and the range.
  Re-read that spot fresh, fix, retry — do not blind-retry.
