use super::{
    for_each_json_line, set_if_none, Discovery, DiscoveryHints, Parser, SessionCandidate,
    UnsupportedFormat,
};
use crate::model::{
    compact, flatten, Agent, Capture, Event, EventKind, NativeSource, ParsedSession, Session,
};
use anyhow::Result;
use chrono::DateTime;
use serde_json::{json, Value};
use std::path::Path;
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
fn parse(path: &Path, root: &Path, candidate: &SessionCandidate) -> Result<ParsedSession> {
    let mut id = None;
    let mut cwd = None;
    let mut model = None;
    let mut provider = None;
    let mut start = None;
    let mut end = start;
    let mut events = Vec::new();
    let row_count = for_each_json_line(path, |r| {
        let typ = r.get("type").and_then(Value::as_str).unwrap_or("");
        if typ == "session" {
            set_if_none(&mut id, s(r.get("id")));
            set_if_none(&mut cwd, s(r.get("cwd")));
        }
        let t = ts(r.get("timestamp"));
        if start.is_none() {
            start = t;
        }
        if t.is_some() {
            end = t;
        }
        match typ {
            "model_change" => {
                set_if_none(&mut model, s(r.get("modelId")));
                set_if_none(&mut provider, s(r.get("provider")));
                let mut e = Event::new(EventKind::System, compact(&r));
                e.subtype = Some(typ.into());
                e.native_id = s(r.get("id"));
                e.parent_id = s(r.get("parentId"));
                e.created_at_ms = t;
                e.data_json = Some(r.clone());
                events.push(e)
            }
            "thinking_level_change" => {
                let mut e = Event::new(EventKind::System, compact(&r));
                e.subtype = Some(typ.into());
                e.native_id = s(r.get("id"));
                e.parent_id = s(r.get("parentId"));
                e.created_at_ms = t;
                e.data_json = Some(r.clone());
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
                    e.data_json = Some(r.clone());
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
                            e.data_json = Some(b.clone());
                            events.push(e)
                        }
                    } else {
                        let mut e = Event::new(kind, flatten(m.get("content").unwrap_or(m)));
                        e.native_id = base_id;
                        e.parent_id = parent;
                        e.created_at_ms = mt;
                        e.data_json = Some(r.clone());
                        events.push(e)
                    }
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    for event in &mut events {
        if event.data_json.is_none() {
            event.data_json = Some(json!({"kind": event.kind.as_str(), "text": event.text}));
        }
    }
    let id = id.ok_or_else(|| {
        UnsupportedFormat(format!(
            "Pi JSONL missing session header: {}",
            path.display()
        ))
    })?;
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
        mode: candidate.mode,
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
            fingerprint: format!("{}:{}", row_count, end.unwrap_or_default()),
            sources: vec![source],
        },
        events,
    })
}
impl Parser for PiParser {
    fn agent(&self) -> Agent {
        Agent::Pi
    }
    fn discover_with_hints(&self, root: &Path, hints: &mut DiscoveryHints) -> Result<Discovery> {
        let mut discovery = Discovery::default();
        if !root.exists() {
            return Ok(discovery);
        };
        for entry in WalkDir::new(root).follow_links(false) {
            let e = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    discovery.push_failure(
                        error.path().unwrap_or(root).display().to_string(),
                        error.into(),
                    );
                    continue;
                }
            };
            if e.file_type().is_file()
                && e.path().extension().is_some_and(|x| x == "jsonl")
                && e.path().file_name().and_then(|name| name.to_str()) != Some("None.jsonl")
            {
                let locator = e.path().display().to_string();
                match SessionCandidate::file_with_cache(
                    e.path().to_path_buf(),
                    hints.fingerprints.get(&locator).map(String::as_str),
                ) {
                    Ok(candidate) => discovery.candidates.push(candidate),
                    Err(error) => {
                        discovery.push_failure(e.path().display().to_string(), error);
                    }
                }
            }
        }
        Ok(discovery)
    }

    fn parse(&self, candidate: &SessionCandidate, root: &Path) -> Result<Option<ParsedSession>> {
        let parsed = parse(&candidate.path, root, candidate)?;
        if parsed.session.cwd.as_deref() == Some("/tmp")
            || parsed.session.provider.as_deref() == Some("faux")
        {
            Ok(None)
        } else {
            Ok(Some(parsed))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discovery_ignores_none_placeholder() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("None.jsonl"),
            r#"{"type":"session","id":null,"cwd":"/tmp","timestamp":"2026-01-01T00:00:00Z"}
"#,
        )
        .unwrap();
        let session = dir.path().join("real.jsonl");
        fs::write(
            &session,
            r#"{"type":"session","id":"s1","cwd":"/workspace","timestamp":"2026-01-01T00:00:00Z"}
"#,
        )
        .unwrap();
        let discovery = PiParser.discover(dir.path()).unwrap();
        assert_eq!(discovery.candidates.len(), 1);
        assert_eq!(discovery.candidates[0].path, session);
    }
}
