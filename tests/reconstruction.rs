use serde_json::{json, Value};
use tempfile::tempdir;
use tracedb::{Agent, IngestMode, IngestRequest, ReconstructionOptions, TraceDb};

fn ingest_one(root: &std::path::Path, agent: Agent) -> (tempfile::TempDir, TraceDb) {
    let dir = tempdir().unwrap();
    let mut db = TraceDb::open(dir.path().join("trace.db")).unwrap();
    let report = db
        .ingest(IngestRequest {
            agents: vec![agent],
            mode: IngestMode::Full,
            root: Some(root.to_path_buf()),
            since_ms: None,
            exclude: Vec::new(),
        })
        .unwrap();
    assert_eq!(report.total_discovered(), 1);
    assert_eq!(report.total_parsed(), 1);
    assert_eq!(report.total_ingested(), 1);
    (dir, db)
}

#[test]
fn reconstruction_preflights_conflicts_before_writing() {
    let fixtures = tempdir().unwrap();
    let first = fixtures.path().join("rollout-conflict.jsonl");
    let second = fixtures.path().join("sidecar.json");
    std::fs::write(
        &first,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"conflict\"}}\n",
    )
    .unwrap();
    std::fs::write(&second, "sidecar\n").unwrap();
    let archive = tempdir().unwrap();
    let db_path = archive.path().join("trace.db");
    let mut db = TraceDb::open(&db_path).unwrap();
    let report = db
        .ingest(IngestRequest {
            agents: vec![Agent::Codex],
            mode: IngestMode::Full,
            root: Some(fixtures.path().to_path_buf()),
            since_ms: None,
            exclude: Vec::new(),
        })
        .unwrap();
    assert_eq!(report.total_ingested(), 1);
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    let object_hash: String = connection
        .query_row("SELECT hash FROM objects LIMIT 1", [], |row| row.get(0))
        .unwrap();
    connection
        .execute(
            "INSERT INTO raw_sources(session_id,locator,kind,restore_path,object_hash) VALUES('codex:conflict','sidecar','json','sidecar.json',?1)",
            [&object_hash],
        )
        .unwrap();
    drop(connection);
    let output = archive.path().join("restore");
    std::fs::create_dir(&output).unwrap();
    std::fs::write(output.join("sidecar.json"), "existing\n").unwrap();

    let error = db.reconstruct("codex:conflict", &output).unwrap_err();

    assert!(error.to_string().contains("already exists"));
    assert!(!output.join("rollout-conflict.jsonl").exists());
    assert_eq!(
        std::fs::read(output.join("sidecar.json")).unwrap(),
        b"existing\n"
    );
}

#[test]
fn reconstruction_revalidates_objects_before_writing() {
    let fixtures = tempdir().unwrap();
    let source = fixtures.path().join("rollout-corrupt.jsonl");
    std::fs::write(
        &source,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"corrupt\"}}\n",
    )
    .unwrap();
    let (archive, db) = ingest_one(fixtures.path(), Agent::Codex);
    let connection = rusqlite::Connection::open(archive.path().join("trace.db")).unwrap();
    connection
        .execute("UPDATE objects SET bytes=bytes+1", [])
        .unwrap();
    drop(connection);
    let output = archive.path().join("restore");

    let error = db.reconstruct("codex:corrupt", &output).unwrap_err();

    assert!(error.to_string().contains("length mismatch"));
    assert!(!output.exists());
}

#[test]
fn reconstruction_requires_explicit_overwrite() {
    let fixtures = tempdir().unwrap();
    let source = fixtures.path().join("rollout-overwrite.jsonl");
    let native = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"overwrite\"}}\n";
    std::fs::write(&source, native).unwrap();
    let (archive, db) = ingest_one(fixtures.path(), Agent::Codex);
    let output = archive.path().join("restore");
    std::fs::create_dir(&output).unwrap();
    let target = output.join("rollout-overwrite.jsonl");
    std::fs::write(&target, "existing\n").unwrap();

    db.reconstruct_with_options(
        "codex:overwrite",
        &output,
        ReconstructionOptions { overwrite: true },
    )
    .unwrap();

    assert_eq!(std::fs::read_to_string(target).unwrap(), native);
}

#[cfg(unix)]
#[test]
fn reconstruction_preserves_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let fixtures = tempdir().unwrap();
    let source = fixtures.path().join("rollout-mode.jsonl");
    std::fs::write(
        &source,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"mode\"}}\n",
    )
    .unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
    let (archive, db) = ingest_one(fixtures.path(), Agent::Codex);
    let output = archive.path().join("restore");

    let paths = db.reconstruct("codex:mode", &output).unwrap();

    assert_eq!(
        std::fs::metadata(&paths[0]).unwrap().permissions().mode() & 0o777,
        0o640
    );
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
