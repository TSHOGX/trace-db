//! Versioned gRPC service backed by the same [`crate::TraceDb`] facade as the CLI.
//!
//! Read-only calls use a bounded pool of read-only SQLite connections. Mutating
//! calls share one writer, and both paths dispatch synchronous SQLite work to
//! blocking workers instead of tonic runtime threads.

use crate::{
    proto as pb, Agent, ArchiveStats, Event, IngestMode, IngestRequest, NativeSource,
    ReconstructionOptions, SearchRequest, Session, SessionTrace, TokenizerKind, TraceDb,
};
use anyhow::{bail, Context, Result};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};
use tokio::sync::Semaphore;
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
    database_path: Arc<PathBuf>,
    read_pool: ReadPool,
    reconstruct_root: Option<Arc<PathBuf>>,
}

const MAX_READ_CONNECTIONS: usize = 8;

#[derive(Clone)]
struct ReadPool {
    path: Arc<PathBuf>,
    tokenizer: TokenizerKind,
    tokenizer_extension: Option<Arc<PathBuf>>,
    idle: Arc<Mutex<Vec<TraceDb>>>,
    permits: Arc<Semaphore>,
}

impl ReadPool {
    fn new(
        path: Arc<PathBuf>,
        tokenizer: TokenizerKind,
        tokenizer_extension: Option<Arc<PathBuf>>,
    ) -> Self {
        let permits = thread::available_parallelism()
            .map(|parallelism| parallelism.get().clamp(2, MAX_READ_CONNECTIONS))
            .unwrap_or(2);
        Self {
            path,
            tokenizer,
            tokenizer_extension,
            idle: Arc::new(Mutex::new(Vec::new())),
            permits: Arc::new(Semaphore::new(permits)),
        }
    }

    async fn run<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&TraceDb) -> Result<T> + Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .context("acquire TraceDB read connection")?;
        let path = self.path.clone();
        let tokenizer = self.tokenizer;
        let tokenizer_extension = self.tokenizer_extension.clone();
        let idle = self.idle.clone();
        tokio::task::spawn_blocking(move || {
            let database = idle
                .lock()
                .map_err(|_| anyhow::anyhow!("TraceDB read pool lock is poisoned"))?
                .pop()
                .map(Ok)
                .unwrap_or_else(|| {
                    TraceDb::open_read_only_configured(
                        &*path,
                        tokenizer,
                        tokenizer_extension.as_deref().map(|path| path.as_path()),
                    )
                });
            let database = database?;
            let result = operation(&database);
            if let Ok(mut idle) = idle.lock() {
                if idle.len() < MAX_READ_CONNECTIONS {
                    idle.push(database);
                }
            }
            drop(permit);
            result
        })
        .await
        .context("TraceDB read worker failed")?
    }
}

impl TraceDbGrpc {
    pub fn new(database: TraceDb) -> Self {
        Self::configured(database, TokenizerKind::Unicode61, None, None)
    }

    pub fn configured(
        database: TraceDb,
        tokenizer: TokenizerKind,
        tokenizer_extension: Option<PathBuf>,
        reconstruct_root: Option<PathBuf>,
    ) -> Self {
        let database_path = Arc::new(database.path().to_path_buf());
        let tokenizer_extension = tokenizer_extension.map(Arc::new);
        Self {
            database: Arc::new(Mutex::new(database)),
            read_pool: ReadPool::new(database_path.clone(), tokenizer, tokenizer_extension),
            database_path,
            reconstruct_root: reconstruct_root.map(Arc::new),
        }
    }

    pub fn with_reconstruct_root(database: TraceDb, root: PathBuf) -> Self {
        Self::configured(database, TokenizerKind::Unicode61, None, Some(root))
    }

