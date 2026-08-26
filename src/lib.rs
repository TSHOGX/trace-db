//! TraceDB's embeddable Rust API.
//!
//! [`TraceDb`] is the primary entry point for ingestion, retrieval, inspection,
//! archive statistics, FTS maintenance, and native-source reconstruction.

mod facade;
#[cfg(feature = "grpc")]
pub mod service;

pub mod benchmark;
pub mod config;
pub mod model;
pub mod parsers;
pub mod relevance;
pub mod search;
pub mod store;

/// Generated `tracedb.v1` types, client, and server.
///
/// Gated on `grpc` together with the proto build step: `include_proto!` reads a
/// file `build.rs` only writes when that feature is on, so the module and its
/// generator are one unit and cannot be enabled apart.
#[cfg(feature = "grpc")]
#[allow(clippy::result_large_err)]
pub mod proto {
    tonic::include_proto!("tracedb.v1");
}

pub use config::{
    default_config_path, ConfigOverrides, OutputFormat, TokenizerKind, TraceDbConfig,
};
pub use facade::{
    doctor_archive, doctor_configured, native_root, verify_archive, AgentIngestDryRunReport,
    AgentIngestReport, AgentStats, ArchiveStats, BackupReport, DoctorAgent, DoctorBackup,
    DoctorDatabase, DoctorFailure, DoctorIngestStatus, DoctorPermissions, DoctorReport,
    DoctorRuntime, DoctorTokenizer, DoctorWatch, GcReport, ImportReport, IngestAck,
    IngestDryRunReport, IngestErrorCategory, IngestIssue, IngestReport, IngestRequest, IngestStage,
    ListPage, ListRequest, ReconstructionOptions, RestoreManifest, RestoreManifestFile,
    SessionCoverage, SessionSummary, SessionTrace, ShowRequest, TraceDb, VerificationFailure,
    VerifyCheck, VerifyReport, WatchEvent, WatchIssue, WatchIssueStage, WatchRequest, WatchRun,
    WatchSummary, WatchTrigger, RESTORE_MANIFEST_SCHEMA_VERSION,
};
pub use model::{
    Agent, Capture, Event, EventKind, EventParentKind, NativeSource, ParsedSession, Session,
    SessionRelation, SessionStatus, Span, SpanKind, SpanStatus, TokenUsage,
};
pub use parsers::SessionCandidate;
// Re-exported at the root because it is an error a consumer is expected to match
// on: `open_or_rebuild` recovers from it internally, and a host driving `open`
// itself has to name the type to `downcast_ref` it.
pub use search::{ScoreBreakdown, SearchMatch, SearchRequest, SearchResult};
pub use store::SchemaVersionMismatch;

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
