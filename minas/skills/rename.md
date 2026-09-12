RENAME — semantic rename vs apply (measured decision)

minas has a built-in semantic rename (content-addressed, ADR-0029):
    minas rename <path> <old> <new>
- Use it for renames with MANY occurrences or MULTIPLE files. Measured (3 files,
  21 occurrences): LSP rename 5/5 success vs apply loop 2/5 (apply kept missing
  occurrences), ~half the tokens (-49%) and cost (-57%).
- It renames the definition and ALL references (imports, calls) in one call,
  saves to disk, and prints the impact: `renamed: old -> new (N files, M edits)`
  plus a `changed:` list — verify the impact is what you meant.
- Exit 1 = not supported / bad input (do not retry), exit 2 = retryable
  (symbol not found, LSP error, stale analysis — re-read and retry).

If only content-editing is available (minas apply):
- For a handful of occurrences in ONE file, loop with apply. Measured on a small
  file it is as cheap as LSP rename.
- For many occurrences or cross-file renames you MUST verify with a final
  `minas references <path> <old>` (or grep/read) that no old name remains —
  measured failure mode is silently leaving one occurrence behind.

Never try to re-implement the rename by hand-editing each call site when an LSP
rename tool exists.
