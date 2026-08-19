//! Versioned gRPC service backed by the same [`crate::TraceDb`] facade as the CLI.

use crate::{
    proto as pb, Agent, ArchiveStats, Event, IngestMode, IngestRequest, NativeSource,
    ReconstructionOptions, SearchRequest, Session, SessionTrace, TraceDb,
};
use anyhow::{bail, Context, Result};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};
use tonic::{transport::Server, Request, Response, Status};

pub const GRPC_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceEndpoint {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

#[derive(Clone)]
pub struct TraceDbGrpc {
    database: Arc<Mutex<TraceDb>>,
    reconstruct_root: Option<Arc<PathBuf>>,
}

impl TraceDbGrpc {
    pub fn new(database: TraceDb) -> Self {
        Self {
            database: Arc::new(Mutex::new(database)),
            reconstruct_root: None,
        }
    }

    pub fn with_reconstruct_root(database: TraceDb, root: PathBuf) -> Self {
        Self {
            database: Arc::new(Mutex::new(database)),
            reconstruct_root: Some(Arc::new(root)),
        }
    }

    fn database(&self) -> Result<MutexGuard<'_, TraceDb>> {
        self.database
            .lock()
            .map_err(|_| anyhow::anyhow!("TraceDB connection lock is poisoned"))
    }

    pub fn into_server(self) -> pb::trace_db_service_server::TraceDbServiceServer<Self> {
        pb::trace_db_service_server::TraceDbServiceServer::new(self)
            .max_decoding_message_size(GRPC_MAX_MESSAGE_BYTES)
            .max_encoding_message_size(GRPC_MAX_MESSAGE_BYTES)
    }
}

#[tonic::async_trait]
impl pb::trace_db_service_server::TraceDbService for TraceDbGrpc {
    async fn ingest(
        &self,
        request: Request<pb::IngestRequest>,
    ) -> Result<Response<pb::IngestResponse>, Status> {
        let request = request.into_inner();
        let agents = request
            .agents
            .iter()
            .map(|agent| agent.parse::<Agent>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Status::invalid_argument)?;
        let mode = if request.mode.is_empty() {
            IngestMode::Partial
        } else {
            request
                .mode
                .parse::<IngestMode>()
                .map_err(Status::invalid_argument)?
        };
        let report = self
            .database()
            .map_err(internal)?
            .ingest(IngestRequest {
                agents,
                mode,
                root: request.root.map(PathBuf::from),
                since_ms: request.since_ms,
            })
            .map_err(internal)?;
        Ok(Response::new(pb::IngestResponse {
            total_discovered: report.total_discovered() as u64,
            total_ingested: report.total_ingested() as u64,
            total_parsed: report.total_parsed() as u64,
            total_unchanged: report.total_unchanged() as u64,
            total_skipped_by_since: report.total_skipped_by_since() as u64,
            total_skipped: report.total_skipped() as u64,
            total_failed: report.total_failed() as u64,
            total_warnings: report.total_warnings() as u64,
            agents: report
                .agents
                .into_iter()
                .map(|row| pb::AgentIngestReport {
                    agent: row.agent.to_string(),
                    root: row.root.display().to_string(),
                    discovered: row.discovered as u64,
                    parsed: row.parsed as u64,
                    ingested: row.ingested as u64,
                    unchanged: row.unchanged as u64,
                    skipped_by_since: row.skipped_by_since as u64,
                    skipped: row.skipped as u64,
                    failed: row.failed as u64,
                    warnings: row.warnings.into_iter().map(issue_to_proto).collect(),
                    failures: row.failures.into_iter().map(issue_to_proto).collect(),
                })
                .collect(),
        }))
    }

