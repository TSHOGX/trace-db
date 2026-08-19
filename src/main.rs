use clap::{Parser, Subcommand};
use std::io::{self, BufRead};
use std::path::PathBuf;
use tracedb::{
    default_db_path,
    model::{Agent, IngestMode},
    open_database,
    parsers::parser,
    store,
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
    let mut conn = open_database(&db_path)?;
    match cli.command {
        Command::Ingest { agent, mode, root } => {
            let agents = if agent.is_empty() {
                Agent::ALL.to_vec()
            } else {
                agent
            };
            let mut total = 0;
            for a in agents {
                let root = root.clone().unwrap_or_else(|| native_root(a));
                let sessions = parser(a).discover(&root)?;
                let n = sessions.len();
                for session in sessions {
                    store::upsert(&mut conn, session, mode)?;
                }
                println!("{a}: ingested {n}");
                total += n;
            }
            println!("total sessions: {total}");
        }
        Command::Search {
            query,
            limit,
            agent,
            cwd,
            since,
            json,
        } => {
            let rows = store::search_filtered(
                &conn,
                &query,
                limit,
                agent,
                cwd.as_deref(),
                since.as_deref().and_then(parse_since),
            )?;
            if json {
                println!("{}",serde_json::to_string_pretty(&rows.iter().map(|(id,agent,cwd,hits)|serde_json::json!({"id":id,"agent":agent,"cwd":cwd,"hits":hits})).collect::<Vec<_>>())?);
            } else {
                for (id, agent, cwd, hits) in rows {
                    println!(
                        "{id}\t{agent}\t{hits}\t{}",
                        cwd.unwrap_or_else(|| "-".into())
                    );
                }
            }
        }
        Command::Show {
            id,
            include_tools,
            json,
        } => {
            let mut stmt=conn.prepare("SELECT idx,kind,subtype,name,call_id,is_error,text,created_at_ms FROM events WHERE session_id=?1 ORDER BY idx")?;
            let rows=stmt.query_map([id.clone()],|r|Ok(serde_json::json!({"idx":r.get::<_,i64>(0)?,"kind":r.get::<_,String>(1)?,"subtype":r.get::<_,Option<String>>(2)?,"name":r.get::<_,Option<String>>(3)?,"callId":r.get::<_,Option<String>>(4)?,"isError":r.get::<_,Option<i64>>(5)?.map(|v|v!=0),"text":r.get::<_,String>(6)?,"createdAtMs":r.get::<_,Option<i64>>(7)?})))?;
            let mut all = Vec::new();
            for row in rows {
                let e = row?;
                if include_tools
                    || matches!(
                        e.get("kind").and_then(|v| v.as_str()),
                        Some("user") | Some("assistant")
                    )
                {
                    all.push(e)
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?)
            } else {
                for e in all {
                    println!("[{}] {}: {}", e["idx"], e["kind"], e["text"]);
                }
            }
        }
        Command::Reindex => {
            store::rebuild_fts(&conn)?;
            println!("events_fts rebuilt");
        }
        Command::Reconstruct { id, out } => {
            let paths = store::reconstruct(&conn, &id, &out)?;
            for p in &paths {
                println!("{}", p.display());
            }
            if paths.is_empty() {
                println!("no full native objects for {id}");
            }
        }
        Command::Stats { json } => {
            let rows = store::stats(&conn)?;
            if json {
                println!("{}",serde_json::to_string_pretty(&rows.iter().map(|(agent,sessions,events,full)|serde_json::json!({"agent":agent,"sessions":sessions,"events":events,"full":full})).collect::<Vec<_>>())?);
            } else {
                println!("db: {}", db_path.display());
                for (agent, sessions, events, full) in rows {
                    println!("{agent}\t{sessions} sessions\t{events} events\t{full} full");
                }
            }
        }
        Command::Api => run_api(&conn)?,
    }
    Ok(())
}

fn native_root(agent: Agent) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    match agent {
        Agent::Claude => home.join(".claude/projects"),
        Agent::Codex => home.join(".codex/sessions"),
        Agent::OpenCode => home.join(".local/share/opencode"),
        Agent::Gemini => home.join(".gemini/tmp"),
        Agent::Pi => home.join(".pi/agent/sessions"),
    }
}

fn parse_since(value: &str) -> Option<i64> {
    if let Ok(days) = value.parse::<i64>() {
        return Some(chrono::Utc::now().timestamp_millis() - days * 86_400_000);
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|x| x.timestamp_millis())
}

fn run_api(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: serde_json::Value = serde_json::from_str(&line)?;
        let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let result = match op {
            "stats" => serde_json::to_value(store::stats(conn)?)?,
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
                serde_json::to_value(store::search_filtered(conn, q, n, a, cwd, since)?)?
            }
            _ => serde_json::json!({"error":"unknown op","supported":["stats","search"]}),
        };
        println!(
            "{}",
            serde_json::to_string(
                &serde_json::json!({"ok":!result.get("error").is_some(),"result":result})
            )?
        );
    }
    Ok(())
}
