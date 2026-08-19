use super::{Parser, SessionCandidate};
use crate::model::{
    compact, Agent, Capture, Event, EventKind, NativeSource, ParsedSession, Session,
};
use anyhow::Result;
use rusqlite::Connection;
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
fn j(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
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
        let md = j(&r.get::<_, String>(1)?);
        let pd: Option<String> = r.get(3)?;
        let role = md
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("assistant");
        let t = r.get::<_, Option<i64>>(4)?.or(r.get(2)?);
        if let Some(pds) = pd {
            let p = j(&pds);
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
        messages.push(json!({"id":r.get::<_,String>(0)?,"session_id":r.get::<_,String>(1)?,"time_created":r.get::<_,i64>(2)?,"time_updated":r.get::<_,i64>(3)?,"data":j(&r.get::<_,String>(4)?)}));
    }
    let mut parts = Vec::new();
    let mut psq=connection.prepare("SELECT id,message_id,session_id,time_created,time_updated,data FROM part WHERE session_id=?1 ORDER BY time_created,id")?;
    let mut pr = psq.query([id])?;
    while let Some(r) = pr.next()? {
        parts.push(json!({"id":r.get::<_,String>(0)?,"message_id":r.get::<_,String>(1)?,"session_id":r.get::<_,String>(2)?,"time_created":r.get::<_,i64>(3)?,"time_updated":r.get::<_,i64>(4)?,"data":j(&r.get::<_,String>(5)?)}));
    }
    let envelope = json!({"format":"trace-db/opencode-session-v1","session":{"id":sid,"parent_id":parent,"directory":directory,"title":title,"agent":agent,"model":model,"time_created":created,"time_updated":updated},"message":messages,"part":parts});
    let bytes = serde_json::to_vec(&envelope)?;
    let src = NativeSource {
        locator: format!("{}#{id}", db.display()),
        kind: "sqlite-session".into(),
        restore_path: format!("{sid}.json"),
        role: None,
        bytes: candidate.bytes,
        mtime_ns: candidate.mtime_ns,
        mode: None,
        capture: Some(Capture::Bytes {
            label: sid.clone(),
            bytes,
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
            sources: vec![src],
        },
        events: evs,
    })
}
impl Parser for OpenCodeParser {
    fn agent(&self) -> Agent {
        Agent::OpenCode
    }
    fn discover(&self, root: &Path) -> Result<Vec<SessionCandidate>> {
        let Some(db) = db_path(root) else {
            return Ok(vec![]);
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
                parent_session_id: None,
                agent_type: None,
            });
        }
        Ok(out)
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
