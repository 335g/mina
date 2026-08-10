//! Text-editing core for mina: a UI-agnostic model of documents and
//! selections, with transactional edits and movement coming in later steps.
//!
//! The core is functional: operations transform state instead of mutating it
//! (see `docs/adr/0002-functional-core-selection.md`). Nothing here knows
//! about terminals, keymaps, or syntax highlighting.

pub mod document;
pub mod selection;

pub use document::Document;
pub use ropey::Rope;
pub use selection::{Range, Selection};
