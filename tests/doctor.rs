use serde_json::Value;
use std::process::Command;
use tempfile::tempdir;

fn doctor(home: &std::path::Path, database: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["--db", database.to_str().unwrap(), "doctor", "--json"])
        .env("HOME", home)
        .env("USERPROFILE", home)
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
        .env("USERPROFILE", dir.path())
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

#[test]
fn doctor_reports_ingest_telemetry_lag_permissions_and_backup_guidance() {
    let dir = tempdir().unwrap();
    let native = dir.path().join(".codex/sessions");
    std::fs::create_dir_all(&native).unwrap();
    std::fs::write(
        native.join("rollout-good.jsonl"),
        concat!(
            "{\"type\":\"session_meta\",\"timestamp\":\"2026-08-20T00:00:00Z\",\"payload\":{\"id\":\"good\"}}\n",
            "{\"type\":\"event_msg\",\"timestamp\":\"2026-08-20T00:00:01Z\",\"payload\":{\"type\":\"user_message\",\"message\":\"doctor telemetry\"}}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        native.join("rollout-bad.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":\n",
    )
    .unwrap();
    let database = dir.path().join("trace.db");
    let ingest = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            database.to_str().unwrap(),
            "ingest",
            "--agent",
            "codex",
            "--json",
        ])
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env_remove("TRACEDB_CONFIG")
        .env_remove("TRACEDB_PATH")
        .output()
        .unwrap();
    assert!(ingest.status.success());

    let output = doctor(dir.path(), &database);
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert_eq!(report["database"]["lastIngest"]["discovered"], 2);
    assert_eq!(report["database"]["lastIngest"]["successful"], false);
    assert_eq!(report["database"]["lastIngest"]["ingested"], 1);
    assert_eq!(report["database"]["lastIngest"]["failed"], 1);
    assert_eq!(report["database"]["lastIngest"]["cumulativeFailed"], 1);
    assert_eq!(report["database"]["backup"]["recommended"], true);
    assert_eq!(report["database"]["backup"]["totalSessions"], 1);
    assert!(report["database"]["archiveLagMs"].is_number());
    assert_eq!(report["permissions"]["archiveWritable"], true);
    assert_eq!(report["permissions"]["nativeReadable"], true);
    assert_eq!(report["watch"]["ready"], true);
    assert!(report["watch"]["watcherAvailable"].is_boolean());

    std::fs::remove_file(native.join("rollout-bad.jsonl")).unwrap();
    let ingest = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            database.to_str().unwrap(),
            "ingest",
            "--agent",
            "codex",
            "--json",
        ])
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env_remove("TRACEDB_CONFIG")
        .env_remove("TRACEDB_PATH")
        .output()
        .unwrap();
    assert!(ingest.status.success());
    let recovered = doctor(dir.path(), &database);
    assert!(recovered.status.success());
    let report: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(report["healthy"], true);
    assert_eq!(report["database"]["lastIngest"]["failed"], 0);
    assert_eq!(report["database"]["lastIngest"]["successful"], true);
    assert_eq!(report["database"]["lastIngest"]["cumulativeFailed"], 1);

    let pending = native.join("rollout-pending.jsonl");
    std::fs::write(
        &pending,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"pending\"}}\n",
    )
    .unwrap();
    let future = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 60;
    filetime::set_file_mtime(&pending, filetime::FileTime::from_unix_time(future, 0)).unwrap();
    let lagging = doctor(dir.path(), &database);
    assert!(lagging.status.success());
    let report: Value = serde_json::from_slice(&lagging.stdout).unwrap();
    assert!(report["database"]["archiveLagMs"].as_i64().unwrap() > 0);
}

#[test]
fn doctor_rejects_corrupt_ingest_telemetry() {
    let dir = tempdir().unwrap();
    let database = dir.path().join("trace.db");
    let connection = tracedb::open_database(&database).unwrap();
    connection
        .execute(
            "INSERT OR REPLACE INTO schema_meta(key,value) VALUES('ingest.last_status','not-json')",
            [],
        )
        .unwrap();
    drop(connection);

    let output = doctor(dir.path(), &database);
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert!(report["database"]["error"]
        .as_str()
        .unwrap()
        .contains("telemetry"));
}

#[cfg(unix)]
#[test]
fn doctor_reports_archive_permission_failures() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let database = dir.path().join("trace.db");
    drop(tracedb::open_database(&database).unwrap());
    let original = std::fs::metadata(&database).unwrap().permissions();
    let mut readonly = original.clone();
    readonly.set_mode(0o400);
    std::fs::set_permissions(&database, readonly).unwrap();

    let output = doctor(dir.path(), &database);
    std::fs::set_permissions(&database, original).unwrap();
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["permissions"]["healthy"], false);
    assert_eq!(report["permissions"]["archiveWritable"], false);
    assert!(report["permissions"]["issues"][0]
        .as_str()
        .unwrap()
        .contains("not writable"));
}
