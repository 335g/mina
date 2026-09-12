minas symbol <path> <query>: find where a name lives in the workspace

SYMBOL — workspace search instead of rg (ADR-0032)

Use:      minas symbol <path> <query>
Output:   compact JSON array, no file text: each hit is
          {"name", "kind", "path", "line" (1-origin)} — read the location
          with `minas get --lines`.

Rules
- <path> is any file in the workspace (it anchors the workspace root);
  results can span the whole root, not just that file.
- Use it INSTEAD of rg when you need to know WHERE a name is defined or
  declared: the server returns exact symbols (no whole lines, no false
  positive comments/strings), and the hit range is the definition.
- Fuzzy matching varies by server; a broad query then narrowing by kind/line
  is cheaper than many greps.
- Exit 1 = not supported / bad input (empty query), 2 = retryable.
