# ヘッドレス接続も接続別 View を持つ（並行 apply が別ファイルを壊す経路を閉じる）

ADR-0037 は「**ロード = 共有・フォーカス = 接続別**」を決め、Interactive 接続に専用の
View（doc / selection / first_line）を与えた。同時に「**ヘッドレスは View を持たない**」
（操作対象パスは明示され、応答は target のスナップショット）としたため、ヘッドレスの
接続は**全員が共有 idle view** を操作対象にしていた。

ところが agent 側の編集経路は、パスを明示していない:

- `minas apply` は 1 接続で `Open → DocumentEdit → Save` を送る（`minas/src/session.rs`）
- daemon は `Open` で**今フォーカスしている View の doc** を開き直し、
  `DocumentEdit` は `current_document()`（= 今フォーカスしている文書）に当たり、
  `Save` は `focused_path()`（= 同じ）へ書き戻す（`process_command` 冒頭の `focus_for`）

つまり「操作対象パス明示」は実際には **共有状態を経由した暗黙の指定**で、ヘッドレス同士が
同じ idle view の doc ポインタを奪い合う。実測（ドッグフーディング #1、並行 5 ファイル作成）:

- 意図したパスが **0 バイトのまま `applied: … (whole file, 9 chars)` / exit 0** で返る
  （編集は別の接続が開いたファイルへ書かれた）。15 ファイル並列で 1 件、追加の
  15 ファイルで 1 件 — 再現率は高い
- 新規パスに対して **`EDIT REJECTED: document changed since read`**（一度も存在したことの
  ないパスなので、文書という概念が無い）
- 空ファイル同士は checksum が同一なので、**checksum ガードでは区別できない**
  （ガードは「同じ文書の変更」を検出する設計で、「別の文書を掴んだ」ことは検出しない）

Status: accepted（ADR-0037 の「ヘッドレスは View を持たない」を改める。ロードの共有は不変）

## Decision

1. **ヘッドレス接続にも専用 View を割り当てる**（`handle_connection` の Hello 処理で
   `register_conn_view` を kind に関わらず呼ぶ）。ヘッドレスは自分の View の doc を
   編集・保存するので、`Open → DocumentEdit → Save` は**同じ接続の同じ文書**に閉じる。
2. ロード（文書集合・undo 履歴・LSP セッション）は従来どおり共有。**フォーカスだけ**が
   接続別になる（ADR-0037 の原則をヘッドレスにも適用しただけ）。
3. 共有 idle view は `open_idle_follow` で Open に追従し続ける
   （「パス無し GetState = 最後に開かれた文書」の観測点は不変）。
4. 接続の View は切断で破棄される（`drop_conn_view` は既に無条件呼び出し）。
   ヘッドレスのワンショット接続でも View が残らない。

## Considered Options

- **`DocumentEdit` / `Save` にパスを足す**: 却下（今すぐは）— プロトコル変更
  （`PROTOCOL_VERSION` bump）になり、driver の再検証ラウンドが 1 回増える。
  「操作対象パス明示」の意味論としては正しいが、接続別 View で同じ保証が
  プロトコルを変えずに得られる。将来、複数文書を 1 接続で扱う必要が出たら再検討する。
- **並行 apply をロックで直列化する**: 却下 — `Open`…`Save` は別メッセージなので、
  daemon が 1 つの編集トランザクションだと知る手段が無い（知るには結局プロトコル追加）。
  直列化しても「別の文書を掴む」構造は残る。
- **ヘッドレスを全部 Interactive 扱いにする**: 却下 — push 購読・カーソルリセット
  （ADR-0027）・コマンド制限（#13）はヘッドレスの契約として別物。

## Consequences

- 並行 apply（同一 daemon に N プロセス）で 0 バイト / 偽の `document changed` が出ない。
  回帰テスト: `concurrent_headless_edits_do_not_steal_the_other_connections_document`
  （空ファイル 2 つ + 2 接続。修正前は一方のファイルが 0 バイトのまま `saved` が返る）。
- **ヘッドレスは自分が Open していない文書を暗黙に編集しなくなる**。既存テスト 3 件は
  「Interactive が開いた文書をヘッドレスがそのまま編集する」前提だったので、ヘッドレス側に
  `Open`（同一パスの再 Open = 文書を再利用する。#7）を足す形へ更新した。
  `interactive_client_receives_push_of_headless_edit` / `activity_reaches_subscribers_through_push` /
  `headless_client_is_restricted_to_document_edit_family`（後者は基準世代を
  「ヘッドレスの Open 後」に取る）。
- 影響はこの 3 テストのみ（ワークスペース全体 488 passed）。
- 残る穴: ヘッドレスが Open せずに `DocumentEdit` を送ると、自分の View の初期文書
  （= 接続時の idle doc、通常は空スクラッチ）に当たる。これは**失敗ではなく**
  「自分が開いていない文書は編集対象にならない」という新しい契約で、`minas` の
  apply 経路は常に Open するので実害は無い。プロトコルで強制するなら
  「Open していない接続の DocumentEdit は拒否」が将来の選択肢（backlog なし・必要になったら）。
