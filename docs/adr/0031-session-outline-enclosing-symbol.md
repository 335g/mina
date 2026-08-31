# エージェント向け Outline と位置解決: 全文を読まずに構造と範囲を得る (ADR-0031)

エージェント（headless）がファイル構造を把握する経路として、LSP の `textDocument/documentSymbol` を基にした **Outline**（階層的記号リスト）と、位置から**それを囲む記号の正確な範囲**を返す **EnclosingSymbol** を追加する。応答はどちらも全文スナップショットを運ばない軽量 `ServerMessage`（ADR-0025 / 0029 と同じ思想）。CLI は `minae session outline <path>` / `minae session at <path> <line>:<col>`。

トークン削減の動機: 現状、エージェントがファイル構造を知る唯一の方法は `session get`（全文読了）で、5000 行のファイルを 200 行見るためにも全行が IPC を流れる。Outline は記号の名前・種別・範囲だけを運び、**その範囲が「読むべき行」「編集すべき箇所」の住所になる** — 読みと編集の両方を「全文なし」で行えるようにする。これは inlay hint（ADR-0020）、PeekDefinitionAt（ADR-0025）、Rename/References（ADR-0029）と同じ開発動機（トークン・操作の有効活用）の延長。

## なぜ LSP documentSymbol か（tree-sitter ではなく）

前提: daemon は既に構文を tree-sitter で所有しており（ADR-0016）、Rename の識別子解決で実績がある。それでも LSP `documentSymbol` を選んだ:

- **selectionRange が標準で付く**: 各記号の**名前トークンだけの範囲**（`fn foo() {}` で `foo` の位置）。これが EnclosingSymbol（位置 → 囲む記号の範囲）をほぼ無料で同時実装可能にする。tree-sitter なら構文ごとに name capture クエリを別途書く必要がある。
- **意味的正確さ**: macro 展開系（`#[derive]`・proc-macro 生成）、`use` 別名、extern block まで把握する。tree-sitter は構文のみで macro 展開は opaque — 「静かに欠落」はこのリポジトリが最も嫌う失敗様態（T5 の数え漏れ教訓を参照）。
- **言語カバレッジ**: server が設定されていれば全言語（languages.toml に足すだけ）。tree-sitter は rust / typescript の 2 言語のみ。

tree-sitter の利点（即応・プロセス不要・コールドなし）は、LspSession が **WorkspaceRoot ごとに常駐**するため実質消える — 同一 root の 2 ファイル目以降の outline は ms 級で、コールドスタートはエージェントが 1 コードベースを編集する限り 1 回で償却される。tree-sitter fallback は「LSP 非設定言語で outline が必要になった」という実測の要請が出るまで作らない（単一経路を維持）。

## プロトコル（PROTOCOL_VERSION 7 → 8）

```rust
// エージェント → daemon
Command::Outline { path: String }
Command::EnclosingSymbol { path: String, line: u32, col: u32 }  // 1-origin 行:列

// daemon → エージェント（全文を運ばない軽量応答）
ServerMessage::Outline {
    path: String,
    generation: u64,        // 応答時点の世代
    symbols: Vec<OutlineSymbol>,
    error: Option<String>,  // 失敗理由（成功時 None）
}
ServerMessage::EnclosingSymbol {
    path: String,
    name: String,
    kind: SymbolKind,
    range: Range,           // 記号全体（char idx）
    selection_range: Range, // 名前トークン（char idx）
    found: bool,
}

struct OutlineSymbol {
    name: String,
    kind: SymbolKind,
    range: Range,           // char idx（既存の Range = anchor/head char インデックスを再利用）
    selection_range: Range, // 名前トークン
    children: Vec<OutlineSymbol>,
}
```

- **階層ツリー**（children インライン・LSP の返す順序 = 位置昇順）を採用。フラットリストは「関数だけ抜く」等のフィルタで木を失い、エージェントが構造を再構築する必要がある。
- **kind は proto 側の小さい enum に写像**する（LSP の 26 種を透過しない — LSP を protocol に漏らさない。HighlightGroup と同じ流儀）。`Module / Function / Method / Type / Enum / Constant / Variable / Other` の 8 種。未知 kind・null は `Other` に潰す。
- 位置は全部 char idx（既存の Selection / DocumentEdit と同じ単位）。EnclosingSymbol の入力だけ 1-origin 行:列（既存 `get a:b`・`PeekDefinitionAt` と同じ流儀）。

## 任意パス・didOpen・settle・focus 復元

