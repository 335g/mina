minas check <path>...: wait for diagnostics, returns only errors

CHECK — the edit -> verify loop in one command (ADR-0032)

Use:      minas check <path> [<path> ...]
Output:   compact JSON, no file text: an array of {"path", "total",
          "diagnostics", "settled"} (one entry per path; ADR-0046) where
          each diagnostic is {"severity", "line" (1-origin), "col"
          (1-origin, char units), "start", "end", "message"}.
Exit:     0 = no error diagnostics (warnings alone are fine),
          2 = at least one error diagnostic, or a retryable LSP failure,
          1 = not supported / bad input.

Rules
- After an edit (minas apply / edit), run check INSTEAD of wait + get +
  JSON-parsing the snapshot: it waits for LSP diagnostics to settle and returns
  only the diagnostics — the round trip and the full text are gone.
- Pass multiple paths to verify every file you touched in one call
  (ADR-0046): `minas check src/lib.rs src/other.rs`.
- The 1-origin line is the address for `minas get --lines <n>:<n>`; the
  char range is the address for `minas apply`.<old>. The line:col pair is the
  address for `minas at / peek / hover <path> <line>:<col>` — pass it straight
  through without computing anything (ADR-0047).
- It is an LSP fast path (incremental), not a compiler: for semantics beyond
  the LSP (borrow checker etc.) still run the real build.
- settled == false means the result is NOT a verified clean: empty +
  settled=false is "clean" UNVERIFIED, NOT clean (ADR-0045). It comes back
  fast — the pull answer is stable on the first request, so an empty result is
  returned after ~0.6s instead of burning a ~10s budget (ADR-0052), and waiting
  would not turn it into a clean anyway. rust-analyzer's pull misses real
  errors (a broken method call can report empty), so never treat empty as
  clean: run the real build (cargo check / cargo test) before trusting it.
