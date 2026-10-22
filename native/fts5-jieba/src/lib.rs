//! Builds the loadable SQLite extension that TraceDB ships.
//!
//! All behaviour lives in <https://github.com/TSHOGX/fts5-jieba>. This crate only
//! re-exports its two extension entry points so they appear in this cdylib's symbol table.

pub use tokenizer::{sqlite3_extension_init, sqlite3_fts5jieba_init};