- **未開ファイルも可**（PeekDefinitionAt と同じ「daemon がディスクから読む → 対象を didOpen → 取得」パターン）。`session outline` はセッションの作業対象を追わずに任意パスへ発行できる。
- **settle 規律は ADR-0029 を小さな予算（〜3 秒）で再利用**: rust-analyzer はプロジェクトロード未完了時に documentSymbol を空/部分で返す。「2 回連続で同一になるまでリトライ」は『ロード完了』の検知手段そのものであり（LSP 標準に「ロード中か?」に答えるリクエストは存在しない）、予算切れは「0 件」として**正直に**返す。リトライは **daemon 内で完結**し、エージェントは 1 往復で 1 つの答えを得る（操作は増えない）。
- didOpen はセッションの `current_uri` を動かすため、要求後は `restore_focus_after_semantic`（ADR-0029）でフォーカス文書を復元する。
- **capability ゲート**: `documentSymbolProvider` 非対応・サーバ未設定は `error: Some("not supported")`（入力エラー。ADR-0029 の分類どおり exit 1 = 再試行不可）。読み取り専用なので headless ゲートの例外は増えない。

## ServerMetrics の拡張

A1 用に 4 カウンタを追加（issue #27 の効果検証と同じ目的 — 「読まなくて済んだ量」を観測可能にする）:

```rust
pub struct ServerMetrics {
    // 既存: edits_total, ... , get_state_total, wait_total, save_total
    outline_total: u64,          // Outline 要求数
    outline_bytes: u64,          // Outline 応答の累積シリアライズ bytes
    symbol_range_total: u64,     // EnclosingSymbol 要求数
    symbol_range_bytes: u64,     // EnclosingSymbol 応答の累積シリアライズ bytes
}
```

bytes は daemon が応答をシリアライズする時点で計上する。既存の軽量応答（Peek / RenameResult）への遡及カウンタ追加はしない（必要な実測が生じてから）。

## 実装上の発見（途中で判明した事実）

- **initialize で `hierarchicalDocumentSymbolSupport: true` を広告しないと
  rust-analyzer はフラットな `SymbolInformation[]`（`location` のみ・`selectionRange`
  なし）を返す**（実測）。広告を追加し、あわせてフラット形状を返すサーバのための
  フォールバック（`location.range` で変換）も持つ — 広告を無視する unvetted サーバで
  「0 件の静かな空」にならないため（T5 の教訓）。
- **settle の単位コストは大きい**: documentSymbol の 1 往復は、rust-analyzer の再解析
  （didOpen 切り替え含む）と「2 回連続同一」判定で数秒かかる。エージェントが同一
  ファイルへ outline / at を連打する想定で、**outline のパスキーキャッシュ**
  （HintCache と同型・text チェックサムで新鮮判定・FIFO 64 件）を追加した。
  at はキャッシュヒットで LSP に触れず位置解決だけで応答する。
- CLI の outline 出力は **compact JSON**（トークン削減が目的の経路なので pretty の
  空白を省く。同一内容で ~40% 削減）。

## 検証（実測）

`tools/verify_outline.sh`（単発計測）の実測（2026-09-01、minae 自身の
`minae-term/src/daemon.rs` 7,981 行・382KB）:

| 経路 | bytes | 備考 |
|---|---|---|
| `session get`（全文スナップショット raw JSON） | 814 KB | 比較の基準 |
| `session outline`（271 記号・compact JSON） | 54 KB | ツリー全体 |
| `session at`（位置 → 囲む記号） | 0.2 KB | 1 往復 |
| **outline + at 合計** | **54.5 KB** | **get 比 93% 削減** |

レイテンシ: outline / at の**コールド時は数秒〜10 秒**（プロジェクトロード + settle）、
キャッシュヒット後は **~0.1 秒**（at は実測 6.5 秒 → 0.09 秒）。
コールドコストとキャッシュの効果が対になっているので、エージェントはセッションの
冒頭で outline を 1 回引いてから at / 範囲編集に入るのが正しい使い方になる。
AB ハーネス（tools/ab）でのタスク比較は後日実施する（本 ADR の追記）。

## 編集系ゲートの standing rule（B1/B2 のための記録）

A1 は読み取り専用でゲートの例外を増やさないが、**原則**として記録する:

> 編集を伴うセマンティック操作（Rename・将来の Quickfix / Format 等）は、テキストの適用・検証を daemon 側で行い、エージェントへは**影響レポート（ファイル数・編集数・変更一覧）だけ**を返す。エージェントは生の WorkspaceEdit を扱わない。

これは ADR-0029 の Rename で実証済みの契約の一般化であり、将来の編集系機能（Quickfix: 診断 → LSP 提示の修正を適用、Format: 整形を 1 コールで）はこの rule の下で個別 ADR を立てて実装する。

## Consequences

- PROTOCOL_VERSION を 8 に bump（既存クライアントの影響なし — 追加のみ）。
- Outline 応答は「全文より小さいが、1 行とは言えない」データ。巨大ファイルの記号数が多く、children を全部運ぶと大きくなり得る — 実測（検証 1）で bytes を確認し、必要ならフィルタ（kind 指定等）を ADR 追記で決める。
- コールドセッションでの初回 outline は数秒〜10 秒（rust-analyzer プロジェクトロード待ち、ADR-0029 と同じ性質）。2 回目以降は ms 級。
- `$/`progress` によるロード進捗の Activity 化（ADR-0028 経由の可視化）は**遅延実装候補**: 正しさの保証は settle 規律で足りており、「待ち時間が見えない」という実測の苦情が出てから追加する。