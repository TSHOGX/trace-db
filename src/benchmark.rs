//! Deterministic end-to-end performance benchmarks for TraceDB.

use crate::{Agent, IngestMode, IngestReport, IngestRequest, ListRequest, SearchRequest, TraceDb};
use anyhow::{bail, Context, Result};
use chrono::{SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};

pub const BENCHMARK_SCHEMA_VERSION: &str = "tracedb-benchmark-v3";
pub const STANDARD_SESSION_COUNTS: [usize; 3] = [1_000, 10_000, 100_000];
pub const EVENTS_PER_SESSION: usize = 6;
pub const SEARCH_REPETITIONS_PER_QUERY: usize = 20;
const GENERATOR_VERSION: u32 = 1;
const CHANGE_DIVISOR: usize = 100;
const BASE_TIMESTAMP_SECONDS: i64 = 1_767_225_600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkConfig {
    pub workspace: PathBuf,
    pub session_counts: Vec<usize>,
}

impl BenchmarkConfig {
    pub fn standard(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            session_counts: STANDARD_SESSION_COUNTS.to_vec(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.session_counts.is_empty() {
            bail!("benchmark requires at least one session count");
        }
        if self.session_counts.contains(&0) {
            bail!("benchmark session counts must be greater than zero");
        }
        let mut unique = self.session_counts.clone();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() != self.session_counts.len() {
            bail!("benchmark session counts must be unique");
        }
        if self.workspace.exists() {
            bail!(
                "benchmark workspace already exists: {}",
                self.workspace.display()
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkHost {
    pub os: String,
    pub architecture: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkSuiteReport {
    pub schema_version: String,
    pub trace_db_version: String,
    pub generator_version: u32,
    pub events_per_session: usize,
    pub search_repetitions_per_query: usize,
    pub changed_fraction: String,
    pub host: BenchmarkHost,
    pub runs: Vec<BenchmarkRunReport>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkRunReport {
    pub sessions: usize,
    pub changed_sessions: usize,
    pub native_bytes: u64,
    pub operations: Vec<BenchmarkOperation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkOperation {
    pub name: BenchmarkOperationName,
    pub metrics: BenchmarkMetrics,
    pub result: BenchmarkResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkOperationName {
    Generate,
    FirstIngest,
    UnchangedIngest,
    ChangedIngest,
    Search,
    List,
    Show,
    Stats,
    Reindex,
    Verify,
    Reconstruct,
}

impl std::fmt::Display for BenchmarkOperationName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Generate => "generate",
            Self::FirstIngest => "first_ingest",
            Self::UnchangedIngest => "unchanged_ingest",
            Self::ChangedIngest => "changed_ingest",
            Self::Search => "search",
            Self::List => "list",
            Self::Show => "show",
            Self::Stats => "stats",
            Self::Reindex => "reindex",
            Self::Verify => "verify",
            Self::Reconstruct => "reconstruct",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkMetrics {
    pub wall_time_ns: u64,
    /// Nearest-rank p95 of inner samples for operations that expose them.
    pub p95_wall_time_ns: Option<u64>,
    pub cpu_time_ns: Option<u64>,
    pub process_peak_rss_bytes: Option<u64>,
    pub database_bytes: u64,
    pub physical_write_bytes: Option<u64>,
    pub logical_source_bytes: u64,
    pub write_amplification: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum BenchmarkResult {
    Generated {
        files: usize,
        bytes: u64,
    },
    Ingested {
        mode: IngestMode,
        discovered: usize,
        parsed: usize,
        ingested: usize,
        unchanged: usize,
        failed: usize,
    },
    Searched {
        queries: usize,
        results: usize,
    },
    Listed {
        sessions: usize,
        has_more: bool,
    },
    Shown {
        events: usize,
    },
    Stats {
        sessions: i64,
        events: i64,
        full_sessions: i64,
    },
    Reindexed {
        events: i64,
    },
    Verified {
        checks: usize,
        checked: usize,
        passed: bool,
        failures: usize,
    },
    Reconstructed {
        files: usize,
        bytes: u64,
    },
}

/// Generate deterministic Codex-native JSONL fixtures. Existing roots are rejected.
pub fn generate_codex_dataset(root: &Path, sessions: usize) -> Result<u64> {
    if sessions == 0 {
        bail!("benchmark session count must be greater than zero");
    }
    if root.exists() {
        bail!("benchmark dataset already exists: {}", root.display());
    }
    fs::create_dir_all(root)?;
    let mut total = 0_u64;
    for index in 0..sessions {
        let path = session_path(root, index);
        let content = session_contents(index, false)?;
        let mut writer = BufWriter::new(File::create(&path)?);
        writer.write_all(content.as_bytes())?;
        writer.flush()?;
        total = total.saturating_add(content.len() as u64);
    }
    Ok(total)
}

/// Run every required lifecycle operation for each configured dataset size.
pub fn run_benchmarks(config: &BenchmarkConfig) -> Result<BenchmarkSuiteReport> {
    config.validate()?;
    fs::create_dir(&config.workspace)?;
    let runs = config
        .session_counts
        .iter()
        .map(|&n| run_one(&config.workspace, n))
        .collect::<Result<Vec<_>>>()?;
    Ok(BenchmarkSuiteReport {
        schema_version: BENCHMARK_SCHEMA_VERSION.into(),
        trace_db_version: env!("CARGO_PKG_VERSION").into(),
        generator_version: GENERATOR_VERSION,
        events_per_session: EVENTS_PER_SESSION,
        search_repetitions_per_query: SEARCH_REPETITIONS_PER_QUERY,
        changed_fraction: format!("1/{CHANGE_DIVISOR}"),
        host: BenchmarkHost {
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
        },
        runs,
    })
}

fn run_one(workspace: &Path, sessions: usize) -> Result<BenchmarkRunReport> {
    let root = workspace.join(format!("sessions-{sessions}"));
    fs::create_dir(&root)?;
    let native = root.join("native");
    let db_path = root.join("trace.db");
    let restore = root.join("reconstructed");
    let mut operations = Vec::with_capacity(11);
    let (native_bytes, op) = measure(BenchmarkOperationName::Generate, &db_path, 0, || {
        let bytes = generate_codex_dataset(&native, sessions)?;
        Ok((
            bytes,
            BenchmarkResult::Generated {
                files: sessions,
                bytes,
            },
        ))
    })?;
    operations.push(op);
    let request = |mode| IngestRequest {
        agents: vec![Agent::Codex],
        mode,
        root: Some(native.clone()),
        since_ms: None,
        exclude: Vec::new(),
    };
    let (mut archive, op) = measure(
        BenchmarkOperationName::FirstIngest,
        &db_path,
        native_bytes,
        || {
            let mut db = TraceDb::open(&db_path)?;
            let report = db.ingest(request(IngestMode::Full))?;
            require_ingest(&report, sessions, sessions, sessions, 0)?;
            Ok((db, ingest_result(IngestMode::Full, &report)))
        },
    )?;
    operations.push(op);
    let (_, op) = measure(BenchmarkOperationName::UnchangedIngest, &db_path, 0, || {
        let report = archive.ingest(request(IngestMode::Full))?;
        require_ingest(&report, sessions, 0, 0, sessions)?;
        Ok(((), ingest_result(IngestMode::Full, &report)))
    })?;
    operations.push(op);
    let changed_sessions = sessions.div_ceil(CHANGE_DIVISOR);
    let changed_bytes = change_sessions(&native, changed_sessions)?;
    let (_, op) = measure(
        BenchmarkOperationName::ChangedIngest,
        &db_path,
        changed_bytes,
        || {
            let report = archive.ingest(request(IngestMode::Full))?;
            require_ingest(
                &report,
                sessions,
                changed_sessions,
                changed_sessions,
                sessions - changed_sessions,
            )?;
            Ok(((), ingest_result(IngestMode::Full, &report)))
        },
    )?;
    operations.push(op);
    let (search_p95, mut op) = measure(BenchmarkOperationName::Search, &db_path, 0, || {
        const QUERIES: [&str; 3] = [
            "deployment benchmark",
            "sqlite migration",
            "parser regression",
        ];
        let mut results = 0;
        let mut samples = Vec::new();
        for query in QUERIES {
            results += archive.search(SearchRequest::new(query))?.len();
        }
        for _ in 0..SEARCH_REPETITIONS_PER_QUERY {
            for query in QUERIES {
                let started = Instant::now();
                archive.search(SearchRequest::new(query))?;
                samples.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
            }
        }
        if results == 0 {
            bail!("benchmark search returned no results");
        }
        let p95 = nearest_rank_p95(&mut samples).context("benchmark search produced no samples")?;
        Ok((
            p95,
            BenchmarkResult::Searched {
                queries: QUERIES.len(),
                results,
            },
        ))
    })?;
    op.metrics.p95_wall_time_ns = Some(search_p95);
    operations.push(op);
    let (_, op) = measure(BenchmarkOperationName::List, &db_path, 0, || {
        let page = archive.list(ListRequest {
            limit: 100,
            ..ListRequest::default()
        })?;
        Ok((
            (),
            BenchmarkResult::Listed {
                sessions: page.sessions.len(),
                has_more: page.next_cursor.is_some(),
            },
        ))
    })?;
    operations.push(op);
    let selected = benchmark_session_id(sessions / 2);
    let (_, op) = measure(BenchmarkOperationName::Show, &db_path, 0, || {
        let trace = archive
            .show(&selected)?
            .with_context(|| format!("missing benchmark session {selected}"))?;
        Ok((
            (),
            BenchmarkResult::Shown {
                events: trace.events.len(),
            },
        ))
    })?;
    operations.push(op);
    let (stats, op) = measure(BenchmarkOperationName::Stats, &db_path, 0, || {
        let stats = archive.stats()?;
        if stats.total_sessions != sessions as i64 || stats.total_full_sessions != sessions as i64 {
            bail!("benchmark stats do not match dataset");
        }
        let total_sessions = stats.total_sessions;
        let total_events = stats.total_events;
        let total_full_sessions = stats.total_full_sessions;
        Ok((
            stats,
            BenchmarkResult::Stats {
                sessions: total_sessions,
                events: total_events,
                full_sessions: total_full_sessions,
            },
        ))
    })?;
    operations.push(op);
    let (_, op) = measure(BenchmarkOperationName::Reindex, &db_path, 0, || {
        archive.reindex()?;
        Ok((
            (),
            BenchmarkResult::Reindexed {
                events: stats.total_events,
            },
        ))
    })?;
    operations.push(op);
    let (_, op) = measure(BenchmarkOperationName::Verify, &db_path, 0, || {
        let report = archive.verify()?;
        let checked = report.checks.iter().map(|c| c.checked).sum();
        let failures = report.failure_count();
        if !report.passed {
            bail!("benchmark verification failed with {failures} failure(s)");
        }
        Ok((
            (),
            BenchmarkResult::Verified {
                checks: report.checks.len(),
                checked,
                passed: report.passed,
                failures,
            },
        ))
    })?;
    operations.push(op);
    let (_, op) = measure(BenchmarkOperationName::Reconstruct, &db_path, 0, || {
        let files = archive.reconstruct(&selected, &restore)?;
        let bytes = files.iter().try_fold(0_u64, |sum, p| {
            Ok::<_, std::io::Error>(sum.saturating_add(fs::metadata(p)?.len()))
        })?;
        Ok((
            (),
            BenchmarkResult::Reconstructed {
                files: files.len(),
                bytes,
            },
        ))
    })?;
    operations.push(op);
    Ok(BenchmarkRunReport {
        sessions,
        changed_sessions,
        native_bytes,
        operations,
    })
}

fn measure<T>(
    name: BenchmarkOperationName,
    db_path: &Path,
    logical: u64,
    operation: impl FnOnce() -> Result<(T, BenchmarkResult)>,
) -> Result<(T, BenchmarkOperation)> {
    let before = ResourceSnapshot::capture();
    let started = Instant::now();
    let (value, result) = operation().with_context(|| format!("benchmark operation {name}"))?;
    let after = ResourceSnapshot::capture();
    let physical = delta(after.write_bytes, before.write_bytes);
    let metrics = BenchmarkMetrics {
        wall_time_ns: u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
        p95_wall_time_ns: None,
        cpu_time_ns: delta(after.cpu_time_ns, before.cpu_time_ns),
        process_peak_rss_bytes: after.peak_rss_bytes,
        database_bytes: database_bytes(db_path)?,
        physical_write_bytes: physical,
        logical_source_bytes: logical,
        write_amplification: physical
            .and_then(|n| (logical > 0).then_some(n as f64 / logical as f64)),
    };
    Ok((
        value,
        BenchmarkOperation {
            name,
            metrics,
            result,
        },
    ))
}

fn nearest_rank_p95(samples: &mut [u64]) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let rank = (samples.len() * 95).div_ceil(100);
    samples.get(rank.saturating_sub(1)).copied()
}

fn ingest_result(mode: IngestMode, r: &IngestReport) -> BenchmarkResult {
    BenchmarkResult::Ingested {
        mode,
        discovered: r.total_discovered(),
        parsed: r.total_parsed(),
        ingested: r.total_ingested(),
        unchanged: r.total_unchanged(),
        failed: r.total_failed(),
    }
}
fn require_ingest(
    r: &IngestReport,
    discovered: usize,
    parsed: usize,
    ingested: usize,
    unchanged: usize,
) -> Result<()> {
    if r.total_discovered() != discovered
        || r.total_parsed() != parsed
        || r.total_ingested() != ingested
        || r.total_unchanged() != unchanged
        || r.total_failed() != 0
    {
        bail!("unexpected ingest result: discovered={}, parsed={}, ingested={}, unchanged={}, failed={}", r.total_discovered(), r.total_parsed(), r.total_ingested(), r.total_unchanged(), r.total_failed());
    }
    Ok(())
}

fn session_contents(index: usize, changed: bool) -> Result<String> {
    let id = benchmark_native_id(index);
    let topic = ["deployment", "sqlite", "parser", "reconstruction"][index % 4];
    let mut lines = Vec::new();
    let record = |typ: &str, ts: i64, payload: serde_json::Value| -> Result<_> {
        Ok(json!({"type": typ, "timestamp": timestamp(index, ts)?, "payload": payload}))
    };
    lines.push(record("session_meta", 0, json!({"id": id, "cwd": format!("/workspace/project-{:03}", index % 100), "model_provider": "openai", "git": {"branch": "benchmark"}}))?);
    lines.push(record(
        "turn_context",
        1,
        json!({"model": "gpt-5-benchmark"}),
    )?);
    lines.push(record("response_item", 2, json!({"type": "message", "id": format!("user-{index:06}"), "role": "user", "content": format!("deterministic {topic} benchmark request {index:06}")}))?);
    lines.push(record("response_item", 3, json!({"type": "reasoning", "id": format!("reason-{index:06}"), "summary": [{"type": "summary_text", "text": format!("inspect {topic} regression fixture")}]}))?);
    lines.push(record("response_item", 4, json!({"type": "function_call", "id": format!("call-{index:06}"), "call_id": format!("call-{index:06}"), "name": "exec_command", "arguments": format!("{{\"cmd\":\"cargo test {topic}\"}}")}))?);
    lines.push(record("response_item", 5, json!({"type": "function_call_output", "id": format!("result-{index:06}"), "call_id": format!("call-{index:06}"), "output": format!("tests passed for fixture {index:06}")}))?);
    lines.push(record("response_item", 6, json!({"type": "message", "id": format!("assistant-{index:06}"), "role": "assistant", "content": [{"type": "output_text", "text": format!("completed {topic} benchmark outcome {index:06}")}]}))?);
    lines.push(record("event_msg", 7, json!({"type": "token_count", "info": {"last_token_usage": {"total_tokens": 1_000 + index % 10_000}}}))?);
    if changed {
        lines.push(record("response_item", 8, json!({"type": "message", "id": format!("changed-{index:06}"), "role": "assistant", "content": format!("deterministic changed ingest marker {index:06}")}))?);
    }
    Ok(lines
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

fn change_sessions(root: &Path, count: usize) -> Result<u64> {
    let mut bytes = 0;
    for index in 0..count {
        let content = session_contents(index, true)?;
        fs::write(session_path(root, index), content.as_bytes())?;
        bytes += content.len() as u64;
    }
    Ok(bytes)
}
fn timestamp(index: usize, offset: i64) -> Result<String> {
    let seconds = BASE_TIMESTAMP_SECONDS
        .checked_add(index as i64 * 60)
        .and_then(|v| v.checked_add(offset))
        .context("benchmark timestamp overflow")?;
    Ok(Utc
        .timestamp_opt(seconds, 0)
        .single()
        .context("benchmark timestamp out of range")?
        .to_rfc3339_opts(SecondsFormat::Secs, true))
}
fn benchmark_native_id(index: usize) -> String {
    format!("bench-{index:06}")
}
fn benchmark_session_id(index: usize) -> String {
    format!("codex:{}", benchmark_native_id(index))
}
fn session_path(root: &Path, index: usize) -> PathBuf {
    root.join(format!("rollout-bench-{index:06}.jsonl"))
}
fn database_bytes(path: &Path) -> Result<u64> {
    Ok([
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ]
    .into_iter()
    .try_fold(0, |sum, p| match fs::metadata(p) {
        Ok(m) => Ok(sum + m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(sum),
        Err(e) => Err(e),
    })?)
}
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    value.into()
}
fn delta(after: Option<u64>, before: Option<u64>) -> Option<u64> {
    after.zip(before).map(|(a, b)| a.saturating_sub(b))
}

#[derive(Debug, Clone, Copy, Default)]
struct ResourceSnapshot {
    cpu_time_ns: Option<u64>,
    peak_rss_bytes: Option<u64>,
    write_bytes: Option<u64>,
}
impl ResourceSnapshot {
    fn capture() -> Self {
        platform_snapshot()
    }
}

#[cfg(target_os = "linux")]
fn platform_snapshot() -> ResourceSnapshot {
    let (cpu_time_ns, peak_rss_bytes) = unix_rusage();
    let write_bytes = fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find_map(|line| line.strip_prefix("write_bytes:")?.trim().parse().ok())
        });
    ResourceSnapshot {
        cpu_time_ns,
        peak_rss_bytes,
        write_bytes,
    }
}
#[cfg(all(unix, not(target_os = "macos")))]
fn unix_rusage() -> (Option<u64>, Option<u64>) {
    let mut raw = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, raw.as_mut_ptr()) } != 0 {
        return (None, None);
    }
    let r = unsafe { raw.assume_init() };
    let t = |v: libc::timeval| {
        u64::try_from(v.tv_sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(v.tv_usec).unwrap_or(0).saturating_mul(1_000))
    };
    (
        Some(t(r.ru_utime).saturating_add(t(r.ru_stime))),
        u64::try_from(r.ru_maxrss).ok().map(|v| v * 1_024),
    )
}
#[cfg(target_os = "macos")]
fn platform_snapshot() -> ResourceSnapshot {
    let mut raw = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: proc_pid_rusage initializes the correctly sized buffer on success.
    let status = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V4,
            raw.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return ResourceSnapshot::default();
    }
    // SAFETY: the call above succeeded and initialized the buffer.
    let usage = unsafe { raw.assume_init() };
    ResourceSnapshot {
        cpu_time_ns: Some(usage.ri_user_time.saturating_add(usage.ri_system_time)),
        peak_rss_bytes: Some(usage.ri_lifetime_max_phys_footprint),
        write_bytes: Some(usage.ri_diskio_byteswritten),
    }
}
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn platform_snapshot() -> ResourceSnapshot {
    let (cpu, rss) = unix_rusage();
    ResourceSnapshot {
        cpu_time_ns: cpu,
        peak_rss_bytes: rss,
        write_bytes: None,
    }
}
#[cfg(windows)]
fn platform_snapshot() -> ResourceSnapshot {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::{GetCurrentProcess, GetProcessIoCounters, GetProcessTimes, IO_COUNTERS},
        },
    };
    // SAFETY: this is a valid pseudo-handle for the current process.
    let process = unsafe { GetCurrentProcess() };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all pointers refer to writable FILETIME values.
    let times_ok =
        unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } != 0;
    let cpu_time_ns = times_ok.then(|| {
        filetime_ticks(kernel)
            .saturating_add(filetime_ticks(user))
            .saturating_mul(100)
    });
    let mut memory = PROCESS_MEMORY_COUNTERS::default();
    // SAFETY: memory points to a correctly sized writable structure.
    let memory_ok = unsafe {
        GetProcessMemoryInfo(
            process,
            &mut memory,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    } != 0;
    let mut io = IO_COUNTERS::default();
    // SAFETY: io points to a writable IO_COUNTERS value.
    let io_ok = unsafe { GetProcessIoCounters(process, &mut io) } != 0;
    ResourceSnapshot {
        cpu_time_ns,
        peak_rss_bytes: memory_ok.then_some(memory.PeakWorkingSetSize as u64),
        write_bytes: io_ok.then_some(io.WriteTransferCount),
    }
}
#[cfg(windows)]
fn filetime_ticks(value: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}
#[cfg(not(any(unix, windows)))]
fn platform_snapshot() -> ResourceSnapshot {
    ResourceSnapshot::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    #[test]
    fn generator_is_deterministic() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ar = a.path().join("d");
        let br = b.path().join("d");
        assert_eq!(
            generate_codex_dataset(&ar, 3).unwrap(),
            generate_codex_dataset(&br, 3).unwrap()
        );
        for i in 0..3 {
            assert_eq!(
                Sha256::digest(fs::read(session_path(&ar, i)).unwrap()),
                Sha256::digest(fs::read(session_path(&br, i)).unwrap())
            );
        }
    }
    #[test]
    fn rejects_existing_workspace() {
        let d = tempfile::tempdir().unwrap();
        assert!(run_benchmarks(&BenchmarkConfig {
            workspace: d.path().into(),
            session_counts: vec![1]
        })
        .is_err());
    }
}
