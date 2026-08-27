# tools/ab — opencode ツール制御 A/B ハーネス

opencode のエージェントを **bash 専用（native read/edit/glob/grep 無効）** に制御し、
ファイル読み書きを **mina CLI をラップした shim（`r` / `e`）に強制**して、
エディタ契約（範囲read・apply vs edit・拒否理由の有無）を実LLMで A/B 計測する。

## なぜこの形か

`opencode run` の既定エージェントはネイティブの Read/Edit ツールを持ち、モデルは
指示に従わずそちらを使う（実測で確認）。そこで `opencode.json` のカスタムエージェント
（`tools` で native ツールを false、bash のみ true）を定義し、read/edit は shim 経由で
`mina session get --lines` / `session apply` / `session edit` に必ず通す。
計測は opencode の使用量DB（`~/.local/share/opencode/opencode.db` の step-finish）から
正確な input/output tokens・cost を取る。

## 使い方

```bash
# 1 run
python3 tools/ab/ab.py run t3 A 1     # t3(apply vs edit) arm A(apply) run 1
python3 tools/ab/ab.py run t3 B 1     # arm B(positional edit)

python3 tools/ab/ab.py run t2 A 1     # t2(拒否理由) arm A(specific) 
python3 tools/ab/ab.py run t2 B 1     # arm B(generic)

python3 tools/ab/ab.py run t4 A 1     # t4(apply vs LSP rename) arm A(apply)
python3 tools/ab/ab.py run t4 B 1     # arm B(mrename = LSP rename)

# 計測だけ再計算 (DB から)
python3 tools/ab/ab.py stats t3 A 1
```

出力: `wall=…s ok=OK/FAIL input=.. output=.. billed=.. cost=$.. edits=.. rejects=.. reads=.. drift=..`

シムの環境変数（上書き可）:
- `OPENCODE_BIN` — opencode バイナリのパス
- `MAB_MODEL` — モデル id（既定 `opencode/gpt-5.4-nano`）
- `MAB_MINABIN` — mina バイナリのパス

## テスト定義

### t2: 拒否理由（C1）の回復コスト
- タスク: cfg.rs の `timeout 5000→30000` と `max_retries 3→5`。
- ドリフト注入: shim が **最初の編集成功直後** に無関係領域を外部から書き換え
  （`max_retries: 3,`→`max_retries: 4,`）。次の編集は記憶ベースの old 文字列が
  合わず拒否される。
- Arm A: 拒否理由を実物（`NOT FOUND: "…"` / expected_text mismatch）で通過。
  Arm B: shim が拒否を **汎用メッセージ**（対象文字列なし）に書き換え。
- 指標: 拒否後の回復ステップ・トークン、成功/失敗（`timeout: 30000` かつ `max_retries: 5`）。

### t3: コンテンツ解決型（apply）vs 位置指定（edit）
- タスク: f1.rs / f2.rs で `USD→JPY`、`price()→amount()` を全箇所リネーム。
- Arm A: `e <path> <old> <new>` → `mina session apply`（位置計算不要）。
  Arm B: `e <path> <documentedit-json>` → `mina session edit`（start/end を char で
  自前計算。checksum も自分で得る必要がある）。
- 指標: 成功率（`USD`/`price(` が残っていない）、編集試行・拒否回数、トークン。

### t5: LSP rename vs apply — 出現多数・複数ファイル
- 3 ファイル（utils/data/main.ts）に `USD` x12・`price` x9 を分散。A=apply ループ、B=mrename。
- 結果: A 2/5（出現取りこぼし）・B 5/5、トークン −49%・コスト −57%（LSP 優位）。
- 小タスク（t4）では均衡。**規模が実用的になると LSP が勝つ** 使い分けの境界データ。

### t4: LSP 意味 rename（mrename） vs apply
- タスク: rename.ts で `USD→JPY`・`price→amount` を全箇所リネーム。
- Arm A: `medit`（=`mina session apply`）。Arm B: `mrename <path> <old> <new>` =
  `shims/rename_shim.py`（サーバー内部で typescript-language-server を stdio 起動し
  `textDocument/rename` の WorkspaceEdit を適用）。
- 成功判定: `USD`/`price` が残っていない。コンプライアンス:`rename` 監査行あり・直接編集
  （sed -i / python replace / session apply/edit 直呼び）なし。
- 依存: `typescript-language-server`（MAB_LSP_BIN で差替可。rust-analyzer 1.98 は
  --stdio 廃止＋rename が content modified で拒否されるため非推奨）。

## 成果物

- `shims/read_shim.py` / `edit_shim.py` — read/edit の実体（モード切替・ドリフト注入・
  監査ログ）。`ab.py` が arm ごとに `bin/r` `bin/e` ラッパーを生成する。
- `ab.py` — フィクスチャ生成・workdir 構築（opencode.json 含む）・実行・計測・成功判定。
- 結果の解釈は `docs/agent-editor-ab-results.md` に追記する。

## 注記

- 計測は単一モデル（既定 nano）・小標本。中央値＋完差分離で判断（p 値は付けない）。
- shim はエージェントから見て「唯一の read/edit 手段」。これ以外（cat 等での直接読み）
  を防ぐ権限までは制御していない。
- コスト: 各 run 数十〜数百円規模。検証は安価モデルで、有意差が出たものだけ
  上位モデルで再現するのが推奨。