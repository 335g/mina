調査完了。以下の通り報告します。**ファイルは一切書きません**（ご指示どおり返答テキストのみで返します）。

---

# 調査レポート: スピナー付きステータス行の設計のための事実確認

## 1. クライアントのメインループ構造 (mina-term/src/client.rs)

**メインループは `tokio::select!` の2分岐のみ。コマンド応答待ちは select 分岐の中の同期的 await。**

`client.rs:236-341` のループ:
```rust
loop {
    let mut redraw = false;
    tokio::select! {
        event = events.next() => { ... }              // キー入力
        push = session.pushes.recv() => { ... }        // daemon push
    }
    if redraw { render::draw_with_cache(...)?; ... }
}
```

- **select 分岐は2つだけ**: キーイベント (`events.next()`) と daemon push (`session.pushes.recv()`)。タイマー/interval 分岐は存在しない。
- **コマンド送信は select の分岐の内側で await される**: `client.rs:~300` の `state = session.request(&command).await?;`。`Session::request` (`client.rs:437-448`) は
  ```rust
  self.responses.recv().await  // 応答1行が来るまでブロック
  ```
  で、これは**select 分岐の内側**で await されるため、応答待ちの間は select ループ全体がサスペンドする。
- **したがって、応答待ちの間にキー入力・push は「処理されない」**。キーイベントは termina の EventStream に溜まり、push は `pushes` mpsc チャネルに溜まる。応答が返って select が再開して初めて処理される。
- **タイマー追加の可否**: `interval.tick()` を第3分岐に足すのは容易だが、**それは「コマンド間」のときにだけポーリングされる**。Open 応答待ちのような in-flight コマンド中は select がサスペンドしているので、interval 分岐も tick されない。**スピナーを Open 中に動かすには、`session.request(...)` 自体を select 分岐として並列に立てる（`response = session.request(...)` と `tick = interval.tick()` の select）か、別タスクで描画駆動する改造が必要**。これが核心の改造範囲。
- **Open 応答待ち(最大10秒)中の画面**: 最後に描画したフレーム（`draw_with_cache` の出力）のまま完全に静止。入力は溜まるだけで応答後に一括反映される可能性がある（実際は通常セッション間で再描画される）。進捗表示は一切なし。

## 2. settle_open_diagnostics の厳密な終了条件 (mina-term/src/lsp.rs:822-883)

```rust
pub async fn settle_open_diagnostics(...) {
    let mut prev: Option<usize> = None;
    for i in 0..120 {                                    // lsp.rs:837 上限120回=60秒
        tokio::time::sleep(Duration::from_millis(500)).await;  // lsp.rs:838 500ms/回
        let text = { ... フォーカス移動なら return (lsp.rs:842-845) };
        let pulled = { ... サーバ死なら return (lsp.rs:851) ... };
        let Some(diags) = pulled.0 else { continue; };    // lsp.rs:860 pull None はスキップ
        let n = diags.len();
        ...
        let _ = push_tx.send((None, snap));               // lsp.rs:865
        if n > 0 {
            if prev == Some(n) { return; }                // lsp.rs:870-874 非空が2回連続・同数で抜ける
        } else if i >= 60 {
            return;                                       // lsp.rs:875-880 空が30秒続いたら抜ける
        }
        prev = Some(n);
    }
}
```

- **早期 break あり**。最大60秒フル走行は「非空だが件数が安定しない(>0のまま毎回変わる)」ケースのみ。
  - 終了条件1: **非空が2回連続で同じ件数** → `prev == Some(n)` かつ `n > 0` (lsp.rs:870-874)。空(0件)は安定判定の対象外（コメント lsp.rs:836, 866-868）。
  - 終了条件2: **空のまま30秒(60回)** → `i >= 60` で「クリーンファイル」とみなして停止 (lsp.rs:875-880)。
  - 終了条件3: フォーカスが他文書へ移動 → return (lsp.rs:842-845)。
  - 終了条件4: サーバ死 → return (lsp.rs:851)。
  - pull が `None`（キャンセル等）の場合は `continue`（push なし、prev 更新なし、i は進む）。
- **push は毎回は送られない**: `Some(diags)` が返った反復でのみ `push_tx.send((None, snap))` (lsp.rs:865)。空/None の反復では push されない。**origin は `None`**（settle はコマンドでないため）→ 全購読者へ届く（コメント lsp.rs:866-867, daemon.rs:784-792）。

## 3. Open 処理中の可視性 (mina-term/src/daemon.rs:1243-1428)

- **spawn+initialize（`lsp::ensure`）は最大10秒、daemon ロックの外で await** (daemon.rs:1365-1374、コメント「M1/ADR-0009: ロック外で行う」)。この await 中に **push は一切送られない**。push の send は `process_command` が返った後の accept_loop 側 (`daemon.rs:848-851`) でのみ発生するため、Open 処理中に他クライアントへも新情報は届かない。
- **settle の spawn タイミング**: 再利用分岐で `daemon.rs:1343-1348`、新規分岐で `daemon.rs:1416-1421`。どちらも Open ハンドラの**末尾近く・最終 snapshot の前**で spawn されるが、spawn はスケジュールするだけ（await しない）。settle は最初に 500ms sleep するので、**Open 応答（snapshot）の方が settle の最初の push より先に届く**。
- **したがって Open 処理中（特に ensure の10秒間）は、TUI は一切の新情報を得られない**。read_open_target で内容を読んだ後、ensure 待ちの間はまだ文書も開かれていない（editor.open_with_path は ensure が返った後のロック内 `daemon.rs:1376+` で行われる）。画面は完全に静止。診断も空のまま。
- 再利用分岐（既に開かれたパス）は focus_open_path が最初にあるため画面はフォーカスが変わるが、LSP init の待ちは同様。

