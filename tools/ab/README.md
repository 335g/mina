# tools/ab — opencode ツール制御 A/B ハーネス

opencode のエージェントを **bash 専用（native read/edit/glob/grep 無効）** に制御し、
ファイル読み書きを **minae CLI をラップした shim（`r` / `e`）に強制**して、
エディタ契約（範囲read・apply vs edit・拒否理由の有無）を実LLMで A/B 計測する。

## なぜこの形か

`opencode run` の既定エージェントはネイティブの Read/Edit ツールを持ち、モデルは
指示に従わずそちらを使う（実測で確認）。そこで `opencode.json` のカスタムエージェント
（`tools` で native ツールを false、bash のみ true）を定義し、read/edit は shim 経由で
`minae session get --lines` / `session apply` / `session edit` に必ず通す。
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
- `MAB_MINABIN` — minae バイナリのパス

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
- Arm A: `e <path> <old> <new>` → `minae session apply`（位置計算不要）。
  Arm B: `e <path> <documentedit-json>` → `minae session edit`（start/end を char で
  自前計算。checksum も自分で得る必要がある）。
- 指標: 成功率（`USD`/`price(` が残っていない）、編集試行・拒否回数、トークン。

### t5: LSP rename vs apply — 出現多数・複数ファイル
- 3 ファイル（utils/data/main.ts）に `USD` x12・`price` x9 を分散。A=apply ループ、B=mrename。
- 結果: A 2/5（出現取りこぼし）・B 5/5、トークン −49%・コスト −57%（LSP 優位）。
- 小タスク（t4）では均衡。**規模が実用的になると LSP が勝つ** 使い分けの境界データ。

### t4: LSP 意味 rename（mrename） vs apply
- タスク: rename.ts で `USD→JPY`・`price→amount` を全箇所リネーム。
- Arm A: `medit`（=`minae session apply`）。Arm B: `mrename <path> <old> <new>` =
  `shims/rename_shim.py`（サーバー内部で typescript-language-server を stdio 起動し
  `textDocument/rename` の WorkspaceEdit を適用）。
- 成功判定: `USD`/`price` が残っていない。コンプライアンス:`rename` 監査行あり・直接編集
  （sed -i / python replace / session apply/edit 直呼び）なし。
- 依存: `typescript-language-server`（MAB_LSP_BIN で差替可）。rust-analyzer は
  t4/t5 では「content modified」で拒否されたが、M0 実測で**ロード未完了が原因**と判明
  （テストクレートが親 workspace にネストされワークスペースロードに失敗していた）。
  rust-analyzer 1.98 の rename は正常動作し、minae 本体の `session rename`（t9）で使用。

### t9（M2）: minae 本体の `session rename` vs apply — Rust で T5 を再現
- T5 の Rust 版フィクスチャ（3 ファイル crate: utils/data/main.rs、`USD` x15・`price` x9）。
- Arm A: `medit`（=`minae session apply`）。Arm B: `mrename <path> <old> <new>` =
  `shims/rename_minae_shim.py`（`minae session rename` のパススルー。ADR-0029）。
- 結果: A 3/5（`price` 取りこぼし）・B 5/5、トークン −61.6%・コスト −61%・wall −56%
  （製品化 rename が shim と同等以上の優位を実 rust-analyzer で再現）。
- 注意: mrename はコールド時に daemon + rust-analyzer の起動・ロード待ち（数秒〜10 秒）が
  乗る。run 間は `pkill -f "target/debug/minae daemon"` で daemon を落としてから実行する。

### t10（Stage 4）: TS 版 T9 — typescript-language-server 経由の `session rename` vs apply
- T9 の TypeScript 版フィクスチャ（3 ファイル: utils/data/index.ts、`USD` x11・`price` x9）。
- Arm A: apply ループ / Arm B: `mrename`（minae `session rename` → typescript-language-server）。
- 成功判定: `USD` / `price` が全 .ts に残っていない（クロスファイルの import も対象）。
- 実装済みの動作検証: 2 ファイル（import 跨ぎ）で references 5 件 / rename 2 ファイル 5 編集
  （tsserver は閉じたファイルを null で拒否するため keep-open 方式 — ADR-0030）。

### t11: 汎用2ファイル機能追加タスク — naive 契約 vs minae session 契約
- LSP を使わない「読む→理解→直す」の実務タスク。フィクスチャ: 単体コンパイルできる
  Rust クレート（config.rs / main.rs、各 ~600 行）に `max_conns` を追加する
  （struct フィールド + Default + decode + validate + main の env 読込の 5 編集）。
- Arm A（素朴）: `mread` = 全文ダンプ（read_naive_shim、行番号なし・範囲不可）、
  `medit` = 無検証の先頭置換（edit_naive_shim、checksum なし・存在確認のみ）。
- Arm B（minae）: `mread` = 番号付き範囲 read（`session get --lines`）、
  `medit` = `session apply`（Open→検証→Save 一体、NOT FOUND で exit 2）。
- 仕掛け: 全文 read のトークン圧迫 / drift 注入（最初の成功編集直後に decode 構文行を
  外部書き換え — 両 arm の次編集の anchor が古くなる。回復コストが差になる）。
- 成功判定: 5 編集の文字列検査 + **`cargo check --offline` が green**（ハーネス側で実行）。
- 実行: `python3 tools/ab/ab.py run t11 A 1..5` / `run t11 B 1..5`。

## 成果物

- `shims/read_shim.py` / `edit_shim.py` / `rename_minae_shim.py` — read/edit/rename の
  実体（モード切替・ドリフト注入・監査ログ）。`ab.py` が arm ごとに `bin/r` `bin/e`
  `bin/mrename` ラッパーを生成する。
- `ab.py` — フィクスチャ生成・workdir 構築（opencode.json 含む）・実行・計測・成功判定。
- 結果の解釈は `docs/benchmarks/agent-editor-ab-results.md` に追記する。

## 注記

- 計測は単一モデル（既定 nano）・小標本。中央値＋完差分離で判断（p 値は付けない）。
- shim はエージェントから見て「唯一の read/edit 手段」。これ以外（cat 等での直接読み）
  を防ぐ権限までは制御していない。
- コスト: 各 run 数十〜数百円規模。検証は安価モデルで、有意差が出たものだけ
  上位モデルで再現するのが推奨。