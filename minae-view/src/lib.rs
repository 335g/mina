//! minae のビュー層: UI 非依存のエディタ状態（imperative shell）。
//!
//! 関数型のコア（minae-core）は状態を持たず、「文書と選択を新しい状態に変換
//! する」ことだけをする。この層はその**現在の状態**を保持する: 開いている
//! 文書の集合、アクティブな View（どの文書をどの選択で表示しているか）、
//! モード、undo/redo の履歴。将来のターミナル UI（minae-term）はこの層に
//! 依存し、コマンドはここを経由してコアを呼ぶ。
//!
//! レイアウト: viewport（先頭行）と分割ツリーは実装済み。水平スクロール・
//! 折り返しはターミナル描画の段階で追加する。

pub mod editor;
pub mod history;
pub mod mode;
pub mod tree;

pub use editor::{DocumentId, Editor, View};
pub use history::History;
pub use mode::Mode;
pub use tree::{SplitDirection, Tree, ViewId};
