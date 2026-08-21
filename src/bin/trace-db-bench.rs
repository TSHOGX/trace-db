use anyhow::{bail, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tracedb::benchmark::{run_benchmarks, BenchmarkConfig, BenchmarkSuiteReport};

#[derive(Debug, Parser)]
#[command(
    name = "trace-db-bench",
    version,
    about = "Deterministic end-to-end TraceDB benchmark harness"
)]
struct Cli {
    /// Dataset sizes, accepting integers or k suffixes (default: 1k,10k,100k).
    #[arg(long, value_delimiter = ',', default_value = "1k,10k,100k")]
    sessions: Vec<String>,
    /// Persist benchmark datasets and archives under this new directory.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Emit the stable machine-readable report.
    #[arg(long)]
    json: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let session_counts = cli
        .sessions
        .iter()
        .map(|v| parse_session_count(v))
        .collect::<Result<Vec<_>>>()?;
    let temporary = cli
        .out
        .is_none()
        .then(|| tempfile::Builder::new().prefix("trace-db-bench-").tempdir())
        .transpose()?;
    let workspace = match (&cli.out, &temporary) {
        (Some(path), _) => path.clone(),
        (None, Some(directory)) => directory.path().join("workspace"),
        (None, None) => unreachable!("temporary workspace selection is exhaustive"),
    };
    let report = run_benchmarks(&BenchmarkConfig {
        workspace: workspace.clone(),
        session_counts,
    })?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(&report);
        if cli.out.is_some() {
            println!("workspace: {}", workspace.display());
        }
    }
    Ok(())
}

fn parse_session_count(value: &str) -> Result<usize> {
    let value = value.trim().to_ascii_lowercase();
    let (number, multiplier) = match value.strip_suffix('k') {
        Some(number) => (number, 1_000_usize),
        None => (value.as_str(), 1),
    };
    let count = number
        .parse::<usize>()
        .with_context(|| format!("invalid benchmark session count {value:?}"))?
        .checked_mul(multiplier)
        .context("benchmark session count overflow")?;
    if count == 0 {
        bail!("benchmark session counts must be greater than zero");
    }
    Ok(count)
}

fn print_text(report: &BenchmarkSuiteReport) {
    for run in &report.runs {
        println!(
            "{} sessions, {} native bytes, {} changed sessions",
            run.sessions, run.native_bytes, run.changed_sessions
        );
        for operation in &run.operations {
            let metrics = &operation.metrics;
            let cpu = metrics
                .cpu_time_ns
                .map(format_duration)
                .unwrap_or_else(|| "n/a".into());
            let rss = metrics
                .process_peak_rss_bytes
                .map(format_bytes)
                .unwrap_or_else(|| "n/a".into());
            let amplification = metrics
                .write_amplification
                .map(|value| format!("{value:.2}x"))
                .unwrap_or_else(|| "n/a".into());
            let p95 = metrics
                .p95_wall_time_ns
                .map(format_duration)
                .unwrap_or_else(|| "n/a".into());
            println!(
                "  {:<26} wall {:>10}  p95 {:>10}  cpu {:>10}  peak {:>10}  db {:>10}  write amp {:>8}",
                operation.name,
                format_duration(metrics.wall_time_ns),
                p95,
                cpu,
                rss,
                format_bytes(metrics.database_bytes),
                amplification
            );
        }
    }
}

fn format_duration(nanoseconds: u64) -> String {
    if nanoseconds >= 1_000_000_000 {
        format!("{:.3}s", nanoseconds as f64 / 1_000_000_000.0)
    } else if nanoseconds >= 1_000_000 {
        format!("{:.3}ms", nanoseconds as f64 / 1_000_000.0)
    } else {
        format!("{:.3}us", nanoseconds as f64 / 1_000.0)
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.2}MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.2}KiB", bytes as f64 / 1_024.0)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_standard_and_custom_sizes() {
        assert_eq!(parse_session_count("1k").unwrap(), 1_000);
        assert_eq!(parse_session_count("100K").unwrap(), 100_000);
        assert_eq!(parse_session_count("17").unwrap(), 17);
        assert!(parse_session_count("0").is_err());
    }
}
