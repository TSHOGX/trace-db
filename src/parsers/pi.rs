use super::Parser;
use crate::model::{
    compact, flatten, Agent, Capture, Event, EventKind, NativeSource, ParsedSession, Session,
};
use anyhow::Result;
use chrono::DateTime;
use serde_json::{json, Value};
use std::{fs, path::Path};
use walkdir::WalkDir;

pub struct PiParser;
fn s(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}
fn ts(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_str)
        .and_then(|x| DateTime::parse_from_rfc3339(x).ok())
        .map(|x| x.timestamp_millis())
        .or_else(|| v.and_then(Value::as_i64).map(|x| x / 1_000))
}
fn parse(path: &Path, root: &Path) -> Result<ParsedSession> {
    let text = fs::read_to_string(path)?;
    let rows: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let head = rows
        .iter()
        .find(|r| r.get("type").and_then(Value::as_str) == Some("session"))
        .unwrap_or(&Value::Null);
    let id = s(head.get("id")).unwrap_or_else(|| {
        path.file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or("unknown")
            .into()
    });
    let cwd = s(head.get("cwd"));
    let mut model = None;
    let mut provider = None;
    let mut start = ts(head.get("timestamp"));
    let mut end = start;
    let mut events = Vec::new();
    for r in &rows {
        let typ = r.get("type").and_then(Value::as_str).unwrap_or("");
        let t = ts(r.get("timestamp"));
        if start.is_none() {
            start = t;
        }
        if t.is_some() {
            end = t;
        }
        match typ {
            "model_change" => {
                model = model.or(s(r.get("modelId")));
                provider = provider.or(s(r.get("provider")));
                let mut e = Event::new(EventKind::System, compact(r));
                e.subtype = Some(typ.into());
                e.native_id = s(r.get("id"));
                e.parent_id = s(r.get("parentId"));
                e.created_at_ms = t;
                events.push(e)
            }
            "thinking_level_change" => {
                let mut e = Event::new(EventKind::System, compact(r));
                e.subtype = Some(typ.into());
                e.native_id = s(r.get("id"));
                e.parent_id = s(r.get("parentId"));
                e.created_at_ms = t;
                events.push(e)
            }
            "message" => {
                let m = r.get("message").unwrap_or(&Value::Null);
                let role = s(m.get("role")).unwrap_or_default();
                let base_id = s(r.get("id"));
                let parent = s(r.get("parentId"));
                let mt = ts(m.get("timestamp")).or(t);
                if role == "toolResult" {
                    let mut e = Event::new(
                        EventKind::ToolResult,
                        flatten(m.get("content").unwrap_or(&Value::Null)),
                    );
                    e.name = s(m.get("toolName"));
                    e.call_id = s(m.get("toolCallId"));
                    e.is_error = m.get("isError").and_then(Value::as_bool);
                    e.native_id = base_id;
                    e.parent_id = parent;
                    e.created_at_ms = mt;
                    events.push(e)
                } else {
                    let kind = if role == "user" {
                        EventKind::User
                    } else {
                        EventKind::Assistant
                    };
                    if let Some(arr) = m.get("content").and_then(Value::as_array) {
                        for b in arr {
                            let typ = b.get("type").and_then(Value::as_str).unwrap_or("text");
                            let k = match typ {
                                "thinking" => EventKind::Thinking,
                                "toolCall" => EventKind::ToolCall,
                                "text" => kind,
                                _ => EventKind::System,
                            };
                            let mut e = Event::new(
                                k,
                                if k == EventKind::ToolCall {
                                    compact(b.get("arguments").unwrap_or(b))
                                } else {
                                    flatten(b)
                                },
                            );
                            e.name = s(b.get("name"));
                            e.call_id = s(b.get("toolCallId").or_else(|| b.get("id")));
                            e.subtype = Some(typ.into());
                            e.native_id = base_id.clone();
                            e.parent_id = parent.clone();
                            e.created_at_ms = mt;
                            events.push(e)
                        }
                    } else {
                        let mut e = Event::new(kind, flatten(m.get("content").unwrap_or(m)));
                        e.native_id = base_id;
                        e.parent_id = parent;
                        e.created_at_ms = mt;
                        events.push(e)
                    }
                }
            }
            _ => {}
        }
    }
    let md = fs::metadata(path)?;
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
        bytes: Some(md.len() as i64),
        mtime_ns: None,
        mode: None,
        capture: Some(Capture::File {
            path: path.display().to_string(),
        }),
    };
    Ok(ParsedSession {
        session: Session {
            id: format!("pi:{id}"),
            agent: Agent::Pi,
            cwd,
            started_at_ms: start,
            ended_at_ms: end,
            title: None,
            model,
            provider,
            git_branch: None,
            parent_session_id: None,
            forked_from: None,
            meta: json!({}),
            fingerprint: format!("{}:{}", rows.len(), end.unwrap_or_default()),
            sources: vec![source],
        },
        events,
    })
}
impl Parser for PiParser {
    fn agent(&self) -> Agent {
        Agent::Pi
    }
    fn discover(&self, root: &Path) -> Result<Vec<ParsedSession>> {
        let mut out = Vec::new();
        if !root.exists() {
            return Ok(out);
        };
        for e in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl") {
                if let Ok(s) = parse(e.path(), root) {
                    if s.session.cwd.as_deref() != Some("/tmp")
                        && s.session.provider.as_deref() != Some("faux")
                    {
                        out.push(s)
                    }
                }
            }
        }
        Ok(out)
    }
}
