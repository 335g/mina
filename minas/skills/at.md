minas at <path> <line>:<col>: enclosing symbol + its exact range

AT — the symbol enclosing a position, with its exact range

Use:      minas at <path> <line>:<col>   (1-origin, like get --lines)
Output:   pretty JSON, no file text: {"name", "kind", "range",
          "selection_range", "found"}. found=false is a SUCCESS (exit 0):
          the position is in no symbol — pick a different spot.

Rules
- Before touching a spot, ask what it belongs to: at names the enclosing
  function/type and its exact span, so your read and your <old> text are
  scoped to the right region — no full-file reads, no guessing which function
  a matching string lives in.
- selection_range is the substring of the symbol NAME: the address for
  rename-style edits and for locating the token named in a rejection message.
- Ranges are char indices: pair the span with minas get --lines to see the
  actual lines before editing inside it.
- Warm after an outline of that path: ~0.1s (measured 6.5s -> 0.09s cold to
  cached); the cold cost is paid by the one session-start outline, not by
  every at.
- Exit 1/2 as with outline/rename (1 = not supported / bad input, 2 =
  retryable LSP error).
