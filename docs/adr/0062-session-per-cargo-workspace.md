# セッションを cargo workspace 単位で共有する（`session_root`）

ドッグフーディング（2026-09-13、driver 報告 #5）で、**10 クレートの仮想ワークスペースを
調べるだけで 6 個の rust-analyzer が起動する**ことを実測した:

```
running_sessions 0 -> 1 -> 2 -> 3 -> 4 -> 5 -> 6   (アンカーファイルのクレートが変わるたび)
ps: minad の子に rust-analyzer × 6 + proc-macro-srv × 6、RSS 計 ~4.9 GB（ページアウト後の下限）
    cold の初回応答 6.7〜10.0 s/クレート
```

機構: セッション鍵は `(workspace_root(path), language_id)` で、`workspace_root` は
**最も近いマーカー**（＝メンバーの自前の `Cargo.toml`）を返す。しかし cargo では、
どのメンバーの manifest を root にしても rust-analyzer は**ワークスペース全体**を
ロードする（driver の 2 本の独立証拠: `cargo metadata` が workspace root を返す /
`references` がそのクレートの依存グラフ外のヒットを返す）。つまり N メンバーで
**同じ 10 クレートの索引を N 回**作っていた。`reap` も無いので数は増える一方。

Status: accepted

## Decision

セッションのキーに使う root を `workspace_root`（最も近いマーカー）から
**`session_root`** に変える:

1. 最も近い manifest が `[workspace]` を持つ → それ（入れ子ワークスペースは内側が勝つ。
   cargo と同じ）。
2. そうでなければ祖先を辿り、最初の `[workspace]` を持つ manifest → それ。
3. ただしそのワークスペースが `exclude` を宣言していたら **1 に戻す**（保守的）。
   cargo のメンバー判定（`members` グロブ + path 依存の自動メンバー − `exclude`）は
   再現できないので、**解析されないリスクよりセッションを分ける方を選ぶ**。
4. どの manifest も `[workspace]` を持たなければ最も近いマーカー（従来どおり）。

- **ADR-0010 の入れ子 root 共存は保たれる**: 内側に `[workspace]` があればそこで止まり、
  ワークスペース宣言が無ければ別々の manifest が別々のセッションになる。
- **開示**: `LspServerInfo` に `roots`（稼働中セッションの root 一覧、ソート済み）を
  追加し、`minas info` が「何個の言語サーバがどのディレクトリを見ているか」を出す。
  driver の言葉: 「crate 2 の時点で知らせてくれていれば `ps` を読む必要はなかった」。
  数だけでは増加に気づけない。
- **PROTOCOL_VERSION を 23 → 24** に上げる（`LspServerInfo.roots` の追加。ADR-0039）。

## Considered Options

- **`cargo metadata` を回して workspace を解決する**: 却下 — 言語サーバの仕事を daemon が
  二重に持つ（起動コスト・失敗経路・キャッシュ）。toml を読むだけで足りる。
- **`members` グロブを自前で解釈する**: 却下 — path 依存の自動メンバーと `exclude` を
  含めた cargo の規則を再現する羽目になる。`exclude` があるときだけ保守側に倒す。
- **`reap`（アイドル TTL でセッションを落とす）を足す**: 却下（今は）— メモリは減るが
  cold 索引の繰り返し（6.7〜10.0 s/クレート）は残る。共有が先。
- **`files.watcher = "server"` だけで済ませる**: 既に導入済み（ADR-0061）だが、これは
  「変更を見る」話で「索引を共有する」話ではない。
- **全言語で workspace 解決を試す**: cargo 以外の manifest は `Package` 扱い
  （`[workspace]` 相当を持たない）ので、実質 Rust だけが対象。他言語は従来どおり。

## Consequences

- **minas 自身の調査が現実的になった**: 3 クレート触って 1 セッション（workspace ルート）。
  実測（同一 daemon、変更前後）: 3 クレート → 旧 3 セッション / 新 1 セッション。
  クロスクレートの意味解析は不変（`references minad/src/daemon.rs DocumentEdit` が
  46 references / 4 files — driver の旧ビルドでの実測と同数）。
- 代償: **共有された 1 セッションの cold は高い**（実測 22.7 s でワークスペース全体を
  索引）。旧構成は「クレートごとに 6.7〜10.0 s」を N 回だったので、1 回 22.7 s +
  1 セッション分のメモリの方が総量で勝つが、「1 クレートだけ触る」用途では cold が
  短い旧構成の方が有利だった可能性がある（この用途は driver の測定範囲外）。
- テスト: `session_root_shares_a_cargo_workspace`（共有 / 入れ子独立 / `exclude` 保守）。
