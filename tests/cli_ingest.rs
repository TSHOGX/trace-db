use serde_json::Value;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn strict_ingest_prints_report_then_exits_nonzero() {
    let dir = tempdir().unwrap();
    let native = dir.path().join("native");
    std::fs::create_dir(&native).unwrap();
    std::fs::write(
        native.join("rollout-good.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"good\"}}\n",
    )
    .unwrap();
    std::fs::write(
        native.join("rollout-bad.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            dir.path().join("trace.db").to_str().unwrap(),
            "ingest",
            "--agent",
            "codex",
            "--root",
            native.to_str().unwrap(),
            "--strict",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["agents"][0]["discovered"], 2);
    assert_eq!(report["agents"][0]["ingested"], 1);
    assert_eq!(report["agents"][0]["failed"], 1);
    assert_eq!(
        report["agents"][0]["failures"][0]["category"],
        "corrupt_data"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("strict ingest failed"));
}

#[test]
fn dry_run_reports_all_outcomes_without_mutating_the_archive() {
    let dir = tempdir().unwrap();
    let native = dir.path().join("native");
    let archive = dir.path().join("archive");
    let database = archive.join("trace.db");
    std::fs::create_dir(&native).unwrap();
    let unchanged = native.join("rollout-unchanged.jsonl");
    std::fs::write(
        &unchanged,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"unchanged\"}}\n",
    )
    .unwrap();
    let ingest = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            database.to_str().unwrap(),
            "ingest",
            "--agent",
            "codex",
            "--root",
            native.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(ingest.status.success());

    let changed = native.join("rollout-changed.jsonl");
    let changed_contents = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"changed\"}}\n";
    std::fs::write(&changed, changed_contents).unwrap();
    let skipped = native.join("rollout-skipped.jsonl");
    std::fs::write(
        &skipped,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"skipped\"}}\n",
    )
    .unwrap();
    filetime::set_file_mtime(&skipped, filetime::FileTime::from_unix_time(1, 0)).unwrap();
    let failed = native.join("rollout-failed.jsonl");
    std::fs::write(&failed, "{\"type\":\"session_meta\",\"payload\":\n").unwrap();
    let before = std::fs::read(&database).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            database.to_str().unwrap(),
            "ingest",
            "--agent",
            "codex",
            "--root",
            native.to_str().unwrap(),
            "--since",
            "1",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(std::fs::read(&database).unwrap(), before);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dryRun"], true);
    assert_eq!(report["mode"], "full");
    assert_eq!(report["agents"][0]["discovered"], 4);
    assert_eq!(report["agents"][0]["changed"], 1);
    assert_eq!(report["agents"][0]["unchanged"], 1);
    assert_eq!(report["agents"][0]["skipped"], 1);
    assert_eq!(report["agents"][0]["skippedBySince"], 1);
    assert_eq!(report["agents"][0]["failed"], 1);
    assert_eq!(
        report["agents"][0]["estimatedFullCaptureBytes"],
        changed_contents.len()
    );
    assert_eq!(report["agents"][0]["failures"][0]["stage"], "parsing");
}

#[test]
fn dry_run_reads_committed_wal_state() {
    let dir = tempdir().unwrap();
    let native = dir.path().join("native");
    let database = dir.path().join("trace.db");
    std::fs::create_dir(&native).unwrap();
    std::fs::write(
        native.join("rollout-wal.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"wal\"}}\n",
    )
    .unwrap();
    let ingest = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            database.to_str().unwrap(),
            "ingest",
            "--agent",
            "codex",
            "--root",
            native.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(ingest.status.success());

    let connection = tracedb::open_database(&database).unwrap();
    connection
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET fingerprint='committed-in-wal' WHERE id='codex:wal'",
            [],
        )
        .unwrap();
    assert!(
        std::fs::metadata(database.with_extension("db-wal"))
            .unwrap()
            .len()
            > 0
    );
    let before = std::fs::read(&database).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            database.to_str().unwrap(),
            "ingest",
            "--agent",
            "codex",
            "--root",
            native.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(std::fs::read(&database).unwrap(), before);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["agents"][0]["changed"], 1);
    assert_eq!(report["agents"][0]["unchanged"], 0);
}

#[test]
fn dry_run_does_not_create_a_missing_archive() {
    let dir = tempdir().unwrap();
    let native = dir.path().join("native");
    let database = dir.path().join("missing/trace.db");
    std::fs::create_dir(&native).unwrap();
    std::fs::write(
        native.join("rollout-new.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"new\"}}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            database.to_str().unwrap(),
            "ingest",
            "--agent",
            "codex",
            "--root",
            native.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!database.exists());
    assert!(!database.parent().unwrap().exists());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["agents"][0]["changed"], 1);
}
