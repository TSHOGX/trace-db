use crate::{
    config::ExcludeMatcher,
    model::{Agent, Capture, EventKind, IngestMode, ParsedSession, Session},
    parsers::{parser, SessionCandidate},
    search, store, ConfigOverrides, SearchRequest, SearchResult, TokenizerKind, TraceDbConfig,
};
use anyhow::{anyhow, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
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

    /// Load the resolved runtime configuration and open its selected archive.
    pub fn open_default() -> Result<Self> {
        let config = TraceDbConfig::load(ConfigOverrides::default())?;
        Self::open_configured(&config)
    }

    /// Open or create the archive selected by a resolved TraceDB configuration.
    pub fn open_configured(config: &TraceDbConfig) -> Result<Self> {
        let path = config.database_path.clone();
        let connection = store::open_configured(
            &path,
            config.tokenizer,
            config.tokenizer_extension.as_deref(),
        )?;
        Ok(Self { path, connection })
    }

    /// Open an existing archive without migrations or archive-record writes.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let connection = store::open_read_only(&path)?;
        Ok(Self { path, connection })
    }

    /// Plan an ingest against an existing or not-yet-created archive without writing it.
    pub fn ingest_dry_run_at(
        path: impl AsRef<Path>,
        request: IngestRequest,
    ) -> Result<IngestDryRunReport> {
        let path = path.as_ref();
        let db = if path.exists() {
            Self::open_read_only(path)?
        } else {
            Self::open(":memory:")?
        };
        db.ingest_dry_run(request)
    }

    /// Return the path used to open this archive.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Discover and ingest sessions from the requested native agent stores.
    pub fn ingest(&mut self, request: IngestRequest) -> Result<IngestReport> {
        let exclusions = ExcludeMatcher::new(&request.exclude)?;
        let agents = if request.agents.is_empty() {
            Agent::ALL.to_vec()
        } else {
            request.agents
        };
        let mut reports = Vec::with_capacity(agents.len());
        for agent in agents {
            let root = request.root.clone().unwrap_or_else(|| native_root(agent));
            let AgentScan {
                discovered,
                unchanged,
                mut skipped,
                skipped_by_since,
                mut failures,
                parsed_candidates,
            } = self.scan_agent(agent, &root, request.mode, request.since_ms, &exclusions);
            let mut parsed = 0;
            let mut ingested = 0;
            for (candidate, parsed_session) in parsed_candidates {
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

    /// Discover and parse sessions without mutating the selected archive.
    pub fn ingest_dry_run(&self, request: IngestRequest) -> Result<IngestDryRunReport> {
        let exclusions = ExcludeMatcher::new(&request.exclude)?;
        let agents = if request.agents.is_empty() {
            Agent::ALL.to_vec()
        } else {
            request.agents
        };
        let mut reports = Vec::with_capacity(agents.len());
        for agent in agents {
            let root = request.root.clone().unwrap_or_else(|| native_root(agent));
            let AgentScan {
                discovered,
                unchanged,
                mut skipped,
                skipped_by_since,
                mut failures,
                parsed_candidates,
            } = self.scan_agent(agent, &root, request.mode, request.since_ms, &exclusions);
            let mut changed = 0;
            let mut estimated_full_capture_bytes = 0;
            for (candidate, parsed_session) in parsed_candidates {
                match parsed_session {
                    Ok(Some(session)) => {
                        changed += 1;
                        estimated_full_capture_bytes += estimated_capture_bytes(&session);
                    }
                    Ok(None) => skipped += 1,
                    Err(error) => failures.push(IngestIssue::from_error(
                        IngestStage::Parsing,
                        candidate.locator,
                        &error,
                    )),
                }
            }
            reports.push(AgentIngestDryRunReport {
                agent,
                root,
                discovered,
                changed,
                unchanged,
                skipped,
                skipped_by_since,
                failed: failures.len(),
                estimated_full_capture_bytes,
                warnings: Vec::new(),
                failures,
            });
        }
        Ok(IngestDryRunReport {
            dry_run: true,
            mode: request.mode,
            agents: reports,
        })
    }

    /// Watch native stores, ingesting on startup, filesystem activity, and a
    /// periodic fallback interval until the caller requests shutdown.
    pub fn watch(
        &mut self,
        request: WatchRequest,
        stop: &AtomicBool,
        observer: &mut dyn FnMut(WatchEvent) -> Result<()>,
    ) -> Result<WatchSummary> {
        request.validate()?;
        let (events, mut watcher, watcher_available, mut issues) =
            build_watch_channel(&request.ingest, observer)?;
        let mut run_count = 0;
        Self::emit_watch_run(
            self,
            &request,
            WatchTrigger::Startup,
            observer,
            &mut issues,
            &mut run_count,
        )?;
        if request.once {
            drop(watcher.take());
            return Ok(WatchSummary {
                runs: run_count,
                stopped: false,
                watcher_available,
                issues,
            });
        }

        let interval = Duration::from_secs(request.interval_seconds);
        let debounce = Duration::from_millis(request.debounce_ms);
        let mut next_periodic = Instant::now() + interval;
        let mut pending_paths = Vec::new();
        let mut pending_deadline: Option<Instant> = None;
        let mut watch_channel_open = true;
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            let deadline =
                pending_deadline.map_or(next_periodic, |pending| pending.min(next_periodic));
            let timeout = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(250));
            if !watch_channel_open {
                thread::sleep(timeout);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if next_periodic <= Instant::now() {
                    Self::emit_watch_run(
                        self,
                        &request,
                        WatchTrigger::Periodic,
                        observer,
                        &mut issues,
                        &mut run_count,
                    )?;
                    next_periodic = Instant::now() + interval;
                }
                continue;
            }
            match events.recv_timeout(timeout) {
                Ok(Ok(event)) => {
                    if is_relevant_watch_event(&event.kind) {
                        pending_paths.extend(event.paths);
                        pending_deadline = Some(Instant::now() + debounce);
                    }
                }
                Ok(Err(error)) => {
                    let issue = WatchIssue::watcher(format!("filesystem notification: {error}"));
                    observer(WatchEvent::Issue(issue.clone()))?;
                    issues.push(issue);
                }
                Err(RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    if pending_deadline.is_some_and(|deadline| deadline <= now) {
                        if let Some(issue) = wait_for_stable_paths(&pending_paths, debounce) {
                            observer(WatchEvent::Issue(issue.clone()))?;
                            issues.push(issue);
                        }
                        pending_paths.clear();
                        pending_deadline = None;
                        Self::emit_watch_run(
                            self,
                            &request,
                            WatchTrigger::Filesystem,
                            observer,
                            &mut issues,
                            &mut run_count,
                        )?;
                        next_periodic = Instant::now() + interval;
                    } else if next_periodic <= now {
                        Self::emit_watch_run(
                            self,
                            &request,
                            WatchTrigger::Periodic,
                            observer,
                            &mut issues,
                            &mut run_count,
                        )?;
                        next_periodic = Instant::now() + interval;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let issue = WatchIssue::watcher("filesystem watcher channel closed");
                    observer(WatchEvent::Issue(issue.clone()))?;
                    issues.push(issue);
                    watch_channel_open = false;
                    Self::emit_watch_run(
                        self,
                        &request,
                        WatchTrigger::Periodic,
                        observer,
                        &mut issues,
                        &mut run_count,
                    )?;
                    next_periodic = Instant::now() + interval;
                }
            }
        }
        drop(watcher.take());
        Ok(WatchSummary {
            runs: run_count,
            stopped: true,
            watcher_available,
            issues,
        })
    }

    fn emit_watch_run(
        db: &mut TraceDb,
        request: &WatchRequest,
        trigger: WatchTrigger,
        observer: &mut dyn FnMut(WatchEvent) -> Result<()>,
        issues: &mut Vec<WatchIssue>,
        run_count: &mut usize,
    ) -> Result<()> {
        let started = Instant::now();
        let started_at_ms = chrono::Utc::now().timestamp_millis();
        match db.ingest(request.ingest.clone()) {
            Ok(report) => {
                *run_count += 1;
                observer(WatchEvent::Run(WatchRun {
                    trigger,
                    started_at_ms,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    report,
                }))?;
            }
            Err(error) => {
                let issue = WatchIssue::ingest(error);
                observer(WatchEvent::Issue(issue.clone()))?;
                issues.push(issue);
            }
        }
        Ok(())
    }

    fn scan_agent(
        &self,
        agent: Agent,
        root: &Path,
        mode: IngestMode,
        since_ms: Option<i64>,
        exclusions: &ExcludeMatcher,
    ) -> AgentScan {
        let parser = parser(agent);
        let mut failures = Vec::new();
        let discovery = match parser.discover(root) {
            Ok(discovery) => discovery,
            Err(error) => {
                failures.push(IngestIssue::from_error(
                    IngestStage::Discovery,
                    root.display().to_string(),
                    &error,
                ));
                return AgentScan::failed(failures);
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
                return AgentScan {
                    discovered,
                    failures,
                    ..AgentScan::default()
                };
            }
        };
        let mut unchanged = 0;
        let mut skipped = 0;
        let mut skipped_by_since = 0;
        let mut pending = Vec::new();
        for candidate in discovery.candidates {
            if exclusions.matches(&candidate.locator, &candidate.path) {
                skipped += 1;
                continue;
            }
            if since_ms
                .is_some_and(|cutoff| candidate.updated_at_ms.is_some_and(|time| time < cutoff))
            {
                skipped_by_since += 1;
                skipped += 1;
                continue;
            }
            if states.get(&candidate.locator).is_some_and(|state| {
                state.fingerprint == candidate.fingerprint
                    && (matches!(mode, IngestMode::Partial)
                        || matches!(state.mode, IngestMode::Full))
            }) {
                unchanged += 1;
                continue;
            }
            pending.push(candidate);
        }
        AgentScan {
            discovered,
            unchanged,
            skipped,
            skipped_by_since,
            failures,
            parsed_candidates: parser.parse_many(&pending, root),
        }
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
        self.show_with_options(ShowRequest::new(session_id))
    }

    /// Load one session and optionally filter its event stream by index and kind.
    pub fn show_with_options(&self, request: ShowRequest) -> Result<Option<SessionTrace>> {
        if request.from_idx.is_some_and(|value| value < 0)
            || request.to_idx.is_some_and(|value| value < 0)
        {
            anyhow::bail!("show event indexes must not be negative");
        }
        if request
            .from_idx
            .zip(request.to_idx)
            .is_some_and(|(from, to)| from > to)
        {
            anyhow::bail!("show --from must not be greater than --to");
        }
        let Some(mut trace) = store::show(&self.connection, &request.session_id)? else {
            return Ok(None);
        };
        if request.from_idx.is_some() || request.to_idx.is_some() || !request.kinds.is_empty() {
            trace.events.retain(|event| {
                request.from_idx.is_none_or(|from| event.idx >= from)
                    && request.to_idx.is_none_or(|to| event.idx <= to)
                    && (request.kinds.is_empty() || request.kinds.contains(&event.kind))
            });
        }
        Ok(Some(trace))
    }

    /// List archived sessions with stable keyset pagination and metadata filters.
    pub fn list(&self, request: ListRequest) -> Result<ListPage> {
        store::list(&self.connection, &request)
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

type WatchChannel = (
    Receiver<notify::Result<notify::Event>>,
    Option<RecommendedWatcher>,
    bool,
    Vec<WatchIssue>,
);

fn build_watch_channel(
    request: &IngestRequest,
    observer: &mut dyn FnMut(WatchEvent) -> Result<()>,
) -> Result<WatchChannel> {
    let (sender, receiver) = mpsc::channel();
    let callback_sender = sender.clone();
    let watcher = match notify::recommended_watcher(move |result| {
        let _ = callback_sender.send(result);
    }) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            let issue = WatchIssue::watcher(format!("create filesystem watcher: {error}"));
            observer(WatchEvent::Issue(issue.clone()))?;
            return Ok((receiver, None, false, vec![issue]));
        }
    };
    let mut watcher = watcher;
    let mut issues = Vec::new();
    let mut available = false;
    let roots = if let Some(root) = &request.root {
        vec![root.clone()]
    } else if request.agents.is_empty() {
        Agent::ALL.iter().copied().map(native_root).collect()
    } else {
        request.agents.iter().copied().map(native_root).collect()
    };
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        if !seen.insert(root.clone()) {
            continue;
        }
        let Some(watcher) = watcher.as_mut() else {
            break;
        };
        match watcher.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => available = true,
            Err(error) => {
                let issue = WatchIssue::watcher(format!("watch {}: {error}", root.display()));
                observer(WatchEvent::Issue(issue.clone()))?;
                issues.push(issue);
            }
        }
    }
    Ok((receiver, watcher, available, issues))
}

