use crate::{
    config::{ExcludeMatcher, DEFAULT_WATCH_DEBOUNCE_MS, DEFAULT_WATCH_INTERVAL_SECONDS},
    model::{Agent, Capture, EventKind, ParsedSession, Session},
    parsers::{parser, DiscoveryHints, SessionCandidate},
    search, store, ConfigOverrides, SearchRequest, SearchResult, TokenizerKind, TraceDbConfig,
};
use anyhow::{anyhow, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
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

    /// Open an existing archive read-only with its configured tokenizer loaded.
    pub fn open_read_only_configured(
        path: impl AsRef<Path>,
        tokenizer: TokenizerKind,
        tokenizer_extension: Option<&Path>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let connection = store::open_read_only_configured(&path, tokenizer, tokenizer_extension)?;
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
        let mut ingested_locators = Vec::new();
        let mut failure_quarantine = Vec::new();
        for agent in agents {
            let root = request.root.clone().unwrap_or_else(|| native_root(agent));
            let AgentScan {
                discovered,
                unchanged,
                mut skipped,
                skipped_by_since,
                mut failures,
                parsed_candidates,
                codex_rollout_cache,
            } = self.scan_agent(agent, &root, request.since_ms, &exclusions);
            if let Some(cache) = codex_rollout_cache {
                store::save_codex_rollout_cache(&mut self.connection, &cache)?;
            }
            let mut parsed = 0;
            let mut ingested = 0;
            let mut ready = Vec::new();
            for (candidate, parsed_session) in parsed_candidates {
                match parsed_session {
                    Ok(Some(mut session)) => {
                        parsed += 1;
                        session.session.fingerprint = candidate.fingerprint.clone();
                        ready.push((candidate, session));
                    }
                    Ok(None) => {
                        parsed += 1;
                        skipped += 1;
                    }
                    Err(error) => {
                        failure_quarantine.push((candidate.locator.clone(), candidate.fingerprint));
                        failures.push(IngestIssue::from_error(
                            IngestStage::Parsing,
                            candidate.locator,
                            &error,
                        ));
                    }
                }
            }
            if !ready.is_empty() {
                let (candidates, mut sessions): (Vec<_>, Vec<_>) = ready.into_iter().unzip();
                if store::upsert_many(&mut self.connection, &mut sessions).is_ok() {
                    ingested += sessions.len();
                    ingested_locators
                        .extend(candidates.into_iter().map(|candidate| candidate.locator));
                } else {
                    // Preserve best-effort ingest semantics if one session has
                    // a malformed object or otherwise poisons the batch.
                    for (candidate, session) in candidates.into_iter().zip(sessions) {
                        match store::upsert(&mut self.connection, session) {
                            Ok(()) => {
                                ingested += 1;
                                ingested_locators.push(candidate.locator);
                            }
                            Err(error) => {
                                failure_quarantine
                                    .push((candidate.locator.clone(), candidate.fingerprint));
                                failures.push(IngestIssue::from_error(
                                    IngestStage::Database,
                                    candidate.locator,
                                    &error,
                                ));
                            }
                        }
                    }
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
        let mut report = IngestReport {
            agents: reports,
            ack: None,
        };
        store::update_ingest_quarantine(
            &mut self.connection,
            &ingested_locators,
            &failure_quarantine,
        )?;
        // Persist the run status before exposing its acknowledgement.  A caller
        // can therefore safely treat the ack as a durable commit boundary; a
        // crash before this point yields no false-positive acknowledgement.
        report.ack = Some(store::record_ingest_status(&mut self.connection, &report)?);
        Ok(report)
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
                codex_rollout_cache: _,
            } = self.scan_agent(agent, &root, request.since_ms, &exclusions);
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
        let _ = Self::emit_watch_run(
            self,
            &request,
            WatchTrigger::Startup,
            None,
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
        let mut periodic_interval = interval;
        let mut next_periodic = Instant::now() + periodic_interval;
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
                    if let Some(report) = Self::emit_watch_run(
                        self,
                        &request,
                        WatchTrigger::Periodic,
                        None,
                        observer,
                        &mut issues,
                        &mut run_count,
                    )? {
                        periodic_interval =
                            adaptive_periodic_interval(interval, periodic_interval, &report);
                    }
                    next_periodic = Instant::now() + periodic_interval;
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
                        let touched_paths = std::mem::take(&mut pending_paths);
                        pending_deadline = None;
                        Self::emit_watch_run(
                            self,
                            &request,
                            WatchTrigger::Filesystem,
                            Some(&touched_paths),
                            observer,
                            &mut issues,
                            &mut run_count,
                        )?;
                        periodic_interval = interval;
                        next_periodic = Instant::now() + periodic_interval;
                    } else if next_periodic <= now {
                        if let Some(report) = Self::emit_watch_run(
                            self,
                            &request,
                            WatchTrigger::Periodic,
                            None,
                            observer,
                            &mut issues,
                            &mut run_count,
                        )? {
                            periodic_interval =
                                adaptive_periodic_interval(interval, periodic_interval, &report);
                        }
                        next_periodic = Instant::now() + periodic_interval;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let issue = WatchIssue::watcher("filesystem watcher channel closed");
                    observer(WatchEvent::Issue(issue.clone()))?;
                    issues.push(issue);
                    watch_channel_open = false;
                    if let Some(report) = Self::emit_watch_run(
                        self,
                        &request,
                        WatchTrigger::Periodic,
                        None,
                        observer,
                        &mut issues,
                        &mut run_count,
                    )? {
                        periodic_interval =
                            adaptive_periodic_interval(interval, periodic_interval, &report);
                    }
                    next_periodic = Instant::now() + periodic_interval;
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
        touched_paths: Option<&[PathBuf]>,
        observer: &mut dyn FnMut(WatchEvent) -> Result<()>,
        issues: &mut Vec<WatchIssue>,
        run_count: &mut usize,
    ) -> Result<Option<IngestReport>> {
        let started = Instant::now();
        let started_at_ms = chrono::Utc::now().timestamp_millis();
        let mut ingest_request = request.ingest.clone();
        if matches!(trigger, WatchTrigger::Filesystem) {
            if let Some(paths) = touched_paths {
                let scoped = agents_for_watch_paths(
                    paths,
                    &ingest_request.agents,
                    ingest_request.root.as_deref(),
                );
                if !scoped.is_empty() {
                    ingest_request.agents = scoped;
                }
            }
        }
        match db.ingest(ingest_request) {
            Ok(report) => {
                *run_count += 1;
                observer(WatchEvent::Run(WatchRun {
                    trigger,
                    started_at_ms,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    report: report.clone(),
                }))?;
                Ok(Some(report))
            }
            Err(error) => {
                let issue = WatchIssue::ingest(error);
                observer(WatchEvent::Issue(issue.clone()))?;
                issues.push(issue);
                Ok(None)
            }
        }
    }

    fn scan_agent(
        &self,
        agent: Agent,
        root: &Path,
        since_ms: Option<i64>,
        exclusions: &ExcludeMatcher,
    ) -> AgentScan {
        let parser = parser(agent);
        let mut failures = Vec::new();
        let states = match store::candidate_states(&self.connection, agent) {
            Ok(states) => states,
            Err(error) => {
                failures.push(IngestIssue::from_error(
                    IngestStage::Database,
                    self.path.display().to_string(),
                    &error,
                ));
                return AgentScan::failed(failures);
            }
        };
        let quarantine = match store::load_ingest_quarantine(&self.connection) {
            Ok(quarantine) => quarantine,
            Err(error) => {
                failures.push(IngestIssue::from_error(
                    IngestStage::Database,
                    self.path.display().to_string(),
                    &error,
                ));
                return AgentScan::failed(failures);
            }
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut hints = DiscoveryHints {
            fingerprints: states
                .iter()
                .map(|(locator, state)| (locator.clone(), state.fingerprint.clone()))
                .collect(),
            codex_rollout_cache: if agent == Agent::Codex {
                match store::load_codex_rollout_cache(&self.connection) {
                    Ok(cache) => cache,
                    Err(error) => {
                        failures.push(IngestIssue::from_error(
                            IngestStage::Database,
                            self.path.display().to_string(),
                            &error,
                        ));
                        return AgentScan::failed(failures);
                    }
                }
            } else {
                HashMap::new()
            },
        };
        let discovery = match parser.discover_with_hints(root, &mut hints) {
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
            if store::is_ingest_quarantined(
                &quarantine,
                &candidate.locator,
                &candidate.fingerprint,
                now_ms,
            ) {
                skipped += 1;
                continue;
            }
            if states
                .get(&candidate.locator)
                .is_some_and(|state| state.fingerprint == candidate.fingerprint)
            {
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
            parsed_candidates: parse_pending(agent, &pending, root),
            codex_rollout_cache: (agent == Agent::Codex).then_some(hints.codex_rollout_cache),
        }
    }

    /// Insert one already-parsed session through the same transactional path.
    pub fn ingest_session(&mut self, session: ParsedSession) -> Result<()> {
        store::upsert(&mut self.connection, session)
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

    /// Read one session's archive coverage without loading events or sources.
    pub fn coverage(&self, session_id: &str) -> Result<Option<SessionCoverage>> {
        store::coverage(&self.connection, session_id)
    }

    /// Return per-agent and archive-wide counts.
    pub fn stats(&self) -> Result<ArchiveStats> {
        let agents = store::stats(&self.connection)?
            .into_iter()
            .map(|(agent, sessions, events)| {
                Ok(AgentStats {
                    agent: agent.parse().map_err(|message: String| anyhow!(message))?,
                    sessions,
                    events,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ArchiveStats {
            path: self.path.clone(),
            total_sessions: agents.iter().map(|row| row.sessions).sum(),
            total_events: agents.iter().map(|row| row.events).sum(),
            agents,
        })
    }

    /// Rebuild the gated FTS index from normalized events.
    pub fn reindex(&self) -> Result<()> {
        store::rebuild_fts(&self.connection)
    }

    /// Create a verified, consistent SQLite snapshot at `destination`.
    pub fn backup(&self, destination: impl AsRef<Path>) -> Result<BackupReport> {
        store::backup(&self.connection, destination.as_ref())
    }

    /// Import a verified archive snapshot idempotently into this archive.
    pub fn import_archive(&mut self, source: impl AsRef<Path>) -> Result<ImportReport> {
        store::import_archive(&mut self.connection, source.as_ref())
    }

    /// Report content-addressed objects that are no longer referenced.
    pub fn gc(&self, dry_run: bool) -> Result<GcReport> {
        store::gc_report(&self.connection, dry_run)
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
        Ok(self
            .reconstruct_manifest(session_id, out_dir, options)?
            .files
            .into_iter()
            .map(|file| file.path)
            .collect())
    }

    /// Restore full-capture sources and return a versioned manifest of every file.
    pub fn reconstruct_manifest(
        &self,
        session_id: &str,
        out_dir: impl AsRef<Path>,
        options: ReconstructionOptions,
    ) -> Result<RestoreManifest> {
        store::reconstruct_manifest(&self.connection, session_id, out_dir.as_ref(), options)
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
    codex_rollout_cache: Option<HashMap<String, crate::parsers::codex::CodexRolloutCacheEntry>>,
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
    doctor_with_roots(
        path.as_ref(),
        roots,
        tokenizer,
        extension,
        DEFAULT_WATCH_INTERVAL_SECONDS,
        DEFAULT_WATCH_DEBOUNCE_MS,
    )
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
        config.watch_interval_seconds,
        config.watch_debounce_ms,
    )
}

fn doctor_with_roots(
    path: &Path,
    roots: Vec<(Agent, PathBuf)>,
    tokenizer_kind: TokenizerKind,
    tokenizer_extension: Option<PathBuf>,
    watch_interval_seconds: u64,
    watch_debounce_ms: u64,
) -> DoctorReport {
    let mut latest_native_updated_at_ms = None;
    let mut agents = Vec::with_capacity(roots.len());
    for (agent, root) in roots {
        let parser = parser(agent);
        let (discovered, latest_updated_at_ms, failures) = if root.exists() {
            match parser.discover(&root) {
                Ok(discovery) => {
                    let latest = discovery
                        .candidates
                        .iter()
                        .filter_map(|candidate| candidate.updated_at_ms)
                        .max();
                    (
                        discovery.candidates.len(),
                        latest,
                        discovery
                            .failures
                            .into_iter()
                            .map(|failure| DoctorFailure {
                                locator: failure.locator,
                                message: format!("{:#}", failure.error),
                            })
                            .collect(),
                    )
                }
                Err(error) => (
                    0,
                    None,
                    vec![DoctorFailure {
                        locator: root.display().to_string(),
                        message: format!("{error:#}"),
                    }],
                ),
            }
        } else {
            (0, None, Vec::new())
        };
        latest_native_updated_at_ms = latest_native_updated_at_ms.max(latest_updated_at_ms);
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
            latest_updated_at_ms,
            failures,
        });
    }
    let database = doctor_database(path, latest_native_updated_at_ms);
    let watch = doctor_watch(&agents, watch_interval_seconds, watch_debounce_ms);
    let permissions = doctor_permissions(&database, &agents);
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
        && agents.iter().all(|agent| agent.failures.is_empty())
        && database
            .last_ingest
            .as_ref()
            .is_none_or(|status| status.failed == 0)
        && permissions.healthy
        && watch.ready;
    DoctorReport {
        healthy,
        database,
        agents,
        tokenizer,
        permissions,
        watch,
        runtime: DoctorRuntime {
            tracedb_version: env!("CARGO_PKG_VERSION").into(),
            sqlite_version: rusqlite::version().into(),
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
        },
    }
}

fn doctor_database(path: &Path, latest_native_updated_at_ms: Option<i64>) -> DoctorDatabase {
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
            last_ingest: None,
            archive_lag_ms: None,
            backup: DoctorBackup::not_created(),
        };
    }
    match verify_archive(path) {
        Ok(verification) => match doctor_database_metrics(path, latest_native_updated_at_ms) {
            Ok(metrics) => DoctorDatabase {
                path: path.to_path_buf(),
                exists,
                writable,
                verification: Some(verification),
                error: None,
                last_ingest: metrics.last_ingest,
                archive_lag_ms: metrics.archive_lag_ms,
                backup: metrics.backup,
            },
            Err(error) => DoctorDatabase {
                path: path.to_path_buf(),
                exists,
                writable,
                verification: Some(verification),
                error: Some(format!("inspect archive telemetry: {error:#}")),
                last_ingest: None,
                archive_lag_ms: None,
                backup: DoctorBackup::unavailable(),
            },
        },
        Err(error) => DoctorDatabase {
            path: path.to_path_buf(),
            exists,
            writable,
            verification: None,
            error: Some(format!("{error:#}")),
            last_ingest: None,
            archive_lag_ms: None,
            backup: DoctorBackup::unavailable(),
        },
    }
}

struct DoctorDatabaseMetrics {
    last_ingest: Option<DoctorIngestStatus>,
    archive_lag_ms: Option<i64>,
    backup: DoctorBackup,
}

fn doctor_database_metrics(
    path: &Path,
    latest_native_updated_at_ms: Option<i64>,
) -> Result<DoctorDatabaseMetrics> {
    let connection = store::open_for_verification(path)?;
    let last_ingest = store::ingest_status(&connection)?.map(DoctorIngestStatus::from);
    let total_sessions = connection.query_row("SELECT count(*) FROM sessions", [], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(DoctorDatabaseMetrics {
        archive_lag_ms: latest_native_updated_at_ms
            .zip(last_ingest.as_ref().map(|status| status.completed_at_ms))
            .map(|(native, completed)| (native - completed).max(0)),
        last_ingest,
        backup: DoctorBackup::from_counts(total_sessions),
    })
}

fn doctor_permissions(database: &DoctorDatabase, agents: &[DoctorAgent]) -> DoctorPermissions {
    let mut issues = Vec::new();
    if !database.writable {
        issues.push(format!(
            "archive is not writable: {}",
            database.path.display()
        ));
    }
    for agent in agents {
        if agent.exists && !agent.readable {
            issues.push(format!(
                "native root is not readable: {}",
                agent.root.display()
            ));
        }
    }
    DoctorPermissions {
        healthy: issues.is_empty(),
        archive_readable: database.exists && database.error.is_none(),
        archive_writable: database.writable,
        native_readable: agents.iter().all(|agent| !agent.exists || agent.readable),
        issues,
    }
}

fn doctor_watch(agents: &[DoctorAgent], interval_seconds: u64, debounce_ms: u64) -> DoctorWatch {
    let mut issues = Vec::new();
    if interval_seconds == 0 {
        issues.push("watch interval must be greater than zero".into());
    }
    if debounce_ms == 0 {
        issues.push("watch debounce must be greater than zero".into());
    }
    let watcher = notify::recommended_watcher(|_event: notify::Result<notify::Event>| {});
    let mut watcher_available = false;
    match watcher {
        Ok(mut watcher) => {
            for agent in agents {
                if !agent.exists {
                    continue;
                }
                match watcher.watch(&agent.root, notify::RecursiveMode::Recursive) {
                    Ok(()) => watcher_available = true,
                    Err(error) => issues.push(format!("watch {}: {}", agent.root.display(), error)),
                }
            }
        }
        Err(error) => issues.push(format!("create filesystem watcher: {error}")),
    }
    DoctorWatch {
        ready: interval_seconds > 0 && debounce_ms > 0,
        watcher_available,
        fallback_only: !watcher_available,
        interval_seconds,
        debounce_ms,
        issues,
    }
}

/// Options for discovering and ingesting native sessions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestRequest {
    #[serde(default)]
    pub agents: Vec<Agent>,
    pub root: Option<PathBuf>,
    pub since_ms: Option<i64>,
    #[serde(default)]
    pub exclude: Vec<String>,
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
    /// Durable acknowledgement for this ingest run.  This is `None` only for
    /// reports assembled by older callers or in-memory test code.
    pub ack: Option<IngestAck>,
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

/// A durable, monotonic acknowledgement for a completed ingest run.
///
/// The sequence is allocated in SQLite metadata and committed together with
/// the run telemetry.  It is intentionally independent of session timestamps:
/// `endedAtMs` describes the source session, not when TraceDB durably accepted
/// it, and is therefore not a safe ingestion watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestAck {
    pub sequence: u64,
    pub committed_at_ms: i64,
}

/// Machine-readable result of an ingest plan that performs no archive writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestDryRunReport {
    pub dry_run: bool,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStats {
    pub path: PathBuf,
    pub total_sessions: i64,
    pub total_events: i64,
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
    /// When true, match cwd as a normalized exact path instead of a substring.
    #[serde(default)]
    pub cwd_exact: bool,
    /// Hide a child only when its direct lineage parent is also in scope.
    #[serde(default)]
    pub collapse_lineage: bool,
    pub since_ms: Option<i64>,
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
            cwd_exact: false,
            collapse_lineage: false,
            since_ms: None,
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
    pub events: i64,
    pub ingested_at_ms: i64,
    pub fingerprint: String,
    pub status: Option<crate::SessionStatus>,
    /// Direct session lineage parent, if present.
    pub parent_session_id: Option<String>,
    /// Relationship to `parent_session_id` (`parent` or `fork`), when known.
    pub parent_relation: Option<String>,
    /// Number of sessions that directly reference this session as a parent.
    pub subagent_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCoverage {
    pub id: String,
    pub fingerprint: String,
    pub ingested_at_ms: i64,
    pub events: i64,
    pub sources: i64,
    pub latest_source_mtime_ns: Option<i64>,
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
    pub last_ingest: Option<DoctorIngestStatus>,
    pub archive_lag_ms: Option<i64>,
    pub backup: DoctorBackup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorAgent {
    pub agent: Agent,
    pub root: PathBuf,
    pub exists: bool,
    pub readable: bool,
    pub discovered: usize,
    pub latest_updated_at_ms: Option<i64>,
    pub failures: Vec<DoctorFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorIngestStatus {
    pub ack_sequence: u64,
    pub successful: bool,
    pub completed_at_ms: i64,
    pub discovered: usize,
    pub ingested: usize,
    pub skipped: usize,
    pub failed: usize,
    pub cumulative_failed: usize,
}

impl From<store::StoredIngestStatus> for DoctorIngestStatus {
    fn from(status: store::StoredIngestStatus) -> Self {
        Self {
            ack_sequence: status.ack_sequence,
            successful: status.failed == 0,
            completed_at_ms: status.completed_at_ms,
            discovered: status.discovered,
            ingested: status.ingested,
            skipped: status.skipped,
            failed: status.failed,
            cumulative_failed: status.cumulative_failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorBackup {
    pub recommended: bool,
    pub total_sessions: i64,
    pub reason: String,
}

impl DoctorBackup {
    fn not_created() -> Self {
        Self {
            recommended: false,
            total_sessions: 0,
            reason: "archive has not been created".into(),
        }
    }

    fn unavailable() -> Self {
        Self {
            recommended: true,
            total_sessions: 0,
            reason: "archive could not be inspected for backup guidance".into(),
        }
    }

    fn from_counts(total_sessions: i64) -> Self {
        let reason = if total_sessions > 0 {
            "native snapshots are present; back up the archive to preserve exact reconstruction"
        } else {
            "archive contains no sessions yet"
        };
        Self {
            recommended: total_sessions > 0,
            total_sessions,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorPermissions {
    pub healthy: bool,
    pub archive_readable: bool,
    pub archive_writable: bool,
    pub native_readable: bool,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorWatch {
    pub ready: bool,
    pub watcher_available: bool,
    pub fallback_only: bool,
    pub interval_seconds: u64,
    pub debounce_ms: u64,
    pub issues: Vec<String>,
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
    pub permissions: DoctorPermissions,
    pub watch: DoctorWatch,
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
    pub events: Vec<crate::model::Event>,
    pub spans: Vec<crate::model::Span>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupReport {
    pub path: PathBuf,
    pub bytes: u64,
    pub sessions: u64,
    pub events: u64,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GcReport {
    pub dry_run: bool,
    pub total_objects: u64,
    pub referenced_objects: u64,
    pub orphan_objects: u64,
    pub orphan_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub source: PathBuf,
    pub imported_sessions: u64,
    pub imported_events: u64,
    pub imported_objects: u64,
    pub skipped_sessions: u64,
    pub skipped_events: u64,
}

pub const RESTORE_MANIFEST_SCHEMA_VERSION: &str = "tracedb-restore-manifest-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreManifest {
    pub schema_version: String,
    pub session_id: String,
    pub output_dir: PathBuf,
    pub files: Vec<RestoreManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreManifestFile {
    pub path: PathBuf,
    pub locator: String,
    pub object_hash: String,
    pub bytes: u64,
    pub mode: Option<u32>,
    pub mtime_ns: Option<i64>,
}

fn adaptive_periodic_interval(
    base: Duration,
    current: Duration,
    report: &IngestReport,
) -> Duration {
    if report.total_parsed() == 0 && report.total_ingested() == 0 && report.total_failed() == 0 {
        current.saturating_mul(2).min(base.saturating_mul(4))
    } else {
        base
    }
}

const PARALLEL_PARSE_THRESHOLD: usize = 8;

fn parse_pending(
    agent: Agent,
    pending: &[SessionCandidate],
    root: &Path,
) -> Vec<(SessionCandidate, Result<Option<ParsedSession>>)> {
    if pending.len() < PARALLEL_PARSE_THRESHOLD {
        return parser(agent).parse_many(pending, root);
    }
    let root = root.to_path_buf();
    let workers = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .min(pending.len());
    let chunk_size = pending.len().div_ceil(workers);
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        for (chunk_index, chunk) in pending.chunks(chunk_size).enumerate() {
            let batch = chunk.to_vec();
            let root = root.clone();
            let sender = sender.clone();
            scope.spawn(move || {
                let parser = parser(agent);
                for (offset, candidate) in batch.into_iter().enumerate() {
                    let parsed = parser.parse(&candidate, &root);
                    sender
                        .send((chunk_index * chunk_size + offset, candidate, parsed))
                        .expect("parse worker result receiver");
                }
            });
        }
    });
    drop(sender);
    let mut results = pending
        .iter()
        .map(|_| None)
        .collect::<Vec<Option<(SessionCandidate, Result<Option<ParsedSession>>)>>>();
    for (index, candidate, parsed) in receiver {
        results[index] = Some((candidate, parsed));
    }
    results
        .into_iter()
        .map(|result| result.expect("parse worker returned every candidate"))
        .collect()
}

fn agents_for_watch_paths(
    paths: &[PathBuf],
    configured: &[Agent],
    configured_root: Option<&Path>,
) -> Vec<Agent> {
    let agents = if configured.is_empty() {
        Agent::ALL.to_vec()
    } else {
        configured.to_vec()
    };
    if paths.is_empty() {
        return agents;
    }
    let mut scoped = Vec::new();
    for agent in agents {
        let root = configured_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| native_root(agent));
        if paths
            .iter()
            .any(|path| path.starts_with(&root) || root.starts_with(path))
        {
            scoped.push(agent);
        }
    }
    scoped
}

/// Resolve the default native store root for one supported agent.
pub fn native_root(agent: Agent) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    match agent {
        Agent::Claude => home.join(".claude/projects"),
        Agent::Codex => home.join(".codex/sessions"),
        Agent::OpenCode => home.join(".local/share/opencode"),
        Agent::Gemini => home.join(".gemini/tmp"),
        Agent::Pi => home.join(".pi/agent/sessions"),
    }
}
