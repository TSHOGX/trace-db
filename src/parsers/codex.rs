//! Codex rollout parser. It merges the two native streams without assuming
//! that `event_msg` is always present (newer Codex writes user messages as
//! `response_item/message`). Native records are retained in full mode; this
//! parser only emits the cross-agent projection used by search/show.

use super::{read_json_lines, Discovery, Parser, SessionCandidate, UnsupportedFormat};
use crate::model::{
    compact, flatten, Agent, Capture, Event, EventKind, NativeSource, ParsedSession, Session,
    TokenUsage,
};
use anyhow::Result;
use chrono::DateTime;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub struct CodexParser;

type Lineage = HashMap<String, (String, Option<String>)>;
type RolloutSessionIds = HashMap<PathBuf, String>;

fn strv(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}
fn epoch(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
}
fn payload(r: &Value) -> &Value {
    r.get("payload").unwrap_or(&Value::Null)
}
fn event(kind: EventKind, text: String, native: Option<String>, ts: Option<i64>) -> Event {
    let mut e = Event::new(kind, text);
    e.native_id = native;
    e.created_at_ms = ts;
    e
}

fn parse_file(path: &Path, candidate: &SessionCandidate) -> Result<ParsedSession> {
    let records = read_json_lines(path)?;
    let mut id = None;
    let mut cwd = None;
    let mut branch = None;
    let mut provider = None;
    let mut model = None;
    let mut started = None;
    let mut ended = None;
    let mut meta = json!({});
    let mut events = Vec::new();
    let mut seen_user = std::collections::HashSet::new();
    for r in &records {
        let p = payload(r);
        let typ = r.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = epoch(r.get("timestamp"));
        started = started.or(ts);
        if ts.is_some() {
            ended = ts;
        }
        match typ {
            "session_meta" => {
                id = id.or_else(|| strv(p.get("id")).or_else(|| strv(p.get("session_id"))));
                cwd = cwd.or_else(|| strv(p.get("cwd")));
                provider = provider.or_else(|| strv(p.get("model_provider")));
                branch = branch.or_else(|| p.get("git").and_then(|g| strv(g.get("branch"))));
                meta = json!({"cli_version":p.get("cli_version"),"originator":p.get("originator"),"source":p.get("source")});
            }
            "turn_context" => {
                model = model.or_else(|| strv(p.get("model")));
            }
            "event_msg" => match p.get("type").and_then(Value::as_str).unwrap_or("") {
                "user_message" => {
                    let text = strv(p.get("message"))
                        .or_else(|| p.get("text_elements").map(flatten))
                        .unwrap_or_default();
                    let key = format!("{}:{}", sha(text.as_bytes()), ts.unwrap_or_default());
                    if seen_user.insert(key) {
                        events.push(event(EventKind::User, text, None, ts));
                    }
                }
                "token_count" => {
                    let mut e = event(
                        EventKind::Usage,
                        compact(p.get("info").unwrap_or(p)),
                        None,
                        ts,
                    );
                    e.subtype = Some("token_count".into());
                    let u = p
                        .get("info")
                        .and_then(|x| x.get("last_token_usage"))
                        .unwrap_or(&Value::Null);
                    e.usage = Some(TokenUsage {
                        total: u.get("total_tokens").and_then(Value::as_i64),
                        ..Default::default()
                    });
                    events.push(e);
                }
                "agent_message" | "item_completed" => {}
                other if !other.is_empty() => {
                    let mut e = event(EventKind::System, compact(p), None, ts);
                    e.subtype = Some(other.into());
                    events.push(e);
                }
                _ => {}
            },
            "response_item" => match p.get("type").and_then(Value::as_str).unwrap_or("") {
                "message" => {
                    let role = strv(p.get("role")).unwrap_or_default();
                    if role == "assistant" {
                        let mut e = event(
                            EventKind::Assistant,
                            flatten(p.get("content").unwrap_or(&Value::Null)),
                            strv(p.get("id")),
                            ts,
                        );
                        e.role = Some(role);
                        events.push(e);
                    } else if role == "user" || role == "developer" {
                        let text = flatten(p.get("content").unwrap_or(&Value::Null));
                        let key = format!("{}:{}", sha(text.as_bytes()), ts.unwrap_or_default());
                        if seen_user.insert(key) {
                            events.push(event(EventKind::User, text, strv(p.get("id")), ts));
                        }
                    }
                }
                "reasoning" => {
                    let mut e = event(
                        EventKind::Thinking,
                        compact(p.get("summary").unwrap_or(p)),
                        strv(p.get("id")),
                        ts,
                    );
                    e.subtype = Some("summary".into());
                    events.push(e);
                }
                "function_call" | "custom_tool_call" | "tool_search_call" | "web_search_call" => {
                    let mut e = event(
                        EventKind::ToolCall,
                        compact(
                            p.get("arguments")
                                .or_else(|| p.get("input"))
                                .or_else(|| p.get("query"))
                                .unwrap_or(p),
                        ),
                        strv(p.get("id")),
                        ts,
                    );
                    e.name = strv(p.get("name"))
                        .or_else(|| p.get("type").and_then(Value::as_str).map(str::to_owned));
                    e.call_id = strv(p.get("call_id"));
                    e.subtype = p.get("type").and_then(Value::as_str).map(str::to_owned);
                    events.push(e);
                }
                "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                    let mut e = event(
                        EventKind::ToolResult,
                        compact(p.get("output").unwrap_or(p)),
                        strv(p.get("id")),
                        ts,
                    );
                    e.call_id = strv(p.get("call_id"));
                    e.subtype = p.get("type").and_then(Value::as_str).map(str::to_owned);
                    events.push(e);
                }
                _ => {}
            },
            "compacted" | "world_state" => {
                let mut e = event(EventKind::System, compact(p), None, ts);
                e.subtype = Some(typ.into());
                events.push(e);
            }
            _ => {}
        }
    }
    let id = id.ok_or_else(|| {
        UnsupportedFormat(format!("rollout missing session id: {}", path.display()))
    })?;
    let fingerprint = format!("{}:{}", records.len(), ended.unwrap_or_default());
    let source = NativeSource {
        locator: path.display().to_string(),
        kind: "jsonl".into(),
        restore_path: restore_path(path),
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
            id: format!("codex:{id}"),
            agent: Agent::Codex,
            cwd,
            started_at_ms: started,
            ended_at_ms: ended,
            title: None,
            model,
            provider,
            git_branch: branch,
            parent_session_id: None,
            forked_from: None,
            meta,
            fingerprint,
            sources: vec![source],
        },
        events,
    })
}

