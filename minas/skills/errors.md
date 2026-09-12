ERRORS — exit codes and recovery

Exit codes (applies to minas apply / edit / hunks):
  0  success (applied, or no-op)
  1  usage/CLI error — fix the arguments; do not retry the same call
  2  retryable failure — old text not found, checksum/expected_text mismatch,
     or Save failed. Re-read the file fresh, then retry.

Semantic commands (rename / references / outline / at / hover / symbol / check)
share 0/1/2 but classify differently: exit 1 = not supported / invalid input
(do not retry), exit 2 = retryable (symbol not found, LSP error — re-run once
later). Exceptions:
- minas at with no enclosing symbol is exit 0 (found=false), not an error —
  only the LSP request itself can fail.
- minas hover with nothing at the position is exit 0 (empty text), not an error.
- minas check returns exit 2 when at least one error diagnostic is present
  (warnings alone exit 0) — branch on $? without parsing the JSON.

Rejections are the editor telling you what changed:
- "document changed since read"      -> the file moved; re-read and retry.
- "expected text mismatch: expected X,
  found Y at [lo,hi)" -> the range is not
  what you assumed; re-read THAT range (use --lines) and fix the old text.
- "NOT FOUND: text"                  -> the old string is not in the file; re-grep.

Recovery loop: read the reported spot with `minas get --lines`, correct the
old/new, retry. Never blind-retry a rejected edit.
