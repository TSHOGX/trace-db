use clap::{Parser, Subcommand};
use std::io::{self, BufRead};
use std::net::SocketAddr;
use std::path::PathBuf;
use tracedb::{
    default_db_path,
    service::{serve, ServiceEndpoint},
    Agent, IngestMode, IngestRequest, SearchRequest, TraceDb,
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
    },
    /// Print archive health and per-agent counts.
    Stats {
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
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(default_db_path);
    let mut db = TraceDb::open(&db_path)?;
    match cli.command {
        Command::Ingest {
            agent,
            mode,
            root,
            since,
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
            for row in &report.agents {
                println!(
                    "{}: discovered {}, parsed {}, ingested {}, unchanged {}, skipped by since {}",
                    row.agent,
                    row.discovered,
                    row.parsed,
                    row.ingested,
                    row.unchanged,
                    row.skipped_by_since
                );
            }
            println!("total sessions: {}", report.total_ingested());
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
        Command::Show {
            id,
            include_tools,
            json,
        } => {
            let trace = db
                .show(&id)?
                .ok_or_else(|| anyhow::anyhow!("session not found: {id}"))?;
            let mut events = Vec::new();
            for event in trace.events {
                if include_tools
                    || matches!(
                        event.kind,
                        tracedb::EventKind::User | tracedb::EventKind::Assistant
                    )
                {
                    events.push(event)
                }
            }
            if json {
                let values = events.iter().map(event_json).collect::<Vec<_>>();
                println!("{}", serde_json::to_string_pretty(&values)?)
            } else {
                for event in events {
                    println!("[{}] {}: {}", event.idx, event.kind, event.text);
                }
            }
        }
        Command::Reindex => {
            db.reindex()?;
            println!("events_fts rebuilt");
        }
        Command::Reconstruct { id, out } => {
            let paths = db.reconstruct(&id, &out)?;
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
                let values = stats
                    .agents
                    .iter()
                    .map(|row| {
                        serde_json::json!({
                            "agent": row.agent,
                            "sessions": row.sessions,
                            "events": row.events,
                            "full": row.full_sessions,
                        })
                    })
                    .collect::<Vec<_>>();
                println!("{}", serde_json::to_string_pretty(&values)?);
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
        Command::Api => run_api(&db)?,
        Command::Serve {
            listen,
            socket,
            allow_remote,
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
            serve(db, endpoint)?;
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
            "show" => {
                let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
                serde_json::to_value(
                    db.show(id)?
                        .map(|trace| trace.events.iter().map(event_json).collect::<Vec<_>>())
                        .unwrap_or_default(),
                )?
            }
            "reconstruct" => {
                let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let out = req.get("out").and_then(|v| v.as_str()).unwrap_or(".");
                serde_json::to_value(
                    db.reconstruct(id, PathBuf::from(out))?
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>(),
                )?
            }
            _ => {
                serde_json::json!({"error":"unknown op","supported":["stats","search","show","reconstruct"]})
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

fn event_json(event: &tracedb::Event) -> serde_json::Value {
    serde_json::json!({
        "idx": event.idx,
        "kind": event.kind,
        "subtype": event.subtype,
        "name": event.name,
        "callId": event.call_id,
        "isError": event.is_error,
        "text": event.text,
        "createdAtMs": event.created_at_ms,
    })
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