    async fn search(
        &self,
        request: Request<pb::SearchRequest>,
    ) -> Result<Response<pb::SearchResponse>, Status> {
        let request = request.into_inner();
        if request.query.trim().is_empty() {
            return Err(Status::invalid_argument("query must not be empty"));
        }
        let agent = request
            .agent
            .map(|agent| agent.parse::<Agent>())
            .transpose()
            .map_err(Status::invalid_argument)?;
        let results = self
            .database()
            .map_err(internal)?
            .search(SearchRequest {
                query: request.query,
                limit: if request.limit == 0 {
                    20
                } else {
                    request.limit as usize
                },
                agent,
                cwd: request.cwd,
                since_ms: request.since_ms,
            })
            .map_err(internal)?
            .into_iter()
            .map(|row| pb::SearchResult {
                id: row.id,
                agent: row.agent.to_string(),
                cwd: row.cwd,
                hits: row.hits,
                lineage_root_id: row.lineage_root_id,
                title: row.title,
                started_at_ms: row.started_at_ms,
                ended_at_ms: row.ended_at_ms,
                score: row.score,
                score_breakdown: Some(pb::ScoreBreakdown {
                    best_match: row.score_breakdown.best_match,
                    hit_coverage: row.score_breakdown.hit_coverage,
                    term_coverage: row.score_breakdown.term_coverage,
                    kind: row.score_breakdown.kind,
                    recency: row.score_breakdown.recency,
                    title: row.score_breakdown.title,
                    lineage: row.score_breakdown.lineage,
                }),
                best_match: Some(pb::SearchMatch {
                    event_idx: row.best_match.event_idx,
                    kind: row.best_match.kind.to_string(),
                    bm25: row.best_match.bm25,
                    snippet: row.best_match.snippet,
                }),
                ask: row.ask,
                outcome: row.outcome,
                related_session_ids: row.related_session_ids,
            })
            .collect();
        Ok(Response::new(pb::SearchResponse { results }))
    }

    async fn show(
        &self,
        request: Request<pb::ShowRequest>,
    ) -> Result<Response<pb::ShowResponse>, Status> {
        let id = request.into_inner().id;
        if id.is_empty() {
            return Err(Status::invalid_argument("id must not be empty"));
        }
        let response = self
            .database()
            .map_err(internal)?
            .show(&id)
            .map_err(internal)?
            .map(trace_to_proto)
            .transpose()
            .map_err(internal)?
            .unwrap_or_default();
        Ok(Response::new(response))
    }

    async fn stats(
        &self,
        _request: Request<pb::StatsRequest>,
    ) -> Result<Response<pb::StatsResponse>, Status> {
        let stats = self
            .database()
            .map_err(internal)?
            .stats()
            .map_err(internal)?;
        Ok(Response::new(stats_to_proto(stats)))
    }

    async fn reindex(
        &self,
        _request: Request<pb::ReindexRequest>,
    ) -> Result<Response<pb::ReindexResponse>, Status> {
        self.database()
            .map_err(internal)?
            .reindex()
            .map_err(internal)?;
        Ok(Response::new(pb::ReindexResponse {}))
    }

    async fn reconstruct(
        &self,
        request: Request<pb::ReconstructRequest>,
    ) -> Result<Response<pb::ReconstructResponse>, Status> {
        let request = request.into_inner();
        if request.id.is_empty() || request.out_dir.is_empty() {
            return Err(Status::invalid_argument("id and out_dir must not be empty"));
        }
        let root = self.reconstruct_root.as_deref().ok_or_else(|| {
            Status::permission_denied(
                "reconstruction is disabled; configure a server reconstruction root",
            )
        })?;
        let relative = std::path::Path::new(&request.out_dir);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(Status::invalid_argument(
                "out_dir must be a safe path relative to the configured reconstruction root",
            ));
        }
        let out_dir = resolve_reconstruct_out(root, relative)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let paths = self
            .database()
            .map_err(internal)?
            .reconstruct_with_options(
                &request.id,
                out_dir,
                ReconstructionOptions {
                    overwrite: request.overwrite,
                },
            )
            .map_err(internal)?
            .into_iter()
            .map(|path| path.display().to_string())
            .collect();
        Ok(Response::new(pb::ReconstructResponse { paths }))
    }
}

fn resolve_reconstruct_out(root: &PathBuf, relative: &std::path::Path) -> Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    let canonical_root = std::fs::canonicalize(root)?;
    let candidate = canonical_root.join(relative);
    let mut ancestor = candidate.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow::anyhow!("reconstruction path has no existing ancestor"))?;
    }
    let canonical_ancestor = std::fs::canonicalize(ancestor)?;
    if !canonical_ancestor.starts_with(&canonical_root) {
        bail!("out_dir resolves outside the configured reconstruction root");
    }
    Ok(candidate)
}

