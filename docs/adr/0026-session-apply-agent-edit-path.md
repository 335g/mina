# session apply: ヘッドレスエージェントの正規編集経路

エージェントがファイルを編集する正規手順を、1コマンドの `mina session apply <path> <old> <new>` に集約する（issue #29 / F1、docs/dev-feedback.md 第2ラウンド 報告#3 の minae ヘルパーの製品化）。`session edit` は生の [`DocumentEdit`] JSON 実行のまま残し、`session apply` がその上に正規の利用手順を載せる。

> **補足（R2/2026-08-26）**: エージェント向けの契約を1か所にまとめた表を本ADR末尾「エージェント向け契約表」に追加（dev02 README の契約表を mina 本体に移植）。`session get --lines`（トークン削減, P1）と `session info` の CLI 世代（I4）も新設。

## 動機

dev01 開発で minae ヘルパー（get → expected_text 検証 → edit → save の1コマンド化）が「無いと実質開発不能」だった（報告#3）。その摩擦を CLI 側で直接除去する:

- checksum 管理・Save の JSON 形式・位置計算（t.find + start/end）を利用者から隠す
- 疑似端末ハック（`script` で TUI open を起動し即死させる）を排除
- 新規ファイルの touch→open 2段階ハックを排除
- argv 制限（128KB）を `--whole-stdin` / `--old-file` / `--new-file` で回避

同時に、A1（位置指定編集ファサード）の未達部分 — daemon 内部の apply は insert_at に委譲されたが、エージェント側の位置計算は手書きのままだった — の現実解として、位置計算を CLI 側の1か所に集約する。

## 設計

`mina session apply`（mina-term/src/session.rs）は1回の実行で次を順に行う:

1. 接続・Hello(Headless)。以後のコマンドはすべて同じ接続（fresh read の保証）
2. `Open { path }` — 未存在パスは空ファイルを作成して再 Open（Save で新規作成される）
3. Open 応答スナップショットの `text`/`checksum` をそのまま使う（追加の GetState 不要）
4. char インデックスで old の位置を求める（マルチバイト文書でずれないよう byte→char 変換）
5. `DocumentEdit { start, end, text: new, checksum, expected_text: Some(old) }` — 検証は daemon の apply_edit（checksum + expected_text の2段、#26/B2）に委譲
6. `Save` — status が `saved` で始まることを確認

入力モード（旧 minae と同一の4形態）:

| モード | 使い方 |
|---|---|
| 位置指定置換 | `session apply <path> <old> <new>` — old の最初の出現を置換 |
| 全文置換 | `session apply <path> --whole <new>` |
| 全文置換（stdin） | `session apply <path> --whole-stdin < new.rs` |
| ファイル入力 | `session apply <path> --old-file f --new-file g` |
| 複数編集 | `session apply <path> --hunks-stdin` — JSON 配列 `[{"old","new"},…]` を stdin から |

### 複数編集 `--hunks-stdin`（ラウンドトリップ削減, e2e-01 対応）

エージェントが複数行・多数箇所を編集する際、単発 apply を N 回呼ぶと N プロセス ×（接続 + Open/Edit/Save）を消費する（e2e では 370 行を 40 回の apply で書いた）。`--hunks-stdin` は N 個の編集を **1 プロセス・1 接続で順次適用**し、**Save は最後に一度だけ**行う:

1. 接続・Hello → `Open` → 2 以降を 1 接続で実行
2. 各 hunk を順に: 直前までの適用後の現在テキストに対し `old` を検索（char 単位）→ `DocumentEdit`（checksum + `expected_text` の2段検証、チェックサムは前の編集結果で連鎖）
3. 全 hunk 成功後に一度だけ `Save`
4. 成功時は `{"applied","edits","generation","checksum"}` を JSON 出力（generation を返すことで、続く `wait <generation>` を別の `session get` なしで直接呼べる — state 呼び出し 1 回分を削減）

**失敗契約は単発 apply の一般化**: 途中の hunk で `old` 未発見・拒否があれば必ず exit 2 で終了する。Save は最後に一度だけなので **直前に適用済みの hunk もディスクへは書き込まれない**（ディスク無変更・再試行可能）。daemon のメモリ内文書は部分的に編集済みだが、次の apply の Open（ディスク再読込）で上書きされ自己修復する。未存在パス・終了コード・再試行の契約は単発 apply と同一。

