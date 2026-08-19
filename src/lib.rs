//! TraceDB's embeddable Rust API.
//!
//! [`TraceDb`] is the primary entry point for ingestion, retrieval, inspection,
//! archive statistics, FTS maintenance, and native-source reconstruction.

mod facade;
pub mod service;

pub mod config;
pub mod model;
pub mod parsers;
pub mod search;
pub mod store;

pub mod proto {
    tonic::include_proto!("tracedb.v1");
}

pub use config::{
    default_config_path, ConfigOverrides, OutputFormat, TokenizerKind, TraceDbConfig,
};
pub use facade::{
    doctor_archive, doctor_configured, native_root, verify_archive, AgentIngestDryRunReport,
    AgentIngestReport, AgentStats, ArchiveStats, DoctorAgent, DoctorDatabase, DoctorFailure,
    DoctorReport, DoctorRuntime, DoctorTokenizer, IngestDryRunReport, IngestErrorCategory,
    IngestIssue, IngestReport, IngestRequest, IngestStage, ListPage, ListRequest,
    ReconstructionOptions, SessionSummary, SessionTrace, ShowRequest, TraceDb, VerificationFailure,
    VerifyCheck, VerifyReport,
};
pub use model::{
    Agent, Capture, Event, EventKind, IngestMode, NativeSource, ParsedSession, Session, TokenUsage,
};
pub use parsers::SessionCandidate;
pub use search::{ScoreBreakdown, SearchMatch, SearchRequest, SearchResult};

use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Resolve the database path using the public deployment contract.
/// `TRACEDB_PATH` always wins, including in tests and containers.
pub fn default_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("TRACEDB_PATH") {
        return PathBuf::from(path);
    }
    config::default_database_path()
}

pub fn open_database(path: impl AsRef<Path>) -> Result<Connection> {
    store::open(path)
}
