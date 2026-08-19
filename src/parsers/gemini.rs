use super::{Parser, SessionCandidate};
use crate::model::{
    compact, flatten, Agent, Capture, Event, EventKind, NativeSource, ParsedSession, Session,
};
use anyhow::Result;
use chrono::DateTime;
use serde_json::{json, Value};
use std::{fs, path::Path};
use walkdir::WalkDir;

pub struct GeminiParser;
fn s(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}
fn ts(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_str)
        .and_then(|x| DateTime::parse_from_rfc3339(x).ok())
        .map(|x| x.timestamp_millis())
}
fn parse(path: &Path, root: &Path, candidate: &SessionCandidate) -> Result<ParsedSession> {
    let text = fs::read_to_string(path)?;
    let mut rows = Vec::new();
    let mut sid = None;
    let mut start = None;
    let mut end = None;
    let mut model = None;
    for line in text.lines() {
        let Ok(v): Result<Value, _> = serde_json::from_str(line) else {
            continue;
        };
        if let Some(x) = s(v.get("sessionId")) {
            sid = Some(x);
            start = start.or(ts(v.get("startTime")));
            end = ts(v.get("lastUpdated")).or(end);
            // Gemini's older store is one JSON object with an embedded
            // `messages` array rather than a change log.
            if let Some(messages) = v.get("messages").and_then(Value::as_array) {
                rows.extend(messages.iter().cloned());
            }
            continue;
        }
        if let Some(set) = v.get("$set") {
            if let Some(ms) = set.get("messages").and_then(Value::as_array) {
                rows.extend(ms.iter().cloned());
            }
            continue;
        }
        if matches!(
            v.get("type").and_then(Value::as_str),
            Some("user") | Some("gemini") | Some("info") | Some("error")
        ) {
            rows.push(v);
        }
    }
    let sid = sid.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or("unknown")
            .into()
    });
    let mut events = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in rows {
        let typ = r.get("type").and_then(Value::as_str).unwrap_or("");
        let id = s(r.get("id"));
        if !seen.insert(id.clone().unwrap_or_default()) {
            continue;
        }
        let t = ts(r.get("timestamp"));
        if start.is_none() {
            start = t;
        }
        if t.is_some() {
            end = t;
        }
        match typ {
            "user" => {
                let txt = flatten(
                    r.get("content")
                        .or_else(|| r.get("message"))
                        .unwrap_or(&Value::Null),
                );
                if !txt.starts_with("<session_context>") {
                    let mut e = Event::new(EventKind::User, txt);
                    e.native_id = id;
                    e.created_at_ms = t;
                    events.push(e)
                }
            }
            "gemini" => {
                let mut e = Event::new(
                    EventKind::Assistant,
                    flatten(
                        r.get("content")
                            .or_else(|| r.get("message"))
                            .unwrap_or(&Value::Null),
                    ),
                );
                e.native_id = id.clone();
                e.created_at_ms = t;
                e.model = s(r.get("model"));
                model = model.or(e.model.clone());
                events.push(e);
                if let Some(arr) = r.get("thoughts").and_then(Value::as_array) {
                    for x in arr {
                        let mut q = Event::new(EventKind::Thinking, compact(x));
                        q.native_id = id.clone();
                        q.created_at_ms = t;
                        events.push(q)
                    }
                }
                if let Some(arr) = r.get("toolCalls").and_then(Value::as_array) {
                    for x in arr {
                        let mut q =
                            Event::new(EventKind::ToolCall, compact(x.get("args").unwrap_or(x)));
                        q.name = s(x.get("name"));
                        q.call_id = s(x.get("callId").or_else(|| x.get("id")));
                        q.native_id = id.clone();
                        q.created_at_ms = t;
                        let qname = q.name.clone();
                        let qcall = q.call_id.clone();
                        events.push(q);
                        if x.get("result").is_some() {
                            let mut z = Event::new(
                                EventKind::ToolResult,
                                compact(x.get("result").unwrap()),
                            );
                            z.call_id = qcall;
                            z.name = qname;
                            z.native_id = id.clone();
                            z.created_at_ms = t;
                            events.push(z)
                        }
                    }
                }
            }
            "info" | "error" => {
                let mut e = Event::new(
                    EventKind::System,
                    flatten(
                        r.get("content")
                            .or_else(|| r.get("message"))
                            .unwrap_or(&Value::Null),
                    ),
                );
                e.subtype = Some(typ.into());
                e.native_id = id;
                e.created_at_ms = t;
                events.push(e)
            }
            _ => {}
        }
    }
    let restore = path
        .strip_prefix(root)
        .ok()
        .and_then(|x| x.to_str())
        .unwrap_or_else(|| {
            path.file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("session.jsonl")
        })
        .replace(std::path::MAIN_SEPARATOR, "/");
    let source = NativeSource {
        locator: path.display().to_string(),
        kind: "jsonl".into(),
        restore_path: restore,
        role: None,
        bytes: candidate.bytes,
        mtime_ns: candidate.mtime_ns,
        mode: None,
        capture: Some(Capture::File {
            path: path.display().to_string(),
        }),
    };
    Ok(ParsedSession {
        session: Session {
            id: format!("gemini:{sid}"),
            agent: Agent::Gemini,
            cwd: None,
            started_at_ms: start,
            ended_at_ms: end,
            title: None,
            model,
            provider: Some("google".into()),
            git_branch: None,
            parent_session_id: None,
            forked_from: None,
            meta: json!({}),
            fingerprint: format!("{}:{}", events.len(), end.unwrap_or_default()),
            sources: vec![source],
        },
        events,
    })
}
impl Parser for GeminiParser {
    fn agent(&self) -> Agent {
        Agent::Gemini
    }
    fn discover(&self, root: &Path) -> Result<Vec<SessionCandidate>> {
        let mut out = Vec::new();
        if !root.exists() {
            return Ok(out);
        };
        for e in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if e.file_type().is_file() && e.file_name().to_string_lossy().starts_with("session-") {
                if let Ok(candidate) = SessionCandidate::file(e.path().to_path_buf()) {
                    out.push(candidate);
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, candidate: &SessionCandidate, root: &Path) -> Result<Option<ParsedSession>> {
        Ok(Some(parse(&candidate.path, root, candidate)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn reads_legacy_json_message_array() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session-legacy.json");
        fs::write(
            &path,
            serde_json::json!({
                "sessionId": "g-1",
                "startTime": "2026-08-19T00:00:00Z",
                "lastUpdated": "2026-08-19T00:00:02Z",
                "messages": [
                    {"id":"u","type":"user","content":"hello"},
                    {"id":"a","type":"gemini","content":"world"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let parsed = GeminiParser.parse_all(dir.path()).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0]
                .events
                .iter()
                .filter(|e| e.kind == EventKind::User)
                .count(),
            1
        );
        assert_eq!(
            parsed[0]
                .events
                .iter()
                .filter(|e| e.kind == EventKind::Assistant)
                .count(),
            1
        );
    }
}
