use super::{Discovery, Parser, SessionCandidate};
use crate::model::{
    compact, Agent, Capture, Event, EventKind, NativeSource, ParsedSession, Session,
};
use anyhow::{Context, Result};
use rusqlite::{types::Value as SqlValue, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct OpenCodeParser;

type NativeSessionRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

fn s(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}
fn ms(v: Option<i64>) -> Option<i64> {
    v
}
fn parse_json(value: &str, locator: &str) -> Result<Value> {
    serde_json::from_str(value).with_context(|| format!("invalid JSON in OpenCode {locator}"))
}
fn db_path(root: &Path) -> Option<PathBuf> {
    if root.is_file() && root.extension().is_some_and(|x| x == "db") {
        Some(root.into())
    } else {
        [root.join("opencode.db"), root.join("data/opencode.db")]
            .into_iter()
            .find(|p| p.exists())
    }
}
fn parse_session(
    connection: &Connection,
    db: &Path,
    id: &str,
    _root: &Path,
    candidate: &SessionCandidate,
) -> Result<ParsedSession> {
    let (sid, parent, directory, title, agent, model, created, updated): NativeSessionRow = connection
        .query_row(
            "SELECT id,parent_id,directory,title,agent,model,time_created,time_updated FROM session WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )?;
    let mut evs = Vec::new();
    let mut stmt=connection.prepare("SELECT m.id,m.data,m.time_created,p.data,p.time_created FROM message m LEFT JOIN part p ON p.message_id=m.id WHERE m.session_id=?1 ORDER BY m.time_created,m.id,p.time_created,p.id")?;
    let mut rows = stmt.query([id])?;
    while let Some(r) = rows.next()? {
        let mid: String = r.get(0)?;
        let md = parse_json(&r.get::<_, String>(1)?, &format!("message {mid}"))?;
        let pd: Option<String> = r.get(3)?;
        let role = md
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("assistant");
        let t = r.get::<_, Option<i64>>(4)?.or(r.get(2)?);
        if let Some(pds) = pd {
            let p = parse_json(&pds, &format!("part for message {mid}"))?;
            let typ = p.get("type").and_then(Value::as_str).unwrap_or("");
            let kind = match typ {
                "text" => {
                    if role == "user" {
                        EventKind::User
                    } else {
                        EventKind::Assistant
                    }
                }
                "reasoning" => EventKind::Thinking,
                "tool" => EventKind::ToolCall,
                "patch" | "step-start" | "step-finish" | "snapshot" | "file" => EventKind::System,
                _ => EventKind::System,
            };
            let mut e = Event::new(
                kind,
                if typ == "tool" {
                    compact(p.get("state").unwrap_or(&p))
                } else {
                    compact(p.get("text").or_else(|| p.get("output")).unwrap_or(&p))
                },
            );
            e.name = s(p.get("tool"));
            e.call_id = s(p.get("callID"));
            e.subtype = Some(typ.into());
            e.native_id = Some(mid.clone());
            e.parent_id = s(md.get("parentID"));
            e.created_at_ms = t;
            evs.push(e)
        }
    }
    let mut messages = Vec::new();
    let mut msq=connection.prepare("SELECT id,session_id,time_created,time_updated,data FROM message WHERE session_id=?1 ORDER BY time_created,id")?;
    let mut mr = msq.query([id])?;
    while let Some(r) = mr.next()? {
        let message_id = r.get::<_, String>(0)?;
        let data = parse_json(&r.get::<_, String>(4)?, &format!("message {message_id}"))?;
        messages.push(json!({"id":message_id,"session_id":r.get::<_,String>(1)?,"time_created":r.get::<_,i64>(2)?,"time_updated":r.get::<_,i64>(3)?,"data":data}));
    }
    let mut parts = Vec::new();
    let mut psq=connection.prepare("SELECT id,message_id,session_id,time_created,time_updated,data FROM part WHERE session_id=?1 ORDER BY time_created,id")?;
    let mut pr = psq.query([id])?;
    while let Some(r) = pr.next()? {
        let part_id = r.get::<_, String>(0)?;
        let data = parse_json(&r.get::<_, String>(5)?, &format!("part {part_id}"))?;
        parts.push(json!({"id":part_id,"message_id":r.get::<_,String>(1)?,"session_id":r.get::<_,String>(2)?,"time_created":r.get::<_,i64>(3)?,"time_updated":r.get::<_,i64>(4)?,"data":data}));
    }
    let envelope = json!({"format":"trace-db/opencode-session-v1","session":{"id":sid,"parent_id":parent,"directory":directory,"title":title,"agent":agent,"model":model,"time_created":created,"time_updated":updated},"message":messages,"part":parts});
    let portable_bytes = serde_json::to_vec(&envelope)?;
    let native_bytes = build_native_bundle(connection, &sid)?;
    let native_source = NativeSource {
        locator: format!("{}#{id}", db.display()),
        kind: "sqlite-session".into(),
        restore_path: format!("{sid}.db"),
        role: None,
        bytes: candidate.bytes,
        mtime_ns: candidate.mtime_ns,
        mode: candidate.mode,
        capture: Some(Capture::Bytes {
            label: sid.clone(),
            bytes: native_bytes,
        }),
    };
    let portable_source = NativeSource {
        locator: format!("{}#{id}:portable", db.display()),
        kind: "portable-json".into(),
        restore_path: format!("{sid}.json"),
        role: Some("portable-fallback".into()),
        bytes: Some(portable_bytes.len() as i64),
        mtime_ns: candidate.mtime_ns,
        mode: candidate.mode,
        capture: Some(Capture::Bytes {
            label: format!("{sid}-portable"),
            bytes: portable_bytes,
        }),
    };
    Ok(ParsedSession {
        session: Session {
            id: format!("opencode:{sid}"),
            agent: Agent::OpenCode,
            cwd: directory,
            started_at_ms: ms(created),
            ended_at_ms: ms(updated).or(created),
            title,
            model,
            provider: None,
            git_branch: None,
            parent_session_id: parent.map(|p| format!("opencode:{p}")),
            forked_from: None,
            meta: json!({"agent":agent}),
            fingerprint: format!("{}:{}", evs.len(), updated.unwrap_or_default()),
            sources: vec![native_source, portable_source],
        },
        events: evs,
    })
}

fn build_native_bundle(source: &Connection, id: &str) -> Result<Vec<u8>> {
    let temporary_directory = tempfile::tempdir()?;
    let path = temporary_directory.path().join("opencode.db");
    let connection = Connection::open(&path)?;
    let user_version: i64 = source.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    connection.pragma_update(None, "user_version", user_version)?;
    clone_schema(source, &connection)?;
    connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
    copy_rows(source, &connection, "migration", None, &[])?;
    let project_id: Option<String> = if table_has_column(source, "session", "project_id")? {
        source
            .query_row("SELECT project_id FROM session WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?
    } else {
        None
    };
    if let Some(project_id) = project_id.as_deref() {
        copy_rows(
            source,
            &connection,
            "project",
            Some("id=?1"),
            &[SqlValue::Text(project_id.into())],
        )?;
    }
    let workspace_id: Option<String> = if table_has_column(source, "session", "workspace_id")? {
        source
            .query_row(
                "SELECT workspace_id FROM session WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten()
    } else {
        None
    };
    if let Some(workspace_id) = workspace_id.as_deref() {
        copy_rows(
            source,
            &connection,
            "workspace",
            Some("id=?1"),
            &[SqlValue::Text(workspace_id.into())],
        )?;
    }
    copy_rows(
        source,
        &connection,
        "session",
        Some("id=?1"),
        &[SqlValue::Text(id.into())],
    )?;
    copy_rows(
        source,
        &connection,
        "message",
        Some("session_id=?1"),
        &[SqlValue::Text(id.into())],
    )?;
    copy_rows(
        source,
        &connection,
        "part",
        Some("session_id=?1"),
        &[SqlValue::Text(id.into())],
    )?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT OR REPLACE INTO schema_meta(key,value) VALUES('format','opencode-native-session-v1'),('version','1');",
    )?;
    connection.execute_batch("PRAGMA foreign_keys=ON;")?;
    connection.execute_batch("VACUUM")?;
    drop(connection);
    Ok(fs::read(path)?)
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn clone_schema(source: &Connection, destination: &Connection) -> Result<()> {
    let mut tables = source.prepare(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let table_sql = tables
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for sql in table_sql {
        destination.execute_batch(&sql)?;
    }
    let mut objects = source.prepare(
        "SELECT sql FROM sqlite_master WHERE type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type,name",
    )?;
    let object_sql = objects
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for sql in object_sql {
        destination.execute_batch(&sql)?;
    }
    Ok(())
}

fn copy_rows(
    source: &Connection,
    destination: &Connection,
    table: &str,
    predicate: Option<&str>,
    values: &[SqlValue],
) -> Result<()> {
    let exists: Option<String> = source
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(());
    }
    let table_sql = quote_identifier(table);
    let mut columns = source.prepare(&format!("PRAGMA table_info({table_sql})"))?;
    let columns = columns
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let quoted_columns = columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(",");
    let predicate = predicate
        .map(|value| format!(" WHERE {value}"))
        .unwrap_or_default();
    let select_sql = format!("SELECT {quoted_columns} FROM {table_sql}{predicate}");
    let mut statement = source.prepare(&select_sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
        (0..columns.len())
            .map(|index| row.get(index))
            .collect::<rusqlite::Result<Vec<SqlValue>>>()
    })?;
    let placeholders = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let insert_sql = format!("INSERT INTO {table_sql} ({quoted_columns}) VALUES ({placeholders})");
    for row in rows {
        destination.execute(&insert_sql, rusqlite::params_from_iter(row?))?;
    }
    Ok(())
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let table_sql = quote_identifier(table);
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table_sql})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns.iter().any(|value| value == column))
}
impl Parser for OpenCodeParser {
    fn agent(&self) -> Agent {
        Agent::OpenCode
    }
    fn discover(&self, root: &Path) -> Result<Discovery> {
        let Some(db) = db_path(root) else {
            return Ok(Discovery::default());
        };
        let metadata = fs::metadata(&db)?;
        let file_candidate = SessionCandidate::file(db.clone())?;
        let c = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut st =
            c.prepare("SELECT id,time_updated,time_created FROM session ORDER BY time_updated")?;
        let rows = st
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for (id, updated, created) in rows {
            out.push(SessionCandidate {
                path: db.clone(),
                locator: format!("{}#{id}", db.display()),
                native_id: Some(id),
                fingerprint: format!("opencode-v1:{}", updated.or(created).unwrap_or_default()),
                updated_at_ms: updated.or(created),
                bytes: Some(metadata.len() as i64),
                mtime_ns: file_candidate.mtime_ns,
                mode: file_candidate.mode,
                parent_session_id: None,
                agent_type: None,
            });
        }
        Ok(Discovery {
            candidates: out,
            failures: Vec::new(),
        })
    }

