use pyo3::{exceptions::PyRuntimeError, prelude::*};
use serde::Serialize;
use std::path::PathBuf;
use tracedb::{
    Agent, IngestMode, IngestRequest, ListRequest, ReconstructionOptions, SearchRequest, TraceDb,
};

fn runtime_error(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

fn json<T: Serialize>(value: T) -> PyResult<String> {
    serde_json::to_string(&value).map_err(runtime_error)
}

/// A small Python wrapper around the same Rust facade used by the CLI and gRPC service.
#[pyclass(unsendable, module = "tracedb._native")]
struct PyTraceDb {
    db: TraceDb,
}

#[pymethods]
impl PyTraceDb {
    /// Open an archive path, or the default path when omitted.
    #[staticmethod]
    #[pyo3(signature = (path=None))]
    fn open(path: Option<String>) -> PyResult<Self> {
        let db = match path {
            Some(path) => TraceDb::open(path),
            None => TraceDb::open_default(),
        }
        .map_err(runtime_error)?;
        Ok(Self { db })
    }

    /// Return archive statistics as a JSON object string.
    fn stats_json(&self) -> PyResult<String> {
        json(self.db.stats().map_err(runtime_error)?)
    }

    /// Search normalized events and return a JSON array string.
    #[pyo3(signature = (query, limit=20, agent=None, cwd=None, since_ms=None))]
    fn search_json(
        &self,
        query: String,
        limit: usize,
        agent: Option<String>,
        cwd: Option<String>,
        since_ms: Option<i64>,
    ) -> PyResult<String> {
        let agent = agent
            .map(|value| value.parse::<Agent>().map_err(runtime_error))
            .transpose()?;
        json(
            self.db
                .search(SearchRequest {
                    query,
                    limit,
                    agent,
                    cwd,
                    since_ms,
                })
                .map_err(runtime_error)?,
        )
    }

    /// List archived sessions with cursor pagination and metadata filters.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (limit=50, cursor=None, agent=None, cwd=None, since_ms=None, mode=None, model=None, provider=None))]
    fn list_json(
        &self,
        limit: usize,
        cursor: Option<String>,
        agent: Option<String>,
        cwd: Option<String>,
        since_ms: Option<i64>,
        mode: Option<String>,
        model: Option<String>,
        provider: Option<String>,
    ) -> PyResult<String> {
        let agent = agent
            .map(|value| value.parse::<Agent>().map_err(runtime_error))
            .transpose()?;
        let mode = mode
            .map(|value| value.parse::<IngestMode>().map_err(runtime_error))
            .transpose()?;
        json(
            self.db
                .list(ListRequest {
                    limit,
                    cursor,
                    agent,
                    cwd,
                    since_ms,
                    mode,
                    model,
                    provider,
                })
                .map_err(runtime_error)?,
        )
    }

    /// Ingest native sessions and return the typed report as a JSON object string.
    #[pyo3(signature = (agents=None, mode="partial", root=None, since_ms=None))]
    fn ingest_json(
        &mut self,
        agents: Option<Vec<String>>,
        mode: &str,
        root: Option<String>,
        since_ms: Option<i64>,
    ) -> PyResult<String> {
        let agents = agents
            .unwrap_or_default()
            .into_iter()
            .map(|value| value.parse::<Agent>().map_err(runtime_error))
            .collect::<PyResult<Vec<_>>>()?;
        let mode = mode.parse::<IngestMode>().map_err(runtime_error)?;
        json(
            self.db
                .ingest(IngestRequest {
                    agents,
                    mode,
                    root: root.map(PathBuf::from),
                    since_ms,
                    exclude: Vec::new(),
                })
                .map_err(runtime_error)?,
        )
    }

    /// Return one normalized session trace as a JSON object, or null when absent.
    fn show_json(&self, session_id: String) -> PyResult<String> {
        json(self.db.show(&session_id).map_err(runtime_error)?)
    }

    /// Reconstruct full-capture native sources and return their paths as JSON.
    #[pyo3(signature = (session_id, out_dir, overwrite=false))]
    fn reconstruct_json(
        &self,
        session_id: String,
        out_dir: String,
        overwrite: bool,
    ) -> PyResult<String> {
        json(
            self.db
                .reconstruct_with_options(&session_id, out_dir, ReconstructionOptions { overwrite })
                .map_err(runtime_error)?
                .into_iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>(),
        )
    }

    /// Rebuild the full-text index.
    fn reindex(&self) -> PyResult<()> {
        self.db.reindex().map_err(runtime_error)
    }
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyTraceDb>()?;
    Ok(())
}
