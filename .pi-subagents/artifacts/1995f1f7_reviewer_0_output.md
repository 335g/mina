一時テストを削除し、`git diff` クリーン・全テスト 118 件パスを確認しました。レビュー結果を報告します。

## Review

### 検証の進め方
- 全ソース（daemon/client/lsp/view/core/protocol）を精読
- `cargo test --workspace`: **118 passed**
- 疑わしい3件は一時テストを追加して実証（pass = バグ確認）→ 確認後に revert、`git diff` クリーンな状態に復元済み

---

### 高: 重大な問題

**H1. undo グループが閉じられないまま永続化する（セッションを跨いで undo が破壊される）**
- 場所: `mina-term/src/daemon.rs` `apply()` の `Command::SetMode` 分岐 / `mina-view/src/history.rs` `grouping` フラグ
- 再現経路: TUI で Insert モードのまま `q` または Ctrl-C で終了 → `SetMode(Normal)`（= `end_group`）は送られない。daemon は常駐なので `History.grouping` が `true` のまま残る。次に同じ文書を開いて Insert モードで入力すると、**前セッションの編集と同一グループに統合**され、undo 1回で複数セッション分の入力がすべて消える。加えて `SetMode(Insert) → SetMode(Select)` を経由した場合も（`end_group` 条件が `current == Insert` 限定のため）グループが閉じない。
- 影響: undo というデータ復旧手段の破壊。誤 undo で消えた分の復元も redo 1回で全部戻るため操作単位が破壊される。グループは文書の History に載るため永続。
- 実証: 一時テスト `temp_undo_group_survives_quit_in_insert_mode` / `temp_undo_group_insert_to_select_leaks_open_group` がバグ挙動を pass（その後 revert）。

**H2. CRLF ファイルの内容が描画されない（行全体が消える）**
- 場所: `mina-term/src/render.rs` `draw_line()` + `LineIndex`
- 再現経路: CRLF ファイル（`"a\r\nb\r\n"`）を開く。`LineIndex` は `\n` のみで行分割するため行内容は `"a\r"`。`draw_line` が `'\r'` をエスケープ列に**生出力** → ターミナルはカーソルを行頭へ移動 → 直後の `"\x1b[K"`（EL 0）がその行全体を消去。全行が空白に見える。
- 影響: Windows 由来のファイルを開くと内容が一切見えず、ブラインド編集になる。カーソル移動も `cursor_pos` は '\r' を幅0で数えるため、表示とカーソルのズレも発生。
- 実証: 一時テスト `temp_crlf_line_is_erased` が `before_erase == "a\r"`（生 `\r` 出力）を pass（その後 revert）。

**H3. Save と他接続の Open が競合すると `mark_saved` が誤った文書に適用される**
- 場所: `mina-term/src/daemon.rs` `Command::Save` 分岐
- 再現経路: 接続Aが Save → ロック解放 → `fs::write` await 中に接続Bが Open → フォーカス文書が切替わる → A が再ロックして `d.editor.mark_saved()` → **現在フォーカスの文書（Bが開いた方）**の dirty がクリアされる。A が保存した文書の dirty は残ったまま。逆に、dirty な文書の編集が「保存済み」扱いになる。
- 影響: 未保存編集の dirty フラグが誤ってクリアされ、ユーザーが「保存済み」と誤認して閉じるとデータ損失。`status: "saved: <旧パス>"` が別文書のスナップショットに載るので表示も矛盾。daemon は複数接続を前提に設計されており（accept_loop の spawn）、TUI + `mina session exec` の併用で到達可能。
- 根拠: コード読解。`mark_saved` は `self.view().doc`（再ロック時点のフォーカス）を対象にし、`open_with_path` はフォーカス View を新文書へ切替える。

---

### 中: 顕著な問題

**M1. LSP ensure/open_document/sync が daemon ロックを握ったまま await**
- 場所: `mina-term/src/daemon.rs` Open 分岐（`lsp::ensure(&mut d, ...).await`, `lsp::open_document(&mut d, ...).await`）、編集分岐（`lsp::sync(&mut d).await`）
- 再現経路: 初回 .rs オープンで rust-analyzer を spawn し initialize を待つ（最大10秒タイムアウト）間、および編集のたびの didChange 通知送信中、daemon 全体の Mutex を握ったまま await する。コメント「I/O コマンドはロックを握ったままブロックしないよう」が意図した設計はファイル読み込みにしか適用されておらず、LSP ハンドシェイクはロック内。
- 影響: rust-analyzer の起動・応答遅延が全接続（TUI・session CLI）の応答をブロックする。**デッドロックではない**（ロック順序は daemon → LSP pending の一方向のみ、循環なし）が、グローバルストールの原因。`notify` にタイムアウトがないため、サーバが stdin を読まなくなると永久停止になりうる（M2）。

**M2. LSP `notify`/`write_frame` にタイムアウトなし**
- 場所: `mina-lsp/src/lib.rs` `notify()` / `write_frame()`
- 再現経路: rust-analyzer がハング（または停止）して stdin パイプが満杯 → `write_all` が永久に await。`request()` には10秒タイムアウトがあるが notify にはない。
- 影響: M1 と組み合わさり、編集コマンドごとの `sync` が戻らず daemon がロック保持のまま永久停止 → 全クライアントが応答不能（クライアント側にもタイムアウトがないため TUI は無反応のまま）。

