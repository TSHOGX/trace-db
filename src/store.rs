use crate::{
    config::TokenizerKind,
    model::{
        assign_indexes, derive_spans, Event, NativeSource, ParsedSession, Session,
        SessionAggregates, Span, TokenUsage,
    },
    IngestAck, IngestReport, ListPage, ListRequest, ReconstructionOptions, SessionCoverage,
    SessionSummary, SessionTrace,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SCHEMA_VERSION: i64 = 8;
pub const PORTABLE_TOKENIZER: &str = "unicode61 remove_diacritics 2";
pub const JIEBA_TOKENIZER: &str = "jieba";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateState {
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredIngestStatus {
    pub ack_sequence: u64,
    pub completed_at_ms: i64,
    pub discovered: usize,
    pub ingested: usize,
    pub skipped: usize,
    pub failed: usize,
    pub cumulative_failed: usize,
}

pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    let conn = open_connection(path.as_ref())?;
    let jieba = if let Some(ext) = std::env::var_os("TRACEDB_JIEBA_EXT") {
        let loaded = unsafe {
            conn.load_extension_enable()
                .and_then(|_| {
                    conn.load_extension(PathBuf::from(ext), Some("sqlite3_fts5jieba_init"))
                })
                .and_then(|result| conn.load_extension_disable().map(|_| result))
        };
        loaded.is_ok()
    } else {
        false
    };
    migrate_with_tokenizer(&conn, jieba)?;
    Ok(conn)
}

pub fn open_configured(
    path: &Path,
    tokenizer: TokenizerKind,
    tokenizer_extension: Option<&Path>,
) -> Result<Connection> {
    let connection = open_connection(path)?;
    let jieba = match tokenizer {
        TokenizerKind::Unicode61 => false,
        TokenizerKind::Jieba => {
            let extension = tokenizer_extension
                .context("jieba tokenizer requires a configured extension path")?;
            unsafe {
                connection.load_extension_enable()?;
                let load_result =
                    connection.load_extension(extension, Some("sqlite3_fts5jieba_init"));
                connection.load_extension_disable()?;
                load_result.with_context(|| {
                    format!("load jieba tokenizer extension {}", extension.display())
                })?;
            }
            true
        }
    };
    migrate_with_tokenizer(&connection, jieba)?;
    Ok(connection)
}

fn open_connection(path: &Path) -> Result<Connection> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000i64)?;
    crate::search::register_term_coverage(&conn)?;
    Ok(conn)
}

pub fn open_read_only(path: &Path) -> Result<Connection> {
    if !path.exists() {
        anyhow::bail!("TraceDB archive does not exist: {}", path.display());
    }
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "busy_timeout", 5000i64)?;
    crate::search::register_term_coverage(&connection)?;
    Ok(connection)
}

pub fn open_read_only_configured(
    path: &Path,
    tokenizer: TokenizerKind,
    tokenizer_extension: Option<&Path>,
) -> Result<Connection> {
    let connection = open_read_only(path)?;
    if matches!(tokenizer, TokenizerKind::Jieba) {
        let extension =
            tokenizer_extension.context("jieba tokenizer requires a configured extension path")?;
        unsafe {
            connection.load_extension_enable()?;
            let load_result = connection.load_extension(extension, Some("sqlite3_fts5jieba_init"));
            connection.load_extension_disable()?;
            load_result.with_context(|| {
                format!("load jieba tokenizer extension {}", extension.display())
            })?;
        }
    }
    Ok(connection)
}

pub fn backup(connection: &Connection, destination: &Path) -> Result<crate::BackupReport> {
    if destination.as_os_str().is_empty() {
        anyhow::bail!("backup destination must not be empty");
    }
    if destination.exists() {
        anyhow::bail!(
            "backup destination already exists: {}",
            destination.display()
        );
    }
    if let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let staging = tempfile::tempdir_in(parent)?;
    let staged_path = staging.path().join("archive.db");
    connection.execute("VACUUM INTO ?1", [staged_path.to_string_lossy().as_ref()])?;
    fs::rename(&staged_path, destination)
        .with_context(|| format!("publish verified archive backup {}", destination.display()))?;
    let snapshot = open_read_only(destination)?;
    let sessions = snapshot.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))?;
    let events = snapshot.query_row("SELECT count(*) FROM events", [], |row| row.get(0))?;
    verify(&snapshot, destination)?;
    let bytes = fs::metadata(destination)?.len();
    Ok(crate::BackupReport {
        path: destination.to_path_buf(),
        bytes,
        sessions,
        events,
        verified: true,
    })
}

pub fn gc_report(connection: &Connection, dry_run: bool) -> Result<crate::GcReport> {
    if !dry_run {
        anyhow::bail!("gc is non-destructive by default; pass --dry-run");
    }
    let total_objects = connection.query_row("SELECT count(*) FROM objects", [], |row| {
        row.get::<_, u64>(0)
    })?;
    let referenced_objects = connection.query_row(
        "SELECT count(DISTINCT object_hash) FROM raw_sources WHERE object_hash IS NOT NULL",
        [],
        |row| row.get::<_, u64>(0),
    )?;
    let (orphan_objects, orphan_bytes) = connection.query_row(
        "SELECT count(*), COALESCE(sum(length(o.payload)), 0)
         FROM objects o
         LEFT JOIN (SELECT DISTINCT object_hash FROM raw_sources WHERE object_hash IS NOT NULL) r
           ON r.object_hash=o.hash
         WHERE r.object_hash IS NULL",
        [],
        |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
    )?;
    Ok(crate::GcReport {
        dry_run: true,
        total_objects,
        referenced_objects,
        orphan_objects,
        orphan_bytes,
    })
}

pub fn import_archive(connection: &mut Connection, source: &Path) -> Result<crate::ImportReport> {
    if !source.exists() {
        anyhow::bail!("import source does not exist: {}", source.display());
    }
    let destination = connection
        .query_row("PRAGMA database_list", [], |row| row.get::<_, String>(2))
        .unwrap_or_default();
    if !destination.is_empty()
        && fs::canonicalize(source).ok() == fs::canonicalize(&destination).ok()
    {
        anyhow::bail!("cannot import an archive into itself: {}", source.display());
    }
    let source_connection = open_read_only(source)?;
    let verification = verify(&source_connection, source)?;
    if verification.failure_count() != 0 {
        anyhow::bail!(
            "import source failed verification with {} failure(s)",
            verification.failure_count()
        );
    }
    drop(source_connection);

    connection.execute(
        "ATTACH DATABASE ?1 AS import_source",
        [source.to_string_lossy().as_ref()],
    )?;
    let source_version = schema_version(connection, "import_source")?;
    let result = (|| -> Result<crate::ImportReport> {
        if source_version != Some(SCHEMA_VERSION) {
            anyhow::bail!(
                "import source has schema version {} but this build requires exactly {SCHEMA_VERSION}; \
                 re-ingest the source archive with a matching TraceDB build before importing",
                source_version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            );
        }
        connection.execute_batch("BEGIN IMMEDIATE")?;
        validate_import_compatibility(connection)?;
        let imported_sessions = connection.execute(
            "INSERT OR IGNORE INTO sessions(id,agent,cwd,started_at_ms,ended_at_ms,status,title,model,provider,git_branch,parent_session_id,parent_relation,fork_point_native_id,fingerprint,meta_json,ingested_at_ms)
             SELECT id,agent,cwd,started_at_ms,ended_at_ms,status,title,model,provider,git_branch,parent_session_id,parent_relation,fork_point_native_id,fingerprint,meta_json,ingested_at_ms
             FROM import_source.sessions",
            [],
        )? as u64;
        let source_sessions =
            connection.query_row("SELECT count(*) FROM import_source.sessions", [], |row| {
                row.get::<_, u64>(0)
            })?;
        let imported_objects = connection.execute(
            "INSERT OR IGNORE INTO objects(hash,compression,bytes,payload,created_at_ms)
             SELECT hash,compression,bytes,payload,created_at_ms FROM import_source.objects",
            [],
        )? as u64;
        let imported_events = connection.execute(
            "INSERT INTO events(session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,parent_kind,span_id,model,provider,usage_json,text,data_json,created_at_ms,ended_at_ms)
             SELECT ie.session_id,ie.idx,ie.kind,ie.subtype,ie.role,ie.name,ie.call_id,ie.is_error,ie.native_id,ie.parent_id,ie.parent_kind,ie.span_id,ie.model,ie.provider,ie.usage_json,ie.text,ie.data_json,ie.created_at_ms,ie.ended_at_ms
             FROM import_source.events ie
             WHERE NOT EXISTS (
               SELECT 1 FROM events e
               WHERE e.session_id=ie.session_id AND e.idx=ie.idx
                 AND COALESCE(e.native_id,'')=COALESCE(ie.native_id,'')
             )",
            [],
        )? as u64;
        connection.execute(
            "INSERT OR IGNORE INTO spans(session_id,id,parent_span_id,kind,name,native_id,call_id,status,started_at_ms,ended_at_ms,start_event_idx,end_event_idx,data_json)
             SELECT session_id,id,parent_span_id,kind,name,native_id,call_id,status,started_at_ms,ended_at_ms,start_event_idx,end_event_idx,data_json
             FROM import_source.spans",
            [],
        )?;
        let source_events =
            connection.query_row("SELECT count(*) FROM import_source.events", [], |row| {
                row.get::<_, u64>(0)
            })?;
        connection.execute(
            "INSERT INTO raw_sources(session_id,locator,kind,restore_path,role,bytes,mtime_ns,mode,object_hash)
             SELECT session_id,locator,kind,restore_path,role,bytes,mtime_ns,mode,object_hash
             FROM import_source.raw_sources WHERE true
             ON CONFLICT(session_id,locator) DO UPDATE SET
               object_hash=COALESCE(raw_sources.object_hash,excluded.object_hash)",
            [],
        )?;
        // Imported rows merge into an existing archive, so child_count and the
        // event totals are properties of the union, not of either input.
        // Recomputing states that invariant instead of trusting copied columns.
        connection.execute(&format!("UPDATE sessions SET {AGGREGATE_PROJECTION}"), [])?;
        connection.execute_batch("COMMIT")?;
        truncate_stored_previews(connection)?;
        Ok(crate::ImportReport {
            source: source.to_path_buf(),
            imported_sessions,
            imported_events,
            imported_objects,
            skipped_sessions: source_sessions.saturating_sub(imported_sessions),
            skipped_events: source_events.saturating_sub(imported_events),
        })
    })();
    if result.is_err() {
        let _ = connection.execute_batch("ROLLBACK");
    }
    let detach_result = connection.execute_batch("DETACH DATABASE import_source");
    match (result, detach_result) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(anyhow::Error::from(error)),
    }
}

