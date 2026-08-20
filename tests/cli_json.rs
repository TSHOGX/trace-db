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
    assert_eq!(stats["totalFullSessions"], 1);
    assert_eq!(stats["agents"][0]["fullSessions"], 1);
}

#[test]
fn jsonl_search_emits_one_json_value_per_result() {
    let (_dir, path) = archive();
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            path.to_str().unwrap(),
            "--format",
            "jsonl",
            "search",
            "inspect",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let lines = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["id"], "codex:json-contract");
}

#[test]
fn markdown_and_quiet_formats_are_stable() {
    let (_dir, path) = archive();
    let markdown = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            path.to_str().unwrap(),
            "--format",
            "markdown",
            "stats",
        ])
        .output()
        .unwrap();
    assert!(markdown.status.success());
    let markdown_stdout = String::from_utf8(markdown.stdout).unwrap();
    assert!(markdown_stdout.starts_with("## TraceDB result"));
    assert!(markdown_stdout.contains("```json"));

    let quiet = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["--db", path.to_str().unwrap(), "--quiet", "stats"])
        .output()
        .unwrap();
    assert!(quiet.status.success());
    assert!(quiet.stdout.is_empty());

    let quiet_json = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            path.to_str().unwrap(),
            "--quiet",
            "--format",
            "jsonl",
            "stats",
        ])
        .output()
        .unwrap();
    assert!(quiet_json.status.success());
    assert_eq!(
        String::from_utf8(quiet_json.stdout)
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn progress_goes_to_stderr_without_changing_json_stdout() {
    let (_dir, path) = archive();
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["--db", path.to_str().unwrap(), "--progress", "reindex"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("events_fts rebuilt"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("reindex: starting"));

    let json = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            path.to_str().unwrap(),
            "--progress",
            "--format",
            "json",
            "verify",
        ])
        .output()
        .unwrap();
    assert!(json.status.success());
    assert!(serde_json::from_slice::<Value>(&json.stdout).is_ok());
    assert!(String::from_utf8_lossy(&json.stderr).contains("verify: starting"));
}

#[test]
fn completions_generate_without_opening_an_archive() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("not-created.db");
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["--db", path.to_str().unwrap(), "completions", "bash"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let script = String::from_utf8(output.stdout).unwrap();
    assert!(script.contains("_trace__db"));
    assert!(!path.exists());
}

#[test]
fn import_json_reports_merge_and_skip_counts() {
    let (dir, source_path) = archive();
    let backup_path = dir.path().join("backup.db");
    TraceDb::open(&source_path)
        .unwrap()
        .backup(&backup_path)
        .unwrap();
    let destination_path = dir.path().join("destination.db");

    let first = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            destination_path.to_str().unwrap(),
            "import",
            backup_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(first.status.success());
    let first_report: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first_report["importedSessions"], 1);
    assert_eq!(first_report["importedEvents"], 2);

    let second = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            destination_path.to_str().unwrap(),
            "import",
            backup_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(second.status.success());
    let second_report: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_report["importedSessions"], 0);
    assert_eq!(second_report["importedEvents"], 0);
    assert_eq!(second_report["skippedSessions"], 1);
    assert_eq!(second_report["skippedEvents"], 2);
}

#[test]
fn list_json_returns_the_canonical_cursor_page() {
    let (_dir, path) = archive();
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            path.to_str().unwrap(),
            "list",
            "--limit",
            "1",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["sessions"][0]["id"], "codex:json-contract");
    assert_eq!(page["sessions"][0]["events"], 2);
    assert!(page["nextCursor"].is_null());
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
    assert_eq!(trace["mode"], "full");
    assert_eq!(trace["events"].as_array().unwrap().len(), 2);
    assert_eq!(trace["events"][1]["kind"], "tool_call");
    assert_eq!(trace["events"][1]["name"], "read_file");
}

#[test]
fn show_json_filters_by_inclusive_index_and_kind() {
    let (_dir, path) = archive();
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            path.to_str().unwrap(),
            "show",
            "codex:json-contract",
            "--from",
            "1",
            "--to",
            "1",
            "--kind",
            "tool_call",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let trace: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(trace["session"]["id"], "codex:json-contract");
    assert_eq!(trace["events"].as_array().unwrap().len(), 1);
    assert_eq!(trace["events"][0]["idx"], 1);
    assert_eq!(trace["events"][0]["kind"], "tool_call");
}

#[test]
fn show_text_honors_explicit_tool_kind_without_include_tools() {
    let (_dir, path) = archive();
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            path.to_str().unwrap(),
            "show",
            "codex:json-contract",
            "--kind",
            "tool_call",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("[1] tool_call"));
    assert!(!stdout.contains("[0] user"));
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
    writeln!(stdin, "{}", json!({"op":"list","limit":1,"agent":"codex"})).unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"op":"show","id":"codex:json-contract","from":1,"kind":["tool_call"]})
    )
    .unwrap();
    writeln!(stdin, "{}", json!({"op":"show","id":"codex:json-contract"})).unwrap();
    writeln!(stdin, "{}", json!({"op":"show","id":"missing"})).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    let rows = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        rows[0]["result"]["sessions"][0]["id"],
        "codex:json-contract"
    );
    assert_eq!(rows[0]["result"]["sessions"][0]["events"], 2);
    assert_eq!(rows[1]["result"]["events"].as_array().unwrap().len(), 1);
    assert_eq!(rows[1]["result"]["events"][0]["kind"], "tool_call");
    assert_eq!(rows[2]["result"]["session"]["id"], "codex:json-contract");
    assert_eq!(rows[2]["result"]["events"].as_array().unwrap().len(), 2);
    assert!(rows[3]["result"].is_null());
}

#[test]
fn json_lines_api_returns_structured_errors_and_continues() {
    let (_dir, path) = archive();
    let mut child = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args(["--db", path.to_str().unwrap(), "api"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{{not json").unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"op":"search","query":"inspect","agent":"unknown"})
    )
    .unwrap();
    writeln!(stdin, "{}", json!({"op":"missing"})).unwrap();
    writeln!(stdin, "{}", json!({"op":"stats"})).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    let rows = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["ok"], false);
    assert_eq!(rows[0]["error"]["code"], "invalid_json");
    assert_eq!(rows[1]["error"]["code"], "invalid_argument");
    assert_eq!(rows[2]["error"]["code"], "unsupported_operation");
    assert!(rows[2]["error"]["details"]["supported"].is_array());
    assert_eq!(rows[3]["ok"], true);
    assert_eq!(rows[3]["result"]["totalSessions"], 1);
}