fn issue_to_proto(issue: crate::IngestIssue) -> pb::IngestIssue {
    pb::IngestIssue {
        stage: issue.stage.to_string(),
        locator: issue.locator,
        category: issue.category.to_string(),
        message: issue.message,
    }
}

pub fn serve(
    database: TraceDb,
    endpoint: ServiceEndpoint,
    reconstruct_root: Option<PathBuf>,
) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve_async(
            match reconstruct_root {
                Some(root) => TraceDbGrpc::with_reconstruct_root(database, root),
                None => TraceDbGrpc::new(database),
            },
            endpoint,
        ))
}

async fn serve_async(service: TraceDbGrpc, endpoint: ServiceEndpoint) -> Result<()> {
    match endpoint {
        ServiceEndpoint::Tcp(address) => Server::builder()
            .add_service(service.into_server())
            .serve_with_shutdown(address, shutdown_signal())
            .await
            .context("serve TraceDB gRPC over TCP")?,
        ServiceEndpoint::Unix(path) => serve_unix(service, path).await?,
    }
    Ok(())
}

#[cfg(unix)]
async fn serve_unix(service: TraceDbGrpc, path: PathBuf) -> Result<()> {
    use tokio::net::UnixListener;
    use tokio_stream::wrappers::UnixListenerStream;

    if path.exists() {
        bail!("Unix socket already exists: {}", path.display());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind Unix socket {}", path.display()))?;
    let result = Server::builder()
        .add_service(service.into_server())
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), shutdown_signal())
        .await;
    let _ = std::fs::remove_file(&path);
    result.context("serve TraceDB gRPC over Unix socket")?;
    Ok(())
}

#[cfg(not(unix))]
async fn serve_unix(_service: TraceDbGrpc, path: PathBuf) -> Result<()> {
    bail!(
        "Unix sockets are not supported on this platform: {}",
        path.display()
    )
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn internal(error: anyhow::Error) -> Status {
    Status::internal(error.to_string())
}

fn trace_to_proto(trace: SessionTrace) -> Result<pb::ShowResponse> {
    Ok(pb::ShowResponse {
        session: Some(session_to_proto(trace.session)),
        mode: trace.mode.to_string(),
        events: trace
            .events
            .into_iter()
            .map(event_to_proto)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn session_to_proto(session: Session) -> pb::Session {
    pb::Session {
        id: session.id,
        agent: session.agent.to_string(),
        cwd: session.cwd,
        started_at_ms: session.started_at_ms,
        ended_at_ms: session.ended_at_ms,
        title: session.title,
        model: session.model,
        provider: session.provider,
        git_branch: session.git_branch,
        parent_session_id: session.parent_session_id,
        forked_from: session.forked_from,
        meta_json: session.meta.to_string(),
        fingerprint: session.fingerprint,
        sources: session.sources.into_iter().map(source_to_proto).collect(),
    }
}

fn source_to_proto(source: NativeSource) -> pb::NativeSource {
    pb::NativeSource {
        locator: source.locator,
        kind: source.kind,
        restore_path: source.restore_path,
        role: source.role,
        bytes: source.bytes,
        mtime_ns: source.mtime_ns,
        mode: source.mode,
    }
}

fn event_to_proto(event: Event) -> Result<pb::Event> {
    Ok(pb::Event {
        idx: event.idx,
        kind: event.kind.to_string(),
        subtype: event.subtype,
        role: event.role,
        name: event.name,
        call_id: event.call_id,
        is_error: event.is_error,
        native_id: event.native_id,
        parent_id: event.parent_id,
        model: event.model,
        provider: event.provider,
        usage_json: event
            .usage
            .map(|usage| serde_json::to_string(&usage))
            .transpose()
            .context("serialize event token usage")?,
        text: event.text,
        data_json: event.data_json.map(|data| data.to_string()),
        created_at_ms: event.created_at_ms,
    })
}

fn stats_to_proto(stats: ArchiveStats) -> pb::StatsResponse {
    pb::StatsResponse {
        path: stats.path.display().to_string(),
        total_sessions: stats.total_sessions,
        total_events: stats.total_events,
        total_full_sessions: stats.total_full_sessions,
        agents: stats
            .agents
            .into_iter()
            .map(|row| pb::AgentStats {
                agent: row.agent.to_string(),
                sessions: row.sessions,
                events: row.events,
                full_sessions: row.full_sessions,
            })
            .collect(),
    }
}
