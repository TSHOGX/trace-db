use crate::{
    default_db_path,
    model::{Agent, IngestMode, ParsedSession, Session},
    parsers::parser,
    search, store, SearchRequest, SearchResult,
};
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    path::{Path, PathBuf},
};

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
            let mut failures = Vec::new();
            let discovery = match parser.discover(&root) {
                Ok(discovery) => discovery,
                Err(error) => {
                    failures.push(IngestIssue::from_error(
                        IngestStage::Discovery,
                        root.display().to_string(),
                        &error,
                    ));
                    reports.push(AgentIngestReport {
                        agent,
                        root,
                        discovered: 0,
                        parsed: 0,
                        ingested: 0,
                        unchanged: 0,
                        skipped: 0,
                        skipped_by_since: 0,
                        failed: failures.len(),
                        warnings: Vec::new(),
                        failures,
                    });
                    continue;
                }
            };
            for failure in discovery.failures {
                failures.push(IngestIssue::from_error(
                    IngestStage::Discovery,
                    failure.locator,
                    &failure.error,
                ));
            }
            let discovered = discovery.candidates.len() + failures.len();
            let states = match store::candidate_states(&self.connection, agent) {
                Ok(states) => states,
                Err(error) => {
                    failures.push(IngestIssue::from_error(
                        IngestStage::Database,
                        self.path.display().to_string(),
                        &error,
                    ));
                    reports.push(AgentIngestReport {
                        agent,
                        root,
                        discovered,
                        parsed: 0,
                        ingested: 0,
                        unchanged: 0,
                        skipped: 0,
                        skipped_by_since: 0,
                        failed: failures.len(),
                        warnings: Vec::new(),
                        failures,
                    });
                    continue;
                }
            };
            let mut parsed = 0;
            let mut ingested = 0;
            let mut unchanged = 0;
            let mut skipped = 0;
            let mut skipped_by_since = 0;
            let mut pending = Vec::new();
            for candidate in discovery.candidates {
                if request
                    .since_ms
                    .is_some_and(|cutoff| candidate.updated_at_ms.is_some_and(|time| time < cutoff))
                {
                    skipped_by_since += 1;
                    skipped += 1;
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
                pending.push(candidate);
            }
            for (candidate, parsed_session) in parser.parse_many(&pending, &root) {
                match parsed_session {
                    Ok(Some(mut session)) => {
                        parsed += 1;
                        session.session.fingerprint = candidate.fingerprint;
                        match store::upsert(&mut self.connection, session, request.mode) {
                            Ok(()) => ingested += 1,
                            Err(error) => failures.push(IngestIssue::from_error(
                                IngestStage::Database,
                                candidate.locator,
                                &error,
                            )),
                        }
                    }
                    Ok(None) => {
                        parsed += 1;
                        skipped += 1;
                    }
                    Err(error) => failures.push(IngestIssue::from_error(
                        IngestStage::Parsing,
                        candidate.locator,
                        &error,
                    )),
                }
            }
            reports.push(AgentIngestReport {
                agent,
                root,
                discovered,
                parsed,
                ingested,
                unchanged,
                skipped,
                skipped_by_since,
                failed: failures.len(),
                warnings: Vec::new(),
                failures,
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

    /// Verify SQLite, index, contract, reference, and archived-object integrity.
    pub fn verify(&self) -> Result<VerifyReport> {
        store::verify(&self.connection, &self.path)
    }

    /// Restore full-capture native sources below `out_dir`.
    pub fn reconstruct(&self, session_id: &str, out_dir: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        self.reconstruct_with_options(session_id, out_dir, ReconstructionOptions::default())
    }

    /// Restore full-capture sources with explicit conflict handling.
    pub fn reconstruct_with_options(
        &self,
        session_id: &str,
        out_dir: impl AsRef<Path>,
        options: ReconstructionOptions,
    ) -> Result<Vec<PathBuf>> {
        store::reconstruct(&self.connection, session_id, out_dir.as_ref(), options)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconstructionOptions {
    #[serde(default)]
    pub overwrite: bool,
}

/// Open an existing archive without migration and verify its stored contract.
pub fn verify_archive(path: impl AsRef<Path>) -> Result<VerifyReport> {
    let path = path.as_ref();
    let connection = store::open_for_verification(path)?;
    store::verify(&connection, path)
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
    pub skipped: usize,
    pub skipped_by_since: usize,
    pub failed: usize,
    pub warnings: Vec<IngestIssue>,
    pub failures: Vec<IngestIssue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestStage {
    Discovery,
    Parsing,
    Database,
}

impl fmt::Display for IngestStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Discovery => "discovery",
            Self::Parsing => "parsing",
            Self::Database => "database",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestErrorCategory {
    UnsupportedFormat,
    CorruptData,
    Permission,
    TransientRead,
    Read,
    Database,
}

impl fmt::Display for IngestErrorCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedFormat => "unsupported_format",
            Self::CorruptData => "corrupt_data",
            Self::Permission => "permission",
            Self::TransientRead => "transient_read",
            Self::Read => "read",
            Self::Database => "database",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestIssue {
    pub stage: IngestStage,
    pub locator: String,
    pub category: IngestErrorCategory,
    pub message: String,
}

impl IngestIssue {
    fn from_error(stage: IngestStage, locator: String, error: &anyhow::Error) -> Self {
        let io_error = error.downcast_ref::<std::io::Error>();
        let category = if matches!(stage, IngestStage::Database)
            || error.downcast_ref::<rusqlite::Error>().is_some()
        {
            IngestErrorCategory::Database
        } else if error
            .downcast_ref::<crate::parsers::UnsupportedFormat>()
            .is_some()
        {
            IngestErrorCategory::UnsupportedFormat
        } else if io_error.is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
        {
            IngestErrorCategory::Permission
        } else if io_error.is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
            )
        }) {
            IngestErrorCategory::TransientRead
        } else if error.downcast_ref::<serde_json::Error>().is_some() {
            IngestErrorCategory::CorruptData
        } else {
            IngestErrorCategory::Read
        };
        Self {
            stage,
            locator,
            category,
            message: format!("{error:#}"),
        }
    }
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

    pub fn total_skipped(&self) -> usize {
        self.agents.iter().map(|row| row.skipped).sum()
    }

    pub fn total_failed(&self) -> usize {
        self.agents.iter().map(|row| row.failed).sum()
    }

    pub fn total_warnings(&self) -> usize {
        self.agents.iter().map(|row| row.warnings.len()).sum()
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationFailure {
    pub locator: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyCheck {
    pub name: String,
    pub checked: usize,
    pub passed: bool,
    pub failures: Vec<VerificationFailure>,
}

impl VerifyCheck {
    pub(crate) fn new(
        name: impl Into<String>,
        checked: usize,
        failures: Vec<VerificationFailure>,
    ) -> Self {
        Self {
            name: name.into(),
            checked,
            passed: failures.is_empty(),
            failures,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyReport {
    pub path: PathBuf,
    pub passed: bool,
    pub checks: Vec<VerifyCheck>,
}

impl VerifyReport {
    pub(crate) fn new(path: PathBuf, checks: Vec<VerifyCheck>) -> Self {
        Self {
            path,
            passed: checks.iter().all(|check| check.passed),
            checks,
        }
    }

    pub fn failure_count(&self) -> usize {
        self.checks.iter().map(|check| check.failures.len()).sum()
    }
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
