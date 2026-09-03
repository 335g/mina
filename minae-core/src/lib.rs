//! minae のテキスト編集コア: 端末非依存の文書・選択・トランザクション・移動・編集のモデル。
//!
//! コアは関数型である: 操作は状態を破壊的に変更するのではなく、新しい状態を
//! 返す（`docs/adr/0002-functional-core-selection.md` 参照）。
//! この層はターミナル・キーマップ・構文ハイライトについて何も知らない。

pub mod document;
pub mod edit;
pub mod movement;
pub mod search;
pub mod selection;
pub mod transaction;

pub use document::Document;
pub use edit::{
    delete_backward, delete_backward_transaction, delete_forward, delete_forward_transaction,
    delete_range, delete_word_backward, delete_word_backward_transaction, delete_word_forward,
    delete_word_forward_transaction, insert_at, insert_text,
};
pub use movement::{
    Direction, Movement, WordMoveTarget, append_selection, extend_line_below, extend_selection,
    extend_to, line_end_of, line_start_of, move_selection, move_selection_lines,
    move_selection_to_line_first_non_whitespace, replace_targets, select_line_selection, word_at,
    word_move_selection,
};
pub use ropey::Rope;
pub use search::{CaseSensitivity, find_matches, find_next, find_prev};
pub use selection::{Range, Selection};
pub use transaction::{Operation, Transaction};