    fn parse(&self, candidate: &SessionCandidate, root: &Path) -> Result<Option<ParsedSession>> {
        let id = candidate
            .native_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OpenCode candidate missing session id"))?;
        let connection = Connection::open_with_flags(
            &candidate.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        Ok(Some(parse_session(
            &connection,
            &candidate.path,
            id,
            root,
            candidate,
        )?))
    }

    fn parse_many(
        &self,
        candidates: &[SessionCandidate],
        root: &Path,
    ) -> Vec<(SessionCandidate, Result<Option<ParsedSession>>)> {
        let Some(first) = candidates.first() else {
            return Vec::new();
        };
        let connection = match Connection::open_with_flags(
            &first.path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            Ok(connection) => connection,
            Err(error) => {
                let error = anyhow::Error::from(error);
                return candidates
                    .iter()
                    .cloned()
                    .map(|candidate| (candidate, Err(anyhow::anyhow!("{error:#}"))))
                    .collect();
            }
        };
        candidates
            .iter()
            .cloned()
            .map(|candidate| {
                let parsed = candidate
                    .native_id
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("OpenCode candidate missing session id"))
                    .and_then(|id| {
                        parse_session(&connection, &candidate.path, id, root, &candidate).map(Some)
                    });
                (candidate, parsed)
            })
            .collect()
    }
}
