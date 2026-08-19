use crate::{
    model::{assign_indexes, Event, IngestMode, NativeSource, ParsedSession, Session, TokenUsage},
    SessionTrace,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SCHEMA_VERSION: i64 = 1;

pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    if let Some(parent) = path
        .as_ref()
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000i64)?;
    let jieba = if let Some(ext) = std::env::var_os("TRACEDB_JIEBA_EXT") {
        // Loading is explicitly opt-in because dynamic libraries are a
        // deployment concern. The bundled release can point this at the
        // `fts5-jieba` cdylib produced by `cargo build -p fts5-jieba --release`.
        let loaded = unsafe {
            conn.load_extension_enable()
                .and_then(|_| {
                    conn.load_extension(PathBuf::from(ext), Some("sqlite3_fts5jieba_init"))
                })
                .and_then(|r| conn.load_extension_disable().map(|_| r))
        };
        loaded.is_ok()
    } else {
        false
    };
    migrate_with_tokenizer(&conn, jieba)?;
    Ok(conn)
}

pub fn migrate(conn: &Connection) -> Result<()> {
    migrate_with_tokenizer(conn, false)
}

fn migrate_with_tokenizer(conn: &Connection, jieba: bool) -> Result<()> {
    let tokenizer = if jieba {
        "jieba"
    } else {
        "unicode61 remove_diacritics 2"
    };
    let schema = r#"
      CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
      INSERT OR IGNORE INTO schema_meta(key,value) VALUES ('schema_version','1');
      CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY, agent TEXT NOT NULL, cwd TEXT, started_at_ms INTEGER,
        ended_at_ms INTEGER, title TEXT, model TEXT, provider TEXT, git_branch TEXT,
        parent_session_id TEXT, forked_from TEXT, mode TEXT NOT NULL DEFAULT 'partial',
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
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta(key,value) VALUES('tokenizer',?1)",
        [tokenizer],
    )?;
    conn.execute("INSERT OR REPLACE INTO schema_meta(key,value) VALUES('archive_contract','partial-v1/full-v1')", [])?;
    Ok(())
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

fn write_session(
    tx: &Transaction<'_>,
    session: &Session,
    events: &[Event],
    mode: IngestMode,
) -> Result<()> {
    tx.execute("INSERT INTO sessions(id,agent,cwd,started_at_ms,ended_at_ms,title,model,provider,git_branch,parent_session_id,forked_from,mode,fingerprint,meta_json,ingested_at_ms)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
                ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,cwd=excluded.cwd,started_at_ms=excluded.started_at_ms,ended_at_ms=excluded.ended_at_ms,title=excluded.title,model=excluded.model,provider=excluded.provider,git_branch=excluded.git_branch,parent_session_id=excluded.parent_session_id,forked_from=excluded.forked_from,mode=excluded.mode,fingerprint=excluded.fingerprint,meta_json=excluded.meta_json,ingested_at_ms=excluded.ingested_at_ms",
        params![session.id, session.agent.as_str(), session.cwd, session.started_at_ms, session.ended_at_ms, session.title, session.model, session.provider, session.git_branch, session.parent_session_id, session.forked_from, mode.to_string(), session.fingerprint, session.meta.to_string(), now_ms()])?;
    tx.execute("DELETE FROM raw_sources WHERE session_id=?1", [&session.id])?;
    for src in &session.sources {
        let object_hash = if matches!(mode, IngestMode::Full) {
            capture_source(tx, src)?
        } else {
            None
        };
        tx.execute("INSERT INTO raw_sources(session_id,locator,kind,restore_path,role,bytes,mtime_ns,mode,object_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![session.id,src.locator,src.kind,src.restore_path,src.role,src.bytes,src.mtime_ns,src.mode.map(|v|v as i64),object_hash])?;
    }
    tx.execute("DELETE FROM events WHERE session_id=?1", [&session.id])?;
    for e in events {
        tx.execute("INSERT INTO events(session_id,idx,kind,subtype,role,name,call_id,is_error,native_id,parent_id,model,provider,usage_json,text,data_json,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)", params![session.id,e.idx,e.kind.as_str(),e.subtype,e.role,e.name,e.call_id,e.is_error.map(i64::from),e.native_id,e.parent_id,e.model,e.provider,e.usage.as_ref().map(|v|serde_json::to_string(v).unwrap()),e.text,e.data_json.as_ref().map(Value::to_string),e.created_at_ms])?;
    }
    Ok(())
}

fn capture_source(tx: &Transaction<'_>, src: &NativeSource) -> Result<Option<String>> {
    let path = match &src.capture {
        Some(crate::model::Capture::File { path }) => PathBuf::from(path),
        Some(crate::model::Capture::Bytes { bytes, .. }) => return store_object(tx, bytes),
        None => return Ok(None),
    };
    let bytes =
        fs::read(&path).with_context(|| format!("read native source {}", path.display()))?;
    store_object(tx, &bytes)
}

fn store_object(tx: &Transaction<'_>, bytes: &[u8]) -> Result<Option<String>> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = hex::encode(hasher.finalize());
    let compressed = zstd::encode_all(bytes, 3)?;
    tx.execute("INSERT OR IGNORE INTO objects(hash,compression,bytes,payload,created_at_ms) VALUES(?1,'zstd',?2,?3,?4)", params![hash,bytes.len() as i64,compressed,now_ms()])?;
    Ok(Some(hash))
}

pub fn rebuild_fts(conn: &Connection) -> Result<()> {
    conn.execute_batch("INSERT INTO events_fts(events_fts) VALUES('delete-all'); INSERT INTO events_fts(rowid,text) SELECT id,text FROM events WHERE kind NOT IN ('tool_result','usage');")?;
    Ok(())
}

pub fn reconstruct(conn: &Connection, session_id: &str, out_dir: &Path) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(out_dir)?;
    let mut stmt = conn.prepare("SELECT locator,restore_path,object_hash FROM raw_sources WHERE session_id=?1 AND object_hash IS NOT NULL ORDER BY locator")?;
    let rows = stmt.query_map([session_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut written = Vec::new();
    for row in rows {
        let (_locator, restore, hash) = row?;
        let payload: Vec<u8> =
            conn.query_row("SELECT payload FROM objects WHERE hash=?1", [&hash], |r| {
                r.get(0)
            })?;
        let bytes = zstd::decode_all(payload.as_slice())?;
        let rel = Path::new(&restore);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            anyhow::bail!("unsafe restore path: {restore}");
        }
        let target = out_dir.join(rel);
        if let Some(p) = target.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&target, bytes)?;
        written.push(target);
    }
    Ok(written)
}

pub fn stats(conn: &Connection) -> Result<Vec<(String, i64, i64, i64)>> {
    let mut stmt=conn.prepare("SELECT agent,count(*),coalesce(sum((SELECT count(*) FROM events e WHERE e.session_id=s.id)),0),coalesce(sum(mode='full'),0) FROM sessions s GROUP BY agent ORDER BY agent")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
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
        reconstruct(&conn, "codex:test", &out).unwrap();
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
