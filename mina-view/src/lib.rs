//! mina のビュー層: UI 非依存のエディタ状態（imperative shell）。
//!
//! 関数型のコア（mina-core）は状態を持たず、「文書と選択を新しい状態に変換
//! する」ことだけをする。この層はその**現在の状態**を保持する: 開いている
//! 文書の集合、アクティブな View（どの文書をどの選択で表示しているか）、
//! モード、undo/redo の履歴。将来のターミナル UI（mina-term）はこの層に
//! 依存し、コマンドはここを経由してコアを呼ぶ。
//!
//! レイアウト（スクロール位置・分割）はターミナルの描画が始まる段階で
//! 追加する。今は単一 View のみ。

pub mod editor;
pub mod history;
pub mod mode;

pub use editor::{DocumentId, Editor, View};
pub use history::History;
pub use mode::Mode;
