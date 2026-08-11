# Task for reviewer

ADVERSARIAL SECURITY / HOSTILE INPUT REVIEW (Japaneseで回答):
リポジトリ /Users/335g/dev/other/mina の daemon が「攻撃的な入力」に対してどう振る舞うかレビューしてください。前提: unix socket は単一ユーザ前提だが、同一ユーザ内で悪意あるクライアント・壊れたクライアント・大きな入力が来る状況を想定する。確認のための前提知識はファイルを読んで取得してください（mina-term/src/daemon.rs, client.rs, lsp.rs, mina-protocol/src/lib.rs, mina-lsp/src/lib.rs）。

攻撃観点:
1. NDJSON パース: 巨大な行（メモリ消費）、複数コマンドを1行に、壊れた JSON の行、
 を含まない無限ストリーム。BufReader read_line の上限はあるか。行数が文書より大きい範囲インデックス（anchor/head が usize 上限）を渡されたら。
2. Command::Open の path: 任意パス読み込み（/etc/passwd 等）で状態が変わるか、存在しないパス、ディレクトリ指定、読み取り権限なし、パスが非 UTF-8。
3. Command::Insert の text: 巨大テキスト、NUL バイト、制御文字、改行だけの挿入で行数/カーソル計算が壊れないか。
4. スナップショットの大きさ: 毎レスポンス全量送信で、巨大文書 × 高速コマンドで daemon がメモリを圧迫する経路。
5. LSP: rust-analyzer が返す診断の座標が文書外（大きな line/col）、診断 flood（大量 publishDiagnostics）、uri 不一致、JSON 型不一致。lsp_pos_to_char が line/col が範囲外のときどうなるか（パニックしないか）。
6. socket 周り: stale socket の削除競合（同時起動）、symlink 攻撃で別ファイルを消す経路（remove_file）、接続数無制限 spawn、クライアント切断を検出せずリソースリーク。
7. serde の unknown field 許容、enum タグ違い、数値オーバーフロー（isize/negative）による Scroll。

出力: 各観点ごとに 脆弱性/問題なし の判定。問題があれば 重大度（高/中/低）× 攻撃経路 × 影響 × 可能なら最小修正案。パニック経路（unwrap/expect/panic）を特に重点的に探してください。

## Acceptance Contract
Acceptance level: attested
Completion is not accepted from prose alone. End with a structured acceptance report.

Criteria:
- criterion-1: Return concrete findings with file paths and severity when applicable

Required evidence: review-findings, residual-risks

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