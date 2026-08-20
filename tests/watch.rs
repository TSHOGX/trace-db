use serde_json::Value;
use std::{
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tempfile::tempdir;
use tracedb::{
    Agent, IngestMode, IngestRequest, TraceDb, WatchEvent, WatchRequest, WatchRun, WatchTrigger,
};

fn request(root: PathBuf, once: bool) -> WatchRequest {
    WatchRequest {
        ingest: IngestRequest {
            agents: vec![Agent::Codex],
            mode: IngestMode::Partial,
            root: Some(root),
            since_ms: None,
            exclude: Vec::new(),
        },
        interval_seconds: 1,
        debounce_ms: 20,
        once,
    }
}

#[test]
fn watch_once_runs_startup_ingest_and_returns_summary() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("native");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("rollout-startup.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"startup\"}}\n",
    )
    .unwrap();
    let archive = dir.path().join("trace.db");
    let mut db = TraceDb::open(&archive).unwrap();
    let stop = AtomicBool::new(false);
    let mut events = Vec::new();
    let summary = db
        .watch(request(root, true), &stop, &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

    assert_eq!(summary.runs, 1);
    assert!(!summary.stopped);
    assert!(matches!(
        events[0],
        WatchEvent::Run(WatchRun {
            trigger: WatchTrigger::Startup,
            ..
        })
    ));
    assert_eq!(
        db.show("codex:startup").unwrap().unwrap().session.id,
        "codex:startup"
    );
}

#[test]
fn watch_ingests_a_file_event_and_stops_cleanly() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("native");
    std::fs::create_dir(&root).unwrap();
    let archive = dir.path().join("trace.db");
    let source = root.join("rollout-event.jsonl");
    let mut db = TraceDb::open(&archive).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let callback_stop = Arc::clone(&stop);
    let mut events = Vec::new();
    let summary = db
        .watch(request(root.clone(), false), &stop, &mut |event| {
            match &event {
                WatchEvent::Run(run) if run.trigger == WatchTrigger::Startup => {
                    std::fs::write(
                        &source,
                        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"event\"}}\n",
                    )
                    .unwrap();
                }
                WatchEvent::Run(run)
                    if matches!(
                        run.trigger,
                        WatchTrigger::Filesystem | WatchTrigger::Periodic
                    ) && run.report.total_ingested() > 0 =>
                {
                    callback_stop.store(true, Ordering::Relaxed);
                }
                _ => {}
            }
            events.push(event);
            Ok(())
        })
        .unwrap();

    assert!(summary.stopped);
    assert!(summary.runs >= 2);
    assert!(events.iter().any(|event| matches!(
        event,
        WatchEvent::Run(run)
            if matches!(run.trigger, WatchTrigger::Filesystem | WatchTrigger::Periodic)
                && run.report.total_ingested() > 0
    )));
    assert!(db.show("codex:event").unwrap().is_some());
}

#[test]
fn watch_rejects_zero_timings() {
    let dir = tempdir().unwrap();
    let mut db = TraceDb::open(dir.path().join("trace.db")).unwrap();
    let stop = AtomicBool::new(false);
    let error = db
        .watch(
            WatchRequest {
                interval_seconds: 0,
                ..request(dir.path().to_path_buf(), true)
            },
            &stop,
            &mut |_| Ok(()),
        )
        .unwrap_err();
    assert!(error.to_string().contains("interval"));
}

#[test]
fn watch_cli_once_emits_json_events_and_summary() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("native");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("rollout-cli.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"cli\"}}\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_trace-db"))
        .args([
            "--db",
            dir.path().join("trace.db").to_str().unwrap(),
            "watch",
            "--agent",
            "codex",
            "--root",
            root.to_str().unwrap(),
            "--once",
            "--json",
        ])
        .env_remove("TRACEDB_CONFIG")
        .env_remove("TRACEDB_PATH")
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["type"], "run");
    assert_eq!(rows[0]["trigger"], "startup");
    assert_eq!(rows[0]["report"]["agents"][0]["ingested"], 1);
    assert_eq!(rows[1]["runs"], 1);
    assert_eq!(rows[1]["stopped"], false);
}
