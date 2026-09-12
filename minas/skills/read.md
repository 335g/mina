map files with outline; read only the lines you need

READ — reading files without wasting tokens

Use:      minas read <path> [--lines <start>:<end>]   (any file, buffer-free — ADR-0048)
          minas get --lines <start>:<end>            (current buffer only)
Output:   numbered lines: {"n":1500,"text":"..."}  (1-origin, end may be empty = last line)

Rules
- Read only the lines you need (measured: ranged read cuts task tokens by ~51% vs
  whole-file reads).
- `minas read <path>` reads ANY file without touching the current buffer — no
  Open/apply needed just to look (a plain `minas read <path>` prints the raw
  text; `--lines` prints numbered lines). `minas get` reads only the buffer
  (last Open/apply) and returns the full snapshot.
- On an unfamiliar file, minas outline FIRST (structure + exact spans for a
  fraction of the bytes), then read only the symbol you need; minas at
  <path> <line>:<col> names the symbol enclosing a spot you are about to touch,
  so both reads and edits happen at symbol granularity.
- For types, minas hover <path> <line>:<col> returns the signature without
  any read; to find where a name lives in the workspace, minas symbol
  <path> <query> replaces rg (both ADR-0032).
- The numbered output lets you paste the exact text you saw into `minas apply`
  as the <old> argument — read and edit share one contract.
- Out-of-range start returns an explained zero result, e.g.
  "no lines in 3000..3100: file has 2000 lines" — this is not an error, just pick
  a valid range.
- A whole-file read is allowed only when you really need the full text; prefer
  --lines otherwise. A whole-file `minas get` is allowed only when you need the
  checksum or diagnostic machinery of the snapshot.
