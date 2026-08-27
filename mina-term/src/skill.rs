//! `mina skill` — エージェント向けの軽量スキル（判断・手順の参考書）。
//!
//! 設計（docs/agent-editor-ab-results.md の実測に基づく）:
//! - `mina skill`（無引数）= **索引**。1トピック1行・約10行。薄く保ち、常時ロードしても
//!   トークン負担が小さい（T1: 範囲read vs 全文read で −51% の実測と同じ原理 —
//!   必要になったトピックだけを読む）。
//! - `mina skill <topic>` = **そのトピックの内容だけ**を返す。ロードオンリー・必要時のみ。
//! - 索引と内容を分離し、モデルが「発見（安い索引）→ 必要分だけ参照（安い内容）」できるようにした。
//!
//! 契約（session と統一）: 成功 = exit 0（stdout に内容）、未知トピック = exit 1（stderr に
//! 英語の理由＋利用可能トピック一覧）。エージェントは $? と stderr だけで制御できる。
//! 内容は英語（H4: エージェントがパースする出力は英語統一）。
//!
//! 各トピックの内容はツール判断の指針であり、tools/ab の A/B 実測（t1〜t5）に基づく。

/// トピック定義。説明は索引行に使う。内容は「行動レベル」に書く（長手順・抑制・判断表）。
const SKILLS: &[(&str, &str, &str)] = &[
    (
        "read",
        "read files by line range; never dump whole files",
        "READ — reading files without wasting tokens

Use:      session get --lines <start>:<end>
Output:   numbered lines: {\"n\":1500,\"text\":\"...\"}  (1-origin, end may be empty = last line)

Rules
- Read only the lines you need (measured: ranged read cuts task tokens by ~51% vs
  whole-file reads).
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
- One invocation replaces the FIRST occurrence of <old>. Repeat for the next one,
  or batch many changes with --hunks-stdin (faster: ~3x on one connection).
- Small targeted replacements beat one huge <old> block: a big mismatch wipes too
  much. For whole-file rewrites use --whole-stdin.
- On rejection (exit 2) the message names the expected/found text and the range.
  Re-read that spot fresh, fix, retry — do not blind-retry.",
    ),
    (
        "rename",
        "use LSP rename (mrename) for many occurrences; apply for a few",
        "RENAME — semantic rename vs apply (measured decision)

If your tool exposes an LSP rename (e.g. mrename <path> <old> <new>):
- Use it for renames with MANY occurrences or MULTIPLE files. Measured (3 files,
  21 occurrences): LSP rename 5/5 success vs apply loop 2/5 (apply kept missing
  occurrences), ~half the tokens (-49%) and cost (-57%).
- It renames the definition and ALL references (imports, calls) in one call — no
  occurrence-counting, no missed references.

If only content-editing is available (session apply):
- For a handful of occurrences in ONE file, loop with apply. Measured on a small
  file it is as cheap as LSP rename.
- For many occurrences or cross-file renames you MUST verify with a final grep/
  read that no old name remains anywhere — measured failure mode is silently
  leaving one occurrence behind.

Never try to re-implement the rename by hand-editing each call site when an LSP
rename tool exists.",
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
- When an edit succeeded but the buffer is dirty, mina prints a stderr note:
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

Rejections are the editor telling you what changed:
- \"document changed since read\"      -> the file moved; re-read and retry.
- \"expected text mismatch: expected X,\n  found Y at [lo,hi)\" -> the range is not
  what you assumed; re-read THAT range (use --lines) and fix the old text.
- \"NOT FOUND: text\"                  -> the old string is not in the file; re-grep.

Recovery loop: read the reported spot with `session get --lines`, correct the
old/new, retry. Never blind-retry a rejected edit.",
    ),
];

/// `mina skill [topic]` の本体。daemon は必要としない（静的コンテンツ）。
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