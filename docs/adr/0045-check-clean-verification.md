# check のクリーン根拠を応答に載せる(settled / false-clean 対策)

check の空応答は「本当にエラーが無い」と「解析が完了しないまま予算切れで空」を
区別できない。検証第3回(2026-09-08, docs/gitignore/study/minas-hypothesis-study.md
H5)で実測したとおり、クロスシンボル破壊(unresolved import・消えた型への参照)
は rust-analyzer の pull 診断が空を返し続け、check は exit 0 を返す —
「clean」と「クリーン」は別物。エージェントが check の exit 0 をクリーンの根拠に
すると、壊れたコードを積み上げる。

Status: accepted

## Decision

`ServerMessage::Check` に `settled: bool` を追加する。意味は「空応答が
解析完了の確認済みクリーンか、予算切れの未確認か」の根拠表示:

- `pull_diagnostics_settled` が「非空が 2 回連続で同数 = 安定」で返った場合
  `settled: true`。
- 予算切れ(空のまま残量を使い切った)で返った場合 `settled: false`
  (クリーン**未確認** — 解析未完・サーバ停滞・クロスシンボル破壊の可能性)。

CLI の `minas check` は `settled` を JSON に載せ、`settled: false` かつ空のとき
**stderr に「clean-unverified」警告**を出す。exit コードは変えない(空 + 
settled=false も exit 0 — エージェントの `$?` 分岐を壊さない。エージェントは
JSON の settled を見て判断する)。検証の二段構え(check → cargo test)の習慣と
「check の exit 0 をクリーンの根拠にしない」運用(第3回考察)を、応答が
構造的に支える。

PROTOCOL_VERSION を 15 → 16 に上げる(応答 wire 形状の変更。ADR-0039 の bump 方式)。

## Considered Options

- 予算切れをエラー扱いにする(exit 2): 却下 — 健常なクリーンファイルでも
  解析未完ならエラーになり、編集→check の日常ループが壊れる。警告 + JSON 表示
  で足りる(エージェントは settled で分岐できる)。
- settle 予算を延長する: 却下 — 予算を延ばしても偽のクリーンは消えない
  (ADI-0032 の「空のまま予算切れ = クリーン」の構造が残る)。解析完了を LSP
  応答から確実に検知する手段は無く、本書は「未確認と表示する」側に倒す。
- 診断をクレート全体に広げる: 大掛かり(private 診断で実現不能。将来の
  `workspace/diagnostic` 対応時に再検討)。

## Consequences

- エージェントは `settled: false` で「解析が安定しなかった」ことを判断できる。
  rename 後の検証などクロスシンボルが絡む局面では cargo test に委ねる判断が
  構造的に可能になる。
- CLI は exit 0/2 の分岐を維持しつつ、JSON のフィールドが増える(後方互換:
  旧クライアントは未知フィールドを無視するが、wire 変更のため bump)。
- skill の check 説明に「空 = クリーン未確認」を追記し、契約を明文化する。
- 計測・mock サーバ・round-trip テストを settled 対応に更新(モックは常時
  診断を返すため settled: true 経路のみ。false 経路は実測で確認)。