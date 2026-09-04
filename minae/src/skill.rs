//! `minae skill` — エージェント向けの軽量スキル（判断・手順の参考書）。
//!
//! 設計（docs/agent-editor-ab-results.md の実測に基づく）:
//! - `minae skill`（無引数）= **索引**。1トピック1行・約10行。薄く保ち、常時ロードしても
//!   トークン負担が小さい（T1: 範囲read vs 全文read で −51% の実測と同じ原理 —
//!   必要になったトピックだけを読む）。
//! - `minae skill <topic>` = **そのトピックの内容だけ**を返す。ロードオンリー・必要時のみ。
//! - 索引と内容を分離し、モデルが「発見（安い索引）→ 必要分だけ参照（安い内容）」できるようにした。
//!
//! 契約（session と統一）: 成功 = exit 0（stdout に内容）、未知トピック = exit 1（stderr に
//! 英語の理由＋利用可能トピック一覧）。エージェントは $? と stderr だけで制御できる。
//! 内容は英語（H4: エージェントがパースする出力は英語統一）。
//!
//! 各トピックの内容はツール判断の指針であり、tools/ab の A/B 実測（t1〜t5）と
//! ADR-0029（rename / references）・ADR-0031（outline / at）・ADR-0032
//! （hover / symbol / check）の実測・仕様に基づく。

