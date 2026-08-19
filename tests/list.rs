use serde_json::json;
use tempfile::tempdir;
use tracedb::{Agent, Event, EventKind, IngestMode, ListRequest, ParsedSession, Session, TraceDb};

struct Fixture<'a> {
    id: &'a str,
    agent: Agent,
    cwd: &'a str,
    time: i64,
    mode: IngestMode,
    model: &'a str,
    provider: &'a str,
}

fn insert(database: &mut TraceDb, fixture: Fixture<'_>) {
    let Fixture {
        id,
        agent,
        cwd,
        time,
        mode,
        model,
        provider,
    } = fixture;
    database
        .ingest_session(
            ParsedSession {
                session: Session {
                    id: id.into(),
                    agent,
                    cwd: Some(cwd.into()),
                    started_at_ms: Some(time - 1),
                    ended_at_ms: Some(time),
                    title: Some(format!("Session {id}")),
                    model: Some(model.into()),
                    provider: Some(provider.into()),
                    git_branch: None,
                    parent_session_id: None,
                    forked_from: None,
                    meta: json!({}),
                    fingerprint: id.into(),
                    sources: Vec::new(),
                },
                events: vec![Event::new(EventKind::User, id)],
            },
            mode,
        )
        .unwrap();
}

#[test]
fn list_uses_stable_keyset_pagination() {
    let dir = tempdir().unwrap();
    let mut database = TraceDb::open(dir.path().join("trace.db")).unwrap();
    insert(
        &mut database,
        Fixture {
            id: "codex:a",
            agent: Agent::Codex,
            cwd: "/workspace/a",
            time: 30,
            mode: IngestMode::Partial,
            model: "gpt-a",
            provider: "openai",
        },
    );
    insert(
        &mut database,
        Fixture {
            id: "codex:b",
            agent: Agent::Codex,
            cwd: "/workspace/b",
            time: 20,
            mode: IngestMode::Partial,
            model: "gpt-b",
            provider: "openai",
        },
    );
    insert(
        &mut database,
        Fixture {
            id: "claude:c",
            agent: Agent::Claude,
            cwd: "/workspace/c",
            time: 20,
            mode: IngestMode::Full,
            model: "claude-test",
            provider: "anthropic",
        },
    );
    insert(
        &mut database,
        Fixture {
            id: "pi:d",
            agent: Agent::Pi,
            cwd: "/workspace/d",
            time: 10,
            mode: IngestMode::Partial,
            model: "gpt-d",
            provider: "openai",
        },
    );

    let first = database
        .list(ListRequest {
            limit: 2,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        first
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        ["codex:a", "claude:c"]
    );
    let cursor = first.next_cursor.unwrap();

    insert(
        &mut database,
        Fixture {
            id: "gemini:new",
            agent: Agent::Gemini,
            cwd: "/workspace/new",
            time: 40,
            mode: IngestMode::Partial,
            model: "gemini-test",
            provider: "google",
        },
    );
    let second = database
        .list(ListRequest {
            limit: 2,
            cursor: Some(cursor),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(
        second
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        ["codex:b", "pi:d"]
    );
    assert!(second.next_cursor.is_none());
}

#[test]
fn list_applies_metadata_filters_in_sql() {
    let dir = tempdir().unwrap();
    let mut database = TraceDb::open(dir.path().join("trace.db")).unwrap();
    insert(
        &mut database,
        Fixture {
            id: "codex:match",
            agent: Agent::Codex,
            cwd: "/workspace/project-a",
            time: 30,
            mode: IngestMode::Full,
            model: "gpt-test",
            provider: "openai",
        },
    );
    insert(
        &mut database,
        Fixture {
            id: "codex:old",
            agent: Agent::Codex,
            cwd: "/workspace/project-a",
            time: 5,
            mode: IngestMode::Full,
            model: "gpt-test",
            provider: "openai",
        },
    );
    insert(
        &mut database,
        Fixture {
            id: "claude:other",
            agent: Agent::Claude,
            cwd: "/workspace/project-a",
            time: 30,
            mode: IngestMode::Full,
            model: "claude-test",
            provider: "anthropic",
        },
    );

    let page = database
        .list(ListRequest {
            limit: 10,
            agent: Some(Agent::Codex),
            cwd: Some("project-a".into()),
            since_ms: Some(10),
            mode: Some(IngestMode::Full),
            model: Some("gpt-test".into()),
            provider: Some("openai".into()),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].id, "codex:match");
    assert_eq!(page.sessions[0].events, 1);
}

#[test]
fn list_rejects_invalid_cursors() {
    let dir = tempdir().unwrap();
    let database = TraceDb::open(dir.path().join("trace.db")).unwrap();
    let error = database
        .list(ListRequest {
            cursor: Some("not-a-cursor".into()),
            ..Default::default()
        })
        .unwrap_err();
    assert!(error.to_string().contains("invalid list cursor"));
}
