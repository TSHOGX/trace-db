use clap::{Parser, Subcommand};
use std::io::{self, BufRead};
use std::net::SocketAddr;
use std::path::PathBuf;
use tracedb::{
    default_db_path, doctor_archive,
    service::{serve, ServiceEndpoint},
    verify_archive, Agent, IngestMode, IngestRequest, ListRequest, SearchRequest, TraceDb,
};

#[derive(Parser, Debug)]
#[command(
    name = "trace-db",
    version,
    about = "Loss-aware archive and retrieval for coding-agent traces"
)]
struct Cli {
    #[arg(long, env = "TRACEDB_PATH", global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Discover and ingest native sessions. The default is loss-minimizing partial mode.
    Ingest {
        #[arg(long, value_delimiter = ',')]
        agent: Vec<Agent>,
        #[arg(long, default_value = "partial")]
        mode: IngestMode,
        #[arg(long)]
        root: Option<PathBuf>,
        /// Only ingest sessions updated within N days or after an RFC3339 timestamp.
        #[arg(long)]
        since: Option<String>,
        /// Return a nonzero exit status when any candidate fails.
        #[arg(long)]
        strict: bool,
        /// Print the complete machine-readable ingest report.
        #[arg(long)]
        json: bool,
    },
    /// Search indexed normalized events.
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        agent: Option<Agent>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List archived sessions with stable cursor pagination.
    List {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long)]
        agent: Option<Agent>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        mode: Option<IngestMode>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List the normalized event stream and metadata for one session.
    Show {
        id: String,
        #[arg(long)]
        include_tools: bool,
        #[arg(long)]
        json: bool,
    },
    /// Rebuild FTS from the gated normalized event set.
    Reindex,
    /// Reconstruct native traces captured by a full ingest.
    Reconstruct {
        id: String,
        #[arg(long)]
        out: PathBuf,
        /// Replace existing restore targets atomically.
        #[arg(long)]
        overwrite: bool,
    },
    /// Print archive health and per-agent counts.
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Verify database, index, contract, references, and archived objects.
    Verify {
        /// Print the complete machine-readable verification report.
        #[arg(long)]
        json: bool,
    },
    /// Diagnose database, native stores, tokenizer, and runtime readiness.
    Doctor {
        /// Print the complete machine-readable diagnostic report.
        #[arg(long)]
        json: bool,
    },
    /// Line-oriented JSON API for language-neutral integrations.
    Api,
    /// Serve the versioned tracedb.v1 gRPC API.
    Serve {
        /// Listen on a TCP address. Defaults to loopback port 50051.
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Listen on a Unix domain socket instead of TCP.
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Permit an unauthenticated TCP listener outside loopback.
        #[arg(long)]
        allow_remote: bool,
        /// Enable reconstruction below this server-controlled root.
        #[arg(long)]
        reconstruct_root: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(default_db_path);
    if let Command::Verify { json } = &cli.command {
        let report = verify_archive(&db_path)?;
        if *json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("db: {}", report.path.display());
            for check in &report.checks {
                println!(
                    "{}\t{}\t{} checked\t{} failure(s)",
                    if check.passed { "ok" } else { "failed" },
                    check.name,
                    check.checked,
                    check.failures.len()
                );
                for failure in &check.failures {
                    eprintln!("{}: {}", failure.locator, failure.message);
                }
            }
        }
        if !report.passed {
            anyhow::bail!(
                "archive verification failed: {} failure(s)",
                report.failure_count()
            );
        }
        return Ok(());
    }
    if let Command::Doctor { json } = &cli.command {
        let report = doctor_archive(&db_path);
        if *json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("db: {}", report.database.path.display());
            println!(
                "database\t{}\t{}",
                if report.database.error.is_none()
                    && report
                        .database
                        .verification
                        .as_ref()
                        .is_none_or(|verification| verification.passed)
                {
                    "ok"
                } else {
                    "failed"
                },
                if report.database.exists {
                    "existing archive"
                } else {
                    "not created"
                }
            );
            for agent in &report.agents {
                println!(
                    "{}\t{}\t{} discovered\t{}",
                    agent.agent,
                    if agent.failures.is_empty() {
                        "ok"
                    } else {
                        "failed"
                    },
                    agent.discovered,
                    agent.root.display()
                );
            }
            println!(
                "tokenizer\t{}\t{}",
                if report.tokenizer.available {
                    "ok"
                } else {
                    "failed"
                },
                report.tokenizer.tokenizer
            );
        }
        if !report.healthy {
            anyhow::bail!("doctor found one or more unhealthy checks");
        }
        return Ok(());
    }
    let mut db = TraceDb::open(&db_path)?;
    match cli.command {
        Command::Ingest {
            agent,
            mode,
            root,
            since,
            strict,
            json,
        } => {
            let agents = if agent.is_empty() {
                Agent::ALL.to_vec()
            } else {
                agent
            };
            let report = db.ingest(IngestRequest {
                agents,
                mode,
                root,
                since_ms: since.as_deref().map(parse_since).transpose()?,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                for row in &report.agents {
                    println!(
                        "{}: discovered {}, parsed {}, ingested {}, unchanged {}, skipped {}, failed {}, warnings {}",
                        row.agent,
                        row.discovered,
                        row.parsed,
                        row.ingested,
                        row.unchanged,
                        row.skipped,
                        row.failed,
                        row.warnings.len()
                    );
                    for issue in row.warnings.iter().chain(&row.failures) {
                        eprintln!(
                            "{} {} [{}]: {}",
                            issue.stage, issue.locator, issue.category, issue.message
                        );
                    }
                }
                println!("total sessions: {}", report.total_ingested());
            }
            if strict && report.total_failed() > 0 {
                anyhow::bail!(
                    "strict ingest failed: {} candidate(s) failed",
                    report.total_failed()
                );
            }
        }
        Command::Search {
            query,
            limit,
            agent,
            cwd,
            since,
            json,
        } => {
            let rows = db.search(SearchRequest {
                query,
                limit,
                agent,
                cwd,
                since_ms: since.as_deref().map(parse_since).transpose()?,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for row in rows {
                    println!(
                        "{}\t{}\t{:.4}\t{} hits\t{}",
                        row.id,
                        row.agent,
                        row.score,
                        row.hits,
                        row.cwd.unwrap_or_else(|| "-".into())
                    );
                    if let Some(title) = row.title {
                        println!("  title: {title}");
                    }
                    println!(
                        "  match[{} {}]: {}",
                        row.best_match.event_idx, row.best_match.kind, row.best_match.snippet
                    );
                    if let Some(ask) = row.ask {
                        println!("  ask: {ask}");
                    }
                    if let Some(outcome) = row.outcome {
                        println!("  outcome: {outcome}");
                    }
                }
            }
        }
        Command::List {
            limit,
            cursor,
            agent,
            cwd,
            since,
            mode,
            model,
            provider,
            json,
        } => {
            let page = db.list(ListRequest {
                limit,
                cursor,
                agent,
                cwd,
                since_ms: since.as_deref().map(parse_since).transpose()?,
                mode,
                model,
                provider,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                for session in &page.sessions {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        session.id,
                        session.agent,
                        session.mode,
                        session.events,
                        session.cwd.as_deref().unwrap_or("-")
                    );
                    if let Some(title) = &session.title {
                        println!("  title: {title}");
                    }
                }
                if let Some(cursor) = page.next_cursor {
                    println!("next cursor: {cursor}");
                }
            }
        }
        Command::Show {
            id,
            include_tools,
            json,
        } => {
            let trace = db
                .show(&id)?
                .ok_or_else(|| anyhow::anyhow!("session not found: {id}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&trace)?)
            } else {
                for event in trace.events {
                    if include_tools
                        || matches!(
                            event.kind,
                            tracedb::EventKind::User | tracedb::EventKind::Assistant
                        )
                    {
                        println!("[{}] {}: {}", event.idx, event.kind, event.text);
                    }
                }
            }
        }
        Command::Reindex => {
            db.reindex()?;
            println!("events_fts rebuilt");
        }
        Command::Reconstruct { id, out, overwrite } => {
            let paths = db.reconstruct_with_options(
                &id,
                &out,
                tracedb::ReconstructionOptions { overwrite },
            )?;
            for p in &paths {
                println!("{}", p.display());
            }
            if paths.is_empty() {
                println!("no full native objects for {id}");
            }
        }
        Command::Stats { json } => {
            let stats = db.stats()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("db: {}", stats.path.display());
                for row in stats.agents {
                    println!(
                        "{}\t{} sessions\t{} events\t{} full",
                        row.agent, row.sessions, row.events, row.full_sessions
                    );
                }
            }
        }
        Command::Verify { .. } => unreachable!("verify returns before opening the archive"),
        Command::Doctor { .. } => unreachable!("doctor returns before opening the archive"),
        Command::Api => run_api(&db)?,
        Command::Serve {
            listen,
            socket,
            allow_remote,
            reconstruct_root,
        } => {
            let endpoint = match (listen, socket) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("--listen and --socket are mutually exclusive")
                }
                (Some(address), None) => ServiceEndpoint::Tcp(address),
                (None, Some(path)) => ServiceEndpoint::Unix(path),
                (None, None) => ServiceEndpoint::Tcp("127.0.0.1:50051".parse()?),
            };
            if matches!(&endpoint, ServiceEndpoint::Tcp(address) if !address.ip().is_loopback())
                && !allow_remote
            {
                anyhow::bail!(
                    "refusing a non-loopback listener without --allow-remote; the service has no authentication or TLS"
                )
            }
            eprintln!("serving tracedb.v1 on {}", display_endpoint(&endpoint));
            serve(db, endpoint, reconstruct_root)?;
        }
    }
    Ok(())
}

