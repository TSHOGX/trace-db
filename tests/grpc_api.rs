use serde_json::json;
use tempfile::tempdir;
use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{transport::Server, Code};
use tracedb::service::TraceDbGrpc;
use tracedb::{
    proto::{
        trace_db_service_client::TraceDbServiceClient, ReconstructRequest,
        SearchRequest as ProtoSearchRequest, ShowRequest, StatsRequest,
    },
    Agent, Event, EventKind, IngestMode, ParsedSession, Session, TraceDb,
};

#[tokio::test]
async fn grpc_round_trip_uses_the_versioned_contract() {
    let dir = tempdir().unwrap();
    let mut database = TraceDb::open(dir.path().join("trace.db")).unwrap();
    database
        .ingest_session(
            ParsedSession {
                session: Session {
                    id: "codex:grpc".into(),
                    agent: Agent::Codex,
                    cwd: Some("/workspace/grpc".into()),
                    started_at_ms: Some(10),
                    ended_at_ms: Some(20),
                    title: Some("gRPC contract".into()),
                    model: None,
                    provider: None,
                    git_branch: None,
                    parent_session_id: None,
                    forked_from: None,
                    meta: json!({}),
                    fingerprint: "grpc-v1".into(),
                    sources: Vec::new(),
                },
                events: vec![Event::new(EventKind::User, "deploy over grpc")],
            },
            IngestMode::Partial,
        )
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(TraceDbGrpc::new(database).into_server())
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let mut client = TraceDbServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let stats = client.stats(StatsRequest {}).await.unwrap().into_inner();
    assert_eq!(stats.total_sessions, 1);
    assert_eq!(stats.total_events, 1);

    let search = client
        .search(ProtoSearchRequest {
            query: "deploy".into(),
            limit: 10,
            agent: Some("codex".into()),
            cwd: None,
            since_ms: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(search.results[0].id, "codex:grpc");
    assert_eq!(search.results[0].lineage_root_id, "codex:grpc");
    assert!(search.results[0].score > 0.0);
    assert!(search.results[0].best_match.is_some());

    let show = client
        .show(ShowRequest {
            id: "codex:grpc".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        show.session.unwrap().title.as_deref(),
        Some("gRPC contract")
    );
    assert_eq!(show.events[0].text, "deploy over grpc");

    let error = client
        .search(ProtoSearchRequest::default())
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);

    let reconstruct_error = client
        .reconstruct(ReconstructRequest {
            id: "codex:grpc".into(),
            out_dir: "restore".into(),
            overwrite: false,
        })
        .await
        .unwrap_err();
    assert_eq!(reconstruct_error.code(), Code::PermissionDenied);

    shutdown_tx.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn grpc_reconstruction_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let database = TraceDb::open(dir.path().join("trace.db")).unwrap();
    let root = dir.path().join("restore-root");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, root.join("escape")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(TraceDbGrpc::with_reconstruct_root(database, root).into_server())
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            })
            .await
    });
    let mut client = TraceDbServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();

    let error = client
        .reconstruct(ReconstructRequest {
            id: "codex:missing".into(),
            out_dir: "escape/target".into(),
            overwrite: false,
        })
        .await
        .unwrap_err();

    assert_eq!(error.code(), Code::InvalidArgument);
    assert!(error.message().contains("outside"));
    shutdown_tx.send(()).unwrap();
    server.await.unwrap().unwrap();
}
