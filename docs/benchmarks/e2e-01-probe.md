# e2e-01: プローブ設計 — プロトコル規律の入力トークン削減の実証

ステータス: 実行済み(準備完了) · 対象: `tmp/e2e01/wt` · 方式: opencode カスタムツールによるA/B比較

## 0. 目的

同一エージェント・同一モデルで、**「ファイル直読みツール群(対照構成A)」と「mina ツール面(処置構成B)」** を同一タスクで実行し、**成功1タスクあたりの入力トークン**を比較する。これにより「LSP対応の有無」ではなく「軽量応答・変化検知・検証付き編集をプロトコルに焼き込む設計」が LLM コストを削るか、を実証する。

## 1. 用語(計測語彙。製品語彙 `CONTEXT.md` には混ぜない)

- **プローブ(probe)**: 1タスクのA/B実行。**ベンチ(bench)**: 複数タスク×複数構成の本計測(本ドキュメントの次の段階)
- **対照構成A / 処置構成B**: 下記 §3
- **ツール面(tool surface)**: エージェントが呼べるツール集合。**窓(window)**: ±30行の有界コンテキスト読み
- **コスト指標**: 成功1タスクあたり入力トークン(cached / uncached を区別して記録。実コストはキャッシュ込みで見る)
- **GO / MODIFY / NO-GO**: §6 の判定規則

## 2. タスク

**#30 「headless に Close を許可して deleted 状態から抜け出せるようにする」** (`github.com/335g/mina`)

- **ground truth**: マージコミット `77639c9` — `mina-term/src/daemon.rs` のみ +17/−5
  1. headless 許可リスト(`process_command` 内 `matches!`)に `Command::Close` を追加
  2. 拒否メッセージを `"...和 Open"` から `"...和 Open, and Close"` に更新
  3. テスト `headless_client_is_restricted_to_document_edit_family` に headless の Close 成功ケースを追加(Close で status None・path None・空文書)
