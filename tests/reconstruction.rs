use serde_json::{json, Value};
use tempfile::tempdir;
use tracedb::{Agent, IngestMode, IngestRequest, TraceDb};

fn ingest_one(root: &std::path::Path, agent: Agent) -> (tempfile::TempDir, TraceDb) {
    let dir = tempdir().unwrap();
    let mut db = TraceDb::open(dir.path().join("trace.db")).unwrap();
    let report = db
        .ingest(IngestRequest {
            agents: vec![agent],
            mode: IngestMode::Full,
            root: Some(root.to_path_buf()),
            since_ms: None,
        })
        .unwrap();
    assert_eq!(report.total_discovered(), 1);
    assert_eq!(report.total_parsed(), 1);
    assert_eq!(report.total_ingested(), 1);
    (dir, db)
}

#[test]
fn file_backed_agents_restore_exact_native_bytes() {
    let fixtures = tempdir().unwrap();
    let cases = [
        (
            Agent::Claude,
            "project/session.jsonl",
            "claude:fixture",
            json!({
                "sessionId":"fixture",
                "timestamp":"2026-08-19T00:00:00Z",
                "type":"user",
                "message":"hello claude"
            })
            .to_string(),
        ),
        (
            Agent::Codex,
            "rollout-fixture.jsonl",
            "codex:fixture",
            json!({
                "type":"session_meta",
                "timestamp":"2026-08-19T00:00:00Z",
                "payload":{"id":"fixture","cwd":"/tmp"}
            })
            .to_string(),
        ),
        (
            Agent::Gemini,
            "session-fixture.json",
            "gemini:fixture",
            json!({
                "sessionId":"fixture",
                "startTime":"2026-08-19T00:00:00Z",
                "lastUpdated":"2026-08-19T00:00:01Z",
                "messages":[{"id":"u","type":"user","content":"hello gemini"}]
            })
            .to_string(),
        ),
        (
            Agent::Pi,
            "session.jsonl",
            "pi:fixture",
            format!(
                "{}\n{}",
                json!({"type":"session","id":"fixture","cwd":"/workspace","timestamp":"2026-08-19T00:00:00Z"}),
                json!({"type":"message","id":"u","timestamp":"2026-08-19T00:00:01Z","message":{"role":"user","content":"hello pi"}})
            ),
        ),
    ];

    for (agent, relative, id, bytes) in cases {
        let root = fixtures.path().join(agent.to_string());
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let (archive_dir, db) = ingest_one(&root, agent);
        let output = archive_dir.path().join("restore");
        let restored = db.reconstruct(id, &output).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(
            std::fs::read(restored[0].as_path()).unwrap(),
            bytes.as_bytes()
        );
    }
}

#[test]
fn opencode_full_capture_restores_deterministic_session_bundle() {
    let fixture = tempdir().unwrap();
    let db_path = fixture.path().join("opencode.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, title TEXT, agent TEXT, model TEXT, time_created INTEGER, time_updated INTEGER);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session VALUES ('s1',NULL,'/workspace','Fixture','build','gpt-test',10,20)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message VALUES ('m1','s1',11,12,'{\"role\":\"user\"}')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part VALUES ('p1','m1','s1',13,14,'{\"type\":\"text\",\"text\":\"hello\"}')",
        [],
    )
    .unwrap();
    drop(conn);

    let (archive_dir, db) = ingest_one(&db_path, Agent::OpenCode);
    let output = archive_dir.path().join("restore");
    let restored = db.reconstruct("opencode:s1", &output).unwrap();
    assert_eq!(restored.len(), 1);
    let bundle: Value = serde_json::from_slice(&std::fs::read(&restored[0]).unwrap()).unwrap();
    assert_eq!(bundle["format"], "trace-db/opencode-session-v1");
    assert_eq!(bundle["session"]["id"], "s1");
    assert_eq!(bundle["message"][0]["data"]["role"], "user");
    assert_eq!(bundle["part"][0]["data"]["text"], "hello");
}