**M3. rust-analyzer 死亡後のサイレント永久死**
- 場所: `mina-term/src/lsp.rs` `ensure()`（`if daemon.lsp.is_some() { return Ok(()) }`）、`did_change()`/`did_open()` の `let _ = ...notify(...).await`
- 再現経路: LSP サーバがクラッシュ → 以後の `sync` は EPIPE で失敗するが `let _` で握り潰される。`ensure` は liveness を再確認せず再 spawn もしない。
- 影響: 以降の診断が二度と表示されず、エラーも出ない。常駐 daemon のため「壊れたまま黙り続ける」状態が永続。

**M4. TUI がエラー時にターミナルを復旧しない**
- 場所: `mina-term/src/client.rs` `run()`（`request(...).await?` の `?`）
- 再現経路: daemon が死ぬ/切断されると `request` は `read_line` が Ok(0) → 空行の parse エラー → `?` で伝播 → raw モード・代替画面・カーソル非表示のまま exit。
- 影響: シェルの表示が壊れたまま（`reset` が必要）。Drop ガードや終了処理の共有化がない。

**M5. 初回 Open の応答破棄でエラー status が消える**
- 場所: `mina-term/src/client.rs`（`let _ = request(..., first).await?`）
- 再現経路: `mina 存在しない.rs` を実行 → daemon は `status: "cannot open ..."` を返すが捨てられる。SetViewport の応答（旧文書の状態）が表示される。
- 影響: ユーザーは「開けなかった」ことを知らされず、空のスクラッチ文書を見る。さらにこのとき .rs なら LSP 起動（数秒）が済んでから表示されるため無反応時間も長い。

**M6. socket の無条件 remove による2重 daemon（スプリットブレイン）**
- 場所: `mina-term/src/daemon.rs` `serve()`（`let _ = std::fs::remove_file(path);` してから bind）
- 再現経路: daemon 稼働中にもう1つ `mina daemon serve`（または spawn 競合）→ 稼働中 daemon の socket を unlink して新 daemon が bind。旧 daemon は orphan inode で生き続け、新旧で編集状態が分岐。
- 影響: 旧 TUI は旧状態、新クライアントは新状態 → 同じファイルを別々に編集し、最後の保存が勝つ = 更新ロスト。正しい実装は bind 失敗時に stale 判定（connect 試行）してから remove。

**M7. 不正 JSON 行で応答なし continue → 送信元が永久ハング**
- 場所: `mina-term/src/daemon.rs`（`Err(_) => continue`）
- 再現経路: socket に壊れた行を書いたプロセスは、応答が返らず `read_line` で永久待ち。
- 影響: 現行クライアントからは到達不能（自前のコマンドは常に正当）だが、socket は他プロセスから書き込める信頼境界。エラー応答 or 切断が望ましい。

---

### 低: 軽微な問題

- **L1. dirty が undo で clean に戻らない**: `mina-view/src/editor.rs` `is_dirty()` の ponytail 注記どおり。Open→編集→Save→編集→Undo で文書が保存時点と同一でも dirty=true のまま。
- **L2. LSP request タイムアウトで pending マップに oneshot が残る**: `mina-lsp/src/lib.rs` `request()` は timeout 時に `pending.remove` しない。タイムアウト毎に小さなメモリリーク。
- **L3. 編集直後に旧テキスト基準の診断が残る（ゴースト診断）**: `mina-term/src/lsp.rs` `drain_into` は新診断が届かなければ既存診断を維持するため、didChange 直後は旧位置の下線が新テキストにずれて表示される（設計注記ありだが位置ズレ自体は実害）。
- **L4. Open のたびに documents/histories/paths が無制限に増加**: `mina-view/src/editor.rs` `open()`。旧文書は削除されない。長期稼働 daemon のメモリ増加。
- **L5. スクロールでカーソルが viewport 外に出るとカーソルが先頭行に描画される**: `mina-term/src/render.rs` `term_row` の `saturating_sub` で 1 に飽和。見た目のみ。

---

### 問題なし + 根拠（観点別）

- **デッドロック**: なし。ロックは `daemon Mutex → LSP pending` の一方向のみ、LSP reader タスクは daemon ロックを取らない。循環経路なし（ただし liveness 問題は M1/M2）。
- **didChange の順序**: 問題なし。全編集コマンドは同一ロック下で `apply → sync` を逐次実行し、`version` もロック下でインクリメント。複数接続が並行しても didChange の送信順はコマンド処理順と一致。二重送信もなし（didOpen は Open 時のみ、sync は `current_uri` 一致時のみ、非 .rs は早期 return）。
- **カーソル/選択が文書長を超える経路**: なし。`movement.rs` は全移動を [0, len] にクランプ、`transaction.rs` の `map_pos` は削除範囲内を削除開始点へ詰め、`Goto` は 0/len_chars。undo/redo の選択復元は記録時点の状態と整合。`head == len_chars` の `char_to_line` は有効（`goto_end_scrolls_viewport` テストで確認済み）。
- **マルチバイト・絵文字**: 問題なし。全位置は char インデックス、移動/削除は書記素境界。結合文字・サロゲートのテストあり。CRLF のカーソル位置は範囲外にならない（H2 の描画問題は別）。
- **ファイル I/O**: 読取失敗・書込失敗（read-only・親ディレクトリなし）は status で報告され `mark_saved` は呼ばれない。巨大ファイルは O(n) だがクラッシュ経路なし。ただし **Save/Open 競合の mark_saved 誤適用は H3 のとおり**。
- **切断時**: daemon は read 0/エラーで return し状態は保持（意図どおり）。クライアント側の復旧は M4 のとおり欠落。

---

### 検証ログ
- `cargo test --workspace` → **118 passed**（検証前後とも。一時テストは revert 済み、`git diff` クリーン）
- 実証済みバグ: H1（2経路）、H2 — 一時テストで pass を確認後、リポジトリを元に戻した