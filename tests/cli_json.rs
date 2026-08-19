use serde_json::{json, Value};
use std::{
    io::Write,
    process::{Command, Stdio},
};
use tempfile::tempdir;
use tracedb::{Agent, Event, EventKind, IngestMode, ParsedSession, Session, TraceDb};

fn archive() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("trace.db");
    let mut db = TraceDb::open(&path).unwrap();
    let mut tool = Event::new(EventKind::ToolCall, "{\"path\":\"README.md\"}");
    tool.name = Some("read_file".into());
    db.ingest_session(
        ParsedSession {
            session: Session {
                id: "codex:json-contract".into(),
                agent: Agent::Codex,
                cwd: Some("/workspace/demo".into()),
                started_at_ms: Some(10),
                ended_at_ms: Some(20),
                title: Some("JSON contract".into()),
                model: Some("gpt-test".into()),
                provider: Some("openai".into()),
                git_branch: Some("main".into()),
                parent_session_id: None,
                forked_from: None,
                meta: json!({"source":"fixture"}),
                fingerprint: "json-v1".into(),
                sources: Vec::new(),
            },
            events: vec![Event::new(EventKind::User, "inspect JSON"), tool],
        },
        IngestMode::Partial,
    )
    .unwrap();
    (dir, path)
}

#[test]
fn stats_json_serializes_the_complete_archive_stats() {
    let (_dir, path) = archive();
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["--db", path.to_str().unwrap(), "stats", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stats: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stats["path"], path.display().to_string());
    assert_eq!(stats["totalSessions"], 1);
    assert_eq!(stats["totalEvents"], 2);
    assert_eq!(stats["totalFullSessions"], 0);
    assert_eq!(stats["agents"][0]["fullSessions"], 0);
}

#[test]
fn show_json_serializes_the_complete_session_trace() {
    let (_dir, path) = archive();
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            path.to_str().unwrap(),
            "show",
            "codex:json-contract",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let trace: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(trace["session"]["id"], "codex:json-contract");
    assert_eq!(trace["session"]["model"], "gpt-test");
    assert_eq!(trace["mode"], "partial");
    assert_eq!(trace["events"].as_array().unwrap().len(), 2);
    assert_eq!(trace["events"][1]["kind"], "tool_call");
    assert_eq!(trace["events"][1]["name"], "read_file");
}

#[test]
fn json_lines_show_uses_the_same_nullable_session_trace() {
    let (_dir, path) = archive();
    let mut child = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["--db", path.to_str().unwrap(), "api"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{}", json!({"op":"show","id":"codex:json-contract"})).unwrap();
    writeln!(stdin, "{}", json!({"op":"show","id":"missing"})).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    let rows = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows[0]["result"]["session"]["id"], "codex:json-contract");
    assert_eq!(rows[0]["result"]["events"].as_array().unwrap().len(), 2);
    assert!(rows[1]["result"].is_null());
}
