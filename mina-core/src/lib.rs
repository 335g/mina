//! mina のテキスト編集コア: 端末非依存の文書・選択・トランザクションのモデル。
//! 移動プリミティブは後続ステップで追加する。
//!
//! コアは関数型である: 操作は状態を破壊的に変更するのではなく、新しい状態を
//! 返す（`docs/adr/0002-functional-core-selection.md` 参照）。
//! この層はターミナル・キーマップ・構文ハイライトについて何も知らない。

pub mod document;
pub mod selection;
pub mod transaction;

pub use document::Document;
pub use ropey::Rope;
pub use selection::{Range, Selection};
pub use transaction::{Operation, Transaction};
