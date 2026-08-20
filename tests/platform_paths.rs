use tempfile::tempdir;
use tracedb::{parsers::gemini::GeminiParser, parsers::Parser};

#[test]
fn nested_native_paths_use_portable_restore_separators() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("native store");
    let nested = root.join("sessions").join("2026");
    std::fs::create_dir_all(&nested).unwrap();
    let source = nested.join("session-portable.json");
    std::fs::write(
        &source,
        r#"{"sessionId":"portable","messages":[{"id":"u","type":"user","content":"path portability"}]}"#,
    )
    .unwrap();

    let sessions = GeminiParser.parse_all(&root).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].session.sources[0].restore_path,
        "sessions/2026/session-portable.json"
    );
    assert!(!sessions[0].session.sources[0].restore_path.contains('\\'));
}
