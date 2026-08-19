use serde_json::json;
use tempfile::tempdir;
use tracedb::{
    Agent, Event, EventKind, IngestMode, ParsedSession, SearchRequest, Session, TraceDb,
};

#[test]
fn trace_db_opens_an_in_memory_archive() {
    let db = TraceDb::open(":memory:").unwrap();
    assert_eq!(db.stats().unwrap().total_sessions, 0);
}

#[test]
fn trace_db_facade_covers_archive_lifecycle() {
    let dir = tempdir().unwrap();
    let mut db = TraceDb::open(dir.path().join("trace.db")).unwrap();
    db.ingest_session(
        ParsedSession {
            session: Session {
                id: "codex:facade".into(),
                agent: Agent::Codex,
                cwd: Some("/workspace/demo".into()),
                started_at_ms: Some(10),
                ended_at_ms: Some(20),
                title: Some("Deploy demo".into()),
                model: None,
                provider: None,
                git_branch: None,
                parent_session_id: None,
                forked_from: None,
                meta: json!({}),
                fingerprint: "facade-v1".into(),
                sources: Vec::new(),
            },
            events: vec![Event::new(EventKind::User, "deploy the demo")],
        },
        IngestMode::Partial,
    )
    .unwrap();

    let hits = db.search(SearchRequest::new("deploy")).unwrap();
    assert_eq!(hits[0].id, "codex:facade");
    let trace = db.show("codex:facade").unwrap().unwrap();
    assert_eq!(trace.events[0].text, "deploy the demo");
    let stats = db.stats().unwrap();
    assert_eq!(stats.total_sessions, 1);
    assert_eq!(stats.total_events, 1);
    db.reindex().unwrap();
}
