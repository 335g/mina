# スクロールキーはカーソルも動かす; A/I は行末・行頭 Insert(Helix 準拠)

C-f/C-b/C-d/C-u/PageUp/PageDown はビューポートのスクロールに加えて、カーソルもスクロールした行数と同じだけ動かす。Normal では選択を折りたたんで移動し、Select では head だけを拡張する(既存の h/j/k/l のモード分岐と同じ規則)。スクロール後にカーソルが画面外へ消える問題が解消され、Helix の page 移動相当の挙動になる。6キーは単一の `Command::Scroll` ハンドラを共有しているため、ハンドラ1箇所の変更で全体に効く。

Shift+a(`A`)と Shift+i(`I`)は新プロトコルコマンド `InsertAtLineEnd` / `InsertAtLineStart`(PROTOCOL_VERSION 5→6)として実装する: 各 Range を head の行の行末 / 最初の非空白文字(空白のみの行は列 0)へ折りたたみ、Insert モードへ移行する(UndoGroup は既存の SetMode と同じく開始時に開く)。`Command::Change` と同じ「動作+モード切替を単一コマンドで原子に行う」パターンを踏襲する。クライアント側の 2 リクエスト合成(Move → SetMode)は、移動とモード切替の間に外部編集が割り込む窓が生じ原子性が崩れるため採用しない。

Helix との意図的な差異: (1) Select モードでも選択を折りたたむ — Helix は anchor を残して行末へ拡張するが、mina の `Transaction::insert` は選択範囲を置換するため、拡張したまま Insert に入るとタイプ文字が選択範囲を置換し、Helix の「行末に追加」という観測挙動と一致しない。(2) 空行の自動インデントは行わない — mina にインデントエンジンが無いため。導入時に追随する。