fn is_relevant_watch_event(kind: &notify::EventKind) -> bool {
    matches!(
        kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Modify(_)
            | notify::EventKind::Remove(_)
            | notify::EventKind::Any
    )
}

fn wait_for_stable_paths(paths: &[PathBuf], debounce: Duration) -> Option<WatchIssue> {
    let paths = paths
        .iter()
        .filter(|path| !path.to_string_lossy().starts_with("watcher-error:"))
        .cloned()
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return None;
    }
    let pause = debounce.min(Duration::from_millis(250));
    if pause.is_zero() {
        return None;
    }
    let before = paths
        .iter()
        .map(|path| metadata_signature(path))
        .collect::<Vec<_>>();
    thread::sleep(pause);
    let after = paths
        .iter()
        .map(|path| metadata_signature(path))
        .collect::<Vec<_>>();
    if before != after {
        Some(WatchIssue::stability(format!(
            "native files were still changing after {} ms; ingesting latest state",
            pause.as_millis()
        )))
    } else {
        None
    }
}

fn metadata_signature(path: &Path) -> Option<(u64, Option<i64>)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok());
    Some((metadata.len(), modified))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchRequest {
    pub ingest: IngestRequest,
    pub interval_seconds: u64,
    pub debounce_ms: u64,
    pub once: bool,
}

