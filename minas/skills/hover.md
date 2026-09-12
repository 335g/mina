HOVER — type/signature lookup without reading (ADR-0032)

Use:      minas hover <path> <line>:<col>   (1-origin, like get --lines)
Output:   compact JSON: {"path", "text"} — type + signature + doc, no file text.
          Empty text is a SUCCESS (exit 0): nothing to hover at that position
          (whitespace, comments).

Rules
- Before reading a definition to learn its type, hover the call site: the
  server returns the signature in one round trip (token-cheap).
- The doc comment is truncated (<=2000 chars) — the type/signature is the
  reliable part; read the doc only when the short part is not enough.
- Pair with minas peek <path> <line>:<col> to jump to the definition.
- Exit 1/2 as with outline/at (1 = not supported / bad input, 2 = retryable).
