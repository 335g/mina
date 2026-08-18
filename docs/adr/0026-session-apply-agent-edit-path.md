# session apply: ヘッドレスエージェントの正規編集経路

エージェントがファイルを編集する正規手順を、1コマンドの `mina session apply <path> <old> <new>` に集約する（issue #29 / F1、docs/dev-feedback.md 第2ラウンド 報告#3 の minae ヘルパーの製品化）。`session edit` は生の [`DocumentEdit`] JSON 実行のまま残し、`session apply` がその上に正規の利用手順を載せる。

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

終了コード: 0 = 成功、1 = トランスポート/引数/Open 失敗、2 = 再試行可能な失敗（old 未発見・checksum/expected_text 不一致による拒否・Save 失敗）。エージェントは `$?` だけで判定し、再読み込み→再試行できる（`session edit` と同じ契約、#11）。

## 制約（#28 の範囲外判断を維持）

- headless は `Open` / `Close` に加え GetState / Save / WaitFor / DocumentEdit のみ（#13 の許可リストに Open → #28、Close → #30 を追加。選択・モード・undo/redo は従来どおり拒否）
- DocumentEdit への `path` フィールド追加（未オープン自動オープン）は引き続き範囲外 — 同期 I/O のため handle 層の段組変更が必要
- TUI 接続中のフォーカス政策（バックグラウンドオープン等）は別途判断。フォーカス変更は世代と push で他クライアントに伝播する（既存の仕組み）

## 代替案の却下

- **minae の同梱（Python スクリプトの追跡）**: 摩擦が残ったままラッパーだけ増殖する。保守対象が増え、製品の動作がラッパー依存になる。→ 摩擦を直接除去する
- **search/replace への全面移行（B1）**: 業界標準ではあるが、既存の checksum + expected_text の2段検証（B2）で局所検証は実現済み。`session apply` は old テキストを位置の代わりに使うため、実質「位置指定を隠した内容解決型」。位置指定 API の公開は `session edit` として残す