プロトコルは変更しない（DocumentEdit / Save のループは既存の headless 許可リスト内で完結。generation の同梱も CLI 側の出力整形であり wire 変更なし）。

終了コード: 0 = 成功、1 = トランスポート/引数/Open 失敗、2 = 再試行可能な失敗（old 未発見・checksum/expected_text 不一致による拒否・Save 失敗）。エージェントは `$?` だけで判定し、再読み込み→再試行できる（`session edit` と同じ契約、#11）。

## 制約（#28 の範囲外判断を維持）

- headless は `Open` / `Close` に加え GetState / Save / WaitFor / DocumentEdit のみ（#13 の許可リストに Open → #28、Close → #30 を追加。選択・モード・undo/redo は従来どおり拒否）
- DocumentEdit への `path` フィールド追加（未オープン自動オープン）は引き続き範囲外 — 同期 I/O のため handle 層の段組変更が必要
- TUI 接続中のフォーカス政策（バックグラウンドオープン等）は別途判断。フォーカス変更は世代と push で他クライアントに伝播する（既存の仕組み）

## 代替案の却下

- **minae の同梱（Python スクリプトの追跡）**: 摩擦が残ったままラッパーだけ増殖する。保守対象が増え、製品の動作がラッパー依存になる。→ 摩擦を直接除去する
- **search/replace への全面移行（B1）**: 業界標準ではあるが、既存の checksum + expected_text の2段検証（B2）で局所検証は実現済み。`session apply` は old テキストを位置の代わりに使うため、実質「位置指定を隠した内容解決型」。位置指定 API の公開は `session edit` として残す

## エージェント向け契約表（R2/2026-08-26 追記）

dev02 README（tools.rs）で整備された契約を、mina 本体の `session` サブコマンドにも適用。エージェントはこの表だけで分岐できる（exit code の三値分類・失敗の意味・再試行可否を JSON パースなしで判定）。

### 終了コード（三値分類）

| code | 意味 | 再試行 | 対応コマンド |
|---|---|---|---|
| 0 | 成功・適用・no-op | — | get / apply / edit / info / wait（完了） |
| 1 | 入力・引数・トランスポート・Open・JSON 解釈エラー | 不要（修正してやり直す） | apply / edit / wait（CLI層エラー） |
| 2 | 再試行可能な失敗: old 未発見・checksum / expected_text 不一致・Save 失敗・wait タイムアウト | 必要（fresh read から） | apply / edit / wait |

エラー内容は stderr に英語（メッセージ冒頭）で、具体的な理由・値を添える（C1）。成功時 stdout は機械可読 JSON。

### read / edit 契約（`session get --lines` + `session apply`）

`session get --lines start:end` は全文の代わりに指定行を番号付きで返す（P1 トークン削減）。

| 入力 | 標準出力 | 備考 |
|---|---|---|
| `--lines` 省略 | 全文スナップショット（従来どおり、checksum 含む） | edit の checksum 取得に使用 |
| `--lines 10:20` | 10〜20行の番号付き JSON `{"n":N,"text":…}` | read の結果がそのまま `apply` の `old` に使える（P2） |
| `start` > 行数 | `note: "no lines in {start}..{end}: file has {line_count} lines"` + 空 `lines` | 説明付きゼロ結果（Q3）。無駄な再試行を防ぐ |
| `end` > 行数 | 最終行へクランプ + `note: "clamped to last line …"` | — |
| `end` 省略（`10:`） | 10行目から最終行まで | — |

`session apply <path> <old> <new>` は `old` の最初の出現を置換・検証・Save する。

### 複数編集 `--hunks-stdin`

stdin に JSON 配列 `[{"old":"…","new":"…"},…]`。

| 契約 | 内容 |
|---|---|
| 適用順序 | 配列順に順次適用（直前の適用結果にマッチ） |
| 原子性 | 途中で old 未発見・拒否なら全体を未保存・exit 2（ディスク無変更） |
| noop | `new == old` はエラーでなく正常完了（冪等リトライ可能） |
| 出力 | 成功時 `{"applied","edits","generation","checksum"}`（新 checksum を返すので続く編集に再利用可） |
| 空配列 / 不正 JSON | CLI層エラー・exit 1 |

### 永続化（H1）

`session edit` は保存しない（dirty のまま）。成功時 dirty なら stderr に1行 `note: buffer is dirty (not saved); persist with: session exec '"Save"'`。`session apply` / `--hunks-stdin` は Save まで行い dirty を解消する。