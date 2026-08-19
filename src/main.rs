use clap::{Parser, Subcommand};
use std::io::{self, BufRead};
use std::path::PathBuf;
use tracedb::{default_db_path, Agent, IngestMode, IngestRequest, SearchRequest, TraceDb};

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
                since_ms: since.as_deref().and_then(parse_since),
            })?;
            for row in &report.agents {
                println!(
                    "{}: ingested {} of {} discovered",
                    row.agent, row.ingested, row.discovered
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
                since_ms: since.as_deref().and_then(parse_since),
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for row in rows {
                    println!(
                        "{}\t{}\t{}\t{}",
                        row.id,
                        row.agent,
                        row.hits,
                        row.cwd.unwrap_or_else(|| "-".into())
                    );
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
    }
    Ok(())
}

fn parse_since(value: &str) -> Option<i64> {
    if let Ok(days) = value.parse::<i64>() {
        return Some(chrono::Utc::now().timestamp_millis() - days * 86_400_000);
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|x| x.timestamp_millis())
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
                    .and_then(parse_since);
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