fn sha(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}
fn restore_path(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("rollout.jsonl")
        .to_owned()
}

fn rollout_paths(root: &Path, discovery: &mut Discovery) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                discovery.push_failure(
                    error.path().unwrap_or(root).display().to_string(),
                    error.into(),
                );
                continue;
            }
        };
        if entry.file_type().is_file()
            && entry.file_name().to_string_lossy().starts_with("rollout-")
            && entry.path().extension().is_some_and(|x| x == "jsonl")
        {
            paths.push(entry.into_path());
        }
    }
    paths
}

/// Codex records the parent edge only in the parent's spawn_agent output. A
/// full pre-pass is therefore required even when the caller later filters by
/// date or project.
fn build_lineage(paths: &[PathBuf]) -> (Lineage, RolloutSessionIds) {
    let mut edges = HashMap::new();
    let mut session_ids = HashMap::new();
    for path in paths {
        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        let mut parent = None;
        let mut pending: HashMap<String, Option<String>> = HashMap::new();
        let mut children = Vec::new();
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(r) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if r.get("type").and_then(Value::as_str) == Some("session_meta") {
                parent = parent.or_else(|| {
                    strv(payload(&r).get("id")).or_else(|| strv(payload(&r).get("session_id")))
                });
                continue;
            }
            if r.get("type").and_then(Value::as_str) != Some("response_item") {
                continue;
            }
            let p = payload(&r);
            let typ = p.get("type").and_then(Value::as_str).unwrap_or("");
            let call = strv(p.get("call_id"));
            if typ == "function_call" && strv(p.get("name")).as_deref() == Some("spawn_agent") {
                if let Some(call) = call {
                    let role = strv(p.get("arguments"))
                        .and_then(|args| serde_json::from_str::<Value>(&args).ok())
                        .and_then(|v| strv(v.get("agent_type")));
                    pending.insert(call, role);
                }
            } else if typ == "function_call_output" {
                if let Some(call) = call {
                    if let Some(role) = pending.remove(&call) {
                        if let Some(child) = strv(p.get("output"))
                            .and_then(|out| serde_json::from_str::<Value>(&out).ok())
                            .and_then(|v| strv(v.get("agent_id")))
                        {
                            children.push((child, role));
                        }
                    }
                }
            }
        }
        let Some(parent) = parent else { continue };
        session_ids.insert(path.clone(), parent.clone());
        for (child, role) in children {
            edges.insert(format!("codex:{child}"), (format!("codex:{parent}"), role));
        }
    }
    (edges, session_ids)
}

