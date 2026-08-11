# セキュリティレビュー: mina daemon の敵対的入力耐性

前提の確認: `cargo test --workspace` は全 118 テスト成功。wire 型 (`mina-protocol/src/lib.rs`) は daemon/client 双方が参照し、`Range`(anchor/head) は**出力専用**で、クライアントから範囲インデックスを受け取るコマンドは存在しない。以下、観点ごとの判定。

---

## 観点1: NDJSON パース

### 1a. `read_line` の上限なし → **脆弱性（中）**
- 経路: `mina-term/src/daemon.rs:73-75` — `BufReader::read_line` は改行まで**無制限に** `String` へ追記する。改行を含まない無限ストリーム（または巨大な1行）を送るクライアントで行バッファが無制限に成長 → OOM。壊れたクライアント（送信バグ）でも同じ経路。
- 影響: daemon プロセス全体のメモリ枯渇（同一ユーザ内 DoS）。
- 最小修正: `read_line` の前にバイト上限をかける。
  ```rust
  let mut line = String::new();
  let n = reader.take(MAX_LINE + 1).read_line(&mut line).await?; // MAX_LINE ≈ 1 MiB
  if line.len() > MAX_LINE { return; }
  ```

### 1b. 複数コマンドを1行に → **問題なし**
`serde_json::from_str` は trailing data でエラーになる（`{"GetState":null} {"Insert":...}` は parse 失敗 → `continue` で無視）。単一行 = 単一コマンドが保証される。

### 1c. 壊れた JSON の行 → **問題なし**
`daemon.rs:76-78` の `Err(_) => continue` で無視。パニック・状態変化なし。

### 1d. 巨大な範囲インデックス（anchor/head = usize 上限）→ **問題なし**
wire (`mina-protocol/src/lib.rs`) に `Range` を受け取るコマンドが存在しない（Move/Extend は movement/direction のみ、Goto は enum のみ）。`Range` は `StateSnapshot` の出力専用。**到達不能**。

### 1e. `SetViewport { height: usize::MAX }` → **低**
`mina-view/src/editor.rs:321` `cursor_line >= first + height` で `first >= 1` のとき debug ビルドのみ overflow panic（release は wrap して無害、プロファイル設定なし = release は overflow-checks off を確認）。panic は `tokio::spawn` された接続タスク内なので daemon は生存し、接続が切れるだけ。tokio Mutex は poison しないためロックも解放される。

---

## 観点2: Command::Open の path

### 2a. 任意パス読み込みで状態が変わる → **脆弱性（中、同一ユーザ前提）**
- 経路: `daemon.rs:87` の `read_to_string` 成功 → `daemon.rs:88-107` で `editor.open_with_path` が共有編集状態（text + focused_path）を攻撃者指定の内容に置換。被害者 TUI は次のキー入力の応答スナップショットでこの状態を描画する（セッション乗っ取り）。さらに `Command::Save`（`daemon.rs:144-151`）は `focused_path()` へ**無確認で上書き書き込み** → 任意の読み書き可能な既存ファイル（~/.bashrc、編集中のファイル等）を攻撃者内容で破壊可能。
- 影響: 同一ユーザ前提では権限昇格ではないが、「被害者が開いているファイルの破壊」「共有セッションの横取り」は実害。**単一ユーザ前提が崩れた場合（観点6f 参照）は 高**。
- 最小修正: Open で `metadata` を取得して「正規ファイルであること」を検証、Save は成功時も内容を daemon 側で保持せずとも、最低限 path の正規化（`fs::canonicalize`）と保存先の確認を入れる。

### 2b. `/dev/zero` 等の無限/巨大ファイル → **脆弱性（高）**
- 経路: `daemon.rs:87` `tokio::fs::read_to_string` はサイズ上限なし。NUL バイトは有効な UTF-8 なので `/dev/zero` は**無限に読み続け**、バッファが無制限に成長 → daemon OOM。blocking pool のスレッドも専有（複数回 Open で全スレッド枯渇 → daemon 全体がストール）。
- 影響: daemon プロセス死・全クライアントへのサービス不能。
- 最小修正: 読み込み前に `metadata` で `is_file()` と `len() <= 上限`（例 16 MiB）を確認し、違反は status エラーにする。

### 2c. 存在しないパス / ディレクトリ / 読み取り権限なし / 非 UTF-8 内容 → **問題なし**
すべて `read_to_string` の Err → `status: Some("cannot open ...")` のみで状態不変（`daemon.rs:112-115`）。非 UTF-8 パスは JSON `String` で表現不能。非 UTF-8 内容は Err で弾かれる。

---

## 観点3: Command::Insert の text

