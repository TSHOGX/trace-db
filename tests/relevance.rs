use std::process::Command;
use tracedb::relevance::{evaluate_relevance, RelevanceTag, RELEVANCE_SCHEMA_VERSION};

#[test]
fn relevance_report_covers_labeled_ranking_and_context_contracts() {
    let report = evaluate_relevance().unwrap();
    assert_eq!(report.schema_version, RELEVANCE_SCHEMA_VERSION);
    assert_eq!(report.query_count, 9);
    assert_eq!(report.corpus_sessions, 12);
    assert!(report.metrics.recall_at_5 > 0.7);
    assert!(report.metrics.recall_at_10 > 0.8);
    assert!(report.metrics.mrr > 0.6);
    assert!(report.metrics.ndcg_at_10 > 0.7);
    assert_eq!(report.metrics.lineage_collapse_accuracy, 1.0);
    assert_eq!(report.metrics.context_answerability, 1.0);
    for tag in [
        RelevanceTag::Multilingual,
        RelevanceTag::MultiTerm,
        RelevanceTag::Title,
        RelevanceTag::Cwd,
        RelevanceTag::Tool,
        RelevanceTag::Error,
        RelevanceTag::Model,
        RelevanceTag::Provider,
        RelevanceTag::OldImportant,
        RelevanceTag::ParentSubagent,
        RelevanceTag::Fork,
        RelevanceTag::DistantContext,
    ] {
        assert!(
            report.metrics_by_tag.contains_key(&tag),
            "missing tag {tag}"
        );
    }
}

#[test]
fn relevance_binary_emits_versioned_json() {
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db-relevance"))
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schemaVersion"], RELEVANCE_SCHEMA_VERSION);
    assert_eq!(report["queryCount"], 9);
    assert!(report["metrics"]["ndcgAt10"].is_number());
}