impl WatchRequest {
    pub fn validate(&self) -> Result<()> {
        if self.interval_seconds == 0 {
            anyhow::bail!("watch interval must be greater than zero");
        }
        if self.debounce_ms == 0 {
            anyhow::bail!("watch debounce must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchRun {
    pub trigger: WatchTrigger,
    pub started_at_ms: i64,
    pub elapsed_ms: u64,
    pub report: IngestReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchTrigger {
    Startup,
    Filesystem,
    Periodic,
}

impl fmt::Display for WatchTrigger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Startup => "startup",
            Self::Filesystem => "filesystem",
            Self::Periodic => "periodic",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchIssue {
    pub stage: WatchIssueStage,
    pub message: String,
}

impl WatchIssue {
    fn watcher(message: impl Into<String>) -> Self {
        Self {
            stage: WatchIssueStage::Watcher,
            message: message.into(),
        }
    }

    fn stability(message: impl Into<String>) -> Self {
        Self {
            stage: WatchIssueStage::Stability,
            message: message.into(),
        }
    }

    fn ingest(error: impl fmt::Display) -> Self {
        Self {
            stage: WatchIssueStage::Ingest,
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchIssueStage {
    Watcher,
    Stability,
    Ingest,
}

impl fmt::Display for WatchIssueStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Watcher => "watcher",
            Self::Stability => "stability",
            Self::Ingest => "ingest",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WatchEvent {
    Run(WatchRun),
    Issue(WatchIssue),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchSummary {
    pub runs: usize,
    pub stopped: bool,
    pub watcher_available: bool,
    pub issues: Vec<WatchIssue>,
}

#[derive(Default)]
struct AgentScan {
    discovered: usize,
    unchanged: usize,
    skipped: usize,
    skipped_by_since: usize,
    failures: Vec<IngestIssue>,
    parsed_candidates: Vec<(SessionCandidate, Result<Option<ParsedSession>>)>,
}

impl AgentScan {
    fn failed(failures: Vec<IngestIssue>) -> Self {
        Self {
            failures,
            ..Self::default()
        }
    }
}

fn estimated_capture_bytes(session: &ParsedSession) -> u64 {
    session
        .session
        .sources
        .iter()
        .filter_map(|source| match source.capture.as_ref() {
            Some(Capture::Bytes { bytes, .. }) => u64::try_from(bytes.len()).ok(),
            Some(Capture::File { path }) => source
                .bytes
                .and_then(|bytes| u64::try_from(bytes).ok())
                .or_else(|| std::fs::metadata(path).ok().map(|metadata| metadata.len())),
            None => source.bytes.and_then(|bytes| u64::try_from(bytes).ok()),
        })
        .sum()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconstructionOptions {
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShowRequest {
    pub session_id: String,
    #[serde(rename = "from")]
    pub from_idx: Option<i64>,
    #[serde(rename = "to")]
    pub to_idx: Option<i64>,
    #[serde(default, rename = "kind")]
    pub kinds: Vec<EventKind>,
}

impl ShowRequest {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            from_idx: None,
            to_idx: None,
            kinds: Vec::new(),
        }
    }
}

/// Open an existing archive without migration and verify its stored contract.
pub fn verify_archive(path: impl AsRef<Path>) -> Result<VerifyReport> {
    let path = path.as_ref();
    let connection = store::open_for_verification(path)?;
    store::verify(&connection, path)
}

/// Inspect runtime readiness, native stores, tokenizer configuration, and archive health.
pub fn doctor_archive(path: impl AsRef<Path>) -> DoctorReport {
    let roots = Agent::ALL
        .into_iter()
        .map(|agent| (agent, native_root(agent)))
        .collect();
    let extension = std::env::var_os("TRACEDB_JIEBA_EXT").map(PathBuf::from);
    let tokenizer = if extension.is_some() {
        TokenizerKind::Jieba
    } else {
        TokenizerKind::Unicode61
    };
    doctor_with_roots(path.as_ref(), roots, tokenizer, extension)
}

/// Inspect readiness using the database, agents, and tokenizer in a resolved config.
pub fn doctor_configured(config: &TraceDbConfig) -> DoctorReport {
    let roots = config
        .default_agents
        .iter()
        .copied()
        .map(|agent| (agent, native_root(agent)))
        .collect();
    doctor_with_roots(
        &config.database_path,
        roots,
        config.tokenizer,
        config.tokenizer_extension.clone(),
    )
}

fn doctor_with_roots(
    path: &Path,
    roots: Vec<(Agent, PathBuf)>,
    tokenizer_kind: TokenizerKind,
    tokenizer_extension: Option<PathBuf>,
) -> DoctorReport {
    let database = doctor_database(path);
    let mut agents = Vec::with_capacity(roots.len());
    for (agent, root) in roots {
        let parser = parser(agent);
        let (discovered, failures) = if root.exists() {
            match parser.discover(&root) {
                Ok(discovery) => (
                    discovery.candidates.len(),
                    discovery
                        .failures
                        .into_iter()
                        .map(|failure| DoctorFailure {
                            locator: failure.locator,
                            message: format!("{:#}", failure.error),
                        })
                        .collect(),
                ),
                Err(error) => (
                    0,
                    vec![DoctorFailure {
                        locator: root.display().to_string(),
                        message: format!("{error:#}"),
                    }],
                ),
            }
        } else {
            (0, Vec::new())
        };
        agents.push(DoctorAgent {
            agent,
            root: root.clone(),
            exists: root.exists(),
            readable: if root.is_file() {
                std::fs::File::open(&root).is_ok()
            } else {
                root.read_dir().is_ok()
            },
            discovered,
            failures,
        });
    }
    let tokenizer = match (tokenizer_kind, tokenizer_extension) {
        (TokenizerKind::Jieba, Some(extension)) => match store::probe_jieba_extension(&extension) {
            Ok(()) => DoctorTokenizer {
                tokenizer: "jieba".into(),
                extension: Some(extension),
                available: true,
                error: None,
            },
            Err(error) => DoctorTokenizer {
                tokenizer: "jieba".into(),
                extension: Some(extension),
                available: false,
                error: Some(format!("{error:#}")),
            },
        },
        (TokenizerKind::Jieba, None) => DoctorTokenizer {
            tokenizer: "jieba".into(),
            extension: None,
            available: false,
            error: Some("jieba tokenizer requires a configured extension path".into()),
        },
        (TokenizerKind::Unicode61, _) => DoctorTokenizer {
            tokenizer: store::PORTABLE_TOKENIZER.into(),
            extension: None,
            available: true,
            error: None,
        },
    };
    let healthy = database.error.is_none()
        && database
            .verification
            .as_ref()
            .is_none_or(|report| report.passed)
        && database.writable
        && tokenizer.available
        && agents.iter().all(|agent| agent.failures.is_empty());
    DoctorReport {
        healthy,
        database,
        agents,
        tokenizer,
        runtime: DoctorRuntime {
            tracedb_version: env!("CARGO_PKG_VERSION").into(),
            sqlite_version: rusqlite::version().into(),
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
        },
    }
}

fn doctor_database(path: &Path) -> DoctorDatabase {
    let exists = path.exists();
    let ancestor = path
        .parent()
        .and_then(|parent| {
            parent
                .ancestors()
                .find(|ancestor| ancestor.exists())
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| PathBuf::from("."));
    let parent_writable = tempfile::NamedTempFile::new_in(&ancestor).is_ok();
    let writable = parent_writable
        && (!exists
            || std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .is_ok());
    if !exists {
        return DoctorDatabase {
            path: path.to_path_buf(),
            exists,
            writable,
            verification: None,
            error: None,
        };
    }
    match verify_archive(path) {
        Ok(verification) => DoctorDatabase {
            path: path.to_path_buf(),
            exists,
            writable,
            verification: Some(verification),
            error: None,
        },
        Err(error) => DoctorDatabase {
            path: path.to_path_buf(),
            exists,
            writable,
            verification: None,
            error: Some(format!("{error:#}")),
        },
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
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Default for IngestRequest {
    fn default() -> Self {
        Self {
            agents: Vec::new(),
            mode: IngestMode::Partial,
            root: None,
            since_ms: None,
            exclude: Vec::new(),
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

/// Machine-readable result of an ingest plan that performs no archive writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestDryRunReport {
    pub dry_run: bool,
    pub mode: IngestMode,
    pub agents: Vec<AgentIngestDryRunReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIngestDryRunReport {
    pub agent: Agent,
    pub root: PathBuf,
    pub discovered: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub skipped_by_since: usize,
    pub failed: usize,
    pub estimated_full_capture_bytes: u64,
    pub warnings: Vec<IngestIssue>,
    pub failures: Vec<IngestIssue>,
}

impl IngestDryRunReport {
    pub fn total_discovered(&self) -> usize {
        self.agents.iter().map(|row| row.discovered).sum()
    }

    pub fn total_changed(&self) -> usize {
        self.agents.iter().map(|row| row.changed).sum()
    }

    pub fn total_unchanged(&self) -> usize {
        self.agents.iter().map(|row| row.unchanged).sum()
    }

    pub fn total_skipped(&self) -> usize {
        self.agents.iter().map(|row| row.skipped).sum()
    }

    pub fn total_failed(&self) -> usize {
        self.agents.iter().map(|row| row.failed).sum()
    }

    pub fn total_estimated_full_capture_bytes(&self) -> u64 {
        self.agents
            .iter()
            .map(|row| row.estimated_full_capture_bytes)
            .sum()
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
#[serde(rename_all = "camelCase")]
pub struct ListRequest {
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    pub cursor: Option<String>,
    pub agent: Option<Agent>,
    pub cwd: Option<String>,
    pub since_ms: Option<i64>,
    pub mode: Option<IngestMode>,
    pub model: Option<String>,
    pub provider: Option<String>,
}

fn default_list_limit() -> usize {
    50
}

impl Default for ListRequest {
    fn default() -> Self {
        Self {
            limit: default_list_limit(),
            cursor: None,
            agent: None,
            cwd: None,
            since_ms: None,
            mode: None,
            model: None,
            provider: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub agent: Agent,
    pub cwd: Option<String>,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub mode: IngestMode,
    pub events: i64,
    pub ingested_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPage {
    pub sessions: Vec<SessionSummary>,
    pub next_cursor: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorFailure {
    pub locator: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorDatabase {
    pub path: PathBuf,
    pub exists: bool,
    pub writable: bool,
    pub verification: Option<VerifyReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorAgent {
    pub agent: Agent,
    pub root: PathBuf,
    pub exists: bool,
    pub readable: bool,
    pub discovered: usize,
    pub failures: Vec<DoctorFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorTokenizer {
    pub tokenizer: String,
    pub extension: Option<PathBuf>,
    pub available: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorRuntime {
    pub tracedb_version: String,
    pub sqlite_version: String,
    pub os: String,
    pub architecture: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub healthy: bool,
    pub database: DoctorDatabase,
    pub agents: Vec<DoctorAgent>,
    pub tokenizer: DoctorTokenizer,
    pub runtime: DoctorRuntime,
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
