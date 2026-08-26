use serde_json::json;
use tempfile::tempdir;
use tracedb::{
    Agent, Event, EventKind, ListRequest, ParsedSession, Session, SessionRelation, SessionStatus,
    TraceDb,
};

struct Fixture<'a> {
    id: &'a str,
    agent: Agent,
    cwd: &'a str,
    time: i64,
    model: &'a str,
    provider: &'a str,
}

fn insert(database: &mut TraceDb, fixture: Fixture<'_>) {
    let Fixture {
        id,
        agent,
        cwd,
        time,
        model,
        provider,
    } = fixture;
    database
        .ingest_session(ParsedSession {
            session: Session {
                id: id.into(),
                agent,
                cwd: Some(cwd.into()),
                started_at_ms: Some(time - 1),
                ended_at_ms: Some(time),
                status: Some(SessionStatus::Completed),
                title: Some(format!("Session {id}")),
                model: Some(model.into()),
                provider: Some(provider.into()),
                git_branch: None,
                parent_session_id: None,
                parent_relation: None,
                fork_point_native_id: None,
                meta: json!({}),
                fingerprint: id.into(),
                sources: Vec::new(),
            },
            events: vec![Event::new(EventKind::User, id)],
        })
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

#[test]
fn list_exact_cwd_and_lineage_metadata_are_sql_projected() {
    let dir = tempdir().unwrap();
    let mut database = TraceDb::open(dir.path().join("trace.db")).unwrap();
    insert(
        &mut database,
        Fixture {
            id: "codex:parent",
            agent: Agent::Codex,
            cwd: "/workspace/app",
            time: 30,
            model: "gpt",
            provider: "openai",
        },
    );
    insert(
        &mut database,
        Fixture {
            id: "codex:app-old",
            agent: Agent::Codex,
            cwd: "/workspace/app-old",
            time: 29,
            model: "gpt",
            provider: "openai",
        },
    );
    database
        .ingest_session(ParsedSession {
            session: Session {
                id: "codex:child".into(),
                agent: Agent::Codex,
                cwd: Some("/workspace/worktree".into()),
                started_at_ms: Some(9),
                ended_at_ms: Some(10),
                status: Some(SessionStatus::Completed),
                title: None,
                model: None,
                provider: None,
                git_branch: None,
                parent_session_id: Some("codex:parent".into()),
                parent_relation: Some(SessionRelation::Subagent),
                fork_point_native_id: None,
                meta: json!({}),
                fingerprint: "child".into(),
                sources: Vec::new(),
            },
            events: vec![Event::new(EventKind::Assistant, "child")],
        })
        .unwrap();

    let page = database
        .list(ListRequest {
            cwd: Some("/workspace/app/".into()),
            cwd_exact: true,
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.sessions.len(), 1);
    let parent = &page.sessions[0];
    assert_eq!(parent.id, "codex:parent");
    assert_eq!(parent.parent_session_id, None);
    assert_eq!(parent.parent_relation, None);
    assert_eq!(parent.subagent_count, 1);

    let all = database
        .list(ListRequest {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    let child = all
        .sessions
        .iter()
        .find(|session| session.id == "codex:child")
        .unwrap();
    assert_eq!(child.parent_session_id.as_deref(), Some("codex:parent"));
    assert_eq!(child.parent_relation, Some(SessionRelation::Subagent));
    assert_eq!(child.status, Some(SessionStatus::Completed));

    let collapsed_all = database
        .list(ListRequest {
            limit: 10,
            collapse_lineage: true,
            ..Default::default()
        })
        .unwrap();
    assert!(collapsed_all
        .sessions
        .iter()
        .all(|session| session.id != "codex:child"));

    let worktree_scope = database
        .list(ListRequest {
            limit: 10,
            cwd: Some("/workspace/worktree".into()),
            cwd_exact: true,
            collapse_lineage: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(worktree_scope.sessions.len(), 1);
    assert_eq!(worktree_scope.sessions[0].id, "codex:child");
}
