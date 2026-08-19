//! TraceDB's embeddable Rust API.
//!
//! [`TraceDb`] is the primary entry point for ingestion, retrieval, inspection,
//! archive statistics, FTS maintenance, and native-source reconstruction.

mod facade;
pub mod service;

pub mod model;
pub mod parsers;
pub mod store;

pub mod proto {
    tonic::include_proto!("tracedb.v1");
}

pub use facade::{
    native_root, AgentIngestReport, AgentStats, ArchiveStats, IngestReport, IngestRequest,
    SearchRequest, SearchResult, SessionTrace, TraceDb,
};
pub use model::{
    Agent, Capture, Event, EventKind, IngestMode, NativeSource, ParsedSession, Session, TokenUsage,
};

use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Resolve the database path using the public deployment contract.
/// `TRACEDB_PATH` always wins, including in tests and containers.
pub fn default_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TRACEDB_PATH") {
        return PathBuf::from(path);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("trace-db").join("trace.db")
}

pub fn open_database(path: impl AsRef<Path>) -> Result<Connection> {
    store::open(path)
}