### 3a. 巨大テキスト × 繰り返し → **脆弱性（中）**
- 経路: `mina-core/src/transaction.rs:50-83` で text が operation + inverse に複製され、`mina-view/src/history.rs:38-58` の undo スタックが**上限なし**で蓄積。スナップショット応答も全文エコー。N 回の挿入で O(N×M) のメモリ。
- 影響: 悪意クライアントの挿入ループで無制限メモリ成長 → OOM。
- 最小修正: `History::push` にグループ数上限（例 100）または総バイト上限。
  ```rust
  if self.undo.len() > MAX_GROUPS { self.undo.remove(0); }
  ```

### 3b. NUL バイト / 制御文字 / 改行のみ → **問題なし**
NUL・制御文字は rope と JSON でそのまま扱われ、パニックなし。改行のみの挿入でも `scroll_to_cursor` の `char_to_line` は正しく動作。ただし制御文字の**端末への生出力**問題は観点「レンダリング」参照（daemon 側は正しい挙動）。

---

## 観点4: スナップショットの大きさ

### 4. 毎コマンド全量送信 → **脆弱性（低〜中）**
- 経路: `daemon.rs:252-263` の `snapshot()` が毎コマンド `text.to_string()`（全文クローン）+ `serde_json::to_string`（もう1回）+ 診断の clone。さらに編集時は `lsp.rs` の `sync` が LSP へ全文送信、`drain_into` も `text.to_string()`。巨大文書 × `GetState`/`Insert` 連打で O(n) の CPU/メモリチャーンを毎コマンド発生 → daemon の CPU 専有で被害者 TUI の応答遅延。
- 影響: 同一ユーザ DoS（蓄積はしないので OOM には至りにくい）。
- 最小修正: コード内 `ponytail:` コメントで既に差分送信への言及あり。まずは編集コマンド以外（GetState 等）の応答に `If-Modified` 的な軽量化、または接続ごとのレート上限。

---

## 観点5: LSP

### 5a. 診断座標の文書外（大きな line/col）→ **問題なし（パニックしない）**
`mina-term/src/lsp.rs:167-191` `lsp_pos_to_char`:
- line が最終行を超える → ループ完走後、最終行の `line_start` が残り、誤位置にマップされる（パニックなし、表示ズレのみ）。
- col 超過 → `position.rs` の `utf8_col_to_char`（`.min(line.len())`）・`utf16_col_to_char`（行末で `chars().count()` にクランプ）で安全。

### 5b. UTF-8 非境界 col → **脆弱性（低、パニック）**
- 経路: `mina-lsp/src/position.rs:6` `line[..byte_col]` — `byte_col` がマルチバイト文字の途中だと Rust の文字列スライスが **panic**（例: `utf8_col_to_char("あ", 1)`）。壊れた/悪意ある LSP サーバが utf-8 エンコーディングで中途半端な col を publishDiagnostics で送ると、`drain_diagnostics`（`lsp.rs:118-145`）経由で接続タスクが死ぬ。tokio Mutex は poison しないため daemon 本体は生存、当該接続のみ切断。
- 影響: 接続 DoS（rust-analyzer は正常時ここを通らないため、悪意ある LSP バイナリ差し替え時にのみ）。
- 最小修正:
  ```rust
  let mut b = (byte_col as usize).min(line.len());
  while b > 0 && !line.is_char_boundary(b) { b -= 1; }
  line[..b].chars().count()
  ```

### 5c. 診断 flood → **脆弱性（中）**
- 経路: `lsp.rs:135-144` が診断ごとに O(文書長) の `lsp_pos_to_char` を2回呼ぶ → O(N×文書長)。通知キューは `mina-lsp/src/lib.rs:84` の `mpsc::unbounded_channel`（無制限）。攻撃者が診断を大量生成する .rs ファイルを Open すると、全診断がスナップショットにも毎回エコーされる。
- 影響: daemon の CPU 専有・応答遅延（メモリは最新 publish 1回分で有界）。
- 最小修正: 変換コストは 1 パス化（行インデックスを事前構築）、診断数の上限（例 500）。

### 5d. uri 不一致 → **問題なし**（`lsp.rs:132-134` で `p.uri != doc_uri` は無視）。

### 5e. JSON 型不一致 → **問題なし**
`serde_json::from_value::<PublishParams>` の Err は `continue`。`severity` は `Option<u32>` で既知値以外は Hint にフォールバック。

### 5f. Content-Length 無制限 → **脆弱性（低）**
`mina-lsp/src/lib.rs:196` `vec![0u8; len]` — 壊れた/悪意ある LSP サーバが巨大な `Content-Length` を送ると巨大アロケーション → OOM。修正: `if len > 64 << 20 { return Err(...) }`。

---

## 観点6: socket 周り

