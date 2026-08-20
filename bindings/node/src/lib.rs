use napi::{Error, Result, Status};
use napi_derive::napi;
use serde::Serialize;
use std::path::PathBuf;
use tracedb::{
    Agent, IngestMode, IngestRequest, ListRequest, ReconstructionOptions, SearchRequest, TraceDb,
};

fn native_error(error: impl std::fmt::Display) -> Error {
    Error::new(Status::GenericFailure, error.to_string())
}

fn json<T: Serialize>(value: T) -> Result<String> {
    serde_json::to_string(&value).map_err(native_error)
}

/// A small Node.js wrapper around the same Rust facade used by the CLI and gRPC service.
#[napi(js_name = "TraceDb")]
pub struct NodeTraceDb {
    db: TraceDb,
}

#[napi]
impl NodeTraceDb {
    /// Open an archive path, or the default path when omitted.
    #[napi(factory)]
    pub fn open(path: Option<String>) -> Result<Self> {
        let db = match path {
            Some(path) => TraceDb::open(path),
            None => TraceDb::open_default(),
        }
        .map_err(native_error)?;
        Ok(Self { db })
    }

    /// Return archive statistics as a JSON object string.
    #[napi]
    pub fn stats_json(&self) -> Result<String> {
        json(self.db.stats().map_err(native_error)?)
    }

    /// Search normalized events and return a JSON array string.
    #[napi]
    pub fn search_json(
        &self,
        query: String,
        limit: Option<u32>,
        agent: Option<String>,
        cwd: Option<String>,
        since_ms: Option<i64>,
    ) -> Result<String> {
        let agent = agent
            .map(|value| value.parse::<Agent>().map_err(native_error))
            .transpose()?;
        json(
            self.db
                .search(SearchRequest {
                    query,
                    limit: limit.unwrap_or(20) as usize,
                    agent,
                    cwd,
                    since_ms,
                })
                .map_err(native_error)?,
        )
    }

    /// List archived sessions with cursor pagination and metadata filters.
    #[allow(clippy::too_many_arguments)]
    #[napi]
    pub fn list_json(
        &self,
        limit: Option<u32>,
        cursor: Option<String>,
        agent: Option<String>,
        cwd: Option<String>,
        since_ms: Option<i64>,
        mode: Option<String>,
        model: Option<String>,
        provider: Option<String>,
    ) -> Result<String> {
        let agent = agent
            .map(|value| value.parse::<Agent>().map_err(native_error))
            .transpose()?;
        let mode = mode
            .map(|value| value.parse::<IngestMode>().map_err(native_error))
            .transpose()?;
        json(
            self.db
                .list(ListRequest {
                    limit: limit.unwrap_or(50) as usize,
                    cursor,
                    agent,
                    cwd,
                    since_ms,
                    mode,
                    model,
                    provider,
                })
                .map_err(native_error)?,
        )
    }

    /// Ingest native sessions and return the typed report as a JSON object string.
    #[napi]
    pub fn ingest_json(
        &mut self,
        agents: Option<Vec<String>>,
        mode: Option<String>,
        root: Option<String>,
        since_ms: Option<i64>,
    ) -> Result<String> {
        let agents = agents
            .unwrap_or_default()
            .into_iter()
            .map(|value| value.parse::<Agent>().map_err(native_error))
            .collect::<Result<Vec<_>>>()?;
        let mode = mode
            .as_deref()
            .unwrap_or("full")
            .parse::<IngestMode>()
            .map_err(native_error)?;
        json(
            self.db
                .ingest(IngestRequest {
                    agents,
                    mode,
                    root: root.map(PathBuf::from),
                    since_ms,
                    exclude: Vec::new(),
                })
                .map_err(native_error)?,
        )
    }

    /// Return one normalized session trace as JSON, or null when absent.
    #[napi]
    pub fn show_json(&self, session_id: String) -> Result<String> {
        json(self.db.show(&session_id).map_err(native_error)?)
    }

    /// Reconstruct full-capture native sources and return their paths as JSON.
    #[napi]
    pub fn reconstruct_json(&self, session_id: String, out_dir: String) -> Result<String> {
        self.reconstruct_json_with_options(session_id, out_dir, false)
    }

    /// Reconstruct full-capture native sources with explicit overwrite handling.
    #[napi]
    pub fn reconstruct_json_with_options(
        &self,
        session_id: String,
        out_dir: String,
        overwrite: bool,
    ) -> Result<String> {
        json(
            self.db
                .reconstruct_with_options(&session_id, out_dir, ReconstructionOptions { overwrite })
                .map_err(native_error)?
                .into_iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>(),
        )
    }

    /// Rebuild the full-text index.
    #[napi]
    pub fn reindex(&self) -> Result<()> {
        self.db.reindex().map_err(native_error)
    }
}