impl Parser for CodexParser {
    fn agent(&self) -> Agent {
        Agent::Codex
    }
    fn discover(&self, root: &Path) -> Result<Discovery> {
        let mut discovery = Discovery::default();
        if !root.exists() {
            return Ok(discovery);
        };
        let paths = rollout_paths(root, &mut discovery);
        let (lineage, session_ids) = build_lineage(&paths);
        for path in paths {
            match SessionCandidate::file(path.clone()) {
                Ok(mut candidate) => {
                    if let Some(native_id) = session_ids.get(&path) {
                        candidate.native_id = Some(native_id.clone());
                        if let Some((parent, role)) = lineage.get(&format!("codex:{native_id}")) {
                            candidate.parent_session_id = Some(parent.clone());
                            candidate.agent_type = role.clone();
                            candidate.fingerprint.push_str(&format!(
                                ":lineage:{parent}:{}",
                                role.as_deref().unwrap_or_default()
                            ));
                        }
                    }
                    discovery.candidates.push(candidate);
                }
                Err(error) => {
                    discovery.push_failure(path.display().to_string(), error);
                }
            }
        }
        Ok(discovery)
    }

    fn parse(&self, candidate: &SessionCandidate, _root: &Path) -> Result<Option<ParsedSession>> {
        let mut parsed = parse_file(&candidate.path, candidate)?;
        parsed.session.parent_session_id = candidate.parent_session_id.clone();
        if candidate.parent_session_id.is_some() {
            parsed.session.meta["agentType"] = candidate
                .agent_type
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null);
        }
        Ok(Some(parsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn merges_new_response_item_user_messages_without_duplicates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rollout-test.jsonl");
        let rows = [
            json!({"type":"session_meta","timestamp":"2026-08-19T00:00:00Z","payload":{"id":"sess-1","cwd":"/tmp/x"}}),
            json!({"type":"response_item","timestamp":"2026-08-19T00:00:01Z","payload":{"type":"message","id":"u-1","role":"user","content":"deploy it"}}),
            json!({"type":"event_msg","timestamp":"2026-08-19T00:00:01Z","payload":{"type":"user_message","message":"deploy it"}}),
            json!({"type":"event_msg","timestamp":"2026-08-19T00:00:03Z","payload":{"type":"user_message","message":"deploy it"}}),
            json!({"type":"response_item","timestamp":"2026-08-19T00:00:02Z","payload":{"type":"message","id":"a-1","role":"assistant","content":[{"type":"output_text","text":"done"}]}}),
        ];
        fs::write(
            &path,
            rows.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let sessions = CodexParser.parse_all(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0]
                .events
                .iter()
                .filter(|e| e.kind == EventKind::User)
                .count(),
            2
        );
        assert_eq!(
            sessions[0]
                .events
                .iter()
                .filter(|e| e.kind == EventKind::Assistant)
                .count(),
            1
        );
    }

    #[test]
    fn derives_subagent_parent_from_spawn_output() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("rollout-parent.jsonl");
        let child = dir.path().join("rollout-child.jsonl");
        let parent_rows = [
            json!({"type":"session_meta","payload":{"id":"parent"}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"spawn_agent","call_id":"c1","arguments":"{\"agent_type\":\"explorer\"}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"{\"agent_id\":\"child\"}"}}),
        ];
        let child_rows = [json!({"type":"session_meta","payload":{"id":"child"}})];
        fs::write(
            &parent,
            parent_rows
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        fs::write(
            &child,
            child_rows
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let sessions = CodexParser.parse_all(dir.path()).unwrap();
        let child = sessions
            .iter()
            .find(|s| s.session.id == "codex:child")
            .unwrap();
        assert_eq!(
            child.session.parent_session_id.as_deref(),
            Some("codex:parent")
        );
        assert_eq!(
            child.session.meta["agentType"],
            Value::String("explorer".into())
        );
    }
}
