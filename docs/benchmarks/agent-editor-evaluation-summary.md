# エージェント向けエディタ 検証総括（t1〜t8・skill 反映マップ）

> **1文要約**: 「範囲 read で読む・apply（内容指定）で書く・多箇所は LSP rename」が
> 実測で最も安く・確実。位置指定編集はどの領域でも損。skill（オンデマンド参照）は
> 自明でない知識に限って価値があり、判断自体はツール説明に載せる方が最安。
>
> 本ドキュメントは一連の検証（agent-editor-performance-considerations.md →
> ab-test-plans.md → ab-results.md）と、その minae CLI（skill サブコマンド）への
> 反映状況を 1 枚にまとめたもの。詳細は各文書を参照。

---

## 1. 検証の流れと成果物

| 文書 | 内容 |
|---|---|
| `agent-editor-performance-considerations.md` | 考察（エディタの理想形・失敗率低減の設計論）＋実装（--lines 等） |
| `agent-editor-ab-test-plans.md` | 実LLM A/B のテストプラン（t1〜t3 設計） |
| `agent-editor-ab-results.md` | 実測結果（t1〜t6・生データ・限界） |
| `tools/ab/` | 再利用可能ハーネス（opencode ツール制御・minae/LSP shim・計測） |
| **本ファイル** | 総括・スキル反映マップ |

方法: opencode の bash 専用 agent（native read/edit 無効）＋ minae/LSP shim 強制。
モデル `opencode/gpt-5.4-nano`、n=4〜5/arm、計測は opencode 使用量DB（input/output tokens・cost）。
非準拠 run（直接編集・迂回）は監査＋bypass 検出で除外。

---

## 2. 全テスト結果（中央値、n=4〜5）

| # | 比較 | 主結果 | 結論 |
|---|---|---|---|
| **T1** | 範囲read vs 全文read | トークン **−50.8%**（全 run 完全分離）、コスト −21% | 範囲 read が支配的に安い。読む総量が費消の主因 |
| **T2** | 拒否理由 具体 vs 汎用 | 両 5/5・トークン差 9% | **ヌル結果**。小タスク・単一拒否では優位なし（大ファイル/反復拒否で未検証） |
| **T3** | apply（内容指定）vs 位置指定 edit | 成功率 **3/5 vs 0/5**、位置指定はトークン5倍・コスト5.9倍 | 位置指定は「拒否地獄」「誤位置への静かな適用」で失敗しやすい。内容指定が正面入口 |
| **T4** | LSP rename vs apply（小タスク） | 両 5/5・LSP がトークン −5%・コスト −26% | 小規模ではほぼ均衡（LSP の優位は未顕在） |
| **T5** | LSP rename vs apply（多数・複数ファイル） | **5/5 vs 2/5**（apply は出現取りこぼし）、トークン **−49%**・コスト **−57%** | 実用的規模では LSP が明確に優位 |
| **T6** | `minae skill` 参照の有無 | 判断に差なし（両 arm mrename 採用・5/5）、skill 参照は **＋59% トークン** | 判断を促すカードとしてはツール説明が最安（skill 不要） |
| **T7** | 拒否回復の大規模再検証（600行・ドリフト）× skill | 4/4 vs 3/4（失敗1件は意味論スリップ）、skill 参照は **＋21% トークン**・成功率に明確な上乗せなし | skill の価値は「行為が自明でない領域」に限定（T6 と一致）。真価仮説（positional 等の罠の回避）は未検証 | 判断がツール説明から自明なら skill はコストだけ増える。skill は自明でない知識用 |
| **T8** | positional 罠回避 × skill | 両 arm とも全 run が内容指定を選択（罠は自発されず、skill は判断を変えず）・skill 参照は **＋61% トークン** | 「skill が罠を回避させる」は**確認できず**（罠自体が自発されない）。skill の価値領域は現実測では未確認 | 判断はツール説明と exit 契約に担わせ、skill は契約知識の参照用に限定する設計が整合 |
| **T9（M2）** | **minae 本体 `session rename`（LSP） vs apply（Rust・T5 再現）** | **5/5 vs 3/5**（apply は `price` を取りこぼし）、トークン **−61.6%**・コスト **−61%**・wall **−56%** | 検証で最良だった LSP rename の製品化（`session rename`, ADR-0029）が、ハーネス shim と同等以上の優位を実 rust-analyzer で再現 | 使い分け判断（skill rename トピック）は本体機能に裏付けられて確定 |

