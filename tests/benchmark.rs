use std::process::Command;
use tempfile::tempdir;
use tracedb::benchmark::{
    run_benchmarks, BenchmarkConfig, BenchmarkOperationName, BenchmarkResult,
    BENCHMARK_SCHEMA_VERSION,
};

#[test]
fn harness_covers_the_end_to_end_archive_lifecycle() {
    let parent = tempdir().unwrap();
    let workspace = parent.path().join("benchmark");
    let report = run_benchmarks(&BenchmarkConfig {
        workspace: workspace.clone(),
        session_counts: vec![8],
    })
    .unwrap();
    assert_eq!(report.schema_version, BENCHMARK_SCHEMA_VERSION);
    let run = &report.runs[0];
    assert_eq!(run.sessions, 8);
    assert_eq!(run.changed_sessions, 1);
    assert_eq!(run.operations.len(), 12);
    assert_eq!(
        run.operations.iter().map(|o| o.name).collect::<Vec<_>>(),
        [
            BenchmarkOperationName::Generate,
            BenchmarkOperationName::FirstPartialIngest,
            BenchmarkOperationName::UnchangedPartialIngest,
            BenchmarkOperationName::ChangedPartialIngest,
            BenchmarkOperationName::FullIngest,
            BenchmarkOperationName::Search,
            BenchmarkOperationName::List,
            BenchmarkOperationName::Show,
            BenchmarkOperationName::Stats,
            BenchmarkOperationName::Reindex,
            BenchmarkOperationName::Verify,
            BenchmarkOperationName::Reconstruct,
        ]
    );
    assert!(matches!(
        run.operations[4].result,
        BenchmarkResult::Ingested {
            mode: tracedb::IngestMode::Full,
            ingested: 8,
            failed: 0,
            ..
        }
    ));
    assert!(matches!(
        run.operations[10].result,
        BenchmarkResult::Verified {
            passed: true,
            failures: 0,
            ..
        }
    ));
    assert!(workspace
        .join("sessions-8/reconstructed/rollout-bench-000004.jsonl")
        .is_file());
}

#[test]
fn benchmark_binary_emits_the_versioned_json_contract() {
    let parent = tempdir().unwrap();
    let workspace = parent.path().join("cli-benchmark");
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db-bench"))
        .args([
            "--sessions",
            "4",
            "--out",
            workspace.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schemaVersion"], BENCHMARK_SCHEMA_VERSION);
    assert_eq!(report["runs"][0]["sessions"], 4);
    assert_eq!(report["runs"][0]["operations"][0]["name"], "generate");
    assert_eq!(
        report["runs"][0]["operations"][4]["result"]["type"],
        "ingested"
    );
}
