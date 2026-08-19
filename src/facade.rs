use crate::{
    default_db_path,
    model::{Agent, IngestMode, ParsedSession, Session},
    parsers::parser,
    search, store, SearchRequest, SearchResult,
};
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// An open TraceDB archive.
///
/// `TraceDb` is the primary in-process API. It owns one SQLite connection and
/// keeps parser discovery, archive writes, retrieval, and reconstruction behind
/// typed requests and results.
pub struct TraceDb {
    path: PathBuf,
    connection: Connection,
}

impl TraceDb {
    /// Open or create an archive at `path` and apply all schema migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let connection = store::open(&path)?;
        Ok(Self { path, connection })
    }

    /// Open the archive selected by `TRACEDB_PATH` or the platform data path.
    pub fn open_default() -> Result<Self> {
        Self::open(default_db_path())
    }

    /// Return the path used to open this archive.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Discover and ingest sessions from the requested native agent stores.
    pub fn ingest(&mut self, request: IngestRequest) -> Result<IngestReport> {
        let agents = if request.agents.is_empty() {
            Agent::ALL.to_vec()
        } else {
            request.agents
        };
        let mut reports = Vec::with_capacity(agents.len());
        for agent in agents {
            let root = request.root.clone().unwrap_or_else(|| native_root(agent));
            let parser = parser(agent);
            let candidates = parser.discover(&root)?;
            let states = store::candidate_states(&self.connection, agent)?;
            let discovered = candidates.len();
            let mut parsed = 0;
            let mut ingested = 0;
            let mut unchanged = 0;
            let mut skipped_by_since = 0;
            for candidate in candidates {
                if request
                    .since_ms
                    .is_some_and(|cutoff| candidate.updated_at_ms.is_some_and(|time| time < cutoff))
                {
                    skipped_by_since += 1;
                    continue;
                }
                if states.get(&candidate.locator).is_some_and(|state| {
                    state.fingerprint == candidate.fingerprint
                        && (matches!(request.mode, IngestMode::Partial)
                            || matches!(state.mode, IngestMode::Full))
                }) {
                    unchanged += 1;
                    continue;
                }
                parsed += 1;
                if let Ok(Some(mut session)) = parser.parse(&candidate, &root) {
                    session.session.fingerprint = candidate.fingerprint;
                    store::upsert(&mut self.connection, session, request.mode)?;
                    ingested += 1;
                }
            }
            reports.push(AgentIngestReport {
                agent,
                root,
                discovered,
                parsed,
                ingested,
                unchanged,
                skipped_by_since,
            });
        }
        Ok(IngestReport { agents: reports })
    }

    /// Insert one already-parsed session through the same transactional path.
    pub fn ingest_session(&mut self, session: ParsedSession, mode: IngestMode) -> Result<()> {
        store::upsert(&mut self.connection, session, mode)
    }

    /// Search normalized events and return lineage-collapsed session results.
    pub fn search(&self, request: SearchRequest) -> Result<Vec<SearchResult>> {
        search::search(&self.connection, &request)
    }

    /// Load a stored session and its complete normalized event stream.
    pub fn show(&self, session_id: &str) -> Result<Option<SessionTrace>> {
        store::show(&self.connection, session_id)
    }

    /// Return per-agent and archive-wide counts.
    pub fn stats(&self) -> Result<ArchiveStats> {
        let agents = store::stats(&self.connection)?
            .into_iter()
            .map(|(agent, sessions, events, full)| {
                Ok(AgentStats {
                    agent: agent.parse().map_err(|message: String| anyhow!(message))?,
                    sessions,
                    events,
                    full_sessions: full,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ArchiveStats {
            path: self.path.clone(),
            total_sessions: agents.iter().map(|row| row.sessions).sum(),
            total_events: agents.iter().map(|row| row.events).sum(),
            total_full_sessions: agents.iter().map(|row| row.full_sessions).sum(),
            agents,
        })
    }

    /// Rebuild the gated FTS index from normalized events.
    pub fn reindex(&self) -> Result<()> {
        store::rebuild_fts(&self.connection)
    }

    /// Restore full-capture native sources below `out_dir`.
    pub fn reconstruct(&self, session_id: &str, out_dir: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        store::reconstruct(&self.connection, session_id, out_dir.as_ref())
    }
}

/// Options for discovering and ingesting native sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestRequest {
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub mode: IngestMode,
    pub root: Option<PathBuf>,
    pub since_ms: Option<i64>,
}

impl Default for IngestRequest {
    fn default() -> Self {
        Self {
            agents: Vec::new(),
            mode: IngestMode::Partial,
            root: None,
            since_ms: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIngestReport {
    pub agent: Agent,
    pub root: PathBuf,
    pub discovered: usize,
    pub parsed: usize,
    pub ingested: usize,
    pub unchanged: usize,
    pub skipped_by_since: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestReport {
    pub agents: Vec<AgentIngestReport>,
}

impl IngestReport {
    pub fn total_discovered(&self) -> usize {
        self.agents.iter().map(|row| row.discovered).sum()
    }

    pub fn total_ingested(&self) -> usize {
        self.agents.iter().map(|row| row.ingested).sum()
    }

    pub fn total_parsed(&self) -> usize {
        self.agents.iter().map(|row| row.parsed).sum()
    }

    pub fn total_unchanged(&self) -> usize {
        self.agents.iter().map(|row| row.unchanged).sum()
    }

    pub fn total_skipped_by_since(&self) -> usize {
        self.agents.iter().map(|row| row.skipped_by_since).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStats {
    pub agent: Agent,
    pub sessions: i64,
    pub events: i64,
    pub full_sessions: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStats {
    pub path: PathBuf,
    pub total_sessions: i64,
    pub total_events: i64,
    pub total_full_sessions: i64,
    pub agents: Vec<AgentStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTrace {
    pub session: Session,
    pub mode: IngestMode,
    pub events: Vec<crate::model::Event>,
}

/// Resolve the default native store root for one supported agent.
pub fn native_root(agent: Agent) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    match agent {
        Agent::Claude => home.join(".claude/projects"),
        Agent::Codex => home.join(".codex/sessions"),
        Agent::OpenCode => home.join(".local/share/opencode"),
        Agent::Gemini => home.join(".gemini/tmp"),
        Agent::Pi => home.join(".pi/agent/sessions"),
    }
}