**横断的な実測知見**:
1. 読む量＝費消の主因。必要な範囲だけ渡す（T1）。
2. 編集は「内容（文字列）」でアドレスする。座標は計算・陳腐化・曖昧さの三重に弱い（T3）。
3. リネーム等の記号操作は意味解析（LSP）に委ねる。取りこぼしが構造的に消える（T5）。
4. 拒否理由は具体的に（期待/実値/範囲）。汎用メッセージの回復コストは小タスクでは変わらないが、
   盲目的再試行を防ぐ行動指示として機能（T2 ヌルの範囲内で）。
5. 失敗時は「exit コード＋説明付きメッセージ」で決定的に分岐し、fresh read から再試行。
6. ツール説明（常時ロード・無料）が最強の判断カード。skill はその補助（T6）。

---

## 3. minae CLI への反映マップ（skill サブコマンド）

`minae skill`（索引）＋ `minae skill <topic>`（内容）。各トピックは検証結果を行動レベルで内蔵している。

| skill topic | 内蔵する検証知見 | 根拠 |
|---|---|---|
| `read` | 範囲 read を前提・全文 dump 禁止・範囲外は説明付きゼロ結果 | T1, Q3/P1, R1 |
| `edit` | apply（内容指定）を正面入口に・位置計算禁止（理由を明記）・hunks バッチ（≈3倍）・小分け置換 | T3, M1/N1, J1, 4-3/11m |
| `rename` | 多箇所/複数ファイルは LSP rename（成功・コスト実測）、少数は apply、未検証分は最終 grep | T4/T5 |
| `persist` | apply は保存まで・edit は dirty のまま・保存導線（H1 の注記） | H1 |
| `errors` | exit 0/1/2 の意味・拒否種別ごとの回復手順（fresh read→修正→再試行） | C1, L1〜L5, K2 |

**反映の妥当性**: 実測で「効果があった/すべき」と確認された設計（T1/T3/T5 の 3 本柱）は
skill の read/edit/rename に明示。ヌル結果（T2）と逆方向（T6）も方針に反映済み:
- T2 のヌル → errors は「拒否理由が行動指示になる」にとどめ、過大な効果主張はしない。
- T6 → skill は「自明でない知識の参照」として分離実装（索引＋詳細）。判断の一行ヒントは
  ツール説明側に置くべき、という示唆は README/方針側に記載。
  ※skill 本文には「このガイドを常に読むべき」とは書いていない — 読むタイミングはエージェント判断。

**skill の価値実測（T6〜T8）**: 3 領域 — 判断がツール説明から自明（T6）・モデルが自然に導出
（T7）・そもそも引き金が無い（T8）— いずれでも skill 参照は付加価値を示さず、トークン +21〜61%。
skill は「擬似コード的・契約知識（exit の全レンジ・保存・非自明なツール）」の参照用に限定し、
判断の本体はツール説明と minae の契約（exit 0/1/2・拒否メッセージ）に担わせる設計が実測と整合
（詳細: agent-editor-ab-results.md テスト6〜8）。

**未反映（残課題）**: 上位モデルでの再現（全テスト nano のみ）。T2 の大規模再検証（=T7）と
skill の positional 罠回避（=T8）は実施済み — ともに skill の付加価値を確認できず。

---

## 4. 実装済み機能との対応（minae 本体）

| 機能 | 所在 | 根拠 |
|---|---|---|
| `session get --lines`（範囲 read・説明付きゼロ結果・クランプ） | minae-term/src/session.rs | T1, P1/Q3 |
| `session apply`/`--hunks-stdin`（内容指定・検証・保存・exit 三値） | minae-term/src/session.rs | T3, T5, M1/N1 |
| 拒否理由の具体化（expected/found/範囲・40 文字） | minae-term/src/daemon.rs | C1, G2 |
| dirty 警告（edit 後 stderr 1行） | minae-term/src/session.rs | H1 |
| `session info`（daemon/CLI 世代・metrics） | minae-term/src/session.rs | D1/I4 |
| `minae skill`（索引＋トピック） | minae-term/src/skill.rs | 本検証の反映（下表） |
| 契約表（exit・read/edit・hunks・永続化・skill） | docs/adr/0026 | R2 |

---

## 5. 限界と残課題

- 単一モデル（gpt-5.4-nano）・小標本。上位モデルで差が縮む/広がるかは未検証。
- T2 は小タスク固有のヌル（大ファイル・複数拒否で C1 優位が出る可能性）。
- T4/T5 の LSP は TypeScript（tsserver）のみ。Rust（rust-analyzer 1.98 は rename 不具合）未対応。
- bash 専用 sandbox のため「絶対に shim を使う」強制は不可。監査除外に依存。
- 総検証コスト: 約 $1.5（nano）。再現・拡張は `tools/ab/` で即可能。