### 6a. stale socket 削除 / symlink → **問題なし**
`daemon.rs:51` の `std::fs::remove_file` は unlink であり symlink を**追わない**（ターゲット削除の経路なし）。`bind` は既存パス（symlink 含む）で EADDRINUSE → 起動失敗止まり。

### 6b. 接続数無制限 spawn → **脆弱性（中）**
- 経路: `daemon.rs:60-62` `accept_loop` が接続ごとに `tokio::spawn`、上限なし・アイドルタイムアウトなし。接続を張るだけで fd + タスク + BufReader が消費される。
- 影響: fd 枯渇 → 以後の accept 不能、タスク/バッファによるメモリ消費（同一ユーザ DoS）。
- 最小修正: `Semaphore` で同時接続数上限（例 4）+ `tokio::time::timeout` でアイドル切断。

### 6c. クライアント切断検出 → **問題なし**
`read_line` の `Ok(0)`（`daemon.rs:75`）と `write_all` の Err（`daemon.rs:152`）で確実に return。リークなし。

### 6d. socket パーミッション → **残存リスク（前提が崩れた場合 高）**
`daemon.rs:330-334` `socket_path()` は uid なしのグローバルパス。bind のデフォルトモードは `0777 & ~umask`。umask が 002/000 の環境では他 OS ユーザが接続可能 → 単一ユーザ前提が崩れると「他ユーザの daemon を乗っ取って任意ファイル読み書き（権限昇格）」になる。修正: bind 後 `set_mode(0o700)` + パスに uid を入れる（`ponytail:` コメントで言及済み）。

---

## 観点7: serde の耐性

| 項目 | 判定 |
|---|---|
| unknown field | **問題なし**（serde 既定で無視 = 後方互換） |
| enum タグ違い / 未知 variant | **問題なし**（parse Err → 行を無視） |
| `Scroll { pages: isize::MIN/MAX }` | **問題なし**（`saturating_mul` + `clamp` で安全、`mina-view/src/editor.rs:333-337`） |
| `SetViewport` 巨大値 | **低**（観点1e 参照、debug のみ・接続タスク内で封じ込め） |

---

## パニック経路まとめ（重点調査）

| 場所 | 内容 | 到達性 | 重大度 |
|---|---|---|---|
| `mina-lsp/src/position.rs:6` | `line[..byte_col]` 非境界 slice | 壊れた/悪意ある LSP サーバの utf-8 col | **低**（接続タスク内で封じ込め、daemon 生存） |
| `mina-view/src/editor.rs:321` | `first + height` overflow | `SetViewport{height:usize::MAX}`（debug ビルドのみ） | **低** |
| `daemon.rs:250` (`apply` の `unreachable!`) | Open/Save が apply に流れる | 到達不能（handler の match が先に捕捉） | なし |
| `daemon.rs:147` (`expect`) | Save 成功時 path なし | 到達不能（write 成功 ⟹ path Some） | なし |
| `transaction.rs` `slice(start..end)` / debug_assert | 範囲外 | 内部 selection 由来のみ（wire から注入不能） | なし |

---

## 補足: 端末インジェクション（daemon 経由の攻撃チェーン、修正は client 側）→ **高**

- 経路: 悪意クライアントが ESC シーケンス入りのファイルを `Open`（または ESC 入りパスで `Open` 失敗 → status にパスがエコー）→ 被害者 TUI の次のキー入力の応答スナップショットにその内容が載り、`mina-term/src/render.rs:155` の `draw_line` が**制御文字を無検証で端末へ出力**（`s.push(ch)`）。ステータス行の `path`（`render.rs:185`）も生出力。
- 影響: 攻撃者制御の端末エスケープ（OSC 52 クリップボード、iTerm2/kitty 拡張、画面破壊など）による端末セッション制御。さらに、**悪意クライアント不要でも**「信頼できないテキストファイル（ダウンロードしたログ等）を mina で開く」だけで発火する。
- 最小修正（client 側）: `draw_line` と status 組み立てで `ch.is_control() && ch != '\t'` を `�`（またはエスケープ表記）に置換。

---

## 総括

- **問題なし**: 1b, 1c, 1d, 2c, 2d, 3b, 5a, 5d, 5e, 6a, 6c, 7 の大半
- **低**: 1e, 5b, 5f
- **中**: 1a, 2a, 3a, 4, 5c, 6b
- **高**: 2b（無限ファイル読み込み → OOM）、レンダリング生出力（daemon 経由チェーン、修正は client）

設計の良い点: 範囲インデックスが wire に存在しない（観点1d が成立）、壊れた JSON・未知 variant・uri 不一致・型不一致がすべて無視でパニックしない、パニックは spawn タスク内で封じ込められ tokio Mutex が poison しないため daemon が生存する構造。