/// トピック定義。説明は索引行に使う。内容は「行動レベル」に書く（長手順・抑制・判断表）。
const SKILLS: &[(&str, &str, &str)] = &[
    (
        "read",
        "map files with outline; read only the lines you need",
        "READ — reading files without wasting tokens

Use:      session get --lines <start>:<end>
Output:   numbered lines: {\"n\":1500,\"text\":\"...\"}  (1-origin, end may be empty = last line)

Rules
- Read only the lines you need (measured: ranged read cuts task tokens by ~51% vs
  whole-file reads).
- On an unfamiliar file, session outline FIRST (structure + exact spans for a
  fraction of the bytes), then read only the symbol you need; session at
  <path> <line>:<col> names the symbol enclosing a spot you are about to touch,
  so both reads and edits happen at symbol granularity.
- For types, session hover <path> <line>:<col> returns the signature without
  any read; to find where a name lives in the workspace, session symbol
  <path> <query> replaces rg (both ADR-0032).
- The numbered output lets you paste the exact text you saw into `session apply`
  as the <old> argument — read and edit share one contract.
- Out-of-range start returns an explained zero result, e.g.
  \"no lines in 3000..3100: file has 2000 lines\" — this is not an error, just pick
  a valid range.
- A whole-file `session get` is allowed only when you really need the checksum or
  the full text; prefer --lines otherwise.",
    ),
    (
        "edit",
        "edit via content (session apply); never compute positions",
        "EDIT — content-resolved edits (the front door)

Use:      session apply <path> <old> <new>
          session apply <path> --hunks-stdin   (JSON array of {\"old\",\"new\"} for many edits)
What it does: Open -> find <old> -> verify (checksum + expected_text) -> replace -> Save,
all in one command. Position math is done for you.

Rules
- Prefer content-resolved edits over positional ones. Measured: positional editing
  (`session edit` with computed char offsets) fails 0/5 vs 3/5 for content-resolved,
  costs ~5x tokens, and can silently apply to the WRONG occurrence (a successful
  exit with the wrong spot changed). Do not compute start/end char indices.
- After an edit, VERIFY with `session check <path>` (ADR-0032): it waits for LSP
  diagnostics and returns only errors — 1 round trip, no full text. Exit 2 means
  at least one error diagnostic (warnings alone exit 0). For semantics beyond the
  LSP (borrow checker etc.), still run the real build.
- Locate before you edit: session outline / session at return the exact span of
  the symbol you are changing, so your <old> text targets the right region
  instead of a look-alike occurrence elsewhere.
- One invocation replaces the FIRST occurrence of <old>. Repeat for the next one,
  or batch many changes with --hunks-stdin (faster: ~3x on one connection).
- Small targeted replacements beat one huge <old> block: a big mismatch wipes too
  much. For whole-file rewrites use --whole-stdin.
- On rejection (exit 2) the message names the expected/found text and the range.
  Re-read that spot fresh, fix, retry — do not blind-retry.",
    ),
    (
        "outline",
        "session outline <path>: symbol tree (name/kind/span) without full text",
        "OUTLINE — map a file's structure without reading it

Use:      session outline <path>
Output:   compact JSON tree, no file text: each symbol is
          {\"name\", \"kind\", \"range\", \"selection_range\", \"children\"},
          kinds: Module/Function/Method/Type/Enum/Constant/Variable/Other

Rules
- On an unfamiliar file, outline FIRST: every symbol's name, kind, and exact
  span for a fraction of a read. Measured (7,981-line file): outline + at =
  55 KB vs a full session get = 814 KB (~93% fewer bytes).
- ranges are char indices (the same unit as DocumentEdit positions);
  selection_range is only the name token. Use them to choose WHICH lines to
  read (session get --lines) and WHICH text to target in session apply — read
  at symbol granularity, not file granularity.
- The tree is nested and grep-able: find the function you need by name instead
  of scanning text.
- The first outline on a path is cold (seconds: project load + LSP settle);
  the daemon caches it, so pull it once at session start and reuse the ranges
  for reads / at / edits. Warm calls are ~0.1s.
- Exit 1 = not supported (no LSP server for this language) or bad input;
  exit 2 = retryable LSP error (retry once later, then give up).",
    ),
    (
        "at",
        "session at <path> <line>:<col>: enclosing symbol + its exact range",
        "AT — the symbol enclosing a position, with its exact range

Use:      session at <path> <line>:<col>   (1-origin, like get --lines)
Output:   pretty JSON, no file text: {\"name\", \"kind\", \"range\",
          \"selection_range\", \"found\"}. found=false is a SUCCESS (exit 0):
          the position is in no symbol — pick a different spot.

Rules
- Before touching a spot, ask what it belongs to: at names the enclosing
  function/type and its exact span, so your read and your <old> text are
  scoped to the right region — no full-file reads, no guessing which function
  a matching string lives in.
- selection_range is the substring of the symbol NAME: the address for
  rename-style edits and for locating the token named in a rejection message.
- Ranges are char indices: pair the span with session get --lines to see the
  actual lines before editing inside it.
- Warm after an outline of that path: ~0.1s (measured 6.5s -> 0.09s cold to
  cached); the cold cost is paid by the one session-start outline, not by
  every at.
- Exit 1/2 as with outline/rename (1 = not supported / bad input, 2 =
  retryable LSP error).",
    ),
    (
        "rename",
        "minae session rename <path> <old> <new> (semantic); apply for a few",
        "RENAME — semantic rename vs apply (measured decision)

minae has a built-in semantic rename (content-addressed, ADR-0029):
    session rename <path> <old> <new>
- Use it for renames with MANY occurrences or MULTIPLE files. Measured (3 files,
  21 occurrences): LSP rename 5/5 success vs apply loop 2/5 (apply kept missing
  occurrences), ~half the tokens (-49%) and cost (-57%).
- It renames the definition and ALL references (imports, calls) in one call,
  saves to disk, and prints the impact: `renamed: old -> new (N files, M edits)`
  plus a `changed:` list — verify the impact is what you meant.
- Exit 1 = not supported / bad input (do not retry), exit 2 = retryable
  (symbol not found, LSP error, stale analysis — re-read and retry).

If only content-editing is available (session apply):
- For a handful of occurrences in ONE file, loop with apply. Measured on a small
  file it is as cheap as LSP rename.
- For many occurrences or cross-file renames you MUST verify with a final
  `session references <path> <old>` (or grep/read) that no old name remains —
  measured failure mode is silently leaving one occurrence behind.

Never try to re-implement the rename by hand-editing each call site when an LSP
rename tool exists.",
    ),
    (
        "references",
        "minae session references <path> <old>: list a symbol's references",
        "REFERENCES — impact check before/after a rename (ADR-0029)

    session references <path> <old>

- Resolves <old> like rename (first identifier occurrence) and lists every
  reference with the definition: `path:line` (1-origin), e.g.
      2 references in 1 files:
      /abs/path.rs:4
      /abs/path.rs:1
- Use before a rename to see what will change, or after apply-based edits to
  verify nothing was missed — the T5 failure mode is a silently-left occurrence.
- The output is locations only (token-cheap): read a location with
  `session get --lines <line>:<line>`.
- Exit 1/2 as with rename.",
    ),
    (
        "check",
        "session check <path>: wait for diagnostics, returns only errors",
        "CHECK — the edit -> verify loop in one command (ADR-0032)

Use:      session check <path>
Output:   compact JSON, no file text: {\"path\", \"total\", \"diagnostics\"} where each
          diagnostic is {\"severity\", \"line\" (1-origin), \"start\", \"end\", \"message\"}.
Exit:     0 = no error diagnostics (warnings alone are fine),
          2 = at least one error diagnostic, or a retryable LSP failure,
          1 = not supported / bad input.

Rules
- After an edit (session apply / edit), run check INSTEAD of wait + get +
  JSON-parsing the snapshot: it waits for LSP diagnostics to settle and returns
  only the diagnostics — the round trip and the full text are gone.
- The 1-origin line is the address for `session get --lines <n>:<n>`; the
  char range is the address for `session apply`.<old>.
- It is an LSP fast path (incremental), not a compiler: for semantics beyond
  the LSP (borrow checker etc.) still run the real build.
- Clean files are detected by a settle budget (~10s): a cold workspace may
  return \"clean\" before analysis finishes — re-check after a moment.",
    ),
    (
        "hover",
        "session hover <path> <line>:<col>: type & signature without full text",
        "HOVER — type/signature lookup without reading (ADR-0032)

Use:      session hover <path> <line>:<col>   (1-origin, like get --lines)
Output:   compact JSON: {\"path\", \"text\"} — type + signature + doc, no file text.
          Empty text is a SUCCESS (exit 0): nothing to hover at that position
          (whitespace, comments).

Rules
- Before reading a definition to learn its type, hover the call site: the
  server returns the signature in one round trip (token-cheap).
- The doc comment is truncated (<=2000 chars) — the type/signature is the
  reliable part; read the doc only when the short part is not enough.
- Pair with session peek <path> <line>:<col> to jump to the definition.
- Exit 1/2 as with outline/at (1 = not supported / bad input, 2 = retryable).",
    ),
    (
        "symbol",
        "session symbol <path> <query>: find where a name lives in the workspace",
        "SYMBOL — workspace search instead of rg (ADR-0032)

Use:      session symbol <path> <query>
Output:   compact JSON array, no file text: each hit is
          {\"name\", \"kind\", \"path\", \"line\" (1-origin)} — read the location
          with `session get --lines`.

Rules
- <path> is any file in the workspace (it anchors the workspace root);
  results can span the whole root, not just that file.
- Use it INSTEAD of rg when you need to know WHERE a name is defined or
  declared: the server returns exact symbols (no whole lines, no false
  positive comments/strings), and the hit range is the definition.
- Fuzzy matching varies by server; a broad query then narrowing by kind/line
  is cheaper than many greps.
- Exit 1 = not supported / bad input (empty query), 2 = retryable.",
    ),
    (
        "persist",
        "session edit leaves the buffer dirty; save explicitly",
        "PERSIST — when changes hit the disk

- `session apply` (and --hunks-stdin) Save for you: after success the file on
  disk is updated.
- Raw `session edit` does NOT save: the daemon buffer changes but the disk file
  stays old (dirty=true). If you used edit, persist with:
      session exec \"Save\"
- When an edit succeeded but the buffer is dirty, minae prints a stderr note:
  \"buffer is dirty (not saved); persist with: session exec '\"Save\"'\".
- Before relying on a file's on-disk content, prefer apply (which re-opens the
  file fresh) over mixing edit + assumptions.",
    ),
    (
        "errors",
        "exit codes 0/1/2 and how to recover from rejections",
        "ERRORS — exit codes and recovery

Exit codes (applies to session apply / edit / hunks):
  0  success (applied, or no-op)
  1  usage/CLI error — fix the arguments; do not retry the same call
  2  retryable failure — old text not found, checksum/expected_text mismatch,
     or Save failed. Re-read the file fresh, then retry.

Semantic commands (rename / references / outline / at / hover / symbol / check)
share 0/1/2 but classify differently: exit 1 = not supported / invalid input
(do not retry), exit 2 = retryable (symbol not found, LSP error — re-run once
later). Exceptions:
- session at with no enclosing symbol is exit 0 (found=false), not an error —
  only the LSP request itself can fail.
- session hover with nothing at the position is exit 0 (empty text), not an error.
- session check returns exit 2 when at least one error diagnostic is present
  (warnings alone exit 0) — branch on $? without parsing the JSON.

Rejections are the editor telling you what changed:
- \"document changed since read\"      -> the file moved; re-read and retry.
- \"expected text mismatch: expected X,\n  found Y at [lo,hi)\" -> the range is not
  what you assumed; re-read THAT range (use --lines) and fix the old text.
- \"NOT FOUND: text\"                  -> the old string is not in the file; re-grep.

Recovery loop: read the reported spot with `session get --lines`, correct the
old/new, retry. Never blind-retry a rejected edit.",
    ),
];

/// `minae skill [topic]` の本体。daemon は必要としない（静的コンテンツ）。
pub fn run(topic: Option<String>) -> std::io::Result<()> {
    match topic {
        None => {
            // 索引: 1トピック1行。薄く保つことが設計要件（常時ロードしても軽い）。
            for (name, desc, _) in SKILLS {
                println!("{name:<10} {desc}");
            }
            Ok(())
        }
        Some(t) => match SKILLS.iter().find(|(name, _, _)| *name == t) {
            Some((_, _, body)) => {
                println!("{body}");
                Ok(())
            }
            None => {
                // 未知トピック: stderr に理由＋候補（エージェントは $?=1 で修正できる）
                let known: Vec<&str> = SKILLS.iter().map(|(n, _, _)| *n).collect();
                eprintln!(
                    "unknown skill topic: {t:?} (known topics: {})",
                    known.join(", ")
                );
                std::process::exit(1);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_lists_every_topic_once() {
        let mut names = SKILLS.iter().map(|(n, _, _)| *n).collect::<Vec<_>>();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(before, names.len(), "トピック名は重複しない");
        assert!(before >= 3, "索引は最低3トピック");
        // ADR-0032 の3コマンドは索引に載る（エージェントが発見できる）
        for required in ["check", "hover", "symbol"] {
            assert!(
                SKILLS.iter().any(|(n, _, _)| *n == required),
                "索引に {required} トピックが必要"
            );
        }
    }

    #[test]
    fn every_topic_has_description_and_body() {
        for (name, desc, body) in SKILLS {
            assert!(!desc.is_empty(), "{name}: 索引説明が空");
            assert!(body.len() > 100, "{name}: 内容が短すぎる");
            assert!(!body.contains('\0'));
        }
    }

    #[test]
    fn topic_lookup_by_name() {
        let found = SKILLS.iter().find(|(n, _, _)| *n == "edit");
        assert!(found.is_some(), "edit トピックが存在する");
        let (_, _, body) = found.unwrap();
        assert!(body.contains("session apply"), "本編は行動レベルの内容");
    }
}