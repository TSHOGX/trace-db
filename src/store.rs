use crate::{
    config::TokenizerKind,
    model::{assign_indexes, Event, IngestMode, NativeSource, ParsedSession, Session, TokenUsage},
    IngestReport, ListPage, ListRequest, ReconstructionOptions, SessionSummary, SessionTrace,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SCHEMA_VERSION: i64 = 1;
pub const ARCHIVE_CONTRACT: &str = "lossless-v1";
pub const PORTABLE_TOKENIZER: &str = "unicode61 remove_diacritics 2";
pub const JIEBA_TOKENIZER: &str = "jieba";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateState {
    pub fingerprint: String,
    pub mode: IngestMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredIngestStatus {
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
    Ok(conn)
}

pub fn open_read_only(path: &Path) -> Result<Connection> {
    if !path.exists() {
        anyhow::bail!("TraceDB archive does not exist: {}", path.display());
    }
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "busy_timeout", 5000i64)?;
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
    let result = (|| -> Result<crate::ImportReport> {
        connection.execute_batch("BEGIN IMMEDIATE")?;
        validate_import_compatibility(connection)?;
        let imported_sessions = connection.execute(
            "INSERT OR IGNORE INTO sessions(id,agent,cwd,started_at_ms,ended_at_ms,title,model,provider,git_branch,parent_session_id,forked_from,mode,fingerprint,meta_json,ingested_at_ms)
             SELECT id,agent,cwd,started_at_ms,ended_at_ms,title,model,provider,git_branch,parent_session_id,forked_from,mode,fingerprint,meta_json,ingested_at_ms
             FROM import_source.sessions",
            [],
        )? as u64;
        connection.execute(
            "UPDATE sessions
             SET mode='full'
             WHERE mode <> 'full'
               AND id IN (SELECT id FROM import_source.sessions WHERE mode='full')",
            [],
        )?;
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
            "INSERT INTO events(session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,model,provider,usage_json,text,data_json,created_at_ms)
             SELECT ie.session_id,ie.idx,ie.kind,ie.subtype,ie.role,ie.name,ie.call_id,ie.is_error,ie.native_id,ie.parent_id,ie.model,ie.provider,ie.usage_json,ie.text,ie.data_json,ie.created_at_ms
             FROM import_source.events ie
             WHERE NOT EXISTS (
               SELECT 1 FROM events e
               WHERE e.session_id=ie.session_id AND e.idx=ie.idx
                 AND COALESCE(e.native_id,'')=COALESCE(ie.native_id,'')
             )",
            [],
        )? as u64;
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
        connection.execute_batch("COMMIT")?;
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
               destination.title IS source.title AND destination.model IS source.model AND
               destination.provider IS source.provider AND destination.git_branch IS source.git_branch AND
               destination.parent_session_id IS source.parent_session_id AND destination.forked_from IS source.forked_from AND
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
                 SELECT session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,model,provider,usage_json,text,data_json,created_at_ms
                 FROM import_source.events WHERE session_id IN (SELECT id FROM sessions)
                 EXCEPT
                 SELECT session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,model,provider,usage_json,text,data_json,created_at_ms
                 FROM events
               )
               UNION ALL
               SELECT * FROM (
                 SELECT session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,model,provider,usage_json,text,data_json,created_at_ms
                 FROM events WHERE session_id IN (SELECT id FROM import_source.sessions)
                 EXCEPT
                 SELECT session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,model,provider,usage_json,text,data_json,created_at_ms
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
pub fn record_ingest_status(conn: &Connection, report: &IngestReport) -> Result<()> {
    let previous: Option<String> = conn
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
    let status = serde_json::json!({
        "completedAtMs": now_ms(),
        "discovered": report.total_discovered(),
        "ingested": report.total_ingested(),
        "skipped": report.total_skipped(),
        "failed": report.total_failed(),
        "cumulativeFailed": cumulative_failed,
    });
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta(key,value) VALUES('ingest.last_status',?1)",
        [status.to_string()],
    )?;
    Ok(())
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
    let version = stored_version
        .as_deref()
        .unwrap_or("0")
        .parse::<i64>()
        .context("invalid TraceDB schema version")?;
    if version > SCHEMA_VERSION {
        anyhow::bail!(
            "TraceDB schema version {version} is newer than supported version {SCHEMA_VERSION}"
        );
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
        ended_at_ms INTEGER, title TEXT, model TEXT, provider TEXT, git_branch TEXT,
        parent_session_id TEXT, forked_from TEXT, mode TEXT NOT NULL DEFAULT 'full',
        fingerprint TEXT NOT NULL, meta_json TEXT NOT NULL, ingested_at_ms INTEGER NOT NULL
      );
      CREATE INDEX IF NOT EXISTS sessions_agent_idx ON sessions(agent);
      CREATE INDEX IF NOT EXISTS sessions_ended_idx ON sessions(ended_at_ms);
      CREATE INDEX IF NOT EXISTS sessions_parent_idx ON sessions(parent_session_id);
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
        call_id TEXT, is_error INTEGER, native_id TEXT, parent_id TEXT, model TEXT,
        provider TEXT, usage_json TEXT, text TEXT NOT NULL, data_json TEXT, created_at_ms INTEGER
      );
      CREATE INDEX IF NOT EXISTS events_session_idx ON events(session_id,idx);
      CREATE INDEX IF NOT EXISTS events_kind_idx ON events(kind);
      CREATE INDEX IF NOT EXISTS events_call_idx ON events(session_id,call_id);
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
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta(key,value) VALUES('archive_contract',?1)",
        [ARCHIVE_CONTRACT],
    )?;
    Ok(())
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

    let contract_failures = verify_contract(connection)?;
    checks.push(VerifyCheck::new("archive_contract", 3, contract_failures));

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
    let expected = [
        ("schema_version", SCHEMA_VERSION.to_string()),
        ("archive_contract", ARCHIVE_CONTRACT.to_owned()),
    ];
    for (key, expected_value) in expected {
        let actual = connection
            .query_row("SELECT value FROM schema_meta WHERE key=?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        if actual.as_deref() != Some(expected_value.as_str()) {
            failures.push(crate::VerificationFailure {
                locator: format!("schema_meta.{key}"),
                message: format!("expected {expected_value:?}, found {actual:?}"),
            });
        }
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
    let legacy_partial: i64 = connection.query_row(
        "SELECT count(*) FROM sessions WHERE mode='partial'",
        [],
        |row| row.get(0),
    )?;
    if legacy_partial != 0 {
        failures.push(crate::VerificationFailure {
            locator: "sessions.mode".into(),
            message: format!(
                "archive contains {legacy_partial} legacy partial session(s) without guaranteed native snapshots"
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

pub fn upsert(
    conn: &mut Connection,
    mut parsed: ParsedSession,
    requested: IngestMode,
) -> Result<()> {
    assign_indexes(&mut parsed.events);
    let tx = conn.transaction()?;
    let old: Option<String> = tx
        .query_row(
            "SELECT mode FROM sessions WHERE id=?1",
            [&parsed.session.id],
            |r| r.get(0),
        )
        .optional()?;
    let mode = requested.retain_full(old.and_then(|s| s.parse().ok()));
    write_session(&tx, &parsed.session, &parsed.events, mode)?;
    tx.commit()?;
    Ok(())
}

pub fn candidate_states(
    conn: &Connection,
    agent: crate::model::Agent,
) -> Result<HashMap<String, CandidateState>> {
    let mut statement = conn.prepare(
        "SELECT r.locator,s.fingerprint,s.mode
         FROM raw_sources r
         JOIN sessions s ON s.id=r.session_id
         WHERE s.agent=?1",
    )?;
    let rows = statement.query_map([agent.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut states = HashMap::new();
    for row in rows {
        let (locator, fingerprint, mode) = row?;
        states.insert(
            locator,
            CandidateState {
                fingerprint,
                mode: mode.parse().map_err(anyhow::Error::msg)?,
            },
        );
    }
    Ok(states)
}

fn write_session(
    tx: &Transaction<'_>,
    session: &Session,
    events: &[Event],
    mode: IngestMode,
) -> Result<()> {
    let previous_full_sources = if matches!(mode, IngestMode::Full) {
        let mut statement = tx.prepare(
            "SELECT locator,kind,restore_path,role,bytes,mtime_ns,mode,object_hash
             FROM raw_sources WHERE session_id=?1 AND object_hash IS NOT NULL",
        )?;
        let rows = statement
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
        rows
    } else {
        Vec::new()
    };
    tx.execute("INSERT INTO sessions(id,agent,cwd,started_at_ms,ended_at_ms,title,model,provider,git_branch,parent_session_id,forked_from,mode,fingerprint,meta_json,ingested_at_ms)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
                ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,cwd=excluded.cwd,started_at_ms=excluded.started_at_ms,ended_at_ms=excluded.ended_at_ms,title=excluded.title,model=excluded.model,provider=excluded.provider,git_branch=excluded.git_branch,parent_session_id=excluded.parent_session_id,forked_from=excluded.forked_from,mode=excluded.mode,fingerprint=excluded.fingerprint,meta_json=excluded.meta_json,ingested_at_ms=excluded.ingested_at_ms",
        params![session.id, session.agent.as_str(), session.cwd, session.started_at_ms, session.ended_at_ms, session.title, session.model, session.provider, session.git_branch, session.parent_session_id, session.forked_from, mode.to_string(), session.fingerprint, session.meta.to_string(), now_ms()])?;
    tx.execute("DELETE FROM raw_sources WHERE session_id=?1", [&session.id])?;
    let mut current_locators = Vec::with_capacity(session.sources.len());
    for src in &session.sources {
        current_locators.push(src.locator.clone());
        let object_hash = if matches!(mode, IngestMode::Full) {
            capture_source(tx, src)?
        } else {
            None
        };
        tx.execute("INSERT INTO raw_sources(session_id,locator,kind,restore_path,role,bytes,mtime_ns,mode,object_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![session.id,src.locator,src.kind,src.restore_path,src.role,src.bytes,src.mtime_ns,src.mode.map(|v|v as i64),object_hash])?;
    }
    for (locator, kind, restore_path, role, bytes, mtime_ns, source_mode, object_hash) in
        previous_full_sources
    {
        if !current_locators.iter().any(|current| current == &locator) {
            tx.execute(
                "INSERT INTO raw_sources(session_id,locator,kind,restore_path,role,bytes,mtime_ns,mode,object_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![session.id, locator, kind, restore_path, role, bytes, mtime_ns, source_mode, object_hash],
            )?;
        }
    }
    tx.execute("DELETE FROM events WHERE session_id=?1", [&session.id])?;
    for e in events {
        let usage_json = e.usage.as_ref().map(serde_json::to_string).transpose()?;
        tx.execute("INSERT INTO events(session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,model,provider,usage_json,text,data_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)", params![session.id,e.idx,e.kind.as_str(),e.subtype,e.role,e.name,e.call_id,e.is_error.map(i64::from),e.native_id,e.parent_id,e.model,e.provider,usage_json,e.text,e.data_json.as_ref().map(Value::to_string),e.created_at_ms])?;
    }
    Ok(())
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

pub fn stats(conn: &Connection) -> Result<Vec<(String, i64, i64, i64)>> {
    let mut stmt=conn.prepare("SELECT agent,count(*),coalesce(sum((SELECT count(*) FROM events e WHERE e.session_id=s.id)),0),coalesce(sum(mode='full'),0) FROM sessions s GROUP BY agent ORDER BY agent")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
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
    let mut sql = String::from(
        "SELECT s.id,s.agent,s.cwd,s.started_at_ms,s.ended_at_ms,s.title,s.model,s.provider,s.mode,
                (SELECT count(*) FROM events e WHERE e.session_id=s.id),s.ingested_at_ms,
                coalesce(s.ended_at_ms,s.started_at_ms,s.ingested_at_ms) AS sort_time
         FROM sessions s WHERE 1=1",
    );
    let mut values = Vec::<rusqlite::types::Value>::new();
    let mode_filter = request.mode;
    let mut bind = |fragment: &str, value: rusqlite::types::Value| {
        sql.push_str(fragment);
        values.push(value);
    };
    if let Some(agent) = request.agent {
        bind(" AND s.agent=?", agent.as_str().to_owned().into());
    }
    if let Some(cwd) = &request.cwd {
        bind(" AND s.cwd LIKE '%' || ? || '%'", cwd.clone().into());
    }
    if let Some(since_ms) = request.since_ms {
        bind(
            " AND coalesce(s.ended_at_ms,s.started_at_ms,s.ingested_at_ms)>=?",
            since_ms.into(),
        );
    }
    if let Some(model) = &request.model {
        bind(" AND s.model=?", model.clone().into());
    }
    if let Some(provider) = &request.provider {
        bind(" AND s.provider=?", provider.clone().into());
    }
    if let Some(mode) = mode_filter {
        if matches!(mode, IngestMode::Partial) {
            sql.push_str(" AND s.mode IN ('partial','full')");
        } else {
            bind(" AND s.mode=?", mode.to_string().into());
        }
    }
    if let Some((sort_time, id)) = cursor {
        sql.push_str(
            " AND (coalesce(s.ended_at_ms,s.started_at_ms,s.ingested_at_ms)<?
                    OR (coalesce(s.ended_at_ms,s.started_at_ms,s.ingested_at_ms)=? AND s.id>?))",
        );
        values.push(sort_time.into());
        values.push(sort_time.into());
        values.push(id.into());
    }
    sql.push_str(" ORDER BY sort_time DESC,s.id ASC LIMIT ?");
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
                    title: row.get(5)?,
                    model: row.get(6)?,
                    provider: row.get(7)?,
                    mode: row
                        .get::<_, String>(8)?
                        .parse()
                        .map_err(|message: String| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                message.into(),
                            )
                        })?,
                    events: row.get(9)?,
                    ingested_at_ms: row.get(10)?,
                },
                row.get::<_, i64>(11)?,
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

pub fn show(conn: &Connection, session_id: &str) -> Result<Option<SessionTrace>> {
    let row = conn
        .query_row(
            "SELECT agent,cwd,started_at_ms,ended_at_ms,title,model,provider,git_branch,parent_session_id,forked_from,mode,fingerprint,meta_json FROM sessions WHERE id=?1",
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
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                ))
            },
        )
        .optional()?;
    let Some((
        agent,
        cwd,
        started_at_ms,
        ended_at_ms,
        title,
        model,
        provider,
        git_branch,
        parent_session_id,
        forked_from,
        mode,
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

    let mut event_stmt = conn.prepare("SELECT idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,model,provider,usage_json,text,data_json,created_at_ms FROM events WHERE session_id=?1 ORDER BY idx")?;
    let raw_events = event_stmt
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
                row.get::<_, String>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, Option<i64>>(14)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let events = raw_events
        .into_iter()
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
                model,
                provider,
                usage_json,
                text,
                data_json,
                created_at_ms,
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
            title,
            model,
            provider,
            git_branch,
            parent_session_id,
            forked_from,
            meta: serde_json::from_str(&meta_json)?,
            fingerprint,
            sources,
        },
        mode: mode.parse().map_err(anyhow::Error::msg)?,
        events,
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
    use crate::model::{
        Agent, Capture, Event, EventKind, IngestMode, NativeSource, ParsedSession, Session,
    };
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
                title: None,
                model: None,
                provider: None,
                git_branch: None,
                parent_session_id: None,
                forked_from: None,
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

    #[test]
    fn full_is_sticky_and_reconstructs_byte_identical_source() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("native.jsonl");
        fs::write(&src, "原始\n").unwrap();
        let db_path = dir.path().join("trace.db");
        let mut conn = open(&db_path).unwrap();
        upsert(&mut conn, session(&src), IngestMode::Full).unwrap();
        upsert(&mut conn, session(&src), IngestMode::Partial).unwrap();
        assert_eq!(
            conn.query_row("SELECT mode FROM sessions WHERE id='codex:test'", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "full"
        );
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
    fn full_reingest_preserves_a_previous_snapshot_when_a_source_disappears() {
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
        upsert(&mut conn, initial, IngestMode::Full).unwrap();
        upsert(&mut conn, session(&first), IngestMode::Full).unwrap();

        let out = dir.path().join("out");
        reconstruct(&conn, "codex:test", &out, ReconstructionOptions::default()).unwrap();
        assert_eq!(fs::read(out.join("rollout.jsonl")).unwrap(), b"first\n");
        assert_eq!(fs::read(out.join("second.json")).unwrap(), b"second\n");
    }

    #[test]
    fn migration_rejects_an_archive_from_a_newer_schema() {
        let dir = tempdir().unwrap();
        let conn = Connection::open(dir.path().join("future.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO schema_meta VALUES ('schema_version','99');",
        )
        .unwrap();
        let error = migrate(&conn).unwrap_err().to_string();
        assert!(error.contains("newer than supported version"));
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
            "INSERT INTO sessions(id,agent,mode,fingerprint,meta_json,ingested_at_ms) VALUES ('codex:tokenizer','codex','partial','v1','{}',0)",
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
        upsert(
            &mut conn,
            named_session("codex:weak", "deploy"),
            IngestMode::Partial,
        )
        .unwrap();
        upsert(
            &mut conn,
            named_session("codex:strong", "deploy netlify production deploy"),
            IngestMode::Partial,
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
        upsert(
            &mut conn,
            named_session("codex:parent", "deploy netlify"),
            IngestMode::Partial,
        )
        .unwrap();
        let mut child = named_session("codex:child", "deploy netlify deploy");
        child.session.parent_session_id = Some("codex:parent".into());
        upsert(&mut conn, child, IngestMode::Partial).unwrap();
        let rows = search::search(&conn, &SearchRequest::new("deploy netlify")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hits, 2);
    }
}
