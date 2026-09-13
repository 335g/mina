---
description: minas のドッグフーディングループを開始/再開する（実装側=このペイン、相手=driver ペイン）
argument-hint: "[driver_repo] [profile] [model]"
---
minas のドッグフーディングループを回して。スキル `.pi/skills/dogfooding-minas/SKILL.md` を読み、
その手順どおりに:

0. **`docs/verification/dogfood-log.md` を読む**（このファイルが計測の本体）:
   - §5 の profile 表で **未実施（exercised 空）の profile** を確認し、今回の profile を決める
     （`$2` があればそれ、無ければ未実施の先頭。**同じ profile を連続で回さない**）
   - §4 の停止基準を満たしていないか確認し、最初の報告に「基準の状態」を書く
   - 前回までの行（`sw`/`dl`/`new_class`/`fallbacks`）を見て、今回何を測るかを意識する

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
   そのペインで Pi を起動する（既存ペインは cwd を移す必要があるので1コマンドで）。
   **driver のモデルは `$3` で差し替え可能**（既定はこのペインと同じ = 対照条件）:

   ```bash
   DRIVER_PROFILE="${2:-<§5 の未実施 profile>}"
   DRIVER_MODEL="${3:-$PI_PROVIDER/$PI_MODEL}"     # 別モデルにするなら $3 に provider/model
   DRIVER_THINKING="${DRIVER_THINKING:-$PI_REASONING_LEVEL}"
   set -a; . ./.env; set +a
   herdr pane run <driver_pane> "cd $DRIVER_REPO && OPENCODE_API_KEY='$OPENCODE_API_KEY' \
     pi --tui-mode fullscreen --model $DRIVER_MODEL --thinking $DRIVER_THINKING"
   ```

   **分割も pane run も呼び出し側の環境・モデルを継承しない**ので、`--model` を明示すること。
   Herdr 管理下のエージェント名を最初から付けたければ
   `herdr agent start <driver> --kind pi --pane <pane> -- --model "$DRIVER_MODEL" --thinking "$DRIVER_THINKING"` でも可。
   起動後 `intercom list` で**要求したモデルと一致する**こと（`unknown` や第三の値でないこと。
   指定が黙って効かないと log に誤った条件が記録される）を確認し、
   `intercom ask` で pong を取ってから次へ。
2. 私（このペイン）を実装側、相手を driver として `intercom` で自己紹介し、スキルの「Activation」文面を
   **profile と goal** で埋めて送る（goal は §5 のその profile の「目標」列から起こす。driver 側の
   働き方・報告形式はグローバルスキル `dogfooding-driver` が持っているので、ここでは profile・goal・
   宛先だけを渡す = 重複させない）。
3. 以降は driver からのフィードバックを 1 件ずつトリアージ（スキルの Triage policy）。バグは再現→修正または
   `.pi/todos` に起票、設計は回答、docs は修正、要望は実装か理由付き backlog（`.pi/todos`）。修正は必ず自分で検証し、
   コマンドと出力と exit code を添えて driver に返す。
4. 私が止めるまでループを続けて。区切りごとに「何を直したか / 見送りと理由 / backlog / 現在のビルド・
   daemon・テスト状態」に加えて、**計測の 6 数（`sw`/`dl`/`findings`/`new_class`/`fallbacks`/`reverify`）**
   を短く報告して（`fallbacks` は driver の round summary から取る。未申告なら 0 ではなく「未申告」と書く）。
5. 終了時: `docs/verification/dogfood-log.md` に **§3 の行 1 つ + §6 の節 1 つ**を書いてから close-out し、
   **§4 の停止基準の状態（達成 / 未達成と欠けている条件）**を明示する。driver の作業ディレクトリを
   どうするか（`/tmp` のスクラッチなら残すか消すか）も私に確認して。

driver が minas を使い続けているか（`rg`/`sed` に逃げていないか）を時々確認し、逃げていたら
skill の不足を疑って補う。driver 側を手動で始めるときは、driver ペインで `/drive [goal]` を打ってもよい
（同じ `dogfooding-driver` スキルに従う）。
