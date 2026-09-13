---
description: minas のドッグフーディングループを開始/再開する（実装側=このペイン、相手=driver ペイン）
argument-hint: "[driver_repo] [goal]"
---
minas のドッグフーディングループを回して。スキル `.pi/skills/dogfooding-minas/SKILL.md` を読み、
その手順どおりに:

1. 現在のペイン/セッションを確認（`herdr pane list`、`intercom list`）。driver ペインが無ければ
   `$1`（既定: `/Users/335g/dev/other/sample-rust2`）で分割し、そのペインで
   `pi --tui-mode fullscreen --model "$PI_PROVIDER/$PI_MODEL" --thinking "$PI_REASONING_LEVEL"` を実行して
   pi を起動、両ペインに名前を付ける（Herdr 管理下のエージェントとして登録したい場合は
   `herdr agent start` でも可 — 起動方法の検証記録はスキル参照）。
   **分割は呼び出し側の環境もモデルも継承しない**ので、`. ./.env` してから
   `herdr pane split ... --env "OPENCODE_API_KEY=$OPENCODE_API_KEY"` で認証情報を渡し、
   `--model` で**このペインと同じモデル**にする。
   起動後 `intercom list` でモデルが自分と一致すること（`unknown` や別モデルでないこと）を確認し、
   `intercom ask` で pong を取ってから次へ。
2. 私（このペイン）を実装側、相手を driver として `intercom` で自己紹介し、スキルの「Activation」文面を
   `$2`（既定: TODO HTTP API を作って壊れにくさを検証する）を埋めて送る。driver 側の働き方・報告形式は
   グローバルスキル `dogfooding-driver` が持っているので、ここでは goal と宛先だけを渡す（重複させない）。
3. 以降は driver からのフィードバックを 1 件ずつトリアージ（スキルの Triage policy）。バグは再現→修正または
   `.pi/todos` に起票、設計は回答、docs は修正、要望は実装か理由付き backlog（`.pi/todos`）。修正は必ず自分で検証し、
   コマンドと出力と exit code を添えて driver に返す。
4. 私が止めるまでループを続けて。区切りごとに「何を直したか / 見送りと理由 / backlog / 現在のビルド・
   daemon・テスト状態」を短く報告して。

driver が minas を使い続けているか（`rg`/`sed` に逃げていないか）を時々確認し、逃げていたら
skill の不足を疑って補う。driver 側を手動で始めるときは、driver ペインで `/drive [goal]` を打ってもよい
（同じ `dogfooding-driver` スキルに従う）。
