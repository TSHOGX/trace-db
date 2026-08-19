pub mod model;
pub mod parsers;
pub mod store;

use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Resolve the database path using the same deployable contract as the old
/// Bun package. `TRACEDB_PATH` always wins, including in tests and containers.
pub fn default_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TRACEDB_PATH") {
        return PathBuf::from(path);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir())
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("trace-db").join("trace.db")
}

pub fn open_database(path: impl AsRef<Path>) -> Result<Connection> {
    store::open(path)
}
