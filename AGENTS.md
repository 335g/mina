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

When referencing or editing rust files, use the installed `minas` (headless session CLI) and `minad` (daemon) for operations. For usage instructions, refer to `minas skill`.
