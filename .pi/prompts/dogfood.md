---
description: minas のドッグフーディングループを開始/再開する（実装側=このペイン、相手=driver ペイン）
argument-hint: "[driver_repo] [goal]"
---
minas のドッグフーディングループを回して。スキル `.pi/skills/dogfooding-minas/SKILL.md` を読み、
その手順どおりに:

1. 現在のペイン/セッションを確認（`herdr pane list`）。driver の作業ディレクトリを決める:

   ```bash
   # 引数 $1 があればそれを、無ければ /tmp に使い捨てを作る（再起動で消える）
   DRIVER_REPO="${1:-$(mktemp -d /tmp/minas-dogfood-XXXXXX)}"
   echo "$DRIVER_REPO"
   ```

   実際のリポジトリを汚さないためのスクラッチなので、driver にもその旨を伝える。
   `/tmp` には `.env` が無いため認証は環境変数でしか渡せない。
   driver ペインは、**既存の空ペイン（例: label が `driver`）があればそれを使い**、無ければ
   `herdr pane split --current --direction right --cwd "$DRIVER_REPO" --no-focus` で作る。
   そのペインで Pi を起動する（既存ペインは cwd を移す必要があるので1コマンドで）:

   ```bash
   set -a; . ./.env; set +a
   herdr pane run <driver_pane> "cd $DRIVER_REPO && OPENCODE_API_KEY='$OPENCODE_API_KEY' \
     pi --tui-mode fullscreen --model $PI_PROVIDER/$PI_MODEL --thinking $PI_REASONING_LEVEL"
   ```

   **分割も pane run も呼び出し側の環境・モデルを継承しない**ので、`--model` で
   **このペインと同じモデル**にすること。Herdr 管理下のエージェント名を最初から付けたければ
   `herdr agent start <driver> --kind pi --pane <pane> -- --model "$PI_PROVIDER/$PI_MODEL" --thinking "$PI_REASONING_LEVEL"` でも可。
   起動後 `intercom list` でモデルが自分と一致すること（`unknown` や別モデルでないこと）を確認し、
   `intercom ask` で pong を取ってから次へ。
2. 私（このペイン）を実装側、相手を driver として `intercom` で自己紹介し、スキルの「Activation」文面を
   `$2`（既定: TODO HTTP API を作って壊れにくさを検証する）を埋めて送る。driver 側の働き方・報告形式は
   グローバルスキル `dogfooding-driver` が持っているので、ここでは goal と宛先だけを渡す（重複させない）。
3. 以降は driver からのフィードバックを 1 件ずつトリアージ（スキルの Triage policy）。バグは再現→修正または
   `.pi/todos` に起票、設計は回答、docs は修正、要望は実装か理由付き backlog（`.pi/todos`）。修正は必ず自分で検証し、
   コマンドと出力と exit code を添えて driver に返す。
4. 私が止めるまでループを続けて。区切りごとに「何を直したか / 見送りと理由 / backlog / 現在のビルド・
   daemon・テスト状態」を短く報告して。終了時に driver の作業ディレクトリをどうするか（`/tmp` の
   スクラッチなら残すか消すか）を私に確認して。

driver が minas を使い続けているか（`rg`/`sed` に逃げていないか）を時々確認し、逃げていたら
skill の不足を疑って補う。driver 側を手動で始めるときは、driver ペインで `/drive [goal]` を打ってもよい
（同じ `dogfooding-driver` スキルに従う）。
