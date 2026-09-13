# ファイル削除（`minas delete`）— 編集面が作る・書く・消すを覆う

ドッグフーディング（2026-09-13、driver 報告 #8）で、minas の編集面に**穴**があることが
分かった: `apply` は作成も変更もできる（`--whole-stdin` は新規ファイルを作り、親
ディレクトリも掘る）が、**削除だけが無い**。`apply --whole-stdin` に空を流しても 0 バイトに
切り詰めるだけで、孤児ファイルは残る。

driver が shell に落ちたのはこの 1 箇所だけで、しかも**モジュール移動**という通常の
リファクタで起きた（`src/router.rs` → `src/router/mod.rs` は削除を伴う）。self-host
（mina 自身の開発）ではモジュールの移動・分割が日常なので、この穴はそのまま毎回
`rm` に落ちることになる。

Status: accepted

## Decision

`Command::DeletePath { path }`（daemon）と `minas delete <path>`（CLI）を追加する。

- **応答は既存のスナップショット**（新しい `ServerMessage` を作らない）。結果は
  `status` の 1 行で表す:
  - `deleted: <path>` — 削除した（CLI は stdout に同じ 1 行、exit 0）
  - `no-op: <path> was already absent` — 不在（CLI は stderr に `NO-OP:`、exit 0。
    `rm -f` と同じで、繰り返し走るスクリプトを失敗させない。ただし黙って成功にせず
    status で区別する）
- **拒否の規律は書込みと同じ**（新しい権限は増やさない）:
  - `read-only-base: …` — 比較用の基準 root 配下（既存の `base_reject` を再利用）。exit 1
  - `cannot delete <path>: not a regular file` — ディレクトリ・FIFO 等。exit 1
  - `delete-rejected: <path> has unsaved edits in the daemon's buffer` — 開いている
    文書に未保存編集がある。**exit 2**（`Close` すれば通るので再試行可能）。
    ADR-0059（保存前の外部変更検知）と同じ規律: 黙って編集を捨てない。
- **daemon 側の後始末**: 開いている文書を `Editor::close_document(doc_id)` で閉じ
  （フォーカスは動かさない）、`outlines` / `hints` / `diagnostics` / `baselines` の
  該当パスを落とし、`deleted` 状態を立て、`EventKind::DeletePath` を記録する。
  閉じた文書の syntax キャッシュは次の `syntax_highlights` が live 判定で落とす。
- **headless 許可リストに追加**（`minas delete` は headless から使う）。書込みガードは
  パス基準で発信元を問わないため、権限は広がらない。
- `EventKind::DeletePath` を追加（テキスト編集の `Delete` = 文書内の削除とは別種別。
  イベント購読側が「ファイルが消えた」と「行が消えた」を混同しない）。
- **PROTOCOL_VERSION を 22 → 23** に上げる（コマンドの追加。ADR-0039 の bump 方式）。

## Considered Options

- **`minas apply <path> --delete`**（apply のフラグ）: 却下 — apply は
  「old を new に置換する」content-addressed な契約で、`--delete` は old/new を
  条件付きにする（引数の意味が実行時に変わる）。削除は `apply` の特殊形ではなく
  独立した操作で、CLI 側の実装も別（作成→Open→replace→Save のダンスが不要）。
- **CLI だけで削除する**（daemon を介さない）: 却下 — daemon が文書・outline キャッシュ・
  ベースライン・`deleted` 状態を保持しているため、外からファイルを消すと
  「開いているがディスクに無い」状態が残り、次の `check`/`apply` が消えたファイルを
  相手に動く（watch_disk の自己修復に任せるのは遅い)。状態を持つ側が消す。
- **不在をエラーにする**: 却下 — モジュール移動を繰り返すスクリプトで 2 回目が失敗する。
  `rm -f` の慣習に合わせ、NO-OP を成功として返しつつ status で区別する。
- **未保存編集があっても削除する（警告だけ）**: 却下 — CLI の出力に依存して警告が
  埋もれる。拒否して `Close` を促す方が、失うものが無い（バッファは削除対象の
  ファイルの内容なので、保存してから消すか、閉じて消すかを選ばせる）。
- **`mv` も同時に追加**: 保留 — 移動は「新規作成 + 削除」で既に表現できる
  （`minas read <old> | minas apply <new> --whole-stdin` + `minas delete <old>`）。
  2 ステップであることが問題になる実測が出たら、そのとき `--move` を検討する。

## Consequences

- 編集面が「作る・書く・消す」を覆い、モジュール移動が minas だけで完結する
  （driver の `fallbacks` から shell の `rm` が消える）。
- 削除後に `check --crate-root` の展開が孤児を拾わなくなる（diskc から消えるため）。
- テスト: `delete_path_removes_the_file_and_is_idempotent`（開文書のクローズ +
  不在の NO-OP）、`delete_path_refuses_unsaved_buffer_edits_and_directories`、
  `delete_exit_codes_distinguish_input_errors`。
- 移動（`mv`）は 2 ステップのまま — 計測が要求したら別の反復で。
