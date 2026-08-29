# ワークスペースの crate ポリシー

minae は初日から Cargo ワークスペースである: `minae-*` の crate 名、フラットなレイアウト (`minae-core/` をリポジトリルートに、Helix を踏襲)。新しい crate は、中身があり、かつ分割基準の少なくとも 1 つを満たす場合にのみ作成される — 新しい依存の方向 (下方向のみを見る層)、他のコンシューマによる独立した再利用、テスト/ビルドの分離。Step 1 では 1 つの populated crate `minae-core` を出荷し、`minae-view`、`minae-term`、`minae-lsp`、`minae-loader`、`minae-event` は将来の crate として宣言され、作業が始まったときに分割される。
