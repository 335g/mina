# 保存前の外部変更検知（lost update を上書きで隠さない）

ドッグフーディング（2026-09-13、driver 報告 #15）で、**`apply` が外部の書き込みを
無言で消す**ことを実測した:

- 5.2 MB のファイルで、外部の書き手が `minas apply` の read→save 窓に入ると
  **6/10 回で lost update**（`exit=0`・stderr 空・外部が書いた行が消える）
- 15 MB では 3/3 回。窓の幅はファイルサイズに比例する（5.2 MB ≈ 240 ms）
- 外部書き込みが daemon の read **より前**なら `NOT FOUND` で正しく拒否する
  （`expected_text` 契約）。穴は read→Save の間だけ

原因: `Command::Save` はバッファ全文を `fs::write` するだけで、**読み込み時の
ベースラインとディスク状態を比べていない**。全文書き込みなので、外部の内容は
マージされず丸ごと消える（破損ではなく消失）。これはこのループが対象にしている
「2 ペイン」の中心的な使い方（別セッション・エディタ・formatter・`git checkout`）で
起きる。

Status: accepted

## Decision

`Command::Save` は書き込む前に、対象パスの現在の `(size, mtime)` を
`baselines`（読み込み時に記録したディスク状態）と比較し、**ずれていれば保存を
拒否する**（書き込みを行わない）:

```
save-rejected: <path> changed on disk since it was read (another writer) —
re-read it and re-apply; the buffer was NOT written
```

- 拒否時: `status` にこの 1 行を載せ、**ディスクは触らない**。文書は dirty のまま
  （未保存の編集を失わせない）。CLI は `SAVE FAILED: <status>` を stderr に出して
  exit 2（`apply` の既存の失敗分類と同じ。作成した空ファイルは rollback される）。
- **watch_disk（ADR-0015）とは役割が違う**: 2 秒周期の watcher は「見えた外部変更」
  をリロード + 警告 + ベースライン更新で処理する。この検査は watcher が間に合わない
  **不可視の read→save 窓**を塞ぐ。両者は排他ではなく、片方が見た変更をもう片方が
  二重に拒否することはない。
- **外部での削除は拒否しない**（`metadata` 失敗 = `false`）: 現在の「削除されたら
  Save で再作成」の契約を変えない。削除は `deleted` 状態 + 既存の警告経路が扱う。
- **merge も --force も作らない**: 「読んだ内容が変わった」ことを検知して止まるのが
  最小で正直な答え（driver の期待もこれ）。マージは別の設計判断。
- 限界を明記する: `(size, mtime)` の比較なので、**同一サイズの外部書き込みが同じ
  mtime 刻みに収まる**と検知できない（APFS は ns、HFS+ は 1s）。内容ハッシュに
  すれば閉じるが、保存ごとに全文読みが増える。実測が要求したら別の反復で。
- **追記（round 6、#17）** driver の追試で 2 つの残差が出たので両方塞いだ:
  1. **`ctime` を比較に加える**（`baseline_ctime` = Unix の `ctime`/`ctime_nsec`）。
     `size` + `mtime` だけだと「同一サイズで `os.utime` により mtime を復元した
     外部書き込み」がすり抜ける（driver 実測: 5.2 MB で 4/6 lost update、guard 沈黙）。
     `utime` は mtime を戻せても ctime は必ず動く。
     実測（同一サイズ + mtime 復元、5.2 MB、書き込みを窓の 35〜60% に置く 6 試行）:
     **refused 5 / lost 0**（旧: 4/6 lost）。
  2. **write 直後にサイズを検算する**（`save-verify:`）。guard の stat と write の
     間に外部が入るとこちらの write がそれを消す（driver 実測 1/14）。POSIX に
     「変わっていたら書かない」は無いので**窓は閉じられない** — 代わりに、書いた
     バイト数とディスクのサイズが違えば未保存扱いのまま
     `save-verify: … another writer interleaved during the write — re-read the file
     before trusting it` を返す（exit 2。黙って成功にしない）。
     限界: **同一サイズの交錯**はこの検算でも分からない（内容読み戻しが要る）。
- CLI の `SAVE FAILED:` は status をそのまま出す（`{:?}` の `Some("…")` は
  エージェントが読む行に Rust のデバッグ表現を混ぜるため。#15 で気づいた）。

## Considered Options

- **保存後に検知して警告する**: 却下 — 手遅れ（外部の内容は既に消えている）。
- **`--force` で上書きを明示させる**: 却下（今は）— 既定を安全側に倒し、必要なら
  呼び出し側が再読み込みして再適用できる。フラグは需要が実測されてから。
- **内容ハッシュで比較する**: 却下（今は）— 保存ごとに全文読み（5 MB なら ~ms だが
  無条件に増える）。`(size, mtime)` で driver の再現は全部捕まる。
- **常にマージする（3-way）**: 却下 — 文書モデルが「全文バッファ」である限り、
  マージ結果を誰が承認するかが未定義。エージェント向けの正解は「衝突を報告して
  再適用させる」。
- **watcher の周期を短くして窓を狭める**: 却下 — 周期では窓は閉じない（0 には
  できない）し、ポーリングコストだけ増える。

## Consequences

- `apply` は「read→save の間に外部が書いたら成功を報告しない」を満たす。
  連続する 2 ペイン作業で片方の書き込みが消えることはなくなる（片方は exit 2 で
  再読み込みを促される）。
- 失敗は `save-rejected:` の安定コードで分岐できる（exit 2 = 再試行可能）。
- テスト: `save_rejects_when_the_file_changed_on_disk_since_read`（open → insert →
  外部書き込み → save 拒否 → ディスクは外部の内容・文書は dirty）。