fn validate_import_compatibility(connection: &Connection) -> Result<()> {
    let conflicting_session: Option<String> = connection
        .query_row(
            "SELECT source.id
             FROM import_source.sessions source
             JOIN sessions destination ON destination.id=source.id
             WHERE NOT (
               destination.agent IS source.agent AND destination.cwd IS source.cwd AND
               destination.started_at_ms IS source.started_at_ms AND destination.ended_at_ms IS source.ended_at_ms AND
               destination.status IS source.status AND
               destination.title IS source.title AND destination.model IS source.model AND
               destination.provider IS source.provider AND destination.git_branch IS source.git_branch AND
               destination.parent_session_id IS source.parent_session_id AND destination.parent_relation IS source.parent_relation AND destination.fork_point_native_id IS source.fork_point_native_id AND
               destination.fingerprint IS source.fingerprint AND destination.meta_json IS source.meta_json
             )
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = conflicting_session {
        anyhow::bail!("import conflicts with existing session {id}");
    }

    let conflicting_event_session: Option<String> = connection
        .query_row(
            "SELECT session_id FROM (
               SELECT * FROM (
                 SELECT session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,parent_kind,span_id,model,provider,usage_json,text,data_json,created_at_ms,ended_at_ms
                 FROM import_source.events WHERE session_id IN (SELECT id FROM sessions)
                 EXCEPT
                 SELECT session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,parent_kind,span_id,model,provider,usage_json,text,data_json,created_at_ms,ended_at_ms
                 FROM events
               )
               UNION ALL
               SELECT * FROM (
                 SELECT session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,parent_kind,span_id,model,provider,usage_json,text,data_json,created_at_ms,ended_at_ms
                 FROM events WHERE session_id IN (SELECT id FROM import_source.sessions)
                 EXCEPT
                 SELECT session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,parent_kind,span_id,model,provider,usage_json,text,data_json,created_at_ms,ended_at_ms
                 FROM import_source.events
               )
             ) LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = conflicting_event_session {
        anyhow::bail!("import has divergent events for existing session {id}");
    }

    let conflicting_source: Option<(String, String)> = connection
        .query_row(
            "SELECT source.session_id,source.locator
             FROM import_source.raw_sources source
             JOIN raw_sources destination
               ON destination.session_id=source.session_id AND destination.locator=source.locator
             WHERE NOT (
               destination.kind IS source.kind AND destination.restore_path IS source.restore_path AND
               destination.role IS source.role AND destination.bytes IS source.bytes AND
               destination.mtime_ns IS source.mtime_ns AND destination.mode IS source.mode
             ) OR (
               destination.object_hash IS NOT NULL AND source.object_hash IS NOT NULL AND
               destination.object_hash <> source.object_hash
             )
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((session_id, locator)) = conflicting_source {
        anyhow::bail!("import has divergent native source {locator} for session {session_id}");
    }
    Ok(())
}

pub fn migrate(conn: &Connection) -> Result<()> {
    migrate_with_tokenizer(conn, false)
}

/// Persist the latest ingest outcome and a cumulative failure counter in the
/// archive metadata table used by doctor and future background services.
pub fn record_ingest_status(conn: &mut Connection, report: &IngestReport) -> Result<IngestAck> {
    let tx = conn.transaction()?;
    let previous: Option<String> = tx
        .query_row(
            "SELECT value FROM schema_meta WHERE key='ingest.last_status'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let cumulative_failed = previous
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .context("invalid persisted ingest status in schema_meta")?
        .as_ref()
        .and_then(|status| status.get("cumulativeFailed"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default()
        + report.total_failed() as i64;
    let previous_sequence: Option<String> = tx
        .query_row(
            "SELECT value FROM schema_meta WHERE key='ingest.next_ack_sequence'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let sequence = previous_sequence
        .as_deref()
        .map(|value| {
            value
                .parse::<u64>()
                .context("invalid persisted ingest acknowledgement sequence")
        })
        .transpose()?
        .or_else(|| {
            previous
                .as_deref()
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                .and_then(|status| {
                    status
                        .get("ackSequence")
                        .and_then(serde_json::Value::as_u64)
                })
        })
        .unwrap_or_default()
        .checked_add(1)
        .context("ingest acknowledgement sequence exhausted")?;
    let committed_at_ms = now_ms();
    let status = serde_json::json!({
        "ackSequence": sequence,
        "completedAtMs": committed_at_ms,
        "discovered": report.total_discovered(),
        "ingested": report.total_ingested(),
        "skipped": report.total_skipped(),
        "failed": report.total_failed(),
        "cumulativeFailed": cumulative_failed,
    });
    tx.execute(
        "INSERT OR REPLACE INTO schema_meta(key,value) VALUES('ingest.next_ack_sequence',?1)",
        [sequence.to_string()],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO schema_meta(key,value) VALUES('ingest.last_status',?1)",
        [status.to_string()],
    )?;
    tx.commit()?;
    Ok(IngestAck {
        sequence,
        committed_at_ms,
    })
}

/// Read persisted ingest telemetry without mutating the archive.
pub fn ingest_status(conn: &Connection) -> Result<Option<StoredIngestStatus>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key='ingest.last_status'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(value) = value else {
        return Ok(None);
    };
    let status: serde_json::Value = serde_json::from_str(&value)
        .with_context(|| "invalid persisted ingest status in schema_meta")?;
    let get_usize = |key: &str| -> Result<usize> {
        status
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .with_context(|| format!("ingest status field {key} is missing or invalid"))
    };
    Ok(Some(StoredIngestStatus {
        ack_sequence: status
            .get("ackSequence")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        completed_at_ms: status
            .get("completedAtMs")
            .and_then(serde_json::Value::as_i64)
            .context("ingest status field completedAtMs is missing or invalid")?,
        discovered: get_usize("discovered")?,
        ingested: get_usize("ingested")?,
        skipped: get_usize("skipped")?,
        failed: get_usize("failed")?,
        cumulative_failed: get_usize("cumulativeFailed")?,
    }))
}

const INGEST_QUARANTINE_KEY: &str = "ingest.quarantine";
const QUARANTINE_FAILURE_THRESHOLD: u32 = 3;
const QUARANTINE_RETRY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestQuarantineEntry {
    pub fingerprint: String,
    pub failures: u32,
    pub last_failed_ms: i64,
}

/// Load persisted ingest quarantine entries keyed by candidate locator.
pub fn load_ingest_quarantine(conn: &Connection) -> Result<HashMap<String, IngestQuarantineEntry>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key=?1",
            [INGEST_QUARANTINE_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    serde_json::from_str(&value).with_context(|| "invalid ingest quarantine in schema_meta")
}

/// Return whether a candidate should be skipped because of repeated failures.
pub fn is_ingest_quarantined(
    quarantine: &HashMap<String, IngestQuarantineEntry>,
    locator: &str,
    fingerprint: &str,
    now_ms: i64,
) -> bool {
    let Some(entry) = quarantine.get(locator) else {
        return false;
    };
    if entry.fingerprint != fingerprint {
        return false;
    }
    if entry.failures < QUARANTINE_FAILURE_THRESHOLD {
        return false;
    }
    now_ms.saturating_sub(entry.last_failed_ms) < QUARANTINE_RETRY_MS
}

/// Record ingest failures and clear quarantine for successfully ingested locators.
pub fn update_ingest_quarantine(
    conn: &mut Connection,
    ingested_locators: &[String],
    failures: &[(String, String)],
) -> Result<()> {
    // The common unchanged-watch pass has neither successes to clear nor
    // failures to record. Avoid decoding and rewriting the potentially large
    // JSON map in schema_meta in that case.
    if ingested_locators.is_empty() && failures.is_empty() {
        return Ok(());
    }
    let mut quarantine = load_ingest_quarantine(conn)?;
    let mut changed = false;
    for locator in ingested_locators {
        changed |= quarantine.remove(locator).is_some();
    }
    let now_ms = now_ms();
    for (locator, fingerprint) in failures {
        let entry = quarantine
            .entry(locator.clone())
            .or_insert(IngestQuarantineEntry {
                fingerprint: fingerprint.clone(),
                failures: 0,
                last_failed_ms: now_ms,
            });
        if entry.fingerprint == *fingerprint {
            entry.failures = entry.failures.saturating_add(1);
            entry.last_failed_ms = now_ms;
        } else {
            *entry = IngestQuarantineEntry {
                fingerprint: fingerprint.clone(),
                failures: 1,
                last_failed_ms: now_ms,
            };
        }
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    let payload = serde_json::to_string(&quarantine)?;
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta(key,value) VALUES(?1,?2)",
        params![INGEST_QUARANTINE_KEY, payload],
    )?;
    Ok(())
}

const CODEX_ROLLOUT_CACHE_KEY: &str = "codex.rollout_cache";

/// Load persisted Codex rollout lineage cache entries keyed by rollout path.
pub fn load_codex_rollout_cache(
    conn: &Connection,
) -> Result<HashMap<String, crate::parsers::codex::CodexRolloutCacheEntry>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key=?1",
            [CODEX_ROLLOUT_CACHE_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    serde_json::from_str(&value).with_context(|| "invalid codex rollout cache in schema_meta")
}

/// Persist the Codex rollout lineage cache used to skip unchanged rollout scans.
pub fn save_codex_rollout_cache(
    conn: &mut Connection,
    cache: &HashMap<String, crate::parsers::codex::CodexRolloutCacheEntry>,
) -> Result<()> {
    let previous: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key=?1",
            params![CODEX_ROLLOUT_CACHE_KEY],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(previous) = previous {
        if let Ok(previous_cache) = serde_json::from_str::<
            HashMap<String, crate::parsers::codex::CodexRolloutCacheEntry>,
        >(&previous)
        {
            if previous_cache == *cache {
                return Ok(());
            }
        }
    }
    let payload = serde_json::to_string(cache)?;
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta(key,value) VALUES(?1,?2)",
        params![CODEX_ROLLOUT_CACHE_KEY, payload],
    )?;
    Ok(())
}

fn migrate_with_tokenizer(conn: &Connection, jieba: bool) -> Result<()> {
    let tokenizer = if jieba { "jieba" } else { PORTABLE_TOKENIZER };
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    let stored_version: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    // The normalized layer is a deterministic, rebuildable projection of the
    // native stores, so TraceDB does not carry per-column upgrade paths. An
    // archive either already speaks the current schema or is re-ingested.
    if let Some(stored) = stored_version.as_deref() {
        let version = stored
            .parse::<i64>()
            .context("invalid TraceDB schema version")?;
        if version != SCHEMA_VERSION {
            anyhow::bail!(
                "TraceDB archive uses schema version {version} but this build requires {SCHEMA_VERSION}; \
                 delete the archive and re-run `trace-db ingest` to rebuild it from the native stores"
            );
        }
    }
    let previous_tokenizer: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key='tokenizer'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let schema = r#"
      CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY, agent TEXT NOT NULL, cwd TEXT, started_at_ms INTEGER,
        ended_at_ms INTEGER, status TEXT, title TEXT, model TEXT, provider TEXT, git_branch TEXT,
        parent_session_id TEXT, parent_relation TEXT, fork_point_native_id TEXT,
        fingerprint TEXT NOT NULL, meta_json TEXT NOT NULL, ingested_at_ms INTEGER NOT NULL,
        -- Materialized deterministic projections of the event stream and native
        -- sources. Retrieval reads these instead of recomputing per row;
        -- `reindex` repairs them and `verify` reports drift.
        event_count INTEGER NOT NULL DEFAULT 0, turn_count INTEGER NOT NULL DEFAULT 0,
        child_count INTEGER NOT NULL DEFAULT 0, tool_call_count INTEGER NOT NULL DEFAULT 0,
        error_count INTEGER NOT NULL DEFAULT 0,
        input_tokens INTEGER, output_tokens INTEGER, total_tokens INTEGER,
        first_user_text TEXT, last_assistant_text TEXT,
        source_count INTEGER NOT NULL DEFAULT 0, source_bytes INTEGER NOT NULL DEFAULT 0,
        latest_source_mtime_ns INTEGER,
        sort_time INTEGER NOT NULL DEFAULT 0,
        CHECK ((parent_session_id IS NULL) = (parent_relation IS NULL))
      );
      CREATE INDEX IF NOT EXISTS sessions_agent_idx ON sessions(agent);
      CREATE INDEX IF NOT EXISTS sessions_ended_idx ON sessions(ended_at_ms);
      CREATE INDEX IF NOT EXISTS sessions_parent_idx ON sessions(parent_session_id);
      -- Serves `list`'s keyset pagination directly: the ordering key is stored,
      -- so paging is an index scan rather than a full scan plus temp B-tree.
      CREATE INDEX IF NOT EXISTS sessions_sort_idx ON sessions(sort_time DESC, id ASC);
      CREATE TABLE IF NOT EXISTS raw_sources (
        session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        locator TEXT NOT NULL, kind TEXT NOT NULL, restore_path TEXT NOT NULL,
        role TEXT, bytes INTEGER, mtime_ns INTEGER, mode INTEGER, object_hash TEXT,
        PRIMARY KEY(session_id, locator)
      );
      CREATE TABLE IF NOT EXISTS objects (
        hash TEXT PRIMARY KEY, compression TEXT NOT NULL, bytes INTEGER NOT NULL,
        payload BLOB NOT NULL, created_at_ms INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS events (
        id INTEGER PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        idx INTEGER NOT NULL, kind TEXT NOT NULL, subtype TEXT, role TEXT, name TEXT,
        call_id TEXT, is_error INTEGER, native_id TEXT, parent_id TEXT, parent_kind TEXT, span_id TEXT, model TEXT,
        provider TEXT, usage_json TEXT, text TEXT NOT NULL, data_json TEXT, created_at_ms INTEGER,
        ended_at_ms INTEGER
      );
      CREATE INDEX IF NOT EXISTS events_session_idx ON events(session_id,idx);
      CREATE INDEX IF NOT EXISTS events_kind_idx ON events(kind);
      CREATE INDEX IF NOT EXISTS events_span_idx ON events(session_id,span_id);
      CREATE INDEX IF NOT EXISTS events_call_idx ON events(session_id,call_id);
      CREATE TABLE IF NOT EXISTS spans (
        session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        id TEXT NOT NULL, parent_span_id TEXT, kind TEXT NOT NULL, name TEXT,
        native_id TEXT, call_id TEXT, status TEXT, started_at_ms INTEGER, ended_at_ms INTEGER,
        start_event_idx INTEGER, end_event_idx INTEGER, data_json TEXT,
        PRIMARY KEY(session_id,id)
      );
      CREATE INDEX IF NOT EXISTS spans_parent_idx ON spans(session_id,parent_span_id);
      CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(text, content='events', content_rowid='id', tokenize='TOKENIZER_PLACEHOLDER');
      CREATE TRIGGER IF NOT EXISTS events_ai AFTER INSERT ON events WHEN new.kind NOT IN ('tool_result','usage') BEGIN INSERT INTO events_fts(rowid,text) VALUES(new.id,new.text); END;
      CREATE TRIGGER IF NOT EXISTS events_ad AFTER DELETE ON events WHEN old.kind NOT IN ('tool_result','usage') BEGIN INSERT INTO events_fts(events_fts,rowid,text) VALUES('delete',old.id,old.text); END;
      CREATE TRIGGER IF NOT EXISTS events_au AFTER UPDATE ON events BEGIN
        INSERT INTO events_fts(events_fts,rowid,text) SELECT 'delete',old.id,old.text WHERE old.kind NOT IN ('tool_result','usage');
        INSERT INTO events_fts(rowid,text) SELECT new.id,new.text WHERE new.kind NOT IN ('tool_result','usage');
      END;
    "#.replace("TOKENIZER_PLACEHOLDER", tokenizer);
    conn.execute_batch(&schema)?;
    if previous_tokenizer
        .as_deref()
        .is_some_and(|value| value != tokenizer)
    {
        conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS events_fts;
             CREATE VIRTUAL TABLE events_fts USING fts5(text, content='events', content_rowid='id', tokenize='{tokenizer}');"
        ))?;
        conn.execute("INSERT INTO events_fts(rowid,text) SELECT id,text FROM events WHERE kind NOT IN ('tool_result','usage')", [])?;
    }
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta(key,value) VALUES('schema_version',?1)",
        [SCHEMA_VERSION.to_string()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta(key,value) VALUES('tokenizer',?1)",
        [tokenizer],
    )?;
    Ok(())
}

/// Read the stored schema version of an attached (or the main) database.
fn schema_version(conn: &Connection, schema: &str) -> Result<Option<i64>> {
    let stored: Option<String> = conn
        .query_row(
            &format!(
                "SELECT value FROM \"{}\".schema_meta WHERE key='schema_version'",
                schema.replace('"', "\"\"")
            ),
            [],
            |row| row.get(0),
        )
        .optional()?;
    stored
        .map(|value| {
            value
                .parse::<i64>()
                .context("invalid TraceDB schema version")
        })
        .transpose()
}

pub fn open_for_verification(path: &Path) -> Result<Connection> {
    if !path.exists() {
        anyhow::bail!("TraceDB archive does not exist: {}", path.display());
    }
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "busy_timeout", 5000i64)?;
    Ok(connection)
}

pub fn probe_jieba_extension(path: &Path) -> Result<()> {
    let connection = Connection::open_in_memory()?;
    unsafe {
        connection.load_extension_enable()?;
        let load_result = connection.load_extension(path, Some("sqlite3_fts5jieba_init"));
        connection.load_extension_disable()?;
        load_result?;
    }
    connection.execute_batch(
        "CREATE VIRTUAL TABLE tokenizer_probe USING fts5(text, tokenize='jieba');
         DROP TABLE tokenizer_probe;",
    )?;
    Ok(())
}

pub fn verify(connection: &Connection, path: &Path) -> Result<crate::VerifyReport> {
    use crate::{VerificationFailure, VerifyCheck, VerifyReport};

    let mut checks = Vec::new();

    let integrity_rows = connection
        .prepare("PRAGMA integrity_check")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let integrity_failures = integrity_rows
        .iter()
        .filter(|row| row.as_str() != "ok")
        .map(|message| VerificationFailure {
            locator: path.display().to_string(),
            message: message.clone(),
        })
        .collect::<Vec<_>>();
    checks.push(VerifyCheck::new(
        "sqlite_integrity",
        integrity_rows.len(),
        integrity_failures,
    ));

    let foreign_key_failures = connection
        .prepare("PRAGMA foreign_key_check")?
        .query_map([], |row| {
            Ok(VerificationFailure {
                locator: format!("{} row {}", row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
                message: format!("references missing parent in {}", row.get::<_, String>(2)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    checks.push(VerifyCheck::new(
        "foreign_keys",
        foreign_key_failures.len(),
        foreign_key_failures,
    ));

    let mut span_reference_failures = connection
        .prepare(
            "SELECT e.session_id,e.idx,e.span_id FROM events e
             LEFT JOIN spans s ON s.session_id=e.session_id AND s.id=e.span_id
             WHERE e.span_id IS NOT NULL AND s.id IS NULL",
        )?
        .query_map([], |row| {
            Ok(VerificationFailure {
                locator: format!(
                    "{} event {}",
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?
                ),
                message: format!("references missing span {}", row.get::<_, String>(2)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut missing_parents = connection
        .prepare(
            "SELECT s.session_id,s.id,s.parent_span_id FROM spans s
             LEFT JOIN spans parent ON parent.session_id=s.session_id AND parent.id=s.parent_span_id
             WHERE s.parent_span_id IS NOT NULL AND parent.id IS NULL",
        )?
        .query_map([], |row| {
            Ok(VerificationFailure {
                locator: format!(
                    "{} span {}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?
                ),
                message: format!(
                    "references missing parent span {}",
                    row.get::<_, String>(2)?
                ),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    span_reference_failures.append(&mut missing_parents);
    checks.push(VerifyCheck::new(
        "span_references",
        connection.query_row("SELECT count(*) FROM spans", [], |row| row.get(0))?,
        span_reference_failures,
    ));

    let contract_failures = verify_contract(connection)?;
    checks.push(VerifyCheck::new("schema_contract", 3, contract_failures));

    // Materialized aggregates are derived state, so drift is exactly the class
    // of corruption a verifier should surface rather than silently serve.
    let aggregate_failures = aggregate_drift(connection)?;
    checks.push(VerifyCheck::new(
        "session_aggregates",
        connection.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))?,
        aggregate_failures,
    ));

    // Rank 0 checks the FTS shadow tables without comparing every row in the
    // external content table. TraceDB intentionally indexes only searchable
    // event kinds, so the rank-1 full-content comparison is not applicable.
    let mut fts_failures = match connection.execute(
        "INSERT INTO events_fts(events_fts,rank) VALUES('integrity-check',0)",
        [],
    ) {
        Ok(_) => Vec::new(),
        // FTS5's rank-0 integrity check writes transient shadow-table state,
        // which SQLite refuses for a read-only connection. The structural
        // document-count and excluded-event checks below remain read-only.
        Err(error) if error.to_string().contains("readonly database") => Vec::new(),
        Err(error) => vec![VerificationFailure {
            locator: "events_fts".into(),
            message: error.to_string(),
        }],
    };
    let searchable_events = connection.query_row(
        "SELECT count(*) FROM events WHERE kind NOT IN ('tool_result','usage')",
        [],
        |row| row.get::<_, usize>(0),
    )?;
    let indexed_events =
        connection.query_row("SELECT count(*) FROM events_fts_docsize", [], |row| {
            row.get::<_, usize>(0)
        })?;
    if indexed_events != searchable_events {
        fts_failures.push(VerificationFailure {
            locator: "events_fts".into(),
            message: format!(
                "indexed document count mismatch: expected {searchable_events}, found {indexed_events}"
            ),
        });
    }
    let excluded_events = connection.query_row(
        "SELECT count(*) FROM events_fts_docsize f
         JOIN events e ON e.id=f.id
         WHERE e.kind IN ('tool_result','usage')",
        [],
        |row| row.get::<_, usize>(0),
    )?;
    if excluded_events != 0 {
        fts_failures.push(VerificationFailure {
            locator: "events_fts".into(),
            message: format!("index contains {excluded_events} excluded event(s)"),
        });
    }
    checks.push(VerifyCheck::new(
        "fts_consistency",
        searchable_events,
        fts_failures,
    ));

    let reference_failures = connection
        .prepare(
            "SELECT r.session_id,r.locator,r.object_hash
             FROM raw_sources r
             LEFT JOIN objects o ON o.hash=r.object_hash
             WHERE r.object_hash IS NOT NULL AND o.hash IS NULL
             ORDER BY r.session_id,r.locator",
        )?
        .query_map([], |row| {
            Ok(VerificationFailure {
                locator: format!("{}:{}", row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                message: format!("referenced object {} is missing", row.get::<_, String>(2)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let referenced_objects = connection.query_row(
        "SELECT count(*) FROM raw_sources WHERE object_hash IS NOT NULL",
        [],
        |row| row.get::<_, usize>(0),
    )?;
    checks.push(VerifyCheck::new(
        "object_references",
        referenced_objects,
        reference_failures,
    ));

    let lineage_failures = connection
        .prepare(
            "SELECT id,parent_session_id,parent_relation FROM sessions
             WHERE (parent_session_id IS NULL) <> (parent_relation IS NULL)
                OR (parent_relation IS NOT NULL
                    AND parent_relation NOT IN ('subagent','fork'))
             ORDER BY id",
        )?
        .query_map([], |row| {
            let id = row.get::<_, String>(0)?;
            let parent = row.get::<_, Option<String>>(1)?;
            let relation = row.get::<_, Option<String>>(2)?;
            Ok(VerificationFailure {
                locator: id,
                message: match (parent, relation) {
                    (Some(_), None) => "parent edge has no typed relation".into(),
                    (None, Some(relation)) => {
                        format!("relation {relation} has no parent session")
                    }
                    (_, Some(relation)) => format!("unknown parent relation {relation}"),
                    (None, None) => "inconsistent lineage".into(),
                },
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let lineage_edges = connection.query_row(
        "SELECT count(*) FROM sessions WHERE parent_session_id IS NOT NULL",
        [],
        |row| row.get::<_, usize>(0),
    )?;
    checks.push(VerifyCheck::new(
        "session_lineage",
        lineage_edges,
        lineage_failures,
    ));

    let mut span_failures = connection
        .prepare(
            "SELECT e.session_id,e.idx,e.span_id
             FROM events e
             LEFT JOIN spans s ON s.session_id=e.session_id AND s.id=e.span_id
             WHERE e.span_id IS NOT NULL AND s.id IS NULL
             ORDER BY e.session_id,e.idx",
        )?
        .query_map([], |row| {
            Ok(VerificationFailure {
                locator: format!("{}:{}", row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
                message: format!(
                    "event references span {} that does not exist; run `trace-db reindex` to repair",
                    row.get::<_, String>(2)?
                ),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    span_failures.extend(
        connection
            .prepare(
                "SELECT sp.session_id,sp.id,sp.start_event_idx,sp.end_event_idx,s.event_count
                 FROM spans sp JOIN sessions s ON s.id=sp.session_id
                 WHERE (sp.start_event_idx IS NOT NULL
                        AND (sp.start_event_idx < 0 OR sp.start_event_idx >= s.event_count))
                    OR (sp.end_event_idx IS NOT NULL
                        AND (sp.end_event_idx < 0 OR sp.end_event_idx >= s.event_count))
                    OR (sp.start_event_idx IS NOT NULL AND sp.end_event_idx IS NOT NULL
                        AND sp.end_event_idx < sp.start_event_idx)
                 ORDER BY sp.session_id,sp.id",
            )?
            .query_map([], |row| {
                Ok(VerificationFailure {
                    locator: format!("{}:{}", row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    message: format!(
                        "span event range [{:?},{:?}] is outside the session's {} event(s)",
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, i64>(4)?
                    ),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    );
    let total_spans = connection.query_row("SELECT count(*) FROM spans", [], |row| {
        row.get::<_, usize>(0)
    })?;
    checks.push(VerifyCheck::new("spans", total_spans, span_failures));

    let mut object_statement =
        connection.prepare("SELECT hash,compression,bytes,payload FROM objects ORDER BY hash")?;
    let objects = object_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut object_failures = Vec::new();
    for (hash, compression, expected_bytes, payload) in &objects {
        if compression != "zstd" {
            object_failures.push(VerificationFailure {
                locator: hash.clone(),
                message: format!("unsupported compression {compression:?}"),
            });
            continue;
        }
        let bytes = match zstd::decode_all(payload.as_slice()) {
            Ok(bytes) => bytes,
            Err(error) => {
                object_failures.push(VerificationFailure {
                    locator: hash.clone(),
                    message: format!("zstd decompression failed: {error}"),
                });
                continue;
            }
        };
        if i64::try_from(bytes.len()).ok() != Some(*expected_bytes) {
            object_failures.push(VerificationFailure {
                locator: hash.clone(),
                message: format!(
                    "length mismatch: expected {expected_bytes}, decoded {}",
                    bytes.len()
                ),
            });
        }
        let actual_hash = hex::encode(Sha256::digest(&bytes));
        if actual_hash != *hash {
            object_failures.push(VerificationFailure {
                locator: hash.clone(),
                message: format!("SHA-256 mismatch: decoded object hashes to {actual_hash}"),
            });
        }
    }
    checks.push(VerifyCheck::new("objects", objects.len(), object_failures));

    Ok(VerifyReport::new(path.to_path_buf(), checks))
}

fn verify_contract(connection: &Connection) -> Result<Vec<crate::VerificationFailure>> {
    let mut failures = Vec::new();
    let expected_version = SCHEMA_VERSION.to_string();
    let actual_version = connection
        .query_row(
            "SELECT value FROM schema_meta WHERE key='schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if actual_version.as_deref() != Some(expected_version.as_str()) {
        failures.push(crate::VerificationFailure {
            locator: "schema_meta.schema_version".into(),
            message: format!("expected {expected_version:?}, found {actual_version:?}"),
        });
    }
    let tokenizer = connection
        .query_row(
            "SELECT value FROM schema_meta WHERE key='tokenizer'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if !matches!(
        tokenizer.as_deref(),
        Some(PORTABLE_TOKENIZER | JIEBA_TOKENIZER)
    ) {
        failures.push(crate::VerificationFailure {
            locator: "schema_meta.tokenizer".into(),
            message: format!("unsupported tokenizer contract {tokenizer:?}"),
        });
    }
    let unsnapshotted: i64 = connection.query_row(
        "SELECT count(*) FROM sessions s
         WHERE EXISTS (SELECT 1 FROM raw_sources r WHERE r.session_id=s.id)
           AND NOT EXISTS (SELECT 1 FROM raw_sources r WHERE r.session_id=s.id AND r.object_hash IS NOT NULL)",
        [],
        |row| row.get(0),
    )?;
    if unsnapshotted != 0 {
        failures.push(crate::VerificationFailure {
            locator: "raw_sources.object_hash".into(),
            message: format!(
                "archive contains {unsnapshotted} session(s) whose native sources have no captured snapshot"
            ),
        });
    }
    Ok(failures)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn upsert(conn: &mut Connection, mut parsed: ParsedSession) -> Result<()> {
    upsert_many(conn, std::slice::from_mut(&mut parsed))
}

/// Upsert a batch in one transaction. Ingest callers use this as the normal
/// path so SQLite pays one WAL/fsync boundary per agent rather than one per
/// session. Callers that need per-candidate best-effort isolation can fall
/// back to [`upsert`] when the batch transaction fails.
pub fn upsert_many(conn: &mut Connection, parsed_sessions: &mut [ParsedSession]) -> Result<()> {
    let tx = conn.transaction()?;
    for parsed in parsed_sessions {
        assign_indexes(&mut parsed.events);
        let spans = derive_spans(&mut parsed.events);
        write_session(&tx, &parsed.session, &parsed.events, &spans)?;
    }
    tx.commit()?;
    Ok(())
}

pub fn candidate_states(
    conn: &Connection,
    agent: crate::model::Agent,
) -> Result<HashMap<String, CandidateState>> {
    let mut statement = conn.prepare(
        "SELECT r.locator,s.fingerprint
         FROM raw_sources r
         JOIN sessions s ON s.id=r.session_id
         WHERE s.agent=?1",
    )?;
    let rows = statement.query_map([agent.as_str()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut states = HashMap::new();
    for row in rows {
        let (locator, fingerprint) = row?;
        states.insert(locator, CandidateState { fingerprint });
    }
    Ok(states)
}

fn write_session(
    tx: &Transaction<'_>,
    session: &Session,
    events: &[Event],
    spans: &[Span],
) -> Result<()> {
    let mut statement = tx.prepare(
        "SELECT locator,kind,restore_path,role,bytes,mtime_ns,mode,object_hash
         FROM raw_sources WHERE session_id=?1 AND object_hash IS NOT NULL",
    )?;
    let previous_captured_sources = statement
        .query_map([&session.id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let previous_parent: Option<Option<String>> = tx
        .query_row(
            "SELECT parent_session_id FROM sessions WHERE id=?1",
            [&session.id],
            |row| row.get(0),
        )
        .optional()?;
    let ingested_at_ms = now_ms();
    let aggregates = SessionAggregates::derive(events).with_sort_time(session, ingested_at_ms);
    tx.execute("INSERT INTO sessions(id,agent,cwd,started_at_ms,ended_at_ms,status,title,model,provider,git_branch,parent_session_id,parent_relation,fork_point_native_id,fingerprint,meta_json,ingested_at_ms,event_count,turn_count,tool_call_count,error_count,input_tokens,output_tokens,total_tokens,first_user_text,last_assistant_text,sort_time)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26)
                ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,cwd=excluded.cwd,started_at_ms=excluded.started_at_ms,ended_at_ms=excluded.ended_at_ms,status=excluded.status,title=excluded.title,model=excluded.model,provider=excluded.provider,git_branch=excluded.git_branch,parent_session_id=excluded.parent_session_id,parent_relation=excluded.parent_relation,fork_point_native_id=excluded.fork_point_native_id,fingerprint=excluded.fingerprint,meta_json=excluded.meta_json,ingested_at_ms=excluded.ingested_at_ms,event_count=excluded.event_count,turn_count=excluded.turn_count,tool_call_count=excluded.tool_call_count,error_count=excluded.error_count,input_tokens=excluded.input_tokens,output_tokens=excluded.output_tokens,total_tokens=excluded.total_tokens,first_user_text=excluded.first_user_text,last_assistant_text=excluded.last_assistant_text,sort_time=excluded.sort_time",
        params![session.id, session.agent.as_str(), session.cwd, session.started_at_ms, session.ended_at_ms, session.status.map(|status| status.to_string()), session.title, session.model, session.provider, session.git_branch, session.parent_session_id, session.parent_relation.map(|relation| relation.to_string()), session.fork_point_native_id, session.fingerprint, session.meta.to_string(), ingested_at_ms, aggregates.event_count, aggregates.turn_count, aggregates.tool_call_count, aggregates.error_count, aggregates.input_tokens, aggregates.output_tokens, aggregates.total_tokens, aggregates.first_user_text, aggregates.last_assistant_text, aggregates.sort_time])?;
    // `child_count` belongs to the parent but is only learnable when a child is
    // written, and children can arrive before their parent, after it, or again
    // on re-ingest. Recomputing the affected parents from the indexed edge
    // inside this transaction is idempotent under every one of those orders,
    // unlike an incrementing counter. A re-parented child repairs both its old
    // and new parent.
    for parent_id in [previous_parent.flatten(), session.parent_session_id.clone()]
        .into_iter()
        .flatten()
        .collect::<std::collections::BTreeSet<_>>()
    {
        refresh_child_count(tx, &parent_id)?;
    }
    // A parent ingested after its children adopts the count they already imply.
    refresh_child_count(tx, &session.id)?;
    tx.execute("DELETE FROM raw_sources WHERE session_id=?1", [&session.id])?;
    let mut current_locators = Vec::with_capacity(session.sources.len());
    for src in &session.sources {
        current_locators.push(src.locator.clone());
        let object_hash = capture_source(tx, src)?;
        tx.execute("INSERT INTO raw_sources(session_id,locator,kind,restore_path,role,bytes,mtime_ns,mode,object_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![session.id,src.locator,src.kind,src.restore_path,src.role,src.bytes,src.mtime_ns,src.mode.map(|v|v as i64),object_hash])?;
    }
    for (locator, kind, restore_path, role, bytes, mtime_ns, source_mode, object_hash) in
        previous_captured_sources
    {
        if !current_locators.iter().any(|current| current == &locator) {
            tx.execute(
                "INSERT INTO raw_sources(session_id,locator,kind,restore_path,role,bytes,mtime_ns,mode,object_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![session.id, locator, kind, restore_path, role, bytes, mtime_ns, source_mode, object_hash],
            )?;
        }
    }
    // Derived from the retained rows rather than the parse, because the block
    // above deliberately keeps snapshots whose locator vanished from the source.
    refresh_source_aggregates(tx, &session.id)?;
    if !events_match_stored(tx, &session.id, events)? {
        tx.execute("DELETE FROM events WHERE session_id=?1", [&session.id])?;
        for e in events {
            let usage_json = e.usage.as_ref().map(serde_json::to_string).transpose()?;
            tx.execute("INSERT INTO events(session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,parent_kind,span_id,model,provider,usage_json,text,data_json,created_at_ms,ended_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)", params![session.id,e.idx,e.kind.as_str(),e.subtype,e.role,e.name,e.call_id,e.is_error.map(i64::from),e.native_id,e.parent_id,e.parent_kind.map(|kind| kind.to_string()),e.span_id,e.model,e.provider,usage_json,e.text,e.data_json.as_ref().map(Value::to_string),e.created_at_ms,e.ended_at_ms])?;
        }
    }
    tx.execute("DELETE FROM spans WHERE session_id=?1", [&session.id])?;
    for span in spans {
        insert_span(tx, &session.id, span)?;
    }
    Ok(())
}

/// Write one span row. Shared by ingest and by `reindex`'s span repair so the
/// two paths cannot drift apart.
fn insert_span(tx: &Transaction<'_>, session_id: &str, span: &Span) -> Result<()> {
    tx.execute(
        "INSERT INTO spans(session_id,id,parent_span_id,kind,name,native_id,call_id,status,started_at_ms,ended_at_ms,start_event_idx,end_event_idx,data_json)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            session_id,
            span.id,
            span.parent_span_id,
            span.kind.to_string(),
            span.name,
            span.native_id,
            span.call_id,
            span.status.map(|status| status.to_string()),
            span.started_at_ms,
            span.ended_at_ms,
            span.start_event_idx,
            span.end_event_idx,
            span.data_json.as_ref().map(Value::to_string),
        ],
    )?;
    Ok(())
}

/// Recompute the stored source totals from the rows the archive retained.
fn refresh_source_aggregates(tx: &Transaction<'_>, session_id: &str) -> Result<()> {
    tx.execute(
        "UPDATE sessions SET
           source_count=(SELECT count(*) FROM raw_sources r WHERE r.session_id=?1),
           source_bytes=(SELECT coalesce(sum(coalesce(r.bytes,0)),0) FROM raw_sources r WHERE r.session_id=?1),
           latest_source_mtime_ns=(SELECT max(r.mtime_ns) FROM raw_sources r WHERE r.session_id=?1)
         WHERE id=?1",
        [session_id],
    )?;
    Ok(())
}

/// Recompute one session's `child_count` from the authoritative lineage edge.
///
/// Idempotent by construction: it derives the count rather than adjusting it,
/// so repeat ingest and out-of-order arrival converge to the same value. A
/// missing session id is a no-op, which is what makes a child that precedes its
/// parent harmless.
fn refresh_child_count(tx: &Transaction<'_>, session_id: &str) -> Result<()> {
    tx.execute(
        "UPDATE sessions SET child_count=
           (SELECT count(*) FROM sessions child WHERE child.parent_session_id=?1)
         WHERE id=?1",
        [session_id],
    )?;
    Ok(())
}

fn events_match_stored(tx: &Transaction<'_>, session_id: &str, events: &[Event]) -> Result<bool> {
    let mut statement = tx.prepare(
        "SELECT idx, kind, subtype, role, name, call_id, is_error, native_id,
                parent_id, parent_kind, span_id, model, provider, usage_json, text, data_json, created_at_ms, ended_at_ms
         FROM events WHERE session_id=?1 ORDER BY idx",
    )?;
    let stored = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, String>(14)?,
                row.get::<_, Option<String>>(15)?,
                row.get::<_, Option<i64>>(16)?,
                row.get::<_, Option<i64>>(17)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if stored.len() != events.len() {
        return Ok(false);
    }
    for (
        event,
        (
            idx,
            kind,
            subtype,
            role,
            name,
            call_id,
            is_error,
            native_id,
            parent_id,
            parent_kind,
            span_id,
            model,
            provider,
            usage_json,
            text,
            data_json,
            created_at_ms,
            ended_at_ms,
        ),
    ) in events.iter().zip(stored)
    {
        if event.idx != idx
            || event.kind.as_str() != kind
            || event.subtype != subtype
            || event.role != role
            || event.name != name
            || event.call_id != call_id
            || event.is_error.map(i64::from) != is_error
            || event.native_id != native_id
            || event.parent_id != parent_id
            || event.parent_kind.map(|kind| kind.to_string()) != parent_kind
            || event.span_id != span_id
            || event.model != model
            || event.provider != provider
            || event
                .usage
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?
                != usage_json
            || event.text != text
            || event.data_json.as_ref().map(ToString::to_string) != data_json
            || event.created_at_ms != created_at_ms
            || event.ended_at_ms != ended_at_ms
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn capture_source(tx: &Transaction<'_>, src: &NativeSource) -> Result<Option<String>> {
    match &src.capture {
        Some(crate::model::Capture::File { path }) => {
            let path = PathBuf::from(path);
            let before = fs::metadata(&path)
                .with_context(|| format!("inspect native source {}", path.display()))?;
            validate_source_metadata(src, &path, &before)?;
            let bytes = fs::read(&path)
                .with_context(|| format!("read native source {}", path.display()))?;
            let after = fs::metadata(&path)
                .with_context(|| format!("reinspect native source {}", path.display()))?;
            validate_source_metadata(src, &path, &after)?;
            if before.len() != after.len() || modified_ns(&before) != modified_ns(&after) {
                anyhow::bail!(
                    "native source changed while it was being captured: {}",
                    path.display()
                );
            }
            store_object(tx, &bytes)
        }
        Some(crate::model::Capture::Bytes { bytes, .. }) => {
            if src
                .bytes
                .is_some_and(|expected| expected != bytes.len() as i64)
            {
                anyhow::bail!(
                    "native source {} length mismatch: metadata says {:?}, capture has {} bytes",
                    src.locator,
                    src.bytes,
                    bytes.len()
                );
            }
            store_object(tx, bytes)
        }
        None => anyhow::bail!(
            "lossless ingest requires capture bytes for native source {}",
            src.locator
        ),
    }
}

fn validate_source_metadata(
    src: &NativeSource,
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<()> {
    if src
        .bytes
        .is_some_and(|expected| expected != metadata.len() as i64)
    {
        anyhow::bail!(
            "native source {} length changed before capture: expected {:?}, found {}",
            path.display(),
            src.bytes,
            metadata.len()
        );
    }
    if src
        .mtime_ns
        .is_some_and(|expected| Some(expected) != modified_ns(metadata))
    {
        anyhow::bail!(
            "native source {} modification time changed before capture",
            path.display()
        );
    }
    #[cfg(unix)]
    if src.mode.is_some_and(|expected| {
        use std::os::unix::fs::PermissionsExt;
        expected != metadata.permissions().mode()
    }) {
        anyhow::bail!(
            "native source {} permissions changed before capture",
            path.display()
        );
    }
    Ok(())
}

fn modified_ns(metadata: &fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
}

fn store_object(tx: &Transaction<'_>, bytes: &[u8]) -> Result<Option<String>> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hex::encode(hasher.finalize());
    if tx
        .query_row("SELECT 1 FROM objects WHERE hash=?1", [&hash], |_| Ok(()))
        .optional()?
        .is_some()
    {
        return Ok(Some(hash));
    }
    let compressed = zstd::encode_all(bytes, 3)?;
    tx.execute("INSERT OR IGNORE INTO objects(hash,compression,bytes,payload,created_at_ms) VALUES(?1,'zstd',?2,?3,?4)", params![hash,bytes.len() as i64,compressed,now_ms()])?;
    Ok(Some(hash))
}

/// The authoritative SQL definition of every session aggregate that is a
/// projection of other tables.
///
/// `reindex` applies it to repair and `verify` compares against it to report
/// drift, so the repair and the check can never disagree.
const AGGREGATE_PROJECTION: &str = "
  event_count=(SELECT count(*) FROM events e WHERE e.session_id=sessions.id),
  turn_count=(SELECT count(*) FROM events e WHERE e.session_id=sessions.id AND e.kind IN ('user','assistant')),
  child_count=(SELECT count(*) FROM sessions c WHERE c.parent_session_id=sessions.id),
  tool_call_count=(SELECT count(*) FROM events e WHERE e.session_id=sessions.id AND e.kind='tool_call'),
  error_count=(SELECT count(*) FROM events e WHERE e.session_id=sessions.id AND e.is_error=1),
  first_user_text=(SELECT e.text FROM events e WHERE e.session_id=sessions.id AND e.kind='user' ORDER BY e.idx LIMIT 1),
  last_assistant_text=(SELECT e.text FROM events e WHERE e.session_id=sessions.id AND e.kind='assistant' ORDER BY e.idx DESC LIMIT 1),
  source_count=(SELECT count(*) FROM raw_sources r WHERE r.session_id=sessions.id),
  source_bytes=(SELECT coalesce(sum(coalesce(r.bytes,0)),0) FROM raw_sources r WHERE r.session_id=sessions.id),
  latest_source_mtime_ns=(SELECT max(r.mtime_ns) FROM raw_sources r WHERE r.session_id=sessions.id),
  sort_time=coalesce(ended_at_ms,started_at_ms,ingested_at_ms)
";

/// Recompute every materialized session aggregate from the underlying rows.
///
/// The stored previews are truncated to [`crate::model::PREVIEW_LIMIT`] after
/// the SQL pass so repair reproduces the ingest-time value exactly.
pub fn rebuild_aggregates(conn: &Connection) -> Result<u64> {
    let repaired = conn.execute(&format!("UPDATE sessions SET {AGGREGATE_PROJECTION}"), [])? as u64;
    truncate_stored_previews(conn)?;
    Ok(repaired)
}

/// Re-derive every session's spans from its stored event stream.
///
/// Spans are a deterministic projection of events, so `reindex` restores them
/// alongside the search index and the session aggregates. The rewrite also
/// refreshes each event's `span_id`, because the projection owns both sides of
/// that link.
pub fn rebuild_spans(conn: &mut Connection) -> Result<u64> {
    let session_ids = conn
        .prepare("SELECT id FROM sessions ORDER BY id")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let tx = conn.transaction()?;
    for session_id in &session_ids {
        let mut events = load_events(&tx, session_id, &EventWindow::default())?;
        let spans = derive_spans(&mut events);
        tx.execute("DELETE FROM spans WHERE session_id=?1", [session_id])?;
        for span in &spans {
            insert_span(&tx, session_id, span)?;
        }
        for event in &events {
            tx.execute(
                "UPDATE events SET span_id=?3 WHERE session_id=?1 AND idx=?2",
                params![session_id, event.idx, event.span_id],
            )?;
        }
    }
    tx.commit()?;
    Ok(session_ids.len() as u64)
}

/// Apply the shared preview budget to the previews the SQL pass copied whole.
fn truncate_stored_previews(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT id,first_user_text,last_assistant_text FROM sessions
         WHERE length(first_user_text)>?1 OR length(last_assistant_text)>?1",
    )?;
    let overlong = statement
        .query_map([crate::model::PREVIEW_LIMIT as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for (id, ask, outcome) in overlong {
        conn.execute(
            "UPDATE sessions SET first_user_text=?2,last_assistant_text=?3 WHERE id=?1",
            params![
                id,
                ask.map(|text| crate::model::preview(&text, crate::model::PREVIEW_LIMIT)),
                outcome.map(|text| crate::model::preview(&text, crate::model::PREVIEW_LIMIT)),
            ],
        )?;
    }
    Ok(())
}

/// Count sessions whose stored aggregates disagree with the underlying rows.
fn aggregate_drift(connection: &Connection) -> Result<Vec<crate::VerificationFailure>> {
    let mut statement = connection.prepare(
        "SELECT s.id,s.event_count,s.child_count,s.turn_count,s.tool_call_count,s.sort_time,
                (SELECT count(*) FROM events e WHERE e.session_id=s.id),
                (SELECT count(*) FROM sessions c WHERE c.parent_session_id=s.id),
                (SELECT count(*) FROM events e WHERE e.session_id=s.id AND e.kind IN ('user','assistant')),
                (SELECT count(*) FROM events e WHERE e.session_id=s.id AND e.kind='tool_call'),
                coalesce(s.ended_at_ms,s.started_at_ms,s.ingested_at_ms)
         FROM sessions s
         WHERE s.event_count <> (SELECT count(*) FROM events e WHERE e.session_id=s.id)
            OR s.child_count <> (SELECT count(*) FROM sessions c WHERE c.parent_session_id=s.id)
            OR s.turn_count <> (SELECT count(*) FROM events e WHERE e.session_id=s.id AND e.kind IN ('user','assistant'))
            OR s.tool_call_count <> (SELECT count(*) FROM events e WHERE e.session_id=s.id AND e.kind='tool_call')
            OR s.sort_time <> coalesce(s.ended_at_ms,s.started_at_ms,s.ingested_at_ms)
         LIMIT 100",
    )?;
    let failures = statement
        .query_map([], |row| {
            let id = row.get::<_, String>(0)?;
            let stored = [
                ("event_count", row.get::<_, i64>(1)?, row.get::<_, i64>(6)?),
                ("child_count", row.get::<_, i64>(2)?, row.get::<_, i64>(7)?),
                ("turn_count", row.get::<_, i64>(3)?, row.get::<_, i64>(8)?),
                (
                    "tool_call_count",
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(9)?,
                ),
                ("sort_time", row.get::<_, i64>(5)?, row.get::<_, i64>(10)?),
            ];
            let detail = stored
                .iter()
                .filter(|(_, stored, actual)| stored != actual)
                .map(|(name, stored, actual)| format!("{name} stored {stored}, actual {actual}"))
                .collect::<Vec<_>>()
                .join("; ");
            Ok(crate::VerificationFailure {
                locator: id,
                message: format!(
                    "materialized aggregates disagree with stored rows ({detail}); run `trace-db reindex` to repair"
                ),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(failures)
}

pub fn rebuild_fts(conn: &Connection) -> Result<()> {
    let tokenizer: String = conn.query_row(
        "SELECT value FROM schema_meta WHERE key='tokenizer'",
        [],
        |row| row.get(0),
    )?;
    if tokenizer != PORTABLE_TOKENIZER && tokenizer != JIEBA_TOKENIZER {
        anyhow::bail!("unsupported stored tokenizer: {tokenizer}");
    }
    // Recreate the virtual table transactionally because external-content
    // FTS5's generic rebuild command would index tool-result and usage rows.
    let rebuild = conn.execute_batch(&format!(
        "BEGIN IMMEDIATE;
         DROP TABLE events_fts;
         CREATE VIRTUAL TABLE events_fts USING fts5(
           text, content='events', content_rowid='id', tokenize='{tokenizer}'
         );
         INSERT INTO events_fts(rowid,text)
         SELECT id,text FROM events WHERE kind NOT IN ('tool_result','usage');
         COMMIT;"
    ));
    if let Err(error) = rebuild {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(error.into());
    }
    Ok(())
}

pub fn reconstruct_manifest(
    conn: &Connection,
    session_id: &str,
    out_dir: &Path,
    options: ReconstructionOptions,
) -> Result<crate::RestoreManifest> {
    let canonical_out_dir = canonicalize_with_missing(out_dir)
        .with_context(|| format!("resolve reconstruction output {}", out_dir.display()))?;
    let mut stmt = conn.prepare("SELECT locator,restore_path,object_hash,mtime_ns,mode FROM raw_sources WHERE session_id=?1 AND object_hash IS NOT NULL ORDER BY locator")?;
    let rows = stmt.query_map([session_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    })?;
    let mut planned = Vec::new();
    let mut targets = std::collections::HashSet::new();
    for row in rows {
        let (locator, restore, hash, mtime_ns, source_mode) = row?;
        let rel = Path::new(&restore);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            anyhow::bail!("unsafe restore path: {restore}");
        }
        let target = out_dir.join(rel);
        let canonical_target = canonicalize_with_missing(&target)
            .with_context(|| format!("resolve reconstruction target {}", target.display()))?;
        if !canonical_target.starts_with(&canonical_out_dir) {
            anyhow::bail!(
                "restore target resolves outside output directory: {}",
                target.display()
            );
        }
        if !targets.insert(target.clone()) {
            anyhow::bail!("duplicate restore target: {}", target.display());
        }
        if target.exists() && !options.overwrite {
            anyhow::bail!(
                "restore target already exists: {} (use --overwrite to replace it)",
                target.display()
            );
        }
        let (compression, expected_bytes, payload) = conn
            .query_row(
                "SELECT compression,bytes,payload FROM objects WHERE hash=?1",
                [&hash],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .with_context(|| format!("load archived object {hash} for {locator}"))?;
        if compression != "zstd" {
            anyhow::bail!("unsupported compression {compression:?} for archived object {hash}");
        }
        let bytes = zstd::decode_all(payload.as_slice())
            .with_context(|| format!("decompress archived object {hash} for {locator}"))?;
        if i64::try_from(bytes.len()).ok() != Some(expected_bytes) {
            anyhow::bail!(
                "archived object {hash} length mismatch: expected {expected_bytes}, decoded {}",
                bytes.len()
            );
        }
        let actual_hash = hex::encode(Sha256::digest(&bytes));
        if actual_hash != hash {
            anyhow::bail!(
                "archived object {hash} SHA-256 mismatch: decoded object hashes to {actual_hash}"
            );
        }
        planned.push((target, locator, hash, bytes, mtime_ns, source_mode));
    }

    for (target, _, _, _, _, _) in &planned {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut files = Vec::with_capacity(planned.len());
    for (target, locator, hash, bytes, mtime_ns, source_mode) in planned {
        let parent = target.parent().unwrap_or(out_dir);
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary restore file in {}", parent.display()))?;
        temporary.write_all(&bytes)?;
        temporary.as_file_mut().sync_all()?;
        #[cfg(unix)]
        if let Some(source_mode) = source_mode.as_ref() {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(*source_mode as u32))?;
        }
        if options.overwrite {
            temporary
                .persist(&target)
                .map_err(|error| error.error)
                .with_context(|| format!("atomically replace {}", target.display()))?;
        } else {
            temporary
                .persist_noclobber(&target)
                .map_err(|error| error.error)
                .with_context(|| format!("atomically create {}", target.display()))?;
        }
        if let Some(mtime_ns) = mtime_ns {
            filetime::set_file_mtime(
                &target,
                filetime::FileTime::from_unix_time(
                    mtime_ns.div_euclid(1_000_000_000),
                    mtime_ns.rem_euclid(1_000_000_000) as u32,
                ),
            )?;
        }
        files.push(crate::RestoreManifestFile {
            path: target,
            locator,
            object_hash: hash,
            bytes: bytes.len() as u64,
            mode: source_mode.and_then(|mode| u32::try_from(mode).ok()),
            mtime_ns,
        });
    }
    Ok(crate::RestoreManifest {
        schema_version: crate::RESTORE_MANIFEST_SCHEMA_VERSION.into(),
        session_id: session_id.into(),
        output_dir: out_dir.to_path_buf(),
        files,
    })
}

pub fn reconstruct(
    conn: &Connection,
    session_id: &str,
    out_dir: &Path,
    options: ReconstructionOptions,
) -> Result<Vec<PathBuf>> {
    Ok(reconstruct_manifest(conn, session_id, out_dir, options)?
        .files
        .into_iter()
        .map(|file| file.path)
        .collect())
}

pub fn stats(conn: &Connection) -> Result<Vec<(String, i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT agent,count(*),coalesce(sum(event_count),0) FROM sessions GROUP BY agent ORDER BY agent",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn list(conn: &Connection, request: &ListRequest) -> Result<ListPage> {
    let limit = request.limit.clamp(1, 500);
    let cursor = request
        .cursor
        .as_deref()
        .map(decode_list_cursor)
        .transpose()?;
    // Every projected value is a stored column, so the planner serves this from
    // `sessions_sort_idx` without correlated subqueries or a sort pass.
    let mut sql = String::from(
        "SELECT s.id,s.agent,s.cwd,s.started_at_ms,s.ended_at_ms,s.status,s.title,s.model,s.provider,
                s.parent_session_id,s.parent_relation,s.child_count,s.event_count,
                s.ingested_at_ms,s.fingerprint,s.turn_count,s.tool_call_count,s.error_count,
                s.input_tokens,s.output_tokens,s.total_tokens,s.sort_time
         FROM sessions s WHERE 1=1",
    );
    let mut values = Vec::<rusqlite::types::Value>::new();
    let mut bind = |fragment: &str, value: rusqlite::types::Value| {
        sql.push_str(fragment);
        values.push(value);
    };
    if let Some(agent) = request.agent {
        bind(" AND s.agent=?", agent.as_str().to_owned().into());
    }
    if let Some(cwd) = &request.cwd {
        if request.cwd_exact {
            let normalized = normalize_cwd(cwd);
            bind(
                " AND (CASE WHEN s.cwd='/' THEN '/' ELSE rtrim(s.cwd,'/') END)=?",
                normalized.into(),
            );
        } else {
            bind(" AND s.cwd LIKE '%' || ? || '%'", cwd.clone().into());
        }
    }
    if let Some(since_ms) = request.since_ms {
        bind(" AND s.sort_time>=?", since_ms.into());
    }
    if let Some(model) = &request.model {
        bind(" AND s.model=?", model.clone().into());
    }
    if let Some(provider) = &request.provider {
        bind(" AND s.provider=?", provider.clone().into());
    }
    if request.collapse_lineage {
        sql.push_str(
            " AND NOT EXISTS (
                SELECT 1 FROM sessions parent
                WHERE parent.id=s.parent_session_id",
        );
        if request.agent.is_some() {
            sql.push_str(" AND parent.agent=s.agent");
        }
        if let Some(cwd) = &request.cwd {
            if request.cwd_exact {
                sql.push_str(
                    " AND (CASE WHEN parent.cwd='/' THEN '/' ELSE rtrim(parent.cwd,'/') END)=?",
                );
                values.push(normalize_cwd(cwd).into());
            } else {
                sql.push_str(" AND parent.cwd LIKE '%' || ? || '%'");
                values.push(cwd.clone().into());
            }
        }
        if let Some(since_ms) = request.since_ms {
            sql.push_str(" AND parent.sort_time>=?");
            values.push(since_ms.into());
        }
        if request.model.is_some() {
            sql.push_str(" AND parent.model=s.model");
        }
        if request.provider.is_some() {
            sql.push_str(" AND parent.provider=s.provider");
        }
        sql.push(')');
    }
    if let Some((sort_time, id)) = cursor {
        sql.push_str(" AND (s.sort_time<? OR (s.sort_time=? AND s.id>?))");
        values.push(sort_time.into());
        values.push(sort_time.into());
        values.push(id.into());
    }
    sql.push_str(" ORDER BY s.sort_time DESC,s.id ASC LIMIT ?");
    values.push(((limit + 1) as i64).into());
    let mut statement = conn.prepare(&sql)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(values), |row| {
            Ok((
                SessionSummary {
                    id: row.get(0)?,
                    agent: row
                        .get::<_, String>(1)?
                        .parse()
                        .map_err(|message: String| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Text,
                                message.into(),
                            )
                        })?,
                    cwd: row.get(2)?,
                    started_at_ms: row.get(3)?,
                    ended_at_ms: row.get(4)?,
                    status: row
                        .get::<_, Option<String>>(5)?
                        .map(|value| value.parse())
                        .transpose()
                        .map_err(|error: String| {
                            rusqlite::Error::FromSqlConversionFailure(
                                5,
                                rusqlite::types::Type::Text,
                                error.into(),
                            )
                        })?,
                    title: row.get(6)?,
                    model: row.get(7)?,
                    provider: row.get(8)?,
                    parent_session_id: row.get(9)?,
                    parent_relation: row
                        .get::<_, Option<String>>(10)?
                        .map(|value| value.parse())
                        .transpose()
                        .map_err(|error: String| {
                            rusqlite::Error::FromSqlConversionFailure(
                                10,
                                rusqlite::types::Type::Text,
                                error.into(),
                            )
                        })?,
                    subagent_count: row.get(11)?,
                    events: row.get(12)?,
                    ingested_at_ms: row.get(13)?,
                    fingerprint: row.get(14)?,
                    turns: row.get(15)?,
                    tool_calls: row.get(16)?,
                    errors: row.get(17)?,
                    input_tokens: row.get(18)?,
                    output_tokens: row.get(19)?,
                    total_tokens: row.get(20)?,
                },
                row.get::<_, i64>(21)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = rows.len() > limit;
    let mut rows = rows.into_iter().take(limit).collect::<Vec<_>>();
    let next_cursor = if has_more {
        rows.last()
            .map(|(session, sort_time)| encode_list_cursor(*sort_time, &session.id))
    } else {
        None
    };
    Ok(ListPage {
        sessions: rows.drain(..).map(|(session, _)| session).collect(),
        next_cursor,
    })
}

pub fn coverage(conn: &Connection, session_id: &str) -> Result<Option<SessionCoverage>> {
    // A single indexed row read: the counts and the newest source mtime are
    // materialized at ingest, so coverage never touches events or raw_sources.
    conn.query_row(
        "SELECT s.id,s.fingerprint,s.ingested_at_ms,s.event_count,s.source_count,
                s.latest_source_mtime_ns,s.source_bytes
         FROM sessions s WHERE s.id=?1",
        [session_id],
        |row| {
            Ok(SessionCoverage {
                id: row.get(0)?,
                fingerprint: row.get(1)?,
                ingested_at_ms: row.get(2)?,
                events: row.get(3)?,
                sources: row.get(4)?,
                latest_source_mtime_ns: row.get(5)?,
                source_bytes: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn normalize_cwd(cwd: &str) -> String {
    if cwd == "/" {
        return cwd.to_owned();
    }
    cwd.trim_end_matches('/').to_owned()
}

fn encode_list_cursor(sort_time: i64, id: &str) -> String {
    format!("{sort_time}:{}", hex::encode(id.as_bytes()))
}

fn decode_list_cursor(cursor: &str) -> Result<(i64, String)> {
    let (sort_time, id) = cursor
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid list cursor"))?;
    let sort_time = sort_time
        .parse::<i64>()
        .context("invalid list cursor time")?;
    let id = String::from_utf8(hex::decode(id).context("invalid list cursor id")?)
        .context("list cursor id is not UTF-8")?;
    Ok((sort_time, id))
}

/// The event slice a `show` call asks for.
///
/// Filtering lives in SQL so a windowed read never deserializes — or even
/// fetches — the rest of the session. The API boundary validates the bounds;
/// this type only carries them.
#[derive(Debug, Clone, Default)]
pub struct EventWindow {
    pub from_idx: Option<i64>,
    pub to_idx: Option<i64>,
    pub kinds: Vec<crate::EventKind>,
}

/// Load a session's normalized event stream in index order.
///
/// Shared by `show` and by `reindex`'s span repair so both observe exactly the
/// same projection of the stored rows.
fn load_events(conn: &Connection, session_id: &str, window: &EventWindow) -> Result<Vec<Event>> {
    let mut sql = String::from(
        "SELECT idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,parent_kind,span_id,
                model,provider,usage_json,text,data_json,created_at_ms,ended_at_ms
         FROM events WHERE session_id=?1",
    );
    let mut values = Vec::<rusqlite::types::Value>::new();
    values.push(session_id.to_owned().into());
    if let Some(from) = window.from_idx {
        values.push(from.into());
        sql.push_str(&format!(" AND idx>=?{}", values.len()));
    }
    if let Some(to) = window.to_idx {
        values.push(to.into());
        sql.push_str(&format!(" AND idx<=?{}", values.len()));
    }
    if !window.kinds.is_empty() {
        let placeholders = window
            .kinds
            .iter()
            .map(|kind| {
                values.push(kind.as_str().to_owned().into());
                format!("?{}", values.len())
            })
            .collect::<Vec<_>>()
            .join(",");
        sql.push_str(&format!(" AND kind IN ({placeholders})"));
    }
    sql.push_str(" ORDER BY idx");
    let mut statement = conn.prepare(&sql)?;
    let raw = statement
        .query_map(rusqlite::params_from_iter(values), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, String>(14)?,
                row.get::<_, Option<String>>(15)?,
                row.get::<_, Option<i64>>(16)?,
                row.get::<_, Option<i64>>(17)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(
            |(
                idx,
                kind,
                subtype,
                role,
                name,
                call_id,
                is_error,
                native_id,
                parent_id,
                parent_kind,
                span_id,
                model,
                provider,
                usage_json,
                text,
                data_json,
                created_at_ms,
                ended_at_ms,
            )| {
                Ok(Event {
                    idx,
                    kind: kind.parse().map_err(anyhow::Error::msg)?,
                    subtype,
                    role,
                    name,
                    call_id,
                    is_error: is_error.map(|value| value != 0),
                    native_id,
                    parent_id,
                    parent_kind: parent_kind
                        .map(|value| value.parse::<crate::EventParentKind>())
                        .transpose()
                        .map_err(anyhow::Error::msg)?,
                    span_id,
                    model,
                    provider,
                    usage: usage_json
                        .map(|json| serde_json::from_str::<TokenUsage>(&json))
                        .transpose()?,
                    text,
                    data_json: data_json
                        .map(|json| serde_json::from_str::<Value>(&json))
                        .transpose()?,
                    created_at_ms,
                    ended_at_ms,
                })
            },
        )
        .collect()
}

pub fn show(
    conn: &Connection,
    session_id: &str,
    window: &EventWindow,
) -> Result<Option<SessionTrace>> {
    let row = conn
        .query_row(
            "SELECT agent,cwd,started_at_ms,ended_at_ms,status,title,model,provider,git_branch,parent_session_id,parent_relation,fork_point_native_id,fingerprint,meta_json FROM sessions WHERE id=?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                ))
            },
        )
        .optional()?;
    let Some((
        agent,
        cwd,
        started_at_ms,
        ended_at_ms,
        status,
        title,
        model,
        provider,
        git_branch,
        parent_session_id,
        parent_relation,
        fork_point_native_id,
        fingerprint,
        meta_json,
    )) = row
    else {
        return Ok(None);
    };

    let mut source_stmt = conn.prepare("SELECT locator,kind,restore_path,role,bytes,mtime_ns,mode FROM raw_sources WHERE session_id=?1 ORDER BY locator")?;
    let sources = source_stmt
        .query_map([session_id], |row| {
            Ok(NativeSource {
                locator: row.get(0)?,
                kind: row.get(1)?,
                restore_path: row.get(2)?,
                role: row.get(3)?,
                bytes: row.get(4)?,
                mtime_ns: row.get(5)?,
                mode: row.get::<_, Option<i64>>(6)?.map(|value| value as u32),
                capture: None,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let events = load_events(conn, session_id, window)?;

    // A span whose interval overlaps the requested event range is context the
    // caller needs; one entirely outside it is not. `kinds` deliberately does
    // not constrain spans: an event-kind filter selects which events to read,
    // not which trajectories exist, and a tool span is exactly the context that
    // makes a `kinds=user` slice interpretable.
    let mut span_sql = String::from(
        "SELECT id,parent_span_id,kind,name,native_id,call_id,status,started_at_ms,ended_at_ms,start_event_idx,end_event_idx,data_json
         FROM spans WHERE session_id=?1",
    );
    let mut span_values = Vec::<rusqlite::types::Value>::new();
    span_values.push(session_id.to_owned().into());
    if let Some(to) = window.to_idx {
        span_values.push(to.into());
        span_sql.push_str(&format!(
            " AND (start_event_idx IS NULL OR start_event_idx<=?{})",
            span_values.len()
        ));
    }
    if let Some(from) = window.from_idx {
        span_values.push(from.into());
        span_sql.push_str(&format!(
            " AND (coalesce(end_event_idx,start_event_idx) IS NULL
                   OR coalesce(end_event_idx,start_event_idx)>=?{})",
            span_values.len()
        ));
    }
    span_sql.push_str(" ORDER BY id");
    let mut span_statement = conn.prepare(&span_sql)?;
    let raw_spans = span_statement
        .query_map(rusqlite::params_from_iter(span_values), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let spans = raw_spans
        .into_iter()
        .map(
            |(
                id,
                parent_span_id,
                kind,
                name,
                native_id,
                call_id,
                status,
                started_at_ms,
                ended_at_ms,
                start_event_idx,
                end_event_idx,
                data_json,
            )| {
                Ok(Span {
                    id,
                    parent_span_id,
                    kind: kind.parse().map_err(anyhow::Error::msg)?,
                    name,
                    native_id,
                    call_id,
                    status: status
                        .map(|value| value.parse())
                        .transpose()
                        .map_err(anyhow::Error::msg)?,
                    started_at_ms,
                    ended_at_ms,
                    start_event_idx,
                    end_event_idx,
                    data_json: data_json
                        .map(|value| serde_json::from_str(&value))
                        .transpose()?,
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(SessionTrace {
        session: Session {
            id: session_id.to_owned(),
            agent: agent.parse().map_err(anyhow::Error::msg)?,
            cwd,
            started_at_ms,
            ended_at_ms,
            status: status
                .map(|value: String| value.parse::<crate::SessionStatus>())
                .transpose()
                .map_err(anyhow::Error::msg)?,
            title,
            model,
            provider,
            git_branch,
            parent_session_id,
            parent_relation: parent_relation
                .map(|value| value.parse::<crate::SessionRelation>())
                .transpose()
                .map_err(anyhow::Error::msg)?,
            fork_point_native_id,
            meta: serde_json::from_str(&meta_json)?,
            fingerprint,
            sources,
        },
        events,
        spans,
    }))
}

/// Canonicalize a path while preserving not-yet-created trailing components.
///
/// This lets reconstruction validate symlinked ancestors without creating any
/// output before every archived object has passed preflight validation.
fn canonicalize_with_missing(path: &Path) -> Result<PathBuf> {
    let mut missing = Vec::new();
    let mut existing = path;
    while !existing.exists() {
        let name = existing
            .file_name()
            .context("reconstruction path has no filename component")?
            .to_os_string();
        missing.push(name);
        existing = existing
            .parent()
            .context("reconstruction path has no existing ancestor")?;
    }
    let mut canonical = fs::canonicalize(existing)?;
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agent, Capture, Event, EventKind, NativeSource, ParsedSession, Session};
    use crate::{search, SearchRequest};
    use tempfile::tempdir;

    fn session(path: &Path) -> ParsedSession {
        ParsedSession {
            session: Session {
                id: "codex:test".into(),
                agent: Agent::Codex,
                cwd: Some("/tmp".into()),
                started_at_ms: Some(1),
                ended_at_ms: Some(2),
                status: None,
                title: None,
                model: None,
                provider: None,
                git_branch: None,
                parent_session_id: None,
                parent_relation: None,
                fork_point_native_id: None,
                meta: serde_json::json!({}),
                fingerprint: "v1".into(),
                sources: vec![NativeSource {
                    locator: path.display().to_string(),
                    kind: "jsonl".into(),
                    restore_path: "rollout.jsonl".into(),
                    role: None,
                    bytes: None,
                    mtime_ns: None,
                    mode: None,
                    capture: Some(Capture::File {
                        path: path.display().to_string(),
                    }),
                }],
            },
            events: vec![
                Event::new(EventKind::User, "部署 tokenizer"),
                Event::new(EventKind::ToolResult, "noisy secret"),
            ],
        }
    }

    fn named_session(id: &str, text: &str) -> ParsedSession {
        let mut parsed = session(Path::new("/tmp/native.jsonl"));
        parsed.session.id = id.into();
        parsed.session.fingerprint = id.into();
        parsed.session.sources.clear();
        parsed.events = vec![Event::new(EventKind::User, text)];
        parsed
    }

    /// `child_count` is the one aggregate the child's own event stream cannot
    /// answer, so it must converge regardless of arrival order and must not
    /// drift when the same child is ingested again.
    #[test]
    fn child_count_converges_under_out_of_order_and_repeat_ingest() {
        let dir = tempdir().unwrap();
        let mut conn = open(dir.path().join("trace.db")).unwrap();

        let child_count = |conn: &Connection, id: &str| -> i64 {
            conn.query_row(
                "SELECT child_count FROM sessions WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };

        // Child first: the parent does not exist yet, so nothing can be
        // incremented. The count must still be right once the parent lands.
        let mut child = named_session("codex:child", "delegated work");
        child.session.parent_session_id = Some("codex:parent".into());
        child.session.parent_relation = Some(crate::SessionRelation::Subagent);
        upsert(&mut conn, child.clone()).unwrap();
        upsert(&mut conn, named_session("codex:parent", "host work")).unwrap();
        assert_eq!(child_count(&conn, "codex:parent"), 1);

        // Re-ingesting the same child must not double-count it.
        upsert(&mut conn, child.clone()).unwrap();
        assert_eq!(child_count(&conn, "codex:parent"), 1);

        // A second distinct child is counted.
        let mut sibling = named_session("codex:sibling", "more delegated work");
        sibling.session.parent_session_id = Some("codex:parent".into());
        sibling.session.parent_relation = Some(crate::SessionRelation::Subagent);
        upsert(&mut conn, sibling).unwrap();
        assert_eq!(child_count(&conn, "codex:parent"), 2);

        // Re-parenting repairs both the old and the new parent.
        upsert(&mut conn, named_session("codex:other", "another host")).unwrap();
        let mut moved = child;
        moved.session.parent_session_id = Some("codex:other".into());
        upsert(&mut conn, moved).unwrap();
        assert_eq!(child_count(&conn, "codex:parent"), 1);
        assert_eq!(child_count(&conn, "codex:other"), 1);

        // The materialized state agrees with the underlying rows.
        assert!(aggregate_drift(&conn).unwrap().is_empty());
    }

    /// Reindex is the repair path for derived state, and verify is the detector.
    #[test]
    fn reindex_repairs_aggregate_drift_that_verify_reports() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("trace.db");
        let mut conn = open(&db_path).unwrap();
        upsert(&mut conn, named_session("codex:drift", "hello")).unwrap();
        assert!(aggregate_drift(&conn).unwrap().is_empty());

        // Corrupt the materialized projection behind the store's back.
        conn.execute(
            "UPDATE sessions SET event_count=99,turn_count=99 WHERE id='codex:drift'",
            [],
        )
        .unwrap();
        let drift = aggregate_drift(&conn).unwrap();
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].locator, "codex:drift");
        let report = verify(&conn, &db_path).unwrap();
        assert!(!report.passed);
        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "session_aggregates" && !check.failures.is_empty()));

        rebuild_aggregates(&conn).unwrap();
        assert!(aggregate_drift(&conn).unwrap().is_empty());
        assert!(verify(&conn, &db_path).unwrap().passed);
    }

    /// The stored previews must reproduce what retrieval used to compute, and
    /// repair must apply the same budget as ingest.
    #[test]
    fn stored_previews_are_truncated_to_the_shared_budget() {
        let dir = tempdir().unwrap();
        let mut conn = open(dir.path().join("trace.db")).unwrap();
        let long = "x".repeat(crate::model::PREVIEW_LIMIT + 200);
        let mut parsed = named_session("codex:long", &long);
        parsed.events.push(Event::new(EventKind::Assistant, &long));
        upsert(&mut conn, parsed).unwrap();

        let stored = |conn: &Connection, column: &str| -> String {
            conn.query_row(
                &format!("SELECT {column} FROM sessions WHERE id='codex:long'"),
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };
        for column in ["first_user_text", "last_assistant_text"] {
            let value = stored(&conn, column);
            assert_eq!(value.chars().count(), crate::model::PREVIEW_LIMIT + 1);
            assert!(value.ends_with('\u{2026}'));
        }

        // Repair reproduces the ingest-time value rather than the raw text.
        rebuild_aggregates(&conn).unwrap();
        for column in ["first_user_text", "last_assistant_text"] {
            let value = stored(&conn, column);
            assert_eq!(value.chars().count(), crate::model::PREVIEW_LIMIT + 1);
            assert!(value.ends_with('\u{2026}'));
        }
    }

    #[test]
    fn every_ingest_reconstructs_a_byte_identical_source() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("native.jsonl");
        fs::write(&src, "原始\n").unwrap();
        let db_path = dir.path().join("trace.db");
        let mut conn = open(&db_path).unwrap();
        upsert(&mut conn, session(&src)).unwrap();
        upsert(&mut conn, session(&src)).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM objects", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let out = dir.path().join("out");
        reconstruct(&conn, "codex:test", &out, ReconstructionOptions::default()).unwrap();
        assert_eq!(
            fs::read(out.join("rollout.jsonl")).unwrap(),
            fs::read(&src).unwrap()
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM events_fts WHERE events_fts MATCH 'secret'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        rebuild_fts(&conn).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM events_fts WHERE events_fts MATCH 'secret'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        conn.execute(
            "INSERT INTO events_fts(events_fts,rank) VALUES('integrity-check',0)",
            [],
        )
        .unwrap();
        assert!(verify(&conn, &db_path).unwrap().passed);
    }

    #[test]
    fn reingest_preserves_a_previous_snapshot_when_a_source_disappears() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.jsonl");
        let second = dir.path().join("second.json");
        fs::write(&first, "first\n").unwrap();
        fs::write(&second, "second\n").unwrap();
        let db_path = dir.path().join("trace.db");
        let mut conn = open(&db_path).unwrap();
        let mut initial = session(&first);
        initial.session.sources.push(NativeSource {
            locator: second.display().to_string(),
            kind: "json".into(),
            restore_path: "second.json".into(),
            role: Some("sidecar".into()),
            bytes: None,
            mtime_ns: None,
            mode: None,
            capture: Some(Capture::File {
                path: second.display().to_string(),
            }),
        });
        upsert(&mut conn, initial).unwrap();
        upsert(&mut conn, session(&first)).unwrap();

        let out = dir.path().join("out");
        reconstruct(&conn, "codex:test", &out, ReconstructionOptions::default()).unwrap();
        assert_eq!(fs::read(out.join("rollout.jsonl")).unwrap(), b"first\n");
        assert_eq!(fs::read(out.join("second.json")).unwrap(), b"second\n");
    }

    #[test]
    fn upsert_many_is_atomic_before_best_effort_fallback() {
        let dir = tempdir().unwrap();
        let mut conn = open(dir.path().join("trace.db")).unwrap();
        let mut invalid = session(&dir.path().join("missing.jsonl"));
        invalid.session.id = "codex:invalid".into();
        let mut batch = vec![named_session("codex:valid", "hello"), invalid];
        assert!(upsert_many(&mut conn, &mut batch).is_err());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM sessions", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn migration_refuses_any_archive_whose_schema_version_differs() {
        for stored in ["4", "99"] {
            let dir = tempdir().unwrap();
            let conn = Connection::open(dir.path().join("other.db")).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO schema_meta VALUES ('schema_version','{stored}');"
            ))
            .unwrap();
            let error = migrate(&conn).unwrap_err().to_string();
            assert!(
                error.contains("re-run `trace-db ingest`"),
                "unexpected error for stored version {stored}: {error}"
            );
        }
    }

    #[test]
    fn configured_read_only_open_requires_the_jieba_extension() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace.db");
        open(&path).unwrap();
        let error = open_read_only_configured(&path, TokenizerKind::Jieba, None).unwrap_err();
        assert!(error
            .to_string()
            .contains("jieba tokenizer requires a configured extension path"));
    }

    #[test]
    fn migration_rebuilds_fts_when_the_tokenizer_contract_changes() {
        let dir = tempdir().unwrap();
        let conn = open(dir.path().join("tokenizer.db")).unwrap();
        conn.execute(
            "INSERT INTO sessions(id,agent,fingerprint,meta_json,ingested_at_ms) VALUES ('codex:tokenizer','codex','v1','{}',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events(session_id,idx,kind,text) VALUES ('codex:tokenizer',0,'user','café deploy')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO schema_meta(key,value) VALUES ('tokenizer','jieba')",
            [],
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT value FROM schema_meta WHERE key='tokenizer'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "unicode61 remove_diacritics 2"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM events_fts WHERE events_fts MATCH 'cafe'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn search_orders_stronger_bm25_hit_first() {
        let dir = tempdir().unwrap();
        let mut conn = open(dir.path().join("trace.db")).unwrap();
        upsert(&mut conn, named_session("codex:weak", "deploy")).unwrap();
        upsert(
            &mut conn,
            named_session("codex:strong", "deploy netlify production deploy"),
        )
        .unwrap();
        let rows = search::search(&conn, &SearchRequest::new("deploy netlify")).unwrap();
        assert_eq!(
            rows.first().map(|row| row.id.as_str()),
            Some("codex:strong")
        );
    }

    #[test]
    fn search_collapses_parent_and_child_lineage() {
        let dir = tempdir().unwrap();
        let mut conn = open(dir.path().join("trace.db")).unwrap();
        upsert(&mut conn, named_session("codex:parent", "deploy netlify")).unwrap();
        let mut child = named_session("codex:child", "deploy netlify deploy");
        child.session.parent_session_id = Some("codex:parent".into());
        child.session.parent_relation = Some(crate::SessionRelation::Subagent);
        upsert(&mut conn, child).unwrap();
        let rows = search::search(&conn, &SearchRequest::new("deploy netlify")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hits, 2);
    }

    /// A tool call/result pair is a span, and the read path must return the
    /// persisted rows rather than re-deriving them. Reindex re-derives spans
    /// from events, so deleting them is repairable and verify reports the gap.
    #[test]
    fn spans_are_persisted_and_reindex_repairs_them() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trace.db");
        let mut conn = open(&path).unwrap();

        let mut parsed = named_session("codex:spans", "run the build");
        let mut call = Event::new(EventKind::ToolCall, "cargo build");
        call.name = Some("Bash".into());
        call.call_id = Some("call-1".into());
        let mut result = Event::new(EventKind::ToolResult, "ok");
        result.call_id = Some("call-1".into());
        parsed.events.push(call);
        parsed.events.push(result);
        upsert(&mut conn, parsed).unwrap();

        // Ingest materialized the span, and show reads it back from storage.
        let stored: i64 = conn
            .query_row(
                "SELECT count(*) FROM spans WHERE session_id='codex:spans'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, 1);
        let trace = show(&conn, "codex:spans", &EventWindow::default())
            .unwrap()
            .unwrap();
        assert_eq!(trace.spans.len(), 1);
        assert_eq!(trace.spans[0].call_id.as_deref(), Some("call-1"));
        assert!(trace
            .events
            .iter()
            .any(|event| event.span_id.as_deref() == Some("call:call-1")));

        // Losing the span rows is visible to verify and is not silently papered
        // over by the read path.
        conn.execute("DELETE FROM spans WHERE session_id='codex:spans'", [])
            .unwrap();
        assert!(show(&conn, "codex:spans", &EventWindow::default())
            .unwrap()
            .unwrap()
            .spans
            .is_empty());
        let report = verify(&conn, &path).unwrap();
        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "spans" && !check.failures.is_empty()));

        // Reindex re-derives them from the stored events.
        rebuild_spans(&mut conn).unwrap();
        assert_eq!(
            show(&conn, "codex:spans", &EventWindow::default())
                .unwrap()
                .unwrap()
                .spans
                .len(),
            1
        );
        let report = verify(&conn, &path).unwrap();
        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "spans" && check.failures.is_empty()));
    }

    /// A windowed `show` must return only the events in range, plus the spans
    /// whose interval overlaps that range — a tool span is the context that
    /// makes a slice interpretable, so `kinds` filters events, not spans.
    #[test]
    fn show_windows_events_and_intersecting_spans() {
        let dir = tempdir().unwrap();
        let mut conn = open(dir.path().join("trace.db")).unwrap();

        let mut parsed = named_session("codex:window", "start");
        for group in 0..4 {
            let mut call = Event::new(EventKind::ToolCall, format!("call {group}"));
            call.name = Some("Bash".into());
            call.call_id = Some(format!("c{group}"));
            let mut result = Event::new(EventKind::ToolResult, format!("done {group}"));
            result.call_id = Some(format!("c{group}"));
            parsed.events.push(call);
            parsed.events.push(result);
        }
        upsert(&mut conn, parsed).unwrap();

        // Events 1..=2 are the first call/result pair, so only span c0 overlaps.
        let window = EventWindow {
            from_idx: Some(1),
            to_idx: Some(2),
            kinds: Vec::new(),
        };
        let trace = show(&conn, "codex:window", &window).unwrap().unwrap();
        assert_eq!(trace.events.len(), 2);
        assert!(trace.events.iter().all(|e| (1..=2).contains(&e.idx)));
        assert_eq!(trace.spans.len(), 1);
        assert_eq!(trace.spans[0].call_id.as_deref(), Some("c0"));

        // A kind filter narrows events without hiding the spans that explain them.
        let kinded = EventWindow {
            from_idx: Some(1),
            to_idx: Some(4),
            kinds: vec![EventKind::ToolCall],
        };
        let trace = show(&conn, "codex:window", &kinded).unwrap().unwrap();
        assert!(trace.events.iter().all(|e| e.kind == EventKind::ToolCall));
        assert_eq!(trace.events.len(), 2);
        assert_eq!(trace.spans.len(), 2);

        // An unfiltered read is unchanged: every event and every span.
        let all = show(&conn, "codex:window", &EventWindow::default())
            .unwrap()
            .unwrap();
        assert_eq!(all.events.len(), 9);
        assert_eq!(all.spans.len(), 4);
    }
}
