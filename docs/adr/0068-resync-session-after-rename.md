# rename の前に**サーバの view を現在にする**（stale な座標でファイルを壊さない）

`ts-frontend` ラウンド（2026-09-13、driver 報告 #14）で、**`minas rename` がソースを無言で
破壊する**ことを実測した。決定論的な再現（2 ファイル・3 コマンド）:

```
pa.ts: export const step1 = (n: number): number => n + 1;
pb.ts: import { step1 } from "./pa";  … step1(step1(value))

A. minas rename src/pa.ts step1 step2   → 2 files, 4 edits ✓（pb.ts は import { step2 } …）
B. minas rename src/pb.ts step2 step3   → 1 files, 3 edits / exit 0
   pb.ts: import { step1 as step3 } from "./pa";     ← **2 回前の名前**を specifier に書き戻す
          return step3(step3(value));
   tsc: error TS2724: '"./pa"' has no exported member named 'step1'
```

より悪い形も一度出た（driver の (b) 実測）: `document.createElement` →
`documepaintTaskment` のように、**編集が関係ない識別子の内側に注入される**。TS では
`check` が pull 非対応で使えない（ADR-0066）ため、**minas の側には破損を見る手段が無い**。

Status: accepted

## Decision

原因は 2 つで、両方を直す:

1. **既に開いている文書に `didOpen` を再送していた**（LSP では `didChange` が正しい）。
   `LspSession` は `current_uri`（最後に開いた 1 つ）しか持っておらず、開き直しのたびに
   `didOpen` を送っていた。tsserver は 2 回目の didOpen を無視して**古いテキストを持ち続ける**
   → 2 回目の rename は古い座標で計算される。
   → `open_uris: HashSet<String>`（サーバが didOpen 済みの URI）を持ち、既に開いていれば
   `didChange`（全文同期）を送る。`didClose` で集合から除く。`did_open` / `did_open_keep`
   の両方に適用。
2. **rename が書き換えたファイルのうち、フォーカス文書しかセッションへ反映していなかった**
   （`apply_and_save_rename` のフェーズ 4 は `restore_focus_after_semantic` のみ）。
   → 書き込み後、**書き換えた全ファイル**を `did_open_keep(path, new_text)` でセッションへ
   反映してから復元する。

## Considered Options

- **適用前に WorkspaceEdit を検証する**（driver の提案: 各 span の期待テキストを照合）: 却下 —
  LSP の編集は「範囲 + 新テキスト」で、**サーバが前提にした旧テキストは送られてこない**ため、
  照合する対象が無い。`documentChanges[].textDocument.version` を送るサーバなら版比較が
  できるが、tsserver は送らない。stale を**起こさない**方が確実。
- **rename の前に全対象ファイルを didOpen し直す**: 却下 — 上記 1 の理由で逆効果（無視される）。
  `didChange` が正しい。
- **rename 後に結果を型検査する**: 却下 — 言語ごとの検査器を daemon が持つことになる。
  テキスト網（ADR-0065）と `check`（pull 対応言語のみ）で足りる。
- **書き換えた各ファイルを閉じて開き直す**: 却下 — `didClose` + `didOpen` でも同じことはできるが、
  開いている文書の数を減らす意味が無く、`didChange` の方が安い。

## Consequences

- 決定論的 repro が**直った**: `import { step2 as step3 } from "./pa"`（正しい別名。
  specifier は現行の名前）、`tsc --noEmit` exit 0。driver の (b) 系列
  （apply を挟んだ連続 rename）でも `paintTask as describeTask` の正しい形になり、
  `formatStats` を含む隣接テキストは無傷、`tsc --noEmit` exit 0。
- 「rename を 2 回続ける」という普通の操作が安全になった。これは Rust ラウンドでも
  起き得た欠陥だが、**`check` が無い TS で初めて「無言の破壊」として現れた** —
  ラウンドを分ける判断（profile の揺らぎ）のもう 1 つの効き目。
- 残る限界: サーバが stale なまま返す別経路（例: 外部プロセスがファイルを書き換えて
  daemon が気づく前）は、外部変更検知（ADR-0015 の watch_disk）に依存する。今回の修正は
  **daemon 自身の書き込み**が原因の stale を塞ぐ。
