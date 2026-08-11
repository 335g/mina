# Task for reviewer

ADVERSARIAL ROBUSTNESS/CORRECTNESS REVIEW (Japaneseで回答):
リポジトリ /Users/335g/dev/other/mina の実装を「壊す気で」レビューしてください。動作確認は cargo test を実行しても構いません（まず read でファイルを把握してから）。ゴールはバグ・競合・データ破損経路・エラー処理漏れを具体的に見つけること。確認のための前提知識はファイルを読んで取得してください。

攻撃・探索観点:
1. daemon の並行性: 接続ごとに tokio::spawn され、Daemon 全体を Mutex で直列化。デッドロック経路、ロック取得順序の問題、await 中にロックを握ったままの箇所（tokio::sync::Mutex は await 可）がないか。Open 処理はロック外 async とのことだが、Open の LSP ensure はロックを握ったまま await していないか（daemon.rs の Command::Open 分岐を精査）。
2. undo/redo グループ: SetMode での begin_group/end_group は、Insert モード中に SetMode(Insert) が再送された場合や、モード切替せずに Insert で始まった場合のエッジケースで壊れないか。end_group が呼ばれないまま daemon が落ちたら状態はどうなるか。
3. カーソル/選択の整合性: 移動・extend・挿入後の selection 位置が文書長を超えないか。マルチバイト文字・絵文字・CRLF での char インデックス処理。
4. dirty フラグ: Open→編集→Save→編集→Undo で dirty が正しく遷移するか（mark_saved と undo の相互作用）。
5. ファイル I/O: Save と Open の競合（同時に来たら）、読み取り専用ファイル、親ディレクトリ無し、Open 中の巨大ファイル。
6. LSP 同期: didChange の順序が壊れる経路（複数コマンド連打時）、診断の stale 状態、open_document と sync の二重送信。
7. クライアント側: 応答 1 行読み飛ばしでズレる経路（Open の応答が SetViewport の応答で置き換わるというコメントの意味）、切断時に request がエラーになるか。
8. エラー処理: serde_json::from_str 失敗時 continue で読み捨てて次の行へ行く挙動、書込失敗で write_half エラー時に return して診断リーク、など。

出力: 発見した問題を 重大度（高/中/低）× 再現経路 × 影響 で列挙。実際に cargo test で失敗するものはテスト名まで。問題が見つからない観点は「問題なし+根拠」と明記。

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