fn display_endpoint(endpoint: &ServiceEndpoint) -> String {
    match endpoint {
        ServiceEndpoint::Tcp(address) => format!("http://{address}"),
        ServiceEndpoint::Unix(path) => path.display().to_string(),
    }
}

fn parse_since(value: &str) -> anyhow::Result<i64> {
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("--since must be a non-empty day count or RFC3339 timestamp");
    }
    if let Ok(days) = value.parse::<i64>() {
        if days < 0 {
            anyhow::bail!("--since day count must not be negative: {days}");
        }
        let offset = days
            .checked_mul(86_400_000)
            .ok_or_else(|| anyhow::anyhow!("--since day count is too large: {days}"))?;
        return chrono::Utc::now()
            .timestamp_millis()
            .checked_sub(offset)
            .ok_or_else(|| anyhow::anyhow!("--since day count is out of range: {days}"));
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_millis())
        .map_err(|error| {
            anyhow::anyhow!(
                "invalid --since value {value:?}; expected a non-negative day count or RFC3339 timestamp ({error})"
            )
        })
}

fn run_api(db: &TraceDb) -> anyhow::Result<()> {
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: serde_json::Value = serde_json::from_str(&line)?;
        let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let result = match op {
            "stats" => serde_json::to_value(db.stats()?)?,
            "search" => {
                let q = req.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let n = req.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let a = req
                    .get("agent")
                    .and_then(|v| v.as_str())
                    .and_then(|x| x.parse().ok());
                let cwd = req.get("cwd").and_then(|v| v.as_str());
                let since = req
                    .get("since")
                    .and_then(|v| v.as_str())
                    .map(parse_since)
                    .transpose()?;
                serde_json::to_value(db.search(SearchRequest {
                    query: q.to_owned(),
                    limit: n,
                    agent: a,
                    cwd: cwd.map(str::to_owned),
                    since_ms: since,
                })?)?
            }
            "list" => {
                let agent = req
                    .get("agent")
                    .and_then(|value| value.as_str())
                    .map(str::parse::<Agent>)
                    .transpose()
                    .map_err(anyhow::Error::msg)?;
                let mode = req
                    .get("mode")
                    .and_then(|value| value.as_str())
                    .map(str::parse::<IngestMode>)
                    .transpose()
                    .map_err(anyhow::Error::msg)?;
                let since_ms = req
                    .get("since")
                    .and_then(|value| value.as_str())
                    .map(parse_since)
                    .transpose()?;
                serde_json::to_value(
                    db.list(ListRequest {
                        limit: req
                            .get("limit")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(50) as usize,
                        cursor: req
                            .get("cursor")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned),
                        agent,
                        cwd: req
                            .get("cwd")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned),
                        since_ms,
                        mode,
                        model: req
                            .get("model")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned),
                        provider: req
                            .get("provider")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned),
                    })?,
                )?
            }
            "show" => {
                let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
                serde_json::to_value(db.show(id)?)?
            }
            "reconstruct" => {
                let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let out = req.get("out").and_then(|v| v.as_str()).unwrap_or(".");
                let overwrite = req
                    .get("overwrite")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                serde_json::to_value(
                    db.reconstruct_with_options(
                        id,
                        PathBuf::from(out),
                        tracedb::ReconstructionOptions { overwrite },
                    )?
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>(),
                )?
            }
            _ => {
                serde_json::json!({"error":"unknown op","supported":["stats","search","list","show","reconstruct"]})
            }
        };
        println!(
            "{}",
            serde_json::to_string(
                &serde_json::json!({"ok":result.get("error").is_none(),"result":result})
            )?
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_since;

    #[test]
    fn parses_relative_day_counts() {
        let before = chrono::Utc::now().timestamp_millis();
        let parsed = parse_since("7").unwrap();
        let after = chrono::Utc::now().timestamp_millis();
        let seven_days = 7 * 86_400_000;
        assert!(parsed >= before - seven_days - 1);
        assert!(parsed <= after - seven_days + 1);
    }

    #[test]
    fn parses_rfc3339_timestamps() {
        assert_eq!(
            parse_since("2025-01-02T03:04:05Z").unwrap(),
            1_735_787_045_000
        );
    }

    #[test]
    fn rejects_invalid_since_values() {
        assert!(parse_since("-1").is_err());
        assert!(parse_since("").is_err());
        assert!(parse_since("yesterday").is_err());
    }
}