## 4. ステータス行の右側余白 (mina-term/src/render.rs:704-780)

**draw_status の要素順序（非コマンドモード）** (`render.rs:720-764`):
```
モードチップ → 診断 [nE nW] → パス+行:列 → pending キー → msg(flash|status)
```
これらを `text` に連結し、**`truncate_wide(&mut text, width)`** (`render.rs:~756`) で右端を切り詰め、その後 mode チップだけをマーカー色で包んで `status` に出力。

- **右端にスピナーを足すと、`text` の末尾として `truncate_wide` で最初に切られる**。現状、右寄せの仕組みはない。スピナーを常時表示するには「`width - スピナー幅` まで `truncate_wide` → スピナーを append」の余白確保が必要。既存の末尾要素（msg など）が優先順位で切られる設計にするなら、スピナーを末尾に置くと長いパス/msg が先に消える（現在は診断カウントを mode 直後に移して切れないよう保護している — `render.rs:727-734` のコメント）。
- **色の仕組み**:
  - ステータス行背景: `UiRole::StatusLine` = `Style::reverse()` (`colorscheme.rs:192`)。
  - モードチップ: `ModeNormal/ModeInsert/ModeSelect`、黒fg+明るいbg（`colorscheme.rs:201-203`、`Ansi(0)`+`Ansi(12/10/13)`）。
  - スピナーに独自色を付けるなら `UiRole` enum (`colorscheme.rs:138-158`) + `DEFAULT_UI` (`colorscheme.rs:185+`) + serde lowercase の3箇所に新ロール追加が必要。`ui_style()` (`colorscheme.rs:181-184`) でロール→Style 解決。**StatusLine の reverse 背景を流用すれば enum 変更は不要**。
- **コマンドモード中の挙動**: `if let Some(buf) = command_line` (`render.rs:713-724`) で**行全体が `:` + バッファに置き換わる**。draw_status は `command_line: Option<&str>` を受け取り、Some 中は完全に別描画。**スピナーがステータス行内要素ならコマンドモード中は消える**。別行に描くか独立レンダリングしない限り、`:w` 入力中はスピナー表示されない（消えてよい/別表示の設計判断が必要）。

## 5. ヘッドレス側の閲覧経路 (mina-term/src/daemon.rs, mina-protocol/src/lib.rs)

- **GetState**: `Command::GetState` (`lib.rs:40`) は `snapshot(daemon, None)` を返すだけ (`daemon.rs:2235-2237`)。`StateSnapshot` (`lib.rs:401-429`) に `activities` フィールドを追加し、`snapshot()` (`daemon.rs:2245+`) と `Default` 実装 (`lib.rs:461+`) で埋めれば、**ヘッドレスの `GetState`/`WaitFor` は自動的に読める**（追加のみでヘッドレス側の変更は不要）。
- **WaitFor**: `daemon.rs:1218-1242`。push_tx watch に subscribe し、ループで `d.generation > generation` (`daemon.rs:1227-1230`) を確認 → 超えてなければ `rx.changed().await` (`daemon.rs:1237`) で次の状態変化を待つ。
  - **WaitFor が wake するのは「generation が増えたとき」だけ**。push は `push_tx.borrow().1.generation != snapshot.generation` のときだけ send される (`daemon.rs:847-851`)。watch の `changed()` は毎 send で発火するが、WaitFor は再び generation 比較で戻るため、**generation が増えない活動の変化では WaitFor は返らない**。
  - **settle の診断 push は generation を進めない**（コメント lsp.rs:836-837、daemon.rs:846-848）ため、現状ヘッドレス WaitFor は診断到着を検知できない。
  - **結論**: Activity を **generation が増える状態変化**として表現すれば、増減のたびに push が送られ WaitFor が wake される。Activity を診断と同様に「generation を進めない」モデルにすると、WaitFor は Activity 変化を待てない。この2択が設計の分岐点。

---

## スピナー実装への示唆（事実から導かれる要点）

1. **最大の構造障壁は Open 中の応答待ち**。現在は select 分岐の内側で `session.request().await` するため、interval 分岐を足しても in-flight 中は動かない。→ Open 中のスピナーを回すには `session.request()` を select 分岐へ昇格させる改造が必要（`client.rs:236-341` のループと `Session::request` `client.rs:437-448` が対象）。
2. スピナー色は `UiRole::StatusLine`（reverse）流用なら enum 変更不要。独自色なら `colorscheme.rs:138-204` に新ロール追加。
3. ステータス右端のスピナーは `truncate_wide` で最初に切られるため、`render.rs:756` の切り詰め幅を `width - spinner` に予約する必要がある。
4. ヘッドレス連携（Activity を WaitFor で待つ）なら **generation を進めるモデル**にする必要がある（現状の診断 push は generation 非変更で WaitFor 非対応）。

---

# acceptance-report