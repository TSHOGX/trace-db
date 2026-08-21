use std::process::Command;
use tempfile::tempdir;
use tracedb::benchmark::{
    run_benchmarks, BenchmarkConfig, BenchmarkOperationName, BenchmarkResult,
    BENCHMARK_SCHEMA_VERSION, SEARCH_REPETITIONS_PER_QUERY,
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
    assert_eq!(
        report.search_repetitions_per_query,
        SEARCH_REPETITIONS_PER_QUERY
    );
    let run = &report.runs[0];
    assert_eq!(run.sessions, 8);
    assert_eq!(run.changed_sessions, 1);
    assert_eq!(run.operations.len(), 11);
    assert_eq!(
        run.operations.iter().map(|o| o.name).collect::<Vec<_>>(),
        [
            BenchmarkOperationName::Generate,
            BenchmarkOperationName::FirstIngest,
            BenchmarkOperationName::UnchangedIngest,
            BenchmarkOperationName::ChangedIngest,
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
        run.operations[1].result,
        BenchmarkResult::Ingested {
            mode: tracedb::IngestMode::Full,
            ingested: 8,
            failed: 0,
            ..
        }
    ));
    assert!(matches!(
        run.operations[9].result,
        BenchmarkResult::Verified {
            passed: true,
            failures: 0,
            ..
        }
    ));
    assert!(run.operations[4].metrics.p95_wall_time_ns.is_some());
    match &run.operations[4].result {
        BenchmarkResult::Searched { queries, results } => {
            assert_eq!(*queries, 3);
            assert!(*results > 0);
        }
        result => panic!("unexpected search result: {result:?}"),
    }
    assert!(run
        .operations
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 4)
        .all(|(_, operation)| operation.metrics.p95_wall_time_ns.is_none()));
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
    assert_eq!(
        report["searchRepetitionsPerQuery"],
        SEARCH_REPETITIONS_PER_QUERY
    );
    assert_eq!(report["runs"][0]["sessions"], 4);
    assert_eq!(report["runs"][0]["operations"][0]["name"], "generate");
    assert_eq!(
        report["runs"][0]["operations"][3]["result"]["type"],
        "ingested"
    );
    assert!(report["runs"][0]["operations"][4]["metrics"]["p95WallTimeNs"].is_u64());
}
