use anyhow::Result;
use clap::Parser;
use tracedb::relevance::{evaluate_relevance, RelevanceReport};

#[derive(Debug, Parser)]
#[command(
    name = "trace-db-relevance",
    version,
    about = "Deterministic labeled relevance evaluation for TraceDB search"
)]
struct Cli {
    /// Emit the stable machine-readable report.
    #[arg(long)]
    json: bool,
}

fn main() -> Result<()> {
    let report = evaluate_relevance()?;
    let cli = Cli::parse();
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(&report);
    }
    Ok(())
}

fn print_text(report: &RelevanceReport) {
    println!(
        "{} labeled queries over {} sessions",
        report.query_count, report.corpus_sessions
    );
    println!("  Recall@5: {:.3}", report.metrics.recall_at_5);
    println!("  Recall@10: {:.3}", report.metrics.recall_at_10);
    println!("  MRR: {:.3}", report.metrics.mrr);
    println!("  nDCG@10: {:.3}", report.metrics.ndcg_at_10);
    println!(
        "  lineage collapse: {:.3}",
        report.metrics.lineage_collapse_accuracy
    );
    println!(
        "  context answerability: {:.3}",
        report.metrics.context_answerability
    );
}
