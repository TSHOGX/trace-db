use rusqlite::{params, Connection};
use serde_json::json;
use std::{
    sync::{Arc, Barrier},
    thread,
    time::Duration,
};
use tempfile::tempdir;
use tracedb::{
    open_database, Agent, Capture, Event, EventKind, IngestMode, NativeSource, ParsedSession,
    Session, TraceDb,
};

fn parsed_session(id: &str, sources: Vec<NativeSource>) -> ParsedSession {
    ParsedSession {
        session: Session {
            id: id.into(),
            agent: Agent::Codex,
            cwd: Some("/workspace".into()),
            started_at_ms: Some(1),
            ended_at_ms: Some(2),
            title: Some("Lifecycle test".into()),
            model: None,
            provider: None,
            git_branch: None,
            parent_session_id: None,
            forked_from: None,
            meta: json!({}),
            fingerprint: format!("{id}-fingerprint"),
            sources,
        },
        events: vec![Event::new(EventKind::User, "transactional lifecycle")],
    }
}

#[test]
fn concurrent_writer_waits_for_the_configured_sqlite_busy_timeout_window() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("busy.db");
    let first = open_database(&path).unwrap();
    let second = open_database(&path).unwrap();
    let timeout: i64 = first
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(timeout, 5_000);

    first.execute_batch("BEGIN IMMEDIATE").unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let writer_barrier = Arc::clone(&barrier);
    let writer = thread::spawn(move || {
        writer_barrier.wait();
        let started = std::time::Instant::now();
        second
            .execute(
                "INSERT INTO sessions(id,agent,mode,fingerprint,meta_json,ingested_at_ms) VALUES (?1,'codex','partial','busy','{}',0)",
                params!["codex:busy"],
            )
            .unwrap();
        started.elapsed()
    });
    barrier.wait();
    thread::sleep(Duration::from_millis(100));
    first.execute_batch("COMMIT").unwrap();

    let waited = writer.join().unwrap();
    assert!(
        waited >= Duration::from_millis(75),
        "contending writer did not observe the lock: {waited:?}"
    );
}

#[test]
fn failed_full_capture_rolls_back_session_and_events() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("rollback.db");
    let mut database = TraceDb::open(&path).unwrap();
    let missing = directory.path().join("missing-native.jsonl");
    let source = NativeSource {
        locator: missing.display().to_string(),
        kind: "jsonl".into(),
        restore_path: "missing-native.jsonl".into(),
        role: None,
        bytes: None,
        mtime_ns: None,
        mode: None,
        capture: Some(Capture::File {
            path: missing.display().to_string(),
        }),
    };

    assert!(database
        .ingest_session(
            parsed_session("codex:rollback", vec![source]),
            IngestMode::Full
        )
        .is_err());
    drop(database);

    let connection: Connection = open_database(&path).unwrap();
    for table in ["sessions", "events", "raw_sources"] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "failed ingest left rows in {table}");
    }
}
