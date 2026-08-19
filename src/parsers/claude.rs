use super::{Parser, SessionCandidate};
use crate::model::{
    compact, flatten, Agent, Capture, Event, EventKind, NativeSource, ParsedSession, Session,
};
use anyhow::Result;
use chrono::DateTime;
use serde_json::{json, Value};
use std::{fs, path::Path};
use walkdir::WalkDir;

pub struct ClaudeParser;
fn s(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}
fn ts(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_str)
        .and_then(|x| DateTime::parse_from_rfc3339(x).ok())
        .map(|x| x.timestamp_millis())
}
fn ev(k: EventKind, text: String, r: &Value, t: i64) -> Event {
    let mut e = Event::new(k, text);
    e.native_id = s(r.get("uuid"));
    e.parent_id = s(r.get("parentUuid"));
    e.created_at_ms = Some(t);
    e
}
fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .ok()
        .and_then(|x| x.to_str())
        .unwrap_or_else(|| {
            p.file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("session.jsonl")
        })
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn parse(path: &Path, root: &Path, candidate: &SessionCandidate) -> Result<ParsedSession> {
    let text = fs::read_to_string(path)?;
    let records: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let mut id = None;
    let mut cwd = None;
    let mut branch = None;
    let mut title = None;
    let mut model = None;
    let mut started = None;
    let mut ended = None;
    let mut forked = None;
    let mut events = Vec::new();
    for r in &records {
        let t = ts(r.get("timestamp"));
        if started.is_none() {
            started = t;
        }
        if t.is_some() {
            ended = t;
        }
        id = id.or_else(|| s(r.get("sessionId")));
        if forked.is_none() {
            forked = r.get("forkedFrom").and_then(|value| {
                let session_id = s(value.get("sessionId"))?;
                let message_uuid = s(value.get("messageUuid"));
                Some(match message_uuid {
                    Some(message_uuid) => format!("claude:{session_id}#{message_uuid}"),
                    None => format!("claude:{session_id}"),
                })
            });
        }
        cwd = cwd.or_else(|| s(r.get("cwd")));
        branch = branch.or_else(|| s(r.get("gitBranch")));
        if r.get("type").and_then(Value::as_str) == Some("ai-title") {
            title = title.or_else(|| s(r.get("aiTitle")));
        }
        if r.get("type").and_then(Value::as_str) == Some("assistant") {
            model = model.or_else(|| r.get("message").and_then(|m| s(m.get("model"))));
        }
        match r.get("type").and_then(Value::as_str).unwrap_or("") {
            "user" => {
                if r.get("toolUseResult").is_some() {
                    let mut e = ev(
                        EventKind::ToolResult,
                        compact(r.get("toolUseResult").unwrap()),
                        r,
                        t.unwrap_or_default(),
                    );
                    e.subtype = Some("tool_result".into());
                    events.push(e)
                } else {
                    events.push(ev(
                        EventKind::User,
                        flatten(r.get("message").unwrap_or(&Value::Null)),
                        r,
                        t.unwrap_or_default(),
                    ));
                }
            }
            "assistant" => {
                let c = r
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .unwrap_or(&Value::Null);
                if let Some(arr) = c.as_array() {
                    for b in arr {
                        let typ = b.get("type").and_then(Value::as_str).unwrap_or("text");
                        let (k, txt) = match typ {
                            "thinking" => (
                                EventKind::Thinking,
                                s(b.get("thinking").or_else(|| b.get("text"))),
                            ),
                            "tool_use" => (
                                EventKind::ToolCall,
                                Some(compact(b.get("input").unwrap_or(b))),
                            ),
                            "tool_result" => (
                                EventKind::ToolResult,
                                Some(compact(b.get("content").unwrap_or(b))),
                            ),
                            _ => (EventKind::Assistant, s(b.get("text"))),
                        };
                        let mut e = ev(k, txt.unwrap_or_default(), r, t.unwrap_or_default());
                        e.subtype = Some(typ.into());
                        e.name = s(b.get("name"));
                        e.call_id = s(b.get("id").or_else(|| b.get("tool_use_id")));
                        e.is_error = b.get("is_error").and_then(Value::as_bool);
                        events.push(e)
                    }
                } else {
                    events.push(ev(
                        EventKind::Assistant,
                        flatten(c),
                        r,
                        t.unwrap_or_default(),
                    ));
                }
            }
            "system" | "mode" | "permission-mode" | "attachment" => {
                let mut e = ev(
                    EventKind::System,
                    compact(
                        r.get("content")
                            .or_else(|| r.get("attachment"))
                            .unwrap_or(r),
                    ),
                    r,
                    t.unwrap_or_default(),
                );
                e.subtype = r
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| s(r.get("subtype")));
                events.push(e)
            }
            _ => {}
        }
    }
    let id = id.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or("unknown")
            .into()
    });
    let mut sources = vec![NativeSource {
        locator: path.display().to_string(),
        kind: "jsonl".into(),
        restore_path: rel(root, path),
        role: None,
        bytes: candidate.bytes,
        mtime_ns: candidate.mtime_ns,
        mode: None,
        capture: Some(Capture::File {
            path: path.display().to_string(),
        }),
    }];
    let sidecar = path.with_extension("meta.json");
    if sidecar.exists() {
        if let Ok(sm) = fs::metadata(&sidecar) {
            sources.push(NativeSource {
                locator: sidecar.display().to_string(),
                kind: "json".into(),
                restore_path: rel(root, &sidecar),
                role: Some("subagent-meta".into()),
                bytes: Some(sm.len() as i64),
                mtime_ns: None,
                mode: None,
                capture: Some(Capture::File {
                    path: sidecar.display().to_string(),
                }),
            });
        }
    }
    Ok(ParsedSession {
        session: Session {
            id: format!("claude:{id}"),
            agent: Agent::Claude,
            cwd,
            started_at_ms: started,
            ended_at_ms: ended,
            title,
            model,
            provider: None,
            git_branch: branch,
            parent_session_id: None,
            forked_from: forked,
            meta: json!({"recordCount":records.len()}),
            fingerprint: format!("{}:{}", records.len(), ended.unwrap_or_default()),
            sources,
        },
        events,
    })
}
impl Parser for ClaudeParser {
    fn agent(&self) -> Agent {
        Agent::Claude
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
            if e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl") {
                if let Ok(mut candidate) = SessionCandidate::file(e.path().to_path_buf()) {
                    candidate.include_file(&e.path().with_extension("meta.json"));
                    out.push(candidate);
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, candidate: &SessionCandidate, root: &Path) -> Result<Option<ParsedSession>> {
        let mut parsed = parse(&candidate.path, root, candidate)?;
        let rel_path = candidate.path.strip_prefix(root).unwrap_or(&candidate.path);
        let parts: Vec<_> = rel_path.components().collect();
        if let Some(index) = parts
            .iter()
            .position(|part| part.as_os_str() == "subagents")
        {
            if index > 0 {
                let parent = parts[index - 1].as_os_str().to_string_lossy();
                let child = candidate
                    .path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("agent");
                parsed.session.id = format!("claude:{parent}/{child}");
                parsed.session.parent_session_id = Some(format!("claude:{parent}"));
                let meta_path = candidate.path.with_extension("meta.json");
                if let Ok(meta_text) = fs::read_to_string(meta_path) {
                    if let Ok(meta) = serde_json::from_str::<Value>(&meta_text) {
                        parsed.session.meta["agentType"] =
                            meta.get("agentType").cloned().unwrap_or(Value::Null);
                        parsed.session.meta["isSubagent"] = Value::Bool(true);
                    }
                }
            }
        }
        Ok(Some(parsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn captures_forked_from_session_and_message() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("fork.jsonl");
        fs::write(
            &path,
            [json!({
                "sessionId":"child",
                "forkedFrom":{"sessionId":"parent","messageUuid":"m-1"},
                "type":"user",
                "timestamp":"2026-08-19T00:00:00Z",
                "message":{"role":"user","content":"continue"}
            })
            .to_string()]
            .join("\n"),
        )
        .unwrap();
        let parsed = ClaudeParser.parse_all(dir.path()).unwrap();
        assert_eq!(
            parsed[0].session.forked_from.as_deref(),
            Some("claude:parent#m-1")
        );
    }
}
