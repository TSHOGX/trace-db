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