    async fn read<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&TraceDb) -> Result<T> + Send + 'static,
    {
        if self.database_path.as_os_str() == ":memory:" {
            let database = self.database.clone();
            return tokio::task::spawn_blocking(move || {
                let database = database
                    .lock()
                    .map_err(|_| anyhow::anyhow!("TraceDB connection lock is poisoned"))?;
                operation(&database)
            })
            .await
            .context("TraceDB in-memory read worker failed")?;
        }
        self.read_pool.run(operation).await
    }

    async fn write<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut TraceDb) -> Result<T> + Send + 'static,
    {
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || {
            let mut database = database
                .lock()
                .map_err(|_| anyhow::anyhow!("TraceDB connection lock is poisoned"))?;
            operation(&mut database)
        })
        .await
        .context("TraceDB write worker failed")?
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
        let ingest = IngestRequest {
            agents,
            mode,
            root: request.root.map(PathBuf::from),
            since_ms: request.since_ms,
            exclude: Vec::new(),
        };
        let report = self
            .write(move |database| database.ingest(ingest))
            .await
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
        let search = SearchRequest {
            query: request.query,
            limit: if request.limit == 0 {
                20
            } else {
                request.limit as usize
            },
            agent,
            cwd: request.cwd,
            since_ms: request.since_ms,
        };
        let results = self
            .read(move |database| database.search(search))
            .await
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
            .read(move |database| database.show(&id))
            .await
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
            .read(|database| database.stats())
            .await
            .map_err(internal)?;
        Ok(Response::new(stats_to_proto(stats)))
    }

    async fn reindex(
        &self,
        _request: Request<pb::ReindexRequest>,
    ) -> Result<Response<pb::ReindexResponse>, Status> {
        self.write(|database| database.reindex())
            .await
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
        let id = request.id;
        let overwrite = request.overwrite;
        let paths = self
            .write(move |database| {
                database.reconstruct_with_options(&id, out_dir, ReconstructionOptions { overwrite })
            })
            .await
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
    serve_configured(
        database,
        endpoint,
        TokenizerKind::Unicode61,
        None,
        reconstruct_root,
    )
}

pub fn serve_configured(
    database: TraceDb,
    endpoint: ServiceEndpoint,
    tokenizer: TokenizerKind,
    tokenizer_extension: Option<PathBuf>,
    reconstruct_root: Option<PathBuf>,
) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve_async(
            TraceDbGrpc::configured(database, tokenizer, tokenizer_extension, reconstruct_root),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Agent, Event, EventKind, IngestMode, ParsedSession, Session};
    use serde_json::json;
    use std::sync::{Arc, Barrier};
    use tempfile::tempdir;

    #[tokio::test]
    async fn read_requests_use_independent_connections() {
        let directory = tempdir().unwrap();
        let mut database = TraceDb::open(directory.path().join("trace.db")).unwrap();
        database
            .ingest_session(
                ParsedSession {
                    session: Session {
                        id: "codex:parallel".into(),
                        agent: Agent::Codex,
                        cwd: Some("/workspace/parallel".into()),
                        started_at_ms: Some(1),
                        ended_at_ms: Some(2),
                        title: Some("parallel reads".into()),
                        model: None,
                        provider: None,
                        git_branch: None,
                        parent_session_id: None,
                        forked_from: None,
                        meta: json!({}),
                        fingerprint: "parallel-v1".into(),
                        sources: Vec::new(),
                    },
                    events: vec![Event::new(EventKind::User, "parallel read")],
                },
                IngestMode::Partial,
            )
            .unwrap();
        let service = TraceDbGrpc::new(database);
        let barrier = Arc::new(Barrier::new(2));
        let first_barrier = barrier.clone();
        let second_barrier = barrier;
        let first = service.read(move |database| {
            first_barrier.wait();
            database.stats()
        });
        let second = service.read(move |database| {
            second_barrier.wait();
            database.search(SearchRequest::new("parallel"))
        });
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(first, second)
        })
        .await
        .expect("read requests must not serialize behind one connection");
        assert_eq!(first.unwrap().total_sessions, 1);
        assert_eq!(second.unwrap()[0].id, "codex:parallel");
    }

    #[tokio::test]
    async fn read_request_does_not_wait_for_the_writer() {
        let directory = tempdir().unwrap();
        let database = TraceDb::open(directory.path().join("trace.db")).unwrap();
        let service = TraceDbGrpc::new(database);
        let writer = service.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let write = tokio::spawn(async move {
            writer
                .write(move |_| {
                    let _ = started_tx.send(());
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();

        let stats = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            service.read(|database| database.stats()),
        )
        .await
        .expect("read request must not wait for the serialized writer")
        .unwrap();
        assert_eq!(stats.total_sessions, 0);
        write.await.unwrap().unwrap();
    }
}