- **作業基点**: `00349a7`(修正の親コミット。headless の Open は許可済み=#28、Close は未許可=タスク本体)
- **タスク文(対照A・処置B 共通)**: 「headless クライアントが `Command::Close` でフォーカス文書を閉じ、deleted 状態から脱出できるようにする。headless の許可リスト(`command = GetState | Save | WaitFor{..} | Open{..}`)に `Close` を追加し、拒否メッセージを更新する。headless の `Close` 成功ケースを既存テスト(headless_client_is_restricted_to_document_edit_family)に追加する。既存テストがすべて通ること。」(issue #30 の記述に基づく)

## 3. 構成定義(opencode 1.14.30 / model `opencode-go/deepseek-v4-flash`)

```
tmp/e2e01/wt/            ← git worktree @ 00349a7 (tmp/ は gitignore 済み)
├── opencode.json        ← agent 定義 e2e-a / e2e-b
└── .opencode/tools/     ← カスタムツール5本(ファイル名 = ツール名)
```

### 対照構成A (`--agent e2e-a`)

- opencode 既定ツール(read / grep / write / edit / bash / glob / todo 等)をそのまま使用
- mina カスタムツールは agent 設定で無効化
- ただし備考: opencode の lsp ツールはデフォルト無効のまま(構成AもLSPツールは使わない)

### 処置構成B (`--agent e2e-b`)

- `read` / `grep` / `write` / `edit` を無効化 → **全文読みの経路を構造的に遮断**
- `bash` は `cargo test` / `cargo build` / `cargo check` / `ls` / `find` / `pwd` のみ許可(それ以外 deny。`cat` 等の回し読みを封じる)
- mina ツール5本を有効化:

| ツール | 実装 | 応答 |
|---|---|---|
| `mina_state <path>` | `session exec Open` → `session get` を解析し **text を除去** | diagnostics・generation・checksum・dirty・activities のみ |
| `mina_peek <path> <line>:<col>` | `session peek` | 定義スニペットのみ(ADR-0025 の軽量応答) |
| `mina_window <path> <line>` | ディスクから ±30 行を読み行番号付きで返す | 有界コンテキスト読み(apply は常に Save するためディスク=daemon 状態) |
| `mina_apply <path> <old> <new>` | `session apply` | 検証付き置換+Save。exit code 2 = 再試行可能な失敗(old 未発見等) |
| `mina_wait <path> <generation>` | `session wait <g>` | 世代超過までブロックし、settle 後の診断のみを返す |

- 各ツール説明に指示文を埋め込み: 「全文は利用不可。diagnostics と peek が正であり、窓は必要な行だけに使え。」

### 構成B のエージェントプロンプト(指示文)

「あなたは mina エディタデーモンが提供するリポジトリでコードを編集している。この環境ではファイルの全文を読むことは**できない**。read/grep は無効化されている。mina_* ツールが正の情報源である: mina_state は全文なしで診断・世代・checksum を返し、mina_peek は定義スニペットを返し、mina_window は ±30 行の有界スライスを返し、mina_apply は検証付き編集を適用し、mina_wait は診断の settle を待つ。診断と定義は完全かつ最新であり、信頼せよ。エラーを知るには mina_state、シンボルを解決するには mina_peek を使い、どうしても見る必要がある行だけ mina_window を使え。他の手段で全文を復元しようとしてはいけない。」

## 4. 測定

- 実行: `opencode run --agent <e2e-a|e2e-b> --model opencode-go/deepseek-v4-flash --print-logs "<タスク文>"`
- ログ: `--print-logs` の stderr + `~/.local/share/opencode/` のセッション storage から usage を抽出(cached / uncached の内訳があれば併記)
- 上限: プローブ全体の入力トークン合計 ≤ 5M・実行は `timeout 20m`。usage 非報告時はステップ数/時間で代替監視し、打ち切り
- スモークテスト(本計測前): 各構成で「利用可能なツール名を列挙せよ」を実行し、(1) ツールが読み込まれる (2) B で read/grep が実際に拒否される を確認(構造の失敗と計測の失敗を分離)

## 5. 採点手順

1. **正しさを先に確認**(トークン比較より優先):
   - 作業ツリーで `cargo test`(workspace 全体)が通ること
   - マージ済み diff(`77639c9` の `daemon.rs`)との意味的等価性: Close が許可リストにあり・拒否メッセージ更新・Close 成功テスト追加
   - 動作確認: headless で `session exec '"Close"'` が status なしで通り path が None になる
2. **トークン比較**: A と B の成功1タスクあたり入力トークン(cached/uncached 別)
3. **判定**(GO / MODIFY / NO-GO は §6)

## 6. 判定規則

- **GO**: B がタスクを正しく完了 かつ 入力トークンが A より実質減(cached/uncached それぞれで確認)
- **MODIFY**: B 完了・トークン同程度 → クエリ回数(小クエリ多発による埋没)を調べ、窓幅・ツール説明を調整して再プローブ
- **NO-GO**: B がタスクを完了できない → このタスク種は全文が必要、として仮説を限定 or 却下

## 7. 結果(2026-08-19 実行)

| 項目 | 構成A(対照, e2e-a) | 構成B(処置, e2e-b) |
|---|---|---|
| セッション | `ses_fe57a25c…` (42 msgs) | `ses_fe55de61…` (63 msgs) |
| タスク完了 | 〇 | 〇 |
| 正しさ | Close許可・メッセージ更新・テスト追加 (挙動OK・diff等価)。さらに docs/adr/0026 も更新 | Close許可・メッセージ更新・テスト追加 (挙動OK・diff=ground truth と同スコープ) |
| cargo test (-p mina-term) | 215/216 (1失敗は `stalled_response_writer` — **main HEAD でも失敗する pre-existing/macOS環境依存**、Close と無関係) | 同 |
| 入力トークン | **155,287** | **148,956** (−4.1%) |
| 出力トークン | 9,642 | 13,015 (+35%) |
| コスト | $0.0566 | $0.0574 |
| 実行時間 | 750s | ~892s (900s 上限で打ち切り。作業は完了済み) |
| ツール | bash 18 / grep 12 / read 11 / edit 5 | mina_window 34 / bash 16(許可コマンドのみ・cat/grep は全 deny) / mina_state 6 / mina_apply 5 / mina_peek 3 / task 2 / glob 1 |
| cached / uncached | cache 報告なし (opencode-go が非報告) | 同 |
| 判定 | — | **MODIFY** |

### 予算

- プローブ全体実使用: 入力トークン累計 ≈ **370K**(内訳: スモーク≈18K / A=155K / 無効試行1回=37K(B設定削除バグ)/ B=149K / 検証等≈8K) — 上限 5M に対し大幅余裕
- 無効試行1回: `git clean -fd -e .opencode` がルートの `opencode.json` を削除 → `agent "e2e-b" not found` で fallback。原因は設定ファイルの除外漏れ。B の有効測定は再実行分のみ。

### 解釈と次アクション (MODIFY)

- **構造的制約は機能した**: B は cat/grep 実行を全て permission deny で遮断され、全文なしでタスクを正しく完了した(ツール面の仕組み自体は成立)。
- **しかしトークン削減は出なかった**: 入力はほぼ同水準(155K vs 149K)。理由は二つ。(1) タスクが小さい(daemon.rs 17行変更)ため「ファイル全文を再読しない」利点が発現しない。B のコストはむしろ **63 ステップ × 毎ステップの履歴再送(システムプロンプト+ツール定義+累積文脈)** という per-step オーバーヘッドが支配的。(2) B は ±30 窓を 34 回呼び、小さな文脈を繰り返し取得して再構成した(窓が小さすぎて関数単位の文脈を保持できず)。
- 類推: プロトコル規律の勝ち目は「**ファイルが大きく・確認ループが重い**」タスクで顕在化する(全文再読が数万トークン/回になる場面)。本タスクは不利側の条件だった。
- **次のベンチでは**: (1) より大きい多ファイルタスク(#27/#29の session.rs 312行、または機能追加タスク)(2) 窓幅の拡大(関数文脈が入る範囲、例 ±80)とツール説明の改善(3) 「全文再読を避ける」利得と「per-step 再送」コストを分離して測る指標(ステップ数・履歴再送トークン)。
- 副次発見: B は diff が ground truth と同スコープ(余計な変更なし)で、A より変更が「的を射ていた」。また全文なしで実タスクを最後まで遂行できること自体が、ツール面として成立している証拠。
## 8. ベンチ2: カーソルリセット(多ファイルタスク)

プローブ(#30/小タスク)で MODIFY と出た「タスク規模で差が出る」仮説を、より大きい多ファイルタスクで検証した。構成・ツール・モデル・採点法はプローブと同一に保ち、変数は「タスク規模」のみ。

- **タスク**: カーソルリセット(ADR-0027)。Interactive 切断時に全 View のカーソル・ビューポートを先頭へリセット(Hello フラグ + config)。
- **ground truth**: `26f08ac..790d6f6`(9ファイル / 274挿入)。実装の実体は protocol(Hello フラグ)+ view(リセットメソッド)+ config(bool)+ term(client 配線)+ daemon(切断時リセット)+ テストの多レイヤ。
- **ベース**: `26f08ac`(B ツール= session apply/peek/wait 利用可・headless Open/Close 済み・コンパイル可)。

| 項目 | 構成A(対照) | 構成B(処置) |
|---|---|---|
| セッション | `ses_fe44262a…` (86 msgs) | `ses_fe4356ab…` (102 msgs) |
| 入力トークン | 283,931 | **175,584 (−38%)** |
| 出力トークン | 32,270 | 45,063 (+40%) |
| コスト | $0.1805 | **$0.1129 (−37%)** |
| 実行時間 | 763s | 1,354s (+77%) |
| ツール | read/grep/edit/bash | mina_window 46 / mina_apply 40 / bash 23(許可コマンドのみ・cat/grep は全て deny) / task 3 / mina_state 2 |
| テスト | 220 pass | **223 pass**(e2eソケット含む) |
| diff | 7ファイル(ground truth と同スコープ) | 7ファイル / 368挿入(同スコープ、やや多め) |

### 結論: 仮説を実証

- **入力トークン −38%・コスト −37%** を B が達成。プローブ(#30)で「同水準」だったのに対し、**タスクが大きくなるほどプロトコル規律の利得が顕在化する**という MODIFY 仮説を、同一の大きいタスクの A/B で確認できた。A は大ファイル(daemon.rs 等)を read/grep で全文再読し、B は診断・窓・検証付き編集で同じ成果物を作った。
- トレードオフ: **B は実行時間 +77%・出力トークン +40%**。理由は、370行を 40 回の mina_apply で書いたこと(小さな編集を多数積む latencies)と、窓読み(46回)を繰り返したこと。
- つまり: **節約は「入力トークン / コスト」で顕在化し、「時間 / 出力トークン」は B が不利**。商品化するなら「大きな編集(apply の大きさ)」と「窓幅」の調整が利得を伸ばし、時間コストは mina_apply の並行/一括化や、write 系ツールの併用(読むのは遮断・書くのは許す)で回収できる余地がある。

### 次アクション(任意)

- 窓幅 ±30→±80、apply の一括(whole 編集)を許可し、時間/出力の不利を改善できるか再測。
- 「読むのを遮断・書くのは許す」構成(B+write 有効)で、出力トークンと時間の不利が解消するか確認。
- 反復を伴う実務タスク(テスト失敗→修正→再テスト)で、確認ループ重いシナリオの利得を測る。

## 9. 3パターン検証: 小 / 中 / 大(カーソルリセット)

3点で「タスク規模 vs Bの利得」の線引きを試みた。構成・ツール・モデルは全て同一。

| タスク | 規模(ground truth) | 構成A | 構成B | Bの利得 |
|---|---|---|---|---|
| **小** #30 headless Close | 1ファイル 22行 | 155,287 (42msgs) | 148,956 (63msgs) | **−4%** (ほぼ無差) |
| **大** カーソルリセット FULL (ADR-0027) | 9ファイル 274行 | 283,931 (86msgs) | **175,584 (102msgs)** | **−38% 入力 / −37% コスト** |
| **中** カーソルリセット コア (protocol+view+daemon、config/TUI除外) | 4ファイル 218行(参考) | 306,889 (63msgs) ※完了 | **判定不可** ※未完(2回タイムアウト) | — |

### 中小判定の注記(誠実)

- **中タスクの構成Bは完了しなかった**(30分・40分の2回を打ち切り。入力トークン合計 132,274 + 335,387 を消費)。ただし原因はタスク難度より、**私のツール欠陥とタスク枠組みの曖昧さ**寄り:
  1. **`mina_wait` のハング**: 最後のツールコールが `mina_wait` status="running"(世代が超過しないと `session wait` が永久ブロック)。B のツールに timeout 無しの wait が入るとループし得る — 製品化では wait に上限が必要(テスト側で DAEMON 世代が進まない編集 no-op 等で発生)。
  2. **「コアのみ・config/TUI は対象外」という subset 指定が B に曖昧**で、範囲の取り違えや過剰な再検証サイクルを招いた。
  3. 一方「中」の構成Aは735s/307Kで完了しており、**このタスクには(A の) read 全開の方が速い**。
- つまり **「規模が大きいほど B が勝つ」という単調な線は立証できていない**。実際は**タスク次第**:
  - 編集対象が「エディタの状態クエリ(診断・定義・窓)」に向く小〜大の機能 → B の利得(−38%)
  - 複数サブシステムを横断し、検証サイクル(B の apply→wait→window ループ)が増える枠組み → B が遅く・ハングリスク(中タスク)
- **確実に言える線引き**: 小タスクでは B 有利なし(small)、大規模で状態クエリに向く機能では B −38%。**中は判定不能**で、ツール(wait の timeout)と枠組み修正の後に再測定が必要。

### 次アクション(ツール修正して 中 を再測定)

- ~~`mina_wait` にタイムアウト上限(例: 生成が N 秒進まなければ現在状態を返す)を追加。~~ → **実施済み (2026-08-20, 下記 §10)**
- 中タスクを「曖昧な subset 指定」でなく、**1サブシステム完結の明確なタスク**として定義し直す(view 単独リセットは小さすぎる。Activity の daemon 追跡 124行を単独タスク化等)。
- 大タスクの「B −38%」と合わせ、小(±0)→ 中(?)→ 大(−38%)の3点を揃える。

## 10. 対応記録: wait タイムアウトの実装 (2026-08-20)

中タスク失敗の直接原因だった「世代が進まない wait の永久ブロック」を、グリルセッション (grill-with-docs + domain-modeling) で設計を詰めて実装した。

### 決定 (設計ツリーの全分岐)

- **実装層: CLI 側のみ** (daemon・プロトコル不変)。待機中の接続は daemon 側でブロックされたままなので、タイムアウト後の現状取得は別接続の GetState で行う。
- **タイムアウト値: 固定 90 秒** (診断 settle の正常終了上限 ~30–60 秒 + マージン)。引数・env オーバーライドなし。
- **応答契約: exit code 2 (再試行可能) + 現状スナップショットを stdout** + 理由を stderr。既存の exit code 2 意味論 (edit 拒否) を「再試行可能な失敗」に拡張して再利用 (新規コード意味論を作らない)。
- **スコープ: 実装 + テスト + ドキュメントまで**。中タスク再測定は別トピック。
- **ドキュメント: 新規 ADR なし**。WaitFor の仕様を所有する **ADR-0014 に 1 箇所更新** (当初の想定 ADR-0012 は WaitFor を所有していなかった — 事実確認で訂正)。CONTEXT.md は触らない。
- **用語: E2E 用語「unsettled フラグ」は不採用**。既存語彙 (exit code 2 + status/スナップショット) で表現する。

### 実装内容

- `mina-term/src/session.rs`: `WAIT_TIMEOUT=90s` 定数・`WaitOutcome` enum・分離ヘルパー `wait_with_timeout` を追加し、`session wait` 分岐を書き換え。プロセスは一時接続をしない (async fn は最初の poll まで接続しないため、正常系で無駄な接続なし)。
- 回帰テスト `wait_with_timeout_returns_current_state_on_timeout` を追加 (分離ヘルパーを短いタイムアウトで検証。実 CLI の 90 秒定数に依存しない)。
- `docs/adr/0014` の WaitFor 条項を更新 (「タイムアウトなし」→「CLI 側 90 秒/exit 2」)。
- プローブ用ツール `tmp/e2e01/wt/.opencode/tools/mina_wait.ts` の説明文にタイムアウト仕様を追記 (tmp/ は gitignore のため追跡外)。

### 検証

- `cargo test -p mina-term`: 222 passed (新テスト含む)。
- 実バイナリでのエンドツーエンド: `mina session wait 18446744073709551615` が 91 秒で exit 2・現状スナップショットを JSON 出力・stderr に理由 (90 秒タイムアウト + 再接続 1 秒。初回は旧 daemon がソケットを掴んだままで fallback の GetState が parse エラー — `session info` で検知する想定の事故。daemon を立て直して成功)、`session wait 0` は即座に exit 0。
- 副次: フルスイートで間欠失敗していた既存テスト `disconnect_does_not_reset_while_another_interactive_remains` (最後の切断→GetState の競合) を、リセット伝播の待ちループを追加して修正 (本変更とは無関係の既存フレーク)。

### 次の一手 (次回セッション)

1. ~~中タスク (Activity の daemon 追跡 124 行を単独タスク化) を wait タイムアウト込みで再測定し、小(±0) → 中(?) → 大(−38%) の3点を揃える。~~ → **実施済み (2026-08-20, 下記 §11)**
2. 窓幅 ±30 の関数/チャンク単位化・多 hunk apply・apply 応答への診断同梱 (往復削減)・B+write 構成の検証。

## 11. 中タスク再測定: Activity 追跡 (wait タイムアウト込み, 2026-08-20)

§10 の方針通り「Activity の daemon 追跡」を単独タスクとして再測定した。構成・ツール・モデル・採点法は従来と同一 (opencode 1.14.30 / `opencode-go/deepseek-v4-flash` / 構成A・B / ADR-0028)。

### タスクとベース

- **タスク**: mina-protocol に `ActivityKind` / `Activity` 型と `StateSnapshot.activities` を追加し、daemon が Open / Save / 外部変更 Reload / LSP 診断確定の各経路で Activity を追加・除去 (増減で generation を進め、重複追加・存在しない除去は無視、settle ループの全出口で `DiagnosticsSettle` を除去)。スナップショットにはフォーカス文書の活動だけを載せる (設計は ADR-0028)。
- **ground truth**: `2e5b152..2efecf1` (4ファイル / +170/−3: protocol +31 / daemon +124 / lsp +15 / render +3。テストはシンプルなユニット2件: `activity_add_remove_bumps_generation` と `snapshot_carries_focused_activities`)。GT での `cargo test` は 380 passed / 0 failures を確認済み (別 worktree で検証)。
- **ベース (合成)**: `2e5b152` + `c7c45fb` (wait 90s タイムアウト) + `31d955e` (切断リセットのフレーク修正) を worktree 内で cherry-pick (wait タイムアウトはベースより後にあるため。tmp/ は gitignore のため履歴汚染なし)。
- **手順**: 各構成を 2 段階で測定 — ①実装 (タスク文どおり) ②安定化 (「現在のツリーを維持し cargo test 全 green に収束せよ。失敗している自作テストは修正・簡素化・削除してよい」)。①だけで「完了宣言 = 実装完了」とみなすと false-done を見逃すため (後述)。

### 結果

| 項目 | 構成A (e2e-a) | 構成B (e2e-b) |
|---|---|---|
| **①実装** | 939s / 90 steps / input **352,606** / output 43,830 / **$0.1775** | 2,481s (41min 打ち切り・作業継続中) / 138 steps / input **251,810** / output 74,298 / **$0.2155** |
| ①の成否 | 完了宣言するも**自作テスト2件が決定的に赤** (3回再実行でも失敗: `external_reload_sync_advances_generation_via_reload_sync_activity`・`settle_diagnostics_activity_appears_then_clears`) | 自作統合テスト1件が赤のまま時間切れ。**ハングは無し** (mina_wait 呼び出しは 0 回) |
| **②安定化** | 506s / 50 steps / input 112,628 / output 22,627 / $0.0603 — 時間依存 assert を撤去し簡素化 → **全 green** | 385s / 36 steps / input 132,782 / output 22,785 / $0.0538 — 統合テストを決定論的 (sleep-and-check) に書き直し → **全 green** |
| **合計** | **1,445s / 140 steps / input 465,234 / output 66,457 / $0.2378** | **2,866s / 174 steps / input 384,592 / output 97,083 / $0.2693** |
| B の利得 | — | **入力 −17.3%** / コスト **+13%** / 時間 **+98%** / 出力 +46% |
| 最終 diff | 4ファイル / +560/−55 (protocol 6→7 化含む, lsp.rs 119行改変) | 4ファイル / +428/−18 (protocol 6→7 化含む, lsp.rs 46行改変) |
| テスト | **386 passed / 0 failures** | **386 passed / 0 failures** |
| 挙動確認 | 自作ユニットテスト群で代替 (世代加算・重複無視・フォーカス分離・スナップショット反映) | 同上 + 実バイナリ headless で `session get` に `activities: [diagnostics_settle]` が実在・generation 進行を確認 |

### 判定

- 中タスクとしての **3点目は「小 0 → 中 −17% → 大 −38%」の単調な入力削減を確認**。ただし削減幅は大タスクの半分で、**時間 (+98%)・コスト (+13%) の不利が最大のタスク**になった。判定は **GO (入力トークン軸)** — 実質減 (−17%) を満たすが、単発の実務利用では B の時間コストが足を引っ張る MODIFY 寄りと解釈する。
- 前回 中タスクの直接障害だった「wait の永久ブロック」: **今回 B は mina_wait を一度も呼ばず、wait タイムアウトは発動しなかった**。それでも 41 分間ハングせず作業継続できた (前回は settle ループで停止) — リスクの除去枠としては成立 (§10 で単体検証済み)。

### 観察 (副次発見)

1. **両構成が同じ罠に嵌った**: 非同期 settle のタイミング依存テスト (モック LSP サーバ + push インターリーブ) を自作して赤 → 安定化ステージで簡素化/決定論化して収束。ground truth のテストはシンプルなユニット 2 件。**タスクの難度は「実装」より「テスト設計」側にあり**、ツール面 (A/B) に依存しない共通要因だった。
2. **A はテスト赤のまま完了宣言** — 単発測定では false-done として記録されるところを、②安定化ステージが救った。中〜大タスクは「実装完了 + 全テスト green の確認」までを 1 測定単位にすべき教訓。
3. **両者とも PROTOCOL_VERSION を 6→7 に上げた** (ground truth は据え置き)。破壊的 wire 変更とみなす設計判断の違い。ソケット名に version が埋まる ADR-0019 の仕組み (旧 daemon は残り新 binary が自動起動) に守られて動作した。
4. **B の最終 diff は A より小さい** (lsp.rs 46 行 vs A 119 行)。±30 窓の小さな往復が過剰リファクタリングを抑制した可能性。
5. 計測上の注意: opencode の SQLite ストレージは WAL フラッシュ遅延があり、セッション終了直後の usage 抽出は空になることがある (数秒待てば入る)。また stage1 のツリーを stage2 前に退避し忘れると実装が失われる (今回 A は DB の edit ツール呼び出し列 (old/new) からのリプレイで復元した)。

## 12. 対応記録: ラウンドトリップ削減 — 複数編集 apply `--hunks-stdin` (2026-08-20)

§11 の中タスク再測定で「入力 −17% を達成したが時間 +98%・出力 +46% が最大の不利」と出た。これは B が多数の小さな apply / 窓読みを積んだためで、グリルセッション (grill-with-docs + domain-modeling) で「ツール呼び出し回数＝LLM ステップ数を減らす = per-step 履歴再送コストの乗数も減らす」設計に落とした。

### 決定 (設計ツリーの全分岐 — 全て推奨構成で確定)

- **Q1 複数 hunk の表現: `--hunks-stdin`** — stdin に JSON 配列 `[{"old","new"},…]`。既存 `--whole-stdin` と同型で一貫、argv エスケープなし。全文置換 (`--whole`) は「全文を読まない」プロトコル規律と衝突するため、**targeted な複数編集** (全文知識を要しない) を正のプリミティブにする。
- **Q2 適用順と失敗 semantics: 順次適用・Save は最後に一度・途中失敗は exit 2 + ディスク無変更**。単発 apply の失敗契約の一般化。daemon のメモリ内文書は部分的に編集されるが次の Open で自己修復。
- **Q3 応答同梱: generation のみ**を JSON で返す (`{"applied","edits","generation","checksum"}`)。診断は settle 前なので含めない。generation があれば続く `wait <generation>` を別の `session get` なしで直接呼べる (state 呼び出し 1 回分削減)。
- **Q4 スコープ: CLI + ツールのみ**。DocumentEdit/Save のループは既存 headless 許可リスト内。**プロトコル・wire 変更なし**。
- **Q5 ドキュメント: 新規 ADR なし**。apply を所有する **ADR-0026 に追記**。CONTEXT.md は不変 (用語も新規導入なし、複数編集は apply の一般化)。

### 実装・検証

- `session.rs`: `Hunk` struct + `parse_hunks` + `apply_hunks` を追加。`apply` に `--hunks-stdin` 分岐。チェックサムは前の編集結果で連鎖、各 hunk の `old` は直前適用後の現在テキストに対し char インデックスで再計算 (行揺れ対応)。
- 実バイナリ E2E: 3 hunk を 1 プロセスで適用 → `{edits:3, generation:7}` + exit 0 + 3 箇所が正しく置換され Save 成功。失敗 path: 2 番目の hunk が未発見 → exit 2 + `NOT FOUND` + **ディスク無変更** (1 番目はメモリ適用済みでも Save されない) を確認。
- ツール `tmp/e2e01/wt/.opencode/tools/mina_apply.ts`: `hunks` 配列を受けて `--hunks-stdin` に流す形に変更 (opencode の schema は zod のため `array(object{old,new})` が利用可)。tmp のため追跡外、次回ベンチで実証。
- `cargo test -p mina-term`: **223 passed / 0 failed** (8 回連続グリーン、`parse_hunks` テスト追加)。
- 副次: 前回入れた切断リセットのフレーク修正が「Interactive を先に接続するとリセット自体を抑止する」と根本原因を捉えていなかったため、**Headless オブザーバで伝播を待つ**形に改め、8/8 グリーンで安定 (ADR-0027 の Interactive 判定に Headless は数えない点を利用)。

### 次の一手

- 窓幅 ±30 の関数/チャンク単位化 (並行して毎セッション残っている課題) と、B+write 構成の検証。多 hunk apply + 窓関数化で §11 の「時間 +98%」を回収できる見込みを、ベンチ 4 で確認する。
