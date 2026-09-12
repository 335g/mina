minas references <path> <old>: list a symbol's references

REFERENCES — impact check before/after a rename (ADR-0029)

    minas references <path> <old>

- Resolves <old> like rename (first identifier occurrence) and lists every
  reference with the definition: `path:line` (1-origin), e.g.
      2 references in 1 files:
      /abs/path.rs:4
      /abs/path.rs:1
- Use before a rename to see what will change, or after apply-based edits to
  verify nothing was missed — the T5 failure mode is a silently-left occurrence.
- The output is locations only (token-cheap): read a location with
  `minas get --lines <line>:<line>`.
- Exit 1/2 as with rename.
