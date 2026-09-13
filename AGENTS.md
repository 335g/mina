## Agent skills

### Issue tracker

GitHub Issues, via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

### Loop engineering (コスト低減の反復)

エージェントのコスト低減は **課題設定 → 検証 → 考察 → 修正 → 効果確認**のループで回す。
起点は `docs/loop/README.md` → `docs/loop/latest.md`（最新結果と次の課題設定）→
`docs/loop/method.md`（回し方・計測器 `docs/loop/l0.py`）。

### Development

Use the installed `minas` (with `minad` as its daemon) for the operations where it beats the
shell, and the harness tools for plain reads and single-hunk edits:

- **rename across files** — `minas rename <path> <old> <new>`, then `minas references` to verify
- **semantics, not text** — `minas symbol` / `references` / `at` / `hover` / `outline` to find where a name lives and what it resolves to
- **reads inside huge files** (thousands of lines) — `minas read <path> --lines a:b`
- **diagnostics** — `minas check <path>...` before claiming a change is green (`cargo` stays the final gate)

Contracts for every command, including the rejection and retry rules: `minas skill` (index) and
`minas skill <topic>`.
