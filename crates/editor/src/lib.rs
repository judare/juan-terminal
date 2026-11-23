//! Editor core: documents, editing commands, language detection and syntax highlighting.
//! No UI code here; the GPUI view lives in the `app` crate.

pub mod document;
pub mod highlight;
pub mod language;
pub mod markdown;
pub mod sql;

pub use document::{Document, Selection};
pub use highlight::{HighlightSpan, MAX_HIGHLIGHT_BYTES, highlight};
pub use language::{HIGHLIGHT_NAMES, Language};
