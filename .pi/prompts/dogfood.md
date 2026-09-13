---
description: minas のドッグフーディングループを開始/再開する（実装側=このペイン、相手=driver ペイン）
argument-hint: "[driver_repo] [goal]"
---
minas のドッグフーディングループを回して。スキル `.agents/skills/dogfooding-minas/SKILL.md` を読み、
その手順どおりに:

1. 現在のペイン/セッションを確認（`herdr pane list`、`intercom list`）。driver ペインが無ければ
   `$1`（既定: `/Users/335g/dev/other/sample-rust2`）で分割して pi を起動し、両ペインに名前を付ける。
2. 私（このペイン）を実装側、相手を driver として `intercom` で自己紹介し、スキルの Driver brief を
   `$2`（既定: TODO HTTP API を作って壊れにくさを検証する）を埋めて送る。相手の返信で疎通確認。
3. 以降は driver からのフィードバックを 1 件ずつトリアージ（スキルの Triage policy）。バグは再現→修正または
   issue 化、設計は回答、docs は修正、要望は実装か理由付き backlog。修正は必ず自分で検証し、
   コマンドと出力と exit code を添えて driver に返す。
4. 私が止めるまでループを続けて。区切りごとに「何を直したか / 見送りと理由 / backlog / 現在のビルド・
   daemon・テスト状態」を短く報告して。

driver が minas を使い続けているか（`rg`/`sed` に逃げていないか）を時々確認し、逃げていたら
skill の不足を疑って補う。
