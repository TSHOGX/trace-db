use serde_json::Value;
use std::process::Command;
use tempfile::tempdir;
use tracedb::{verify_archive, Agent, IngestMode, IngestRequest, TraceDb};

fn full_archive() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let native = dir.path().join("native");
    std::fs::create_dir(&native).unwrap();
    std::fs::write(
        native.join("rollout-verify.jsonl"),
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"verify\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"verify archive\"}}\n"
        ),
    )
    .unwrap();
    let path = dir.path().join("trace.db");
    let mut db = TraceDb::open(&path).unwrap();
    let report = db
        .ingest(IngestRequest {
            agents: vec![Agent::Codex],
            mode: IngestMode::Full,
            root: Some(native),
            since_ms: None,
        })
        .unwrap();
    assert_eq!(report.total_ingested(), 1);
    drop(db);
    (dir, path)
}

#[test]
fn verify_accepts_a_healthy_full_archive() {
    let (_dir, path) = full_archive();
    let report = verify_archive(path).unwrap();

    assert!(report.passed);
    assert_eq!(report.failure_count(), 0);
    assert_eq!(report.checks.len(), 6);
    assert!(report.checks.iter().all(|check| check.passed));
    assert!(report
        .checks
        .iter()
        .any(|check| check.name == "objects" && check.checked == 1));
}

#[test]
fn verify_reports_contract_and_object_corruption() {
    let (_dir, path) = full_archive();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE schema_meta SET value='future-v9' WHERE key='archive_contract'",
            [],
        )
        .unwrap();
    connection
        .execute("UPDATE objects SET payload=x'00'", [])
        .unwrap();
    drop(connection);

    let report = verify_archive(path).unwrap();

    assert!(!report.passed);
    let contract = report
        .checks
        .iter()
        .find(|check| check.name == "archive_contract")
        .unwrap();
    assert!(!contract.passed);
    assert!(contract.failures[0].message.contains("future-v9"));
    let objects = report
        .checks
        .iter()
        .find(|check| check.name == "objects")
        .unwrap();
    assert!(!objects.passed);
    assert!(objects.failures[0].message.contains("decompression failed"));
}

#[test]
fn verify_cli_prints_json_before_nonzero_exit() {
    let (_dir, path) = full_archive();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute("UPDATE objects SET bytes=bytes+1", [])
        .unwrap();
    drop(connection);

    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["--db", path.to_str().unwrap(), "verify", "--json"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], false);
    assert!(report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "objects" && check["passed"] == false));
    assert!(String::from_utf8_lossy(&output.stderr).contains("verification failed"));
}
