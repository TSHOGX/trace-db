use serde_json::Value;
use std::process::Command;
use tempfile::tempdir;

fn doctor(home: &std::path::Path, database: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["--db", database.to_str().unwrap(), "doctor", "--json"])
        .env("HOME", home)
        .env_remove("TRACEDB_JIEBA_EXT")
        .output()
        .unwrap()
}

#[test]
fn doctor_accepts_a_ready_fresh_install() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("data/trace.db");

    let output = doctor(dir.path(), &database);

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["healthy"], true);
    assert_eq!(report["database"]["exists"], false);
    assert_eq!(report["database"]["writable"], true);
    assert_eq!(report["tokenizer"]["available"], true);
    assert_eq!(report["agents"].as_array().unwrap().len(), 5);
    assert_eq!(
        report["runtime"]["tracedbVersion"],
        env!("CARGO_PKG_VERSION")
    );
    assert!(report["runtime"]["sqliteVersion"].as_str().is_some());
}

#[test]
fn doctor_reports_a_corrupt_existing_archive() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("trace.db");
    std::fs::write(&database, "not sqlite").unwrap();

    let output = doctor(dir.path(), &database);

    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert_eq!(report["database"]["exists"], true);
    assert!(report["database"]["error"].as_str().is_some());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unhealthy"));
}

#[test]
fn doctor_reports_an_invalid_tokenizer_extension() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("trace.db");
    let extension = dir.path().join("missing-tokenizer.so");

    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["--db", database.to_str().unwrap(), "doctor", "--json"])
        .env("HOME", dir.path())
        .env("TRACEDB_JIEBA_EXT", &extension)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert_eq!(report["tokenizer"]["available"], false);
    assert_eq!(
        report["tokenizer"]["extension"],
        extension.display().to_string()
    );
    assert!(report["tokenizer"]["error"].as_str().is_some());
}
