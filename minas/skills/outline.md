minas outline <path>: symbol tree (name/kind/span) without full text

OUTLINE — map a file's structure without reading it

Use:      minas outline <path>
          minas outline <path> --recursive [--depth N]  (cross files — ADR-0049)
Output:   compact JSON tree, no file text: each symbol is
          {"name", "kind", "range", "selection_range", "children"},
          kinds: Module/Function/Method/Type/Enum/Constant/Variable/Other

Rules
- On an unfamiliar file, outline FIRST: every symbol's name, kind, and exact
  span for a fraction of a read. Measured (7,981-line file): outline + at =
  55 KB vs a full minas get = 814 KB (~93% fewer bytes).
- --recursive follows file-scoped modules (`mod name;` → the file that defines
  them) and inlines their symbols as children — one call maps a whole crate
  instead of one outline per file. Default depth 3; --depth N implies --recursive
  and sets the cap (--depth 1 = direct child modules only). A "truncated" note on
  stderr means the 500-symbol cap cut the tree (still exit 0; per-file outline
  for the parts you need).
- ranges are char indices (the same unit as DocumentEdit positions);
  selection_range is only the name token. Use them to choose WHICH lines to
  read (minas get --lines / minas read --lines) and WHICH text to target in
  minas apply — read at symbol granularity, not file granularity.
- The tree is nested and grep-able: find the function you need by name instead
  of scanning text.
- The first outline on a path is cold (seconds: project load + LSP settle);
  the daemon caches it, so pull it once at session start and reuse the ranges
  for reads / at / edits. Warm calls are ~0.1s.
- Exit 1 = not supported (no LSP server for this language) or bad input;
  exit 2 = retryable LSP error (retry once later, then give up).
