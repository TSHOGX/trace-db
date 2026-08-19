use serde_json::json;
use tempfile::tempdir;
use tracedb::{
    Agent, Event, EventKind, IngestErrorCategory, IngestMode, IngestRequest, IngestStage,
    ParsedSession, SearchRequest, Session, TraceDb,
};

#[test]
fn trace_db_opens_an_in_memory_archive() {
    let db = TraceDb::open(":memory:").unwrap();
    assert_eq!(db.stats().unwrap().total_sessions, 0);
}

#[test]
fn native_ingest_reports_corrupt_candidates_and_continues() {
    let dir = tempdir().unwrap();
    let native = dir.path().join("native");
    std::fs::create_dir(&native).unwrap();
    std::fs::write(
        native.join("rollout-good.jsonl"),
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"good\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n"
        ),
    )
    .unwrap();
    let corrupt = native.join("rollout-corrupt.jsonl");
    std::fs::write(
        &corrupt,
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"bad\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":\n"
        ),
    )
    .unwrap();
    let mut db = TraceDb::open(dir.path().join("trace.db")).unwrap();

    let report = db
        .ingest(IngestRequest {
            agents: vec![Agent::Codex],
            mode: IngestMode::Partial,
            root: Some(native),
            since_ms: None,
        })
        .unwrap();

    assert_eq!(report.total_discovered(), 2);
    assert_eq!(report.total_parsed(), 1);
    assert_eq!(report.total_ingested(), 1);
    assert_eq!(report.total_failed(), 1);
    assert_eq!(report.agents[0].failures[0].stage, IngestStage::Parsing);
    assert_eq!(
        report.agents[0].failures[0].category,
        IngestErrorCategory::CorruptData
    );
    assert_eq!(
        report.agents[0].failures[0].locator,
        corrupt.display().to_string()
    );
    assert!(report.agents[0].failures[0].message.contains("line 2"));
    assert!(db.show("codex:good").unwrap().is_some());
    assert!(db.show("codex:bad").unwrap().is_none());
}

#[test]
fn native_ingest_distinguishes_unsupported_format() {
    let dir = tempdir().unwrap();
    let native = dir.path().join("native");
    std::fs::create_dir(&native).unwrap();
    std::fs::write(
        native.join("rollout-unknown.jsonl"),
        "{\"type\":\"future_record\",\"payload\":{}}\n",
    )
    .unwrap();
    let mut db = TraceDb::open(dir.path().join("trace.db")).unwrap();

    let report = db
        .ingest(IngestRequest {
            agents: vec![Agent::Codex],
            mode: IngestMode::Partial,
            root: Some(native),
            since_ms: None,
        })
        .unwrap();

    assert_eq!(report.total_failed(), 1);
    assert_eq!(
        report.agents[0].failures[0].category,
        IngestErrorCategory::UnsupportedFormat
    );
}

#[cfg(unix)]
#[test]
fn native_ingest_reports_permission_failures() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let native = dir.path().join("native");
    std::fs::create_dir(&native).unwrap();
    let source = native.join("rollout-private.jsonl");
    std::fs::write(
        &source,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"private\"}}\n",
    )
    .unwrap();
    let original_permissions = std::fs::metadata(&source).unwrap().permissions();
    let mut private_permissions = original_permissions.clone();
    private_permissions.set_mode(0o000);
    std::fs::set_permissions(&source, private_permissions).unwrap();
    let mut db = TraceDb::open(dir.path().join("trace.db")).unwrap();

    let report = db
        .ingest(IngestRequest {
            agents: vec![Agent::Codex],
            mode: IngestMode::Partial,
            root: Some(native),
            since_ms: None,
        })
        .unwrap();
    std::fs::set_permissions(&source, original_permissions).unwrap();

    assert_eq!(report.total_failed(), 1);
    assert_eq!(
        report.agents[0].failures[0].category,
        IngestErrorCategory::Permission
    );
    assert_eq!(
        report.agents[0].failures[0].locator,
        source.display().to_string()
    );
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
            events: vec![
                Event::new(EventKind::User, "deploy the demo"),
                Event::new(EventKind::Assistant, "the demo is deployed"),
            ],
        },
        IngestMode::Partial,
    )
    .unwrap();

    let hits = db.search(SearchRequest::new("deploy")).unwrap();
    assert_eq!(hits[0].id, "codex:facade");
    assert_eq!(hits[0].ask.as_deref(), Some("deploy the demo"));
    assert_eq!(hits[0].outcome.as_deref(), Some("the demo is deployed"));
    assert_eq!(hits[0].best_match.kind, EventKind::User);
    assert!(hits[0].score > 0.0);
    let trace = db.show("codex:facade").unwrap().unwrap();
    assert_eq!(trace.events[0].text, "deploy the demo");
    let stats = db.stats().unwrap();
    assert_eq!(stats.total_sessions, 1);
    assert_eq!(stats.total_events, 2);
    db.reindex().unwrap();
}

#[test]
fn native_ingest_skips_unchanged_sessions_before_parsing() {
    let dir = tempdir().unwrap();
    let native = dir.path().join("native");
    std::fs::create_dir(&native).unwrap();
    let source = native.join("session-incremental.json");
    std::fs::write(
        &source,
        json!({
            "sessionId": "incremental",
            "startTime": "2026-08-19T00:00:00Z",
            "lastUpdated": "2026-08-19T00:00:01Z",
            "messages": [{"id":"u","type":"user","content":"hello"}]
        })
        .to_string(),
    )
    .unwrap();
    let mut db = TraceDb::open(dir.path().join("trace.db")).unwrap();
    let request = |mode, since_ms| IngestRequest {
        agents: vec![Agent::Gemini],
        mode,
        root: Some(native.clone()),
        since_ms,
    };

    let first = db.ingest(request(IngestMode::Partial, None)).unwrap();
    assert_eq!(first.total_discovered(), 1);
    assert_eq!(first.total_parsed(), 1);
    assert_eq!(first.total_ingested(), 1);
    assert_eq!(first.total_unchanged(), 0);

    let unchanged = db.ingest(request(IngestMode::Partial, None)).unwrap();
    assert_eq!(unchanged.total_parsed(), 0);
    assert_eq!(unchanged.total_ingested(), 0);
    assert_eq!(unchanged.total_unchanged(), 1);

    let upgraded = db.ingest(request(IngestMode::Full, None)).unwrap();
    assert_eq!(upgraded.total_parsed(), 1);
    assert_eq!(upgraded.total_ingested(), 1);
    assert_eq!(
        db.show("gemini:incremental").unwrap().unwrap().mode,
        IngestMode::Full
    );

    std::fs::write(
        &source,
        json!({
            "sessionId": "incremental",
            "startTime": "2026-08-19T00:00:00Z",
            "lastUpdated": "2026-08-19T00:00:02Z",
            "messages": [
                {"id":"u","type":"user","content":"hello"},
                {"id":"a","type":"gemini","content":"world"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let changed = db.ingest(request(IngestMode::Partial, None)).unwrap();
    assert_eq!(changed.total_parsed(), 1);
    assert_eq!(changed.total_ingested(), 1);
    let trace = db.show("gemini:incremental").unwrap().unwrap();
    assert_eq!(trace.mode, IngestMode::Full);
    assert_eq!(trace.events.len(), 2);

    let skipped = db
        .ingest(request(IngestMode::Partial, Some(i64::MAX)))
        .unwrap();
    assert_eq!(skipped.total_parsed(), 0);
    assert_eq!(skipped.total_unchanged(), 0);
    assert_eq!(skipped.total_skipped_by_since(), 1);
}
