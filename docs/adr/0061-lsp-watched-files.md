# ワークスペースの変更を言語サーバに通知する（`didChangeWatchedFiles` + `files.watcher`）

ドッグフーディング（2026-09-13、driver 報告 #18 / #19）で、**新しいワークスペースメンバーや
新しいモジュールディレクトリが、daemon を再起動するまで言語サーバの解析に入らない**ことを
実測した:

```
# /tmp/drvp/ws2（members = ["crates/*"]）で daemon 稼働中に crates/d を作る
sleep 20; minas symbol crates/b/src/lib.rs d_helper   -> []           # 見えない
pkill -f "minad serve"; minas symbol … d_helper      -> 1 件ヒット     # 再起動で見える
```

この状態では `workspace/symbol` / `references` / `hover` / `rename` が**黙って空を返し**、
`outline` / `at` / `check` だけが答える（構文層は生きている）。#18 の「意味層が死んだ
ように見える」劣化の説明がこれでつく。

原因は rust-analyzer 側の既定にある: `files.watcher` の既定は `"client"`（ワークスペースの
ファイル変更は**クライアントが `workspace/didChangeWatchedFiles` で通知する**前提）で、
daemon はそれを実装していなかった（ADR-0015 が「未オープンファイルの監視 +
didChangeWatchedFiles 配線」を将来課題として残していた）。加えて、埋め込み既定を
`"server"` にしても **新しいディレクトリ**は crate graph に無いため watcher の対象にもならない。

Status: accepted

## Decision

1. **埋め込み既定に `files.watcher = "server"` を入れる**（`minad/src/default_languages.toml`）。
   rust-analyzer 自身がファイルシステムを監視するので、**daemon が一度も開いていない
   ファイルの外部変更**が解析に反映される（実測: 外部で書き換えた `helper.rs` の新関数を
   `minas symbol` が見つける）。
2. **daemon が自分で書いた/消したファイルを `workspace/didChangeWatchedFiles` で通知する**
   （`LspSession::did_change_watched_files` + `notify_watched_files`）:
   - 発火点: `Save` が書き込みに成功したとき（`Changed`）と `DeletePath` が消したとき
     （`Deleted`）。どちらも daemon ロックを離してから送る（notify で解析タスクを締め出さない）。
   - **通知先の選び方**が要点: セッション鍵は `(workspace_root, language_id)` で、
     `workspace_root` は**最も近い manifest** を返す（`languages.rs`）ため cargo workspace の
     メンバーごとにセッションが分かれる。新しいメンバーは「どのセッションの root 配下でも
     ない」ので、**manifest（`Cargo.toml` / `Cargo.lock` / `package.json` / `tsconfig.json` /
     `jsconfig.json` / `pyproject.toml` / `go.mod`）の変更は全セッションへ broadcast** する。
     通常のファイルは root 配下のセッションだけに送る（`watcher_wants`）。どのセッションが
     同じ cargo workspace に属するかは `cargo metadata` を回さないと分からないので、
     manifest の broadcast が唯一正しい選び方（manifest の変更は稀）。
   - 拡張子による `server_for` の判定は**非 manifest にだけ**適用する（`Cargo.toml` は
     拡張子で LSP に紐づかない。最初の実装はここで manifest を弾いてしまい、通知が 1 件も
     出なかった）。
   - **セッションが無ければ何もしない** — 通知のために LSP を spawn しない。
   - `Save` では `Created` と `Changed` を区別しない（CLI の `apply` は新規ファイルを
     touch → Open → 保存するので、daemon から見ると既知のファイルと区別できない。
     RA はどちらの通知でも読み直し、manifest なら cargo metadata を再実行する）。

## Considered Options

- **通知だけで `files.watcher` を既定（"client"）のままにする**: 却下 — daemon が知って
  いる変更（自分の Save/Delete）しか覆えず、**外部の書き込みが一切見えない**。
  `"server"` は 1 行で「既知ディレクトリ内の外部変更」を全部拾える。
- **変更されたパスを担当するセッションだけに通知する**（root 一致で 1 つ選ぶ）: 却下 —
  新メンバーはどの root にも属さないため、まさに必要な場面で誰にも届かない
  （実測で `symbol` が `[]` のままだった）。
- **`client/registerCapability` でワークスペース root の glob（`**/Cargo.toml`）を登録し、
  外部の manifest 変更も通知する**: 保留 — ファイル監視（notify クレート等）を daemon に
  持ち込むことになり、ADR-0015 が避けた方向。**外部で新しいディレクトリを作る場合**
  （shell の `cargo new`、`mkdir`）は今も再起動まで見えない。agent の通常経路
  （minas の apply で作る）は本 ADR で覆えたので、実測が要求したら別の反復で。
- **`cargo metadata` を daemon が回して workspace のメンバーを解決する**: 却下 —
  言語サーバの仕事を daemon が二重に持つことになる（起動コスト・失敗経路・キャッシュ）。
- **daemon がワークスペースを再帰的に監視する（notify クレート）**: 却下（今は）—
  依存追加 + 監視コスト。`files.watcher = "server"` で大半が済む。

## Consequences

- **minas 経由で新しいモジュール・新しいメンバーを作ると、daemon 再起動なしで解析に入る**
  （実測: workspace に `crates/c` を `minas apply` で作り、6 秒後に
  `minas symbol crates/a/src/lib.rs c_helper` が新メンバーのシンボルを返す）。
  これは self-host の前提そのもの（mina 自身のリファクタはモジュール追加・移動の連続）。
- 外部ツール（shell の `mkdir` / `cargo new`）が**新しいディレクトリ**を作る場合は
  依然として daemon 再起動まで見えない — `.pi/todos` `2d5d12f6` に残す（範囲を絞った形で）。
- テスト: `watcher_notification_targets_manifests_and_owning_sessions`（純関数
  `watcher_wants` / `is_manifest_path`）。RA の再ロードそのものは手動の受け入れ試験
  （上の実測）で確認する — 実 rust-analyzer を回す結合テストは重すぎる。
- 通知は解析タスクを締め出さない（daemon ロック外・セッションロックは `LSP_LOCK_TIMEOUT`
  で諦める）。取りこぼしても次の書き込みで再通知される。

## 追記（2026-09-14、iteration #12 / ADR-0073）

通知の**送信を Save / DeletePath の応答経路から外した**（背景タスク化）。応答の末尾で
`session.lock()` を待っていたため、編集後の背景 pull（ADR-0055）がロックを保持している
間だけ **`minas apply` の初回が ~1.0s** になっていた（`Save` 往復 965 → 1.8ms）。
Decision の「daemon ロック外」は維持したまま、「セッションロックを待ってから応答する」
という含意だけを落とした。**Save の応答は通知済みを意味しない**（通知はロックが空いた
時点で送られる。順序は次の LSP 要求より先）。詳細と実測は ADR-0073。
