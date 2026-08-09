調査完了。Helix のソースコード (v25.7.1, shallow clone) を一次情報として読み、公式 `docs/architecture.md` とコードの裏取りを行い、日本語ノートを保存しました。

**要点 (5行)**:
1. 編集コアは関数型 (CodeMirror 6 由来)。Rope (ropey) + `Selection` (複数カーソル第一級) + OT 風 `Transaction` (invert で undo)
2. workspace は helix-stdx → core → view → term の依存階層 + lsp/dap/event/loader/vcs/parsec の横断クレート
3. UI は Cursive 風 `Compositor`/`Component` レイヤー合成、イベントループは `tokio::select!` で多重化
4. tree-sitter は `tree-house` でラップし、grammar は git 取得→共有ライブラリにビルド (`helix-loader/src/grammar.rs`)
5. LSP 統合の落とし穴は UTF-16 オフセット変換 (`OffsetEncoding`)。「1文書を複数 View で表示するため selection は View 側に持つ」という判断も重要