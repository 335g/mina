# HighlightGroup はフラットな正規集合 + UI ロールで固定する

Helix は scoped 階層テーマ（dotted scope と親フォールバック）を使うが、mina の Colorscheme（名前付きロール→色の写像）は未実装であり、今回の成果物は taxonomy そのもの。tree-sitter の capture を**フラットな正規グループ集合**（comment / keyword / string / number / constant / function / type / parameter / field / operator / punctuation / attribute / error の13種）に写像する自前クエリを書く。UI ロール（カーソル / 選択 / 診断 Error・Warning / ステータス行 / コマンドライン / ポップアップ）も taxonomy に含め、将来の Colorscheme がハイライトと UI を一括でカバーする。wire・テーマ形式は小文字、Rust は `HighlightGroup` enum とする。

## 検討した代替案

- **scoped 階層（Helix 流）**: コミュニティクエリをそのまま使えるが、taxonomy が実質 unbounded になり、テーマ側にスコープ知識とフォールバック規則が必要。現段階にテーマ作者はいない。
- **capture 直接 = グループ**: 写像なしで最小だが、クエリの capture 名がそのままテーマの顔になり言語間でばらつく。

## 帰結

- ハイライトクエリは自前実装になる（ADR-0001 のクリーンルーム方針と整合）。
- 将来の Colorscheme は「グループ→色」1テーブルで済む。
- scoped 階層が必要になったら、capture→スコープの写像層を足すだけで移行可能（タクソノミー側は壊れない）。
