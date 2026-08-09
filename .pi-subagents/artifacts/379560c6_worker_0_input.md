# Task for worker

You are a delegated subagent running from a fork of the parent session. Treat the inherited conversation as reference-only context, not a live thread to continue. Do not continue or answer prior messages as if they are waiting for a reply. Your sole job is to execute the task below and return a focused result for that task using your tools.

Task:
Helixエディタ(https://github.com/helix-editor/helix)のアーキテクチャを一次情報(ソースコード本体)から調査し、Markdownノートとして保存してください。手順: 1) git clone --depth 1 https://github.com/helix-editor/helix /tmp/helix-research で浅くクローンする(ディレクトリが既にあれば rm -rf して再取得)。2) ルートの Cargo.toml で workspace メンバー(crate一覧)を確認する。各 crate(helix-core, helix-view, helix-term, helix-lsp, helix-loader, helix-dap, helix-vcs など)の役割を、src/ のモジュール構成と主要ファイルの doc コメント・型定義から特定する。3) 以下を重点的に読んで中核を理解する: helix-term/src/application.rs(イベントループ/Compositor起動)、helix-view の Compositor trait と Component trait(UI合成モデル)、helix-core の document.rs(バッファ), selection.rs(複数カーソル), movement.rs と文字データ構造(ropey?)、helix-loader の構文ハイライト(tree-sitter)とgrammarロード、helix-lsp のクライアント構造。4) 依存関係: 各Cargo.toml の主要依存(ropey, tree-sitter, tokio, crossterm, tui/ratatui系, nucleo など)を箇条書きに。5) docs/ や book/ にアーキテクチャ解説がないか確認し、あれば参照。6) 調査結果を日本語で /Users/335g/dev/other/mina/docs/helix-architecture.md に保存。構成: (a) Helixの全体像と設計思想(モーダル編集, tree-sitter構造編集, LSP統合, async)、(b) crate構成と責務分担、(c) 主要モジュールと型(Document, Selection, Compositor, Component, イベントループ)、(d) 主要依存ライブラリと用途、(e) Helixのようなエディタを作る際に検討すべき事項(バッファデータ構造, レンダリング, 入力処理, モーダルキーマップ, ハイライト, LSP, 非同期実行モデル)、(f) 各セクションに出典(実在のファイルパス)を記載。7) 最後に保存したファイルパスと要点5行以内を出力として返す。推測ではなく読んだコードに基づくこと。

## Acceptance Contract
Acceptance level: checked
Completion is not accepted from prose alone. End with a structured acceptance report.

Criteria:
- criterion-1: Implement the requested change without widening scope

Required evidence: changed-files, tests-added, commands-run, residual-risks, no-staged-files

Finish with a fenced JSON block tagged `acceptance-report` in this shape:
Use empty arrays when no items apply; array fields contain strings unless object entries are shown.
`criteriaSatisfied[].status` must be exactly one of: satisfied, not-satisfied, not-applicable.
`commandsRun[].result` must be exactly one of: passed, failed, not-run.
`manualNotes` and `notes` are optional strings; an empty string means no note and does not satisfy `manual-notes` evidence.
```acceptance-report
{
  "criteriaSatisfied": [
    {
      "id": "criterion-1",
      "status": "satisfied",
      "evidence": "specific proof"
    }
  ],
  "changedFiles": [
    "src/file.ts"
  ],
  "testsAddedOrUpdated": [
    "test/file.test.ts"
  ],
  "commandsRun": [
    {
      "command": "command",
      "result": "passed",
      "summary": "short result"
    }
  ],
  "validationOutput": [
    "validation output or concise summary"
  ],
  "residualRisks": [
    "none"
  ],
  "noStagedFiles": true,
  "diffSummary": "short description of the diff",
  "reviewFindings": [
    "blocker: file.ts:12 - issue found, or no blockers"
  ],
  "manualNotes": "anything else the parent should know"
}
```